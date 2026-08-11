//! 系统通知（macOS osascript display notification；无额外依赖）。

use std::process::Command;

/// 核心 `Notifier` trait 实现（macOS）。
pub struct MacNotifier;

impl aerodesk_core::platform::Notifier for MacNotifier {
    fn notify(&self, title: &str, body: &str) {
        let script = format!(
            "display notification \"{}\" with title \"{}\"",
            body.replace('"', "\\\""),
            title.replace('"', "\\\"")
        );
        let _ = Command::new("osascript").args(["-e", &script]).output();
    }
}
