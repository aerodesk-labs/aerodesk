//! 非 macOS 桌面端（Windows/Linux）主控端：真实媒体观看。
//!
//! #277：解码/渲染统一走 core `Decoder`/`Renderer` trait（泛型管线
//! `generic_viewer::run_viewer_generic`），本模块只负责组装
//! SoftDecoder（OpenH264 软解）+ SlintRenderer。

use std::sync::Arc;
use std::sync::atomic::AtomicBool;

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
    let ui2 = ui_weak.clone();
    crate::generic_viewer::run_viewer_generic(
        server,
        room,
        token,
        ui_weak,
        session_idx,
        input_rx,
        stop,
        "OpenH264 软解",
        || aerodesk_softenc::decode::SoftDecoder::new(),
        move || crate::SlintRenderer::new(ui2.clone(), session_idx),
    );
}
