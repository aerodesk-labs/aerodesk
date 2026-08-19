//! 输入注入（骨架）。
//!
//! 真机路径：AccessibilityService 接收 input 通道 JSON 事件 →
//! `dispatchGesture`（触控）/ `injectInputEvent`（API 33+）→ 系统输入。
//! 权限：`BIND_ACCESSIBILITY_SERVICE` + `canPerformGestures`。

use aerodesk_core::protocol::input::InputEvent;

/// TODO(P3): AccessibilityService 实现。
pub struct AccessibilityInjector;

impl aerodesk_core::platform::InputInjector for AccessibilityInjector {
    type Error = String;

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
