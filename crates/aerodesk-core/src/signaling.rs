//! WSS 信令客户端（crate::protocol::signal 消息）。

use std::net::TcpStream;
use std::time::Duration;

use crate::protocol::signal::{Role, SignalMessage};
use tracing::info;
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
    normalize_signal_url_with_tls(input, true)
}

/// 归一化信令服务器地址，无显式协议时按 `default_tls` 选择 `ws://` / `wss://`（#504）。
///
/// 与 [`normalize_signal_url`] 的唯一区别在非回环裸地址的默认协议：
/// `default_tls=false` 时补 `ws://`（自建明文信令服务器场景）。回环地址始终补
/// `ws://`（loopback 上 TLS 无意义）；已带 `://` 的显式协议输入不受开关影响。
pub fn normalize_signal_url_with_tls(input: &str, default_tls: bool) -> String {
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
        let loopback = matches!(host, "localhost" | "127.0.0.1" | "::1" | "0:0:0:0:0:0:0:1");
        let scheme = if !loopback && default_tls {
            "wss"
        } else {
            "ws"
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

/// `WsSignalClient::recv_timeout` 的错误分类。
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum WsRecvError {
    /// 在超时窗口内没有读到完整消息。
    Timeout,
    /// 连接已关闭、解析失败或其它不可恢复错误。
    Closed(String),
}

impl std::fmt::Display for WsRecvError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            WsRecvError::Timeout => write!(f, "signal receive timed out"),
            WsRecvError::Closed(message) => write!(f, "signal receive failed: {message}"),
        }
    }
}

impl std::error::Error for WsRecvError {}

impl From<std::io::Error> for WsRecvError {
    fn from(value: std::io::Error) -> Self {
        WsRecvError::Closed(value.to_string())
    }
}

impl WsSignalClient {
    /// 连接信令服务器（ws:// 或 wss://），地址会自动归一化（补协议/路径）。
    pub fn connect(url: &str) -> Result<Self, tungstenite::Error> {
        let (ws, _) = connect(&normalize_signal_url(url))?;
        // #539：写超时 5s 兜底——对端读循环阻塞（等消息/已断开）时 ws.send
        // 会无限阻塞，卡死 presence 循环（超时拒绝、心跳全停，形成互等死锁）。
        match ws.get_ref() {
            MaybeTlsStream::Plain(stream) => {
                let _ = stream.set_write_timeout(Some(Duration::from_secs(5)));
            }
            MaybeTlsStream::Rustls(stream) => {
                let _ = stream.sock.set_write_timeout(Some(Duration::from_secs(5)));
            }
            _ => {}
        }
        Ok(Self { ws, peer_id: None })
    }

    /// 返回服务器分配的 peer_id（Join 成功后可用）。
    pub fn peer_id(&self) -> Option<&str> {
        self.peer_id.as_deref()
    }

    /// 发送任意信令消息（呼叫/响铃/挂断等扩展消息）。
    pub fn send_signal(&mut self, msg: SignalMessage) -> Result<(), String> {
        self.send(msg)
    }

    /// #539：发送信令心跳（`SignalMessage::Ping`）——连接保活 + 驱动服务端
    /// 读循环的发送队列 drain。服务端 session_loop 的阻塞 next() 收到任一消息
    /// 即返回，得以执行发送队列的 drain（呼叫等可靠消息经队列投递后送达）。
    /// 注：ws 层 Ping 帧会被 rouille 自动回 Pong 且不返回消息，无法驱动 drain。
    pub fn send_ping(&mut self) -> Result<(), String> {
        self.send_signal(crate::protocol::signal::SignalMessage::Ping)
    }

    /// 加入房间，返回服务器分配的 peer_id 与 TURN 配置。
    ///
    /// 多 PoP（#146）：服务器返回 [`SignalMessage::Redirect`] 时自动断开并
    /// 重连到目标 PoP 的信令地址（最多 `MAX_REDIRECTS` 跳，防环）。
    pub fn join(
        &mut self,
        room: &str,
        role: Role,
        auth_token: Option<&str>,
    ) -> Result<(String, Option<crate::protocol::signal::TurnConfig>), String> {
        const MAX_REDIRECTS: usize = 3;
        for hop in 0..=MAX_REDIRECTS {
            self.send(SignalMessage::Join {
                room: room.into(),
                role,
                auth_token: auth_token.map(|s| s.to_string()),
                // #467：声明会在 offer/answer 通道 DCEP 完成后发 signal_ready，
                // SFU 据此门控重协商，消除 DCEP 窗口内的 offer 丢失竞态。
                dc_ready: true,
            })?;
            match self.recv()? {
                SignalMessage::Redirect { url, .. } => {
                    info!("signal redirect (hop {hop}/{MAX_REDIRECTS}): following to {url}");
                    let (ws, _) = connect(&normalize_signal_url(&url))
                        .map_err(|e| format!("redirect connect {url}: {e}"))?;
                    self.ws = ws;
                    self.peer_id = None;
                }
                SignalMessage::Joined { peer_id, turn, .. } => {
                    self.peer_id = Some(peer_id.clone());
                    return Ok((peer_id, turn));
                }
                SignalMessage::Error { message } => return Err(message),
                other => return Err(format!("unexpected join response: {other:?}")),
            }
        }
        Err("too many signal redirects".into())
    }

    /// 发送 SDP offer，等待 SFU answer（阻塞）。
    pub fn exchange_description(&mut self, sdp: &str) -> Result<String, String> {
        let peer_id = self.peer_id.clone().ok_or("not joined")?;
        self.send(SignalMessage::Description {
            from: peer_id,
            to: "sfu".into(),
            description: sdp.into(),
        })?;
        // #539：viewer 发起的 Call 响应（CallRejected/CallRinging 等）与 SDP
        // answer 交织到达（e2e 无被叫端时 CallRejected 先回）——跳过继续等。
        loop {
            match self.recv()? {
                SignalMessage::Description { description, .. } => return Ok(description),
                SignalMessage::Error { message } => return Err(message),
                other => {
                    tracing::debug!("skip during description exchange: {other:?}");
                }
            }
        }
    }

    /// 带读取超时地读取一条消息。
    ///
    /// 用于常驻 presence 连接：`Timeout` 表示等待窗口内无消息（连接仍可能存活），
    /// 上层可借此轮询停止标志；`Closed` 表示连接已不可用，应进入重连。
    pub fn recv_timeout(&mut self, timeout: Duration) -> Result<SignalMessage, WsRecvError> {
        self.set_read_timeout(Some(timeout))?;
        loop {
            match self.ws.read() {
                Ok(Message::Text(t)) => {
                    return serde_json::from_str(&t)
                        .map_err(|e| WsRecvError::Closed(e.to_string()));
                }
                Ok(Message::Binary(b)) => {
                    return serde_json::from_slice(&b)
                        .map_err(|e| WsRecvError::Closed(e.to_string()));
                }
                Ok(_) => continue,
                Err(tungstenite::Error::Io(e))
                    if matches!(
                        e.kind(),
                        std::io::ErrorKind::WouldBlock | std::io::ErrorKind::TimedOut
                    ) =>
                {
                    return Err(WsRecvError::Timeout);
                }
                Err(e) => return Err(WsRecvError::Closed(e.to_string())),
            }
        }
    }

    fn set_read_timeout(&mut self, timeout: Option<Duration>) -> std::io::Result<()> {
        match self.ws.get_mut() {
            MaybeTlsStream::Plain(stream) => stream.set_read_timeout(timeout),
            MaybeTlsStream::Rustls(stream) => stream.sock.set_read_timeout(timeout),
            _ => Err(std::io::Error::new(
                std::io::ErrorKind::Unsupported,
                "unsupported websocket stream type for read timeout",
            )),
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
    use super::{normalize_signal_url, normalize_signal_url_with_tls};

    #[test]
    fn normalize_with_tls_toggle_picks_default_scheme() {
        // 裸地址 + TLS 关 → ws://（自建明文服务器场景，#504）
        assert_eq!(
            normalize_signal_url_with_tls("129.226.150.174:14703", false),
            "ws://129.226.150.174:14703/ws"
        );
        // 裸地址 + TLS 开 → wss://
        assert_eq!(
            normalize_signal_url_with_tls("signal.aerodesk.io", true),
            "wss://signal.aerodesk.io/ws"
        );
        // 回环地址不受开关影响，始终 ws://（loopback 上 TLS 无意义）
        assert_eq!(
            normalize_signal_url_with_tls("127.0.0.1:3003", true),
            "ws://127.0.0.1:3003/ws"
        );
        assert_eq!(
            normalize_signal_url_with_tls("localhost:3003", false),
            "ws://localhost:3003/ws"
        );
        // 显式 scheme 优先于开关（两个方向都保留）
        assert_eq!(
            normalize_signal_url_with_tls("wss://h:3003", false),
            "wss://h:3003/ws"
        );
        assert_eq!(
            normalize_signal_url_with_tls("ws://h:3003/ws", true),
            "ws://h:3003/ws"
        );
        // 空串原样返回
        assert_eq!(normalize_signal_url_with_tls("", true), "");
        assert_eq!(normalize_signal_url_with_tls("  ", false), "");
    }

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
