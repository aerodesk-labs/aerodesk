//! CGEvent 输入注入（被控端：把 viewer 发来的输入事件注入系统）。

use aerodesk_protocol::input::{ButtonState, InputEvent, MouseButton};
use core_graphics::event::{CGEvent, CGEventTapLocation, CGEventType, CGMouseButton};
use core_graphics::event_source::{CGEventSource, CGEventSourceStateID};
use core_graphics_types::geometry::CGPoint;

/// 注入鼠标/键盘事件。返回错误描述。
pub fn inject(event: &InputEvent) -> Result<(), String> {
    let source = match CGEventSource::new(CGEventSourceStateID::HIDSystemState) {
        Ok(s) => s,
        Err(_) => return Err("event source".into()),
    };
    match event {
        InputEvent::MouseMove { x, y } => {
            let pt = CGPoint::new(*x, *y);
            let ev =
                CGEvent::new_mouse_event(source, CGEventType::MouseMoved, pt, CGMouseButton::Left)
                    .map_err(|_| "mouse event")?;
            ev.post(CGEventTapLocation::HID);
            Ok(())
        }
        InputEvent::MouseButton {
            button,
            state,
            x,
            y,
        } => {
            let pt = CGPoint::new(*x, *y);
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
            let ev = CGEvent::new_mouse_event(source, ev_type, pt, cg_button)
                .map_err(|_| "mouse button event")?;
            ev.post(CGEventTapLocation::HID);
            Ok(())
        }
        InputEvent::Key { code, state, .. } => {
            let ch = code.chars().next().ok_or("empty key code")?;
            let down = matches!(state, ButtonState::Pressed);
            let ev = CGEvent::new_keyboard_event(source, keycode_for_char(ch), down)
                .map_err(|_| "keyboard event")?;
            ev.post(CGEventTapLocation::HID);
            Ok(())
        }
        _ => Err("unsupported event type".into()),
    }
}

/// 简化键码映射（完整映射表为 P2 后续项）。
fn keycode_for_char(c: char) -> u16 {
    match c {
        'a'..='z' => c as u16 - 'a' as u16,
        'A'..='Z' => c as u16 - 'A' as u16,
        '0'..='9' => 0x12 + (c as u16 - '0' as u16),
        '\r' | '\n' => 0x24,
        ' ' => 0x31,
        _ => 0,
    }
}
