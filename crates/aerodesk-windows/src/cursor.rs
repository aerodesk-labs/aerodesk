//! 被控端光标读取（#75）：GetCursorPos → 归一化 0..1（供观看端叠加层）。
//!
//! 归一化基准：活动显示器在虚拟屏幕中的区域（与 [`inject::SendInputInjector`]
//! 的注入坐标映射同口径，多显示器下 viewer 坐标与注入坐标一致）；未指定活动
//! 显示器时用整个虚拟屏幕。非交互会话（无桌面）时 GetCursorPos 失败返回 None，
//! 上层（CLI publisher）回退合成轨迹，cursor 通道保持常活。

use aerodesk_core::platform::CursorSource;

/// 归一化：光标绝对坐标 → 0..1（越界收敛到边界；基准矩形无效返回 None）。
fn normalize(x: i32, y: i32, rect: (i32, i32, u32, u32)) -> Option<(f64, f64)> {
    let (rx, ry, rw, rh) = rect;
    let (w, h) = (rw as f64, rh as f64);
    if w <= 0.0 || h <= 0.0 {
        return None;
    }
    let nx = ((x - rx) as f64 / w).clamp(0.0, 1.0);
    let ny = ((y - ry) as f64 / h).clamp(0.0, 1.0);
    Some((nx, ny))
}

/// 当前光标位置（归一化 0..1，相对 `display_rect` 指定的活动显示器区域）。
#[cfg(windows)]
pub fn cursor_position_normalized(
    display_rect: Option<(i32, i32, u32, u32)>,
) -> Option<(f64, f64)> {
    use windows::Win32::Foundation::POINT;
    use windows::Win32::UI::WindowsAndMessaging::GetCursorPos;
    let mut pt = POINT { x: 0, y: 0 };
    // 非交互会话（服务/无桌面）GetCursorPos 失败，返回 None 由上层回退。
    unsafe {
        if GetCursorPos(&mut pt).is_err() {
            return None;
        }
    }
    let virtual_rect = crate::inject::virtual_screen();
    normalize(pt.x, pt.y, display_rect.unwrap_or(virtual_rect))
}

/// Windows 光标源（GetCursorPos，活动显示器区域归一化）。
#[cfg(windows)]
pub struct WindowsCursor {
    /// 活动显示器在虚拟屏幕中的区域（像素 x,y,w,h）；None = 整个虚拟屏幕。
    display_rect: Option<(i32, i32, u32, u32)>,
}

#[cfg(windows)]
impl WindowsCursor {
    pub fn new(display_rect: Option<(i32, i32, u32, u32)>) -> Self {
        Self { display_rect }
    }
}

#[cfg(windows)]
impl Default for WindowsCursor {
    fn default() -> Self {
        Self::new(None)
    }
}

#[cfg(windows)]
impl CursorSource for WindowsCursor {
    fn position_normalized(&mut self) -> Option<(f64, f64)> {
        cursor_position_normalized(self.display_rect)
    }

    /// 切换显示器后同步坐标基准区域（#58 运行中切换；None 回退整个虚拟屏幕）。
    fn set_active_display(&mut self, rect: Option<(i32, i32, u32, u32)>) {
        self.display_rect = rect;
    }
}

/// 非 Windows 主机上的编译期骨架（保证 workspace 全平台可编译）。
#[cfg(not(windows))]
pub struct WindowsCursor;

#[cfg(not(windows))]
impl CursorSource for WindowsCursor {
    fn position_normalized(&mut self) -> Option<(f64, f64)> {
        None
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn normalize_center() {
        assert_eq!(normalize(960, 540, (0, 0, 1920, 1080)), Some((0.5, 0.5)));
    }

    #[test]
    fn normalize_corners_and_clamp() {
        assert_eq!(normalize(0, 0, (0, 0, 1920, 1080)), Some((0.0, 0.0)));
        // 越界坐标（多显示器负区/超界）收敛到 0..1。
        assert_eq!(normalize(-100, 2000, (0, 0, 1920, 1080)), Some((0.0, 1.0)));
    }

    #[test]
    fn normalize_uses_active_display_rect() {
        // 活动显示器位于主屏右侧：1920x1080 主屏 + 右侧 1920x1080 副屏。
        let right = (1920, 0, 1920, 1080);
        assert_eq!(normalize(1920, 0, right), Some((0.0, 0.0)));
        assert_eq!(normalize(3840, 1080, right), Some((1.0, 1.0)));
        assert_eq!(normalize(2880, 540, right), Some((0.5, 0.5)));
        // 光标在主屏（虚拟屏幕左半区）时收敛到活动显示器左边界。
        assert_eq!(normalize(0, 540, right), Some((0.0, 0.5)));
    }

    #[test]
    fn normalize_zero_size_returns_none() {
        assert_eq!(normalize(10, 10, (0, 0, 0, 1080)), None);
        assert_eq!(normalize(10, 10, (0, 0, 1920, 0)), None);
    }
}

#[cfg(all(test, windows))]
mod windows_tests {
    use super::*;

    #[test]
    fn reads_current_position() {
        // 非交互会话可能失败；只验证返回值在归一化范围。
        if let Some((x, y)) = cursor_position_normalized(None) {
            assert!((0.0..=1.0).contains(&x));
            assert!((0.0..=1.0).contains(&y));
        }
    }
}
