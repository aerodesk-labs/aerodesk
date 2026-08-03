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
use aerodesk_protocol::input::{INPUT_PROTOCOL_VERSION, InputEvent, InputFrame};
use aerodesk_protocol::signal::Role;
use str0m::media::{Frequency, MediaTime};
use str0m::net::Protocol;
use str0m::{Input, Output, net::Receive};

fn main() {
    init_log();
    let args: Vec<String> = std::env::args().collect();
    if args.iter().any(|a| a == "--issue-token") {
        issue_token(&args);
        return;
    }
    let role = arg(&args, "--role").unwrap_or_else(|| "viewer".into());
    let signal = arg(&args, "--signal").unwrap_or_else(|| "ws://127.0.0.1:3003/ws".into());
    let signal = if signal.contains("/ws") {
        signal
    } else {
        format!("{signal}/ws")
    };
    let room = arg(&args, "--room").unwrap_or_else(|| "demo".into());
    let token = arg(&args, "--token");
    let encoder = arg(&args, "--encoder").unwrap_or_else(|| "pcap".into());

    match role.as_str() {
        "publisher" if encoder == "screen" => publisher_capture(&signal, &room, token.as_deref()),
        "publisher" if encoder == "vt" => {
            let w: u32 = arg(&args, "--width")
                .and_then(|v| v.parse().ok())
                .unwrap_or(640);
            let h: u32 = arg(&args, "--height")
                .and_then(|v| v.parse().ok())
                .unwrap_or(360);
            let fps: u32 = arg(&args, "--fps")
                .and_then(|v| v.parse().ok())
                .unwrap_or(30);
            let br: u32 = arg(&args, "--bitrate")
                .and_then(|v| v.parse().ok())
                .unwrap_or(800_000);
            publisher_vt(&signal, &room, token.as_deref(), w, h, fps, br);
        }
        "publisher" if encoder == "x264" => publisher_x264(&signal, &room, token.as_deref()),
        "publisher" => publisher(&signal, &room, token.as_deref()),
        "viewer" => viewer(&signal, &room, token.as_deref()),
        other => panic!("unknown role {other}"),
    }
}

/// 签发信令 JWT（供运维/测试使用）。
///
/// 用法：
///   JWT_SECRET=<secret> aerodesk-cli --issue-token --user u1 --device mac-1 --room demo --role publisher --ttl 3600
///   JWT_SECRET=<secret> aerodesk-cli --issue-token --user u1 --room demo --role "*" --ttl 86400
fn issue_token(args: &[String]) {
    let secret = match std::env::var("JWT_SECRET") {
        Ok(s) if !s.is_empty() => s,
        _ => {
            eprintln!("JWT_SECRET 环境变量未设置");
            std::process::exit(1);
        }
    };
    let user = arg(args, "--user").unwrap_or_else(|| {
        eprintln!("缺少 --user");
        std::process::exit(1);
    });
    let device = arg(args, "--device");
    let room = arg(args, "--room");
    let role = match arg(args, "--role").as_deref() {
        Some("publisher") => Some(Role::Publisher),
        Some("viewer") => Some(Role::Viewer),
        Some("*") | None => None,
        Some(other) => {
            eprintln!("unknown role: {other} (publisher/viewer/*)");
            std::process::exit(1);
        }
    };
    let ttl: u64 = arg(args, "--ttl")
        .and_then(|t| t.parse().ok())
        .unwrap_or(3600);

    match aerodesk_protocol::jwt::mint_token(
        &secret,
        &user,
        device.as_deref(),
        room.as_deref(),
        role,
        ttl,
    ) {
        Ok(token) => println!("{token}"),
        Err(e) => {
            eprintln!("签发失败: {e}");
            std::process::exit(1);
        }
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
    auth: Option<&str>,
) -> (WsSignalClient, Endpoint, UdpSocket, str0m::media::Mid) {
    connect_inner(signal_url, room, role, false, auth)
}

fn connect_h264(
    signal_url: &str,
    room: &str,
    role: Role,
    auth: Option<&str>,
) -> (WsSignalClient, Endpoint, UdpSocket, str0m::media::Mid) {
    connect_inner(signal_url, room, role, true, auth)
}

fn connect_inner(
    signal_url: &str,
    room: &str,
    role: Role,
    h264_only: bool,
    auth: Option<&str>,
) -> (WsSignalClient, Endpoint, UdpSocket, str0m::media::Mid) {
    let mut signal = WsSignalClient::connect(signal_url).expect("signal connect");
    let (peer_id, turn) = signal.join(room, role, auth).expect("join");
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

    // #12：viewer 的 offer 用 recvonly（SFU 拒绝 viewer 发布媒体）。
    if role == Role::Viewer {
        endpoint.add_video_recvonly();
    } else {
        endpoint.add_video();
    }
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

/// 发布端公共事件处理：输入通道（观看端 → 被控端）。
fn handle_publisher_input(endpoint: &mut Endpoint, ev: ClientEvent) {
    match ev {
        ClientEvent::ChannelOpen(label, _) if label == "input" => {
            info!("input channel open");
        }
        ClientEvent::ChannelData(cid, _, data) => {
            if endpoint.channel_label(cid).as_deref() == Some("input") {
                if let Ok(frame) = serde_json::from_slice::<InputFrame>(&data) {
                    info!("input: seq={} {:?}", frame.seq, frame.event);
                }
            }
        }
        _ => {}
    }
}

fn publisher(signal_url: &str, room: &str, auth: Option<&str>) {
    let pcap = include_bytes!("../../../crates/aerodesk-core/tests/data/vp8.pcap");
    let frames = parse_vp8_pcap(pcap);
    info!("loaded {} VP8 frames from pcap", frames.len());

    let (mut signal, mut endpoint, socket, video_mid) =
        connect(signal_url, room, Role::Publisher, auth);
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
                ev => handle_publisher_input(&mut endpoint, ev),
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

fn viewer(signal_url: &str, room: &str, auth: Option<&str>) {
    let (mut signal, mut endpoint, socket, _) = connect(signal_url, room, Role::Viewer, auth);
    let mut frames = 0u64;
    let mut bytes = 0u64;
    let mut keyframes = 0u64;
    let mut last_report = Instant::now();
    let mut input_open = false;
    let mut input_seq = 0u64;
    let mut last_input = Instant::now();

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
                ClientEvent::ChannelOpen(label, _) if label == "input" => {
                    info!("input channel open");
                    input_open = true;
                }
                ClientEvent::Closed => {
                    info!("connection closed");
                    return;
                }
                _ => {}
            }
        }

        // 输入事件回传：input 通道打开后周期性发送鼠标移动（模拟观看端输入）。
        if input_open && last_input.elapsed() >= Duration::from_millis(100) {
            let frame = InputFrame {
                version: INPUT_PROTOCOL_VERSION,
                seq: input_seq,
                timestamp_ms: std::time::SystemTime::now()
                    .duration_since(std::time::UNIX_EPOCH)
                    .map(|d| d.as_millis() as u64)
                    .unwrap_or(0),
                event: InputEvent::MouseMove { x: 0.5, y: 0.5 },
            };
            if let Ok(json) = serde_json::to_string(&frame) {
                if endpoint.send_channel_data("input", false, json.as_bytes()) {
                    input_seq += 1;
                    last_input = Instant::now();
                }
            }
        }

        if last_report.elapsed() >= Duration::from_secs(2) {
            info!(
                "RECEIVED: {frames} frames, {bytes} bytes, {keyframes} keyframes, input sent: {input_seq}"
            );
            last_report = Instant::now();
        }
        std::thread::sleep(Duration::from_millis(2));
        let _ = &mut signal;
    }
}

/// x264 发布端：合成帧 → H.264 编码 → SFU。
fn publisher_x264(signal_url: &str, room: &str, auth: Option<&str>) {
    use aerodesk_macos::encoder::X264Encoder;
    use aerodesk_macos::synthetic::SyntheticSource;

    const W: u32 = 640;
    const H: u32 = 360;
    const FPS: u32 = 30;

    let (mut signal, mut endpoint, socket, video_mid) =
        connect_h264(signal_url, room, Role::Publisher, auth);
    let mut encoder = X264Encoder::new(W, H, FPS, 800).expect("x264 encoder");
    let mut source = SyntheticSource::new(W, H);
    let mut connected = false;
    let mut next_frame = Instant::now();
    let mut pts = 0i64;

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
                    info!("ICE connected, starting x264 stream");
                    connected = true;
                    next_frame = Instant::now();
                }
                ClientEvent::Closed => {
                    info!("connection closed");
                    return;
                }
                ev => handle_publisher_input(&mut endpoint, ev),
            }
        }

        // 30fps 节奏编码发送
        if connected && Instant::now() >= next_frame {
            next_frame += Duration::from_millis(1000 / FPS as u64);
            let rgb = source.next_frame();
            if let Some(frame) = encoder.encode(rgb).expect("encode") {
                let rtp_time = str0m::media::MediaTime::new(
                    pts as u64 * 3000,
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
/// 压测可传 --width/--height/--fps/--bitrate（如 3840x2160@60 8Mbps）。
fn publisher_vt(
    signal_url: &str,
    room: &str,
    auth: Option<&str>,
    width: u32,
    height: u32,
    fps: u32,
    bitrate: u32,
) {
    use aerodesk_macos::synthetic::SyntheticSource;
    use aerodesk_macos::vt_encoder::{VtEncoder, avcc_to_annexb};

    let (mut signal, mut endpoint, socket, video_mid) =
        connect_h264(signal_url, room, Role::Publisher, auth);
    let mut encoder = VtEncoder::new(width, height, fps, bitrate).expect("vt encoder");
    let mut source = SyntheticSource::new(width, height);
    info!("VT publisher: {width}x{height}@{fps} {bitrate}bps");
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
                ev => handle_publisher_input(&mut endpoint, ev),
            }
        }

        if connected && Instant::now() >= next_frame {
            next_frame += Duration::from_millis(1000 / fps as u64);
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

/// 真实屏幕采集发布端：ScreenCaptureKit → VideoToolbox 硬编（零拷贝）→ SFU。
/// 需要屏幕录制权限（TCC）。
fn publisher_capture(signal_url: &str, room: &str, auth: Option<&str>) {
    use aerodesk_macos::capture::ScreenCapture;
    use aerodesk_macos::vt_encoder::{VtEncoder, avcc_to_annexb};

    const W: u32 = 1920;
    const H: u32 = 1080;
    const FPS: u32 = 30;

    let (mut signal, mut endpoint, socket, video_mid) =
        connect_h264(signal_url, room, Role::Publisher, auth);
    let mut capture = match ScreenCapture::start(0, FPS, W, H) {
        Ok(c) => c,
        Err(e) => {
            error!("screen capture init failed: {e}");
            info!("grant Screen Recording permission in System Settings > Privacy & Security");
            return;
        }
    };
    let mut encoder = VtEncoder::new(W, H, FPS, 8_000_000).expect("vt encoder");
    let mut connected = false;

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
                    info!("ICE connected, starting screen capture stream");
                    connected = true;
                }
                ClientEvent::Closed => {
                    info!("connection closed");
                    return;
                }
                ev => handle_publisher_input(&mut endpoint, ev),
            }
        }

        if connected && let Some(surface) = capture.next_frame(Duration::from_millis(50)) {
            match encoder.encode_surface(&surface) {
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
