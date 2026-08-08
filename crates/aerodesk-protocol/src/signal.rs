//! 信令消息协议（客户端 ↔ 信令服务）。
//!
//! 信令服务在 P1 独立化；类型先行，供 aerodesk-sfu / aerodesk-core / Web 共用。

use serde::{Deserialize, Serialize};

/// 客户端角色。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Role {
    /// 被控端（采集 + 编码 + 输入注入）。
    Publisher,
    /// 观看端（解码渲染 + 输入捕获）。
    Viewer,
}

/// TURN 服务器配置（信令下发，ICE 失败时兜底）。
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct TurnConfig {
    pub urls: Vec<String>,
    pub username: String,
    pub credential: String,
}

/// 信令消息。
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum SignalMessage {
    /// 客户端加入房间。
    Join {
        room: String,
        role: Role,
        auth_token: Option<String>,
    },
    /// 加入成功：房间内已有 peer 与 TURN 配置。
    Joined {
        peer_id: String,
        peers: Vec<PeerInfo>,
        turn: Option<TurnConfig>,
    },
    /// 多 PoP：房间钉在其它 PoP，需重连到 `url`（#146）。
    Redirect {
        pop: String,
        url: String,
        reason: Option<String>,
    },
    /// WebRTC 会话描述（offer/answer，JSON 字符串）。
    Description {
        from: String,
        to: String,
        description: String,
    },
    /// ICE candidate。
    IceCandidate {
        from: String,
        to: String,
        candidate: String,
    },
    /// 请求关键帧 / 码率提示（可选扩展）。
    PeerLeft {
        peer_id: String,
    },
    Error {
        message: String,
    },
}

/// 房间内 peer 信息。
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct PeerInfo {
    pub peer_id: String,
    pub role: Role,
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn roundtrip_join() {
        let msg = SignalMessage::Join {
            room: "room-1".into(),
            role: Role::Publisher,
            auth_token: Some("token".into()),
        };
        let back: SignalMessage =
            serde_json::from_str(&serde_json::to_string(&msg).unwrap()).unwrap();
        assert_eq!(msg, back);
    }

    #[test]
    fn roundtrip_redirect() {
        let msg = SignalMessage::Redirect {
            pop: "pop-eu".into(),
            url: "wss://eu.example.com:443/ws".into(),
            reason: Some("room pinned to pop-eu".into()),
        };
        let back: SignalMessage =
            serde_json::from_str(&serde_json::to_string(&msg).unwrap()).unwrap();
        assert_eq!(msg, back);
    }

    #[test]
    fn roundtrip_joined_with_turn() {
        let msg = SignalMessage::Joined {
            peer_id: "p1".into(),
            peers: vec![PeerInfo {
                peer_id: "p2".into(),
                role: Role::Viewer,
            }],
            turn: Some(TurnConfig {
                urls: vec!["turn:turn.example.com:3478".into()],
                username: "u".into(),
                credential: "c".into(),
            }),
        };
        let back: SignalMessage =
            serde_json::from_str(&serde_json::to_string(&msg).unwrap()).unwrap();
        assert_eq!(msg, back);
    }
}
