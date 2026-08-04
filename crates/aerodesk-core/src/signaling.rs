//! WSS 信令客户端（aerodesk-protocol::signal 消息）。

use std::net::TcpStream;

use aerodesk_protocol::signal::{Role, SignalMessage};
use tungstenite::stream::MaybeTlsStream;
use tungstenite::{Message, WebSocket, connect};

/// 信令客户端。
pub struct WsSignalClient {
    ws: WebSocket<MaybeTlsStream<TcpStream>>,
    peer_id: Option<String>,
}

/// 归一化信令服务器地址：补全协议前缀与 `/ws` 路径。
///
/// - 未带 `://` 时自动补协议：回环地址（localhost/127.0.0.1/::1）用 `ws://`，
///   其余默认 `wss://`（用户也可显式写 `ws://host:port`）。
/// - 未带路径时补 `/ws`（服务器 WebSocket 端点统一挂在此路径下，连根路径会被回 200 而卡住）。
/// - 已带协议/路径的输入原样保留。
pub fn normalize_signal_url(input: &str) -> String {
    let input = input.trim();
    if input.is_empty() {
        return input.to_string();
    }
    let with_scheme = if input.contains("://") {
        input.to_string()
    } else {
        let host = if let Some(rest) = input.strip_prefix('[') {
            rest.split(']').next().unwrap_or("")
        } else {
            input.split(':').next().unwrap_or("")
        };
        let scheme = if matches!(host, "localhost" | "127.0.0.1" | "::1" | "0:0:0:0:0:0:0:1") {
            "ws"
        } else {
            "wss"
        };
        format!("{scheme}://{input}")
    };
    // 路径处理：仅当无路径（或只有根路径 `/`）时补 `/ws`；
    // 已有任何显式路径则原样保留（与 docstring 一致），
    // 避免 `contains("/ws")` 子串误判（如 /wsfoo）与尾部斜杠产生 `//ws`。
    let Some(rest) = with_scheme.split_once("://").map(|(_, r)| r) else {
        return with_scheme;
    };
    match rest.find('/') {
        // 无路径：补 /ws
        None => format!("{with_scheme}/ws"),
        // 只有尾部根斜杠：去掉后补 /ws（避免 //ws）
        Some(i) if i == rest.len() - 1 => format!("{}/ws", &with_scheme[..with_scheme.len() - 1]),
        // 已有路径（含 /ws 或其它）：原样保留
        Some(_) => with_scheme,
    }
}

impl WsSignalClient {
    /// 连接信令服务器（ws:// 或 wss://），地址会自动归一化（补协议/路径）。
    pub fn connect(url: &str) -> Result<Self, tungstenite::Error> {
        let (ws, _) = connect(&normalize_signal_url(url))?;
        Ok(Self { ws, peer_id: None })
    }

    /// 加入房间，返回服务器分配的 peer_id 与 TURN 配置。
    pub fn join(
        &mut self,
        room: &str,
        role: Role,
        auth_token: Option<&str>,
    ) -> Result<(String, Option<aerodesk_protocol::signal::TurnConfig>), String> {
        self.send(SignalMessage::Join {
            room: room.into(),
            role,
            auth_token: auth_token.map(|s| s.to_string()),
        })?;
        match self.recv()? {
            SignalMessage::Joined { peer_id, turn, .. } => {
                self.peer_id = Some(peer_id.clone());
                Ok((peer_id, turn))
            }
            SignalMessage::Error { message } => Err(message),
            other => Err(format!("unexpected join response: {other:?}")),
        }
    }

    /// 发送 SDP offer，等待 SFU answer（阻塞）。
    pub fn exchange_description(&mut self, sdp: &str) -> Result<String, String> {
        let peer_id = self.peer_id.clone().ok_or("not joined")?;
        self.send(SignalMessage::Description {
            from: peer_id,
            to: "sfu".into(),
            description: sdp.into(),
        })?;
        match self.recv()? {
            SignalMessage::Description { description, .. } => Ok(description),
            SignalMessage::Error { message } => Err(message),
            other => Err(format!("unexpected description response: {other:?}")),
        }
    }

    /// 读取一条消息（阻塞）。
    pub fn recv(&mut self) -> Result<SignalMessage, String> {
        loop {
            match self.ws.read() {
                Ok(Message::Text(t)) => return serde_json::from_str(&t).map_err(|e| e.to_string()),
                Ok(Message::Binary(b)) => {
                    return serde_json::from_slice(&b).map_err(|e| e.to_string());
                }
                Ok(_) => continue,
                Err(e) => return Err(e.to_string()),
            }
        }
    }

    fn send(&mut self, msg: SignalMessage) -> Result<(), String> {
        let text = serde_json::to_string(&msg).map_err(|e| e.to_string())?;
        self.ws
            .send(Message::Text(text.into()))
            .map_err(|e| e.to_string())
    }
}

#[cfg(test)]
mod tests {
    use super::normalize_signal_url;

    #[test]
    fn normalize_adds_scheme_and_path() {
        // 回环地址 → ws://
        assert_eq!(
            normalize_signal_url("127.0.0.1:3003"),
            "ws://127.0.0.1:3003/ws"
        );
        assert_eq!(
            normalize_signal_url("localhost:3003"),
            "ws://localhost:3003/ws"
        );
        assert_eq!(normalize_signal_url("[::1]:3003"), "ws://[::1]:3003/ws");
        // 非回环 → wss://（默认安全）
        assert_eq!(
            normalize_signal_url("signal.aerodesk.io"),
            "wss://signal.aerodesk.io/ws"
        );
        assert_eq!(
            normalize_signal_url("signal.aerodesk.io:3001"),
            "wss://signal.aerodesk.io:3001/ws"
        );
        // 已带协议/路径 → 原样保留
        assert_eq!(
            normalize_signal_url("wss://signal.aerodesk.io/ws"),
            "wss://signal.aerodesk.io/ws"
        );
        assert_eq!(
            normalize_signal_url("ws://127.0.0.1:3003"),
            "ws://127.0.0.1:3003/ws"
        );
        // 尾部根斜杠 → 补 /ws 且不产生双斜杠
        assert_eq!(
            normalize_signal_url("127.0.0.1:3003/"),
            "ws://127.0.0.1:3003/ws"
        );
        assert_eq!(
            normalize_signal_url("ws://127.0.0.1:3003/"),
            "ws://127.0.0.1:3003/ws"
        );
        // 已有非 /ws 显式路径 → 原样保留（不追加，不因子串误判）
        assert_eq!(
            normalize_signal_url("ws://h:3003/signaling"),
            "ws://h:3003/signaling"
        );
        assert_eq!(
            normalize_signal_url("ws://h:3003/wsfoo"),
            "ws://h:3003/wsfoo"
        );
        // 空串
        assert_eq!(normalize_signal_url(""), "");
        assert_eq!(normalize_signal_url("   "), "");
    }
}
