//! 非 macOS 桌面端（Windows/Linux）主控端：真实媒体观看（H.264 OpenH264 软解）。
//!
//! 与 macOS 分支共用 aerodesk-core 网络链路 + AccessUnitAssembler；解码用
//! aerodesk-softenc::SoftDecoder（OpenH264 全平台），渲染走 Slint set_video_frame。
//! 音频/文件传输/多会话标签为 macOS 增强项，非 macOS 先收敛到"真实视频观看"。

use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::{Duration, Instant};

use aerodesk_core::access_unit::AccessUnitAssembler;
use aerodesk_core::connect::connect_live_role;
use aerodesk_core::endpoint::ClientEvent;
use aerodesk_protocol::signal::Role;
use aerodesk_softenc::decode::SoftDecoder;
use str0m::net::Protocol;

/// 启动非 macOS 主控端观看：连接 → 收流 → 组装 → OpenH264 软解 → Slint 渲染。
pub fn run_generic_viewer(
    server: String,
    room: String,
    token: Option<String>,
    ui_weak: slint::Weak<crate::AppWindow>,
    session_idx: usize,
    input_rx: std::sync::mpsc::Receiver<String>,
    stop: Arc<AtomicBool>,
) {
    eprintln!("generic viewer: start server={server} room={room}");
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
            eprintln!("generic viewer connect failed: {e}");
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
            eprintln!("generic viewer connect TIMEOUT (20s)");
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
                "房间: {room2}\n服务器: {server2}\nSDP 交换: OK\nICE: {}\n\nOpenH264 软解渲染（Windows/Linux）。",
                if ice { "connected" } else { "pending(5s 超时)" }
            )
            .into(),
        );
        crate::add_recent(ui, &room2, &server2);
        ui.set_conn_state(2);
        ui.set_in_session(true);
        ui.set_session_status("会话中 · OpenH264 软解".into());
    });
    // #29 多会话：登记标签并切到当前会话。
    crate::session_joined_weak(&ui_weak, session_idx);
    eprintln!(
        "generic viewer connected peer={} ice={}",
        live.peer_id, live.ice_connected
    );

    let mut assembler = AccessUnitAssembler::new();
    let mut decoder: Option<SoftDecoder> = None;
    let mut frames: u64 = 0;
    let mut pkts: u64 = 0;
    let mut media_evts: u64 = 0;
    let mut last_stat = Instant::now();
    // #136 关键帧请求：首包/不连续/切层时向 SFU 发 PLI（节流 1s）。
    let mut last_kf_request: Option<std::time::Instant> = None;
    let mut last_kf_rid: Option<str0m::media::Rid> = None;
    let mut seen_video = false;
    while !stale() {
        // 输入事件：UI 键鼠 → input data channel → SFU → 被控端（与 macOS/Android 同款）。
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
                        match SoftDecoder::new() {
                            Ok(d) => decoder = Some(d),
                            Err(e) => {
                                eprintln!("OpenH264 init failed: {e}");
                                return;
                            }
                        }
                    }
                    let d = decoder.as_mut().unwrap();
                    if let Ok(Some((rgba, w, h))) = d.decode_rgba(&au.data) {
                        frames += 1;
                        // 多会话：写入本会话帧槽 + 活动会话显示帧。
                        crate::present_frame(&ui_weak, &rgba, w, h, session_idx);
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
