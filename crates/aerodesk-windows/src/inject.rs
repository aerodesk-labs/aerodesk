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
///
/// 多显示器：`set_active_display` 设置被控显示器在虚拟屏幕中的区域，
/// 归一化坐标 (0..1) 映射到该显示器（#75），而非整个虚拟屏幕。
#[cfg(windows)]
pub struct SendInputInjector {
    /// 活动显示器在虚拟屏幕中的区域（像素 x,y,w,h）；None = 整个虚拟屏幕。
    display_rect: Option<(i32, i32, u32, u32)>,
}

#[cfg(windows)]
impl SendInputInjector {
    pub fn new() -> Self {
        Self { display_rect: None }
    }

    /// 设置活动显示器在虚拟屏幕中的区域（像素）；None 回退整个虚拟屏幕。
    pub fn set_active_display(&mut self, rect: Option<(i32, i32, u32, u32)>) {
        self.display_rect = rect;
    }
}

#[cfg(windows)]
impl aerodesk_core::platform::InputInjector for SendInputInjector {
    type Error = String;

    fn inject(&mut self, event: &InputEvent) -> Result<(), String> {
        use std::mem::size_of;
        use windows::Win32::UI::Input::KeyboardAndMouse::SendInput;

        unsafe {
            let inputs = match event {
                InputEvent::MouseMove { x, y } => vec![self.mouse_move(*x as f32, *y as f32)],
                InputEvent::MouseButton {
                    button: MouseButton::Left,
                    state,
                    x,
                    y,
                } => vec![
                    self.mouse_move(*x as f32, *y as f32),
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
impl SendInputInjector {
    /// 归一化坐标 (0..1) → 虚拟屏幕绝对坐标（0..65535）。
    /// 映射到 `display_rect` 指定的活动显示器区域（多显示器，#75）。
    fn mouse_move(&self, x: f32, y: f32) -> windows::Win32::UI::Input::KeyboardAndMouse::INPUT {
        use windows::Win32::UI::Input::KeyboardAndMouse::{
            INPUT, INPUT_0, INPUT_MOUSE, MOUSEEVENTF_ABSOLUTE, MOUSEEVENTF_MOVE, MOUSEINPUT,
        };
        let virtual_rect = virtual_screen();
        let (dx, dy) = map_to_virtual(x, y, self.display_rect, virtual_rect);
        INPUT {
            r#type: INPUT_MOUSE,
            Anonymous: INPUT_0 {
                mi: MOUSEINPUT {
                    dx,
                    dy,
                    mouseData: 0,
                    dwFlags: MOUSEEVENTF_MOVE | MOUSEEVENTF_ABSOLUTE,
                    time: 0,
                    dwExtraInfo: 0,
                },
            },
        }
    }
}

/// 虚拟屏幕（所有显示器并集）像素矩形。
#[cfg(windows)]
pub(crate) fn virtual_screen() -> (i32, i32, u32, u32) {
    use windows::Win32::UI::WindowsAndMessaging::{
        GetSystemMetrics, SM_CXVIRTUALSCREEN, SM_CYVIRTUALSCREEN, SM_XVIRTUALSCREEN,
        SM_YVIRTUALSCREEN,
    };
    unsafe {
        let vx = GetSystemMetrics(SM_XVIRTUALSCREEN);
        let vy = GetSystemMetrics(SM_YVIRTUALSCREEN);
        let vw = GetSystemMetrics(SM_CXVIRTUALSCREEN).max(0) as u32;
        let vh = GetSystemMetrics(SM_CYVIRTUALSCREEN).max(0) as u32;
        (vx, vy, vw, vh)
    }
}

/// 归一化坐标 (0..1) → 虚拟屏幕绝对坐标（0..65535）。
/// `display` 为活动显示器在虚拟屏幕中的区域；`virtual` 为虚拟屏幕矩形。
fn map_to_virtual(
    x: f32,
    y: f32,
    display: Option<(i32, i32, u32, u32)>,
    virtual_rect: (i32, i32, u32, u32),
) -> (i32, i32) {
    let (vx, vy, vw, vh) = virtual_rect;
    let (dx, dy, dw, dh) = display.unwrap_or((vx, vy, vw, vh));
    let cx = x.clamp(0.0, 1.0) as f64;
    let cy = y.clamp(0.0, 1.0) as f64;
    let px = dx as f64 + cx * dw as f64;
    let py = dy as f64 + cy * dh as f64;
    let sx = if vw > 0 {
        ((px - vx as f64) / vw as f64 * 65535.0).round() as i32
    } else {
        0
    };
    let sy = if vh > 0 {
        ((py - vy as f64) / vh as f64 * 65535.0).round() as i32
    } else {
        0
    };
    (sx.clamp(0, 65535), sy.clamp(0, 65535))
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
    use super::*;

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

    /// 多显示器坐标换算：#75 归一化坐标映射到活动显示器在虚拟屏幕中的区域。
    #[test]
    fn map_to_virtual_uses_active_display_rect() {
        // 双显示器：左屏 1920x1080（0,0），右屏 1920x1080（1920,0），虚拟屏幕 3840x1080。
        let virtual_rect = (0, 0, 3840, 1080);
        // 未设置显示器：归一化到整个虚拟屏幕（x=0.5 → 虚拟屏中点）。
        let (sx, sy) = map_to_virtual(0.5, 0.5, None, virtual_rect);
        assert_eq!((sx, sy), (32768, 32768), "全虚拟屏幕映射");
        // 活动显示器 = 右屏（1920,0,1920,1080）：x=0 → 右屏左缘 = 虚拟屏 50% 位置。
        let right = Some((1920, 0, 1920, 1080));
        let (sx0, _) = map_to_virtual(0.0, 0.0, right, virtual_rect);
        assert_eq!(sx0, 32768, "右屏左缘应为虚拟屏中点");
        // x=1 → 右屏右缘 = 虚拟屏 100%。
        let (sx1, _) = map_to_virtual(1.0, 0.0, right, virtual_rect);
        assert_eq!(sx1, 65535, "右屏右缘应为虚拟屏末端");
        // x=0.5 → 右屏中点 = 虚拟屏 75%。
        let (sx5, _) = map_to_virtual(0.5, 0.0, right, virtual_rect);
        assert_eq!(sx5, 49151, "右屏中点应为虚拟屏 75%");
    }

    /// 归一化坐标越界应 clamp 到 0..65535。
    #[test]
    fn map_to_virtual_clamps_out_of_range() {
        let virtual_rect = (0, 0, 1920, 1080);
        let (sx, sy) = map_to_virtual(-1.0, 2.0, None, virtual_rect);
        assert_eq!((sx, sy), (0, 65535));
    }
}
