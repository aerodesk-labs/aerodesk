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
