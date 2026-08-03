//! macOS 观看端：真实 H.264 解码渲染（#29）。
//!
//! 连接 `LiveSession` → `AccessUnitAssembler` 聚合成完整访问单元 →
//! VideoToolbox 硬解 → CVPixelBuffer → RGBA → Slint `Image`。
//! 替换演示帧源；其余平台仍走演示帧（等各自解码管线接入）。

use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::{Duration, Instant};

use aerodesk_core::access_unit::AccessUnitAssembler;
use aerodesk_core::connect::connect_live_role;
use aerodesk_core::endpoint::ClientEvent;
use aerodesk_macos::decode::{H264Decoder, to_rgba};
use aerodesk_protocol::signal::Role;
use slint::{Image, Model, Rgba8Pixel, SharedPixelBuffer};
use str0m::net::Protocol;

use crate::AppWindow;

/// 运行 macOS 观看会话（阻塞直到断开/代际失效）。
pub fn run_viewer(
    server: String,
    room: String,
    token: Option<String>,
    ui_weak: slint::Weak<AppWindow>,
    epoch: Arc<AtomicU64>,
    my_epoch: u64,
    control_rx: std::sync::mpsc::Receiver<String>,
    session_idx: usize,
) {
    let stale = || epoch.load(Ordering::SeqCst) != my_epoch;
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
            return;
        }
    };
    if stale() {
        return;
    }
    let Some(ui) = ui_weak.upgrade() else { return };
    ui.set_status(format!("已连接：peer={} ice={}", live.peer_id, live.ice_connected).into());
    ui.set_log(
        format!(
            "房间: {room}\n服务器: {server}\nSDP 交换: OK\nICE: {}\n\n真实 H.264 解码渲染（VideoToolbox）。",
            if live.ice_connected { "connected" } else { "pending(5s 超时)" }
        )
        .into(),
    );
    crate::add_recent(&ui, &room, &server);
    ui.set_conn_state(2);
    ui.set_in_session(true);
    ui.set_session_status("会话中 · 真实 H.264 解码（VideoToolbox）".into());

    // #29 多会话标签：登记会话房间与帧槽。
    {
        let mut tabs: Vec<slint::SharedString> = (0..ui.get_session_tabs().row_count())
            .filter_map(|i| ui.get_session_tabs().row_data(i))
            .collect();
        if !tabs.iter().any(|t| t.as_str() == room) {
            tabs.push(room.clone().into());
        }
        ui.set_session_tabs(slint::ModelRc::new(slint::VecModel::from(tabs)));
        let mut frames: Vec<slint::Image> = (0..ui.get_session_frames().row_count())
            .filter_map(|i| ui.get_session_frames().row_data(i))
            .collect();
        if frames.len() <= session_idx {
            frames.resize(session_idx + 1, slint::Image::default());
        }
        ui.set_session_frames(slint::ModelRc::new(slint::VecModel::from(frames)));
        ui.set_active_session(session_idx as i32);
    }

    let mut assembler = AccessUnitAssembler::new();
    let mut decoder = H264Decoder::new();
    let mut frames: u64 = 0;
    let mut last_stat = Instant::now();

    while !stale() {
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
        // #29：UI 选层请求（画质/显示器按钮）→ control 通道 → SFU。
        while let Ok(req) = control_rx.try_recv() {
            let _ = live
                .endpoint
                .send_channel_data("control", false, req.as_bytes());
        }
        while let Some(ev) = live.endpoint.poll_event() {
            match ev {
                ClientEvent::Media(data) => {
                    if let Some(mid) = live.video_mid
                        && data.mid == mid
                        && let Some(au) = assembler.push(
                            data.data.as_ref(),
                            data.time.as_micros(),
                            data.is_keyframe(),
                        )
                    {
                        if let Ok(Some(pixbuf)) = decoder.decode_annexb(&au.data, au.pts_us as i64)
                            && let Some((rgba, w, h)) = to_rgba(&pixbuf)
                        {
                            let buffer = SharedPixelBuffer::<Rgba8Pixel>::clone_from_slice(
                                &rgba, w as u32, h as u32,
                            );
                            let img = Image::from_rgba8(buffer);
                            if let Some(fui) = ui_weak.upgrade() {
                                // 更新本会话帧槽；当前标签同时更新显示帧。
                                let mut arr: Vec<slint::Image> =
                                    (0..fui.get_session_frames().row_count())
                                        .filter_map(|i| fui.get_session_frames().row_data(i))
                                        .collect();
                                if arr.len() <= session_idx {
                                    arr.resize(session_idx + 1, slint::Image::default());
                                }
                                arr[session_idx] = img.clone();
                                fui.set_session_frames(slint::ModelRc::new(slint::VecModel::from(
                                    arr,
                                )));
                                if fui.get_active_session() == session_idx as i32 {
                                    fui.set_video_frame(img);
                                }
                            }
                            frames += 1;
                        }
                    }
                }
                ClientEvent::Closed => {
                    if let Some(fui) = ui_weak.upgrade() {
                        fui.set_in_session(false);
                        fui.set_status("会话结束（连接关闭）".into());
                    }
                    return;
                }
                _ => {}
            }
        }
        if last_stat.elapsed() >= Duration::from_secs(2) {
            if let Some(fui) = ui_weak.upgrade() {
                fui.set_session_status(format!("会话中 · 真实 H.264 解码 · {frames} 帧/2s").into());
            }
            frames = 0;
            last_stat = Instant::now();
        }
        std::thread::sleep(Duration::from_millis(1));
    }
}
