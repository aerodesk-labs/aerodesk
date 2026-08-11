//! 输入注入（被控端）。
//!
//! X11：XTestFakeInput（x11rb XTEST 扩展）；Wayland：/dev/uinput（真机阶段）。

use aerodesk_protocol::input::{ButtonState, InputEvent, MouseButton};

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
            // TODO(P4)：协议键码（String）→ X11 keysym 映射表，随 Linux
            // 适配器真机批次实现；macOS 已有完整键码映射（inject.rs）。
            InputEvent::Key { .. } => {
                return Err("linux: key code mapping not implemented yet (P4)".into());
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
