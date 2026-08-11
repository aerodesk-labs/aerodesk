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
use aerodesk_protocol::signal::Role;
use str0m::net::Protocol;

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
    stop: Arc<AtomicBool>,
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
                "房间: {room2}\n服务器: {server2}\nSDP 交换: OK\nICE: {}\n\n{decoder_label}渲染。",
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
        ui.set_in_session(true);
        ui.set_session_status(format!("会话中 · {decoder_label}").into());
    });
    // #29 多会话：登记标签并切到当前会话。
    crate::session_joined_weak(&ui_weak, session_idx);

    let mut assembler = AccessUnitAssembler::new();
    let mut decoder: Option<D> = None;
    let mut renderer: Option<R> = None;
    let mut frames: u64 = 0;
    let mut pkts: u64 = 0;
    let mut media_evts: u64 = 0;
    let mut last_stat = Instant::now();
    // #136 关键帧请求：首包/不连续/切层时向 SFU 发 PLI（节流 1s）。
    let mut last_kf_request: Option<std::time::Instant> = None;
    let mut last_kf_rid: Option<str0m::media::Rid> = None;
    let mut seen_video = false;
    while !stale() {
        // 输入事件：UI 键鼠 → input data channel → SFU → 被控端。
        while let Ok(json) = input_rx.try_recv() {
            live.endpoint
                .send_channel_data("input", false, json.as_bytes());
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
            if let ClientEvent::Media(data) = ev {
                media_evts += 1;
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
                                eprintln!("decoder init failed: {e}");
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
        std::thread::sleep(Duration::from_millis(1));
    }
    // 会话结束（断开置 stop）：提示后清理注册表与 UI 槽位。
    let msg = format!("已断开：{room}");
    with_ui(&ui_weak, move |ui| ui.set_status(msg.into()));
    crate::session_cleanup_weak(&ui_weak, session_idx, None);
}
