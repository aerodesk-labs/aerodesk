//! 输入注入（骨架）。
//!
//! 真机路径：`SendInput`（合成 INPUT 结构，鼠标/键盘/滚轮）。

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

/// TODO(P4): SendInput 实现。
pub struct SendInputInjector;

impl InputInjector for SendInputInjector {
    fn inject(&mut self, _event: &InputEvent) -> Result<(), String> {
        Err("windows: SendInput injection not implemented yet (P4)".into())
    }
}
