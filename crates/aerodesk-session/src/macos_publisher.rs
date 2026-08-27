//! macOS 桌面端被控端（屏幕发布，#487 互控最高优先级第一批）：
//! SCK 采集 + VideoToolbox 硬编 + CGEvent 输入注入 + SCK 系统音频。
//! 结构镜像 `generic_publisher::imp`（Windows DXGI/OpenH264/WASAPI 版本），
//! 平台差异收敛在采集/编码/注入/音频四个适配点；循环骨架（网络泵/节拍/
//! 事件分发）与 Windows 版一致。
//!
//! 接入方式：`generic_publisher::start_publisher` 在 macOS 目标转发到本模块
//! （调用点与定义点同 cfg 门控，RULE 可达性）。
//!
//! #508 B1：启动参数为 [`crate::PublisherConfig`] 快照，UI 副作用经
//! [`crate::PublisherEvent`] 回调回传，本模块不再引用 Slint/UI 类型。

use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use aerodesk_codec::audio::RealAudioSender;
use aerodesk_core::connect::{LiveSession, connect_live_role_codec_timeout};
use aerodesk_core::endpoint::ClientEvent;
use aerodesk_core::platform::Codec;
use aerodesk_core::platform::CursorSource;
use aerodesk_core::platform::SystemWakeLock;
use aerodesk_core::protocol::cmd::{CmdAction, CmdRequest, CmdResponse, CmdResult};
use aerodesk_core::protocol::cursor::CursorPos;
use aerodesk_core::protocol::input::{InputEvent, InputFrame};
use aerodesk_core::protocol::signal::Role;
use str0m::Output;
use str0m::media::{Frequency, MediaTime};
use str0m::net::Protocol;

use crate::PublisherEvent;
use crate::generic_publisher::PublisherEventSink;

const FPS: u32 = 30;
const DEFAULT_BITRATE_BPS: u32 = 8_000_000;

/// 发送侧墙钟（毫秒）：光标/延迟测量的 sent_ms 字段，与 Windows/CLI 同口径。
fn now_ms() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_millis() as u64)
        .unwrap_or(0)
}

/// 当前发布线程的 stop 句柄（同一时刻只允许一个被控端发布线程）。
static STOP: Mutex<Option<Arc<AtomicBool>>> = Mutex::new(None);

/// 启动 macOS 被控端（SCK 采集 + VT 硬编 + CGEvent 注入 + SCK 系统音频）。
pub fn start_publisher(cfg: crate::PublisherConfig, on_event: PublisherEventSink) {
    // 同一时刻仅一个发布线程：先停止旧线程再启动新配置。
    stop_publisher(on_event.clone());

    let crate::PublisherConfig {
        server,
        room: raw_room,
        token,
        audio,
        mouse,
        view_only,
    } = cfg;
    let Some(room) = crate::generic_publisher::valid_publisher_room(&raw_room) else {
        on_event(PublisherEvent::StartFailed(
            "被控端启动失败：本机 ID 无效".into(),
        ));
        return;
    };

    let stop = Arc::new(AtomicBool::new(false));
    *STOP.lock().unwrap() = Some(stop.clone());
    on_event(PublisherEvent::Starting);
    // 数据通道收发链（str0m/SCTP）调用栈深，放大线程栈防溢出（RULE 同款）。
    let sink = on_event.clone();
    if std::thread::Builder::new()
        .stack_size(16 * 1024 * 1024)
        .spawn(move || run_publisher(server, room, token, sink, audio, mouse, view_only, stop))
        .is_err()
    {
        *STOP.lock().unwrap() = None;
        on_event(PublisherEvent::StartFailed(
            "被控端启动失败：无法创建线程".into(),
        ));
    }
}

/// #552：SIP 1:1 P2P 被叫发布入口（P2pCall 已建立——accept_offer+accept 由
/// 调用方完成），SCK 采集/VT 编码/CGEvent 注入与 SFU 路径共用同一泵。
pub fn start_publisher_peer(
    p2p: aerodesk_core::p2p_call::P2pCall,
    video_mid: str0m::media::Mid,
    room: String,
    trickle_rx: Option<std::sync::mpsc::Receiver<String>>,
    on_event: PublisherEventSink,
) {
    stop_publisher(on_event.clone());
    let stop = Arc::new(AtomicBool::new(false));
    *STOP.lock().unwrap() = Some(stop.clone());
    on_event(PublisherEvent::Starting);
    let sink = on_event.clone();
    if std::thread::Builder::new()
        .stack_size(16 * 1024 * 1024)
        .spawn(move || run_publisher_peer(p2p, video_mid, room, sink, trickle_rx, stop))
        .is_err()
    {
        *STOP.lock().unwrap() = None;
        on_event(PublisherEvent::StartFailed(
            "被控端启动失败：无法创建线程".into(),
        ));
    }
}

/// 停止 macOS 被控端。
pub fn stop_publisher(on_event: PublisherEventSink) {
    if let Some(stop) = STOP.lock().unwrap().take() {
        stop.store(true, Ordering::SeqCst);
    }
    on_event(PublisherEvent::Stopped);
}

/// 媒体通道抽象：SFU（LiveSession）与 P2P（P2pCall）共用采集/编码/注入泵
/// （与 generic_publisher 的 PublisherTransport 同构；P2P 以首个 ChannelOpen
/// 为会话就绪——DTLS 完成 ⟺ SRTP 密钥就绪，早写媒体被对端静默丢弃）。
enum PublisherTransport {
    Sfu(LiveSession),
    Peer(aerodesk_core::p2p_call::P2pCall),
}

impl PublisherTransport {
    fn pump(&mut self) {
        use str0m::{Input, Output};
        match self {
            Self::Sfu(live) => {
                live.socket
                    .set_read_timeout(Some(Duration::from_millis(5)))
                    .ok();
                let mut buf = [0u8; 2000];
                for _ in 0..512 {
                    match live.socket.recv_from(&mut buf) {
                        Ok((n, source)) => {
                            let Ok(contents) = buf[..n].try_into() else {
                                continue;
                            };
                            let input = Input::Receive(
                                Instant::now(),
                                str0m::net::Receive {
                                    proto: str0m::net::Protocol::Udp,
                                    source,
                                    destination: live.socket.local_addr().unwrap(),
                                    contents,
                                },
                            );
                            let _ = live.endpoint.handle_input(input);
                        }
                        Err(_) => break,
                    }
                }
                let _ = live.endpoint.handle_timeout(Instant::now());
                while let Some(output) = live.endpoint.poll_output() {
                    match output {
                        Output::Transmit(t) => {
                            let _ = live.socket.send_to(&t.contents, t.destination);
                        }
                        Output::Timeout(_) => break,
                        Output::Event(_) => {}
                    }
                }
            }
            Self::Peer(p2p) => {
                let _ = p2p.poll();
            }
        }
    }

    fn poll_event(&mut self) -> Option<aerodesk_core::endpoint::ClientEvent> {
        match self {
            Self::Sfu(live) => live.endpoint.poll_event(),
            Self::Peer(p2p) => p2p.poll_event(),
        }
    }

    fn endpoint(&mut self) -> &mut aerodesk_core::Endpoint {
        match self {
            Self::Sfu(live) => &mut live.endpoint,
            Self::Peer(p2p) => p2p.endpoint(),
        }
    }

    /// #552：注入对端后到候选（P2P trickle；SFU 路径 no-op）。
    fn add_remote_candidate(&mut self, sdp_candidate: &str) {
        if let Self::Peer(p2p) = self {
            if let Err(e) = p2p.add_remote_candidate(sdp_candidate) {
                tracing::warn!("trickle 候选注入失败：{e}");
            }
        }
    }
}

fn run_publisher(
    server: String,
    room: String,
    token: String,
    on_event: PublisherEventSink,
    audio: bool,
    mouse: bool,
    view_only: bool,
    stop: Arc<AtomicBool>,
) {
    let auth = Some(token.as_str()).filter(|t| !t.is_empty());

    // #487 审查：连接链路在异常网络下可能无限阻塞且无法被 stop 中断——
    // 子线程 + 30s 总超时（正常约 1-5s），超时报错返回，不再静默卡死。
    let live = match connect_live_role_codec_timeout(
        &server,
        &room,
        Role::Publisher,
        auth,
        Some(Codec::H264),
        Duration::from_secs(30),
    ) {
        Ok(l) => l,
        Err(e) => {
            let msg = format!("被控端连接失败：{e}");
            on_event(PublisherEvent::Status(msg));
            return;
        }
    };
    let Some(video_mid) = live.video_mid else {
        on_event(PublisherEvent::Status("被控端连接失败：无视频 mid".into()));
        return;
    };
    let audio_mid = live.audio_mid;
    let connected0 = live.ice_connected;
    run_publisher_pump(
        PublisherTransport::Sfu(live),
        video_mid,
        audio_mid,
        connected0,
        room,
        on_event,
        audio,
        mouse,
        view_only,
        None,
        stop,
    );
}

/// #552：SIP 1:1 P2P 被叫发布（P2pCall 已由调用方建立并 accept）。
fn run_publisher_peer(
    p2p: aerodesk_core::p2p_call::P2pCall,
    video_mid: str0m::media::Mid,
    room: String,
    on_event: PublisherEventSink,
    trickle_rx: Option<std::sync::mpsc::Receiver<String>>,
    stop: Arc<AtomicBool>,
) {
    run_publisher_pump(
        PublisherTransport::Peer(p2p),
        video_mid,
        None,
        false,
        room,
        on_event,
        false,
        true,
        false,
        trickle_rx,
        stop,
    );
}

#[allow(clippy::too_many_arguments)]
fn run_publisher_pump(
    mut t: PublisherTransport,
    video_mid: str0m::media::Mid,
    audio_mid: Option<str0m::media::Mid>,
    connected0: bool,
    room: String,
    on_event: PublisherEventSink,
    audio: bool,
    mouse: bool,
    view_only: bool,
    trickle_rx: Option<std::sync::mpsc::Receiver<String>>,
    stop: Arc<AtomicBool>,
) {
    let stale = || stop.load(Ordering::SeqCst);

    // SCK 屏幕采集（0,0 = 按显示器原生宽高比缩放，避免拉伸致坐标错位）。
    use aerodesk_platform::macos::capture::ScreenCapture;
    let mut capture = match ScreenCapture::start(0, FPS, 0, 0) {
        Ok(c) => c,
        Err(e) => {
            let msg = format!(
                "SCK 采集初始化失败：{e}（授予「屏幕录制」权限后重试：系统设置 > 隐私与安全性）"
            );
            on_event(PublisherEvent::Status(msg));
            return;
        }
    };
    let (w, h) = (capture.width(), capture.height());
    if w == 0 || h == 0 {
        on_event(PublisherEvent::Status(
            "SCK 采集失败：无可用显示器输出".into(),
        ));
        return;
    }
    // 输入注入坐标基准跟随被控显示器。
    aerodesk_platform::macos::inject::set_active_display(Some(capture.display_id()));

    // VideoToolbox 硬编 H264（分辨率 = 采集实际尺寸，保持显示器宽高比）。
    let mut encoder = match aerodesk_platform::macos::vt_encoder::VtEncoder::new_with_codec(
        w,
        h,
        FPS,
        DEFAULT_BITRATE_BPS,
        videotoolbox::Codec::H264,
    ) {
        Ok(e) => e,
        Err(e) => {
            on_event(PublisherEvent::Status(format!("VT 编码器初始化失败：{e}")));
            return;
        }
    };

    // #315：采集期间保持显示器唤醒（防闲置休眠后 SCK 无显示器）。
    let _keep_awake = SystemWakeLock::acquire(
        &aerodesk_platform::macos::wake_lock::MacSystemWakeLock,
        true,
    )
    .map_err(|e| tracing::warn!("保持显示器唤醒失败: {e}"))
    .ok();

    // 真实系统音频（SCK audio-only → PCMU）；失败仅告警，视频照常发布。
    let mut audio_sender = if audio {
        match aerodesk_platform::macos::audio_capture::SystemAudioCapture::start() {
            Ok(cap) => Some(RealAudioSender::new(cap, false)),
            Err(e) => {
                tracing::warn!("SCK 系统音频采集失败，被控端仅发布视频: {e}");
                None
            }
        }
    } else {
        None
    };

    on_event(PublisherEvent::Status(format!(
        "正在注册被控端：设备 {room} · {w}x{h}@30"
    )));

    // #477：connect 建链阶段的 ICE 泵会消费掉首个 IceConnected 事件——必须
    // 用状态标志初始化（generic_publisher/cli 同款），否则经公网/TURN 建链后事件
    // 已被消费、connected 永远为 false，一帧不发（本地直连靠重协商二次事件掩盖）。
    let mut connected = connected0;
    let mut next_frame = Instant::now();
    let mut next_cursor = Instant::now();
    let mut pts: i64 = 0;

    // #503-2 被控端剪贴板双向：file 通道接收 viewer 发来的文本/图片 + 本地
    // 1s 轮询回传（与 CLI 被控端同语义；图片优先，防回声）。
    // #503 传输中心：desktop→desktop 发送的文件落盘 Downloads/AeroDesk
    // （与 generic_publisher 同款默认；创建失败 → None，接收禁用）。
    let recv_dir = crate::generic_publisher::resolve_recv_dir();
    let mut file_transfer = aerodesk_core::file_transfer::FileTransfer::new(recv_dir);
    // 被控端允许响应 FileControl::Request 提供文件（#255 审查语义，同 CLI）。
    file_transfer.set_allow_request(true);
    let mut clip_poller = crate::clipboard_sync::ClipboardPoller::new();

    // #487 光标列缺口（macOS 侧）：真实光标 30Hz 上报，与 Windows 端 #532
    // 同款线格式（CursorPos + sent_ms）——观看端叠加层据此绘制远端光标。
    let mut cursor_source = aerodesk_platform::macos::cursor::MacCursor;

    while !stale() {
        // #552：信令面转发的对端后到候选（INFO sdpfrag；无积压时一次空转）。
        if let Some(rx) = trickle_rx.as_ref() {
            while let Ok(cand) = rx.try_recv() {
                t.add_remote_candidate(&cand);
            }
        }
        t.pump();

        while let Some(ev) = t.endpoint().poll_event() {
            // #503-2 被控端 file 通道事件交给状态机（非 file 事件为 no-op）。
            file_transfer.handle_event(&ev, t.endpoint());
            match ev {
                ClientEvent::IceConnected => {
                    connected = true;
                    next_frame = Instant::now();
                    on_event(PublisherEvent::Status(format!(
                        "已在线，可被呼叫：设备 {room} · 屏幕发布中"
                    )));
                }
                ClientEvent::Closed => {
                    on_event(PublisherEvent::Status("被控端连接已关闭".into()));
                    return;
                }
                ClientEvent::KeyframeRequest(_) => {
                    if let Err(e) = encoder.force_keyframe() {
                        tracing::warn!("vt capture force keyframe failed: {e}");
                    }
                }
                ev => handle_input(t.endpoint(), view_only, mouse, ev),
            }
        }

        // #503-2 被控端剪贴板：应用 viewer 发来的文本/图片 + 本地轮询回传。
        if let Some(msg) = crate::clipboard_sync::tick_publisher_clipboard(
            &mut file_transfer,
            t.endpoint(),
            &mut clip_poller,
        ) {
            on_event(PublisherEvent::Status(msg));
        }

        if let Some(amid) = audio_mid
            && let Some(sender) = &mut audio_sender
        {
            sender.tick(t.endpoint(), amid, Instant::now());
        }

        // 光标 30Hz 上报（静屏无视频帧时 cursor 通道仍常活，观看端能持续绘制）。
        if connected && Instant::now() >= next_cursor {
            next_cursor += Duration::from_millis(33);
            if let Some((x, y)) = cursor_source.position_normalized() {
                let pos =
                    CursorPos::new(x.clamp(0.0, 1.0), y.clamp(0.0, 1.0)).with_sent_ms(now_ms());
                if let Ok(json) = serde_json::to_string(&pos) {
                    t.endpoint()
                        .send_channel_data("cursor", false, json.as_bytes());
                }
            }
        }

        if connected && Instant::now() >= next_frame {
            next_frame += Duration::from_nanos(1_000_000_000 / FPS as u64);
            // SCK 出帧（50ms 超时；无帧 = 屏幕无变化，正常跳过）。
            if let Some(surface) = capture.capture_frame(Duration::from_millis(50)) {
                match encoder.encode_surface(&surface) {
                    Ok(Some(frame)) => {
                        // VT 输出 annexb 直接进 RTP 载荷。
                        let annexb = encoder.to_annexb(&frame);
                        let rtp_time = MediaTime::new(pts as u64 * 3000, Frequency::NINETY_KHZ);
                        if let Err(e) = t.endpoint().send_video_frame(video_mid, annexb, rtp_time) {
                            tracing::warn!("发送视频帧失败: {e:?}");
                        }
                        pts += 1;
                    }
                    Ok(None) => {}
                    Err(e) => tracing::warn!("VT 编码失败: {e}"),
                }
            }
        }

        std::thread::sleep(Duration::from_millis(2));
    }

    on_event(PublisherEvent::Status("被控端已停止".into()));
}

/// 被控端输入通道：远端键鼠 → CGEvent 注入；剪贴板文本 → 系统剪贴板。
fn handle_input(
    endpoint: &mut aerodesk_core::Endpoint,
    view_only: bool,
    mouse: bool,
    ev: ClientEvent,
) {
    match ev {
        ClientEvent::ChannelOpen(label, _) if label == "input" => {
            tracing::info!("被控端 input channel open");
        }
        ClientEvent::ChannelData(cid, _, data) => {
            if endpoint.channel_label(cid).as_deref() == Some("cmd") {
                // #458 发消息：被控端收到 Chat 后回 CmdResponse::Chat 给观看端。
                // #503 系统电源：与 CLI 被控端同语义——内置安全命令执行 +
                // CmdResult::Power 回执（此前 SystemPower 被静默丢弃）。
                if let Ok(req) = serde_json::from_slice::<CmdRequest>(&data) {
                    let result = match req.action {
                        CmdAction::Chat { text, sender, .. } => CmdResult::Chat { sender, text },
                        CmdAction::SystemPower { action } => {
                            let out = aerodesk_core::cmd_exec::system_power(action);
                            CmdResult::Power {
                                action,
                                error: out.error,
                                code: out.code,
                            }
                        }
                        // 其余动作（Run/ReadFile/…）桌面被控端不执行：与旧行为一致。
                        _ => return,
                    };
                    let resp = CmdResponse { id: req.id, result };
                    if let Ok(json) = serde_json::to_string(&resp) {
                        let _ = endpoint.send_channel_data("cmd", false, json.as_bytes());
                    }
                }
                return;
            }
            if endpoint.channel_label(cid).as_deref() != Some("input") {
                return;
            }
            let Ok(frame) = serde_json::from_slice::<InputFrame>(&data) else {
                return;
            };
            match frame.event {
                InputEvent::ClipboardText(text) => {
                    aerodesk_core::clipboard::set_cache(text.clone());
                    if !aerodesk_core::clipboard::write(&text) {
                        tracing::warn!("写入远端剪贴板文本失败");
                    }
                }
                event => {
                    if view_only {
                        tracing::info!("view_only 模式，忽略远端输入 {:?}", event);
                        return;
                    }
                    if matches!(
                        &event,
                        InputEvent::MouseButton { .. }
                            | InputEvent::MouseMove { .. }
                            | InputEvent::Wheel { .. }
                    ) && !mouse
                    {
                        tracing::info!("鼠标控制已关闭，忽略远端鼠标输入");
                        return;
                    }
                    if let Err(e) = aerodesk_platform::macos::inject::inject(&event) {
                        tracing::warn!("CGEvent 注入失败: {e}");
                    }
                }
            }
        }
        _ => {}
    }
}
