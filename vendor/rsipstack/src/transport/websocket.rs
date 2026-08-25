use crate::sip::SipMessage;
use crate::{
    transport::{
        connection::{TransportSender, KEEPALIVE_REQUEST, KEEPALIVE_RESPONSE},
        sip_addr::SipAddr,
        stream::StreamConnection,
        transport_layer::TransportLayerInnerRef,
        SipConnection, TransportEvent,
    },
    Result,
};
use futures_util::{SinkExt, StreamExt};
use socket2::{Domain, Protocol, Socket, Type};
use std::{fmt, net::SocketAddr, pin::Pin, sync::Arc};
use tokio::{net::TcpListener, sync::Mutex};
use tokio_tungstenite::{
    connect_async,
    tungstenite::{
        client::IntoClientRequest,
        handshake::server::{Request, Response},
        protocol::{Message, Role},
    },
};
use tokio_util::sync::CancellationToken;
use tracing::{debug, warn};

// Define a type alias for the WebSocket sink to make the code more readable
// aerodesk patch：sink/read 类型擦除为 dyn（Sink/Stream trait object）——
// 服务器端 WSS（TLS accept 后的 server::TlsStream）与客户端/Plain 流统一
// 进同一 WebSocketInner（钨钢 MaybeTlsStream 无 server 流变体）。
type WsSink = Pin<Box<dyn futures_util::sink::Sink<Message, Error = tungstenite::Error> + Send>>;
type WsRead =
    Pin<Box<dyn futures_util::stream::Stream<Item = tungstenite::Result<Message>> + Send>>;

/// aerodesk patch：服务器端 accept 后的流统一包装（Plain TCP 或 TLS）。
/// 钨钢 accept_hdr_async 要求单类型 S: AsyncRead + AsyncWrite + Unpin——TcpStream
/// 与 tokio_rustls server::TlsStream 不能共用一个 dyn（两个非 auto trait），
/// 手写委托（Poll 转发两臂）。
enum AcceptedStream {
    Plain(tokio::net::TcpStream),
    Tls(tokio_rustls::server::TlsStream<tokio::net::TcpStream>),
}

impl tokio::io::AsyncRead for AcceptedStream {
    fn poll_read(
        mut self: Pin<&mut Self>,
        cx: &mut std::task::Context<'_>,
        buf: &mut tokio::io::ReadBuf<'_>,
    ) -> std::task::Poll<std::io::Result<()>> {
        match &mut *self {
            Self::Plain(s) => Pin::new(s).poll_read(cx, buf),
            Self::Tls(s) => Pin::new(s).poll_read(cx, buf),
        }
    }
}

impl tokio::io::AsyncWrite for AcceptedStream {
    fn poll_write(
        mut self: Pin<&mut Self>,
        cx: &mut std::task::Context<'_>,
        buf: &[u8],
    ) -> std::task::Poll<std::io::Result<usize>> {
        match &mut *self {
            Self::Plain(s) => Pin::new(s).poll_write(cx, buf),
            Self::Tls(s) => Pin::new(s).poll_write(cx, buf),
        }
    }
    fn poll_flush(
        mut self: Pin<&mut Self>,
        cx: &mut std::task::Context<'_>,
    ) -> std::task::Poll<std::io::Result<()>> {
        match &mut *self {
            Self::Plain(s) => Pin::new(s).poll_flush(cx),
            Self::Tls(s) => Pin::new(s).poll_flush(cx),
        }
    }
    fn poll_shutdown(
        mut self: Pin<&mut Self>,
        cx: &mut std::task::Context<'_>,
    ) -> std::task::Poll<std::io::Result<()>> {
        match &mut *self {
            Self::Plain(s) => Pin::new(s).poll_shutdown(cx),
            Self::Tls(s) => Pin::new(s).poll_shutdown(cx),
        }
    }
}

/// RFC 3174 SHA-1（Sec-WebSocket-Accept 计算用；避免新增依赖）。
fn sha1(data: &[u8]) -> [u8; 20] {
    let mut msg = data.to_vec();
    let bit_len = (data.len() as u64).wrapping_mul(8);
    msg.push(0x80);
    while msg.len() % 64 != 56 {
        msg.push(0);
    }
    msg.extend_from_slice(&bit_len.to_be_bytes());
    let mut h: [u32; 5] = [0x67452301, 0xEFCDAB89, 0x98BADCFE, 0x10325476, 0xC3D2E1F0];
    for chunk in msg.chunks(64) {
        let mut w = [0u32; 80];
        for (i, c) in chunk.chunks(4).enumerate() {
            w[i] = u32::from_be_bytes([c[0], c[1], c[2], c[3]]);
        }
        for i in 16..80 {
            w[i] = (w[i - 3] ^ w[i - 8] ^ w[i - 14] ^ w[i - 16]).rotate_left(1);
        }
        let (mut a, mut b, mut c, mut d, mut e) = (h[0], h[1], h[2], h[3], h[4]);
        for (i, &wi) in w.iter().enumerate() {
            let (f, k) = match i {
                0..=19 => ((b & c) | ((!b) & d), 0x5A827999u32),
                20..=39 => (b ^ c ^ d, 0x6ED9EBA1),
                40..=59 => ((b & c) | (b & d) | (c & d), 0x8F1BBCDC),
                _ => (b ^ c ^ d, 0xCA62C1D6),
            };
            let tmp = a
                .rotate_left(5)
                .wrapping_add(f)
                .wrapping_add(e)
                .wrapping_add(k)
                .wrapping_add(wi);
            e = d;
            d = c;
            c = b.rotate_left(30);
            b = a;
            a = tmp;
        }
        h[0] = h[0].wrapping_add(a);
        h[1] = h[1].wrapping_add(b);
        h[2] = h[2].wrapping_add(c);
        h[3] = h[3].wrapping_add(d);
        h[4] = h[4].wrapping_add(e);
    }
    let mut out = [0u8; 20];
    for (i, v) in h.iter().enumerate() {
        out[i * 4..i * 4 + 4].copy_from_slice(&v.to_be_bytes());
    }
    out
}

/// RFC 4648 base64 编码（Sec-WebSocket-Accept 计算用；避免新增依赖）。
fn base64_encode(data: &[u8]) -> String {
    const T: &[u8] = b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789+/";
    let mut out = String::new();
    for chunk in data.chunks(3) {
        let b = [chunk[0], *chunk.get(1).unwrap_or(&0), *chunk.get(2).unwrap_or(&0)];
        out.push(T[(b[0] >> 2) as usize] as char);
        out.push(T[(((b[0] & 0x03) << 4) | (b[1] >> 4)) as usize] as char);
        out.push(if chunk.len() > 1 {
            T[(((b[1] & 0x0F) << 2) | (b[2] >> 6)) as usize] as char
        } else {
            '='
        });
        out.push(if chunk.len() > 2 { T[(b[2] & 0x3F) as usize] as char } else { '=' });
    }
    out
}

/// aerodesk patch：手动 WebSocket 服务器握手（RFC 6455 §4.2.2）。
/// 背景：钨钢 accept_hdr_async 与 tokio_rustls server 流（TlsStream）组合时
/// 握手解析异常（收到的 HTTP 数据完整连续且 httparse 1.3.4 单测 PASS，钨钢
/// 仍报 invalid token，见 #553 验收发现）；手动握手绕开钨钢握手解析，
/// 帧读取仍用钨钢 WebSocketStream（Plain 分支证明钨钢流读取工作正常）。
/// 客户端在收到 101 前不会发送帧数据，读循环精确停在头部边界（无 tail 丢失）。
async fn manual_accept(
    mut stream: AcceptedStream,
    remote: &SipAddr,
) -> std::result::Result<tokio_tungstenite::WebSocketStream<AcceptedStream>, String> {
    use tokio::io::{AsyncReadExt, AsyncWriteExt};
    // 1. 读 HTTP 头部至 \r\n\r\n（上限 16KB 防攻击）。
    let mut header = Vec::new();
    let mut buf = [0u8; 512];
    loop {
        let n = stream
            .read(&mut buf)
            .await
            .map_err(|e| format!("读头部失败: {e}"))?;
        if n == 0 {
            return Err("客户端提前关闭".into());
        }
        header.extend_from_slice(&buf[..n]);
        if header.len() > 16 * 1024 {
            return Err("HTTP 头部超限".into());
        }
        if header.windows(4).any(|w| w == b"\r\n\r\n") {
            break;
        }
    }
    let text = String::from_utf8_lossy(&header);
    let lines: Vec<&str> = text.split("\r\n").collect();
    let request_line = *lines.first().ok_or("无请求行")?;
    if !request_line.starts_with("GET ") {
        return Err(format!("非 GET: {request_line}"));
    }
    let lower = |l: &&str| l.to_ascii_lowercase();
    let key = lines
        .iter()
        .find(|l| lower(l).starts_with("sec-websocket-key:"))
        .and_then(|l| l.split_once(':').map(|(_, v)| v.trim()))
        .ok_or("缺 Sec-WebSocket-Key")?
        .to_string();
    let upgrade_ok = lines.iter().any(|l| lower(l).starts_with("upgrade: websocket"));
    let connection_ok = lines.iter().any(|l| {
        lower(l).starts_with("connection:") && lower(l).contains("upgrade")
    });
    if !upgrade_ok || !connection_ok {
        return Err("缺 Upgrade/Connection 头".into());
    }
    let req_proto = lines
        .iter()
        .find(|l| lower(l).starts_with("sec-websocket-protocol:"))
        .and_then(|l| l.split_once(':').map(|(_, v)| v.trim().to_string()));
    // 2. Sec-WebSocket-Accept = base64(SHA1(key + GUID))（RFC 6455 §4.2.2）。
    let mut hasher_input = key.into_bytes();
    hasher_input.extend_from_slice(b"258EAFA5-E914-47DA-95CA-C5AB0DC85B11");
    let accept = base64_encode(&sha1(&hasher_input));
    // 3. 回 101（含请求的 sip 子协议，等价钨钢 callback 行为）。
    let mut resp = format!(
        "HTTP/1.1 101 Switching Protocols\r\nUpgrade: websocket\r\nConnection: Upgrade\r\nSec-WebSocket-Accept: {accept}\r\n"
    );
    if let Some(p) = req_proto.as_deref().filter(|p| *p == "sip") {
        resp.push_str(&format!("Sec-WebSocket-Protocol: {p}\r\n"));
    }
    resp.push_str("\r\n");
    stream
        .write_all(resp.as_bytes())
        .await
        .map_err(|e| format!("写 101 失败: {e}"))?;
    let _ = stream.flush().await;
    debug!(%remote, "手动 WS 握手完成（101）");
    Ok(tokio_tungstenite::WebSocketStream::from_raw_socket(stream, Role::Server, None).await)
}

// WebSocket Listener Connection Structure
pub struct WebSocketListenerConnectionInner {
    pub local_addr: SipAddr,
    pub external: Option<SipAddr>,
    pub is_secure: bool,
}

#[derive(Clone)]
pub struct WebSocketListenerConnection {
    pub inner: Arc<WebSocketListenerConnectionInner>,
}

impl WebSocketListenerConnection {
    pub async fn new(
        local_addr: SocketAddr,
        external: Option<SocketAddr>,
        is_secure: bool,
    ) -> Result<Self> {
        let transport_type = if is_secure {
            crate::sip::transport::Transport::Wss
        } else {
            crate::sip::transport::Transport::Ws
        };

        let inner = WebSocketListenerConnectionInner {
            local_addr: SipAddr {
                r#type: Some(transport_type),
                addr: local_addr.into(),
            },
            external: external.map(|addr| SipAddr {
                r#type: Some(transport_type),
                addr: addr.into(),
            }),
            is_secure,
        };
        Ok(WebSocketListenerConnection {
            inner: Arc::new(inner),
        })
    }

    pub async fn serve_listener(
        &self,
        transport_layer_inner: TransportLayerInnerRef,
    ) -> Result<()> {
        let local = self.inner.local_addr.get_socketaddr()?;
        let domain = if local.is_ipv6() {
            Domain::IPV6
        } else {
            Domain::IPV4
        };
        let socket = Socket::new(domain, Type::STREAM, Some(Protocol::TCP))?;
        if let Err(e) = socket.set_reuse_address(true) {
            warn!(error = %e, "failed to set SO_REUSEADDR on WebSocket listener");
        }
        socket.set_nonblocking(true)?;
        socket.bind(&local.into())?;
        socket.listen(128)?;
        let listener = TcpListener::from_std(socket.into())?;
        let transport_type = if self.inner.is_secure {
            crate::sip::transport::Transport::Wss
        } else {
            crate::sip::transport::Transport::Ws
        };
        let listener_local_addr = self.get_addr().clone();

        // aerodesk patch（WSS TLS 封装）：rsipstack 0.6.4 的 secure WebSocket
        // listener 未做 TLS（MaybeTlsStream::Plain 写死）——"SIP/WSS" 实际是
        // 明文 WS，Digest 凭据与 SDP 明文暴露。此修复按 TransportLayer 已配置
        // 的 TlsConfig 构建 TlsAcceptor（与 TlsListenerConnection 同源），
        // is_secure 时先 TLS 握手再 WS 升级。
        let acceptor = if self.inner.is_secure {
            let config = transport_layer_inner.tls_config().ok_or_else(|| {
                crate::Error::Error(
                    "WSS listener: TLS config 未设置（须先 set_tls_config）".to_string(),
                )
            })?;
            Some(
                crate::transport::tls::TlsListenerConnection::create_acceptor(&config)
                    .await?
                    .0,
            )
        } else {
            None
        };

        debug!(local = %self.inner.local_addr, "Starting WebSocket listener");
        tokio::spawn(async move {
            loop {
                let (stream, remote_addr) = match listener.accept().await {
                    Ok((stream, remote_addr)) => (stream, remote_addr),
                    Err(e) => {
                        warn!(error = ?e, "Failed to accept WebSocket connection");
                        continue;
                    }
                };
                if !transport_layer_inner.is_whitelisted(remote_addr.ip()).await {
                    debug!(remote = %remote_addr, "websocket connection rejected by whitelist");
                    continue;
                }

                debug!(remote = %remote_addr, "New WebSocket connection");

                let remote_addr = SipAddr {
                    r#type: Some(transport_type),
                    addr: remote_addr.into(),
                };
                let transport_layer_inner_ref = transport_layer_inner.clone();
                let local_addr = listener_local_addr.clone();
                let acceptor = acceptor.clone();
                tokio::spawn(async move {
                    // aerodesk patch：is_secure 时先做 TLS 握手（钨钢标准做法：
                    // TLS accept 后的流直接喂 accept_hdr_async，无需 MaybeTlsStream
                    // 包装——该枚举无 server 流变体），再 WS 升级。
                    let mut stream: AcceptedStream = match &acceptor {
                        Some(acceptor) => match acceptor.accept(stream).await {
                            Ok(tls_stream) => {
                                debug!(remote = %remote_addr, "TLS handshake ok (WSS)");
                                AcceptedStream::Tls(tls_stream)
                            }
                            Err(e) => {
                                warn!(error = %e, remote = %remote_addr, "WSS TLS handshake failed");
                                return;
                            }
                        },
                        None => AcceptedStream::Plain(stream),
                    };

                    // Accept the WebSocket connection with custom header handling
                    // aerodesk patch：TLS（WSS）分支走手动握手——钨钢 accept_hdr_async
                    // 与 tokio_rustls server 流组合时握手解析异常（数据完整连续且
                    // httparse 单测 PASS，钨钢仍报 invalid token，见 #553 验收发现）；
                    // Plain（WS）分支保持钨钢原生握手（已验证工作）。
                    let callback = |req: &Request, mut response: Response| {
                        // Check if client requested 'sip' subprotocol
                        if let Some(protocols) = req.headers().get("sec-websocket-protocol") {
                            if let Ok(protocols_str) = protocols.to_str() {
                                if protocols_str.contains("sip") {
                                    // Add the 'sip' subprotocol to response
                                    response
                                        .headers_mut()
                                        .insert("sec-websocket-protocol", "sip".parse().unwrap());
                                }
                            }
                        }
                        Ok(response)
                    };
                    let ws_stream = if acceptor.is_some() {
                        match manual_accept(stream, &remote_addr).await {
                            Ok(ws) => ws,
                            Err(e) => {
                                warn!(error = %e, remote = %remote_addr, "WSS 手动握手失败");
                                return;
                            }
                        }
                    } else {
                        match tokio_tungstenite::accept_hdr_async(stream, callback).await {
                            Ok(ws) => ws,
                            Err(e) => {
                                warn!(error = %e, remote = %remote_addr, "Error upgrading to WebSocket");
                                return;
                            }
                        }
                    };

                    // aerodesk patch：split 结果 Box::pin 擦除到 dyn 类型别名
                    //（服务器端 TlsStream / 客户端 MaybeTlsStream 统一）。
                    let (sink, read) = ws_stream.split();
                    let ws_sink: WsSink = Box::pin(sink);
                    let ws_read: WsRead = Box::pin(read);
                    let connection = WebSocketConnection {
                        inner: Arc::new(WebSocketInner {
                            local_addr,
                            remote_addr,
                            ws_sink: Mutex::new(ws_sink),
                            ws_read: Mutex::new(Some(ws_read)),
                        }),
                        cancel_token: Some(transport_layer_inner_ref.cancel_token.child_token()),
                    };
                    let sip_connection = SipConnection::WebSocket(connection.clone());
                    let connection_addr = connection.get_addr().clone();
                    transport_layer_inner_ref.add_connection(sip_connection.clone());
                    debug!(?connection_addr, "new websocket connection");
                });
            }
        });
        Ok(())
    }

    pub fn get_addr(&self) -> &SipAddr {
        if let Some(external) = &self.inner.external {
            external
        } else {
            &self.inner.local_addr
        }
    }

    pub async fn close(&self) -> Result<()> {
        Ok(())
    }
}

impl fmt::Display for WebSocketListenerConnection {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        let transport = if self.inner.is_secure { "WSS" } else { "WS" };
        write!(f, "{} Listener {}", transport, self.get_addr())
    }
}

impl fmt::Debug for WebSocketListenerConnection {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        fmt::Display::fmt(self, f)
    }
}

pub struct WebSocketInner {
    pub local_addr: SipAddr,
    pub remote_addr: SipAddr,
    pub ws_sink: Mutex<WsSink>,
    pub ws_read: Mutex<Option<WsRead>>,
}

#[derive(Clone)]
pub struct WebSocketConnection {
    pub inner: Arc<WebSocketInner>,
    pub cancel_token: Option<CancellationToken>,
}

impl WebSocketConnection {
    pub async fn connect(
        remote: &SipAddr,
        cancel_token: Option<CancellationToken>,
    ) -> Result<Self> {
        Self::connect_with_path(remote, None, cancel_token).await
    }

    /// Connect to a WebSocket server with an optional URL path.
    ///
    /// `path` defaults to `/` when `None` is passed, allowing callers to
    /// specify a custom path such as `/ws` for rustpbx-style endpoints.
    pub async fn connect_with_path(
        remote: &SipAddr,
        path: Option<&str>,
        cancel_token: Option<CancellationToken>,
    ) -> Result<Self> {
        let scheme = match remote.r#type {
            Some(crate::sip::transport::Transport::Wss) => "wss",
            _ => "ws",
        };

        let host = match &remote.addr.host {
            crate::sip::Host::Domain(domain) => domain.to_string(),
            crate::sip::Host::IpAddr(ip) => ip.to_string(),
        };

        let port = remote.addr.port.as_ref().map_or(5060, |p| p.value());
        let ws_path = path.unwrap_or("/");
        let url = format!("{}://{}:{}{}", scheme, host, port, ws_path);
        let mut request = url.into_client_request()?;
        request
            .headers_mut()
            .insert("sec-websocket-protocol", "sip".parse().unwrap());

        let (ws_stream, _) = connect_async(request).await?;
        let local_addr = SipAddr {
            r#type: Some(
                remote
                    .r#type
                    .unwrap_or(crate::sip::transport::Transport::Ws),
            ),
            addr: ws_stream.get_ref().get_ref().local_addr()?.into(),
        };
        // aerodesk patch：split 结果 Box::pin 擦除到 dyn 类型别名（与服务器端一致）。
        let (sink, stream) = ws_stream.split();
        let ws_sink: WsSink = Box::pin(sink);
        let ws_read: WsRead = Box::pin(stream);

        let connection = WebSocketConnection {
            inner: Arc::new(WebSocketInner {
                local_addr,
                remote_addr: remote.clone(),
                ws_sink: Mutex::new(ws_sink),
                ws_read: Mutex::new(Some(ws_read)),
            }),
            cancel_token,
        };

        debug!(
            local = %connection.get_addr(),
            remote = %remote,
            "Created WebSocket client connection"
        );

        Ok(connection)
    }
    pub fn cancel_token(&self) -> Option<CancellationToken> {
        self.cancel_token.clone()
    }
}

#[async_trait::async_trait]
impl StreamConnection for WebSocketConnection {
    fn get_addr(&self) -> &SipAddr {
        &self.inner.local_addr
    }

    fn get_remote_addr(&self) -> &SipAddr {
        &self.inner.remote_addr
    }

    async fn send_message(&self, msg: SipMessage) -> Result<()> {
        let data = msg.to_string();
        let mut sink = self.inner.ws_sink.lock().await;
        debug!(dest = %self.inner.remote_addr, raw_message = %data, "websocket send");
        sink.send(Message::Text(data.into())).await?;
        Ok(())
    }

    async fn send_raw(&self, data: &[u8]) -> Result<()> {
        let mut sink = self.inner.ws_sink.lock().await;
        sink.send(Message::Binary(data.to_vec().into())).await?;
        Ok(())
    }

    async fn serve_loop(&self, sender: TransportSender) -> Result<()> {
        let sip_connection = SipConnection::WebSocket(self.clone());

        let remote_addr = self.inner.remote_addr.clone();
        let mut ws_read = match self.inner.ws_read.lock().await.take() {
            Some(ws_read) => ws_read,
            None => {
                warn!(src = %remote_addr, "WebSocket connection already closed");
                return Ok(());
            }
        };
        while let Some(msg) = ws_read.next().await {
            match msg {
                Ok(Message::Text(text)) => {
                    debug!(src = %remote_addr, raw_message = %text, "websocket message received");
                    match SipMessage::try_from(text.as_str()) {
                        Ok(sip_msg) => {
                            let remote_socket_addr = remote_addr.get_socketaddr()?;
                            let sip_msg = SipConnection::update_msg_received(
                                sip_msg,
                                remote_socket_addr,
                                remote_addr.r#type.unwrap_or_default(),
                            )?;

                            if let Err(e) = sender.send(TransportEvent::Incoming(
                                sip_msg,
                                sip_connection.clone(),
                                remote_addr.clone(),
                            )) {
                                warn!(error = ?e, src = %remote_addr, "Error sending incoming message");
                                break;
                            }
                        }
                        Err(e) => {
                            warn!(error = %e, src = %remote_addr, raw_message = %text, "Error parsing SIP message");
                        }
                    }
                }
                Ok(Message::Binary(bin)) => {
                    if bin == *KEEPALIVE_REQUEST {
                        if let Err(e) = self.send_raw(KEEPALIVE_RESPONSE).await {
                            warn!(error = ?e, src = %remote_addr, "Error sending keepalive response");
                        }
                        continue;
                    }
                    debug!(src = %remote_addr, "websocket binary message received");
                    match SipMessage::try_from(bin) {
                        Ok(sip_msg) => {
                            if let Err(e) = sender.send(TransportEvent::Incoming(
                                sip_msg,
                                sip_connection.clone(),
                                remote_addr.clone(),
                            )) {
                                warn!(error = ?e, src = %remote_addr, "Error sending incoming message");
                                break;
                            }
                        }
                        Err(e) => {
                            warn!(error = %e, src = %remote_addr, "Error parsing SIP message from binary");
                        }
                    }
                }
                Ok(Message::Ping(data)) => {
                    let mut sink = self.inner.ws_sink.lock().await;
                    if let Err(e) = sink.send(Message::Pong(data)).await {
                        warn!(error = %e, src = %remote_addr, "Error sending pong");
                        break;
                    }
                }
                Ok(Message::Close(_)) => {
                    debug!(src = %remote_addr, "WebSocket connection closed by peer");
                    break;
                }
                Err(e) => {
                    warn!(error = %e, src = %remote_addr, "WebSocket error");
                    break;
                }
                _ => {}
            }
        }

        debug!(src = %remote_addr, "WebSocket serve_loop exiting");
        Ok(())
    }

    async fn close(&self) -> Result<()> {
        let mut sink = self.inner.ws_sink.lock().await;
        sink.send(Message::Close(None)).await?;
        Ok(())
    }
}

impl fmt::Display for WebSocketConnection {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        let transport = match self.inner.remote_addr.r#type {
            Some(crate::sip::transport::Transport::Wss) => "WSS",
            _ => "WS",
        };
        write!(
            f,
            "{} {} -> {}",
            transport, self.inner.local_addr, self.inner.remote_addr
        )
    }
}

impl fmt::Debug for WebSocketConnection {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        fmt::Display::fmt(self, f)
    }
}
