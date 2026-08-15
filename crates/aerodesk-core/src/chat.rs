//! 聊天消息收发（#458）：复用 [`crate::Endpoint`] 的 data channel 机制。
//!
//! 发送经 `"chat"` label 的 JSON 文本数据通道；接收方在拿到
//! [`crate::ClientEvent::ChannelData`] 后，先按 [`crate::Endpoint::channel_label`]
//! 判断 label 为 [`aerodesk_protocol::chat::CHAT_CHANNEL`]，再调用 [`parse_chat_data`] 解析。

use crate::{ClientEvent, Endpoint};
use aerodesk_protocol::chat::{CHAT_CHANNEL, ChatMessage};

/// 发送一条文本聊天消息；返回 data channel 是否成功接收写入。
pub fn send_text(
    endpoint: &mut Endpoint,
    sender: impl Into<String>,
    text: impl Into<String>,
) -> bool {
    let message = ChatMessage::new(sender, text, now_unix_ms());
    send_message(endpoint, &message)
}

/// 发送已构造的 [`ChatMessage`]。
pub fn send_message(endpoint: &mut Endpoint, message: &ChatMessage) -> bool {
    let Ok(json) = serde_json::to_vec(message) else {
        return false;
    };
    endpoint.send_channel_data(CHAT_CHANNEL, false, &json)
}

/// 解析 `"chat"` 数据通道收到的 JSON 文本。
pub fn parse_chat_data(data: &[u8]) -> Result<ChatMessage, serde_json::Error> {
    serde_json::from_slice(data)
}

/// 从 [`ClientEvent`] 中提取 chat 通道消息。
///
/// 非 chat 通道事件返回 `Ok(None)`；chat 通道数据解析失败返回 `Err`。
/// 调用方可沿用现有 `poll_event()` 循环，无需自行查 label。
pub fn from_event(
    event: &ClientEvent,
    endpoint: &Endpoint,
) -> Result<Option<ChatMessage>, serde_json::Error> {
    match event {
        ClientEvent::ChannelData(cid, _, data)
            if endpoint.channel_label(*cid).as_deref() == Some(CHAT_CHANNEL) =>
        {
            parse_chat_data(data).map(Some)
        }
        _ => Ok(None),
    }
}

fn now_unix_ms() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| u64::try_from(d.as_millis()).unwrap_or(0))
        .unwrap_or(0)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parse_chat_data_roundtrip() {
        let msg = ChatMessage::new("alice", "hello 你好", 1_723_766_400_123);
        let json = serde_json::to_vec(&msg).unwrap();
        let parsed = parse_chat_data(&json).unwrap();
        assert_eq!(parsed, msg);
    }

    #[test]
    fn send_text_without_chat_channel_returns_false() {
        let mut endpoint = Endpoint::new();
        assert!(!send_text(&mut endpoint, "alice", "hello"));
    }

    #[test]
    fn send_message_returns_false_when_channel_not_open() {
        let mut endpoint = Endpoint::new();
        let msg = ChatMessage::new("alice", "hello", 123);
        assert!(!send_message(&mut endpoint, &msg));
    }

    #[test]
    fn from_event_ignores_non_chat_events() {
        let endpoint = Endpoint::new();
        let event = ClientEvent::IceConnected;
        let parsed = from_event(&event, &endpoint).unwrap();
        assert!(parsed.is_none());
    }
}
