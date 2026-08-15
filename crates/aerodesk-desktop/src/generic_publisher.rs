//! 非 macOS 桌面端被控端（屏幕发布）：Windows DXGI 采集 + OpenH264 软编
//! + SendInput 输入注入。泛型循环只依赖 core `MediaSource`/`Encoder` 与
//! `Endpoint`，平台差异收敛在适配器构造（本文件 `imp` 内的 Windows 工厂）。
//!
//! macOS 被控端路径仍在 `macos_media`；Linux 被控端后续接入，本模块当前为
//! no-op 桩（`RULE_可达性`：调用点与定义点同 cfg 门控，未接入平台不编译平台引用）。

/// 校验被控端发布房间号：本机 ID 即房间号；空/未初始化返回 None。
fn valid_publisher_room(device_id: &str) -> Option<String> {
    let room = device_id.trim();
    if room.is_empty() || room == "—" {
        None
    } else {
        Some(room.to_string())
    }
}

/// UI「开启被控」开关统一入口：开启时启动发布线程，关闭时置 stop 退出线程。
pub fn toggle_publisher(ui: &crate::AppWindow) {
    if ui.get_inc_enabled() {
        start_publisher(ui);
    } else {
        stop_publisher(ui);
    }
}

/// 启动被控端（外部仅 windows 目标调用；Linux/其他非 macOS 为 no-op 提示）。
#[cfg(windows)]
pub fn start_publisher(ui: &crate::AppWindow) {
    imp::start_publisher(ui);
}

/// 停止被控端（windows 实现）。
#[cfg(windows)]
pub fn stop_publisher(ui: &crate::AppWindow) {
    imp::stop_publisher(ui);
}

/// 非 Windows 的非 macOS 平台当前未接入被控端发布：提示但不破坏 UI。
#[cfg(not(windows))]
pub fn start_publisher(ui: &crate::AppWindow) {
    ui.set_settings_status("被控端发布当前仅 Windows 实现".into());
}

/// 非 Windows 的非 macOS 平台停止为 no-op（保持调用点对称，见 RULE 可达性）。
#[cfg(not(windows))]
pub fn stop_publisher(ui: &crate::AppWindow) {
    ui.set_settings_status("被控端发布当前仅 Windows 实现".into());
}

/// Windows 被控端实现。独立 cfg 模块避免在 Linux/macOS 引用 `aerodesk-windows`。
#[cfg(windows)]
mod imp {
    use super::valid_publisher_room;
    use std::sync::atomic::{AtomicBool, Ordering};
    use std::sync::{Arc, Mutex};
    use std::time::{Duration, Instant};

    use aerodesk_core::Endpoint;
    use aerodesk_core::connect::connect_live_role_codec;
    use aerodesk_core::endpoint::ClientEvent;
    use aerodesk_core::media_pipeline::Codec;
    use aerodesk_core::media_socket::MediaSocket;
    use aerodesk_core::platform::{Encoder, InputInjector, MediaSource, SystemWakeLock};
    use aerodesk_protocol::input::{InputEvent, InputFrame};
    use aerodesk_protocol::signal::Role;
    use slint::ComponentHandle;
    use str0m::Output;
    use str0m::media::{Frequency, MediaTime, Mid};
    use str0m::net::Protocol;

    const FPS: u32 = 30;
    const DEFAULT_BITRATE_KBPS: u32 = 8_000;
    const DEFAULT_TARGET_W: u32 = 1920;
    const DEFAULT_TARGET_H: u32 = 1080;

    /// 当前发布线程的 stop 句柄（同一时刻只允许一个被控端发布线程）。
    pub(super) static STOP: Mutex<Option<Arc<AtomicBool>>> = Mutex::new(None);

    pub(super) fn start_publisher(ui: &crate::AppWindow) {
        // 同一时刻仅一个发布线程：先停止旧线程再启动新配置。
        stop_publisher(ui);

        let Some(room) = valid_publisher_room(&ui.get_device_id().to_string()) else {
            ui.set_settings_status("被控端启动失败：本机 ID 无效".into());
            return;
        };
        let server = ui.get_server_default().to_string();
        let token = ui.get_token_default().to_string();
        let audio = ui.get_inc_audio();
        let mouse = ui.get_inc_mouse();
        let view_only = ui.get_inc_view_only();

        let stop = Arc::new(AtomicBool::new(false));
        *STOP.lock().unwrap() = Some(stop.clone());
        let weak = ui.as_weak();
        ui.set_settings_status("正在启动被控端…".into());
        // 数据通道收发链（str0m/SCTP）调用栈深，放大线程栈防溢出（RULE 数据通道大块传输线程栈需放大默认2MB.md）。
        if std::thread::Builder::new()
            .stack_size(16 * 1024 * 1024)
            .spawn(move || run_publisher(server, room, token, weak, audio, mouse, view_only, stop))
            .is_err()
        {
            *STOP.lock().unwrap() = None;
            ui.set_settings_status("被控端启动失败：无法创建线程".into());
        }
    }

    pub(super) fn stop_publisher(ui: &crate::AppWindow) {
        if let Some(stop) = STOP.lock().unwrap().take() {
            stop.store(true, Ordering::SeqCst);
        }
        ui.set_settings_status("被控端已停止".into());
    }

    fn set_publisher_status(ui_weak: &slint::Weak<crate::AppWindow>, msg: String) {
        let ui_weak = ui_weak.clone();
        crate::with_ui(&ui_weak, move |ui| {
            ui.set_status(msg.clone().into());
            ui.set_settings_status(msg.into());
        });
    }

    fn run_publisher(
        server: String,
        room: String,
        token: String,
        ui_weak: slint::Weak<crate::AppWindow>,
        audio: bool,
        mouse: bool,
        view_only: bool,
        stop: Arc<AtomicBool>,
    ) {
        let stale = || stop.load(Ordering::SeqCst);
        let auth = Some(token.as_str()).filter(|t| !t.is_empty());

        // 发布端连接：H.264 视频 + 音频 m-line（core 泛型连接，CLI 同款）。
        let mut live =
            match connect_live_role_codec(&server, &room, Role::Publisher, auth, Some(Codec::H264))
            {
                Ok(l) => l,
                Err(e) => {
                    let msg = format!("被控端连接失败：{e}");
                    set_publisher_status(&ui_weak, msg);
                    return;
                }
            };
        let Some(video_mid) = live.video_mid else {
            set_publisher_status(&ui_weak, "被控端连接失败：无视频 mid".into());
            return;
        };
        let audio_mid = live.audio_mid;

        // DXGI Desktop Duplication 采集（4K 软编性能不足，默认缩放到 1080p）。
        use aerodesk_platform::windows::capture::DxgiCapturer;
        let mut capture = match DxgiCapturer::new_with_scale(DEFAULT_TARGET_W, DEFAULT_TARGET_H) {
            Ok(c) => c,
            Err(e) => {
                let msg = format!("DXGI 采集初始化失败：{e}");
                set_publisher_status(&ui_weak, msg);
                return;
            }
        };
        // 多显示器：输入注入坐标按被控显示器在虚拟屏幕中的区域映射（#75）。
        let display_rect = capture.display_rect();
        let (w, h) = capture.size();
        if w == 0 || h == 0 {
            set_publisher_status(&ui_weak, "DXGI 采集失败：无可用显示器输出".into());
            return;
        }
        if let Err(e) = MediaSource::start(&mut capture, FPS, false) {
            set_publisher_status(&ui_weak, format!("屏幕采集启动失败：{e}"));
            return;
        }

        let mut injector = aerodesk_platform::windows::inject::SendInputInjector::new();
        injector.set_active_display(Some(display_rect));

        // OpenH264 软编：Windows 无系统 x264 时的全平台回退，输入统一 BGRA。
        let mut encoder = match aerodesk_platform::windows::encode::SoftEncoder::new(
            w,
            h,
            FPS,
            DEFAULT_BITRATE_KBPS,
        ) {
            Ok(e) => e,
            Err(e) => {
                set_publisher_status(&ui_weak, format!("OpenH264 编码器初始化失败：{e}"));
                return;
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
        let mut audio_sender = if audio {
            match aerodesk_platform::windows::audio_capture::WasapiLoopbackCapture::start() {
                Ok(cap) => Some(PcmuAudioSender::new(cap)),
                Err(e) => {
                    tracing::warn!("WASAPI 回环采集失败，被控端仅发布视频: {e}");
                    None
                }
            }
        } else {
            None
        };

        set_publisher_status(
            &ui_weak,
            format!("正在注册被控端：设备 {room} · {w}x{h}@30"),
        );

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
                        set_publisher_status(
                            &ui_weak,
                            format!("已在线，可被呼叫：设备 {room} · 屏幕发布中"),
                        );
                    }
                    ClientEvent::Closed => {
                        set_publisher_status(&ui_weak, "被控端连接已关闭".into());
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
                    Err(e) => tracing::warn!("OpenH264 编码失败: {e}"),
                }
            }

            std::thread::sleep(Duration::from_millis(2));
        }

        set_publisher_status(&ui_weak, "被控端已停止".into());
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
                        if let Err(e) = injector.inject(&event) {
                            tracing::warn!("SendInput 注入失败: {e}");
                        }
                    }
                }
            }
            _ => {}
        }
    }

    /// WASAPI 48kHz f32 样本 → PCMU 8kHz 20ms 帧（6:1 降采样），一次补一帧。
    struct PcmuAudioSender {
        cap: aerodesk_platform::windows::audio_capture::WasapiLoopbackCapture,
        buf48: Vec<i16>,
        buf8: Vec<i16>,
        pts: u64,
        next_send: Instant,
    }

    impl PcmuAudioSender {
        fn new(cap: aerodesk_platform::windows::audio_capture::WasapiLoopbackCapture) -> Self {
            Self {
                cap,
                buf48: Vec::new(),
                buf8: Vec::new(),
                pts: 0,
                next_send: Instant::now(),
            }
        }

        fn tick(&mut self, endpoint: &mut Endpoint, mid: Mid, now: Instant) {
            if now < self.next_send {
                return;
            }
            let samples = self.cap.next_samples(48_000 * 5);
            self.buf48.extend(
                samples
                    .into_iter()
                    .map(|v| (v.clamp(-1.0, 1.0) * 32767.0) as i16),
            );

            let mut i = 0;
            while i + 6 <= self.buf48.len() {
                let sum: i32 = self.buf48[i..i + 6].iter().map(|&x| x as i32).sum();
                self.buf8.push((sum / 6) as i16);
                i += 6;
            }
            self.buf48.drain(..i);

            if self.buf8.len() >= 160 {
                let frame: Vec<i16> = self.buf8.drain(..160).collect();
                let data = aerodesk_core::pcmu::pcmu_encode(&frame);
                let rtp_time = MediaTime::new(self.pts * 160, Frequency::EIGHT_KHZ);
                if let Err(e) = endpoint.send_audio_frame(mid, data, rtp_time) {
                    tracing::warn!("发送 PCMU 音频失败: {e:?}");
                }
                self.pts += 1;
                self.next_send = now + Duration::from_millis(20);
            } else {
                self.next_send = now + Duration::from_millis(5);
            }
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
