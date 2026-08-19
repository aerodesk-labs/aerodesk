//! 聊天消息协议（#458）：经 WebRTC data channel（label `chat`）双向传输。
//!
//! 文本消息使用 JSON 编码，字段命名沿用 input/cursor 的 snake_case 约定；
//! 后续如需图片/富文本可扩展消息类型，但通道 label 保持 `chat`。

use serde::{Deserialize, Serialize};

/// 聊天数据通道 label。
pub const CHAT_CHANNEL: &str = "chat";

/// 单条文本聊天消息（最少必要字段：发送者/文本/时间戳）。
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ChatMessage {
    /// 发送者标识（显示名或 peer id）。
    pub sender: String,
    /// 文本内容（UTF-8）。
    pub text: String,
    /// 发送端墙钟（unix 毫秒）。
    #[serde(default)]
    pub timestamp_ms: u64,
}

impl ChatMessage {
    pub fn new(sender: impl Into<String>, text: impl Into<String>, timestamp_ms: u64) -> Self {
        Self {
            sender: sender.into(),
            text: text.into(),
            timestamp_ms,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn roundtrip_json() {
        let msg = ChatMessage::new("alice", "hello 你好", 1_723_766_400_123);
        let json = serde_json::to_string(&msg).unwrap();
        assert!(json.contains("\"sender\":\"alice\""));
        assert!(json.contains("\"text\":\"hello 你好\""));
        assert!(json.contains("\"timestamp_ms\":1723766400123"));
        let back: ChatMessage = serde_json::from_str(&json).unwrap();
        assert_eq!(back, msg);
    }

    #[test]
    fn missing_timestamp_defaults_to_zero() {
        let back: ChatMessage = serde_json::from_str(r#"{"sender":"bob","text":"hi"}"#).unwrap();
        assert_eq!(back.sender, "bob");
        assert_eq!(back.text, "hi");
        assert_eq!(back.timestamp_ms, 0);
    }
}
