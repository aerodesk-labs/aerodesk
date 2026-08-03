//! 输入注入：SendInput（鼠标绝对坐标/按键/滚轮）。

use std::mem::size_of;

use windows::Win32::UI::Input::KeyboardAndMouse::{
    INPUT, INPUT_0, INPUT_KEYBOARD, INPUT_MOUSE, KEYBDINPUT, KEYEVENTF_KEYUP, MOUSEEVENTF_ABSOLUTE,
    MOUSEEVENTF_LEFTDOWN, MOUSEEVENTF_LEFTUP, MOUSEEVENTF_MOVE, MOUSEEVENTF_WHEEL, MOUSEINPUT,
    SendInput,
};

/// 输入事件（与 aerodesk-protocol::input 对齐）。
#[derive(Debug, Clone)]
pub enum InputEvent {
    MouseMove {
        x: f32,
        y: f32,
    },
    MouseButton {
        x: f32,
        y: f32,
        button: u8,
        down: bool,
    },
    Wheel {
        dx: f32,
        dy: f32,
    },
    Key {
        code: u32,
        down: bool,
    },
}

/// 注入抽象（被控端）。
pub trait InputInjector {
    fn inject(&mut self, event: &InputEvent) -> Result<(), String>;
}

/// SendInput 注入器（普通桌面会话可用）。
pub struct SendInputInjector;

impl InputInjector for SendInputInjector {
    fn inject(&mut self, event: &InputEvent) -> Result<(), String> {
        unsafe {
            let inputs = match event {
                InputEvent::MouseMove { x, y } => vec![mouse_move(*x, *y)],
                InputEvent::MouseButton {
                    x,
                    y,
                    button: 0,
                    down,
                } => vec![mouse_move(*x, *y), mouse_button(*down)],
                InputEvent::MouseButton { .. } => {
                    return Err("unsupported button (only left supported)".into());
                }
                InputEvent::Wheel { dy, .. } => vec![wheel(*dy)],
                InputEvent::Key { code, down } => vec![key(*code, *down)],
            };
            let sent = SendInput(&inputs, size_of::<INPUT>() as i32);
            if sent as usize != inputs.len() {
                return Err("SendInput partial".into());
            }
        }
        Ok(())
    }
}

fn mouse_move(x: f32, y: f32) -> INPUT {
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

fn mouse_button(down: bool) -> INPUT {
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

fn wheel(dy: f32) -> INPUT {
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

fn key(code: u32, down: bool) -> INPUT {
    INPUT {
        r#type: INPUT_KEYBOARD,
        Anonymous: INPUT_0 {
            ki: KEYBDINPUT {
                wVk: code as u16,
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
