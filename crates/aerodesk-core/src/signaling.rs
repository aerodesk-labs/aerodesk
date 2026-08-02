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

impl WsSignalClient {
    /// 连接信令服务器（ws:// 或 wss://）。
    pub fn connect(url: &str) -> Result<Self, tungstenite::Error> {
        let (ws, _) = connect(url)?;
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
