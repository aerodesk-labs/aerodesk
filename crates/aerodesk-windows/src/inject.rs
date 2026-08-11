//! 输入注入：SendInput（鼠标绝对坐标/按键/滚轮）。

use aerodesk_protocol::input::{ButtonState, InputEvent, MouseButton};

/// 平台无关键码（协议）→ Windows Virtual-Key（VK_*）。
pub fn vk_for_code(code: &str) -> Option<u16> {
    let vk = match code {
        "KeyA" => 0x41,
        "KeyB" => 0x42,
        "KeyC" => 0x43,
        "KeyD" => 0x44,
        "KeyE" => 0x45,
        "KeyF" => 0x46,
        "KeyG" => 0x47,
        "KeyH" => 0x48,
        "KeyI" => 0x49,
        "KeyJ" => 0x4A,
        "KeyK" => 0x4B,
        "KeyL" => 0x4C,
        "KeyM" => 0x4D,
        "KeyN" => 0x4E,
        "KeyO" => 0x4F,
        "KeyP" => 0x50,
        "KeyQ" => 0x51,
        "KeyR" => 0x52,
        "KeyS" => 0x53,
        "KeyT" => 0x54,
        "KeyU" => 0x55,
        "KeyV" => 0x56,
        "KeyW" => 0x57,
        "KeyX" => 0x58,
        "KeyY" => 0x59,
        "KeyZ" => 0x5A,
        "Digit0" => 0x30,
        "Digit1" => 0x31,
        "Digit2" => 0x32,
        "Digit3" => 0x33,
        "Digit4" => 0x34,
        "Digit5" => 0x35,
        "Digit6" => 0x36,
        "Digit7" => 0x37,
        "Digit8" => 0x38,
        "Digit9" => 0x39,
        "Minus" => 0xBD,
        "Equal" => 0xBB,
        "BracketLeft" => 0xDB,
        "BracketRight" => 0xDD,
        "Backslash" => 0xDC,
        "Semicolon" => 0xBA,
        "Quote" => 0xDE,
        "Backquote" => 0xC0,
        "Comma" => 0xBC,
        "Period" => 0xBE,
        "Slash" => 0xBF,
        "Enter" => 0x0D,
        "Tab" => 0x09,
        "Space" => 0x20,
        "Backspace" => 0x08,
        "Escape" => 0x1B,
        "Delete" => 0x2E,
        "ArrowUp" => 0x26,
        "ArrowDown" => 0x28,
        "ArrowLeft" => 0x25,
        "ArrowRight" => 0x27,
        "Home" => 0x24,
        "End" => 0x23,
        "PageUp" => 0x21,
        "PageDown" => 0x22,
        "ShiftLeft" | "ShiftRight" => 0x10,
        "ControlLeft" | "ControlRight" => 0x11,
        "AltLeft" | "AltRight" => 0x12,
        "MetaLeft" | "MetaRight" => 0x5B,
        "CapsLock" => 0x14,
        "F1" => 0x70,
        "F2" => 0x71,
        "F3" => 0x72,
        "F4" => 0x73,
        "F5" => 0x74,
        "F6" => 0x75,
        "F7" => 0x76,
        "F8" => 0x77,
        "F9" => 0x78,
        "F10" => 0x79,
        "F11" => 0x7A,
        "F12" => 0x7B,
        _ => return None,
    };
    Some(vk)
}

/// SendInput 注入器（普通桌面会话可用）。
#[cfg(windows)]
pub struct SendInputInjector;

#[cfg(windows)]
impl aerodesk_core::platform::InputInjector for SendInputInjector {
    type Error = String;

    fn inject(&mut self, event: &InputEvent) -> Result<(), String> {
        use std::mem::size_of;
        use windows::Win32::UI::Input::KeyboardAndMouse::SendInput;

        unsafe {
            let inputs = match event {
                InputEvent::MouseMove { x, y } => vec![mouse_move(*x as f32, *y as f32)],
                InputEvent::MouseButton {
                    button: MouseButton::Left,
                    state,
                    x,
                    y,
                } => vec![
                    mouse_move(*x as f32, *y as f32),
                    mouse_button(*state == ButtonState::Pressed),
                ],
                InputEvent::MouseButton { .. } => {
                    return Err("unsupported button (only left supported)".into());
                }
                InputEvent::Wheel { delta_y, .. } => vec![wheel(*delta_y as f32)],
                InputEvent::Key {
                    code,
                    state,
                    modifiers,
                } => {
                    let vk =
                        vk_for_code(code).ok_or_else(|| format!("unsupported key code: {code}"))?;
                    let down = *state == ButtonState::Pressed;
                    let mods: [u16; 4] = [
                        if modifiers.ctrl { 0x11 } else { 0 },
                        if modifiers.shift { 0x10 } else { 0 },
                        if modifiers.alt { 0x12 } else { 0 },
                        if modifiers.meta { 0x5B } else { 0 },
                    ];
                    let mut inputs = Vec::new();
                    if down {
                        for m in mods.into_iter().filter(|m| *m != 0) {
                            inputs.push(key(m as u32, true));
                        }
                        inputs.push(key(vk as u32, true));
                    } else {
                        inputs.push(key(vk as u32, false));
                        for m in mods.into_iter().filter(|m| *m != 0) {
                            inputs.push(key(m as u32, false));
                        }
                    }
                    inputs
                }
                InputEvent::Touch { .. } => {
                    return Err("windows: touch injection not implemented".into());
                }
                InputEvent::ClipboardText(_) => {
                    return Err("windows: clipboard inject not implemented".into());
                }
            };
            let sent = SendInput(
                &inputs,
                size_of::<windows::Win32::UI::Input::KeyboardAndMouse::INPUT>() as i32,
            );
            if sent as usize != inputs.len() {
                return Err("SendInput partial".into());
            }
        }
        Ok(())
    }
}

/// 非 Windows 主机上的编译期骨架（保证 workspace 全平台可编译）。
#[cfg(not(windows))]
pub struct SendInputInjector;

#[cfg(not(windows))]
impl aerodesk_core::platform::InputInjector for SendInputInjector {
    type Error = String;

    fn inject(&mut self, _event: &InputEvent) -> Result<(), String> {
        Err("windows: SendInput injection only available on Windows".into())
    }
}

#[cfg(windows)]
fn mouse_move(x: f32, y: f32) -> windows::Win32::UI::Input::KeyboardAndMouse::INPUT {
    use windows::Win32::UI::Input::KeyboardAndMouse::{
        INPUT, INPUT_0, INPUT_MOUSE, MOUSEEVENTF_ABSOLUTE, MOUSEEVENTF_MOVE, MOUSEINPUT,
    };
    INPUT {
        r#type: INPUT_MOUSE,
        Anonymous: INPUT_0 {
            mi: MOUSEINPUT {
                dx: (x.clamp(0.0, 1.0) * 65535.0) as i32,
                dy: (y.clamp(0.0, 1.0) * 65535.0) as i32,
                mouseData: 0,
                dwFlags: MOUSEEVENTF_MOVE | MOUSEEVENTF_ABSOLUTE,
                time: 0,
                dwExtraInfo: 0,
            },
        },
    }
}

#[cfg(windows)]
fn mouse_button(down: bool) -> windows::Win32::UI::Input::KeyboardAndMouse::INPUT {
    use windows::Win32::UI::Input::KeyboardAndMouse::{
        INPUT, INPUT_0, INPUT_MOUSE, MOUSEEVENTF_LEFTDOWN, MOUSEEVENTF_LEFTUP, MOUSEINPUT,
    };
    INPUT {
        r#type: INPUT_MOUSE,
        Anonymous: INPUT_0 {
            mi: MOUSEINPUT {
                dx: 0,
                dy: 0,
                mouseData: 0,
                dwFlags: if down {
                    MOUSEEVENTF_LEFTDOWN
                } else {
                    MOUSEEVENTF_LEFTUP
                },
                time: 0,
                dwExtraInfo: 0,
            },
        },
    }
}

#[cfg(windows)]
fn wheel(dy: f32) -> windows::Win32::UI::Input::KeyboardAndMouse::INPUT {
    use windows::Win32::UI::Input::KeyboardAndMouse::{
        INPUT, INPUT_0, INPUT_MOUSE, MOUSEEVENTF_WHEEL, MOUSEINPUT,
    };
    INPUT {
        r#type: INPUT_MOUSE,
        Anonymous: INPUT_0 {
            mi: MOUSEINPUT {
                dx: 0,
                dy: 0,
                mouseData: (dy.clamp(-100.0, 100.0) * 120.0) as u32,
                dwFlags: MOUSEEVENTF_WHEEL,
                time: 0,
                dwExtraInfo: 0,
            },
        },
    }
}

#[cfg(windows)]
fn key(code: u32, down: bool) -> windows::Win32::UI::Input::KeyboardAndMouse::INPUT {
    use windows::Win32::UI::Input::KeyboardAndMouse::{
        INPUT, INPUT_0, INPUT_KEYBOARD, KEYBD_EVENT_FLAGS, KEYBDINPUT, KEYEVENTF_KEYUP, VIRTUAL_KEY,
    };
    INPUT {
        r#type: INPUT_KEYBOARD,
        Anonymous: INPUT_0 {
            ki: KEYBDINPUT {
                wVk: VIRTUAL_KEY(code as u16),
                wScan: 0,
                dwFlags: if down {
                    KEYBD_EVENT_FLAGS(0)
                } else {
                    KEYEVENTF_KEYUP
                },
                time: 0,
                dwExtraInfo: 0,
            },
        },
    }
}

#[cfg(test)]
mod tests {
    use super::vk_for_code;

    #[test]
    fn vk_map_covers_letters_digits_and_common_keys() {
        assert_eq!(vk_for_code("KeyA"), Some(0x41));
        assert_eq!(vk_for_code("KeyZ"), Some(0x5A));
        assert_eq!(vk_for_code("Digit0"), Some(0x30));
        assert_eq!(vk_for_code("Digit9"), Some(0x39));
        assert_eq!(vk_for_code("Enter"), Some(0x0D));
        assert_eq!(vk_for_code("Space"), Some(0x20));
        assert_eq!(vk_for_code("ArrowUp"), Some(0x26));
        assert_eq!(vk_for_code("ControlLeft"), Some(0x11));
        assert_eq!(vk_for_code("F12"), Some(0x7B));
        assert_eq!(vk_for_code("NotAKey"), None);
    }
}
