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

/// 运行 macOS 观看会话（阻塞直到断开/代际失效）。
pub fn run_viewer(
    server: String,
    room: String,
    token: Option<String>,
    ui_weak: slint::Weak<AppWindow>,
    epoch: Arc<AtomicU64>,
    my_epoch: u64,
    control_rx: std::sync::mpsc::Receiver<String>,
    input_rx: std::sync::mpsc::Receiver<String>,
    file_cmd_rx: std::sync::mpsc::Receiver<FileCmd>,
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
    // #72 文件传输 + 剪贴板（接收落盘到 ~/Downloads/AeroDesk）。
    let mut file_transfer =
        aerodesk_core::file_transfer::FileTransfer::new(Some(default_recv_dir()));
    let mut last_file_status = Instant::now();

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
                                // #75：视频源尺寸（letterbox 光标换算用）。
                                fui.set_frame_w(w as f32);
                                fui.set_frame_h(h as f32);
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
                        fui.set_in_session(false);
                        fui.set_status("会话结束（连接关闭）".into());
                    }
                    return;
                }
                _ => {}
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
                fui.set_session_status(format!("会话中 · 真实 H.264 解码 · {frames} 帧/2s").into());
            }
            frames = 0;
            last_stat = Instant::now();
        }
        std::thread::sleep(Duration::from_millis(1));
    }
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
