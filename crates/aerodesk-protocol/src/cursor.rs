//! 远程光标位置协议（#75）：被控端 → 观看端，经 data channel（label "cursor"）。
//!
//! 坐标与输入协议一致：相对屏幕 0..1 归一化，避免分辨率差异。

use serde::{Deserialize, Serialize};

/// 光标位置（归一化 0..1）。
#[derive(Debug, Clone, Copy, PartialEq, Serialize, Deserialize)]
pub struct CursorPos {
    pub x: f64,
    pub y: f64,
}

impl CursorPos {
    pub fn new(x: f64, y: f64) -> Self {
        Self { x, y }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn roundtrip_json() {
        let pos = CursorPos::new(0.5, 0.25);
        let json = serde_json::to_string(&pos).unwrap();
        assert!(json.contains("\"x\":0.5"));
        let back: CursorPos = serde_json::from_str(&json).unwrap();
        assert_eq!(back, pos);
    }
}
