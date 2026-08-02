//! 输入注入（骨架）。
//!
//! 真机路径：AccessibilityService 接收 input 通道 JSON 事件 →
//! `dispatchGesture`（触控）/ `injectInputEvent`（API 33+）→ 系统输入。
//! 权限：`BIND_ACCESSIBILITY_SERVICE` + `canPerformGestures`。

/// 输入事件（与 aerodesk-protocol::input 对齐，后续自动生成）。
#[derive(Debug, Clone)]
pub enum InputEvent {
    MouseMove { x: f32, y: f32 },
    MouseButton { x: f32, y: f32, button: u8, down: bool },
    Wheel { dx: f32, dy: f32 },
    Key { code: u32, down: bool },
}

/// 注入抽象（被控端）。
pub trait InputInjector {
    fn inject(&mut self, event: &InputEvent) -> Result<(), String>;
}

/// TODO(P3): AccessibilityService 实现。
pub struct AccessibilityInjector;

impl InputInjector for AccessibilityInjector {
    fn inject(&mut self, _event: &InputEvent) -> Result<(), String> {
        Err("android: AccessibilityService injection not implemented yet (P3)".into())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn events_are_send() {
        fn assert_send<T: Send>() {}
        assert_send::<InputEvent>();
    }
}
