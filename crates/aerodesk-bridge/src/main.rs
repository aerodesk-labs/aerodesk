//! #216 跨 PoP 媒体桥接客户端（M1 媒体 + M2 data channel）。
//!
//! 以 **Viewer** 身份连主 PoP（远端 `--remote-signal`）收媒体，以 **Publisher**
//! 身份连本 PoP（`--local-signal`）转发——`ClientEvent::Media` 拿到的是 str0m
//! 去包化的编码载荷（`MediaData.data`，如 H.264 NAL 单元），经本地端点
//! `Writer::write` **原样重打包**（新 RTP 头/SSRC，载荷不重编码）。
//!
//! 关键帧：本 PoP viewer 的 KeyframeRequest 回传到主 PoP publisher（viewer leg
//! `Writer::request_keyframe`），保证跨 PoP 加入后能拿到 IDR 解码。
//!
//! M2（data channel 桥）：按 label 白名单（input/file/cursor/cmd，跳过
//! offer/answer 与 control）双向转发 ChannelData——本 PoP viewer 的输入/文件
//! 经 bridge 到主 PoP publisher，主 PoP 的剪贴板/文件/光标反向到本 PoP viewer。
//!
//! 用法：
//! ```sh
//! aerodesk-bridge --remote-signal ws://127.0.0.1:14603 --local-signal ws://127.0.0.1:14703 \
//!   --room bridge-demo [--auth-token <token>] [--codec h264|hevc|vp9|av1|default]
//! ```
//!
//! `--auth-token` 传给双腿 Join（生产信令开启 JWT/静态 token 时必填；#216 M3 编排
//! 中经 `BRIDGE_CMD` 的 `$BRIDGE_AUTH_TOKEN` 注入）。

use std::sync::mpsc;
use std::time::{Duration, Instant};

use aerodesk_core::connect::{LiveSession, connect_live_role_codec};
use aerodesk_core::endpoint::ClientEvent;
use aerodesk_core::media_pipeline::Codec;
use aerodesk_protocol::signal::Role;
use str0m::media::{KeyframeRequestKind, MediaData};
use str0m::net::{Protocol, Receive};
use str0m::{Input, Output};

/// M2 转发白名单：跳过 offer/answer（信令 SDP）与 control（viewer→SFU 选层）。
fn is_forwardable_label(label: &str) -> bool {
    matches!(label, "input" | "file" | "cursor" | "cmd")
}

fn arg(args: &[String], key: &str) -> Option<String> {
    args.iter()
        .position(|a| a == key)
        .and_then(|i| args.get(i + 1))
        .cloned()
}

fn parse_codec(v: Option<&str>) -> Option<Codec> {
    match v {
        None | Some("h264") => Some(Codec::H264),
        Some("hevc") => Some(Codec::Hevc),
        Some("vp9") => Some(Codec::Vp9),
        Some("av1") => Some(Codec::Av1),
        Some("default") => None,
        other => {
            eprintln!("unknown --codec {other:?}; use h264|hevc|vp9|av1|default");
            std::process::exit(2);
        }
    }
}

/// 单次网络泵：排空 socket 输入 → handle_timeout → 发送所有待发输出。
fn pump_once(session: &mut LiveSession) {
    let mut buf = [0u8; 2000];
    loop {
        match session.socket.recv_from(&mut buf) {
            Ok((n, source)) => {
                let Ok(contents) = buf[..n].try_into() else {
                    continue;
                };
                let _ = session.endpoint.handle_input(Input::Receive(
                    Instant::now(),
                    Receive {
                        proto: Protocol::Udp,
                        source,
                        destination: session.socket.local_addr().unwrap_or(source),
                        contents,
                    },
                ));
            }
            Err(e) => {
                if e.kind() != std::io::ErrorKind::WouldBlock
                    && e.kind() != std::io::ErrorKind::TimedOut
                {
                    tracing::debug!("recv error: {e:?}");
                }
                break;
            }
        }
    }
    let _ = session.endpoint.handle_timeout(Instant::now());
    while let Some(output) = session.endpoint.poll_output() {
        match output {
            Output::Transmit(t) => {
                let _ = session.socket.send_to(&t.contents, t.destination);
            }
            Output::Timeout(_) => break,
            Output::Event(_) => {}
        }
    }
}

/// viewer leg 线程：泵远端会话，把媒体载荷发给主线程转发；接收关键帧请求并回传。
fn run_viewer(
    mut view: LiveSession,
    media_tx: mpsc::Sender<MediaData>,
    cmd_rx: mpsc::Receiver<KeyframeRequestKind>,
    stats_tx: mpsc::Sender<(u64, u64)>,
    data_to_local: mpsc::Sender<(String, bool, Vec<u8>)>,
    data_from_local: mpsc::Receiver<(String, bool, Vec<u8>)>,
) {
    let mut media = 0u64;
    let mut kf = 0u64;
    let mut last_stats = Instant::now();
    loop {
        pump_once(&mut view);
        while let Some(ev) = view.endpoint.poll_event() {
            match ev {
                ClientEvent::Media(md) => {
                    media += 1;
                    if md.is_keyframe() {
                        kf += 1;
                    }
                    if media_tx.send(md).is_err() {
                        tracing::warn!("viewer: forward channel closed, exiting");
                        return;
                    }
                }
                // M2：主 PoP publisher → 本 PoP viewer（剪贴板/文件/光标等）。
                ClientEvent::ChannelData(cid, binary, data) => {
                    if let Some(label) = view.endpoint.channel_label(cid) {
                        if is_forwardable_label(&label)
                            && data_to_local.send((label, binary, data)).is_err()
                        {
                            tracing::warn!("viewer: data channel closed, exiting");
                            return;
                        }
                    }
                }
                ClientEvent::Closed => {
                    tracing::warn!("viewer: remote session closed, exiting");
                    return;
                }
                _ => {}
            }
        }
        // M2：本 PoP viewer → 主 PoP publisher（input 等）。
        while let Ok((label, binary, data)) = data_from_local.try_recv() {
            if !view.endpoint.send_channel_data(&label, binary, &data) {
                tracing::debug!("viewer: send {label} dropped (channel not open)");
            } else if label == "input" {
                tracing::debug!("viewer: forwarded input {} bytes to remote", data.len());
            }
        }
        if let Ok(kind) = cmd_rx.try_recv() {
            let mid = view.video_mid;
            if let Some(mid) = mid {
                if let Some(mut w) = view.endpoint.writer(mid) {
                    match w.request_keyframe(None, kind) {
                        Ok(()) => tracing::info!("viewer: keyframe request {kind:?} -> remote"),
                        Err(e) => tracing::debug!("viewer: keyframe request failed: {e:?}"),
                    }
                }
            }
        }
        if last_stats.elapsed() >= Duration::from_secs(5) {
            let _ = stats_tx.send((media, kf));
            last_stats = Instant::now();
        }
        std::thread::sleep(Duration::from_millis(2));
    }
}

fn main() {
    tracing_subscriber::fmt()
        .with_env_filter(
            tracing_subscriber::EnvFilter::try_from_default_env().unwrap_or_else(|_| "info".into()),
        )
        .init();

    let args: Vec<String> = std::env::args().skip(1).collect();
    let remote_signal =
        arg(&args, "--remote-signal").unwrap_or_else(|| "ws://127.0.0.1:14603".into());
    let local_signal =
        arg(&args, "--local-signal").unwrap_or_else(|| "ws://127.0.0.1:14703".into());
    let room = arg(&args, "--room").unwrap_or_else(|| "bridge-demo".into());
    let auth = arg(&args, "--auth-token");
    let codec = parse_codec(arg(&args, "--codec").as_deref());
    tracing::info!(
        "bridge start: remote(view)={remote_signal} local(pub)={local_signal} room={room} codec={codec:?}"
    );

    // viewer leg（主 PoP，收流）
    let view = connect_live_role_codec(&remote_signal, &room, Role::Viewer, auth.as_deref(), codec)
        .expect("viewer leg connect");
    tracing::info!("viewer leg: {}", view.summary());
    let Some(view_mid) = view.video_mid else {
        eprintln!("fatal: no video mid on viewer leg");
        std::process::exit(1);
    };
    tracing::info!("viewer video mid: {view_mid:?}");

    // publisher leg（本 PoP，转发）
    let mut local = connect_live_role_codec(
        &local_signal,
        &room,
        Role::Publisher,
        auth.as_deref(),
        codec,
    )
    .expect("publisher leg connect");
    tracing::info!("publisher leg: {}", local.summary());
    let Some(local_mid) = local.video_mid else {
        eprintln!("fatal: no video mid on publisher leg");
        std::process::exit(1);
    };
    tracing::info!("publisher video mid: {local_mid:?}");

    let (media_tx, media_rx) = mpsc::channel::<MediaData>();
    let (cmd_tx, cmd_rx) = mpsc::channel::<KeyframeRequestKind>();
    let (stats_tx, stats_rx) = mpsc::channel::<(u64, u64)>();
    let (data_to_local_tx, data_to_local_rx) = mpsc::channel::<(String, bool, Vec<u8>)>();
    let (data_to_remote_tx, data_to_remote_rx) = mpsc::channel::<(String, bool, Vec<u8>)>();
    // #230：大块 data channel（文件 8KB/块）转发时 str0m SCTP 发送在 2MB 默认栈
    // 下栈溢出（与 SFU 同款问题，见 LESSON 线程栈溢出）——放大到 16MB。
    std::thread::Builder::new()
        .name("bridge-viewer".into())
        .stack_size(16 * 1024 * 1024)
        .spawn(move || {
            run_viewer(
                view,
                media_tx,
                cmd_rx,
                stats_tx,
                data_to_local_tx,
                data_to_remote_rx,
            )
        })
        .expect("spawn viewer thread");

    let mut forwarded = 0u64;
    let mut forwarded_kf = 0u64;
    let mut data_forwarded = 0u64;
    let mut data_forwarded_bytes = 0u64;
    let mut last_stats = Instant::now();
    // 初次加入可能错过首帧 IDR：向主 PoP publisher 连发 3 次 PLI（立即/1s/2s），
    // 保证远端 viewer 能拿到关键帧起流。
    let mut kf_requests = 3u32;
    let mut next_kf_at = Some(Instant::now());
    loop {
        // 泵本 PoP 会话（发送 + 收 RTCP/输入）
        pump_once(&mut local);

        // M2：本 PoP viewer → 主 PoP publisher（input 等）——在 pub 侧接收并回发
        while let Some(ev) = local.endpoint.poll_event() {
            match ev {
                ClientEvent::ChannelData(cid, binary, data) => {
                    if let Some(label) = local.endpoint.channel_label(cid) {
                        if is_forwardable_label(&label) {
                            data_forwarded += 1;
                            data_forwarded_bytes += data.len() as u64;
                            if data_to_remote_tx.send((label, binary, data)).is_err() {
                                tracing::warn!("publisher: data channel closed, exiting");
                                std::process::exit(1);
                            }
                        }
                    }
                }
                ClientEvent::KeyframeRequest(kr) => {
                    tracing::info!("publisher: keyframe request from local viewer {kr:?}");
                    let _ = cmd_tx.send(kr.kind);
                }
                ClientEvent::Closed => {
                    tracing::warn!("publisher: local session closed, exiting");
                    std::process::exit(1);
                }
                _ => {}
            }
        }
        // M2：主 PoP publisher → 本 PoP viewer（剪贴板/文件/光标等）
        while let Ok((label, binary, data)) = data_to_local_rx.try_recv() {
            if !local.endpoint.send_channel_data(&label, binary, &data) {
                tracing::debug!("publisher: send {label} dropped (channel not open)");
            }
        }

        // 初始关键帧请求（加入补偿）
        if let Some(t) = next_kf_at
            && Instant::now() >= t
            && kf_requests > 0
        {
            let _ = cmd_tx.send(KeyframeRequestKind::Pli);
            kf_requests -= 1;
            tracing::info!(
                "bridge: initial PLI to remote publisher ({} left)",
                kf_requests
            );
            next_kf_at = Some(Instant::now() + Duration::from_secs(1));
        }

        // 转发媒体：原样重打包（不重编码）
        while let Ok(md) = media_rx.try_recv() {
            let Some(w) = local.endpoint.writer(local_mid) else {
                tracing::debug!("publisher: no writer for mid {local_mid:?}");
                break;
            };
            let pt = w.match_params(md.params.clone()).unwrap_or(md.pt);
            match w.write(pt, Instant::now(), md.time, md.data.clone()) {
                Ok(()) => {
                    forwarded += 1;
                    if md.is_keyframe() {
                        forwarded_kf += 1;
                        tracing::info!("publisher: forwarded keyframe #{forwarded_kf}");
                    }
                }
                Err(e) => tracing::debug!("publisher: write {e:?}"),
            }
        }

        if last_stats.elapsed() >= Duration::from_secs(5) {
            let (vm, vk) = stats_rx.try_recv().unwrap_or((0, 0));
            tracing::info!(
                "bridge stats: viewer_media={vm} viewer_kf={vk} forwarded={forwarded} forwarded_kf={forwarded_kf} data_forwarded={data_forwarded} data_bytes={data_forwarded_bytes}"
            );
            last_stats = Instant::now();
        }
        std::thread::sleep(Duration::from_millis(2));
    }
}
