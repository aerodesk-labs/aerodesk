//! 输入注入（骨架）。
//!
//! 风险项：OH_Input 注入需要系统权限（`INTERCEPT_INPUT_EVENT`），
//! 普通应用不可用；企业签名/系统应用通道评估中。先做观看端。

/// 输入事件（与 aerodesk-protocol::input 对齐）。
#[derive(Debug, Clone)]
pub enum InputEvent {
    MouseMove { x: f32, y: f32 },
    MouseButton { x: f32, y: f32, button: u8, down: bool },
    Wheel { dx: f32, dy: f32 },
    Key { code: u32, down: bool },
}

/// TODO(P4): OH_Input 实现（权限评估后）。
pub struct OhosInputInjector;

impl OhosInputInjector {
    pub fn inject(&mut self, _event: &InputEvent) -> Result<(), String> {
        Err("ohos: OH_Input injection not implemented yet (P4, 权限评估中)".into())
    }
}
