//! Windows 被控端授权（#417）：Windows 无 macOS TCC 式系统权限弹窗。
//!
//! DXGI 屏幕采集与 SendInput 注入在交互会话下天然可用，授权语义 = 用户显式
//! 开启「开启被控（允许被远程接入）」开关；本实现把权限状态如实上报 UI
//! （有活动显示器 = 交互会话 = 已授权），避免授权卡片显示"平台未实现"。

use aerodesk_core::platform::Permissions;

/// Windows 被控端权限（有活动显示器即视为已授权）。
#[derive(Debug, Clone, Copy, Default)]
pub struct WindowsPermissions;

/// 是否有活动显示器（交互桌面会话；服务/无桌面会话 GetSystemMetrics 返回 0）。
fn has_active_display() -> bool {
    use windows::Win32::UI::WindowsAndMessaging::{GetSystemMetrics, SM_CMONITORS};
    unsafe { GetSystemMetrics(SM_CMONITORS) > 0 }
}

impl Permissions for WindowsPermissions {
    fn screen_capture_authorized(&self) -> bool {
        has_active_display()
    }

    fn accessibility_authorized(&self) -> bool {
        has_active_display()
    }

    fn request_screen_capture(&self) -> bool {
        // 无系统弹窗：授权即「开启被控」开关，直接放行（由开关状态控制）。
        true
    }

    fn open_screen_capture_settings(&self) {}

    fn open_accessibility_settings(&self) {}

    fn trigger_screen_capture_registration(&self) {}
}
