//! 输入注入（骨架）。
//!
//! X11：XTestFakeMotionEvent / XTestFakeButtonEvent / XTestFakeKeyEvent；
//! Wayland：/dev/uinput（uinput crate）或 `ydotool`。

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

/// TODO(P4): XTest/uinput 实现。
pub struct XTestInjector;

impl InputInjector for XTestInjector {
    fn inject(&mut self, _event: &InputEvent) -> Result<(), String> {
        Err("linux: XTest injection not implemented yet (P4)".into())
    }
}
