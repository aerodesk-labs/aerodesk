//! 被控端光标读取（#75）：CGEvent 空事件携带当前全局鼠标位置。

use core_graphics::display::CGDisplay;
use core_graphics::event::CGEvent;
use core_graphics::event_source::{CGEventSource, CGEventSourceStateID};

/// 当前光标位置（归一化 0..1，相对**活动/主显示器**）。
///
/// CGEvent 位置是全局显示坐标（原点=主屏左上角，多屏布局下副屏有正/负偏移），
/// 必须先减去目标显示器 bounds 原点再按其尺寸归一化——直接全局/主屏尺寸
/// 相除在副屏场景会得到 >1 或负值（远端光标叠加越界）。
/// 光标当前不在该显示器上时返回 None（由调用方保持/隐藏上次位置）。
pub fn cursor_position_normalized() -> Option<(f64, f64)> {
    let source = CGEventSource::new(CGEventSourceStateID::HIDSystemState).ok()?;
    let ev = CGEvent::new(source).ok()?;
    let loc = ev.location();
    let bounds = match crate::inject::active_display() {
        Some(id) => CGDisplay::new(id).bounds(),
        None => CGDisplay::main().bounds(),
    };
    let (sw, sh) = (bounds.size.width, bounds.size.height);
    if sw <= 0.0 || sh <= 0.0 {
        return None;
    }
    let (lx, ly) = (loc.x - bounds.origin.x, loc.y - bounds.origin.y);
    if !(0.0..=sw).contains(&lx) || !(0.0..=sh).contains(&ly) {
        return None;
    }
    Some((lx / sw, ly / sh))
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
