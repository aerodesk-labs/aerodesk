//! iOS/iPad Slint UI 宿主（统一 Slint 的端侧入口）。
//!
//! 平台实现位于 [`aerodesk_platform::ios`]；本文件负责启动 Slint UI，
//! 并把“连接/断开”操作桥接到 `ViewerSession`。Swift 壳层负责生命周期与
//! 系统权限，UI 由本模块渲染。

use std::sync::{Mutex, MutexGuard};

use aerodesk_platform::ios::viewer::ViewerSession;
use slint::ComponentHandle;

/// 当前观看会话。连接在后台线程执行，断开/重连在锁内串行切换。
static ACTIVE_VIEWER: Mutex<Option<ViewerSession>> = Mutex::new(None);

slint::include_modules!();

fn reset_viewer() {
    let mut guard: MutexGuard<'_, Option<ViewerSession>> =
        ACTIVE_VIEWER.lock().unwrap_or_else(|e| e.into_inner());
    *guard = None;
}

fn set_viewer(viewer: ViewerSession) {
    let mut guard = ACTIVE_VIEWER.lock().unwrap_or_else(|e| e.into_inner());
    *guard = Some(viewer);
}

/// 供 Swift 调用的 Slint 启动入口（阻塞运行事件循环）。
#[unsafe(no_mangle)]
pub extern "C" fn ad_slint_run() {
    let window = MobileAppWindow::new().expect("failed to create Slint window");

    let weak = window.as_weak();
    window.on_connect(
        move |server: slint::SharedString, room: slint::SharedString| {
            reset_viewer();
            let weak = weak.clone();
            let server = server.to_string();
            let room = room.to_string();
            std::thread::spawn(move || {
                let status = match ViewerSession::connect(&server, &room) {
                    Ok(viewer) => {
                        set_viewer(viewer);
                        "已连接".to_string()
                    }
                    Err(err) => format!("连接失败: {err}"),
                };
                let _ = weak.upgrade_in_event_loop(move |ui| {
                    ui.set_status(status.into());
                });
            });
        },
    );

    let weak = window.as_weak();
    window.on_disconnect(move || {
        reset_viewer();
        if let Some(ui) = weak.upgrade() {
            ui.set_status("已断开".into());
        }
    });

    let _ = window.run();
}
