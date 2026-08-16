//! 观看端泛型管线（#277）：连接 → 收流 → AccessUnit 组装 → core `Decoder`
//! → core `Renderer`。平台差异收敛在解码器/渲染器工厂，循环本身只依赖 trait。
//!
//! macOS 增强版（音频/A-V 同步/文件传输）仍在 macos_media；本泛型核心先服务
//! Windows/Linux（SoftDecoder + SlintRenderer），后续可承载 macOS 全量。

use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::{Duration, Instant};

use crate::with_ui;
use aerodesk_core::access_unit::AccessUnitAssembler;
use aerodesk_core::connect::connect_live_role;
use aerodesk_core::endpoint::ClientEvent;
use aerodesk_core::platform::{Decoder, Renderer};
use aerodesk_protocol::cmd::{CmdResponse, CmdResult};
use aerodesk_protocol::signal::Role;
use str0m::net::Protocol;

/// 解析 cursor 通道的 `CursorPos`（#75；返回归一化 0..1 坐标，与 macOS UI 一致）。
fn cursor_pos(data: &[u8]) -> Option<(f32, f32)> {
    serde_json::from_slice::<aerodesk_protocol::cursor::CursorPos>(data)
        .ok()
        .map(|p| (p.x as f32, p.y as f32))
}

/// 把终端命令响应格式化为窗口可读文本（stdout/stderr/错误/截断提示）。
pub(crate) fn format_cmd_response(response: &CmdResponse) -> String {
    match &response.result {
        CmdResult::Run {
            exit_code,
            stdout,
            stderr,
            truncated,
            error,
        } => {
            let mut out = String::new();
            if let Some(error) = error {
                out.push_str(&format!("[错误] {error}"));
            }
            if !stdout.is_empty() {
                if !out.is_empty() {
                    out.push('\n');
                }
                out.push_str(stdout.trim_end_matches(['\r', '\n']));
            }
            if !stderr.is_empty() {
                if !out.is_empty() {
                    out.push('\n');
                }
                out.push_str(stderr.trim_end_matches(['\r', '\n']));
            }
            if *truncated {
                out.push_str("\n[输出已截断]");
            }
            if out.is_empty() {
                out.push_str("(无输出)");
            }
            let code = exit_code
                .map(|c| c.to_string())
                .unwrap_or_else(|| "无退出码".to_string());
            format!("{out}\n[exit={code}]")
        }
        CmdResult::File { size, error, .. } => {
            if let Some(error) = error {
                format!("[文件响应错误] {error}")
            } else {
                format!("[文件响应] {size} 字节")
            }
        }
        CmdResult::ProcessList { processes, error } => {
            if let Some(error) = error {
                format!("[进程列表错误] {error}")
            } else {
                let mut out = String::from("[进程列表]");
                for p in processes {
                    out.push_str(&format!("\n  {} {}", p.pid, p.name));
                }
                out
            }
        }
        CmdResult::Killed { pid, error } => {
            if let Some(error) = error {
                format!("[结束进程 {pid} 失败] {error}")
            } else {
                format!("[已结束进程 {pid}]")
            }
        }
    }
}

/// 运行泛型观看会话（阻塞直到断开/代际失效）。
///
/// - `decoder_label`：状态栏解码器显示名（如 "OpenH264 软解"）。
/// - `mk_decoder`：连接成功后惰性创建解码器（可失败）。
/// - `mk_renderer`：连接成功后创建渲染器。
#[allow(clippy::too_many_arguments)]
pub fn run_viewer_generic<D, R, DF, RF>(
    server: String,
    room: String,
    token: Option<String>,
    ui_weak: slint::Weak<crate::AppWindow>,
    session_idx: usize,
    input_rx: std::sync::mpsc::Receiver<String>,
    cmd_rx: std::sync::mpsc::Receiver<aerodesk_protocol::cmd::CmdRequest>,
    file_cmd_rx: std::sync::mpsc::Receiver<crate::FileCmd>,
    chat_cmd_rx: std::sync::mpsc::Receiver<crate::ChatCmd>,
    stop: Arc<AtomicBool>,
    view_only: Arc<AtomicBool>,
    decoder_label: &'static str,
    mut mk_decoder: DF,
    mut mk_renderer: RF,
) where
    D: Decoder + 'static,
    R: Renderer + 'static,
    DF: FnMut() -> Result<D, String>,
    RF: FnMut() -> R,
{
    // #29 多会话：本会话 stop 置位即退出（断开只关当前活动会话）。
    let stale = || stop.load(Ordering::SeqCst);
    let auth = token.as_deref().filter(|t| !t.is_empty());
    // connect_live_role 在异常环境可能阻塞（如 UDP read_timeout 失效）；
    // 放子线程 + 20s 超时保护，避免 UI 线程永久挂起。
    let (tx, rx) = std::sync::mpsc::channel::<Result<_, String>>();
    let srv = server.clone();
    let rm = room.clone();
    let auth2 = auth.map(|s| s.to_string());
    std::thread::spawn(move || {
        let r = connect_live_role(&srv, &rm, Role::Viewer, auth2.as_deref());
        let _ = tx.send(r);
    });
    let mut live = match rx.recv_timeout(Duration::from_secs(20)) {
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
                    ui.set_status("连接超时：请检查服务器 ws://地址:端口 / token / 网络；对方未在线时也会等待媒体流".into());
                });
            }
            crate::session_cleanup_weak(
                &ui_weak,
                session_idx,
                Some("连接超时：请检查服务器 ws://地址:端口 / token / 网络".into()),
            );
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
    let connected_status = format!("已连接：peer={peer} ice={ice}");
    let main_status = connected_status.clone();
    with_ui(&ui_weak, move |ui| {
        ui.set_status(main_status.into());
        ui.set_log(
            format!(
                "设备: {room2}\n服务器: {server2}\nSDP 交换: OK\nICE: {}\n\n{decoder_label}渲染。",
                if ice {
                    "connected"
                } else {
                    "pending(5s 超时)"
                }
            )
            .into(),
        );
        crate::add_recent(ui, &room2, &server2);
        ui.set_conn_state(2);
    });
    crate::session_set_status(&ui_weak, session_idx, connected_status);
    // #438：连上信令只表示设备已连接/可被找到，不进入观察页；
    // 收到首个渲染帧后才 session_joined_weak 进入会话视图。

    let mut assembler = AccessUnitAssembler::new();
    // #72/#271 文件/剪贴板状态机：观看端不落盘接收，但剪贴板文本/图片接收生效。
    let mut file_transfer = aerodesk_core::file_transfer::FileTransfer::new(None);
    // #271 剪贴板自动同步：本机复制内容 → 1s 节流轮询 → 发往被控端。
    // last_clip_img 为最近一次已发送/已应用的图片字节，防回声（远端写回又发回）。
    let mut last_clip_poll: Option<Instant> = None;
    let mut last_clip_img: Option<Vec<u8>> = None;
    let mut last_file_status = Instant::now();
    let mut decoder: Option<D> = None;
    let mut renderer: Option<R> = None;
    let mut frames: u64 = 0;
    let mut pkts: u64 = 0;
    let mut media_evts: u64 = 0;
    let mut last_stat = Instant::now();
    // #73 观看端音频播放：PCMU 解码 → jitter buffer → cpal AudioSink（全平台）。
    let mut audio_sink: Option<aerodesk_core::audio_sink::AudioSink> = None;
    let mut avsync = aerodesk_core::avsync::AvSync::new();
    let mut jitter = aerodesk_core::avsync::AudioJitterBuffer::new(0.08);
    let mut audio_frames: u64 = 0;
    let mut audio_played: u64 = 0;
    // #73 Opus（48kHz）解码器（惰性创建；不可用时仅统计不播放）。
    let mut opus_decoder: Option<aerodesk_ffmpeg::audio::OpusDecoder> = None;
    // #136 关键帧请求：首包/不连续/切层时向 SFU 发 PLI（节流 1s）。
    let mut last_kf_request: Option<std::time::Instant> = None;
    let mut last_kf_rid: Option<str0m::media::Rid> = None;
    let mut seen_video = false;
    let mut session_ui_joined = false;
    // #425：连接建立后 10s 内无任何 RTP → 提示"对方不在线/未开启被控"（保持等待）。
    let no_media_deadline = Instant::now() + Duration::from_secs(10);
    let mut no_media_notified = false;
    while !stale() {
        // 输入事件：UI 键鼠 → input data channel → SFU → 被控端。
        // #441 观看模式（仅观看）不发送键鼠输入。
        if !view_only.load(Ordering::SeqCst) {
            while let Ok(json) = input_rx.try_recv() {
                live.endpoint
                    .send_channel_data("input", false, json.as_bytes());
            }
        }
        // #109/#452 终端命令：UI 终端窗口 → cmd data channel → SFU → 被控端执行。
        while let Ok(req) = cmd_rx.try_recv() {
            if let Ok(json) = serde_json::to_string(&req) {
                let sent = live
                    .endpoint
                    .send_channel_data("cmd", false, json.as_bytes());
                if !sent {
                    crate::append_terminal_output(
                        session_idx,
                        "[错误] cmd 通道未就绪，命令未送达".to_string(),
                    );
                }
            }
        }
        // #72/#271 文件/剪贴板命令（UI 工具栏）：发送文件/剪贴板文本/图片、取消。
        while let Ok(cmd) = file_cmd_rx.try_recv() {
            match cmd {
                crate::FileCmd::SendFile(path) => match file_transfer.send_file(&path) {
                    Ok(()) => {
                        let msg = format!("开始发送文件：{}", path.display());
                        crate::session_set_status(&ui_weak, session_idx, msg);
                    }
                    Err(e) => {
                        let msg = format!("发送失败：{e}");
                        crate::session_set_status(&ui_weak, session_idx, msg);
                    }
                },
                crate::FileCmd::SendClipboard(text) => {
                    aerodesk_core::clipboard::set_cache(text.clone());
                    let sent = file_transfer.send_clipboard(&text, &mut live.endpoint);
                    let msg = if sent {
                        "已发送剪贴板到被控端".to_string()
                    } else {
                        "剪贴板：file 通道未就绪".to_string()
                    };
                    crate::session_set_status(&ui_weak, session_idx, msg);
                }
                crate::FileCmd::SendClipboardImage(png) => {
                    match file_transfer.send_clipboard_image(png) {
                        Ok(()) => {
                            let msg = "已发送剪贴板图片到被控端".to_string();
                            crate::session_set_status(&ui_weak, session_idx, msg);
                        }
                        Err(e) => {
                            let msg = format!("剪贴板图片发送失败：{e}");
                            crate::session_set_status(&ui_weak, session_idx, msg);
                        }
                    }
                }
                crate::FileCmd::Cancel => {
                    file_transfer.cancel_send(&mut live.endpoint);
                }
            }
        }
        // #458 聊天消息：UI 聊天窗口 → chat data channel → SFU → 被控端。
        while let Ok(cmd) = chat_cmd_rx.try_recv() {
            match cmd {
                crate::ChatCmd::Send(text) => {
                    let text = text.trim().to_string();
                    if text.is_empty() {
                        continue;
                    }
                    let payload = serde_json::json!({
                        "sender": "我",
                        "text": text,
                        "timestamp_ms": crate::system_time_millis(),
                    });
                    if let Ok(json) = serde_json::to_string(&payload) {
                        let sent = live
                            .endpoint
                            .send_channel_data("chat", false, json.as_bytes());
                        if !sent {
                            crate::set_message_window_status(
                                session_idx,
                                "发送失败：chat 通道未就绪".to_string(),
                            );
                        }
                    }
                }
            }
        }
        live.socket
            .set_read_timeout(Some(Duration::from_millis(10)))
            .ok();
        let mut buf = [0u8; 4096];
        if let Ok((n, source)) = live.socket.recv_from(&mut buf) {
            pkts += 1;
            if pkts % 200 == 0 {
                eprintln!("generic viewer: udp={pkts} media={media_evts} frames={frames}");
            }
            if let Ok(contents) = buf[..n].try_into() {
                let _ = live.endpoint.handle_input(str0m::Input::Receive(
                    std::time::Instant::now(),
                    str0m::net::Receive {
                        proto: Protocol::Udp,
                        source,
                        destination: live.socket.local_addr().unwrap(),
                        contents,
                    },
                ));
            }
        }
        let _ = live.endpoint.handle_timeout(std::time::Instant::now());
        while let Some(output) = live.endpoint.poll_output() {
            match output {
                str0m::Output::Transmit(t) => {
                    let _ = live.socket.send_to(&t.contents, t.destination);
                }
                // 必须 break：sans-IO 的 Timeout 不消费会 100% CPU 死循环（#125 教训）。
                str0m::Output::Timeout(_) => break,
                str0m::Output::Event(_) => {}
            }
        }
        while let Some(ev) = live.endpoint.poll_event() {
            // #72 文件通道事件交给状态机（非 file 事件为 no-op）。
            file_transfer.handle_event(&ev, &mut live.endpoint);
            // #109/#452 终端命令响应 → 终端独立窗口回显。
            if let ClientEvent::ChannelData(cid, _, data) = &ev
                && live.endpoint.channel_label(*cid).as_deref() == Some("cmd")
                && let Ok(response) = serde_json::from_slice::<CmdResponse>(data)
            {
                crate::append_terminal_output(session_idx, format_cmd_response(&response));
            }
            // #75 远程光标：被控端经 cursor 通道广播位置 → UI 叠加（与 macOS UI 一致）。
            if let ClientEvent::ChannelData(cid, _, data) = &ev
                && live.endpoint.channel_label(*cid).as_deref() == Some("cursor")
                && let Some((cx, cy)) = cursor_pos(data)
            {
                crate::with_session_ui_state(&ui_weak, session_idx, move |s| {
                    s.cursor = Some((cx, cy));
                });
            }
            // #458 聊天消息：被控端经 chat 通道回传 → 聊天窗口消息列表。
            if let ClientEvent::ChannelData(cid, _, data) = &ev
                && live.endpoint.channel_label(*cid).as_deref() == Some("chat")
                && let Some((sender, text)) = crate::decode_chat_text(data)
            {
                crate::append_chat_message(session_idx, sender, text, false);
            }
            if let ClientEvent::Media(data) = ev {
                media_evts += 1;
                // #73 音频识别：用协商 codec（PCMU/Opus）区分，不按 mid（SFU 转发
                // 用本地 mid）。当前播放 PCMU（默认音频）；Opus 需 aerodesk-ffmpeg 非
                // macOS 依赖，留后续。
                if data.params.spec().codec == str0m::format::Codec::PCMU {
                    audio_frames += 1;
                    if audio_sink.is_none() {
                        audio_sink = aerodesk_core::audio_sink::AudioSink::new_with_rate(8000).ok();
                    }
                    let pcm = aerodesk_core::pcmu::pcmu_decode(&data.data);
                    avsync.on_audio(data.time.numer(), data.time.denom());
                    jitter.push(avsync.audio_time_secs(), pcm);
                    if let Some(sink) = &mut audio_sink {
                        while let Some(f) = jitter.pop(avsync.audio_time_secs()) {
                            sink.push_pcm(&f);
                            audio_played += 1;
                        }
                    }
                    continue;
                }
                if data.params.spec().codec == str0m::format::Codec::Opus {
                    audio_frames += 1;
                    if opus_decoder.is_none() {
                        opus_decoder = aerodesk_ffmpeg::audio::OpusDecoder::new().ok();
                    }
                    if audio_sink.is_none() {
                        audio_sink =
                            aerodesk_core::audio_sink::AudioSink::new_with_rate(48_000).ok();
                    }
                    if let (Some(dec), Some(sink)) = (&mut opus_decoder, &mut audio_sink) {
                        if let Some(pcm) = dec.decode(&data.data).ok().flatten() {
                            avsync.on_audio(data.time.numer(), data.time.denom());
                            jitter.push(avsync.audio_time_secs(), pcm);
                            while let Some(f) = jitter.pop(avsync.audio_time_secs()) {
                                sink.push_pcm(&f);
                                audio_played += 1;
                            }
                        }
                    }
                    continue;
                }
                // #136 首包 / 不连续 / 切层 → 请求关键帧（PLI，节流 1s）。
                let now = std::time::Instant::now();
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
                // SFU 转发用本地 mid（非协商 mid），iOS/CLI 同款：不过滤直接组装。
                if let Some(au) = assembler.push(
                    data.data.as_ref(),
                    data.time.as_micros(),
                    data.is_keyframe(),
                ) {
                    if decoder.is_none() {
                        match mk_decoder() {
                            Ok(d) => decoder = Some(d),
                            Err(e) => {
                                // 与正常退出路径一致：先清理会话注册表与 UI 槽位，
                                // 否则会话残留（标签卡住、无法再次连接该槽位）。
                                eprintln!("decoder init failed: {e}");
                                crate::session_cleanup_weak(
                                    &ui_weak,
                                    session_idx,
                                    Some(format!("解码器初始化失败：{e}")),
                                );
                                return;
                            }
                        }
                    }
                    if renderer.is_none() {
                        renderer = Some(mk_renderer());
                    }
                    let unit = aerodesk_core::media_pipeline::EncodedUnit {
                        data: au.data.clone(),
                        keyframe: data.is_keyframe(),
                        pts_ms: au.pts_us / 1000,
                        rtp_timestamp: 0,
                    };
                    let decoded = decoder
                        .as_mut()
                        .and_then(|d| Decoder::decode(d, &unit).ok().flatten());
                    if let Some(frame) = decoded
                        && let Some(r) = renderer.as_mut()
                        && Renderer::render(r, &frame).is_ok()
                    {
                        frames += 1;
                        if last_stat.elapsed() >= Duration::from_secs(5) {
                            eprintln!("generic viewer: decoded {frames} frames");
                            last_stat = Instant::now();
                        }
                    }
                }
            }
        }
        // #425：连接建立后无媒体流 → 明确提示，而非一直"等待媒体流"；
        // 收到首个视频帧后恢复"会话中"。
        if !no_media_notified && media_evts == 0 && Instant::now() >= no_media_deadline {
            no_media_notified = true;
            let room_msg = format!("对方不在线或未开启被控（设备 {room}），等待对方上线后自动出流");
            crate::session_set_status(&ui_weak, session_idx, room_msg.clone());
            with_ui(&ui_weak, move |ui| ui.set_status(room_msg.into()));
        }
        if frames > 0 && !session_ui_joined {
            session_ui_joined = true;
            crate::session_joined_weak(&ui_weak, session_idx);
            crate::session_set_status(&ui_weak, session_idx, "会话中 · 媒体流已接通".to_string());
        }
        if frames > 0 && no_media_notified {
            no_media_notified = false;
        }
        // #72 文件传输推进 + 剪贴板接收落地（文本/图片写入系统剪贴板）。
        file_transfer.tick(&mut live.endpoint);
        if let Some(text) = file_transfer.take_incoming_clipboard() {
            aerodesk_core::clipboard::set_cache(text.clone());
            aerodesk_core::clipboard::write(&text);
            crate::session_set_status(&ui_weak, session_idx, "已应用远端剪贴板文本".to_string());
        }
        if let Some(png) = file_transfer.take_incoming_clipboard_image() {
            let ok = aerodesk_core::clipboard::write_image(&png);
            // 防回声：远端图片已落地，视为已同步（避免自动轮询原样发回）。
            last_clip_img = Some(png);
            let msg = if ok {
                "已应用远端剪贴板图片".to_string()
            } else {
                "远端剪贴板图片写入失败".to_string()
            };
            crate::session_set_status(&ui_weak, session_idx, msg);
        }
        // #452 文件传输进度：500ms 节流同步到会话状态和独立文件窗口。
        if last_file_status.elapsed() >= Duration::from_millis(500) {
            last_file_status = Instant::now();
            let st = file_transfer.status();
            if let Some(msg) = st.message {
                crate::session_set_status(&ui_weak, session_idx, msg.clone());
                crate::clear_file_window_progress(session_idx, Some(msg));
            } else if let Some((name, done, total)) = st.sending {
                let pct = done as f64 * 100.0 / total.max(1) as f64;
                let label = format!("发送 {name} {pct:.0}%");
                crate::session_set_status(
                    &ui_weak,
                    session_idx,
                    format!("发送文件：{name} {done}/{total} ({pct:.0}%)"),
                );
                crate::update_file_window_progress(
                    session_idx,
                    (pct / 100.0) as f32,
                    label,
                    format!("正在发送：{name}"),
                );
            } else if let Some((name, done, total)) = st.receiving {
                let pct = done as f64 * 100.0 / total.max(1) as f64;
                let label = format!("接收 {name} {pct:.0}%");
                crate::session_set_status(
                    &ui_weak,
                    session_idx,
                    format!("接收文件：{name} {done}/{total} ({pct:.0}%)"),
                );
                crate::update_file_window_progress(
                    session_idx,
                    (pct / 100.0) as f32,
                    label,
                    format!("正在接收：{name}"),
                );
            } else {
                crate::clear_file_window_progress(session_idx, None);
            }
        }
        // #271 剪贴板自动同步（1s 节流）：图片优先，否则文本；变化才发，防回声。
        if last_clip_poll
            .map(|t| t.elapsed() >= Duration::from_secs(1))
            .unwrap_or(true)
        {
            last_clip_poll = Some(Instant::now());
            match decide_clipboard_sync(
                aerodesk_core::clipboard::read_image(),
                aerodesk_core::clipboard::read(),
                aerodesk_core::clipboard::cached().as_deref(),
                last_clip_img.as_deref(),
            ) {
                ClipboardSync::Image(png) => {
                    if file_transfer.send_clipboard_image(png.clone()).is_ok() {
                        last_clip_img = Some(png);
                        tracing::info!("clipboard auto-sync: image sent");
                    }
                }
                ClipboardSync::Text(text) => {
                    if file_transfer.send_clipboard(&text, &mut live.endpoint) {
                        aerodesk_core::clipboard::set_cache(text.clone());
                        tracing::info!("clipboard auto-sync: text sent");
                    }
                }
                ClipboardSync::None => {}
            }
        }
        std::thread::sleep(Duration::from_millis(1));
    }
    // 会话结束（断开置 stop）：提示后清理注册表与 UI 槽位。
    let msg = format!("已断开：{room}");
    with_ui(&ui_weak, move |ui| ui.set_status(msg.into()));
    crate::session_cleanup_weak(&ui_weak, session_idx, None);
}

/// 剪贴板自动同步决策结果（#271）。
#[derive(Debug, PartialEq, Eq)]
enum ClipboardSync {
    Image(Vec<u8>),
    Text(String),
    None,
}

/// 剪贴板自动同步决策（纯逻辑，便于单测）：图片优先于文本；内容未变化不发送。
/// - `read_image`：当前剪贴板图片（PNG，无则 None）
/// - `read_text`：当前剪贴板文本（无则 None）
/// - `cached_text`：最近一次已同步文本（防回声）
/// - `last_img`：最近一次已同步图片字节（防回声）
fn decide_clipboard_sync(
    read_image: Option<Vec<u8>>,
    read_text: Option<String>,
    cached_text: Option<&str>,
    last_img: Option<&[u8]>,
) -> ClipboardSync {
    if let Some(png) = read_image {
        if last_img == Some(png.as_slice()) {
            return ClipboardSync::None;
        }
        return ClipboardSync::Image(png);
    }
    match read_text {
        Some(text) if !text.is_empty() && cached_text != Some(text.as_str()) => {
            ClipboardSync::Text(text)
        }
        _ => ClipboardSync::None,
    }
}

#[cfg(test)]
mod tests {

    /// #75 远程光标 CursorPos JSON 解析（归一化坐标 + 旧端无 sent_ms 兼容）。
    #[test]
    fn cursor_pos_parses_normalized() {
        let json = br#"{"x":0.5,"y":0.25,"sent_ms":123}"#;
        assert_eq!(super::cursor_pos(json), Some((0.5, 0.25)));
        // 旧端无 sent_ms（serde default）也能解析。
        let old = br#"{"x":0.1,"y":0.9}"#;
        assert_eq!(super::cursor_pos(old), Some((0.1, 0.9)));
        assert_eq!(super::cursor_pos(b"not json"), None);
        assert_eq!(super::cursor_pos(b""), None);
    }

    use super::*;

    #[test]
    fn format_cmd_response_shows_run_output() {
        let response = CmdResponse {
            id: 1,
            result: CmdResult::Run {
                exit_code: Some(0),
                stdout: "hello\n".into(),
                stderr: String::new(),
                truncated: false,
                error: None,
            },
        };
        let text = format_cmd_response(&response);
        assert!(text.contains("hello"));
        assert!(text.contains("[exit=0]"));
        assert!(!text.contains("\n\n[exit=0]"));
    }

    #[test]
    fn format_cmd_response_shows_errors_and_truncation() {
        let response = CmdResponse {
            id: 2,
            result: CmdResult::Run {
                exit_code: Some(1),
                stdout: String::new(),
                stderr: "denied".into(),
                truncated: true,
                error: Some("blocked by policy".into()),
            },
        };
        let text = format_cmd_response(&response);
        assert!(text.contains("[错误] blocked by policy"));
        assert!(text.contains("denied"));
        assert!(text.contains("[输出已截断]"));
        assert!(text.contains("[exit=1]"));
    }

    #[test]
    fn decide_sync_prefers_image_and_dedups() {
        let png = vec![1u8, 2, 3, 4];
        // 图片优先于文本。
        assert_eq!(
            decide_clipboard_sync(Some(png.clone()), Some("hello".into()), None, None),
            ClipboardSync::Image(png.clone())
        );
        // 图片未变化 → None。
        assert_eq!(
            decide_clipboard_sync(Some(png.clone()), None, None, Some(png.as_slice())),
            ClipboardSync::None
        );
    }

    #[test]
    fn decide_sync_text_dedup_and_empty() {
        assert_eq!(
            decide_clipboard_sync(None, Some("hi".into()), None, None),
            ClipboardSync::Text("hi".into())
        );
        // 与缓存相同 → None（防回声）。
        assert_eq!(
            decide_clipboard_sync(None, Some("hi".into()), Some("hi"), None),
            ClipboardSync::None
        );
        // 空文本/无内容 → None。
        assert_eq!(
            decide_clipboard_sync(None, Some("".into()), None, None),
            ClipboardSync::None
        );
        assert_eq!(
            decide_clipboard_sync(None, None, None, None),
            ClipboardSync::None
        );
    }
}
