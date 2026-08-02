//! AeroDesk CLI 客户端。
//!
//! publisher：连接 SFU，用真实 VP8 抓包流作为媒体源发送视频。
//! viewer：连接 SFU，接收媒体并打印统计。
//!
//! 用法：
//!   aerodesk-cli --role publisher --signal ws://127.0.0.1:3003 --room demo
//!   aerodesk-cli --role viewer    --signal ws://127.0.0.1:3003 --room demo

#[macro_use]
extern crate tracing;

use std::net::UdpSocket;
use std::time::{Duration, Instant};

use aerodesk_core::endpoint::ClientEvent;
use aerodesk_core::media::{Vp8Frame, parse_vp8_pcap};
use aerodesk_core::{Endpoint, signaling::WsSignalClient};
use aerodesk_protocol::signal::Role;
use str0m::media::{Frequency, MediaTime};
use str0m::net::Protocol;
use str0m::{Input, Output, net::Receive};

fn main() {
    init_log();
    let args: Vec<String> = std::env::args().collect();
    let role = arg(&args, "--role").unwrap_or_else(|| "viewer".into());
    let signal = arg(&args, "--signal").unwrap_or_else(|| "ws://127.0.0.1:3003/ws".into());
    let signal = if signal.contains("/ws") {
        signal
    } else {
        format!("{signal}/ws")
    };
    let room = arg(&args, "--room").unwrap_or_else(|| "demo".into());
    let encoder = arg(&args, "--encoder").unwrap_or_else(|| "pcap".into());

    match role.as_str() {
        "publisher" if encoder == "vt" => publisher_vt(&signal, &room),
        "publisher" if encoder == "x264" => publisher_x264(&signal, &room),
        "publisher" => publisher(&signal, &room),
        "viewer" => viewer(&signal, &room),
        other => panic!("unknown role {other}"),
    }
}

fn arg(args: &[String], key: &str) -> Option<String> {
    args.iter()
        .position(|a| a == key)
        .map(|i| args[i + 1].clone())
}

fn init_log() {
    use tracing_subscriber::{EnvFilter, fmt, prelude::*};
    let filter =
        EnvFilter::try_from_default_env().unwrap_or_else(|_| EnvFilter::new("aerodesk_cli=info"));
    tracing_subscriber::registry()
        .with(fmt::layer())
        .with(filter)
        .init();
}

/// 连接信令 + 建立 Endpoint（公共步骤）。返回 (signal, endpoint, socket, video_mid)。
fn connect(
    signal_url: &str,
    room: &str,
    role: Role,
) -> (WsSignalClient, Endpoint, UdpSocket, str0m::media::Mid) {
    connect_inner(signal_url, room, role, false)
}

fn connect_h264(
    signal_url: &str,
    room: &str,
    role: Role,
) -> (WsSignalClient, Endpoint, UdpSocket, str0m::media::Mid) {
    connect_inner(signal_url, room, role, true)
}

fn connect_inner(
    signal_url: &str,
    room: &str,
    role: Role,
    h264_only: bool,
) -> (WsSignalClient, Endpoint, UdpSocket, str0m::media::Mid) {
    let mut signal = WsSignalClient::connect(signal_url).expect("signal connect");
    let (peer_id, turn) = signal.join(room, role, None).expect("join");
    info!("joined room {room} as {peer_id}");

    let socket = UdpSocket::bind("127.0.0.1:0").expect("bind udp");
    let addr = socket.local_addr().unwrap();
    info!("local UDP addr: {addr}");

    let mut endpoint = if h264_only {
        Endpoint::new_h264()
    } else {
        Endpoint::new()
    };
    endpoint
        .add_local_candidate(addr, Protocol::Udp)
        .expect("candidate");
    let _ = turn;

    endpoint.add_video();
    let (offer, pending, video_mid) = endpoint.create_offer().expect("offer");
    info!("video mid: {video_mid:?}");
    let offer_json = serde_json::to_string(&offer).unwrap();
    let answer_json = signal.exchange_description(&offer_json).expect("answer");
    let answer: str0m::change::SdpAnswer =
        serde_json::from_str(&answer_json).expect("answer parse");
    debug!("answer media lines: {:?}", answer.media_lines);
    debug!("offer media lines: {:?}", offer.media_lines);
    endpoint
        .accept_answer(pending, answer)
        .expect("accept answer");

    info!("SDP negotiated, awaiting ICE...");
    let video_mid = video_mid.expect("video mid");
    (signal, endpoint, socket, video_mid)
}

fn publisher(signal_url: &str, room: &str) {
    let pcap = include_bytes!("../../../crates/aerodesk-core/tests/data/vp8.pcap");
    let frames = parse_vp8_pcap(pcap);
    info!("loaded {} VP8 frames from pcap", frames.len());

    let (mut signal, mut endpoint, socket, video_mid) = connect(signal_url, room, Role::Publisher);
    let mut connected = false;
    let mut frame_idx = 0usize;
    let mut last_frame_time = Instant::now();
    let mut next_deadline = Instant::now() + Duration::from_millis(100);

    loop {
        // UDP 输入
        let wait = next_deadline
            .saturating_duration_since(Instant::now())
            .max(Duration::from_millis(1));
        socket.set_read_timeout(Some(wait)).ok();
        let mut buf = [0u8; 2000];
        match socket.recv_from(&mut buf) {
            Ok((n, source)) => {
                debug!("recv {} bytes from {} type={:#04x}", n, source, buf[0]);
                let Ok(contents) = buf[..n].try_into() else {
                    continue;
                };
                let input = Input::Receive(
                    Instant::now(),
                    Receive {
                        proto: Protocol::Udp,
                        source,
                        destination: socket.local_addr().unwrap(),
                        contents,
                    },
                );
                let _ = endpoint.handle_input(input);
            }
            Err(e) => {
                if e.kind() != std::io::ErrorKind::WouldBlock
                    && e.kind() != std::io::ErrorKind::TimedOut
                {
                    debug!("recv error: {e:?}");
                }
            }
        }
        let _ = endpoint.handle_timeout(Instant::now());

        // 输出
        let mut deadline = Instant::now() + Duration::from_secs(1);
        while let Some(output) = endpoint.poll_output() {
            match output {
                Output::Transmit(t) => {
                    debug!(
                        "tx {} bytes to {} type={:#04x}",
                        t.contents.len(),
                        t.destination,
                        t.contents.first().copied().unwrap_or(0)
                    );
                    let _ = socket.send_to(&t.contents, t.destination);
                }
                Output::Timeout(t) => {
                    deadline = deadline.min(t);
                    break;
                }
                Output::Event(_) => {}
            }
        }
        next_deadline = deadline.min(next_deadline);

        // 客户端事件
        while let Some(ev) = endpoint.poll_event() {
            match ev {
                ClientEvent::IceConnected => {
                    info!("ICE connected, starting stream");
                    connected = true;
                    last_frame_time = Instant::now();
                }
                ClientEvent::Closed => {
                    info!("connection closed");
                    return;
                }
                _ => {}
            }
        }

        // 发送帧（按 90kHz 时间戳节奏）
        if connected && frame_idx < frames.len() {
            let f: &Vp8Frame = &frames[frame_idx];
            let interval = if frame_idx > 0 {
                Duration::from_secs_f64(
                    (f.rtp_timestamp as i64 - frames[frame_idx - 1].rtp_timestamp as i64).max(0)
                        as f64
                        / 90_000.0,
                )
            } else {
                Duration::ZERO
            };
            if frame_idx == 0 || last_frame_time.elapsed() >= interval {
                let rtp_time = MediaTime::new(f.rtp_timestamp as u64, Frequency::NINETY_KHZ);
                if let Err(e) = endpoint.send_video_frame(video_mid, f.data.clone(), rtp_time) {
                    warn!("send frame failed: {e:?}");
                }
                if f.keyframe {
                    debug!("sent keyframe #{frame_idx}");
                }
                last_frame_time = Instant::now();
                frame_idx += 1;
                if frame_idx == frames.len() {
                    info!("stream finished ({} frames)", frames.len());
                }
            }
        }

        std::thread::sleep(Duration::from_millis(2));
        let _ = &mut signal;
    }
}

fn viewer(signal_url: &str, room: &str) {
    let (mut signal, mut endpoint, socket, _) = connect(signal_url, room, Role::Viewer);
    let mut frames = 0u64;
    let mut bytes = 0u64;
    let mut keyframes = 0u64;
    let mut last_report = Instant::now();

    loop {
        let wait = Duration::from_millis(50);
        socket.set_read_timeout(Some(wait)).ok();
        let mut buf = [0u8; 2000];
        match socket.recv_from(&mut buf) {
            Ok((n, source)) => {
                debug!("recv {} bytes from {} type={:#04x}", n, source, buf[0]);
                let Ok(contents) = buf[..n].try_into() else {
                    continue;
                };
                let input = Input::Receive(
                    Instant::now(),
                    Receive {
                        proto: Protocol::Udp,
                        source,
                        destination: socket.local_addr().unwrap(),
                        contents,
                    },
                );
                let _ = endpoint.handle_input(input);
            }
            Err(e) => {
                if e.kind() != std::io::ErrorKind::WouldBlock
                    && e.kind() != std::io::ErrorKind::TimedOut
                {
                    debug!("recv error: {e:?}");
                }
            }
        }
        let _ = endpoint.handle_timeout(Instant::now());

        while let Some(output) = endpoint.poll_output() {
            match output {
                Output::Transmit(t) => {
                    let _ = socket.send_to(&t.contents, t.destination);
                }
                Output::Timeout(_) => break,
                Output::Event(_) => {}
            }
        }

        while let Some(ev) = endpoint.poll_event() {
            match ev {
                ClientEvent::Media(data) => {
                    frames += 1;
                    bytes += data.data.len() as u64;
                    if data.is_keyframe() {
                        keyframes += 1;
                    }
                }
                ClientEvent::IceConnected => info!("ICE connected"),
                ClientEvent::Closed => {
                    info!("connection closed");
                    return;
                }
                _ => {}
            }
        }

        if last_report.elapsed() >= Duration::from_secs(2) {
            info!("RECEIVED: {frames} frames, {bytes} bytes, {keyframes} keyframes");
            last_report = Instant::now();
        }
        std::thread::sleep(Duration::from_millis(2));
        let _ = &mut signal;
    }
}

/// x264 发布端：合成帧 → H.264 编码 → SFU。
fn publisher_x264(signal_url: &str, room: &str) {
    use aerodesk_macos::encoder::X264Encoder;
    use aerodesk_macos::synthetic::SyntheticSource;

    const W: u32 = 640;
    const H: u32 = 360;
    const FPS: u32 = 30;

    let (mut signal, mut endpoint, socket, video_mid) =
        connect_h264(signal_url, room, Role::Publisher);
    let mut encoder = X264Encoder::new(W, H, FPS, 800).expect("x264 encoder");
    let mut source = SyntheticSource::new(W, H);
    let mut connected = false;
    let mut next_frame = Instant::now();
    let mut pts = 0i64;

    loop {
        let wait = Duration::from_millis(5);
        socket.set_read_timeout(Some(wait)).ok();
        let mut buf = [0u8; 2000];
        match socket.recv_from(&mut buf) {
            Ok((n, source)) => {
                if let Ok(contents) = buf[..n].try_into() {
                    let input = Input::Receive(
                        Instant::now(),
                        Receive {
                            proto: Protocol::Udp,
                            source,
                            destination: socket.local_addr().unwrap(),
                            contents,
                        },
                    );
                    let _ = endpoint.handle_input(input);
                }
            }
            Err(_) => {}
        }
        let _ = endpoint.handle_timeout(Instant::now());

        let mut deadline = Instant::now() + Duration::from_secs(1);
        while let Some(output) = endpoint.poll_output() {
            match output {
                Output::Transmit(t) => {
                    let _ = socket.send_to(&t.contents, t.destination);
                }
                Output::Timeout(t) => {
                    deadline = deadline.min(t);
                    break;
                }
                Output::Event(_) => {}
            }
        }

        while let Some(ev) = endpoint.poll_event() {
            match ev {
                ClientEvent::IceConnected => {
                    info!("ICE connected, starting x264 stream");
                    connected = true;
                    next_frame = Instant::now();
                }
                ClientEvent::Closed => {
                    info!("connection closed");
                    return;
                }
                _ => {}
            }
        }

        // 30fps 节奏编码发送
        if connected && Instant::now() >= next_frame {
            next_frame += Duration::from_millis(1000 / FPS as u64);
            let rgb = source.next_frame();
            if let Some(frame) = encoder.encode(rgb).expect("encode") {
                let rtp_time = str0m::media::MediaTime::new(
                    (pts as u64 * 3000) as u64,
                    str0m::media::Frequency::NINETY_KHZ,
                );
                if let Err(e) = endpoint.send_video_frame(video_mid, frame.data, rtp_time) {
                    warn!("send frame failed: {e:?}");
                }
                if frame.keyframe {
                    info!("sent keyframe (pts {pts})");
                }
            }
            pts += 1;
        }

        std::thread::sleep(Duration::from_millis(2));
        let _ = &mut signal;
    }
}

/// VideoToolbox 硬编发布端：合成 BGRA → 硬编 → SFU。
fn publisher_vt(signal_url: &str, room: &str) {
    use aerodesk_macos::synthetic::SyntheticSource;
    use aerodesk_macos::vt_encoder::{VtEncoder, avcc_to_annexb};

    const W: u32 = 640;
    const H: u32 = 360;
    const FPS: u32 = 30;

    let (mut signal, mut endpoint, socket, video_mid) =
        connect_h264(signal_url, room, Role::Publisher);
    let mut encoder = VtEncoder::new(W, H, FPS, 800_000).expect("vt encoder");
    let mut source = SyntheticSource::new(W, H);
    let mut connected = false;
    let mut next_frame = Instant::now();

    loop {
        let wait = Duration::from_millis(5);
        socket.set_read_timeout(Some(wait)).ok();
        let mut buf = [0u8; 2000];
        if let Ok((n, source)) = socket.recv_from(&mut buf)
            && let Ok(contents) = buf[..n].try_into()
        {
            let input = Input::Receive(
                Instant::now(),
                Receive {
                    proto: Protocol::Udp,
                    source,
                    destination: socket.local_addr().unwrap(),
                    contents,
                },
            );
            let _ = endpoint.handle_input(input);
        }
        let _ = endpoint.handle_timeout(Instant::now());

        let mut deadline = Instant::now() + Duration::from_secs(1);
        while let Some(output) = endpoint.poll_output() {
            match output {
                Output::Transmit(t) => {
                    let _ = socket.send_to(&t.contents, t.destination);
                }
                Output::Timeout(t) => {
                    deadline = deadline.min(t);
                    break;
                }
                Output::Event(_) => {}
            }
        }

        while let Some(ev) = endpoint.poll_event() {
            match ev {
                ClientEvent::IceConnected => {
                    info!("ICE connected, starting VideoToolbox stream");
                    connected = true;
                    next_frame = Instant::now();
                }
                ClientEvent::Closed => {
                    info!("connection closed");
                    return;
                }
                _ => {}
            }
        }

        if connected && Instant::now() >= next_frame {
            next_frame += Duration::from_millis(1000 / FPS as u64);
            let bgra = source.next_frame_bgra();
            match encoder.encode_bgra(bgra) {
                Ok(Some(frame)) => {
                    let annexb = avcc_to_annexb(&frame.data);
                    let rtp_time = str0m::media::MediaTime::new(
                        frame.presentation_time.0 as u64,
                        str0m::media::Frequency::NINETY_KHZ,
                    );
                    if let Err(e) = endpoint.send_video_frame(video_mid, annexb, rtp_time) {
                        warn!("send frame failed: {e:?}");
                    }
                }
                Ok(None) => {}
                Err(e) => warn!("vt encode: {e}"),
            }
        }

        std::thread::sleep(Duration::from_millis(2));
        let _ = &mut signal;
    }
}
