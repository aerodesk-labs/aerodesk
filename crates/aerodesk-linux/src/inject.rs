//! 输入注入（被控端）。
//!
//! X11：XTestFakeInput（x11rb XTEST 扩展）；Wayland：/dev/uinput（真机阶段）。

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
            .map_err(|e| format!("xtest_fake_input: {e:?}"))
    }
}

#[cfg(target_os = "linux")]
impl InputInjector for XTestInjector {
    fn inject(&mut self, event: &InputEvent) -> Result<(), String> {
        use x11rb::protocol::xtest::{
            FAKE_INPUT_BUTTON_PRESS, FAKE_INPUT_BUTTON_RELEASE, FAKE_INPUT_KEY_PRESS,
            FAKE_INPUT_KEY_RELEASE, FAKE_INPUT_MOTION,
        };
        let to_px = |v: f32| (v.clamp(0.0, 1.0) * self.width as f32) as i16;
        let to_py = |v: f32| (v.clamp(0.0, 1.0) * self.height as f32) as i16;
        match event {
            InputEvent::MouseMove { x, y } => {
                self.fake(FAKE_INPUT_MOTION, 0, to_px(*x), to_py(*y))?;
            }
            InputEvent::MouseButton {
                x,
                y,
                button: 0,
                down,
            } => {
                self.fake(FAKE_INPUT_MOTION, 0, to_px(*x), to_py(*y))?;
                let kind = if *down {
                    FAKE_INPUT_BUTTON_PRESS
                } else {
                    FAKE_INPUT_BUTTON_RELEASE
                };
                self.fake(kind, 1, 0, 0)?; // 左键
            }
            InputEvent::MouseButton { .. } => {
                return Err("unsupported button (only left supported)".into());
            }
            InputEvent::Wheel { dy, .. } => {
                // X11 滚轮：按钮 4=上 / 5=下。
                let btn = if *dy > 0.0 { 4u8 } else { 5u8 };
                self.fake(FAKE_INPUT_BUTTON_PRESS, btn, 0, 0)?;
                self.fake(FAKE_INPUT_BUTTON_RELEASE, btn, 0, 0)?;
            }
            InputEvent::Key { code, down } => {
                let kind = if *down {
                    FAKE_INPUT_KEY_PRESS
                } else {
                    FAKE_INPUT_KEY_RELEASE
                };
                self.fake(kind, *code as u8, 0, 0)?;
            }
        }
        Ok(())
    }
}

/// 非 Linux 主机上的编译期骨架。
#[cfg(not(target_os = "linux"))]
pub struct XTestInjector;

#[cfg(not(target_os = "linux"))]
impl InputInjector for XTestInjector {
    fn inject(&mut self, _event: &InputEvent) -> Result<(), String> {
        Err("linux: XTest injection only available on Linux".into())
    }
}
