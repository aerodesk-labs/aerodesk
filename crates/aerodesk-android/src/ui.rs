//! Android Slint UI 宿主（统一 Slint 的端侧入口）。
//!
//! 平台实现位于 [`aerodesk_platform::android`]；本文件负责启动 Slint UI，
//! 并把“连接/断开”操作桥接到 `ViewerSession`。Kotlin 壳层只保留系统垫片
//! （权限、前台服务、MediaCodec 采集/解码）。

use std::sync::{Mutex, MutexGuard};

use aerodesk_platform::android::viewer::ViewerSession;
use slint::ComponentHandle;

/// 当前观看会话。Slint 回调运行在 UI 线程，连接/断开都在这里串行切换。
/// `ViewerSession::connect` 内部会阻塞并启动后台收流线程，因此 UI 回调
/// 只负责把状态和句柄保存起来，真正的连接操作由 Rust 端直接完成。
static ACTIVE_VIEWER: Mutex<Option<ViewerSession>> = Mutex::new(None);

slint::slint! {
    import { Button } from "std-widgets.slint";

    export component AndroidAppWindow inherits Window {
        title: "AeroDesk";
        in-out property <string> server: "ws://127.0.0.1:3003";
        in-out property <string> room: "demo";
        in-out property <string> status: "未连接";
        callback connect(string, string);
        callback disconnect();

        VerticalLayout {
            padding: 20px;
            spacing: 12px;

            Text {
                text: "AeroDesk Android (Slint)";
                font-size: 20px;
                horizontal-alignment: center;
            }

            TextInput {
                text <=> root.server;
                single-line: true;
            }

            TextInput {
                text <=> root.room;
                single-line: true;
            }

            HorizontalLayout {
                spacing: 10px;

                Button {
                    text: "连接";
                    clicked => { root.connect(root.server, root.room); }
                }

                Button {
                    text: "断开";
                    clicked => { root.disconnect(); }
                }
            }

            Text {
                text: root.status;
                wrap: word-wrap;
            }
        }
    }
}

/// 取走并销毁旧会话，避免重复连接时后台线程泄漏。
fn reset_viewer() {
    let mut guard: MutexGuard<'_, Option<ViewerSession>> =
        ACTIVE_VIEWER.lock().unwrap_or_else(|e| e.into_inner());
    *guard = None;
}

/// 设置当前观看会话。
fn set_viewer(viewer: ViewerSession) {
    let mut guard = ACTIVE_VIEWER.lock().unwrap_or_else(|e| e.into_inner());
    *guard = Some(viewer);
}

/// NativeActivity 的 Rust 入口：初始化 Slint Android 后端并运行事件循环。
///
/// 由 `android.app.NativeActivity`（或派生类）在加载
/// `libaerodesk_android.so` 后调用，对应 `ANativeActivity_onCreate` 入口。
#[unsafe(no_mangle)]
pub extern "C" fn android_main(app: slint::android::AndroidApp) {
    slint::android::init(app).expect("failed to initialize Slint Android backend");
    let window = AndroidAppWindow::new().expect("failed to create Slint window");

    let weak = window.as_weak();
    window.on_connect(
        move |server: slint::SharedString, room: slint::SharedString| {
            reset_viewer();
            let weak = weak.clone();
            let server = server.to_string();
            let room = room.to_string();
            std::thread::spawn(move || {
                let status = match ViewerSession::connect(&server, &room, false) {
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
