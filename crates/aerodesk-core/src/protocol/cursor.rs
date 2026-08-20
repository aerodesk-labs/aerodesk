//! 远程光标位置协议（#75）：被控端 → 观看端，经 data channel（label "cursor"）。
//!
//! 坐标与输入协议一致：相对屏幕 0..1 归一化，避免分辨率差异。

use serde::{Deserialize, Serialize};

/// 光标位置（归一化 0..1）。
#[derive(Debug, Clone, Copy, PartialEq, Serialize, Deserialize)]
pub struct CursorPos {
    pub x: f64,
    pub y: f64,
    /// 发送端墙钟（unix ms，#8 端到端延迟测量；旧端无此字段视为 0）。
    #[serde(default)]
    pub sent_ms: u64,
}

impl CursorPos {
    pub fn new(x: f64, y: f64) -> Self {
        Self { x, y, sent_ms: 0 }
    }

    /// 附带发送时间戳（端到端延迟测量用）。
    pub fn with_sent_ms(mut self, sent_ms: u64) -> Self {
        self.sent_ms = sent_ms;
        self
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn roundtrip_json() {
        let pos = CursorPos::new(0.5, 0.25).with_sent_ms(12345);
        let json = serde_json::to_string(&pos).unwrap();
        assert!(json.contains("\"x\":0.5"));
        assert!(json.contains("\"sent_ms\":12345"));
        let back: CursorPos = serde_json::from_str(&json).unwrap();
        assert_eq!(back, pos);
        assert_eq!(back.sent_ms, 12345);
    }

    #[test]
    fn old_json_without_sent_ms_parses_to_zero() {
        // 向后兼容：旧发布端消息无 sent_ms 字段。
        let back: CursorPos = serde_json::from_str("{\"x\":0.1,\"y\":0.2}").unwrap();
        assert_eq!(back.sent_ms, 0);
    }
}
