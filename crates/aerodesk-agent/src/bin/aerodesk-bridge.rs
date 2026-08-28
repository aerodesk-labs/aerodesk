//! #216 跨 PoP 媒体桥接客户端（M1 媒体 + M2 data channel）。
//!
//! 以 **Viewer** 身份连主 PoP（远端 `--remote-signal`）收媒体，以 **Publisher**
//! 身份连本 PoP（`--local-signal`）转发——`ClientEvent::Media` 拿到的是 str0m
//! 去包化的编码载荷（`MediaData.data`，如 H.264 NAL 单元），经本地端点
//! `Writer::write` **原样重打包**（新 RTP 头/SSRC，载荷不重编码）。
//!
//! #598 P1b 信令迁 SIP（双腿原走 JSON WSS join）：
//! - viewer 腿 = `connect_viewer_sip`（REGISTER 任意名 → INVITE 房间 AoR）；
//! - publisher 腿 = 内联 UAS：先 REGISTER（以 `--room` 为设备 AoR）即打
//!   READY_MARKER（编排器 bridge.rs 以 stdout 出现 "publisher leg:" 判就绪并
//!   才启动本 PoP viewer 来拨——marker 若等 ICE 会与「等来电」互锁，故 marker
//!   定格在注册完成时点）；主循环内收 INVITE → P2pCall Callee 静默接听 →
//!   双腿转发。后续新 INVITE 拒 busy（单会话，与旧 JSON 单 Join 一致）。
//!
//! 关键帧：本 PoP viewer 的 KeyframeRequest 回传到主 PoP publisher（viewer leg
//! `Writer::request_keyframe`），保证跨 PoP 加入后能拿到 IDR 解码。
//!
//! M2（data channel 桥）：按 label+内容白名单（input/file/cursor/cmd + control
//! 的显示器切换 {"display":N}，跳过 offer/answer 与 control 的选层请求）双向转发
//! ChannelData——本 PoP viewer 的输入/文件/显示器切换经 bridge 到主 PoP publisher，
//! 主 PoP 的剪贴板/文件/光标反向到本 PoP viewer。
//!
//! M6（#260）：媒体按 kind 转发（视频+音频），viewer 腿 with_audio 协商音频。
//!
//! 用法：
//! ```sh
//! aerodesk-bridge --remote-signal ws://127.0.0.1 --local-signal ws://127.0.0.1 \
//!   --room bridge-demo [--auth-token <token>] [--sip-transport udp|tls] \
//!   [--remote-sip-port <port>] [--local-sip-port <port>] [--codec <default>]
//! ```
//!
//! `--remote/--local-signal` 沿用旧名、仅作地址载体（ws/wss scheme 推导传输，
//! URL 端口不进信令面）；显式端口用 `--remote-sip-port/--local-sip-port`。
//! `--auth-token` = 自身 Digest 口令。`--codec` 保留兼容但当前仅接受 default/h264
//! （SIP 腿使用 str0m 默认编解码协商；hevc/vp9/av1 尚未接线，传入即退出报错）。

use std::sync::mpsc;
use std::time::{Duration, Instant};

use aerodesk_core::connect::{connect_viewer_sip, force_relay_env};
use aerodesk_core::endpoint::ClientEvent;
use aerodesk_core::media_socket::MediaSocket;
use aerodesk_core::p2p_call::{P2pCall, P2pCallConfig, P2pRole, offer_audio_mid, offer_video_mid};
use aerodesk_core::protocol::signal::Role;
use aerodesk_core::sip_link::{SipCallLink, SipLinkConfig, SipLinkEvent};
use str0m::media::{KeyframeRequestKind, MediaData};
use str0m::net::{Protocol, Receive};
use str0m::{Input, Output};

/// M2/M6 转发判定：按 label + 内容。
/// - input/file/cursor/cmd：直接放行；
/// - control：仅放行显示器切换 `{"display":N}`（#260），跳过选层（layer）请求
///   （由本 PoP SFU 处理）；
/// - offer/answer 等其它 label：不放行。
fn should_forward(label: &str, data: &[u8]) -> bool {
    match label {
        "input" | "file" | "cursor" | "cmd" => true,
        "control" => serde_json::from_slice::<serde_json::Value>(data)
            .ok()
            .is_some_and(|v| v.get("display").is_some()),
        _ => false,
    }
}

fn arg(args: &[String], key: &str) -> Option<String> {
    args.iter()
        .position(|a| a == key)
        .and_then(|i| args.get(i + 1))
        .cloned()
}

fn parse_codec(v: Option<&str>) -> Option<&'static str> {
    match v.as_deref() {
        None | Some("h264") | Some("default") => Some("h264"),
        other => {
            eprintln!("unsupported --codec {other:?} on SIP legs; use h264|default");
            std::process::exit(2);
        }
    }
}

fn parse_port(v: Option<&str>) -> u16 {
    v.and_then(|s| s.parse().ok()).unwrap_or(0)
}

/// 观看腿视图：run_viewer/pump 只需 endpoint+socket+video mid（LiveSession 子集）。
struct LegView {
    endpoint: aerodesk_core::Endpoint,
    socket: MediaSocket,
    video_mid: str0m::media::Mid,
}

/// 单次网络泵：排空 socket 输入 → handle_timeout → 发送所有待发输出。
fn pump_once(session: &mut LegView) {
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
    mut view: LegView,
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
                    if let Some(label) = view.endpoint.channel_label(cid)
                        && should_forward(&label, &data)
                        && data_to_local.send((label, binary, data)).is_err()
                    {
                        tracing::warn!("viewer: data channel closed, exiting");
                        return;
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
            if let Some(mut w) = view.endpoint.writer(view.video_mid) {
                match w.request_keyframe(None, kind) {
                    Ok(()) => tracing::info!("viewer: keyframe request {kind:?} -> remote"),
                    Err(e) => tracing::debug!("viewer: keyframe request failed: {e:?}"),
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

/// 由被叫 offer 构建本地媒体端点并生成 answer（供 link.accept）。
fn create_pub_answer(
    offer_sdp: &str,
    force_relay: bool,
) -> Result<
    (
        P2pCall,
        String,
        str0m::media::Mid,
        Option<str0m::media::Mid>,
    ),
    String,
> {
    let mut p2p = P2pCall::new(P2pCallConfig {
        role: P2pRole::Callee,
        device_role: Role::Publisher,
        codec: None,
        with_audio: false,
        with_camera: false,
        force_relay,
        bind: "0.0.0.0:0".parse().unwrap(),
        turn: aerodesk_core::turn_client::p2p_turn_transport(
            &std::env::var("AERO_TURN_URLS").unwrap_or_default(),
            &std::env::var("AERO_TURN_USERNAME").unwrap_or_default(),
            &std::env::var("AERO_TURN_CREDENTIAL").unwrap_or_default(),
        ),
        inline_candidates: true,
    })
    .map_err(|e| format!("端点创建失败：{e}"))?;
    let answer = p2p
        .accept_offer(offer_sdp)
        .map_err(|e| format!("accept_offer 失败：{e}"))?;
    let video_mid = offer_video_mid(offer_sdp).ok_or_else(|| "offer 无视频 m-line".to_string())?;
    Ok((p2p, answer, video_mid, offer_audio_mid(offer_sdp)))
}

/// 本地被叫腿的活动呼叫（INVITE 已接、ICE 阶段/已连通）。
struct PubCall {
    p2p: P2pCall,
    video_mid: str0m::media::Mid,
    audio_mid: Option<str0m::media::Mid>,
}

fn main() {
    tracing_subscriber::fmt()
        .with_env_filter(
            tracing_subscriber::EnvFilter::try_from_default_env().unwrap_or_else(|_| "info".into()),
        )
        .init();

    let args: Vec<String> = std::env::args().skip(1).collect();
    let remote_signal = arg(&args, "--remote-signal").unwrap_or_else(|| "ws://127.0.0.1".into());
    let local_signal = arg(&args, "--local-signal").unwrap_or_else(|| "ws://127.0.0.1".into());
    let room = arg(&args, "--room").unwrap_or_else(|| "bridge-demo".into());
    let auth = arg(&args, "--auth-token");
    let sip_transport = arg(&args, "--sip-transport");
    let remote_port = parse_port(arg(&args, "--remote-sip-port").as_deref());
    let local_port = parse_port(arg(&args, "--local-sip-port").as_deref());
    let _codec = parse_codec(arg(&args, "--codec").as_deref());
    tracing::info!(
        "bridge start: remote(view)={remote_signal} local(pub)={local_signal} room={room}"
    );

    // viewer leg（主 PoP，收流）。#598 P1b：connect_viewer_sip 内含
    // REGISTER→INVITE→Answered→ICE 收敛全流程；with_audio=true 保 #260 双 kind 转发。
    let force_relay = force_relay_env();
    let (_view_guard, v_endpoint, v_socket, view_video_mid, view_audio_mid, _camera_mid) =
        connect_viewer_sip(
            &remote_signal,
            &room,
            auth.as_deref(),
            force_relay,
            true,
            false,
            sip_transport.as_deref(),
            (remote_port != 0).then_some(remote_port),
        )
        .unwrap_or_else(|e| {
            eprintln!("fatal: viewer leg connect: {e}");
            std::process::exit(1);
        });
    tracing::info!("viewer leg: room={room} sdp=ok ice=connected (SIP)");
    // view_video_mid 为裸 Mid（connect_viewer_sip 已保证存在）。
    tracing::info!("viewer video mid: {view_video_mid:?}");
    // #260：音频 mid（可能为 None，仅转发有音频的发布流）。
    tracing::info!("viewer audio mid: {view_audio_mid:?}");

    // #216：viewer 腿一连上就立刻起泵线程（DTLS 握手/重传必须持续泵）——
    // 若先连 publisher 腿（TURN TCP 3s 超时 + ICE 等待），期间 viewer 腿
    // DTLS 无人泵，SFU 侧握手超时 → 间歇失败（已复现）。
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
                LegView {
                    endpoint: v_endpoint,
                    socket: v_socket,
                    video_mid: view_video_mid,
                },
                media_tx,
                cmd_rx,
                stats_tx,
                data_to_local_tx,
                data_to_remote_rx,
            )
        })
        .expect("spawn viewer thread");

    // publisher leg（本 PoP，转发）。#598 P1b：READY_MARKER 在 REGISTER 完成后
    // 即打（不等来电/ICE）——编排器据此启动本地 viewer 来完成 INVITE 握手；
    // 接听在下方主循环内联（桌面 UAS 流的收敛版，等待与转发共存一循环）。
    let mut pub_cfg = SipLinkConfig::from_parts(
        &local_signal,
        &room,
        auth.as_deref().unwrap_or(""),
        sip_transport.as_deref().unwrap_or({
            #[allow(clippy::match_like_matches_macro)]
            let tls = local_signal.starts_with("wss");
            if tls { "tls" } else { "udp" }
        }),
        local_port,
        "",
        "",
    )
    .unwrap_or_else(|e| {
        eprintln!("fatal: publisher leg config: {e}");
        std::process::exit(1);
    });
    if pub_cfg.transport == aerodesk_core::protocol::sip_client::SipTransport::Tls
        && pub_cfg.tls.is_none()
    {
        pub_cfg.tls = Some(aerodesk_core::protocol::sip_client::SipTlsConfig {
            ca_certs: aerodesk_core::protocol::sip_client::system_ca_pem(),
            sni_hostname: None,
            client_cert: None,
            client_key: None,
        });
    }
    let mut pub_link = SipCallLink::new(pub_cfg);
    pub_link.start();
    {
        let deadline = Instant::now() + Duration::from_secs(10);
        loop {
            let st = pub_link.poll();
            if st.is_online() {
                break;
            }
            if Instant::now() >= deadline {
                eprintln!("fatal: publisher leg 注册未完成（10s）：{st:?}");
                std::process::exit(1);
            }
            std::thread::sleep(Duration::from_millis(50));
        }
    }
    tracing::info!("publisher leg: room={room} registered (等待本 PoP viewer INVITE)");

    let mut active: Option<PubCall> = None;
    let mut forwarded = 0u64;
    let mut forwarded_kf = 0u64;
    let mut forwarded_audio = 0u64;
    let mut data_forwarded = 0u64;
    let mut data_forwarded_bytes = 0u64;
    let mut last_stats = Instant::now();
    // 初次加入可能错过首帧 IDR：向主 PoP publisher 连发 3 次 PLI（立即/1s/2s），
    // 保证远端 viewer 能拿到关键帧起流。仅在本腿已有活动呼叫后才触发
    // （SIP 腿注册 ≠ 已入会）。
    let mut kf_requests = 3u32;
    let mut next_kf_at: Option<Instant> = None;

    loop {
        // 信令泵：注册刷新 / 来电事件。
        let _ = pub_link.poll();
        for ev in pub_link.take_events() {
            match ev {
                SipLinkEvent::IncomingCall {
                    call_id, offer_sdp, ..
                } => {
                    if active.is_some() {
                        let _ = pub_link.reject(&call_id, "busy");
                        continue;
                    }
                    match create_pub_answer(&offer_sdp, force_relay) {
                        Ok((p2p, answer, video_mid, audio_mid)) => {
                            if pub_link.accept(&call_id, &answer).is_ok() {
                                tracing::info!("publisher leg: accepted incoming call");
                                next_kf_at = Some(Instant::now());
                                active = Some(PubCall {
                                    p2p,
                                    video_mid,
                                    audio_mid,
                                });
                            } else {
                                tracing::warn!("publisher leg: accept 失败");
                            }
                        }
                        Err(e) => {
                            tracing::warn!("publisher leg: incoming call 失败：{e}");
                            let _ = pub_link.reject(&call_id, "internal");
                        }
                    }
                }
                SipLinkEvent::PeerHangup { .. } => {
                    tracing::info!("publisher leg: 对端挂断，恢复待呼");
                    active = None;
                }
                SipLinkEvent::Rejected { status, .. } => {
                    tracing::info!("publisher leg: 呼叫被拒（{status}）");
                }
                _ => {}
            }
        }

        let Some(st) = active.as_mut() else {
            // 未入会：无媒体可转；保持统计心跳节律。
            if last_stats.elapsed() >= Duration::from_secs(5) {
                let (vm, vk) = stats_rx.try_recv().unwrap_or((0, 0));
                tracing::info!(
                    "bridge stats: viewer_media={vm} viewer_kf={vk} forwarded={forwarded} forwarded_kf={forwarded_kf} forwarded_audio={forwarded_audio} data_forwarded={data_forwarded} data_bytes={data_forwarded_bytes}"
                );
                last_stats = Instant::now();
            }
            std::thread::sleep(Duration::from_millis(2));
            continue;
        };

        // 泵本 PoP 会话（发送 + 收 RTCP/输入）
        if let Err(e) = st.p2p.poll() {
            tracing::debug!("publisher: poll {e:?}");
        }

        // M2：本 PoP viewer → 主 PoP publisher（input 等）——在 pub 侧接收并回发
        while let Some(ev) = st.p2p.endpoint().poll_event() {
            match ev {
                ClientEvent::ChannelData(cid, binary, data) => {
                    if let Some(label) = st.p2p.endpoint().channel_label(cid)
                        && should_forward(&label, &data)
                    {
                        data_forwarded += 1;
                        data_forwarded_bytes += data.len() as u64;
                        if data_to_remote_tx.send((label, binary, data)).is_err() {
                            tracing::warn!("publisher: data channel closed, exiting");
                            std::process::exit(1);
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
            if !st.p2p.endpoint().send_channel_data(&label, binary, &data) {
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

        // 转发媒体：原样重打包（不重编码）。#260：按 kind 选 writer——
        // viewer leg 的 audio mid 对应 local leg 的 audio mid，其余按 video。
        while let Ok(md) = media_rx.try_recv() {
            let target_mid = if view_audio_mid == Some(md.mid) {
                st.audio_mid
            } else {
                Some(st.video_mid)
            };
            let Some(w) = target_mid.and_then(|mid| st.p2p.endpoint().writer(mid)) else {
                tracing::debug!("publisher: no writer for mid {target_mid:?}");
                break;
            };
            let pt = w.match_params(md.params.clone()).unwrap_or(md.pt);
            match w.write(pt, Instant::now(), md.time, md.data.clone()) {
                Ok(()) => {
                    if view_audio_mid == Some(md.mid) {
                        forwarded_audio += 1;
                    } else {
                        forwarded += 1;
                        if md.is_keyframe() {
                            forwarded_kf += 1;
                            tracing::info!("publisher: forwarded keyframe #{forwarded_kf}");
                        }
                    }
                }
                Err(e) => tracing::debug!("publisher: write {e:?}"),
            }
        }

        if last_stats.elapsed() >= Duration::from_secs(5) {
            let (vm, vk) = stats_rx.try_recv().unwrap_or((0, 0));
            tracing::info!(
                "bridge stats: viewer_media={vm} viewer_kf={vk} forwarded={forwarded} forwarded_kf={forwarded_kf} forwarded_audio={forwarded_audio} data_forwarded={data_forwarded} data_bytes={data_forwarded_bytes}"
            );
            last_stats = Instant::now();
        }
        std::thread::sleep(Duration::from_millis(2));
    }
}

#[cfg(test)]
mod tests {
    use super::should_forward;

    #[test]
    fn whitelist_labels_forwarded() {
        for label in ["input", "file", "cursor", "cmd"] {
            assert!(should_forward(label, b"{}"), "{label} 应放行");
        }
    }

    #[test]
    fn control_display_forwarded_layer_skipped() {
        assert!(should_forward("control", br#"{"display":1}"#));
        assert!(should_forward("control", br#"{"display":0}"#));
        assert!(
            !should_forward("control", br#"{"layer":"f"}"#),
            "选层请求由本 PoP SFU 处理"
        );
        assert!(!should_forward("control", b"not-json"), "非法 JSON 不放行");
        assert!(!should_forward("control", b"{}"), "空对象不放行");
    }

    #[test]
    fn signaling_labels_never_forwarded() {
        assert!(!should_forward("offer/answer", b"{}"));
        assert!(!should_forward("answer", b"{}"));
        // 混合消息含 display 也放行（display 优先）。
        assert!(should_forward("control", br#"{"display":1,"layer":"f"}"#));
    }
}
