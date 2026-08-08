//! 非 macOS 桌面端（Windows/Linux）主控端：真实媒体观看（H.264 OpenH264 软解）。
//!
//! 与 macOS 分支共用 aerodesk-core 网络链路 + AccessUnitAssembler；解码用
//! aerodesk-softenc::SoftDecoder（OpenH264 全平台），渲染走 Slint set_video_frame。
//! 音频/文件传输/多会话标签为 macOS 增强项，非 macOS 先收敛到"真实视频观看"。

use std::sync::atomic::{AtomicU64, Ordering};
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
    epoch: Arc<AtomicU64>,
    my_epoch: u64,
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
            "房间: {room}\n服务器: {server}\nSDP 交换: OK\nICE: {}\n\nOpenH264 软解渲染（Windows/Linux）。",
            if live.ice_connected { "connected" } else { "pending(5s 超时)" }
        )
        .into(),
    );
    crate::add_recent(&ui, &room, &server);
    ui.set_conn_state(2);
    ui.set_in_session(true);
    ui.set_session_status("会话中 · OpenH264 软解".into());

    let mut assembler = AccessUnitAssembler::new();
    let mut decoder: Option<SoftDecoder> = None;
    let mut frames: u64 = 0;
    let mut last_stat = Instant::now();
    while !stale() {
        live.socket
            .set_read_timeout(Some(Duration::from_millis(10)))
            .ok();
        let mut buf = [0u8; 4096];
        if let Ok((n, source)) = live.socket.recv_from(&mut buf) {
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
                        if let Some(ui) = ui_weak.upgrade() {
                            let buffer =
                                slint::SharedPixelBuffer::<slint::Rgba8Pixel>::clone_from_slice(
                                    &rgba, w as u32, h as u32,
                                );
                            ui.set_video_frame(slint::Image::from_rgba8(buffer));
                            ui.set_frame_w(w as f32);
                            ui.set_frame_h(h as f32);
                        }
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
    if let Some(ui) = ui_weak.upgrade() {
        ui.set_conn_state(4);
        ui.set_in_session(false);
    }
}
