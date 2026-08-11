//! 输入注入：SendInput（鼠标绝对坐标/按键/滚轮）。

use aerodesk_protocol::input::{ButtonState, InputEvent, MouseButton};

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
                // TODO(P4)：协议键码（String）→ Windows VK 映射表，随 Windows
                // 适配器真机批次实现；macOS 已有完整键码映射（inject.rs）。
                InputEvent::Key { .. } => {
                    return Err("windows: key code mapping not implemented yet (P4)".into());
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
