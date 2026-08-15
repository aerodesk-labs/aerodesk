//! 系统通知（macOS osascript display notification；无额外依赖）。

use std::process::Command;

/// 核心 `Notifier` trait 实现（macOS）。
pub struct MacNotifier;

impl aerodesk_core::platform::Notifier for MacNotifier {
    fn notify(&self, title: &str, body: &str) {
        // 先转义反斜杠再转义引号：只转义引号时，`\` + `"` 相邻会让 AppleScript
        // 把 `\\` 当转义反斜杠、其后的 `"` 变成字符串结束符（内容均为内部受控串，
        // 仍按规范转义）。
        let esc = |s: &str| s.replace('\\', "\\\\").replace('"', "\\\"");
        let script = format!(
            "display notification \"{}\" with title \"{}\"",
            esc(body),
            esc(title)
        );
        let _ = Command::new("osascript").args(["-e", &script]).output();
    }
}
