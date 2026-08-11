//! 输入注入（被控端）。
//!
//! X11：XTestFakeInput（x11rb XTEST 扩展）；Wayland：/dev/uinput（真机阶段）。

use aerodesk_protocol::input::{ButtonState, InputEvent, MouseButton};

/// 平台无关键码（协议）→ X11 keysym。
pub fn keysym_for_code(code: &str) -> Option<u32> {
    let ks = match code {
        "KeyA" => 0x61,
        "KeyB" => 0x62,
        "KeyC" => 0x63,
        "KeyD" => 0x64,
        "KeyE" => 0x65,
        "KeyF" => 0x66,
        "KeyG" => 0x67,
        "KeyH" => 0x68,
        "KeyI" => 0x69,
        "KeyJ" => 0x6A,
        "KeyK" => 0x6B,
        "KeyL" => 0x6C,
        "KeyM" => 0x6D,
        "KeyN" => 0x6E,
        "KeyO" => 0x6F,
        "KeyP" => 0x70,
        "KeyQ" => 0x71,
        "KeyR" => 0x72,
        "KeyS" => 0x73,
        "KeyT" => 0x74,
        "KeyU" => 0x75,
        "KeyV" => 0x76,
        "KeyW" => 0x77,
        "KeyX" => 0x78,
        "KeyY" => 0x79,
        "KeyZ" => 0x7A,
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
        "Minus" => 0x2D,
        "Equal" => 0x3D,
        "BracketLeft" => 0x5B,
        "BracketRight" => 0x5D,
        "Backslash" => 0x5C,
        "Semicolon" => 0x3B,
        "Quote" => 0x27,
        "Backquote" => 0x60,
        "Comma" => 0x2C,
        "Period" => 0x2E,
        "Slash" => 0x2F,
        "Enter" => 0xFF0D,
        "Tab" => 0xFF09,
        "Space" => 0x20,
        "Backspace" => 0xFF08,
        "Escape" => 0xFF1B,
        "Delete" => 0xFFFF,
        "ArrowUp" => 0xFF52,
        "ArrowDown" => 0xFF54,
        "ArrowLeft" => 0xFF51,
        "ArrowRight" => 0xFF53,
        "Home" => 0xFF50,
        "End" => 0xFF57,
        "PageUp" => 0xFF55,
        "PageDown" => 0xFF56,
        "ShiftLeft" | "ShiftRight" => 0xFFE1,
        "ControlLeft" | "ControlRight" => 0xFFE3,
        "AltLeft" | "AltRight" => 0xFFE9,
        "MetaLeft" | "MetaRight" => 0xFFEB,
        "CapsLock" => 0xFFE5,
        "F1" => 0xFFBE,
        "F2" => 0xFFBF,
        "F3" => 0xFFC0,
        "F4" => 0xFFC1,
        "F5" => 0xFFC2,
        "F6" => 0xFFC3,
        "F7" => 0xFFC4,
        "F8" => 0xFFC5,
        "F9" => 0xFFC6,
        "F10" => 0xFFC7,
        "F11" => 0xFFC8,
        "F12" => 0xFFC9,
        _ => return None,
    };
    Some(ks)
}

/// XTest fake input 类型码（X11 核心事件码）。
#[cfg(target_os = "linux")]
mod fake_input {
    pub const KEY_PRESS: u8 = 2;
    pub const KEY_RELEASE: u8 = 3;
    pub const BUTTON_PRESS: u8 = 4;
    pub const BUTTON_RELEASE: u8 = 5;
    pub const MOTION_NOTIFY: u8 = 6;
}

/// XTest 注入器（X11 桌面）。
#[cfg(target_os = "linux")]
pub struct XTestInjector {
    conn: x11rb::rust_connection::RustConnection,
    root: x11rb::protocol::xproto::Window,
    width: u32,
    height: u32,
}

#[cfg(target_os = "linux")]
impl XTestInjector {
    pub fn new() -> Result<Self, String> {
        use x11rb::connection::Connection;
        use x11rb::protocol::xproto::ConnectionExt;

        let (conn, screen_num) = x11rb::rust_connection::RustConnection::connect(None)
            .map_err(|e| format!("x11 connect: {e}"))?;
        let root = conn.setup().roots[screen_num].root;
        let geo = conn
            .get_geometry(root)
            .map_err(|e| format!("get_geometry: {e:?}"))?
            .reply()
            .map_err(|e| format!("get_geometry reply: {e}"))?;
        let (width, height) = (geo.width.max(1) as u32, geo.height.max(1) as u32);
        Ok(Self {
            conn,
            root,
            width,
            height,
        })
    }

    fn fake(&self, kind: u8, detail: u8, x: i16, y: i16) -> Result<(), String> {
        use x11rb::connection::Connection;
        use x11rb::protocol::xproto::ConnectionExt;
        use x11rb::protocol::xtest::ConnectionExt as _;

        self.conn
            .xtest_fake_input(
                kind,
                detail,
                x11rb::CURRENT_TIME.into(),
                self.root,
                x,
                y,
                0, // deviceid
            )
            .map_err(|e| format!("xtest_fake_input: {e:?}"))?;
        Ok(())
    }

    /// keysym → keycode（XTestFakeInput 需要硬件 keycode；查键盘映射表）。
    fn keycode_for_keysym(&self, keysym: u32) -> Result<u8, String> {
        use x11rb::connection::Connection;
        use x11rb::protocol::xproto::ConnectionExt;

        let reply = self
            .conn
            .get_keyboard_mapping(8, 248) // X11 keycode 固定 8..=255
            .map_err(|e| format!("get_keyboard_mapping: {e:?}"))?
            .reply()
            .map_err(|e| format!("get_keyboard_mapping reply: {e:?}"))?;
        let per = reply.keysyms_per_keycode.max(1) as usize;
        for (i, chunk) in reply.keysyms.chunks(per).enumerate() {
            if chunk.contains(&keysym) {
                return Ok(8 + i as u8);
            }
        }
        Err(format!("keysym 0x{keysym:x} 无对应 keycode"))
    }
}

#[cfg(target_os = "linux")]
impl aerodesk_core::platform::InputInjector for XTestInjector {
    type Error = String;

    fn inject(&mut self, event: &InputEvent) -> Result<(), String> {
        let to_px = |v: f64| (v.clamp(0.0, 1.0) * self.width as f64) as i16;
        let to_py = |v: f64| (v.clamp(0.0, 1.0) * self.height as f64) as i16;
        match event {
            InputEvent::MouseMove { x, y } => {
                self.fake(fake_input::MOTION_NOTIFY, 0, to_px(*x), to_py(*y))?;
            }
            InputEvent::MouseButton {
                button: MouseButton::Left,
                state,
                x,
                y,
            } => {
                self.fake(fake_input::MOTION_NOTIFY, 0, to_px(*x), to_py(*y))?;
                let kind = if *state == ButtonState::Pressed {
                    fake_input::BUTTON_PRESS
                } else {
                    fake_input::BUTTON_RELEASE
                };
                self.fake(kind, 1, 0, 0)?; // 左键
            }
            InputEvent::MouseButton { .. } => {
                return Err("unsupported button (only left supported)".into());
            }
            InputEvent::Wheel { delta_y, .. } => {
                // X11 滚轮：按钮 4=上 / 5=下。
                let btn = if *delta_y > 0.0 { 4u8 } else { 5u8 };
                self.fake(fake_input::BUTTON_PRESS, btn, 0, 0)?;
                self.fake(fake_input::BUTTON_RELEASE, btn, 0, 0)?;
            }
            InputEvent::Key {
                code,
                state,
                modifiers,
            } => {
                let keysym =
                    keysym_for_code(code).ok_or_else(|| format!("unsupported key code: {code}"))?;
                let keycode = self.keycode_for_keysym(keysym)?;
                let down = *state == ButtonState::Pressed;
                let mods: [u32; 4] = [
                    if modifiers.ctrl { 0xFFE3 } else { 0 },
                    if modifiers.shift { 0xFFE1 } else { 0 },
                    if modifiers.alt { 0xFFE9 } else { 0 },
                    if modifiers.meta { 0xFFEB } else { 0 },
                ];
                if down {
                    for ms in mods.into_iter().filter(|m| *m != 0) {
                        let mc = self.keycode_for_keysym(ms)?;
                        self.fake(fake_input::KEY_PRESS, mc, 0, 0)?;
                    }
                    self.fake(fake_input::KEY_PRESS, keycode, 0, 0)?;
                } else {
                    self.fake(fake_input::KEY_RELEASE, keycode, 0, 0)?;
                    for ms in mods.into_iter().filter(|m| *m != 0) {
                        let mc = self.keycode_for_keysym(ms)?;
                        self.fake(fake_input::KEY_RELEASE, mc, 0, 0)?;
                    }
                }
            }
            InputEvent::Touch { .. } => {
                return Err("linux: touch injection not implemented".into());
            }
            InputEvent::ClipboardText(_) => {
                return Err("linux: clipboard inject not implemented".into());
            }
        }
        Ok(())
    }
}

/// 非 Linux 主机上的编译期骨架。
#[cfg(not(target_os = "linux"))]
pub struct XTestInjector;

#[cfg(not(target_os = "linux"))]
impl aerodesk_core::platform::InputInjector for XTestInjector {
    type Error = String;

    fn inject(&mut self, _event: &InputEvent) -> Result<(), String> {
        Err("linux: XTest injection only available on Linux".into())
    }
}

#[cfg(test)]
mod tests {
    use super::keysym_for_code;

    #[test]
    fn keysym_map_covers_letters_digits_and_common_keys() {
        assert_eq!(keysym_for_code("KeyA"), Some(0x61));
        assert_eq!(keysym_for_code("KeyZ"), Some(0x7A));
        assert_eq!(keysym_for_code("Digit0"), Some(0x30));
        assert_eq!(keysym_for_code("Enter"), Some(0xFF0D));
        assert_eq!(keysym_for_code("Space"), Some(0x20));
        assert_eq!(keysym_for_code("ArrowUp"), Some(0xFF52));
        assert_eq!(keysym_for_code("ControlLeft"), Some(0xFFE3));
        assert_eq!(keysym_for_code("F12"), Some(0xFFC9));
        assert_eq!(keysym_for_code("NotAKey"), None);
    }
}
