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
use slint::{Image, Model, Rgba8Pixel, SharedPixelBuffer};
use str0m::net::Protocol;

use crate::{AppWindow, FileCmd};

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
    let mut live = match connect_live_role(&server, &room, Role::Viewer, auth) {
        Ok(l) => l,
        Err(e) => {
            if !stale() {
                if let Some(ui) = ui_weak.upgrade() {
                    ui.set_conn_state(3);
                    ui.set_status(format!("连接失败：{e}").into());
                }
            }
            if let Some(ui) = ui_weak.upgrade() {
                crate::session_cleanup(&ui, session_idx);
            }
            return;
        }
    };
    if stale() {
        if let Some(ui) = ui_weak.upgrade() {
            crate::session_cleanup(&ui, session_idx);
        }
        return;
    }
    let Some(ui) = ui_weak.upgrade() else { return };
    ui.set_status(format!("已连接：peer={} ice={}", live.peer_id, live.ice_connected).into());
    ui.set_log(
        format!(
            "房间: {room}\n服务器: {server}\nSDP 交换: OK\nICE: {}\n\n真实解码渲染（H.264/H.265 硬解优先，VP9/AV1 FFmpeg）。",
            if live.ice_connected { "connected" } else { "pending(5s 超时)" }
        )
        .into(),
    );
    crate::add_recent(&ui, &room, &server);
    ui.set_conn_state(2);
    ui.set_in_session(true);
    ui.set_session_status("会话中 · 真实解码（H.264/H.265/VP9/AV1）".into());

    // #29 多会话：登记会话标签并切到当前会话（SESSIONS 为唯一事实源）。
    crate::session_joined(&ui, session_idx);

    let mut assembler = AccessUnitAssembler::new();
    let mut decoder: Option<UiDecoder> = None;
    let mut frames: u64 = 0;
    let mut last_stat = Instant::now();
    // #72 文件传输 + 剪贴板（接收落盘到 ~/Downloads/AeroDesk）。
    let mut file_transfer =
        aerodesk_core::file_transfer::FileTransfer::new(Some(default_recv_dir()));
    let mut last_file_status = Instant::now();
    // #73 音频播放 + A/V 同步：PCMU/Opus 解码 → jitter buffer → AudioSink（cpal）；
    // sink 按首个音频帧的 codec 采样率惰性创建；无输出设备时降级为仅统计。
    let mut avsync = aerodesk_core::avsync::AvSync::new();
    let mut jitter = aerodesk_core::avsync::AudioJitterBuffer::new(0.08);
    let mut audio_sink: Option<aerodesk_macos::audio::AudioSink> = None;
    // #73 Opus 音频：libopus 解码（惰性创建）。
    let mut opus_decoder: Option<aerodesk_ffmpeg::audio::OpusDecoder> = None;
    let mut audio_played: u64 = 0;
    let mut audio_dropped: u64 = 0;
    let mut audio_buffered: usize = 0;
    let mut last_audio = Instant::now();
    let mut pending_frame: Option<(Vec<u8>, u32, u32, f64)> = None;

    while !stale() {
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
            if let str0m::Output::Transmit(t) = output {
                let _ = live.socket.send_to(&t.contents, t.destination);
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
                        if let Some(fui) = ui_weak.upgrade() {
                            fui.set_session_status(
                                format!("开始发送文件：{}", path.display()).into(),
                            );
                        }
                    }
                    Err(e) => {
                        if let Some(fui) = ui_weak.upgrade() {
                            fui.set_session_status(format!("发送失败：{e}").into());
                        }
                    }
                },
                FileCmd::SendClipboard(text) => {
                    aerodesk_core::clipboard::set_cache(text.clone());
                    let sent = file_transfer.send_clipboard(&text, &mut live.endpoint);
                    if let Some(fui) = ui_weak.upgrade() {
                        fui.set_session_status(if sent {
                            "已发送剪贴板到被控端".into()
                        } else {
                            "剪贴板：file 通道未就绪".into()
                        });
                    }
                }
            }
        }
        while let Some(ev) = live.endpoint.poll_event() {
            // #72 文件通道事件交给状态机（非 file 事件为 no-op）。
            file_transfer.handle_event(&ev, &mut live.endpoint);
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
                            if audio_sink.is_none() {
                                audio_sink =
                                    aerodesk_macos::audio::AudioSink::new_with_rate(8000).ok();
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
                            if audio_sink.is_none() {
                                audio_sink = aerodesk_macos::audio::AudioSink::new_with_rate(
                                    aerodesk_ffmpeg::audio::OPUS_SAMPLE_RATE,
                                )
                                .ok();
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
                    } else if let Some(mid) = live.video_mid
                        && data.mid == mid
                    {
                        // #74 按协商 codec 选解码器（硬解优先，FFmpeg 回退）。
                        let codec = match data.params.spec().codec {
                            str0m::format::Codec::H264 => Some(Codec::H264),
                            str0m::format::Codec::H265 => Some(Codec::Hevc),
                            str0m::format::Codec::Vp9 => Some(Codec::Vp9),
                            str0m::format::Codec::Av1 => Some(Codec::Av1),
                            _ => None,
                        };
                        if let Some(cc) = codec {
                            if decoder.as_ref().map(|d| !d.matches(cc)).unwrap_or(true) {
                                decoder = UiDecoder::for_codec(cc);
                            }
                            if let Some(dec) = &mut decoder
                                && let Some(au) = assembler.push(
                                    data.data.as_ref(),
                                    data.time.as_micros(),
                                    data.is_keyframe(),
                                )
                                && let Ok(Some((rgba, w, h))) =
                                    dec.decode_rgba(cc, &au.data, au.pts_us as i64)
                            {
                                avsync.on_video(data.time.numer(), data.time.denom());
                                // #73：先缓存最新帧，按音频时钟到点再渲染。
                                pending_frame = Some((rgba, w, h, avsync.video_time_secs()));
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
                        && let Some(fui) = ui_weak.upgrade()
                    {
                        fui.set_remote_cursor_x(pos.x as f32);
                        fui.set_remote_cursor_y(pos.y as f32);
                        fui.set_remote_cursor_visible(true);
                    }
                }
                ClientEvent::Closed => {
                    if let Some(fui) = ui_weak.upgrade() {
                        fui.set_status("会话结束（连接关闭）".into());
                        crate::session_cleanup(&fui, session_idx);
                    }
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
        file_transfer.tick(&mut live.endpoint);
        if let Some(text) = file_transfer.take_incoming_clipboard() {
            aerodesk_core::clipboard::set_cache(text.clone());
            aerodesk_core::clipboard::write(&text);
            if let Some(fui) = ui_weak.upgrade() {
                fui.set_session_status("已收到远端剪贴板".into());
            }
        }
        if last_file_status.elapsed() >= Duration::from_millis(500) {
            last_file_status = Instant::now();
            let st = file_transfer.status();
            if let Some(msg) = st.message {
                if let Some(fui) = ui_weak.upgrade() {
                    fui.set_session_status(msg.into());
                    fui.set_file_progress(-1.0);
                    fui.set_file_label("".into());
                }
            } else if let Some((name, done, total)) = st.sending {
                if let Some(fui) = ui_weak.upgrade() {
                    let pct = done as f64 * 100.0 / total.max(1) as f64;
                    fui.set_session_status(
                        format!("发送文件：{name} {done}/{total} ({pct:.0}%)").into(),
                    );
                    fui.set_file_progress((pct / 100.0) as f32);
                    fui.set_file_label(format!("发送 {name} {pct:.0}%").into());
                }
            } else if let Some((name, done, total)) = st.receiving {
                if let Some(fui) = ui_weak.upgrade() {
                    let pct = done as f64 * 100.0 / total.max(1) as f64;
                    fui.set_session_status(
                        format!("接收文件：{name} {done}/{total} ({pct:.0}%)").into(),
                    );
                    fui.set_file_progress((pct / 100.0) as f32);
                    fui.set_file_label(format!("接收 {name} {pct:.0}%").into());
                }
            } else if let Some(fui) = ui_weak.upgrade() {
                fui.set_file_progress(-1.0);
                fui.set_file_label("".into());
            }
        }
        if last_stat.elapsed() >= Duration::from_secs(2) {
            if let Some(fui) = ui_weak.upgrade() {
                let audio = if muted.load(Ordering::SeqCst) {
                    "音频已静音".to_string()
                } else {
                    format!("音频 {audio_played}帧 缓存{audio_buffered} 丢{audio_dropped}")
                };
                fui.set_session_status(format!("会话中 · H.264 {frames}帧/2s · {audio}").into());
            }
            frames = 0;
            audio_played = 0;
            audio_dropped = 0;
            last_stat = Instant::now();
        }
        std::thread::sleep(Duration::from_millis(1));
    }
    // 会话结束（断开置 stop）：清理注册表与 UI 槽位。
    if let Some(ui) = ui_weak.upgrade() {
        crate::session_cleanup(&ui, session_idx);
    }
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
    use aerodesk_macos::decode::to_rgba;
    use aerodesk_macos::synthetic::SyntheticSource;
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
