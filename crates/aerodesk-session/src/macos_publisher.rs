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
use aerodesk_core::connect::connect_live_role_codec;
use aerodesk_core::endpoint::ClientEvent;
use aerodesk_core::media_pipeline::Codec;
use aerodesk_core::platform::SystemWakeLock;
use aerodesk_protocol::cmd::{CmdAction, CmdRequest, CmdResponse, CmdResult};
use aerodesk_protocol::input::{InputEvent, InputFrame};
use aerodesk_protocol::signal::Role;
use str0m::Output;
use str0m::media::{Frequency, MediaTime};
use str0m::net::Protocol;

use crate::PublisherEvent;
use crate::generic_publisher::PublisherEventSink;

const FPS: u32 = 30;
const DEFAULT_BITRATE_BPS: u32 = 8_000_000;

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

/// 停止 macOS 被控端。
pub fn stop_publisher(on_event: PublisherEventSink) {
    if let Some(stop) = STOP.lock().unwrap().take() {
        stop.store(true, Ordering::SeqCst);
    }
    on_event(PublisherEvent::Stopped);
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
    let stale = || stop.load(Ordering::SeqCst);
    let auth = Some(token.as_str()).filter(|t| !t.is_empty());

    let mut live =
        match connect_live_role_codec(&server, &room, Role::Publisher, auth, Some(Codec::H264)) {
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

    let mut connected = false;
    let mut next_frame = Instant::now();
    let mut pts: i64 = 0;

    while !stale() {
        // #211：排空式读取 UDP，保证 SCTP ACK 及时消费，远端输入送达率不塌陷。
        let wait = Duration::from_millis(5);
        live.socket.set_read_timeout(Some(wait)).ok();
        drain_udp_input(&mut live.socket, &mut live.endpoint, 512);
        let _ = live.endpoint.handle_timeout(Instant::now());

        while let Some(output) = live.endpoint.poll_output() {
            match output {
                Output::Transmit(t) => {
                    let _ = live.socket.send_to(&t.contents, t.destination);
                }
                // 关键：Timeout 必须 break，否则 str0m 反复返回同一 Timeout（100% CPU）。
                Output::Timeout(_) => break,
                Output::Event(_) => {}
            }
        }

        while let Some(ev) = live.endpoint.poll_event() {
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
                ev => handle_input(&mut live.endpoint, view_only, mouse, ev),
            }
        }

        if let Some(amid) = audio_mid
            && let Some(sender) = &mut audio_sender
        {
            sender.tick(&mut live.endpoint, amid, Instant::now());
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
                        if let Err(e) = live.endpoint.send_video_frame(video_mid, annexb, rtp_time)
                        {
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

/// #211：网络泵排空式读取，最多 `max_packets` 包。
fn drain_udp_input(
    socket: &mut aerodesk_core::media_socket::MediaSocket,
    endpoint: &mut aerodesk_core::Endpoint,
    max_packets: usize,
) {
    let mut buf = [0u8; 2000];
    for _ in 0..max_packets {
        match socket.recv_from(&mut buf) {
            Ok((n, source)) => {
                let Ok(contents) = buf[..n].try_into() else {
                    continue;
                };
                let input = str0m::Input::Receive(
                    Instant::now(),
                    str0m::net::Receive {
                        proto: Protocol::Udp,
                        source,
                        destination: socket.local_addr().unwrap(),
                        contents,
                    },
                );
                let _ = endpoint.handle_input(input);
            }
            Err(_) => break,
        }
    }
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
                if let Ok(req) = serde_json::from_slice::<CmdRequest>(&data)
                    && let CmdAction::Chat { text, sender, .. } = req.action
                {
                    let resp = CmdResponse {
                        id: req.id,
                        result: CmdResult::Chat { sender, text },
                    };
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
