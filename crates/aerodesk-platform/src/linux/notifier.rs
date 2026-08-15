//! 系统通知（Linux `notify-send`；libnotify-bin，无额外依赖）。
//!
//! 桌面环境（GNOME/KDE/…）都有 notify-send；无通知守护进程时静默失败
//! （与 macOS osascript 通知同样的 best-effort 语义）。

use std::process::Command;

/// 核心 `Notifier` trait 实现（Linux）。
pub struct LinuxNotifier;

impl aerodesk_core::platform::Notifier for LinuxNotifier {
    fn notify(&self, title: &str, body: &str) {
        // 参数化传递（不经 shell），标题/正文含引号也安全。
        let _ = Command::new("notify-send")
            .args(["--app-name=AeroDesk", "--urgency=normal", title, body])
            .output();
    }
}
