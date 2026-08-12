//! macOS 观看端：真实 H.264 解码渲染（#29）。
//!
//! 连接 `LiveSession` → `AccessUnitAssembler` 聚合成完整访问单元 →
//! VideoToolbox 硬解 → CVPixelBuffer → RGBA → Slint `Image`。
//! 替换演示帧源；其余平台仍走演示帧（等各自解码管线接入）。

use std::sync::Arc;
use std::sync::atomic::{AtomicBool, AtomicU16, Ordering};
use std::time::{Duration, Instant};

use aerodesk_core::access_unit::AccessUnitAssembler;
use aerodesk_core::connect::connect_live_role;
use aerodesk_core::endpoint::ClientEvent;
use aerodesk_core::media_pipeline::Codec;
use aerodesk_macos::decode::{H264Decoder, HevcDecoder, to_rgba};
use aerodesk_protocol::signal::Role;
use str0m::net::Protocol;

use crate::{AppWindow, FileCmd, with_ui};

/// 默认接收目录：~/Downloads/AeroDesk（不存在则创建）。
fn default_recv_dir() -> std::path::PathBuf {
    let dir = std::env::var("HOME")
        .map(|h| {
            std::path::PathBuf::from(h)
                .join("Downloads")
                .join("AeroDesk")
        })
        .unwrap_or_else(|_| std::env::temp_dir().join("AeroDesk"));
    std::fs::create_dir_all(&dir).ok();
    dir
}

/// #74 观看端多 codec 解码器：H264/H265 走 VideoToolbox 硬解（H265 无硬解
/// 时回退 FFmpeg），VP9/AV1 走 FFmpeg 软解。统一输出 RGBA。
enum UiDecoder {
    H264(H264Decoder),
    Hevc(HevcDecoder),
    Ffmpeg(aerodesk_ffmpeg::decode::FfmpegDecoder),
}

impl UiDecoder {
    fn for_codec(codec: Codec) -> Option<Self> {
        match codec {
            Codec::H264 => Some(UiDecoder::H264(H264Decoder::new())),
            Codec::Hevc if HevcDecoder::is_hardware_supported() => {
                Some(UiDecoder::Hevc(HevcDecoder::new()))
            }
            Codec::Hevc | Codec::Vp9 | Codec::Av1 => {
                aerodesk_ffmpeg::decode::FfmpegDecoder::new(codec)
                    .ok()
                    .map(UiDecoder::Ffmpeg)
            }
            _ => None,
        }
    }

    fn matches(&self, codec: Codec) -> bool {
        matches!(
            (self, codec),
            (UiDecoder::H264(_), Codec::H264)
                | (UiDecoder::Hevc(_), Codec::Hevc)
                | (UiDecoder::Ffmpeg(_), Codec::Vp9 | Codec::Av1 | Codec::Hevc)
        )
    }

    fn decode_rgba(
        &mut self,
        codec: Codec,
        data: &[u8],
        pts: i64,
    ) -> Result<Option<(Vec<u8>, u32, u32)>, String> {
        match self {
            UiDecoder::H264(d) => d
                .decode_annexb(data, pts)
                .map(|pb| pb.and_then(|pb| to_rgba(&pb).map(|(r, w, h)| (r, w as u32, h as u32)))),
            UiDecoder::Hevc(d) => d
                .decode_annexb(data, pts)
                .map(|pb| pb.and_then(|pb| to_rgba(&pb).map(|(r, w, h)| (r, w as u32, h as u32)))),
            UiDecoder::Ffmpeg(d) => {
                let unit = aerodesk_core::media_pipeline::EncodedUnit {
                    data: data.to_vec(),
                    keyframe: false,
                    pts_ms: pts.max(0) as u64 / 1000,
                    rtp_timestamp: 0,
                };
                d.decode_unit(&unit)
                    .map(|f| f.and_then(|f| f.raw.map(|raw| (raw, f.width, f.height))))
            }
        }
    }
}

/// 核心 `Decoder` trait 实现：`UiDecoder` 已按 codec 收敛 H264/HEVC 硬解 +
/// FFmpeg 回退，直接对接 `EncodedUnit`（跨平台观看管线可泛型调用）。
impl aerodesk_core::platform::Decoder for UiDecoder {
    type Error = String;

    fn configure(&mut self, _codec: Codec, _width: u32, _height: u32) -> Result<(), Self::Error> {
        Ok(())
    }

    fn decode(
        &mut self,
        unit: &aerodesk_core::media_pipeline::EncodedUnit,
    ) -> Result<Option<aerodesk_core::platform::VideoFrame>, Self::Error> {
        let pts_us = unit.pts_ms.saturating_mul(1000) as i64;
        match self {
            UiDecoder::H264(d) => d
                .decode_annexb(&unit.data, pts_us)
                .map_err(|e| e.to_string())
                .map(|pb| {
                    pb.and_then(|pb| {
                        to_rgba(&pb).map(|(raw, w, h)| aerodesk_core::platform::VideoFrame {
                            platform: None,
                            handle: None,
                            raw: Some(raw),
                            width: w as u32,
                            height: h as u32,
                            pts_ms: unit.pts_ms,
                        })
                    })
                }),
            UiDecoder::Hevc(d) => d
                .decode_annexb(&unit.data, pts_us)
                .map_err(|e| e.to_string())
                .map(|pb| {
                    pb.and_then(|pb| {
                        to_rgba(&pb).map(|(raw, w, h)| aerodesk_core::platform::VideoFrame {
                            platform: None,
                            handle: None,
                            raw: Some(raw),
                            width: w as u32,
                            height: h as u32,
                            pts_ms: unit.pts_ms,
                        })
                    })
                }),
            UiDecoder::Ffmpeg(d) => d.decode_unit(unit).map_err(|e| e.to_string()),
        }
    }
}

/// 状态栏 codec 显示名（首包前未知 → H.264 兼容占位，收到首包后更新）。
/// 系统通知（#277 `Notifier` trait 的 macOS 消费入口；非 macOS no-op）。
fn notify_user(title: &str, body: &str) {
    #[cfg(target_os = "macos")]
    {
        use aerodesk_core::platform::Notifier;
        aerodesk_macos::notifier::MacNotifier.notify(title, body);
    }
    #[cfg(not(target_os = "macos"))]
    {
        let _ = (title, body);
    }
}

fn codec_label(codec: Option<Codec>) -> &'static str {
    match codec {
        Some(Codec::Hevc) => "H.265",
        Some(Codec::Vp9) => "VP9",
        Some(Codec::Av1) => "AV1",
        _ => "H.264",
    }
}

/// 运行 macOS 观看会话（阻塞直到断开/代际失效）。
pub fn run_viewer(
    server: String,
    room: String,
    token: Option<String>,
    ui_weak: slint::Weak<AppWindow>,
    session_idx: usize,
    control_rx: std::sync::mpsc::Receiver<String>,
    input_rx: std::sync::mpsc::Receiver<String>,
    file_cmd_rx: std::sync::mpsc::Receiver<FileCmd>,
    muted: Arc<AtomicBool>,
    volume: Arc<AtomicU16>,
    stop: Arc<AtomicBool>,
) {
    // #29 多会话：本会话 stop 置位即退出（断开只关当前活动会话）。
    let stale = || stop.load(Ordering::SeqCst);
    let auth = token.as_deref().filter(|t| !t.is_empty());
    // connect_live_role 可能阻塞（异常网络环境）；放子线程 + 20s 超时保护，
    // 避免连接中的会话占满 MAX_SESSIONS 槽位且无法取消。
    let (tx, rx) = std::sync::mpsc::channel::<Result<_, String>>();
    let srv = server.clone();
    let rm = room.clone();
    let auth2 = auth.map(|s| s.to_string());
    // 数据通道收发链（str0m/SCTP）调用栈深，放大线程栈防溢出（RULE 数据通道大块传输线程栈需放大默认2MB.md）。
    let spawn_res = std::thread::Builder::new()
        .stack_size(16 * 1024 * 1024)
        .spawn(move || {
            let r = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
                connect_live_role(&srv, &rm, Role::Viewer, auth2.as_deref())
            }));
            match r {
                Ok(res) => {
                    let _ = tx.send(res);
                }
                Err(_) => {
                    eprintln!("run_viewer: connect thread PANICKED");
                }
            }
        });
    let recv_res = rx.recv_timeout(Duration::from_secs(20));
    let mut live = match recv_res {
        Ok(Ok(l)) => l,
        Ok(Err(e)) => {
            let msg = format!("连接失败：{e}");
            let terminal = msg.clone();
            if !stale() {
                with_ui(&ui_weak, move |ui| {
                    ui.set_conn_state(3);
                    ui.set_status(msg.into());
                });
            }
            crate::session_cleanup_weak(&ui_weak, session_idx, Some(terminal));
            return;
        }
        Err(_) => {
            if !stale() {
                with_ui(&ui_weak, |ui| {
                    ui.set_conn_state(3);
                    ui.set_status("连接超时".into());
                });
            }
            crate::session_cleanup_weak(&ui_weak, session_idx, Some("连接超时".into()));
            return;
        }
    };
    if stale() {
        crate::session_cleanup_weak(&ui_weak, session_idx, None);
        return;
    }
    let peer = live.peer_id.clone();
    let ice = live.ice_connected;
    let room2 = room.clone();
    let server2 = server.clone();
    with_ui(&ui_weak, move |ui| {
        ui.set_status(format!("已连接：peer={peer} ice={ice}").into());
        ui.set_log(
            format!(
                "房间: {room2}\n服务器: {server2}\nSDP 交换: OK\nICE: {}\n\n真实解码渲染（H.264/H.265 硬解优先，VP9/AV1 FFmpeg）。",
                if ice { "connected" } else { "pending(5s 超时)" }
            )
            .into(),
        );
        crate::add_recent(ui, &room2, &server2);
        ui.set_conn_state(2);
        ui.set_in_session(true);
        ui.set_session_status("会话中 · 真实解码（H.264/H.265/VP9/AV1）".into());
    });

    // #29 多会话：登记会话标签并切到当前会话（SESSIONS 为唯一事实源）。
    crate::session_joined_weak(&ui_weak, session_idx);

    let mut assembler = AccessUnitAssembler::new();
    let mut decoder: Option<UiDecoder> = None;
    // 实际协商/正在解码的视频 codec（状态栏显示用；首包后才有值）。
    let mut current_codec: Option<Codec> = None;
    let mut frames: u64 = 0;
    let mut last_stat = Instant::now();
    // #72 文件传输 + 剪贴板（接收落盘到 ~/Downloads/AeroDesk）。
    let mut file_transfer =
        aerodesk_core::file_transfer::FileTransfer::new(Some(default_recv_dir()));
    let mut last_file_status = Instant::now();
    // #277：收到文件完成时发一次系统通知（Notifier trait）。
    let mut last_notified_file: Option<String> = None;
    // #73 音频播放 + A/V 同步：PCMU/Opus 解码 → jitter buffer → AudioSink（cpal）；
    // sink 按首个音频帧的 codec 采样率惰性创建；无输出设备时降级为仅统计。
    let mut avsync = aerodesk_core::avsync::AvSync::new();
    let mut jitter = aerodesk_core::avsync::AudioJitterBuffer::new(0.08);
    let mut audio_sink: Option<aerodesk_macos::audio::AudioSink> = None;
    /// 当前 AudioSink 采样率（codec 切换时重建，防 8k↔48k 重采样变速）。
    let mut sink_rate: u32 = 0;
    // #73 Opus 音频：libopus 解码（惰性创建）。
    let mut opus_decoder: Option<aerodesk_ffmpeg::audio::OpusDecoder> = None;
    let mut audio_played: u64 = 0;
    let mut audio_dropped: u64 = 0;
    let mut audio_buffered: usize = 0;
    let mut last_audio = Instant::now();
    let mut pending_frame: Option<(Vec<u8>, u32, u32, f64)> = None;
    // #136 关键帧请求：首包/不连续/切层时向 SFU 发 PLI（节流 1s）。
    let mut last_kf_request: Option<Instant> = None;
    let mut last_kf_rid: Option<str0m::media::Rid> = None;
    let mut seen_video = false;
    /// 连续解码失败计数（超时/解码器卡死，≥8 次触发 PLI 自救）。
    let mut decode_failures: u32 = 0;

    while !stale() {
        // #72 文件传输总开关：关闭时跳过 file 通道事件与收发推进（不落盘）。
        let ft_enabled = crate::FILE_TRANSFER_ENABLED.load(Ordering::SeqCst);
        // #73 音量滑块：每次循环同步到 AudioSink（sink 可能尚未创建）。
        if let Some(sink) = &audio_sink {
            sink.set_volume(volume.load(Ordering::SeqCst));
        }
        live.socket
            .set_read_timeout(Some(Duration::from_millis(10)))
            .ok();
        let mut buf = [0u8; 4096];
        if let Ok((n, source)) = live.socket.recv_from(&mut buf)
            && let Ok(contents) = buf[..n].try_into()
        {
            let _ = live.endpoint.handle_input(str0m::Input::Receive(
                Instant::now(),
                str0m::net::Receive {
                    proto: Protocol::Udp,
                    source,
                    destination: live.socket.local_addr().unwrap(),
                    contents,
                },
            ));
        }
        let _ = live.endpoint.handle_timeout(Instant::now());
        while let Some(output) = live.endpoint.poll_output() {
            match output {
                str0m::Output::Transmit(t) => {
                    let _ = live.socket.send_to(&t.contents, t.destination);
                }
                // 必须 break：str0m 无输出时反复返回同一个 Timeout，
                // 不 break 会 100% CPU 死循环（与 generic_media/CLI/iOS 一致）。
                str0m::Output::Timeout(_) => break,
                str0m::Output::Event(_) => {}
            }
        }
        // #75：UI 指针输入 → input 通道 → SFU → 被控端注入。
        while let Ok(req) = input_rx.try_recv() {
            let _ = live
                .endpoint
                .send_channel_data("input", false, req.as_bytes());
        }
        // #29：UI 选层请求（画质/显示器按钮）→ control 通道 → SFU。
        while let Ok(req) = control_rx.try_recv() {
            let _ = live
                .endpoint
                .send_channel_data("control", false, req.as_bytes());
        }
        // #72 文件/剪贴板命令（UI 工具栏按钮）。
        while let Ok(cmd) = file_cmd_rx.try_recv() {
            match cmd {
                FileCmd::SendFile(path) => match file_transfer.send_file(&path) {
                    Ok(()) => {
                        let msg = format!("开始发送文件：{}", path.display());
                        with_ui(&ui_weak, move |ui| ui.set_session_status(msg.into()));
                    }
                    Err(e) => {
                        let msg = format!("发送失败：{e}");
                        with_ui(&ui_weak, move |ui| ui.set_session_status(msg.into()));
                    }
                },
                FileCmd::SendClipboard(text) => {
                    aerodesk_core::clipboard::set_cache(text.clone());
                    let sent = file_transfer.send_clipboard(&text, &mut live.endpoint);
                    let msg = if sent {
                        "已发送剪贴板到被控端".to_string()
                    } else {
                        "剪贴板：file 通道未就绪".to_string()
                    };
                    with_ui(&ui_weak, move |ui| ui.set_session_status(msg.into()));
                }
                FileCmd::SendClipboardImage(png) => {
                    // #271 图片剪贴板：复用 file 分片通道（接收端不落盘、写系统剪贴板）。
                    match file_transfer.send_clipboard_image(png) {
                        Ok(()) => {
                            let msg = "已发送剪贴板图片到被控端".to_string();
                            with_ui(&ui_weak, move |ui| ui.set_session_status(msg.into()));
                        }
                        Err(e) => {
                            let msg = format!("剪贴板图片发送失败：{e}");
                            with_ui(&ui_weak, move |ui| ui.set_session_status(msg.into()));
                        }
                    }
                }
                FileCmd::Cancel => {
                    file_transfer.cancel_send(&mut live.endpoint);
                }
            }
        }
        while let Some(ev) = live.endpoint.poll_event() {
            // #72 文件通道事件交给状态机（非 file 事件为 no-op）。
            // 开关关闭时跳过：不处理 Meta/Chunk/Done，绝不落盘。
            if ft_enabled {
                file_transfer.handle_event(&ev, &mut live.endpoint);
            }
            match ev {
                ClientEvent::Media(data) => {
                    // #58/#73 音频识别：SFU 转发时 mid 是 SFU 本地 mid，用协商
                    // codec（PCMU/Opus）识别音频帧。
                    if data.params.spec().codec == str0m::format::Codec::PCMU {
                        last_audio = Instant::now();
                        if muted.load(Ordering::SeqCst) {
                            audio_dropped += 1;
                        } else {
                            let pcm = aerodesk_core::pcmu::pcmu_decode(&data.data);
                            if audio_sink.is_none() || sink_rate != 8000 {
                                audio_sink =
                                    aerodesk_macos::audio::AudioSink::new_with_rate(8000).ok();
                                sink_rate = 8000;
                            }
                            avsync.on_audio(data.time.numer(), data.time.denom());
                            jitter.push(avsync.audio_time_secs(), pcm);
                            while let Some(pcm) = jitter.pop(avsync.audio_time_secs()) {
                                if let Some(sink) = &audio_sink {
                                    sink.push_pcm(&pcm);
                                }
                                audio_played += 1;
                            }
                            audio_buffered = jitter.buffered();
                        }
                    } else if data.params.spec().codec == str0m::format::Codec::Opus {
                        // #73 Opus（48kHz）：libopus 解码 → 同一 jitter buffer/时间轴。
                        last_audio = Instant::now();
                        if muted.load(Ordering::SeqCst) {
                            audio_dropped += 1;
                        } else {
                            if opus_decoder.is_none() {
                                opus_decoder = aerodesk_ffmpeg::audio::OpusDecoder::new().ok();
                            }
                            let pcm = opus_decoder
                                .as_mut()
                                .and_then(|dec| dec.decode(&data.data).ok().flatten())
                                .unwrap_or_default();
                            if audio_sink.is_none()
                                || sink_rate != aerodesk_ffmpeg::audio::OPUS_SAMPLE_RATE
                            {
                                audio_sink = aerodesk_macos::audio::AudioSink::new_with_rate(
                                    aerodesk_ffmpeg::audio::OPUS_SAMPLE_RATE,
                                )
                                .ok();
                                sink_rate = aerodesk_ffmpeg::audio::OPUS_SAMPLE_RATE;
                            }
                            avsync.on_audio(data.time.numer(), data.time.denom());
                            jitter.push(avsync.audio_time_secs(), pcm);
                            while let Some(pcm) = jitter.pop(avsync.audio_time_secs()) {
                                if let Some(sink) = &audio_sink {
                                    sink.push_pcm(&pcm);
                                }
                                audio_played += 1;
                            }
                            audio_buffered = jitter.buffered();
                        }
                    } else {
                        // #74 按协商 codec 选解码器（硬解优先，FFmpeg 回退）。
                        // 注意：不能按 live.video_mid 过滤——SFU 重协商新增的
                        // 媒体 m-line 用 SFU 本地 mid，与初始 offer 的 mid 不同
                        // （CLI/iOS/Android 同结论）。非音频即按 codec 识别视频。
                        let codec = match data.params.spec().codec {
                            str0m::format::Codec::H264 => Some(Codec::H264),
                            str0m::format::Codec::H265 => Some(Codec::Hevc),
                            str0m::format::Codec::Vp9 => Some(Codec::Vp9),
                            str0m::format::Codec::Av1 => Some(Codec::Av1),
                            _ => None,
                        };
                        if let Some(cc) = codec {
                            current_codec = Some(cc);
                            // #136 首包 / 不连续 / 切层 → 请求关键帧（PLI，节流 1s）。
                            // SFU 收到后按当前 chosen_rid 转发给发布端强制 IDR。
                            let now = Instant::now();
                            let rid_changed = last_kf_rid != data.rid;
                            let due = last_kf_request
                                .map(|t| now.duration_since(t) >= Duration::from_secs(1))
                                .unwrap_or(true);
                            if due && (rid_changed || !data.contiguous || !seen_video) {
                                let _ = live.endpoint.request_keyframe(
                                    data.mid,
                                    data.rid,
                                    str0m::media::KeyframeRequestKind::Fir,
                                );
                                last_kf_request = Some(now);
                                last_kf_rid = data.rid;
                            }
                            seen_video = true;
                            if decoder.as_ref().map(|d| !d.matches(cc)).unwrap_or(true) {
                                decoder = UiDecoder::for_codec(cc);
                            }
                            if let Some(dec) = &mut decoder
                                && let Some(au) = assembler.push(
                                    data.data.as_ref(),
                                    data.time.as_micros(),
                                    data.is_keyframe(),
                                )
                            {
                                match dec.decode_rgba(cc, &au.data, au.pts_us as i64) {
                                    Ok(Some((rgba, w, h))) => {
                                        decode_failures = 0;
                                        avsync.on_video(data.time.numer(), data.time.denom());
                                        // #73：先缓存最新帧，按音频时钟到点再渲染。
                                        pending_frame =
                                            Some((rgba, w, h, avsync.video_time_secs()));
                                    }
                                    Ok(None) => {}
                                    Err(e) => {
                                        // 连续解码失败（超时/解码器卡死）：请求关键帧自救。
                                        decode_failures += 1;
                                        if decode_failures >= 8 {
                                            decode_failures = 0;
                                            let _ = live.endpoint.request_keyframe(
                                                data.mid,
                                                data.rid,
                                                str0m::media::KeyframeRequestKind::Fir,
                                            );
                                            eprintln!(
                                                "macos viewer: 连续 8 次解码失败，请求关键帧自救: {e}"
                                            );
                                        }
                                    }
                                }
                            }
                        }
                    }
                }
                // #75 远程光标：被控端经 cursor 通道广播位置 → UI 叠加。
                ClientEvent::ChannelData(cid, _, data)
                    if live.endpoint.channel_label(cid).as_deref() == Some("cursor") =>
                {
                    if let Ok(pos) =
                        serde_json::from_slice::<aerodesk_protocol::cursor::CursorPos>(&data)
                    {
                        // 光标按会话保存；仅活动会话同步到 UI（切换会话后恢复各自光标）。
                        let (cx, cy) = (pos.x as f32, pos.y as f32);
                        crate::with_session_ui_state(&ui_weak, session_idx, move |s| {
                            s.cursor = Some((cx, cy));
                        });
                    }
                }
                ClientEvent::Closed => {
                    with_ui(&ui_weak, |ui| {
                        ui.set_status("会话结束（连接关闭）".into());
                    });
                    crate::session_cleanup_weak(
                        &ui_weak,
                        session_idx,
                        Some("会话结束（连接关闭）".into()),
                    );
                    return;
                }
                _ => {}
            }
        }
        // #73 A/V 同步渲染：音频活跃时视频不超前 >50ms；无音频时立即渲染兜底。
        if let Some((rgba, w, h, vtime)) = pending_frame.take() {
            let audio_active = last_audio.elapsed() < Duration::from_millis(500);
            let due = !audio_active || avsync.audio_time_secs() + 0.05 >= vtime;
            if due {
                present_frame(
                    &ui_weak,
                    &rgba,
                    w as usize,
                    h as usize,
                    session_idx,
                    &mut frames,
                );
            } else {
                pending_frame = Some((rgba, w, h, vtime));
            }
        }
        // #72 文件发送推进 + 远端剪贴板落地 + 进度回显（500ms 节流）。
        // 文件传输总开关：关闭时暂停收发（tick 不推进，重新开启后继续）。
        let ft_enabled = crate::FILE_TRANSFER_ENABLED.load(Ordering::SeqCst);
        if ft_enabled {
            file_transfer.tick(&mut live.endpoint);
            if let Some(text) = file_transfer.take_incoming_clipboard() {
                aerodesk_core::clipboard::set_cache(text.clone());
                // pbcopy 会阻塞等待子进程，放后台线程避免卡媒体循环。
                std::thread::spawn(move || {
                    aerodesk_core::clipboard::write(&text);
                });
                with_ui(&ui_weak, |ui| {
                    ui.set_session_status("已收到远端剪贴板".into());
                });
            }
            // #271：接收远端图片（PNG）→ 写入系统剪贴板（NSPasteboard PNGf）。
            if let Some(png) = file_transfer.take_incoming_clipboard_image() {
                std::thread::spawn(move || {
                    if aerodesk_core::clipboard::write_image(&png) {
                        eprintln!("clipboard: applied {}B image from viewer", png.len());
                    } else {
                        eprintln!("clipboard: 图片写系统剪贴板失败（{}B）", png.len());
                    }
                });
                with_ui(&ui_weak, |ui| {
                    ui.set_session_status("已收到远端剪贴板图片".into());
                });
            }
            if last_file_status.elapsed() >= Duration::from_millis(500) {
                last_file_status = Instant::now();
                let st = file_transfer.status();
                if let Some(msg) = st.message {
                    with_ui(&ui_weak, move |ui| ui.set_session_status(msg.into()));
                    crate::with_session_ui_state(&ui_weak, session_idx, |s| {
                        s.file_progress = -1.0;
                        s.file_label.clear();
                    });
                } else if let Some((name, done, total)) = st.sending {
                    let pct = done as f64 * 100.0 / total.max(1) as f64;
                    let status = format!("发送文件：{name} {done}/{total} ({pct:.0}%)");
                    let label = format!("发送 {name} {pct:.0}%");
                    with_ui(&ui_weak, move |ui| ui.set_session_status(status.into()));
                    crate::with_session_ui_state(&ui_weak, session_idx, move |s| {
                        s.file_progress = (pct / 100.0) as f32;
                        s.file_label = label;
                    });
                } else if let Some((name, done, total)) = st.receiving {
                    let pct = done as f64 * 100.0 / total.max(1) as f64;
                    let status = format!("接收文件：{name} {done}/{total} ({pct:.0}%)");
                    let label = format!("接收 {name} {pct:.0}%");
                    if done >= total && last_notified_file.as_deref() != Some(name.as_str()) {
                        notify_user("AeroDesk", &format!("收到文件：{name}"));
                        last_notified_file = Some(name.clone());
                    }
                    with_ui(&ui_weak, move |ui| ui.set_session_status(status.into()));
                    crate::with_session_ui_state(&ui_weak, session_idx, move |s| {
                        s.file_progress = (pct / 100.0) as f32;
                        s.file_label = label;
                    });
                } else {
                    crate::with_session_ui_state(&ui_weak, session_idx, |s| {
                        s.file_progress = -1.0;
                        s.file_label.clear();
                    });
                }
            }
        }
        if last_stat.elapsed() >= Duration::from_secs(2) {
            let audio = if muted.load(Ordering::SeqCst) {
                "音频已静音".to_string()
            } else {
                format!("音频 {audio_played}帧 缓存{audio_buffered} 丢{audio_dropped}")
            };
            let stat = format!(
                "会话中 · {} {frames}帧/2s · {audio}",
                codec_label(current_codec)
            );
            with_ui(&ui_weak, move |ui| ui.set_session_status(stat.into()));
            frames = 0;
            audio_played = 0;
            audio_dropped = 0;
            last_stat = Instant::now();
        }
        std::thread::sleep(Duration::from_millis(1));
    }
    // 会话结束（断开置 stop）：先取消未完成发送，再提示并清理注册表与 UI 槽位。
    file_transfer.cancel_send(&mut live.endpoint);
    let msg = format!("已断开：{room}");
    with_ui(&ui_weak, move |ui| ui.set_status(msg.into()));
    crate::session_cleanup_weak(&ui_weak, session_idx, None);
}

/// 把一帧 RGBA 呈现到会话帧槽 + 当前显示帧（#73 抽出的渲染入口；
/// 多会话映射到稠密槽位见 crate::present_frame）。
fn present_frame(
    ui_weak: &slint::Weak<AppWindow>,
    rgba: &[u8],
    w: usize,
    h: usize,
    session_idx: usize,
    frames: &mut u64,
) {
    crate::present_frame(ui_weak, rgba, w, h, session_idx);
    *frames += 1;
}

#[cfg(test)]
mod tests {
    use super::*;
    use aerodesk_core::synthetic::SyntheticSource;
    use aerodesk_macos::decode::to_rgba;
    use aerodesk_macos::vt_encoder::VtEncoder;

    /// 按 AnnexB 起始码拆分 NAL（保留起始码，模拟 str0m 的 `Output::Media` AnnexB 输出）。
    fn split_annexb_nalus(annexb: &[u8]) -> Vec<&[u8]> {
        // 记录所有起始码（位置 + 长度；4 字节优先，避免被误判为 3 字节）。
        let mut codes: Vec<(usize, usize)> = Vec::new();
        let mut i = 0usize;
        while i + 3 <= annexb.len() {
            if i + 4 <= annexb.len()
                && annexb[i] == 0
                && annexb[i + 1] == 0
                && annexb[i + 2] == 0
                && annexb[i + 3] == 1
            {
                codes.push((i, 4));
                i += 4;
            } else if annexb[i] == 0 && annexb[i + 1] == 0 && annexb[i + 2] == 1 {
                codes.push((i, 3));
                i += 3;
            } else {
                i += 1;
            }
        }
        codes
            .iter()
            .enumerate()
            .map(|(k, &(code, _))| {
                // 返回含起始码的完整 NAL（str0m AnnexB 输出格式），供 assembler 拼接。
                let payload_end = codes.get(k + 1).map(|&(c, _)| c).unwrap_or(annexb.len());
                &annexb[code..payload_end]
            })
            .collect()
    }

    /// 桌面 UI 观看链路（无网络）：VT 编码 → to_annexb（完整 AU）→ 拆成
    /// NAL 事件 → AccessUnitAssembler 重组 → H264Decoder 解码 → RGBA。
    /// 与 `run_viewer` 的媒体路径完全一致（#29 真实解码）。
    #[test]
    fn desktop_ui_decode_chain_roundtrip() {
        let (w, h) = (320u32, 180u32);
        let mut enc = VtEncoder::new(w, h, 30, 1_000_000).expect("vt encoder");
        let mut src = SyntheticSource::new(w, h);
        let mut assembler = AccessUnitAssembler::new();
        let mut decoder = H264Decoder::new();

        let mut decoded = None;
        for i in 0..12u32 {
            let frame = enc
                .encode_bgra(&src.next_frame_bgra())
                .expect("encode")
                .expect("frame");
            let au = enc.to_annexb(&frame);
            let pts_us = i as u64 * 33_333; // ~30fps
            // 模拟 str0m 逐条 NAL 事件：同 pts 聚合为完整访问单元后整帧解码。
            for nal in split_annexb_nalus(&au) {
                if let Some(complete) = assembler.push(nal, pts_us, false)
                    && let Ok(Some(buf)) =
                        decoder.decode_annexb(&complete.data, complete.pts_us as i64)
                    && let Some((rgba, dw, dh)) = to_rgba(&buf)
                {
                    decoded = Some((rgba, dw, dh));
                }
            }
            if decoded.is_some() {
                break;
            }
        }
        let (rgba, dw, dh) = decoded.expect("应在若干帧内解码出 RGBA");
        assert_eq!((dw, dh), (w as usize, h as usize));
        assert_eq!(rgba.len(), (w * h * 4) as usize);
        assert!(
            rgba.chunks_exact(4).any(|p| p[3] == 255),
            "alpha 应全不透明"
        );
    }

    /// #277 跨平台抽象：泛型消费者只依赖 core `Decoder` trait 即可解码。
    #[test]
    fn generic_decoder_trait_drives_macos_decoder() {
        fn count_frames<D: aerodesk_core::platform::Decoder>(
            dec: &mut D,
            units: &[aerodesk_core::media_pipeline::EncodedUnit],
        ) -> usize {
            let mut n = 0;
            for u in units {
                if let Ok(Some(_)) = dec.decode(u) {
                    n += 1;
                }
            }
            n
        }

        use aerodesk_ffmpeg::encode::FfmpegEncoder;
        for codec in [Codec::H264, Codec::Hevc] {
            let mut enc = FfmpegEncoder::new(320, 180, 30, 1_000_000, codec).expect("encoder");
            enc.request_keyframe();
            let mut dec = UiDecoder::for_codec(codec).expect("decoder");
            let mut frame = vec![0u8; 320 * 180 * 4];
            let mut units = Vec::new();
            for i in 0..8u32 {
                for (j, px) in frame.iter_mut().enumerate() {
                    *px = (i * 30 + j as u32 / 100) as u8;
                }
                if let Some(u) = enc.encode_bgra(&frame).expect("encode") {
                    units.push(u);
                }
            }
            let n = count_frames(&mut dec, &units);
            assert!(n >= 1, "{codec:?} 泛型 Decoder 应解出帧，got {n}");
        }
    }

    /// #277 观看端泛型链路：`Decoder + Renderer` trait 驱动解码并渲染。
    #[test]
    fn generic_decoder_renderer_chain() {
        struct CountingRenderer {
            frames: usize,
        }
        impl aerodesk_core::platform::Renderer for CountingRenderer {
            type Error = String;
            fn render(
                &mut self,
                frame: &aerodesk_core::platform::VideoFrame,
            ) -> Result<(), Self::Error> {
                assert!(!frame.raw.as_deref().unwrap_or_default().is_empty());
                self.frames += 1;
                Ok(())
            }
        }

        fn pump<D: aerodesk_core::platform::Decoder, R: aerodesk_core::platform::Renderer>(
            dec: &mut D,
            ren: &mut R,
            units: &[aerodesk_core::media_pipeline::EncodedUnit],
        ) -> usize {
            let mut rendered = 0;
            for u in units {
                if let Ok(Some(frame)) = dec.decode(u) {
                    if ren.render(&frame).is_ok() {
                        rendered += 1;
                    }
                }
            }
            rendered
        }

        use aerodesk_ffmpeg::encode::FfmpegEncoder;
        for codec in [Codec::H264, Codec::Hevc] {
            let mut enc = FfmpegEncoder::new(320, 180, 30, 1_000_000, codec).expect("encoder");
            enc.request_keyframe();
            let mut dec = UiDecoder::for_codec(codec).expect("decoder");
            let mut ren = CountingRenderer { frames: 0 };
            let mut frame = vec![0u8; 320 * 180 * 4];
            let mut units = Vec::new();
            for i in 0..8u32 {
                for (j, px) in frame.iter_mut().enumerate() {
                    *px = (i * 30 + (j as u32 / 100)) as u8;
                }
                if let Some(u) = enc.encode_bgra(&frame).expect("encode") {
                    units.push(u);
                }
            }
            let n = pump(&mut dec, &mut ren, &units);
            assert!(n >= 1, "{codec:?} 泛型 Decoder+Renderer 应渲染，got {n}");
        }
    }

    /// 状态栏 codec 显示名与协商 codec 一致（H.265 不再误显示 H.264）。
    #[test]
    fn codec_label_matches_negotiated() {
        assert_eq!(codec_label(None), "H.264");
        assert_eq!(codec_label(Some(Codec::H264)), "H.264");
        assert_eq!(codec_label(Some(Codec::Hevc)), "H.265");
        assert_eq!(codec_label(Some(Codec::Vp9)), "VP9");
        assert_eq!(codec_label(Some(Codec::Av1)), "AV1");
    }

    /// #74 UI 解码器（硬解优先 + FFmpeg 回退）对全部 codec 回环出 RGBA。
    #[test]
    fn ui_decoder_decodes_all_codecs() {
        use aerodesk_ffmpeg::encode::FfmpegEncoder;

        let (w, h) = (320u32, 180u32);
        for codec in [Codec::H264, Codec::Hevc, Codec::Vp9, Codec::Av1] {
            let mut enc = FfmpegEncoder::new(w, h, 30, 1_000_000, codec).expect("encoder");
            enc.request_keyframe();
            let mut dec = UiDecoder::for_codec(codec).expect("decoder");
            let mut ok = false;
            for i in 0..80u32 {
                let bgra: Vec<u8> = (0..(w * h * 4) as usize)
                    .map(|j| ((i * 7 + (j as u32) / 4) & 0xff) as u8)
                    .collect();
                let Some(unit) = enc.encode_bgra(&bgra).expect("encode") else {
                    continue;
                };
                if let Ok(Some((rgba, dw, dh))) = dec.decode_rgba(codec, &unit.data, 0) {
                    assert_eq!((dw, dh), (w, h));
                    assert_eq!(rgba.len(), (w * h * 4) as usize);
                    ok = true;
                    break;
                }
            }
            assert!(ok, "{codec:?} 应解出 RGBA");
        }
    }

    #[test]
    fn split_annexb_nalus_works() {
        // 3 字节与 4 字节起始码都要识别。
        let data: Vec<u8> = [
            &[0u8, 0, 0, 1, 0x67, 0x01][..],
            &[0, 0, 0, 1, 0x65, 0x02][..],
            &[0, 0, 1, 0x41, 0x03][..],
        ]
        .concat();
        let nals = split_annexb_nalus(&data);
        assert_eq!(nals.len(), 3);
        assert_eq!(nals[0], &[0, 0, 0, 1, 0x67, 0x01]);
        assert_eq!(nals[1], &[0, 0, 0, 1, 0x65, 0x02]);
        assert_eq!(nals[2], &[0, 0, 1, 0x41, 0x03]);
    }
}
