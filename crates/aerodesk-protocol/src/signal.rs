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
        /// #467：客户端能力声明——会在 offer/answer 通道 DCEP 完成后发送
        /// `{"type":"signal_ready"}` 就绪包，SFU 据此门控重协商时机。
        /// 旧客户端不带该字段（serde 缺省 false），SFU 走不门控的兼容路径。
        #[serde(default)]
        dc_ready: bool,
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
    /// 呼叫目标设备（`target` 为设备 ID/房间名，即 presence 常驻房间）。
    ///
    /// #453：viewer 先通过该消息触发被叫端按需出流；媒体仍沿用现有
    /// [`SignalMessage::Description`] 的 offer/answer 路径。
    Call {
        /// 呼叫方在信令服务器中的 peer_id。
        from: String,
        /// 被叫设备 ID（房间名）。
        target: String,
        /// 呼叫唯一标识，由呼叫方生成。
        call_id: String,
        /// 被叫端响铃/发布超时（毫秒）。`None` 表示使用被叫端默认值。
        timeout_ms: Option<u64>,
    },
    /// 被叫端已收到呼叫并开始响铃。
    CallRinging {
        from: String,
        to: String,
        call_id: String,
    },
    /// 被叫端已接受呼叫（presence 自动接听或用户确认）。
    CallAccepted {
        from: String,
        to: String,
        call_id: String,
    },
    /// 呼叫被拒绝（被叫忙、离线或主动拒绝）。
    CallRejected {
        from: String,
        to: String,
        call_id: String,
        reason: Option<String>,
    },
    /// 任一方挂断呼叫。
    Hangup {
        from: String,
        to: String,
        call_id: String,
        reason: Option<String>,
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
            dc_ready: true,
        };
        let back: SignalMessage =
            serde_json::from_str(&serde_json::to_string(&msg).unwrap()).unwrap();
        assert_eq!(msg, back);
    }

    /// #467：旧客户端 Join 不带 dc_ready 字段 → 缺省 false，双向兼容。
    #[test]
    fn join_without_dc_ready_defaults_false() {
        let json = r#"{"type":"join","room":"r","role":"viewer","auth_token":null}"#;
        let msg: SignalMessage = serde_json::from_str(json).unwrap();
        let SignalMessage::Join { dc_ready, .. } = msg else {
            panic!("not a Join")
        };
        assert!(!dc_ready);
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

    #[test]
    fn roundtrip_call_and_friends() {
        let messages = vec![
            SignalMessage::Call {
                from: "caller-peer".into(),
                target: "device-1".into(),
                call_id: "call-1".into(),
                timeout_ms: Some(30_000),
            },
            SignalMessage::Call {
                from: "caller-peer".into(),
                target: "device-1".into(),
                call_id: "call-2".into(),
                timeout_ms: None,
            },
            SignalMessage::CallRinging {
                from: "device-1-peer".into(),
                to: "caller-peer".into(),
                call_id: "call-1".into(),
            },
            SignalMessage::CallAccepted {
                from: "device-1-peer".into(),
                to: "caller-peer".into(),
                call_id: "call-1".into(),
            },
            SignalMessage::CallRejected {
                from: "signal".into(),
                to: "caller-peer".into(),
                call_id: "call-1".into(),
                reason: Some("target offline".into()),
            },
            SignalMessage::Hangup {
                from: "caller-peer".into(),
                to: "device-1-peer".into(),
                call_id: "call-1".into(),
                reason: Some("done".into()),
            },
        ];

        for msg in messages {
            let json = serde_json::to_string(&msg).unwrap();
            let back: SignalMessage = serde_json::from_str(&json).unwrap();
            assert_eq!(msg, back);
        }
    }
}
