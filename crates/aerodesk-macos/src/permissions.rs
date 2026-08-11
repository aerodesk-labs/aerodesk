//! macOS 被控端权限（#29 授权流程 UI）。
//!
//! 屏幕录制 / 辅助功能状态检查与系统设置引导。

use core_graphics::access::ScreenCaptureAccess;

/// 屏幕录制权限是否已授权。
pub fn screen_capture_authorized() -> bool {
    ScreenCaptureAccess.preflight()
}

/// 辅助功能权限是否已授权（AXIsProcessTrusted）。
pub fn accessibility_authorized() -> bool {
    #[link(name = "ApplicationServices", kind = "framework")]
    unsafe extern "C" {
        fn AXIsProcessTrusted() -> bool;
    }
    unsafe { AXIsProcessTrusted() }
}

/// 系统设置面板。
#[derive(Debug, Clone, Copy)]
pub enum SettingsPane {
    ScreenCapture,
    Accessibility,
}

/// 打开对应系统设置面板。
pub fn open_system_settings(pane: SettingsPane) {
    let url = match pane {
        SettingsPane::ScreenCapture => {
            "x-apple.systempreferences:com.apple.preference.security?Privacy_ScreenCapture"
        }
        SettingsPane::Accessibility => {
            "x-apple.systempreferences:com.apple.preference.security?Privacy_Accessibility"
        }
    };
    let _ = std::process::Command::new("open").arg(url).spawn();
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn permission_checks_are_bool() {
        // 只验证接口可调用（授权状态随系统/运行环境变化，不断言具体值）。
        let _ = screen_capture_authorized();
        let _ = accessibility_authorized();
    }

    #[test]
    fn settings_panes_are_stable() {
        // 编译期枚举稳定性（URL 由 open_system_settings 内部保证）。
        let _ = (SettingsPane::ScreenCapture, SettingsPane::Accessibility);
    }
}

/// 触发一次屏幕采集尝试，让系统把本应用登记进「屏幕录制」授权列表。
/// macOS TCC 只列出尝试过受保护资源的应用；仅 preflight 检查不会登记。
/// 无权限时本次采集会失败（返回 None），但登记动作已完成，用户随后
/// 可在系统设置里勾选本应用。
pub fn trigger_screen_capture_registration() {
    use std::time::Duration;
    if let Ok(mut cap) = crate::capture::ScreenCapture::start(0, 5, 320, 200) {
        let _ = cap.capture_frame(Duration::from_millis(250));
    }
}

/// 主动请求屏幕录制权限（用户显式点击「打开屏幕录制设置」时调用）。
/// `CGRequestScreenCaptureAccess()` 会把本应用登记进授权列表（不在列表时
/// 会打开系统设置窗口），比只做采集尝试更可靠；返回当前授权结果。
pub fn request_screen_capture() -> bool {
    ScreenCaptureAccess.request()
}
