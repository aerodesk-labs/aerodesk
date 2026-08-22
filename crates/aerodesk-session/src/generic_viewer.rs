//! 观看端泛型管线（#277）：连接 → 收流 → AccessUnit 组装 → core `Decoder`
//! → core `Renderer`。平台差异收敛在解码器/渲染器工厂，循环本身只依赖 trait。
//!
//! macOS 增强版（音频/A-V 同步/文件传输）仍在 macos_media；本泛型核心先服务
//! Windows/Linux（SoftDecoder + SlintRenderer），后续可承载 macOS 全量。
//!
//! #508 B1：UI 副作用全部经 [`crate::SessionUi`] 缝回传，本模块不再引用 Slint。

use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::{Duration, Instant};

use aerodesk_core::access_unit::AccessUnitAssembler;
use aerodesk_core::connect::{LiveSession, connect_live_role};
use aerodesk_core::endpoint::ClientEvent;
use aerodesk_core::p2p_call::P2pCall;
use aerodesk_core::platform::{Decoder, Renderer};
use aerodesk_core::protocol::cmd::{CmdAction, CmdRequest, CmdResponse, CmdResult};
use aerodesk_core::protocol::signal::Role;
use str0m::net::Protocol;

use crate::SessionUi;

/// 观看端传输抽象：SFU（LiveSession）与 P2P（已建立的 P2pCall）共用会话泵。
/// P2P 模式 `poll_event` 以首个 `ChannelOpen` 为会话就绪（DTLS 完成 ⟺ SRTP
/// 密钥就绪；ICE connected 早于就绪——早到媒体由接收侧静默丢弃）。
enum ViewerTransport {
    Sfu(LiveSession),
    Peer(P2pCall),
}

impl ViewerTransport {
    fn pump(&mut self) {
        match self {
            Self::Sfu(live) => {
                live.socket
                    .set_read_timeout(Some(Duration::from_millis(10)))
                    .ok();
                let mut buf = [0u8; 4096];
                if let Ok((n, source)) = live.socket.recv_from(&mut buf) {
                    if let Ok(contents) = buf[..n].try_into() {
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
                }
                let _ = live.endpoint.handle_timeout(Instant::now());
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
            }
            Self::Peer(p2p) => {
                let _ = p2p.poll();
            }
        }
    }

    fn poll_event(&mut self) -> Option<ClientEvent> {
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
}

/// 解析 cursor 通道的 `CursorPos`（#75；归一化 0..1 坐标 + 发送端墙钟，
/// 观看端据此计算端到端单向延时）。
fn cursor_pos(data: &[u8]) -> Option<aerodesk_core::protocol::cursor::CursorPos> {
    serde_json::from_slice::<aerodesk_core::protocol::cursor::CursorPos>(data).ok()
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
            code: _,
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
        CmdResult::ProcessList {
            processes,
            error,
            code: _,
        } => {
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
        CmdResult::Killed {
            pid,
            error,
            code: _,
        } => {
            if let Some(error) = error {
                format!("[结束进程 {pid} 失败] {error}")
            } else {
                format!("[已结束进程 {pid}]")
            }
        }
        CmdResult::Chat { text, .. } => format!("[消息] {text}"),
    }
}

/// 运行泛型观看会话（阻塞直到断开/代际失效）。
///
/// - `ui`：会话事件缝（desktop 为 Slint 适配器；槽位语义含在实现内，
///   引擎自身不再消费 session_idx）。
/// - `decoder_label`：状态栏解码器显示名（如 "OpenH264 软解"）。
/// - `mk_decoder`：连接成功后惰性创建解码器（可失败）。
/// - `mk_renderer`：连接成功后创建渲染器。
#[allow(clippy::too_many_arguments)]
pub fn run_viewer_generic<U, D, R, DF, RF>(
    server: String,
    room: String,
    token: Option<String>,
    ui: U,
    input_rx: std::sync::mpsc::Receiver<String>,
    cmd_rx: std::sync::mpsc::Receiver<aerodesk_core::protocol::cmd::CmdRequest>,
    file_cmd_rx: std::sync::mpsc::Receiver<crate::FileCmd>,
    chat_cmd_rx: std::sync::mpsc::Receiver<crate::ChatCmd>,
    stop: Arc<AtomicBool>,
    view_only: Arc<AtomicBool>,
    decoder_label: &'static str,
    mk_decoder: DF,
    mk_renderer: RF,
) where
    U: SessionUi,
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
    let (tx, rx) = std::sync::mpsc::channel::<Result<_, aerodesk_core::connect::ConnectError>>();
    let srv = server.clone();
    let rm = room.clone();
    let auth2 = auth.map(|s| s.to_string());
    std::thread::spawn(move || {
        let r = connect_live_role(&srv, &rm, Role::Viewer, auth2.as_deref());
        let _ = tx.send(r);
    });
    let live = match rx.recv_timeout(Duration::from_secs(20)) {
        Ok(Ok(l)) => l,
        Ok(Err(e)) => {
            let msg = format!("连接失败：{e}");
            let terminal = msg.clone();
            if !stale() {
                ui.set_conn_state(3);
                ui.set_status(msg);
            }
            ui.cleanup(Some(terminal));
            return;
        }
        Err(_) => {
            if !stale() {
                ui.set_conn_state(3);
                ui.set_status("连接超时：请检查服务器 ws://地址:端口 / token / 网络；对方未在线时也会等待媒体流".into());
            }
            ui.cleanup(Some(
                "连接超时：请检查服务器 ws://地址:端口 / token / 网络".into(),
            ));
            return;
        }
    };
    if stale() {
        ui.cleanup(None);
        return;
    }
    let peer = live.peer_id.clone();
    let ice = live.ice_connected;
    let room2 = room.clone();
    let server2 = server.clone();
    let connected_status = format!("已连接：peer={peer} ice={ice}");
    ui.set_status(connected_status.clone());
    ui.set_log(format!(
        "设备: {room2}\n服务器: {server2}\nSDP 交换: OK\nICE: {}\n\n{decoder_label}渲染。",
        if ice {
            "connected"
        } else {
            "pending(5s 超时)"
        }
    ));
    ui.add_recent(&room2, &server2);
    ui.set_conn_state(2);
    ui.session_status(connected_status);
    // #438：连上信令只表示设备已连接/可被找到，不进入观察页；
    // 收到首个渲染帧后才 joined 进入会话视图。
    run_viewer_impl(
        ViewerTransport::Sfu(live),
        room,
        ui,
        input_rx,
        cmd_rx,
        file_cmd_rx,
        chat_cmd_rx,
        stop,
        view_only,
        mk_decoder,
        mk_renderer,
    );
}

/// #552：SIP 1:1 P2P 观看入口——P2pCall 已建立（create_offer→call→Answered→
/// accept_answer 由调用方完成），解码/渲染/通道与 SFU 路径共用同一会话泵。
#[allow(clippy::too_many_arguments)]
pub fn run_viewer_generic_peer<U, D, R, DF, RF>(
    p2p: aerodesk_core::p2p_call::P2pCall,
    room: String,
    ui: U,
    input_rx: std::sync::mpsc::Receiver<String>,
    cmd_rx: std::sync::mpsc::Receiver<aerodesk_core::protocol::cmd::CmdRequest>,
    file_cmd_rx: std::sync::mpsc::Receiver<crate::FileCmd>,
    chat_cmd_rx: std::sync::mpsc::Receiver<crate::ChatCmd>,
    stop: Arc<AtomicBool>,
    view_only: Arc<AtomicBool>,
    decoder_label: &'static str,
    mk_decoder: DF,
    mk_renderer: RF,
) where
    U: SessionUi,
    D: Decoder + 'static,
    R: Renderer + 'static,
    DF: FnMut() -> Result<D, String>,
    RF: FnMut() -> R,
{
    let connected_status = format!("已连接：1:1 会话（{room}）");
    ui.set_status(connected_status.clone());
    ui.set_log(format!(
        "设备: {room}\nSIP 1:1 P2P 会话\nSDP 交换: OK\n\n{decoder_label}渲染。"
    ));
    ui.add_recent(&room, &"sip".to_string());
    ui.set_conn_state(2);
    ui.session_status(connected_status);
    run_viewer_impl(
        ViewerTransport::Peer(p2p),
        room,
        ui,
        input_rx,
        cmd_rx,
        file_cmd_rx,
        chat_cmd_rx,
        stop,
        view_only,
        mk_decoder,
        mk_renderer,
    );
}

/// 会话泵（SFU/P2P 共用）：媒体收发、通道事件、解码渲染、文件/剪贴板/统计。
#[allow(clippy::too_many_arguments)]
fn run_viewer_impl<U, D, R, DF, RF>(
    mut t: ViewerTransport,
    room: String,
    ui: U,
    input_rx: std::sync::mpsc::Receiver<String>,
    cmd_rx: std::sync::mpsc::Receiver<aerodesk_core::protocol::cmd::CmdRequest>,
    file_cmd_rx: std::sync::mpsc::Receiver<crate::FileCmd>,
    chat_cmd_rx: std::sync::mpsc::Receiver<crate::ChatCmd>,
    stop: Arc<AtomicBool>,
    view_only: Arc<AtomicBool>,
    mut mk_decoder: DF,
    mut mk_renderer: RF,
) where
    U: SessionUi,
    D: Decoder + 'static,
    R: Renderer + 'static,
    DF: FnMut() -> Result<D, String>,
    RF: FnMut() -> R,
{
    let stale = || stop.load(Ordering::SeqCst);
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
    let mut media_evts: u64 = 0;
    let mut last_stat = Instant::now();
    // #73 观看端音频播放：PCMU 解码 → jitter buffer → cpal AudioSink（全平台）。
    let mut audio_sink: Option<aerodesk_core::audio_sink::AudioSink> = None;
    let mut avsync = aerodesk_core::avsync::AvSync::new();
    let mut jitter = aerodesk_core::avsync::AudioJitterBuffer::new(0.08);
    let mut audio_frames: u64 = 0;
    let mut audio_played: u64 = 0;
    // #73 Opus（48kHz）解码器（惰性创建；不可用时仅统计不播放）。
    let mut opus_decoder: Option<aerodesk_codec::audio::OpusDecoder> = None;
    // #136 关键帧请求：首包/不连续/切层时向 SFU 发 PLI（节流 1s）。
    let mut last_kf_request: Option<std::time::Instant> = None;
    let mut last_kf_rid: Option<str0m::media::Rid> = None;
    let mut seen_video = false;
    let mut session_ui_joined = false;
    // #425：连接建立后 10s 内无任何 RTP → 提示"对方不在线/未开启被控"（保持等待）。
    let no_media_deadline = Instant::now() + aerodesk_core::util::NO_MEDIA_DEADLINE;
    let mut no_media_notified = false;
    // 会话延时统计：端到端单向（cursor sent_ms）/ 网络 RTT（RTCP PeerStats）/
    // 解码帧率，500ms 节流推送给 UI。
    let mut last_e2e_ms: Option<u64> = None;
    let mut last_stats_push = Instant::now();
    let mut last_stats_frames: u64 = 0;
    while !stale() {
        // 输入事件：UI 键鼠 → input data channel → SFU → 被控端。
        // #441 观看模式（仅观看）不发送键鼠输入。
        if !view_only.load(Ordering::SeqCst) {
            let mut sent = 0u32;
            while let Ok(json) = input_rx.try_recv() {
                let ok = t
                    .endpoint()
                    .send_channel_data("input", false, json.as_bytes());
                if !ok {
                    tracing::warn!("input 通道发送失败（通道未建立？）");
                }
                sent += 1;
            }
            if sent > 0 {
                tracing::debug!("input 发送 {sent} 条");
            }
        }
        // #109/#452 终端命令：UI 终端窗口 → cmd data channel → SFU → 被控端执行。
        while let Ok(req) = cmd_rx.try_recv() {
            if let Ok(json) = serde_json::to_string(&req) {
                let sent = t
                    .endpoint()
                    .send_channel_data("cmd", false, json.as_bytes());
                if !sent {
                    ui.append_terminal_output("[错误] cmd 通道未就绪，命令未送达".to_string());
                }
            }
        }
        // #72/#271 文件/剪贴板命令（UI 工具栏）：发送文件/剪贴板文本/图片、取消。
        while let Ok(cmd) = file_cmd_rx.try_recv() {
            match cmd {
                crate::FileCmd::SendFile(path) => match file_transfer.send_file(&path) {
                    Ok(()) => {
                        let msg = format!("开始发送文件：{}", path.display());
                        ui.session_status(msg);
                    }
                    Err(e) => {
                        let msg = format!("发送失败：{e}");
                        ui.session_status(msg);
                    }
                },
                crate::FileCmd::SendClipboard(text) => {
                    aerodesk_core::clipboard::set_cache(text.clone());
                    let sent = file_transfer.send_clipboard(&text, t.endpoint());
                    let msg = if sent {
                        "已发送剪贴板到被控端".to_string()
                    } else {
                        "剪贴板：file 通道未就绪".to_string()
                    };
                    ui.session_status(msg);
                }
                crate::FileCmd::SendClipboardImage(png) => {
                    match file_transfer.send_clipboard_image(png) {
                        Ok(()) => {
                            let msg = "已发送剪贴板图片到被控端".to_string();
                            ui.session_status(msg);
                        }
                        Err(e) => {
                            let msg = format!("剪贴板图片发送失败：{e}");
                            ui.session_status(msg);
                        }
                    }
                }
                crate::FileCmd::Cancel => {
                    file_transfer.cancel_send(t.endpoint());
                }
            }
        }
        // #458 聊天消息：复用 cmd 通道（CmdAction::Chat），避免新增 data channel
        // 破坏 str0m 媒体协商（新增第 7 个 channel 会导致 RTP 0 帧）。
        while let Ok(cmd) = chat_cmd_rx.try_recv() {
            match cmd {
                crate::ChatCmd::Send(text) => {
                    let text = text.trim().to_string();
                    if text.is_empty() {
                        continue;
                    }
                    let req = CmdRequest::new(
                        crate::system_time_millis(),
                        CmdAction::Chat {
                            text,
                            sender: "我".to_string(),
                            timestamp_ms: crate::system_time_millis(),
                        },
                    );
                    if let Ok(json) = serde_json::to_string(&req) {
                        let sent = t
                            .endpoint()
                            .send_channel_data("cmd", false, json.as_bytes());
                        if !sent {
                            ui.set_message_window_status("发送失败：cmd 通道未就绪".to_string());
                        }
                    }
                }
            }
        }
        t.pump();
        while let Some(ev) = t.poll_event() {
            // #72 文件通道事件交给状态机（非 file 事件为 no-op）。
            file_transfer.handle_event(&ev, t.endpoint());
            // #109/#452 终端命令响应 → 终端独立窗口回显。
            if let ClientEvent::ChannelData(cid, _, data) = &ev
                && t.endpoint().channel_label(*cid).as_deref() == Some("cmd")
                && let Ok(response) = serde_json::from_slice::<CmdResponse>(data)
            {
                if let CmdResult::Chat { sender, text } = &response.result {
                    ui.append_chat_message(sender.clone(), text.clone(), false);
                } else {
                    ui.append_terminal_output(format_cmd_response(&response));
                }
            }
            // #75 远程光标：被控端经 cursor 通道广播位置 → UI 叠加（与 macOS UI 一致）；
            // sent_ms 墙钟 → 端到端单向延时（#8，节流推送交给下方统计 ticker）。
            if let ClientEvent::ChannelData(cid, _, data) = &ev
                && t.endpoint().channel_label(*cid).as_deref() == Some("cursor")
                && let Some(pos) = cursor_pos(data)
            {
                ui.set_remote_cursor(pos.x as f32, pos.y as f32);
                if pos.sent_ms > 0 {
                    last_e2e_ms = Some(crate::system_time_millis().saturating_sub(pos.sent_ms));
                }
            }
            if let ClientEvent::Media(data) = ev {
                media_evts += 1;
                // #73 音频识别：用协商 codec（PCMU/Opus）区分，不按 mid（SFU 转发
                // 用本地 mid）。当前播放 PCMU（默认音频）；Opus 需 aerodesk-codec 非
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
                        opus_decoder = aerodesk_codec::audio::OpusDecoder::new().ok();
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
                    let _ = t.endpoint().request_keyframe(
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
                                ui.cleanup(Some(format!("解码器初始化失败：{e}")));
                                return;
                            }
                        }
                    }
                    if renderer.is_none() {
                        renderer = Some(mk_renderer());
                    }
                    let unit = aerodesk_core::platform::EncodedUnit {
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
            ui.session_status(room_msg.clone());
            ui.set_status(room_msg);
        }
        if frames > 0 && !session_ui_joined {
            session_ui_joined = true;
            ui.joined();
            ui.session_status("会话中 · 媒体流已接通".to_string());
        }
        if frames > 0 && no_media_notified {
            no_media_notified = false;
        }
        // 会话延时统计推送（500ms 节流；fps 为窗口内解码帧率）。
        if last_stats_push.elapsed() >= Duration::from_millis(500) {
            let dt = last_stats_push.elapsed().as_secs_f64().max(0.001);
            let fps = (frames - last_stats_frames) as f32 / dt as f32;
            last_stats_frames = frames;
            last_stats_push = Instant::now();
            let rtt_ms = t.endpoint().last_rtt().map(|d| d.as_millis() as u64);
            tracing::debug!(
                "session stats: e2e={:?}ms rtt={:?}ms fps={fps:.1}",
                last_e2e_ms,
                rtt_ms
            );
            ui.set_session_stats(last_e2e_ms, rtt_ms, fps);
        }
        // #72 文件传输推进 + 剪贴板接收落地（文本/图片写入系统剪贴板）。
        file_transfer.tick(t.endpoint());
        if let Some(text) = file_transfer.take_incoming_clipboard() {
            aerodesk_core::clipboard::set_cache(text.clone());
            aerodesk_core::clipboard::write(&text);
            ui.session_status("已应用远端剪贴板文本".to_string());
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
            ui.session_status(msg);
        }
        // #452 文件传输进度：500ms 节流同步到会话状态和独立文件窗口。
        if last_file_status.elapsed() >= Duration::from_millis(500) {
            last_file_status = Instant::now();
            let st = file_transfer.status();
            if let Some(msg) = st.message {
                ui.session_status(msg.clone());
                ui.clear_file_window_progress(Some(msg));
            } else if let Some((name, done, total)) = st.sending {
                let pct = done as f64 * 100.0 / total.max(1) as f64;
                let label = format!("发送 {name} {pct:.0}%");
                ui.session_status(format!("发送文件：{name} {done}/{total} ({pct:.0}%)"));
                ui.update_file_window_progress(
                    (pct / 100.0) as f32,
                    label,
                    format!("正在发送：{name}"),
                );
            } else if let Some((name, done, total)) = st.receiving {
                let pct = done as f64 * 100.0 / total.max(1) as f64;
                let label = format!("接收 {name} {pct:.0}%");
                ui.session_status(format!("接收文件：{name} {done}/{total} ({pct:.0}%)"));
                ui.update_file_window_progress(
                    (pct / 100.0) as f32,
                    label,
                    format!("正在接收：{name}"),
                );
            } else {
                ui.clear_file_window_progress(None);
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
                    if file_transfer.send_clipboard(&text, t.endpoint()) {
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
    ui.set_status(format!("已断开：{room}"));
    ui.cleanup(None);
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
        let pos = super::cursor_pos(json).unwrap();
        assert_eq!((pos.x, pos.y), (0.5, 0.25));
        assert_eq!(pos.sent_ms, 123);
        // 旧端无 sent_ms（serde default）也能解析。
        let old = br#"{"x":0.1,"y":0.9}"#;
        let pos = super::cursor_pos(old).unwrap();
        assert_eq!((pos.x, pos.y), (0.1, 0.9));
        assert_eq!(pos.sent_ms, 0);
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
                code: None,
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
                code: Some("blocked_by_policy".into()),
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
