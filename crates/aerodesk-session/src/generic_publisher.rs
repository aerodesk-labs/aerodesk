//! 非 macOS 桌面端被控端（屏幕发布）：Windows DXGI 采集 + FFmpeg 编码
//! （h264_mf 硬编优先、一帧探测回退 libx264；无 FFmpeg 环境兜底 OpenH264）
//! + SendInput 输入注入。泛型循环只依赖 core `MediaSource`/`Encoder` 与
//! `Endpoint`，平台差异收敛在适配器构造（本文件 `imp` 内的 Windows 工厂）。
//!
//! macOS 被控端路径仍在 `macos_media`；Linux 被控端后续接入，本模块当前为
//! no-op 桩（`RULE_可达性`：调用点与定义点同 cfg 门控，未接入平台不编译平台引用）。
//!
//! #508 B1：启动参数收敛为 [`crate::PublisherConfig`] 快照，UI 副作用经
//! [`crate::PublisherEvent`] 回调回传，本模块不再引用 Slint/UI 类型。

use std::sync::Arc;

use crate::{PublisherConfig, PublisherEvent};

/// 被控端生命周期事件回调（实现方负责线程安全/排队到 UI 线程）。
pub type PublisherEventSink = Arc<dyn Fn(PublisherEvent) + Send + Sync>;

/// 校验被控端发布房间号：本机 ID 即房间号；空/未初始化返回 None。
pub(crate) fn valid_publisher_room(device_id: &str) -> Option<String> {
    let room = device_id.trim();
    if room.is_empty() || room == "—" {
        None
    } else {
        Some(room.to_string())
    }
}

/// 启动被控端（外部仅 windows 目标调用；Linux/其他非 macOS 为 no-op 提示）。
#[cfg(windows)]
pub fn start_publisher(cfg: PublisherConfig, on_event: PublisherEventSink) {
    imp::start_publisher(cfg, on_event);
}

/// 停止被控端（windows 实现）。
#[cfg(windows)]
pub fn stop_publisher(on_event: PublisherEventSink) {
    imp::stop_publisher(on_event);
}

/// macOS 被控端：SCK+VT+CGEvent 实现（#487 互控最高优先级）。
#[cfg(target_os = "macos")]
pub fn start_publisher(cfg: PublisherConfig, on_event: PublisherEventSink) {
    crate::macos_publisher::start_publisher(cfg, on_event);
}

/// macOS 被控端停止。
#[cfg(target_os = "macos")]
pub fn stop_publisher(on_event: PublisherEventSink) {
    crate::macos_publisher::stop_publisher(on_event);
}

/// 其余平台（Linux 等）当前未接入被控端发布：提示但不破坏 UI。
#[cfg(not(any(windows, target_os = "macos")))]
pub fn start_publisher(_cfg: PublisherConfig, on_event: PublisherEventSink) {
    on_event(PublisherEvent::StartFailed(
        "被控端发布当前仅 Windows/macOS 实现".into(),
    ));
}

/// 其余平台停止为 no-op（保持调用点对称，见 RULE 可达性）。
#[cfg(not(any(windows, target_os = "macos")))]
pub fn stop_publisher(on_event: PublisherEventSink) {
    on_event(PublisherEvent::StartFailed(
        "被控端发布当前仅 Windows/macOS 实现".into(),
    ));
}

/// Windows 被控端实现。独立 cfg 模块避免在 Linux/macOS 引用 `aerodesk-platform`。
#[cfg(windows)]
mod imp {
    use super::{PublisherEventSink, valid_publisher_room};
    use crate::{PublisherConfig, PublisherEvent};
    use std::sync::atomic::{AtomicBool, Ordering};
    use std::sync::{Arc, Mutex};
    use std::time::{Duration, Instant};

    use aerodesk_codec::audio::RealAudioSender;
    use aerodesk_core::Endpoint;
    use aerodesk_core::connect::connect_live_role_codec_timeout;
    use aerodesk_core::endpoint::ClientEvent;
    use aerodesk_core::media_socket::MediaSocket;
    use aerodesk_core::platform::Codec;
    use aerodesk_core::platform::{
        CursorSource, Encoder, InputInjector, MediaSource, SystemWakeLock,
    };
    use aerodesk_core::protocol::cmd::{CmdAction, CmdRequest, CmdResponse, CmdResult};
    use aerodesk_core::protocol::input::{InputEvent, InputFrame};
    use aerodesk_core::protocol::signal::Role;
    use str0m::Output;
    use str0m::media::{Frequency, MediaTime};
    use str0m::net::Protocol;

    const FPS: u32 = 30;

    /// 发送侧墙钟（毫秒）：光标/延迟测量的 sent_ms 字段，与 cli 同口径。
    fn now_ms() -> u64 {
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|d| d.as_millis() as u64)
            .unwrap_or(0)
    }
    const DEFAULT_BITRATE_KBPS: u32 = 8_000;
    const DEFAULT_TARGET_W: u32 = 1920;
    const DEFAULT_TARGET_H: u32 = 1080;

    /// 当前发布线程的 stop 句柄（同一时刻只允许一个被控端发布线程）。
    pub(super) static STOP: Mutex<Option<Arc<AtomicBool>>> = Mutex::new(None);

    pub(super) fn start_publisher(cfg: PublisherConfig, on_event: PublisherEventSink) {
        // 同一时刻仅一个发布线程：先停止旧线程再启动新配置。
        stop_publisher(on_event.clone());

        let PublisherConfig {
            server,
            room: raw_room,
            token,
            audio,
            mouse,
            view_only,
        } = cfg;
        let Some(room) = valid_publisher_room(&raw_room) else {
            on_event(PublisherEvent::StartFailed(
                "被控端启动失败：本机 ID 无效".into(),
            ));
            return;
        };

        let stop = Arc::new(AtomicBool::new(false));
        *STOP.lock().unwrap() = Some(stop.clone());
        on_event(PublisherEvent::Starting);
        // 数据通道收发链（str0m/SCTP）调用栈深，放大线程栈防溢出（RULE 数据通道大块传输线程栈需放大默认2MB.md）。
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

    pub(super) fn stop_publisher(on_event: PublisherEventSink) {
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

        // 发布端连接：H.264 视频 + 音频 m-line（core 泛型连接，CLI 同款）。
        // #487 审查：连接链路（TCP 握手/Join/SDP 交换）在异常网络下可能无限
        // 阻塞且无法被 stop 中断——子线程 + 30s 总超时（正常约 1-5s），超时
        // 报错返回，UI 不再「已授权但无媒体」式静默卡死。
        let mut live = match connect_live_role_codec_timeout(
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

        // 屏幕采集链（#514）：WGC 主 → DXGI 备，首帧 GDI 引导内置（#477）。
        // （4K 软编性能不足，默认缩放到 1080p。）
        use aerodesk_platform::windows::capture::ScreenCapturer;
        let mut capture = match ScreenCapturer::new_with_scale(DEFAULT_TARGET_W, DEFAULT_TARGET_H) {
            Ok(c) => c,
            Err(e) => {
                let msg = format!("屏幕采集初始化失败：{e}");
                on_event(PublisherEvent::Status(msg));
                return;
            }
        };
        // 多显示器：输入注入坐标按被控显示器在虚拟屏幕中的区域映射（#75）。
        let display_rect = capture.display_rect();
        let (w, h) = capture.size();
        if w == 0 || h == 0 {
            on_event(PublisherEvent::Status(
                "屏幕采集失败：无可用显示器输出".into(),
            ));
            return;
        }
        if let Err(e) = MediaSource::start(&mut capture, FPS, false) {
            on_event(PublisherEvent::Status(format!("屏幕采集启动失败：{e}")));
            return;
        }

        let mut injector = aerodesk_platform::windows::inject::SendInputInjector::new();
        injector.set_active_display(Some(display_rect));

        // #487 光标列缺口：真实被控端 30Hz 上报本机光标（观看端 overlay 已就绪）。
        // 归一化基准与被捕显示器一致（单显示器时 display_rect 即虚拟屏幕）。
        use aerodesk_platform::windows::cursor::WindowsCursor;
        let mut cursor_source = WindowsCursor::new(Some(display_rect));

        // #506：与 cli screen 路径同款 FFmpeg 编码（h264_mf 硬编优先、一帧探测
        // 回退 libx264）——1440p/4K 源头不再受 OpenH264 软编瓶颈；FFmpeg 不可用
        // （DLL 缺失等）兜底 OpenH264 软编。输入统一 BGRA。
        let mut encoder: Box<dyn Encoder<Error = String>> =
            match aerodesk_codec::encode::FfmpegEncoder::new(
                w,
                h,
                FPS,
                u64::from(DEFAULT_BITRATE_KBPS) * 1_000,
                Codec::H264,
            ) {
                Ok(e) => Box::new(e),
                Err(ff_err) => {
                    tracing::warn!("FFmpeg 编码器不可用({ff_err})，回退 OpenH264 软编");
                    match aerodesk_platform::windows::encode::SoftEncoder::new(
                        w,
                        h,
                        FPS,
                        DEFAULT_BITRATE_KBPS,
                    ) {
                        Ok(e) => Box::new(e),
                        Err(e) => {
                            on_event(PublisherEvent::Status(format!("编码器初始化失败：{e}")));
                            return;
                        }
                    }
                }
            };

        // #334：采集期间保持显示器唤醒（防闲置休眠后 DXGI 无输出）。
        let _keep_awake = SystemWakeLock::acquire(
            &aerodesk_platform::windows::wake_lock::WindowsSystemWakeLock,
            true,
        )
        .map_err(|e| tracing::warn!("保持显示器唤醒失败: {e}"))
        .ok();

        // 真实系统音频（WASAPI loopback → PCMU）；失败仅告警，视频照常发布。
        // #487：PcmuAudioSender 与 aerodesk-codec::audio::RealAudioSender 的
        // PCMU 路径完全重复，统一用后者（20ms 节拍逻辑单份维护）。
        let mut audio_sender = if audio {
            match aerodesk_platform::windows::audio_capture::WasapiLoopbackCapture::start() {
                Ok(cap) => Some(RealAudioSender::new(cap, false)),
                Err(e) => {
                    tracing::warn!("WASAPI 回环采集失败，被控端仅发布视频: {e}");
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
        // 用状态标志初始化（cli 同款），否则经公网/TURN 建链后事件已被消费、
        // connected 永远为 false，一帧不发（本地直连靠重协商二次事件掩盖）。
        let mut connected = live.ice_connected;
        let mut next_frame = Instant::now();
        let mut next_cursor = Instant::now();
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
                    // 关键：Timeout 必须 break，否则 str0m 会反复返回同一 Timeout（100% CPU）。
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
                    ClientEvent::KeyframeRequest(_) => encoder.request_keyframe(),
                    ev => handle_input(&mut live.endpoint, &mut injector, view_only, mouse, ev),
                }
            }

            if let Some(amid) = audio_mid
                && let Some(sender) = &mut audio_sender
            {
                sender.tick(&mut live.endpoint, amid, Instant::now());
            }

            // #487 光标列缺口：真实光标 30Hz 上报（静屏无视频帧时通道仍常活）。
            // 与 cli 同款线格式（CursorPos JSON + sent_ms），观看端 overlay 零改动。
            if connected && Instant::now() >= next_cursor {
                next_cursor += Duration::from_millis(33);
                if let Some((x, y)) = cursor_source.position_normalized() {
                    let pos = aerodesk_core::protocol::cursor::CursorPos::new(
                        x.clamp(0.0, 1.0),
                        y.clamp(0.0, 1.0),
                    )
                    .with_sent_ms(now_ms());
                    if let Ok(json) = serde_json::to_string(&pos) {
                        live.endpoint
                            .send_channel_data("cursor", false, json.as_bytes());
                    }
                }
            }

            if connected && Instant::now() >= next_frame {
                next_frame += Duration::from_nanos(1_000_000_000 / FPS as u64);
                let frame = match capture.next_frame() {
                    Ok(Some(f)) => f,
                    Ok(None) => continue,
                    Err(e) => {
                        tracing::warn!("屏幕采集 next_frame: {e}");
                        continue;
                    }
                };
                match encoder.encode(&frame) {
                    Ok(Some(unit)) => {
                        let rtp_time = MediaTime::new(pts as u64 * 3000, Frequency::NINETY_KHZ);
                        if let Err(e) = live
                            .endpoint
                            .send_video_frame(video_mid, unit.data, rtp_time)
                        {
                            tracing::warn!("发送视频帧失败: {e:?}");
                        }
                        pts += 1;
                    }
                    Ok(None) => {}
                    Err(e) => tracing::warn!("视频编码失败: {e}"),
                }
            }

            std::thread::sleep(Duration::from_millis(2));
        }

        on_event(PublisherEvent::Status("被控端已停止".into()));
    }

    /// #211：网络泵排空式读取，最多 `max_packets` 包。
    fn drain_udp_input(socket: &mut MediaSocket, endpoint: &mut Endpoint, max_packets: usize) {
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

    /// 被控端输入通道：远端键鼠 → SendInput 注入；剪贴板文本 → 系统剪贴板。
    fn handle_input(
        endpoint: &mut Endpoint,
        injector: &mut aerodesk_platform::windows::inject::SendInputInjector,
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
                // 输入链路诊断（与 cea288f 观看端 send_input_to_slot 对称）：
                // debug 级逐事件，经 RUST_LOG=aerodesk_session=debug 打开。
                tracing::debug!("被控端 input 事件: {:?}", frame.event);
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
                        if let Err(e) = injector.inject(&event) {
                            tracing::warn!("SendInput 注入失败: {e}");
                        }
                    }
                }
            }
            _ => {}
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn valid_publisher_room_rejects_empty_placeholder_and_accepts_device_id() {
        assert_eq!(valid_publisher_room(""), None);
        assert_eq!(valid_publisher_room("   "), None);
        assert_eq!(valid_publisher_room("—"), None);
        assert_eq!(
            valid_publisher_room("  AD-123456  "),
            Some("AD-123456".into())
        );
    }
}
