//! CGEvent input injection (controlled end: inject viewer input into the system).
//!
//! Protocol coordinates are normalized 0..1 relative to the remote screen;
//! this module converts to the actual display size in points (DPI-aware) and
//! posts CGEvents. Wheel and modifier keys are supported (#75).

use aerodesk_core::protocol::input::{ButtonState, InputEvent, Modifiers, MouseButton};
use core_graphics::display::CGDisplay;
use core_graphics::event::{
    CGEvent, CGEventFlags, CGEventTapLocation, CGEventType, CGMouseButton, ScrollEventUnit,
};
use core_graphics::event_source::{CGEventSource, CGEventSourceStateID};
use core_graphics_types::geometry::CGPoint;
use std::sync::atomic::{AtomicI64, Ordering};

/// Convert normalized (0..1) coordinates to display points.
pub fn normalized_to_points(x: f64, y: f64, width: f64, height: f64) -> (f64, f64) {
    (x.clamp(0.0, 1.0) * width, y.clamp(0.0, 1.0) * height)
}

/// 当前活动（被控）显示器 CGDirectDisplayID；-1 = 主显示器（默认）。
/// #75：发布端切换显示器（--display N / control 切换）时必须同步，
/// 否则注入坐标仍按主屏换算，副屏 DPI/分辨率不同时鼠标位置错误。
static ACTIVE_DISPLAY: AtomicI64 = AtomicI64::new(-1);

/// 设置输入注入使用的活动显示器（发布端在采集初始化/切换显示器时调用）。
pub fn set_active_display(display_id: Option<u32>) {
    ACTIVE_DISPLAY.store(display_id.map(|d| d as i64).unwrap_or(-1), Ordering::SeqCst);
}

/// 当前活动显示器 CGDirectDisplayID（无则 None = 主显示器）。
pub fn active_display() -> Option<u32> {
    let id = ACTIVE_DISPLAY.load(Ordering::SeqCst);
    if id >= 0 { Some(id as u32) } else { None }
}

/// Display size in points (CGEvent coordinates are in points, DPI-aware).
/// 有活动显示器时用其 bounds，否则主显示器。
pub fn screen_size_points() -> Result<(f64, f64), String> {
    let bounds = match active_display() {
        Some(id) => CGDisplay::new(id).bounds(),
        None => CGDisplay::main().bounds(),
    };
    Ok((bounds.size.width, bounds.size.height))
}

fn modifier_flags(modifiers: &Modifiers) -> CGEventFlags {
    let mut flags = CGEventFlags::CGEventFlagNull;
    if modifiers.ctrl {
        flags |= CGEventFlags::CGEventFlagControl;
    }
    if modifiers.shift {
        flags |= CGEventFlags::CGEventFlagShift;
    }
    if modifiers.alt {
        flags |= CGEventFlags::CGEventFlagAlternate;
    }
    if modifiers.meta {
        flags |= CGEventFlags::CGEventFlagCommand;
    }
    flags
}

/// Map platform-agnostic key code ("KeyA", "Enter", "ArrowUp", "F1"... ) to CGKeyCode.
pub fn keycode_for_code(code: &str) -> Option<u16> {
    use core_graphics::event::KeyCode;
    match code {
        "KeyA" => Some(KeyCode::ANSI_A),
        "KeyB" => Some(KeyCode::ANSI_B),
        "KeyC" => Some(KeyCode::ANSI_C),
        "KeyD" => Some(KeyCode::ANSI_D),
        "KeyE" => Some(KeyCode::ANSI_E),
        "KeyF" => Some(KeyCode::ANSI_F),
        "KeyG" => Some(KeyCode::ANSI_G),
        "KeyH" => Some(KeyCode::ANSI_H),
        "KeyI" => Some(KeyCode::ANSI_I),
        "KeyJ" => Some(KeyCode::ANSI_J),
        "KeyK" => Some(KeyCode::ANSI_K),
        "KeyL" => Some(KeyCode::ANSI_L),
        "KeyM" => Some(KeyCode::ANSI_M),
        "KeyN" => Some(KeyCode::ANSI_N),
        "KeyO" => Some(KeyCode::ANSI_O),
        "KeyP" => Some(KeyCode::ANSI_P),
        "KeyQ" => Some(KeyCode::ANSI_Q),
        "KeyR" => Some(KeyCode::ANSI_R),
        "KeyS" => Some(KeyCode::ANSI_S),
        "KeyT" => Some(KeyCode::ANSI_T),
        "KeyU" => Some(KeyCode::ANSI_U),
        "KeyV" => Some(KeyCode::ANSI_V),
        "KeyW" => Some(KeyCode::ANSI_W),
        "KeyX" => Some(KeyCode::ANSI_X),
        "KeyY" => Some(KeyCode::ANSI_Y),
        "KeyZ" => Some(KeyCode::ANSI_Z),
        "Digit0" => Some(KeyCode::ANSI_0),
        "Digit1" => Some(KeyCode::ANSI_1),
        "Digit2" => Some(KeyCode::ANSI_2),
        "Digit3" => Some(KeyCode::ANSI_3),
        "Digit4" => Some(KeyCode::ANSI_4),
        "Digit5" => Some(KeyCode::ANSI_5),
        "Digit6" => Some(KeyCode::ANSI_6),
        "Digit7" => Some(KeyCode::ANSI_7),
        "Digit8" => Some(KeyCode::ANSI_8),
        "Digit9" => Some(KeyCode::ANSI_9),
        "Minus" => Some(KeyCode::ANSI_MINUS),
        "Equal" => Some(KeyCode::ANSI_EQUAL),
        "BracketLeft" => Some(KeyCode::ANSI_LEFT_BRACKET),
        "BracketRight" => Some(KeyCode::ANSI_RIGHT_BRACKET),
        "Backslash" => Some(KeyCode::ANSI_BACKSLASH),
        "Semicolon" => Some(KeyCode::ANSI_SEMICOLON),
        "Quote" => Some(KeyCode::ANSI_QUOTE),
        "Backquote" => Some(KeyCode::ANSI_GRAVE),
        "Comma" => Some(KeyCode::ANSI_COMMA),
        "Period" => Some(KeyCode::ANSI_PERIOD),
        "Slash" => Some(KeyCode::ANSI_SLASH),
        "Enter" => Some(KeyCode::RETURN),
        "Tab" => Some(KeyCode::TAB),
        "Space" => Some(KeyCode::SPACE),
        "Backspace" => Some(KeyCode::DELETE),
        "Escape" => Some(KeyCode::ESCAPE),
        "Delete" => Some(KeyCode::FORWARD_DELETE),
        "ArrowUp" => Some(KeyCode::UP_ARROW),
        "ArrowDown" => Some(KeyCode::DOWN_ARROW),
        "ArrowLeft" => Some(KeyCode::LEFT_ARROW),
        "ArrowRight" => Some(KeyCode::RIGHT_ARROW),
        "Home" => Some(KeyCode::HOME),
        "End" => Some(KeyCode::END),
        "PageUp" => Some(KeyCode::PAGE_UP),
        "PageDown" => Some(KeyCode::PAGE_DOWN),
        "ShiftLeft" => Some(KeyCode::SHIFT),
        "ShiftRight" => Some(KeyCode::SHIFT),
        "ControlLeft" => Some(KeyCode::CONTROL),
        "ControlRight" => Some(KeyCode::CONTROL),
        "AltLeft" => Some(KeyCode::OPTION),
        "AltRight" => Some(KeyCode::OPTION),
        "MetaLeft" => Some(KeyCode::COMMAND),
        "MetaRight" => Some(KeyCode::COMMAND),
        "CapsLock" => Some(KeyCode::CAPS_LOCK),
        "F1" => Some(KeyCode::F1),
        "F2" => Some(KeyCode::F2),
        "F3" => Some(KeyCode::F3),
        "F4" => Some(KeyCode::F4),
        "F5" => Some(KeyCode::F5),
        "F6" => Some(KeyCode::F6),
        "F7" => Some(KeyCode::F7),
        "F8" => Some(KeyCode::F8),
        "F9" => Some(KeyCode::F9),
        "F10" => Some(KeyCode::F10),
        "F11" => Some(KeyCode::F11),
        "F12" => Some(KeyCode::F12),
        _ => None,
    }
}

/// Inject a mouse/keyboard event into the system.
pub fn inject(event: &InputEvent) -> Result<(), String> {
    let source = CGEventSource::new(CGEventSourceStateID::HIDSystemState)
        .map_err(|_| "event source".to_string())?;
    let (sw, sh) = screen_size_points()?;
    match event {
        InputEvent::MouseMove { x, y } => {
            let (px, py) = normalized_to_points(*x, *y, sw, sh);
            let ev = CGEvent::new_mouse_event(
                source,
                CGEventType::MouseMoved,
                CGPoint::new(px, py),
                CGMouseButton::Left,
            )
            .map_err(|_| "mouse move event".to_string())?;
            ev.post(CGEventTapLocation::HID);
            Ok(())
        }
        InputEvent::MouseButton {
            button,
            state,
            x,
            y,
        } => {
            let (px, py) = normalized_to_points(*x, *y, sw, sh);
            let (ev_type, cg_button) = match (button, state) {
                (MouseButton::Left, ButtonState::Pressed) => {
                    (CGEventType::LeftMouseDown, CGMouseButton::Left)
                }
                (MouseButton::Left, ButtonState::Released) => {
                    (CGEventType::LeftMouseUp, CGMouseButton::Left)
                }
                (MouseButton::Right, ButtonState::Pressed) => {
                    (CGEventType::RightMouseDown, CGMouseButton::Right)
                }
                (MouseButton::Right, ButtonState::Released) => {
                    (CGEventType::RightMouseUp, CGMouseButton::Right)
                }
                (MouseButton::Middle, ButtonState::Pressed) => {
                    (CGEventType::OtherMouseDown, CGMouseButton::Center)
                }
                (MouseButton::Middle, ButtonState::Released) => {
                    (CGEventType::OtherMouseUp, CGMouseButton::Center)
                }
                _ => return Err("unsupported button".into()),
            };
            let ev = CGEvent::new_mouse_event(source, ev_type, CGPoint::new(px, py), cg_button)
                .map_err(|_| "mouse button event".to_string())?;
            ev.post(CGEventTapLocation::HID);
            Ok(())
        }
        InputEvent::Wheel {
            x,
            y,
            delta_x,
            delta_y,
        } => {
            let (px, py) = normalized_to_points(*x, *y, sw, sh);
            let ev = CGEvent::new_scroll_event(
                source,
                ScrollEventUnit::PIXEL,
                2,
                *delta_y as i32,
                *delta_x as i32,
                0,
            )
            .map_err(|_| "scroll event".to_string())?;
            ev.set_location(CGPoint::new(px, py));
            ev.post(CGEventTapLocation::HID);
            Ok(())
        }
        InputEvent::Key {
            code,
            state,
            modifiers,
        } => {
            let Some(keycode) = keycode_for_code(code) else {
                return Err(format!("unsupported key code: {code}"));
            };
            let down = matches!(state, ButtonState::Pressed);
            let ev = CGEvent::new_keyboard_event(source, keycode, down)
                .map_err(|_| "keyboard event".to_string())?;
            ev.set_flags(modifier_flags(modifiers));
            ev.post(CGEventTapLocation::HID);
            Ok(())
        }
        _ => Err("unsupported event type".into()),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use aerodesk_core::protocol::input::InputEvent;

    #[test]
    fn normalized_maps_to_display_points() {
        let (x, y) = normalized_to_points(0.5, 0.25, 1920.0, 1080.0);
        assert_eq!(x, 960.0);
        assert_eq!(y, 270.0);
        let (x, y) = normalized_to_points(1.5, -0.2, 100.0, 100.0);
        assert_eq!(x, 100.0);
        assert_eq!(y, 0.0);
    }

    #[test]
    fn keycodes_cover_common_codes() {
        assert_eq!(keycode_for_code("KeyA"), Some(0x00)); // ANSI_A = 0
        assert!(keycode_for_code("Enter").is_some());
        assert!(keycode_for_code("ArrowUp").is_some());
        assert!(keycode_for_code("F5").is_some());
        assert!(keycode_for_code("UnknownKey").is_none());
    }

    #[test]
    fn keycodes_cover_punctuation() {
        // #75 键盘注入：标点/符号（viewer key-code 映射输出）。
        for code in [
            "Minus",
            "Equal",
            "BracketLeft",
            "BracketRight",
            "Backslash",
            "Semicolon",
            "Quote",
            "Backquote",
            "Comma",
            "Period",
            "Slash",
            "CapsLock",
        ] {
            assert!(
                keycode_for_code(code).is_some(),
                "{code} 应能映射到 CGKeyCode"
            );
        }
        assert_eq!(keycode_for_code("Period"), Some(0x2F)); // ANSI_PERIOD
        assert_eq!(keycode_for_code("Minus"), Some(0x1B)); // ANSI_MINUS
    }

    #[test]
    fn active_display_set_get_roundtrip() {
        // 纯状态读写，无需硬件。结尾恢复 None：该静态跨测试共享，
        // 残留 Some(0) 会污染 cursor 测试的坐标系选择。
        set_active_display(Some(42));
        assert_eq!(active_display(), Some(42));
        set_active_display(Some(0));
        assert_eq!(active_display(), Some(0));
        set_active_display(None);
        assert_eq!(active_display(), None);
    }

    #[test]
    fn dpi_mapping_common_display_sizes() {
        // #75 高 DPI/多分辨率：CGEvent 坐标为 points，normalized 0..1 直接映射。
        // Retina 2x（物理 3456x2234 → 逻辑 1728x1117 points）
        let (x, y) = normalized_to_points(0.5, 0.5, 1728.0, 1117.0);
        assert_eq!(x, 864.0);
        assert_eq!(y, 558.5);
        // 4K 电视 @200% → 逻辑 1920x1080 points
        let (x, y) = normalized_to_points(0.25, 0.75, 1920.0, 1080.0);
        assert_eq!(x, 480.0);
        assert_eq!(y, 810.0);
        // 1440p
        let (x, y) = normalized_to_points(1.0, 0.0, 2560.0, 1440.0);
        assert_eq!(x, 2560.0);
        assert_eq!(y, 0.0);
        // 720p
        let (x, y) = normalized_to_points(0.0, 1.0, 1280.0, 720.0);
        assert_eq!(x, 0.0);
        assert_eq!(y, 720.0);
    }

    #[test]
    fn wheel_event_supported() {
        // 直接测 inject 需要屏幕权限；这里只验证 Wheel 走 scroll 分支不 panic。
        let ev = InputEvent::Wheel {
            x: 0.5,
            y: 0.5,
            delta_x: 10.0,
            delta_y: -20.0,
        };
        let _ = inject(&ev);
    }
}

/// 核心 `InputInjector` 实现（被控端输入注入，CGEvent）。
pub struct MacInjector;

impl aerodesk_core::platform::InputInjector for MacInjector {
    type Error = String;

    fn inject(
        &mut self,
        event: &aerodesk_core::protocol::input::InputEvent,
    ) -> Result<(), Self::Error> {
        crate::macos::inject::inject(event)
    }
}
