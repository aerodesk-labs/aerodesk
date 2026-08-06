//! CGEvent input injection (controlled end: inject viewer input into the system).
//!
//! Protocol coordinates are normalized 0..1 relative to the remote screen;
//! this module converts to the actual display size in points (DPI-aware) and
//! posts CGEvents. Wheel and modifier keys are supported (#75).

use aerodesk_protocol::input::{ButtonState, InputEvent, Modifiers, MouseButton};
use core_graphics::display::CGDisplay;
use core_graphics::event::{
    CGEvent, CGEventFlags, CGEventTapLocation, CGEventType, CGMouseButton, ScrollEventUnit,
};
use core_graphics::event_source::{CGEventSource, CGEventSourceStateID};
use core_graphics_types::geometry::CGPoint;

/// Convert normalized (0..1) coordinates to display points.
pub fn normalized_to_points(x: f64, y: f64, width: f64, height: f64) -> (f64, f64) {
    (x.clamp(0.0, 1.0) * width, y.clamp(0.0, 1.0) * height)
}

/// Main display size in points (CGEvent coordinates are in points, DPI-aware).
pub fn screen_size_points() -> Result<(f64, f64), String> {
    let bounds = CGDisplay::main().bounds();
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
    use aerodesk_protocol::input::InputEvent;

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
