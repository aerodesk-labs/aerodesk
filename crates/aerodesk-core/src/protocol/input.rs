//! 输入事件协议（观看端 → 被控端）。
//!
//! 经 SFU 的 `input` 数据通道转发。版本字段保证协议演进兼容。

use serde::{Deserialize, Serialize};

/// 当前协议版本。
pub const INPUT_PROTOCOL_VERSION: u32 = 1;

/// 输入事件封装（带协议版本）。
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct InputFrame {
    pub version: u32,
    pub seq: u64,
    pub timestamp_ms: u64,
    pub event: InputEvent,
}

/// 输入事件。
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum InputEvent {
    /// 绝对坐标（相对屏幕/画面，0..1 归一化，避免分辨率差异）。
    MouseMove {
        x: f64,
        y: f64,
    },
    MouseButton {
        button: MouseButton,
        state: ButtonState,
        x: f64,
        y: f64,
    },
    Wheel {
        x: f64,
        y: f64,
        delta_x: f64,
        delta_y: f64,
    },
    Key {
        /// 平台无关键码（如 "KeyA"、"Enter"），注入层映射到平台键码。
        code: String,
        state: ButtonState,
        modifiers: Modifiers,
    },
    Touch {
        touch_id: u32,
        action: TouchAction,
        x: f64,
        y: f64,
    },
    ClipboardText(String),
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum MouseButton {
    Left,
    Middle,
    Right,
    Back,
    Forward,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ButtonState {
    Pressed,
    Released,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum TouchAction {
    Down,
    Move,
    Up,
    Cancel,
}

/// 修饰键位掩码。
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct Modifiers {
    pub ctrl: bool,
    pub shift: bool,
    pub alt: bool,
    pub meta: bool,
}

impl InputFrame {
    pub fn new(seq: u64, event: InputEvent) -> Self {
        Self {
            version: INPUT_PROTOCOL_VERSION,
            seq,
            timestamp_ms: 0,
            event,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn roundtrip_json() {
        let frame = InputFrame::new(42, InputEvent::MouseMove { x: 0.5, y: 0.25 });
        let json = serde_json::to_string(&frame).unwrap();
        let back: InputFrame = serde_json::from_str(&json).unwrap();
        assert_eq!(frame, back);
        assert!(json.contains("\"type\":\"mouse_move\""));
    }

    #[test]
    fn roundtrip_key() {
        let frame = InputFrame::new(
            1,
            InputEvent::Key {
                code: "KeyA".into(),
                state: ButtonState::Pressed,
                modifiers: Modifiers {
                    ctrl: true,
                    ..Default::default()
                },
            },
        );
        let back: InputFrame =
            serde_json::from_str(&serde_json::to_string(&frame).unwrap()).unwrap();
        assert_eq!(frame, back);
    }

    #[test]
    fn protocol_version_is_1() {
        assert_eq!(INPUT_PROTOCOL_VERSION, 1);
    }
}
