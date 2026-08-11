//! 被控端光标读取（#75）：CGEvent 空事件携带当前全局鼠标位置。

use core_graphics::event::CGEvent;
use core_graphics::event_source::{CGEventSource, CGEventSourceStateID};

/// 当前光标位置（归一化 0..1，相对主显示器）。
pub fn cursor_position_normalized() -> Option<(f64, f64)> {
    let source = CGEventSource::new(CGEventSourceStateID::HIDSystemState).ok()?;
    let ev = CGEvent::new(source).ok()?;
    let loc = ev.location();
    let (sw, sh) = crate::inject::screen_size_points().ok()?;
    if sw <= 0.0 || sh <= 0.0 {
        return None;
    }
    Some((loc.x / sw, loc.y / sh))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn reads_current_position() {
        // 无屏幕录制权限也能读到全局鼠标位置；只验证返回归一化范围。
        if let Some((x, y)) = cursor_position_normalized() {
            assert!((0.0..=1.0).contains(&x));
            assert!((0.0..=1.0).contains(&y));
        }
    }
}

/// 核心 `CursorSource` 实现（被控端真实光标位置，归一化 0..1）。
pub struct MacCursor;

impl aerodesk_core::platform::CursorSource for MacCursor {
    fn position_normalized(&mut self) -> Option<(f64, f64)> {
        cursor_position_normalized()
    }
}
