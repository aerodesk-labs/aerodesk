//! TURN 客户端（RFC 5766 长时凭证）——应用层 TURN 传输（#157 M2；#199 TCP/TLS）。
//!
//! M1：`allocate` 完成 Allocate 拿到 relayed 地址（401→REALM/NONCE 重试）。
//! M2：`TurnTransport`——Allocate 后持久化 realm/nonce，按需 CreatePermission +
//! ChannelBind + ChannelData 收发、Data indication 解析、定期 Refresh。
//! #199：传输抽象 `TurnIo`——UDP / TCP（RFC 4571 帧）/ TLS（rustls）统一读写。
//! 与 str0m 的集成在 `media_socket`（应用层双路径收发，不 patch str0m fork）。

use std::collections::HashMap;
use std::io::{self, Read, Write};
use std::net::{IpAddr, Ipv4Addr, SocketAddr, TcpStream, ToSocketAddrs, UdpSocket};
use std::path::Path;
use std::sync::Arc;
use std::time::{Duration, Instant};

use aerodesk_protocol::turn::codec::*;

/// Refresh 间隔（allocation 默认 lifetime 600s，按 RFC 建议在到期前刷新）。
const REFRESH_INTERVAL: Duration = Duration::from_secs(300);
/// 关键请求（permission/channel bind）超时。
const REQUEST_TIMEOUT: Duration = Duration::from_secs(2);
/// 快速请求（refresh）预算：失败下轮重试，不阻塞媒体泵。
const FAST_REQUEST_BUDGET: Duration = Duration::from_millis(30);
/// TCP/TLS 连接读超时（泵粒度）。
const TCP_READ_TIMEOUT: Duration = Duration::from_millis(10);
/// 响应匹配时最多跳过的无关包（Data indication 等）。
const MAX_RESPONSE_SKIP: usize = 8;

/// 连接 IO trait 对象（明文 TCP 或 rustls TlsStream）。
trait ReadWriteSend: Read + Write + Send {}
impl<T: Read + Write + Send> ReadWriteSend for T {}

/// 传输载体。
enum TurnIo {
    Udp(UdpSocket),
    Tcp {
        stream: Box<dyn ReadWriteSend>,
        local: Option<SocketAddr>,
    },
}

impl TurnIo {
    fn set_read_timeout(&self, t: Option<Duration>) -> io::Result<()> {
        match self {
            TurnIo::Udp(s) => s.set_read_timeout(t),
            // TCP/TLS 在 connect 时已固定 10ms（泵粒度），动态调整无意义。
            TurnIo::Tcp { .. } => Ok(()),
        }
    }

    fn local_addr(&self) -> io::Result<SocketAddr> {
        match self {
            TurnIo::Udp(s) => s.local_addr(),
            TurnIo::Tcp { local, .. } => {
                local.ok_or_else(|| io::Error::other("no local addr for TCP TURN"))
            }
        }
    }
}

/// TURN URL 解析结果。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum TurnProto {
    Udp,
    Tcp,
    Tls,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TurnUrl {
    pub host: String,
    pub port: u16,
    pub proto: TurnProto,
}

/// 解析 TURN URL：`turn:host:port?transport=udp|tcp` 与 `turns:host:port?transport=tcp`。
pub fn parse_turn_url(url: &str) -> Option<TurnUrl> {
    let (rest, tls) = if let Some(r) = url.strip_prefix("turns:") {
        (r, true)
    } else {
        (url.strip_prefix("turn:")?, false)
    };
    let (hostport, query) = rest.split_once('?').unwrap_or((rest, ""));
    let transport = query
        .split('&')
        .find_map(|q| q.strip_prefix("transport="))
        .unwrap_or("udp");
    let proto = if tls {
        TurnProto::Tls
    } else {
        match transport {
            "tcp" => TurnProto::Tcp,
            _ => TurnProto::Udp,
        }
    };
    let default_port = if tls { 5349 } else { 3478 };
    let (host, port) = match hostport.rsplit_once(':') {
        Some((h, p)) => (h, p.parse().ok()?),
        None => (hostport, default_port),
    };
    if host.is_empty() {
        return None;
    }
    Some(TurnUrl {
        host: host.to_string(),
        port,
        proto,
    })
}

fn resolve_server(host: &str, port: u16) -> Option<SocketAddr> {
    (host, port).to_socket_addrs().ok()?.find(|a| a.is_ipv4())
}

/// 由信令下发的 TURN 配置建立传输（失败返回 None：直连兜底）。
/// 按 URL 顺序尝试（SFU 默认 udp,tcp,turns），任一成功即用；`TURN_TLS_CA`（PEM）追加 TLS 根。
pub fn setup_turn(
    turn: &aerodesk_protocol::signal::TurnConfig,
    loopback: bool,
) -> Option<TurnTransport> {
    let bind_ip: IpAddr = if loopback {
        Ipv4Addr::LOCALHOST.into()
    } else {
        Ipv4Addr::UNSPECIFIED.into()
    };
    let ca = std::env::var("TURN_TLS_CA")
        .ok()
        .map(std::path::PathBuf::from);
    for url in &turn.urls {
        let Some(parsed) = parse_turn_url(url) else {
            continue;
        };
        let Some(server) = resolve_server(&parsed.host, parsed.port) else {
            continue;
        };
        let res = match parsed.proto {
            TurnProto::Udp => TurnTransport::connect(
                server,
                &turn.username,
                &turn.credential,
                bind_ip,
                Duration::from_secs(3),
            ),
            TurnProto::Tcp => TurnTransport::connect_tcp(
                &parsed.host,
                server,
                &turn.username,
                &turn.credential,
                false,
                None,
                Duration::from_secs(3),
            ),
            TurnProto::Tls => TurnTransport::connect_tcp(
                &parsed.host,
                server,
                &turn.username,
                &turn.credential,
                true,
                ca.as_deref(),
                Duration::from_secs(3),
            ),
        };
        match res {
            Ok(tt) => {
                tracing::info!("TURN allocation ok ({url}): relayed={}", tt.relayed_addr());
                return Some(tt);
            }
            Err(e) => {
                tracing::warn!("TURN url {url} failed (fallback direct): {e}");
            }
        }
    }
    None
}

/// TURN 传输（UDP/TCP/TLS allocation + ChannelData 数据通路）。
///
/// 线程模型：与直连 UDP 同线程轮询；`recv_packet` 非阻塞（读超时返回
/// `Ok(None)`），`ensure_peer`/`refresh` 使用同步短事务（deadline 轮询）。
pub struct TurnTransport {
    io: TurnIo,
    server: SocketAddr,
    /// TCP 帧缓冲（半包累积，RFC 4571）。
    frame_buf: Vec<u8>,
    username: String,
    password: String,
    realm: String,
    nonce: String,
    relayed_addr: SocketAddr,
    /// peer 地址 → channel 号。
    channels: HashMap<SocketAddr, u16>,
    /// channel 号 → peer 地址（解析入站 ChannelData）。
    reverse: HashMap<u16, SocketAddr>,
    next_channel: u16,
    last_refresh: Instant,
}

impl TurnTransport {
    /// 发起 TURN Allocate（UDP），成功返回持久化的传输（realm/nonce 已保留）。
    pub fn connect(
        server: SocketAddr,
        username: &str,
        password: &str,
        bind_ip: IpAddr,
        timeout: Duration,
    ) -> Result<Self, String> {
        let socket = UdpSocket::bind((bind_ip, 0)).map_err(|e| format!("turn bind: {e}"))?;
        socket
            .set_read_timeout(Some(timeout))
            .map_err(|e| format!("turn set_read_timeout: {e}"))?;
        let mut t = TurnTransport {
            io: TurnIo::Udp(socket),
            server,
            frame_buf: Vec::new(),
            username: username.to_string(),
            password: password.to_string(),
            realm: String::new(),
            nonce: String::new(),
            relayed_addr: SocketAddr::new(IpAddr::V4(Ipv4Addr::UNSPECIFIED), 0),
            channels: HashMap::new(),
            reverse: HashMap::new(),
            next_channel: CHANNEL_BASE,
            last_refresh: Instant::now(),
        };
        t.allocate_flow()?;
        Ok(t)
    }

    /// 发起 TURN Allocate（TCP/TLS，RFC 4571 帧）。`hostname` 用于 TLS SNI/证书校验；
    /// `ca` 为可选的 PEM 根证书文件（追加到系统根）。
    pub fn connect_tcp(
        hostname: &str,
        server: SocketAddr,
        username: &str,
        password: &str,
        tls: bool,
        ca: Option<&Path>,
        timeout: Duration,
    ) -> Result<Self, String> {
        // #216：TCP connect 用调用方超时（默认 3s），避免 OS 默认超时（~85s）
        // 阻塞整条腿——桥/客户端在 TURN TCP 不可达时快速回退直连。
        let stream = TcpStream::connect_timeout(&server, timeout)
            .map_err(|e| format!("turn tcp connect: {e}"))?;
        stream.set_nodelay(true).ok();
        // 握手/allocate 用长超时；allocate 成功后切 10ms 泵超时。
        stream
            .set_read_timeout(Some(timeout))
            .map_err(|e| format!("turn tcp set_read_timeout: {e}"))?;
        let pump_handle = stream.try_clone().ok(); // 共享 fd：allocate 后切泵超时
        let local = stream.local_addr().ok();
        let io = if tls {
            let cfg = build_tls_config(ca)?;
            let sni = rustls::pki_types::ServerName::try_from(hostname.to_string())
                .map_err(|e| format!("bad TLS server name {hostname}: {e}"))?;
            let conn =
                rustls::ClientConnection::new(cfg, sni).map_err(|e| format!("tls conn: {e}"))?;
            let mut tls_stream = rustls::StreamOwned::new(conn, stream);
            // 显式完成 TLS 握手（10ms 读超时会中断 lazy 握手，见 #199）。
            tls_stream
                .conn
                .complete_io(&mut tls_stream.sock)
                .map_err(|e| format!("tls handshake: {e}"))?;
            TurnIo::Tcp {
                stream: Box::new(tls_stream),
                local,
            }
        } else {
            TurnIo::Tcp {
                stream: Box::new(stream),
                local,
            }
        };
        let mut t = TurnTransport {
            io,
            server,
            frame_buf: Vec::new(),
            username: username.to_string(),
            password: password.to_string(),
            realm: String::new(),
            nonce: String::new(),
            relayed_addr: SocketAddr::new(IpAddr::V4(Ipv4Addr::UNSPECIFIED), 0),
            channels: HashMap::new(),
            reverse: HashMap::new(),
            next_channel: CHANNEL_BASE,
            last_refresh: Instant::now(),
        };
        t.allocate_flow()?;
        if let Some(h) = pump_handle {
            let _ = h.set_read_timeout(Some(TCP_READ_TIMEOUT));
        }
        Ok(t)
    }

    /// Allocate 流程：无凭证 → 401（REALM/NONCE）→ 带凭证重试。
    fn allocate_flow(&mut self) -> Result<(), String> {
        // 1) 无凭证 Allocate → 401 拿 REALM/NONCE
        let txid = random_txid();
        let req = build_stun(MSG_ALLOCATE, &allocate_attrs(), txid, None)?;
        self.send_raw(&req)?;
        let (resp, _) = self.recv_matching(txid, REQUEST_TIMEOUT)?;
        let (realm, nonce, err) = parse_common(&resp)?;
        if err != Some(401) {
            return Err(format!("allocate expected 401, got error={err:?}"));
        }
        self.realm = realm.ok_or("no realm in 401")?;
        self.nonce = nonce.ok_or("no nonce in 401")?;

        // 2) 带长时凭证重试（438 stale nonce 由 request 自动重试）
        let (resp2, _) = self.request(MSG_ALLOCATE, allocate_attrs())?;
        let relayed_attr = find_attr(&resp2, ATTR_XOR_RELAYED_ADDRESS).ok_or_else(|| {
            format!(
                "no XOR-RELAYED-ADDRESS in allocate response: method={:#06x} attrs={:?} err={:?}",
                stun_method(&resp2),
                parse_attrs(&resp2)
                    .iter()
                    .map(|(t, _)| format!("{t:#06x}"))
                    .collect::<Vec<_>>(),
                parse_common(&resp2).ok().and_then(|(_, _, e)| e),
            )
        })?;
        self.relayed_addr = parse_xor_addr(&relayed_attr).ok_or("bad XOR-RELAYED-ADDRESS")?;
        Ok(())
    }

    /// TURN 服务器分配的 relayed 地址（对端发往该地址即中继到本端）。
    pub fn relayed_addr(&self) -> SocketAddr {
        self.relayed_addr
    }

    /// allocation 连接本地地址。
    pub fn local_addr(&self) -> io::Result<SocketAddr> {
        self.io.local_addr()
    }

    pub fn set_read_timeout(&self, t: Option<Duration>) -> io::Result<()> {
        self.io.set_read_timeout(t)
    }

    /// 确保与 peer 建立 TURN 通路（CreatePermission + ChannelBind），返回 channel。
    pub fn ensure_peer(&mut self, peer: SocketAddr) -> Result<u16, String> {
        if let Some(&c) = self.channels.get(&peer) {
            return Ok(c);
        }
        let xpeer = encode_xor_peer(peer);
        let (resp, _) = self.request(
            MSG_CREATE_PERMISSION,
            vec![(ATTR_XOR_PEER_ADDRESS, xpeer.clone())],
        )?;
        expect_success(&resp, MSG_CREATE_PERMISSION)?;
        if self.next_channel > CHANNEL_MAX {
            return Err("TURN channels exhausted".into());
        }
        let chan = self.next_channel;
        self.next_channel += 1;
        let (resp, _) = self.request(
            MSG_CHANNEL_BIND,
            vec![
                (ATTR_CHANNEL_NUMBER, chan.to_be_bytes().to_vec()),
                (ATTR_XOR_PEER_ADDRESS, xpeer),
            ],
        )?;
        expect_success(&resp, MSG_CHANNEL_BIND)?;
        self.channels.insert(peer, chan);
        self.reverse.insert(chan, peer);
        Ok(chan)
    }

    /// 向 peer 发送媒体/ICE 数据（首包前自动建 permission + channel）。
    pub fn send_to(&mut self, peer: SocketAddr, payload: &[u8]) -> Result<(), String> {
        let chan = self.ensure_peer(peer)?;
        let mut pkt = Vec::with_capacity(4 + payload.len());
        pkt.extend_from_slice(&chan.to_be_bytes());
        pkt.extend_from_slice(&(payload.len() as u16).to_be_bytes());
        pkt.extend_from_slice(payload);
        self.send_raw(&pkt)
    }

    /// 接收一帧对端数据（Data indication / ChannelData 还原为 (peer, 载荷长度)）。
    /// 无数据（读超时）返回 `Ok(None)`；控制类响应内部消化后返回 `Ok(None)`。
    pub fn recv_packet(&mut self, buf: &mut [u8]) -> io::Result<Option<(SocketAddr, usize)>> {
        let n = match self.recv_raw(buf) {
            Ok(Some(n)) => n,
            Ok(None) => return Ok(None),
            Err(e) => return Err(e),
        };
        let pkt = &buf[..n];
        // ChannelData：channel 0x4000-0x7FFF（首两 bit 01）。
        if pkt.len() >= 4 && (pkt[0] & 0xc0) == 0x40 {
            let chan = u16::from_be_bytes([pkt[0], pkt[1]]);
            let len = u16::from_be_bytes([pkt[2], pkt[3]]) as usize;
            let Some(&peer) = self.reverse.get(&chan) else {
                return Ok(None); // 未绑定 channel：丢弃
            };
            // 防恶意 length 越界：不超过实际收包与缓冲区。
            let n2 = len.min(n.saturating_sub(4)).min(buf.len());
            buf.copy_within(4..4 + n2, 0);
            return Ok(Some((peer, n2)));
        }
        if pkt.len() < 20 {
            return Ok(None);
        }
        let ty = u16::from_be_bytes([pkt[0], pkt[1]]);
        if ty == MSG_DATA_INDICATION {
            let attrs = parse_attrs(pkt);
            let peer = attrs
                .iter()
                .find(|(t, _)| *t == ATTR_XOR_PEER_ADDRESS)
                .and_then(|(_, v)| parse_xor_addr(v));
            let data = attrs
                .iter()
                .find(|(t, _)| *t == ATTR_DATA)
                .map(|(_, v)| v.as_slice());
            if let (Some(peer), Some(data)) = (peer, data) {
                let n2 = data.len().min(buf.len());
                buf[..n2].copy_from_slice(&data[..n2]);
                return Ok(Some((peer, n2)));
            }
        }
        Ok(None) // 其它 STUN（refresh/channel bind 响应等）：调用方事务消化
    }

    /// 到期时发送 Refresh 维持 allocation。非阻塞语义：快速失败，
    /// 失败返回 Err（调用方下轮重试，不阻塞媒体泵）。
    pub fn refresh_if_due(&mut self, now: Instant) -> Result<(), String> {
        if now.duration_since(self.last_refresh) < REFRESH_INTERVAL {
            return Ok(());
        }
        let (resp, _) = self.request_fast(
            MSG_REFRESH,
            vec![(ATTR_LIFETIME, 600u32.to_be_bytes().to_vec())],
        )?;
        expect_success(&resp, MSG_REFRESH)?;
        self.last_refresh = now;
        Ok(())
    }

    // ---------- 传输原语 ----------

    /// 发送一帧（UDP 直发；TCP/TLS 加 2 字节长度前缀）。
    fn send_raw(&mut self, bytes: &[u8]) -> Result<(), String> {
        match &mut self.io {
            TurnIo::Udp(s) => s
                .send_to(bytes, self.server)
                .map(|_| ())
                .map_err(|e| format!("turn send: {e}")),
            TurnIo::Tcp { stream, .. } => {
                let mut framed = Vec::with_capacity(2 + bytes.len());
                framed.extend_from_slice(&(bytes.len() as u16).to_be_bytes());
                framed.extend_from_slice(bytes);
                stream
                    .write_all(&framed)
                    .map_err(|e| format!("turn tcp send: {e}"))
            }
        }
    }

    /// 接收一帧（UDP 原始报文；TCP/TLS 从帧缓冲切出一帧）。
    fn recv_raw(&mut self, buf: &mut [u8]) -> io::Result<Option<usize>> {
        match &mut self.io {
            TurnIo::Udp(s) => match s.recv_from(buf) {
                Ok((n, _)) => Ok(Some(n)),
                Err(e)
                    if e.kind() == io::ErrorKind::WouldBlock
                        || e.kind() == io::ErrorKind::TimedOut =>
                {
                    Ok(None)
                }
                Err(e) => Err(e),
            },
            TurnIo::Tcp { stream, .. } => {
                let frame_buf = &mut self.frame_buf;
                recv_tcp_frame(&mut **stream, frame_buf, buf)
            }
        }
    }

    /// 发送请求并等待匹配 txid 响应（deadline 内轮询；跳过无关包）。
    fn request(
        &mut self,
        method: u16,
        attrs: Vec<(u16, Vec<u8>)>,
    ) -> Result<(Vec<u8>, SocketAddr), String> {
        self.request_impl(method, attrs, REQUEST_TIMEOUT)
    }

    /// 快速请求（refresh）：短预算，失败下轮重试。
    fn request_fast(
        &mut self,
        method: u16,
        attrs: Vec<(u16, Vec<u8>)>,
    ) -> Result<(Vec<u8>, SocketAddr), String> {
        self.request_impl(method, attrs, FAST_REQUEST_BUDGET)
    }

    fn request_impl(
        &mut self,
        method: u16,
        attrs: Vec<(u16, Vec<u8>)>,
        budget: Duration,
    ) -> Result<(Vec<u8>, SocketAddr), String> {
        let deadline = Instant::now() + budget;
        for attempt in 0..MAX_RESPONSE_SKIP {
            let txid = random_txid();
            let auth = (
                self.username.as_str(),
                self.password.as_str(),
                self.realm.as_str(),
                self.nonce.as_str(),
            );
            let req = build_stun(method, &attrs, txid, Some(auth))?;
            self.send_raw(&req)
                .map_err(|e| format!("turn send {method:#06x}: {e}"))?;
            loop {
                if Instant::now() >= deadline {
                    break;
                }
                let mut buf = [0u8; 4096];
                match self.recv_raw(&mut buf) {
                    Ok(Some(n)) => {
                        let pkt = &buf[..n];
                        if pkt.len() >= 20
                            && pkt[4..8] == STUN_MAGIC.to_be_bytes()
                            && pkt[8..20] == txid
                        {
                            let resp = pkt.to_vec();
                            let (new_realm, new_nonce, err) = parse_common(&resp)?;
                            if let Some(r) = new_realm {
                                self.realm = r;
                            }
                            if let Some(n) = new_nonce {
                                self.nonce = n;
                            }
                            if err == Some(438) && attempt + 1 < MAX_RESPONSE_SKIP {
                                break; // stale nonce：换新 nonce 重试
                            }
                            if err.is_some() && err != Some(401) && err != Some(438) {
                                return Err(format!("TURN {method:#06x} error: {err:?}"));
                            }
                            return Ok((resp, self.server));
                        }
                        // 无关包（Data indication / 并发事务）：跳过
                    }
                    Ok(None) => {
                        std::thread::sleep(Duration::from_millis(2));
                    }
                    Err(e) => return Err(format!("turn recv {method:#06x}: {e}")),
                }
            }
        }
        Err("TURN request retries exhausted".into())
    }

    /// 等待与 txid 匹配的响应（deadline 内轮询，跳过无关包）。
    fn recv_matching(
        &mut self,
        txid: [u8; 12],
        budget: Duration,
    ) -> Result<(Vec<u8>, SocketAddr), String> {
        let deadline = Instant::now() + budget;
        loop {
            if Instant::now() >= deadline {
                return Err("TURN response timeout".into());
            }
            let mut buf = [0u8; 2048];
            match self.recv_raw(&mut buf) {
                Ok(Some(n)) => {
                    let pkt = &buf[..n];
                    if pkt.len() >= 20
                        && pkt[4..8] == STUN_MAGIC.to_be_bytes()
                        && pkt[8..20] == txid
                    {
                        return Ok((pkt.to_vec(), self.server));
                    }
                }
                Ok(None) => {
                    std::thread::sleep(Duration::from_millis(2));
                }
                Err(e) => return Err(format!("turn recv: {e}")),
            }
        }
    }
}

/// TCP 帧读取：先切帧缓冲，不足则读流补充（读超时 10ms；半包累积）。
fn recv_tcp_frame(
    stream: &mut dyn ReadWriteSend,
    frame_buf: &mut Vec<u8>,
    buf: &mut [u8],
) -> io::Result<Option<usize>> {
    if let Some(frame) = take_tcp_frame(frame_buf) {
        let n = frame.len().min(buf.len());
        buf[..n].copy_from_slice(&frame[..n]);
        return Ok(Some(n));
    }
    let mut tmp = [0u8; 65535];
    let n = match stream.read(&mut tmp) {
        Ok(0) => return Err(io::Error::new(io::ErrorKind::UnexpectedEof, "turn tcp eof")),
        Ok(n) => n,
        Err(e) if e.kind() == io::ErrorKind::WouldBlock || e.kind() == io::ErrorKind::TimedOut => {
            return Ok(None);
        }
        Err(e) => return Err(e),
    };
    frame_buf.extend_from_slice(&tmp[..n]);
    if let Some(frame) = take_tcp_frame(frame_buf) {
        let n = frame.len().min(buf.len());
        buf[..n].copy_from_slice(&frame[..n]);
        Ok(Some(n))
    } else {
        Ok(None)
    }
}

/// 从 TCP 帧缓冲切出一完整帧（返回去除 2 字节长度前缀的载荷）。
fn take_tcp_frame(frame_buf: &mut Vec<u8>) -> Option<Vec<u8>> {
    if frame_buf.len() < 2 {
        return None;
    }
    let len = u16::from_be_bytes([frame_buf[0], frame_buf[1]]) as usize;
    if len < 20 {
        // 非法帧：丢弃缓冲避免卡死
        frame_buf.clear();
        return None;
    }
    if frame_buf.len() < 2 + len {
        return None;
    }
    let frame = frame_buf[2..2 + len].to_vec();
    frame_buf.drain(..2 + len);
    Some(frame)
}

/// rustls 客户端配置：webpki-roots 系统根 + 可选 `TURN_TLS_CA` PEM 追加。
fn build_tls_config(ca: Option<&Path>) -> Result<Arc<rustls::ClientConfig>, String> {
    // 显式安装 ring provider（跨 crate feature 解析可能歧义，见 LESSON rustls0.23）。
    let _ = rustls::crypto::ring::default_provider().install_default();
    let mut roots = rustls::RootCertStore::empty();
    roots.extend(webpki_roots::TLS_SERVER_ROOTS.iter().cloned());
    if let Some(path) = ca {
        let pem = std::fs::read(path).map_err(|e| format!("TURN_TLS_CA 读取失败: {e}"))?;
        let mut rd = io::BufReader::new(&pem[..]);
        let certs =
            rustls_pemfile::certs(&mut rd).map_err(|e| format!("TURN_TLS_CA 解析失败: {e}"))?;
        for c in certs {
            roots
                .add(rustls::pki_types::CertificateDer::from(c))
                .map_err(|e| format!("TURN_TLS_CA 添加根失败: {e}"))?;
        }
    }
    let cfg = rustls::ClientConfig::builder()
        .with_root_certificates(roots)
        .with_no_client_auth();
    Ok(Arc::new(cfg))
}

fn allocate_attrs() -> Vec<(u16, Vec<u8>)> {
    vec![
        (ATTR_REQUESTED_TRANSPORT, vec![17, 0, 0, 0]),
        (ATTR_SOFTWARE, b"aerodesk".to_vec()),
    ]
}

fn expect_success(resp: &[u8], method: u16) -> Result<(), String> {
    if stun_method(resp) != method | MSG_SUCCESS_BASE {
        return Err(format!(
            "unexpected TURN response method {:#06x} (want {:#06x})",
            stun_method(resp),
            method | MSG_SUCCESS_BASE
        ));
    }
    Ok(())
}

fn random_txid() -> [u8; 12] {
    let mut t = [0u8; 12];
    let now = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_nanos())
        .unwrap_or(0);
    let addr = &t as *const _ as usize as u64;
    t[..8].copy_from_slice(&(now as u64).to_le_bytes());
    t[8..].copy_from_slice(&addr.to_le_bytes()[..4]);
    t
}

#[cfg(test)]
pub(crate) mod testutil {
    use super::*;
    use std::net::{TcpListener, UdpSocket};
    use std::sync::Arc;
    use std::sync::atomic::{AtomicUsize, Ordering};

    /// mock TURN 服务器（UDP）：401 → 校验 MI → 按方法回 200；ChannelData → Data indication 回显。
    /// `bind_requests` 统计 ChannelBind 次数（用于 channel 复用断言）。
    #[allow(clippy::too_many_arguments)]
    pub fn spawn_mock_turn(
        sock: UdpSocket,
        username: &str,
        password: &str,
        relayed: SocketAddr,
        bind_requests: Arc<AtomicUsize>,
    ) -> std::thread::JoinHandle<()> {
        let username = username.to_string();
        let password = password.to_string();
        std::thread::spawn(move || {
            let mut buf = [0u8; 65535];
            let mut channel_peer: Option<(u16, SocketAddr)> = None;
            loop {
                let Ok((n, from)) = sock.recv_from(&mut buf) else {
                    break;
                };
                let pkt = &buf[..n];
                // ChannelData → Data indication 回显
                if pkt.len() >= 4 && (pkt[0] & 0xc0) == 0x40 {
                    let len = u16::from_be_bytes([pkt[2], pkt[3]]) as usize;
                    let payload = &pkt[4..4 + len];
                    let Some((_, peer)) = channel_peer else {
                        continue;
                    };
                    let mut body = Vec::new();
                    body.extend_from_slice(&encode_attr(
                        ATTR_XOR_PEER_ADDRESS,
                        &encode_xor_peer(peer),
                    ));
                    body.extend_from_slice(&encode_attr(ATTR_DATA, payload));
                    let txid = [0u8; 12];
                    let msg = encode_header(MSG_DATA_INDICATION, txid, &body);
                    let _ = sock.send_to(&msg, from);
                    continue;
                }
                if pkt.len() < 20 || pkt[4..8] != STUN_MAGIC.to_be_bytes() {
                    continue;
                }
                let method = u16::from_be_bytes([pkt[0], pkt[1]]) & 0x3fff;
                let txid: [u8; 12] = pkt[8..20].try_into().unwrap();
                let attrs = parse_attrs(pkt);
                let has_mi = attrs.iter().any(|(t, _)| *t == ATTR_MESSAGE_INTEGRITY);
                if !has_mi {
                    let mut body = Vec::new();
                    body.extend_from_slice(&encode_attr(ATTR_REALM, b"aerodesk.test"));
                    body.extend_from_slice(&encode_attr(ATTR_NONCE, b"nonce-1"));
                    body.extend_from_slice(&encode_attr(ATTR_ERROR_CODE, &[0, 0, 4, 1]));
                    let msg = encode_header(MSG_ERROR_BASE | 0x0003, txid, &body);
                    let _ = sock.send_to(&msg, from);
                    continue;
                }
                let realm = "aerodesk.test";
                let key = md5_key(&username, realm, &password);
                let mi_off = pkt.len() - 24;
                let mac = hmac_sha1(&key, &pkt[..mi_off]);
                if pkt[mi_off + 4..] != mac {
                    let mut body = Vec::new();
                    body.extend_from_slice(&encode_attr(ATTR_ERROR_CODE, &[0, 0, 4, 1]));
                    let msg = encode_header(MSG_ERROR_BASE | method, txid, &body);
                    let _ = sock.send_to(&msg, from);
                    continue;
                }
                match method {
                    MSG_ALLOCATE => {
                        let mut body = Vec::new();
                        body.extend_from_slice(&encode_attr(
                            ATTR_XOR_RELAYED_ADDRESS,
                            &encode_xor_peer(relayed),
                        ));
                        let msg = encode_header(MSG_SUCCESS_BASE | MSG_ALLOCATE, txid, &body);
                        let _ = sock.send_to(&msg, from);
                    }
                    MSG_CREATE_PERMISSION => {
                        let msg =
                            encode_header(MSG_SUCCESS_BASE | MSG_CREATE_PERMISSION, txid, &[]);
                        let _ = sock.send_to(&msg, from);
                    }
                    MSG_CHANNEL_BIND => {
                        bind_requests.fetch_add(1, Ordering::SeqCst);
                        let chan = attrs
                            .iter()
                            .find(|(t, _)| *t == ATTR_CHANNEL_NUMBER)
                            .map(|(_, v)| u16::from_be_bytes([v[0], v[1]]))
                            .unwrap_or(0);
                        let peer = attrs
                            .iter()
                            .find(|(t, _)| *t == ATTR_XOR_PEER_ADDRESS)
                            .and_then(|(_, v)| parse_xor_addr(v))
                            .unwrap();
                        channel_peer = Some((chan, peer));
                        let msg = encode_header(MSG_SUCCESS_BASE | MSG_CHANNEL_BIND, txid, &[]);
                        let _ = sock.send_to(&msg, from);
                    }
                    MSG_REFRESH => {
                        let msg = encode_header(MSG_SUCCESS_BASE | MSG_REFRESH, txid, &[]);
                        let _ = sock.send_to(&msg, from);
                    }
                    _ => {}
                }
            }
        })
    }

    pub fn start_mock(
        username: &str,
        password: &str,
    ) -> (
        SocketAddr,
        SocketAddr,
        Arc<AtomicUsize>,
        std::thread::JoinHandle<()>,
    ) {
        let sock = UdpSocket::bind("127.0.0.1:0").unwrap();
        let server = sock.local_addr().unwrap();
        let relayed: SocketAddr = "203.0.113.5:5000".parse().unwrap();
        let binds = Arc::new(AtomicUsize::new(0));
        let h = spawn_mock_turn(sock, username, password, relayed, binds.clone());
        std::thread::sleep(Duration::from_millis(100));
        (server, relayed, binds, h)
    }

    /// mock TURN 服务器（TCP，RFC 4571 帧）：协议同 UDP mock，收发都带 2 字节长度前缀。
    pub fn spawn_mock_turn_tcp(
        listener: TcpListener,
        username: &str,
        password: &str,
        relayed: SocketAddr,
    ) -> std::thread::JoinHandle<()> {
        let username = username.to_string();
        let password = password.to_string();
        std::thread::spawn(move || {
            for stream in listener.incoming() {
                let Ok(mut stream) = stream else {
                    continue;
                };
                let username = username.clone();
                let password = password.clone();
                let relayed = relayed;
                std::thread::spawn(move || {
                    let mut channel_peer: Option<(u16, SocketAddr)> = None;
                    let mut pending = Vec::new();
                    let mut tmp = [0u8; 4096];
                    loop {
                        // 读帧（累积）
                        match stream.read(&mut tmp) {
                            Ok(0) => break,
                            Ok(n) => pending.extend_from_slice(&tmp[..n]),
                            Err(_) => break,
                        }
                        // 切出完整帧
                        while pending.len() >= 2 {
                            let len = u16::from_be_bytes([pending[0], pending[1]]) as usize;
                            if pending.len() < 2 + len {
                                break;
                            }
                            let pkt = pending[2..2 + len].to_vec();
                            pending.drain(..2 + len);
                            if pkt.len() >= 4 && (pkt[0] & 0xc0) == 0x40 {
                                // ChannelData → Data indication 回显（帧化）
                                let clen = u16::from_be_bytes([pkt[2], pkt[3]]) as usize;
                                let payload = &pkt[4..4 + clen];
                                let Some((_, peer)) = channel_peer else {
                                    continue;
                                };
                                let mut body = Vec::new();
                                body.extend_from_slice(&encode_attr(
                                    ATTR_XOR_PEER_ADDRESS,
                                    &encode_xor_peer(peer),
                                ));
                                body.extend_from_slice(&encode_attr(ATTR_DATA, payload));
                                let msg = encode_header(MSG_DATA_INDICATION, [0u8; 12], &body);
                                let mut framed = Vec::with_capacity(2 + msg.len());
                                framed.extend_from_slice(&(msg.len() as u16).to_be_bytes());
                                framed.extend_from_slice(&msg);
                                let _ = stream.write_all(&framed);
                                continue;
                            }
                            if pkt.len() < 20 || pkt[4..8] != STUN_MAGIC.to_be_bytes() {
                                continue;
                            }
                            let method = u16::from_be_bytes([pkt[0], pkt[1]]) & 0x3fff;
                            let txid: [u8; 12] = pkt[8..20].try_into().unwrap();
                            let attrs = parse_attrs(&pkt);
                            let has_mi = attrs.iter().any(|(t, _)| *t == ATTR_MESSAGE_INTEGRITY);
                            let resp: Option<Vec<u8>> = if !has_mi {
                                let mut body = Vec::new();
                                body.extend_from_slice(&encode_attr(ATTR_REALM, b"aerodesk.test"));
                                body.extend_from_slice(&encode_attr(ATTR_NONCE, b"nonce-1"));
                                body.extend_from_slice(&encode_attr(
                                    ATTR_ERROR_CODE,
                                    &[0, 0, 4, 1],
                                ));
                                Some(encode_header(MSG_ERROR_BASE | 0x0003, txid, &body))
                            } else {
                                let realm = "aerodesk.test";
                                let key = md5_key(&username, realm, &password);
                                let mi_off = pkt.len() - 24;
                                let mac = hmac_sha1(&key, &pkt[..mi_off]);
                                if pkt[mi_off + 4..] != mac {
                                    let mut body = Vec::new();
                                    body.extend_from_slice(&encode_attr(
                                        ATTR_ERROR_CODE,
                                        &[0, 0, 4, 1],
                                    ));
                                    Some(encode_header(MSG_ERROR_BASE | method, txid, &body))
                                } else {
                                    match method {
                                        MSG_ALLOCATE => {
                                            let mut body = Vec::new();
                                            body.extend_from_slice(&encode_attr(
                                                ATTR_XOR_RELAYED_ADDRESS,
                                                &encode_xor_peer(relayed),
                                            ));
                                            Some(encode_header(
                                                MSG_SUCCESS_BASE | MSG_ALLOCATE,
                                                txid,
                                                &body,
                                            ))
                                        }
                                        MSG_CREATE_PERMISSION => Some(encode_header(
                                            MSG_SUCCESS_BASE | MSG_CREATE_PERMISSION,
                                            txid,
                                            &[],
                                        )),
                                        MSG_CHANNEL_BIND => {
                                            let chan = attrs
                                                .iter()
                                                .find(|(t, _)| *t == ATTR_CHANNEL_NUMBER)
                                                .map(|(_, v)| u16::from_be_bytes([v[0], v[1]]))
                                                .unwrap_or(0);
                                            let peer = attrs
                                                .iter()
                                                .find(|(t, _)| *t == ATTR_XOR_PEER_ADDRESS)
                                                .and_then(|(_, v)| parse_xor_addr(v))
                                                .unwrap();
                                            channel_peer = Some((chan, peer));
                                            Some(encode_header(
                                                MSG_SUCCESS_BASE | MSG_CHANNEL_BIND,
                                                txid,
                                                &[],
                                            ))
                                        }
                                        MSG_REFRESH => Some(encode_header(
                                            MSG_SUCCESS_BASE | MSG_REFRESH,
                                            txid,
                                            &[],
                                        )),
                                        _ => None,
                                    }
                                }
                            };
                            if let Some(msg) = resp {
                                let mut framed = Vec::with_capacity(2 + msg.len());
                                framed.extend_from_slice(&(msg.len() as u16).to_be_bytes());
                                framed.extend_from_slice(&msg);
                                let _ = stream.write_all(&framed);
                            }
                        }
                        // 防止 pending 无限增长
                        if pending.len() > 65535 {
                            pending.clear();
                        }
                    }
                });
            }
        })
    }
}

#[cfg(test)]
mod tests {
    use super::testutil::{spawn_mock_turn_tcp, start_mock};
    use super::*;
    use std::net::TcpListener;
    use std::sync::atomic::Ordering;

    #[test]
    fn connect_gets_relayed_address() {
        let (server, relayed, _, _h) = start_mock("user1", "pass1");
        let tt = TurnTransport::connect(
            server,
            "user1",
            "pass1",
            Ipv4Addr::LOCALHOST.into(),
            Duration::from_secs(2),
        )
        .expect("connect");
        assert_eq!(tt.relayed_addr(), relayed);
    }

    #[test]
    fn connect_rejects_bad_password() {
        let (server, _, _, _h) = start_mock("user1", "pass1");
        let res = TurnTransport::connect(
            server,
            "user1",
            "wrong",
            Ipv4Addr::LOCALHOST.into(),
            Duration::from_secs(2),
        );
        assert!(res.is_err(), "wrong password must fail");
    }

    #[test]
    fn send_recv_roundtrip_via_channel() {
        let (server, _, binds, _h) = start_mock("user1", "pass1");
        let mut tt = TurnTransport::connect(
            server,
            "user1",
            "pass1",
            Ipv4Addr::LOCALHOST.into(),
            Duration::from_secs(2),
        )
        .expect("connect");
        let peer: SocketAddr = "192.0.2.10:4000".parse().unwrap();
        tt.send_to(peer, b"hello-turn").expect("send");
        assert_eq!(binds.load(Ordering::SeqCst), 1, "first send binds channel");
        let mut buf = [0u8; 4096];
        let got = tt.recv_packet(&mut buf).expect("recv").expect("packet");
        assert_eq!(got.0, peer);
        assert_eq!(&buf[..got.1], b"hello-turn");
    }

    #[test]
    fn channel_reused_for_same_peer() {
        let (server, _, binds, _h) = start_mock("user1", "pass1");
        let mut tt = TurnTransport::connect(
            server,
            "user1",
            "pass1",
            Ipv4Addr::LOCALHOST.into(),
            Duration::from_secs(2),
        )
        .expect("connect");
        let peer: SocketAddr = "192.0.2.10:4000".parse().unwrap();
        tt.send_to(peer, b"a").expect("send 1");
        tt.send_to(peer, b"b").expect("send 2");
        assert_eq!(binds.load(Ordering::SeqCst), 1, "channel must be reused");
    }

    #[test]
    fn refresh_extends_allocation() {
        let (server, _, _, _h) = start_mock("user1", "pass1");
        let mut tt = TurnTransport::connect(
            server,
            "user1",
            "pass1",
            Ipv4Addr::LOCALHOST.into(),
            Duration::from_secs(2),
        )
        .expect("connect");
        tt.last_refresh = Instant::now() - REFRESH_INTERVAL - Duration::from_secs(1);
        tt.refresh_if_due(Instant::now()).expect("refresh");
        assert!(Instant::now().duration_since(tt.last_refresh) < Duration::from_secs(1));
    }

    #[test]
    fn tcp_roundtrip_via_turn_transport() {
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let server = listener.local_addr().unwrap();
        let relayed: SocketAddr = "203.0.113.5:5000".parse().unwrap();
        let _srv = spawn_mock_turn_tcp(listener, "user1", "pass1", relayed);
        std::thread::sleep(Duration::from_millis(100));

        let mut tt = TurnTransport::connect_tcp(
            "localhost",
            server,
            "user1",
            "pass1",
            false,
            None,
            Duration::from_secs(3),
        )
        .expect("tcp connect");
        assert_eq!(tt.relayed_addr(), relayed);

        let peer: SocketAddr = "192.0.2.10:4000".parse().unwrap();
        tt.send_to(peer, b"hello-tcp-turn").expect("send");
        let mut buf = [0u8; 4096];
        let got = tt.recv_packet(&mut buf).expect("recv").expect("packet");
        assert_eq!(got.0, peer);
        assert_eq!(&buf[..got.1], b"hello-tcp-turn");

        // 半包路径：再发一次，验证帧缓冲累积正常
        tt.send_to(peer, b"second").expect("send2");
        let got2 = tt.recv_packet(&mut buf).expect("recv2").expect("packet2");
        assert_eq!(got2.0, peer);
        assert_eq!(&buf[..got2.1], b"second");
    }

    #[test]
    fn parse_turn_url_variants() {
        assert_eq!(
            parse_turn_url("turn:turn.example.com:3478?transport=udp"),
            Some(TurnUrl {
                host: "turn.example.com".to_string(),
                port: 3478,
                proto: TurnProto::Udp,
            })
        );
        assert_eq!(
            parse_turn_url("turn:127.0.0.1:14789?transport=tcp"),
            Some(TurnUrl {
                host: "127.0.0.1".to_string(),
                port: 14789,
                proto: TurnProto::Tcp,
            })
        );
        assert_eq!(
            parse_turn_url("turns:host.example.com:5349?transport=tcp"),
            Some(TurnUrl {
                host: "host.example.com".to_string(),
                port: 5349,
                proto: TurnProto::Tls,
            })
        );
        assert_eq!(
            parse_turn_url("turn:127.0.0.1"),
            Some(TurnUrl {
                host: "127.0.0.1".to_string(),
                port: 3478,
                proto: TurnProto::Udp,
            })
        );
        assert_eq!(parse_turn_url(""), None);
    }

    #[test]
    fn md5_key_hmac_matches_reference() {
        let key = md5_key("user1", "aerodesk.test", "pass1");
        assert_eq!(
            key.iter().map(|b| format!("{b:02x}")).collect::<String>(),
            "f68e2c7bcdbfed125ca188fe968af6ba"
        );
        let mac = hmac_sha1(&key, b"Hello");
        assert_eq!(
            mac.iter().map(|b| format!("{b:02x}")).collect::<String>(),
            "48d2d4be79e9b9166ab3a68c14e5cd58cc57ea15"
        );
    }

    #[test]
    fn hmac_sha1_rfc2202_vector() {
        let key = [0x0bu8; 20];
        let mac = hmac_sha1(&key, b"Hi There");
        let expect: [u8; 20] = [
            0xb6, 0x17, 0x31, 0x86, 0x55, 0x05, 0x72, 0x64, 0xe2, 0x8b, 0xc0, 0xb6, 0xfb, 0x37,
            0x8c, 0x8e, 0xf1, 0x46, 0xbe, 0x00,
        ];
        assert_eq!(mac, expect);
    }
}
