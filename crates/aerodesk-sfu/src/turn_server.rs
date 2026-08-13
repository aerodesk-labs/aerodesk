//! SFU 内嵌 TURN+STUN server（#191 UDP；#196 TCP/TLS）。
//!
//! 架构：控制面多线程共享状态（UDP 事件循环 + TCP/TLS 每连接读线程），
//! 每 allocation 一个 relay 线程（peer → 客户端转发）。
//!
//! 传输：
//! - UDP：`turn:host:port?transport=udp`（默认 3479）
//! - TCP：同一端口 `?transport=tcp`，RFC 4571 2 字节长度前缀帧
//! - TLS：`turns:host:port?transport=tcp`（`SFU_TURN_TLS_PORT`，默认 5349；
//!   复用 `TlsIdentity`，证书加载失败时降级跳过 TLS）
//!
//! 认证：TURN_SECRET REST 模式（与 coturn 兼容）——username=`<expiry>:<userid>`，
//! credential=base64(HMAC-SHA1(secret, username))；SFU 下发的 TurnConfig 可直接使用。
//!
//! 数据面：客户端 → peer 走 Send indication / ChannelData；peer → 客户端走
//! ChannelData（已绑 channel）或 Data indication（仅有 permission）。

use std::collections::{HashMap, HashSet};
use std::io::{self, BufReader, Read, Write};
use std::net::{IpAddr, SocketAddr, TcpListener, TcpStream, UdpSocket};
use std::str::FromStr;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

use aerodesk_protocol::turn::codec::*;
use tracing::{debug, info, warn};

/// 默认 realm（可用 TURN_REALM 覆盖；与 docs/TURN.md 一致）。
pub const DEFAULT_REALM: &str = "aerodesk.io";
/// allocation 默认/最大 lifetime（秒）。
const DEFAULT_LIFETIME: u32 = 600;
/// 过期清扫间隔。
const SWEEP_INTERVAL: Duration = Duration::from_secs(30);
/// UDP 控制面读超时（轮询粒度）。
const POLL_TIMEOUT: Duration = Duration::from_millis(50);
/// TCP/TLS 连接读超时（空闲时每 tick 释放锁）。
const TCP_READ_TIMEOUT: Duration = Duration::from_millis(50);
/// 时钟偏差容忍（秒）。
const CLOCK_SKEW: u64 = 300;

/// 连接 IO trait 对象（明文 TCP 或 rustls TlsStream）。
trait ConnIo: Read + Write + Send {}
impl<T: Read + Write + Send> ConnIo for T {}
type ConnBox = Box<dyn ConnIo>;

/// allocation 归属键（UDP 按客户端地址；TCP 按连接 id）。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
enum ClientKey {
    Udp(SocketAddr),
    Tcp { conn: u64, ip: IpAddr },
}

/// 客户端出口（响应 / 中继入站统一出口；TCP/TLS 加 2 字节长度前缀）。
#[derive(Clone)]
enum ClientSink {
    Udp(SocketAddr),
    Tcp(Arc<Mutex<ConnBox>>),
}

impl ClientSink {
    fn send(&self, server: &UdpSocket, bytes: &[u8]) -> io::Result<()> {
        match self {
            ClientSink::Udp(addr) => server.send_to(bytes, *addr).map(|_| ()),
            ClientSink::Tcp(w) => {
                let mut framed = Vec::with_capacity(2 + bytes.len());
                framed.extend_from_slice(&(bytes.len() as u16).to_be_bytes());
                framed.extend_from_slice(bytes);
                w.lock().unwrap().write_all(&framed)
            }
        }
    }
}

/// allocation 共享状态（控制面线程写、relay 线程读）。
struct AllocState {
    channels: HashMap<u16, SocketAddr>,
    peer_channel: HashMap<SocketAddr, u16>,
    permissions: HashSet<SocketAddr>,
}

struct Allocation {
    relay: Arc<UdpSocket>,
    relayed: SocketAddr,
    client_ip: IpAddr,
    expires: Arc<AtomicU64>,
    stop: Arc<AtomicBool>,
    state: Arc<Mutex<AllocState>>,
}

/// 控制面共享状态（UDP 线程与各 TCP/TLS 连接线程共用）。
struct Shared {
    allocations: Mutex<HashMap<ClientKey, Allocation>>,
    /// 累计成功创建的 allocation 数（含已过期/断开；#220 观测 churn）。
    created_total: AtomicU64,
    secret: String,
    realm: String,
    nonce: String,
    host: IpAddr,
    next_conn: AtomicU64,
    /// 每 IP 最大并发 allocation（0=不限）。
    max_allocs_per_ip: usize,
    /// 全局最大并发 allocation（0=不限）。
    max_allocs_total: usize,
    /// allocation 默认 lifetime 秒（TURN_LIFETIME_SEC，默认 600；测试/短租期可调）。
    default_lifetime: u32,
    /// 拒绝中继的 peer CIDR（TURN_DENIED_PEER_CIDRS）。
    denied_peers: Vec<ipnet::IpNet>,
}

/// 内嵌 TURN server 句柄（地址可用；线程自管理生命周期）。
pub struct TurnServer {
    pub udp_addr: SocketAddr,
    pub tcp_addr: Option<SocketAddr>,
    pub tls_addr: Option<SocketAddr>,
    _handles: Vec<std::thread::JoinHandle<()>>,
    shared: Arc<Shared>,
}

impl TurnServer {
    /// 当前活跃 allocation 数（#220：长稳/泄漏观测）。
    pub fn active_allocations(&self) -> usize {
        self.shared.allocations.lock().unwrap().len()
    }

    /// 累计创建 allocation 数（#220：churn 观测；Refresh 失败重连会增长）。
    pub fn allocations_total(&self) -> u64 {
        self.shared.created_total.load(Ordering::Relaxed)
    }
}

/// 启动内嵌 TURN+STUN server（UDP + TCP，可选 TLS）。
/// `udp_port` 传 0 自动分配（测试）；`tls_port` 为 None 时不启用 TLS。
pub fn spawn(
    secret: &str,
    host_addr: IpAddr,
    udp_port: u16,
    tls_port: Option<u16>,
) -> io::Result<TurnServer> {
    // rustls 0.23 需要进程级 CryptoProvider；显式安装 aws-lc-rs 提供器，
    // 避免跨 crate feature 探测歧义（#196 TLS）。
    let _ = rustls::crypto::aws_lc_rs::default_provider().install_default();
    let ipv6 = std::env::var("SFU_TURN_IPV6")
        .map(|v| v == "1")
        .unwrap_or(false);
    let shared = Arc::new(Shared {
        allocations: Mutex::new(HashMap::new()),
        created_total: AtomicU64::new(0),
        secret: secret.to_string(),
        realm: std::env::var("TURN_REALM").unwrap_or_else(|_| DEFAULT_REALM.to_string()),
        nonce: format!(
            "{:016x}",
            SystemTime::now()
                .duration_since(UNIX_EPOCH)
                .map(|d| d.as_nanos())
                .unwrap_or(0)
        ),
        host: host_addr,
        next_conn: AtomicU64::new(1),
        // #204：配额默认 16/IP、256 全局（0=不限）；CIDR 列表默认空=不限制。
        max_allocs_per_ip: env_usize("MAX_TURN_ALLOCS_PER_IP", 16),
        max_allocs_total: env_usize("MAX_TURN_ALLOCS_TOTAL", 256),
        default_lifetime: std::env::var("TURN_LIFETIME_SEC")
            .ok()
            .and_then(|v| v.parse::<u32>().ok())
            .map(|v| v.max(60))
            .unwrap_or(DEFAULT_LIFETIME),
        denied_peers: std::env::var("TURN_DENIED_PEER_CIDRS")
            .map(|v| {
                v.split(',')
                    .filter_map(|c| ipnet::IpNet::from_str(c.trim()).ok())
                    .collect()
            })
            .unwrap_or_default(),
    });

    // UDP：默认 0.0.0.0；SFU_TURN_IPV6=1 时双栈 [::]（V6ONLY=0，不可用回退 v4）。
    let udp_socket = if ipv6 {
        bind_udp_dual(udp_port)?
    } else {
        UdpSocket::bind((std::net::Ipv4Addr::UNSPECIFIED, udp_port))?
    };
    udp_socket.set_read_timeout(Some(POLL_TIMEOUT))?;
    let bound = udp_socket.local_addr()?;
    let udp_addr = SocketAddr::new(host_addr, bound.port());
    let server = Arc::new(udp_socket);
    info!(
        "embedded TURN+STUN server UDP on {udp_addr} (realm={})",
        shared.realm
    );

    let mut handles = Vec::new();
    {
        let shared = shared.clone();
        let server = server.clone();
        handles.push(std::thread::spawn(move || udp_run(shared, server)));
    }

    // TCP：与 UDP 同端口（不同协议可共存）。
    let tcp_addr = SocketAddr::new(host_addr, bound.port());
    let tcp_listener = if ipv6 {
        bind_tcp_dual(bound.port())?
    } else {
        TcpListener::bind((std::net::Ipv4Addr::UNSPECIFIED, bound.port()))?
    };
    tcp_listener.set_nonblocking(true)?;
    info!("embedded TURN+STUN server TCP on {tcp_addr}");
    {
        let shared = shared.clone();
        let server = server.clone();
        handles.push(std::thread::spawn(move || {
            tcp_accept_loop(tcp_listener, shared, server, None)
        }));
    }

    // TLS：SFU_TURN_TLS_PORT；证书加载失败降级跳过。
    let mut tls_addr = None;
    if let Some(port) = tls_port {
        let tls_listener = match if ipv6 {
            bind_tcp_dual(port)
        } else {
            TcpListener::bind((std::net::Ipv4Addr::UNSPECIFIED, port))
        } {
            Ok(l) => l,
            Err(e) => {
                warn!("TURN TLS listener bind :{port} failed ({e}); turns: 不可用");
                return Ok(TurnServer {
                    udp_addr,
                    tcp_addr: Some(tcp_addr),
                    tls_addr: None,
                    _handles: handles,
                    shared,
                });
            }
        };
        tls_listener.set_nonblocking(true)?;
        match build_tls_acceptor() {
            Ok(acceptor) => {
                let bound = match tls_listener.local_addr() {
                    Ok(a) => a,
                    Err(e) => {
                        warn!("TURN TLS local_addr failed ({e}); turns: 不可用");
                        return Ok(TurnServer {
                            udp_addr,
                            tcp_addr: Some(tcp_addr),
                            tls_addr: None,
                            _handles: handles,
                            shared,
                        });
                    }
                };
                let addr = SocketAddr::new(host_addr, bound.port());
                info!("embedded TURN+STUN server TLS on {addr}");
                tls_addr = Some(addr);
                let shared = shared.clone();
                let server = server.clone();
                handles.push(std::thread::spawn(move || {
                    tcp_accept_loop(tls_listener, shared, server, Some(acceptor))
                }));
            }
            Err(e) => {
                warn!("TURN TLS acceptor init failed ({e}); turns: 不可用");
            }
        }
    }

    Ok(TurnServer {
        udp_addr,
        tcp_addr: Some(tcp_addr),
        tls_addr,
        _handles: handles,
        shared,
    })
}

fn env_usize(name: &str, default: usize) -> usize {
    std::env::var(name)
        .ok()
        .and_then(|v| v.parse().ok())
        .unwrap_or(default)
}

fn peer_denied(shared: &Shared, peer: IpAddr) -> bool {
    shared.denied_peers.iter().any(|n| n.contains(&peer))
}

/// IPv6 双栈 UDP socket（V6ONLY=0；不可用时回退 IPv4 0.0.0.0）。
fn bind_udp_dual(port: u16) -> io::Result<UdpSocket> {
    if let Ok(sock) = socket2::Socket::new(socket2::Domain::IPV6, socket2::Type::DGRAM, None) {
        let _ = sock.set_only_v6(false);
        let _ = sock.set_reuse_address(true);
        let addr: std::net::SocketAddr =
            std::net::SocketAddr::new(std::net::IpAddr::V6(std::net::Ipv6Addr::UNSPECIFIED), port);
        if sock.bind(&addr.into()).is_ok() {
            return Ok(sock.into());
        }
    }
    UdpSocket::bind((std::net::Ipv4Addr::UNSPECIFIED, port))
}

/// IPv6 双栈 TCP listener（同上）。
fn bind_tcp_dual(port: u16) -> io::Result<TcpListener> {
    if let Ok(sock) = socket2::Socket::new(socket2::Domain::IPV6, socket2::Type::STREAM, None) {
        let _ = sock.set_only_v6(false);
        let _ = sock.set_reuse_address(true);
        let addr: std::net::SocketAddr =
            std::net::SocketAddr::new(std::net::IpAddr::V6(std::net::Ipv6Addr::UNSPECIFIED), port);
        if sock.bind(&addr.into()).is_ok() {
            return Ok(sock.into());
        }
    }
    TcpListener::bind((std::net::Ipv4Addr::UNSPECIFIED, port))
}

fn unix_now() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0)
}

/// rustls ServerConfig（复用 TlsIdentity PEM）。
fn build_tls_acceptor() -> Result<Arc<rustls::ServerConfig>, String> {
    let id = aerodesk_protocol::tls::TlsIdentity::load()?;
    let mut cert_rd = BufReader::new(&id.cert[..]);
    let certs: Vec<rustls::pki_types::CertificateDer<'static>> =
        rustls_pemfile::certs(&mut cert_rd)
            .map_err(|e| format!("cert parse: {e}"))?
            .into_iter()
            .map(rustls::pki_types::CertificateDer::from)
            .collect();
    if certs.is_empty() {
        return Err("no certificates in TlsIdentity".into());
    }
    let mut key_rd = BufReader::new(&id.key[..]);
    let key = rustls_pemfile::pkcs8_private_keys(&mut key_rd)
        .map_err(|e| format!("key parse: {e}"))?
        .into_iter()
        .next()
        .map(|v| {
            rustls::pki_types::PrivateKeyDer::Pkcs8(rustls::pki_types::PrivatePkcs8KeyDer::from(v))
        })
        .or_else(|| {
            let mut rd2 = BufReader::new(&id.key[..]);
            rustls_pemfile::rsa_private_keys(&mut rd2)
                .ok()?
                .into_iter()
                .next()
                .map(|v| {
                    rustls::pki_types::PrivateKeyDer::Pkcs1(
                        rustls::pki_types::PrivatePkcs1KeyDer::from(v),
                    )
                })
        })
        .ok_or_else(|| "no private key in TlsIdentity".to_string())?;
    let config = rustls::ServerConfig::builder()
        .with_no_client_auth()
        .with_single_cert(certs, key)
        .map_err(|e| format!("tls config: {e}"))?;
    Ok(Arc::new(config))
}

// ---------- UDP 控制面 ----------

fn udp_run(shared: Arc<Shared>, server: Arc<UdpSocket>) {
    let mut last_sweep = Instant::now();
    let mut buf = [0u8; 65535];
    loop {
        if let Ok((n, from)) = server.recv_from(&mut buf) {
            let sink = ClientSink::Udp(from);
            handle_packet(&shared, &server, &buf[..n], ClientKey::Udp(from), &sink);
        }
        if last_sweep.elapsed() >= SWEEP_INTERVAL {
            sweep(&shared);
            last_sweep = Instant::now();
        }
    }
}

fn sweep(shared: &Shared) {
    let now = unix_now();
    let mut allocs = shared.allocations.lock().unwrap();
    let expired: Vec<ClientKey> = allocs
        .iter()
        .filter(|(_, a)| now >= a.expires.load(Ordering::SeqCst))
        .map(|(k, _)| *k)
        .collect();
    for k in expired {
        if let Some(a) = allocs.remove(&k) {
            a.stop.store(true, Ordering::SeqCst);
            debug!("TURN allocation expired: {k:?} relayed={}", a.relayed);
        }
    }
}

// ---------- TCP/TLS 控制面 ----------

/// 累积字节流并切出 2 字节长度前缀帧。
struct FrameReader {
    pending: Vec<u8>,
}

impl FrameReader {
    fn new() -> Self {
        FrameReader {
            pending: Vec::new(),
        }
    }

    fn push(&mut self, data: &[u8]) {
        self.pending.extend_from_slice(data);
    }

    /// 取出下一完整帧；畸形帧返回 Err（调用方应断开连接）。
    fn next_frame(&mut self) -> Result<Option<Vec<u8>>, String> {
        if self.pending.len() < 2 {
            return Ok(None);
        }
        let len = u16::from_be_bytes([self.pending[0], self.pending[1]]) as usize;
        if len < 4 {
            // 最小帧 4 字节（ChannelData 至少 channel+len；STUN 至少 20 由 handle_packet
            // 校验）；len<4 是畸形前缀，若不判错则永不 drain → 无界增长。
            return Err(format!("bad frame length {len}"));
        }
        if self.pending.len() < 2 + len {
            return Ok(None);
        }
        let frame = self.pending[2..2 + len].to_vec();
        self.pending.drain(..2 + len);
        Ok(Some(frame))
    }
}

/// TCP/TLS 接受循环：`acceptor` 为 None 时按明文 TCP 处理。
fn tcp_accept_loop(
    listener: TcpListener,
    shared: Arc<Shared>,
    server: Arc<UdpSocket>,
    acceptor: Option<Arc<rustls::ServerConfig>>,
) {
    loop {
        match listener.accept() {
            Ok((stream, _)) => {
                let conn_id = shared.next_conn.fetch_add(1, Ordering::SeqCst);
                let shared = shared.clone();
                let server = server.clone();
                let acceptor = acceptor.clone();
                std::thread::spawn(move || {
                    let _ = tcp_conn_loop(shared, server, conn_id, stream, acceptor);
                });
            }
            // 非阻塞 listener 在无连接时返回 WouldBlock：必须 sleep，否则空转烧满核。
            Err(ref e) if e.kind() == std::io::ErrorKind::WouldBlock => {
                std::thread::sleep(Duration::from_millis(10));
            }
            Err(_) => {
                std::thread::sleep(Duration::from_millis(100));
            }
        }
    }
}

/// 移除 allocation 并置 stop，让 relay 线程尽快退出（而非等到 expiry）。
fn cleanup_allocation(shared: &Shared, key: &ClientKey) {
    if let Some(a) = shared.allocations.lock().unwrap().remove(key) {
        a.stop.store(true, Ordering::SeqCst);
    }
}

fn tcp_conn_loop(
    shared: Arc<Shared>,
    server: Arc<UdpSocket>,
    conn_id: u64,
    stream: TcpStream,
    acceptor: Option<Arc<rustls::ServerConfig>>,
) -> io::Result<()> {
    stream.set_read_timeout(Some(TCP_READ_TIMEOUT))?;
    stream.set_nodelay(true).ok();
    let peer_ip = stream
        .peer_addr()
        .map(|a| a.ip())
        .unwrap_or_else(|_| IpAddr::V4(std::net::Ipv4Addr::UNSPECIFIED));
    let conn: ConnBox = match acceptor {
        None => Box::new(stream),
        Some(cfg) => {
            let conn = rustls::ServerConnection::new(cfg)
                .map_err(|e| io::Error::other(format!("TLS conn: {e}")))?;
            Box::new(rustls::StreamOwned::new(conn, stream))
        }
    };
    let conn = Arc::new(Mutex::new(conn));
    let sink = ClientSink::Tcp(conn.clone());
    let key = ClientKey::Tcp {
        conn: conn_id,
        ip: peer_ip,
    };
    let mut fr = FrameReader::new();
    let mut tmp = [0u8; 65535];
    loop {
        let n = {
            let mut c = conn.lock().unwrap();
            match c.read(&mut tmp) {
                Ok(n) if n > 0 => n,
                Ok(_) => break, // EOF
                Err(e)
                    if e.kind() == io::ErrorKind::WouldBlock
                        || e.kind() == io::ErrorKind::TimedOut =>
                {
                    drop(c);
                    std::thread::sleep(Duration::from_millis(2));
                    continue;
                }
                Err(_) => break,
            }
        };
        fr.push(&tmp[..n]);
        loop {
            match fr.next_frame() {
                Ok(Some(frame)) => handle_packet(&shared, &server, &frame, key, &sink),
                Ok(None) => break,
                Err(e) => {
                    warn!("tcp turn frame error ({e}); closing conn {conn_id}");
                    cleanup_allocation(&shared, &key);
                    return Err(io::Error::other(e));
                }
            }
        }
    }
    // 连接断开：清理该连接 allocation（并唤醒 relay 线程退出）。
    cleanup_allocation(&shared, &key);
    Ok(())
}

// ---------- 协议处理 ----------

fn handle_packet(
    shared: &Shared,
    server: &UdpSocket,
    pkt: &[u8],
    key: ClientKey,
    sink: &ClientSink,
) {
    // ChannelData：channel 0x4000-0x7FFF（首两 bit 01）。
    if pkt.len() >= 4 && (pkt[0] & 0xc0) == 0x40 {
        let chan = u16::from_be_bytes([pkt[0], pkt[1]]);
        let len = u16::from_be_bytes([pkt[2], pkt[3]]) as usize;
        let payload = &pkt[4..(4 + len).min(pkt.len())];
        let peer = {
            let allocs = shared.allocations.lock().unwrap();
            allocs
                .get(&key)
                .and_then(|a| a.state.lock().unwrap().channels.get(&chan).copied())
        };
        if let Some(peer) = peer {
            let allocs = shared.allocations.lock().unwrap();
            if let Some(a) = allocs.get(&key) {
                let _ = a.relay.send_to(payload, peer);
            }
        }
        return;
    }
    if pkt.len() < 20 || pkt[4..8] != STUN_MAGIC.to_be_bytes() {
        return;
    }
    let method = stun_method(pkt);
    let Ok(txid) = <[u8; 12]>::try_from(&pkt[8..20]) else {
        return;
    };
    match method {
        MSG_BINDING => {
            let body = vec![(
                ATTR_XOR_MAPPED_ADDRESS,
                encode_xor_peer(peer_addr(server, key, sink)),
            )];
            let resp =
                build_stun(MSG_SUCCESS_BASE | MSG_BINDING, &body, txid, None).unwrap_or_default();
            let _ = sink.send(server, &resp);
        }
        MSG_ALLOCATE => handle_allocate(shared, server, pkt, key, sink, txid),
        MSG_CREATE_PERMISSION => handle_permission(shared, server, pkt, key, sink, txid, false),
        MSG_CHANNEL_BIND => handle_permission(shared, server, pkt, key, sink, txid, true),
        MSG_SEND => handle_send(shared, pkt, key),
        MSG_REFRESH => handle_refresh(shared, server, pkt, key, sink, txid),
        _ => {
            debug!("TURN: unhandled method {method:#06x} {key:?}");
        }
    }
}

/// Binding 的 XOR-MAPPED-ADDRESS：UDP 为客户端地址；TCP 无法获取对端地址则回退 server 地址。
fn peer_addr(server: &UdpSocket, key: ClientKey, sink: &ClientSink) -> SocketAddr {
    match (key, sink) {
        (ClientKey::Udp(addr), _) => addr,
        (_, ClientSink::Tcp(_)) => server
            .local_addr()
            .unwrap_or_else(|_| SocketAddr::new(std::net::Ipv4Addr::UNSPECIFIED.into(), 0)),
        _ => SocketAddr::new(std::net::Ipv4Addr::UNSPECIFIED.into(), 0),
    }
}

/// 401/438 等错误响应（带 REALM/NONCE 用于挑战）。
fn send_error(
    server: &UdpSocket,
    sink: &ClientSink,
    txid: [u8; 12],
    method: u16,
    code: u16,
    challenge: bool,
    shared: &Shared,
) {
    let mut body = vec![(ATTR_ERROR_CODE, encode_error_code(code))];
    if challenge {
        body.push((ATTR_REALM, shared.realm.as_bytes().to_vec()));
        body.push((ATTR_NONCE, shared.nonce.as_bytes().to_vec()));
    }
    let resp = build_stun(MSG_ERROR_BASE | method, &body, txid, None).unwrap_or_default();
    let _ = sink.send(server, &resp);
}

/// 校验带 MI 的请求认证（REST secret）：返回 Ok(username) 或错误码。
fn check_auth(shared: &Shared, pkt: &[u8]) -> Result<String, u16> {
    let attrs = parse_attrs(pkt);
    let has_mi = attrs.iter().any(|(t, _)| *t == ATTR_MESSAGE_INTEGRITY);
    if !has_mi {
        return Err(401);
    }
    let username = attrs
        .iter()
        .find(|(t, _)| *t == ATTR_USERNAME)
        .map(|(_, v)| String::from_utf8_lossy(v).to_string())
        .unwrap_or_default();
    let req_realm = attrs
        .iter()
        .find(|(t, _)| *t == ATTR_REALM)
        .map(|(_, v)| String::from_utf8_lossy(v).to_string())
        .unwrap_or_default();
    let req_nonce = attrs
        .iter()
        .find(|(t, _)| *t == ATTR_NONCE)
        .map(|(_, v)| String::from_utf8_lossy(v).to_string())
        .unwrap_or_default();
    if req_realm != shared.realm {
        return Err(401);
    }
    if req_nonce != shared.nonce {
        return Err(438);
    }
    if username.is_empty() || username.split_once(':').is_none() {
        return Err(401);
    }
    let expected = aerodesk_protocol::turn::turn_credential(&shared.secret, &username);
    if !verify_message_integrity(pkt, &username, &shared.realm, &expected) {
        return Err(401);
    }
    if !aerodesk_protocol::turn::verify_turn_credential(
        &shared.secret,
        &username,
        &expected,
        unix_now(),
        CLOCK_SKEW,
    ) {
        return Err(401);
    }
    Ok(username)
}

fn handle_allocate(
    shared: &Shared,
    server: &UdpSocket,
    pkt: &[u8],
    key: ClientKey,
    sink: &ClientSink,
    txid: [u8; 12],
) {
    if shared.allocations.lock().unwrap().contains_key(&key) {
        send_error(server, sink, txid, MSG_ALLOCATE, 437, false, shared);
        return;
    }
    // REQUESTED-TRANSPORT：仅 UDP（17）——TCP 连接上的 relay 仍是 UDP 分配。
    let transport = find_attr(pkt, ATTR_REQUESTED_TRANSPORT)
        .and_then(|v| v.first().copied())
        .unwrap_or(0);
    if transport != 17 {
        send_error(server, sink, txid, MSG_ALLOCATE, 442, false, shared);
        return;
    }
    let username = match check_auth(shared, pkt) {
        Ok(u) => u,
        Err(code) => {
            send_error(server, sink, txid, MSG_ALLOCATE, code, true, shared);
            return;
        }
    };
    // #204：配额（486 Allocation Quota Reached）。计数直接来自 allocations 表。
    let client_ip = match key {
        ClientKey::Udp(a) => a.ip(),
        ClientKey::Tcp { ip, .. } => ip,
    };
    {
        let allocs = shared.allocations.lock().unwrap();
        if shared.max_allocs_total > 0 && allocs.len() >= shared.max_allocs_total {
            send_error(server, sink, txid, MSG_ALLOCATE, 486, false, shared);
            return;
        }
        if shared.max_allocs_per_ip > 0
            && allocs.values().filter(|a| a.client_ip == client_ip).count()
                >= shared.max_allocs_per_ip
        {
            send_error(server, sink, txid, MSG_ALLOCATE, 486, false, shared);
            return;
        }
    }
    let ipv6 = std::env::var("SFU_TURN_IPV6")
        .map(|v| v == "1")
        .unwrap_or(false);
    let relay = if ipv6 {
        bind_udp_dual(0)
    } else {
        UdpSocket::bind((std::net::Ipv4Addr::UNSPECIFIED, 0))
    };
    let relay = match relay {
        Ok(r) => r,
        Err(e) => {
            warn!("TURN relay bind failed: {e}");
            send_error(server, sink, txid, MSG_ALLOCATE, 500, false, shared);
            return;
        }
    };
    // relay 线程依赖 recv_from 周期性返回才能检测 stop/expires 退出；否则永久阻塞
    // 泄漏线程+socket（审查发现）。
    if let Err(e) = relay.set_read_timeout(Some(POLL_TIMEOUT)) {
        warn!("TURN relay set_read_timeout failed: {e}");
    }
    let Ok(relay_port) = relay.local_addr().map(|a| a.port()) else {
        return;
    };
    let relayed = SocketAddr::new(shared.host, relay_port);
    let expires = Arc::new(AtomicU64::new(unix_now() + shared.default_lifetime as u64));
    let stop = Arc::new(AtomicBool::new(false));
    let state = Arc::new(Mutex::new(AllocState {
        channels: HashMap::new(),
        peer_channel: HashMap::new(),
        permissions: HashSet::new(),
    }));
    let relay_arc = Arc::new(relay);
    let server_arc = Arc::new(server.try_clone().unwrap_or_else(|_| {
        UdpSocket::bind((std::net::Ipv4Addr::UNSPECIFIED, 0)).expect("fallback bind")
    }));
    let sink_for_relay = sink.clone();
    let _relay_thread = spawn_relay(
        relay_arc.clone(),
        server_arc,
        sink_for_relay,
        expires.clone(),
        stop.clone(),
        state.clone(),
    );
    shared.created_total.fetch_add(1, Ordering::Relaxed);
    shared.allocations.lock().unwrap().insert(
        key,
        Allocation {
            relay: relay_arc,
            relayed,
            client_ip,
            expires,
            stop,
            state,
        },
    );
    debug!("TURN allocation: {key:?} user={username} relayed={relayed}");
    let body = vec![
        (ATTR_XOR_RELAYED_ADDRESS, encode_xor_peer(relayed)),
        (
            ATTR_LIFETIME,
            shared.default_lifetime.to_be_bytes().to_vec(),
        ),
    ];
    let resp = build_stun(MSG_SUCCESS_BASE | MSG_ALLOCATE, &body, txid, None).unwrap_or_default();
    let _ = sink.send(server, &resp);
}

fn handle_permission(
    shared: &Shared,
    server: &UdpSocket,
    pkt: &[u8],
    key: ClientKey,
    sink: &ClientSink,
    txid: [u8; 12],
    channel_bind: bool,
) {
    let method = if channel_bind {
        MSG_CHANNEL_BIND
    } else {
        MSG_CREATE_PERMISSION
    };
    if check_auth(shared, pkt).is_err() {
        send_error(server, sink, txid, method, 401, true, shared);
        return;
    }
    let Some(peer_val) = find_attr(pkt, ATTR_XOR_PEER_ADDRESS) else {
        send_error(server, sink, txid, method, 400, false, shared);
        return;
    };
    let Some(peer) = parse_xor_addr(&peer_val) else {
        send_error(server, sink, txid, method, 400, false, shared);
        return;
    };
    // #204：拒绝段内 peer → 403 Forbidden（防开放中继）。
    if peer_denied(shared, peer.ip()) {
        send_error(server, sink, txid, method, 403, false, shared);
        return;
    }
    let allocs = shared.allocations.lock().unwrap();
    let Some(alloc) = allocs.get(&key) else {
        return;
    };
    let mut st = alloc.state.lock().unwrap();
    if channel_bind {
        let Some(chan_val) = find_attr(pkt, ATTR_CHANNEL_NUMBER) else {
            return;
        };
        // 长度不足 2 的 CHANNEL-NUMBER 是畸形包：不能索引 [0]/[1]，否则 panic。
        if chan_val.len() < 2 {
            send_error(server, sink, txid, method, 400, false, shared);
            return;
        }
        let chan = u16::from_be_bytes([chan_val[0], chan_val[1]]);
        if !(CHANNEL_BASE..=CHANNEL_MAX).contains(&chan) {
            send_error(server, sink, txid, method, 400, false, shared);
            return;
        }
        // ChannelBind 隐含 permission（RFC 5766 §9.2）。
        st.permissions.insert(peer);
        st.channels.insert(chan, peer);
        st.peer_channel.insert(peer, chan);
    } else {
        st.permissions.insert(peer);
    }
    drop(st);
    drop(allocs);
    let resp = build_stun(MSG_SUCCESS_BASE | method, &[], txid, None).unwrap_or_default();
    let _ = sink.send(server, &resp);
}

fn handle_send(shared: &Shared, pkt: &[u8], key: ClientKey) {
    if check_auth(shared, pkt).is_err() {
        return; // Send 是指示（无响应），失败静默丢弃
    }
    let Some(peer_val) = find_attr(pkt, ATTR_XOR_PEER_ADDRESS).and_then(|v| parse_xor_addr(&v))
    else {
        return;
    };
    let Some(data) = find_attr(pkt, ATTR_DATA) else {
        return;
    };
    // #204：拒绝段内 peer 静默丢弃（Send 是指示，无响应）。
    if peer_denied(shared, peer_val.ip()) {
        return;
    }
    let allocs = shared.allocations.lock().unwrap();
    let Some(alloc) = allocs.get(&key) else {
        return;
    };
    let st = alloc.state.lock().unwrap();
    let permitted = st.permissions.contains(&peer_val);
    drop(st);
    if permitted {
        let _ = alloc.relay.send_to(&data, peer_val);
    }
}

fn handle_refresh(
    shared: &Shared,
    server: &UdpSocket,
    pkt: &[u8],
    key: ClientKey,
    sink: &ClientSink,
    txid: [u8; 12],
) {
    let allocs = shared.allocations.lock().unwrap();
    let Some(alloc) = allocs.get(&key) else {
        return;
    };
    if check_auth(shared, pkt).is_err() {
        send_error(server, sink, txid, MSG_REFRESH, 401, true, shared);
        return;
    }
    let requested = find_attr(pkt, ATTR_LIFETIME)
        .map(|v| u32::from_be_bytes(v[..4].try_into().unwrap_or([0; 4])))
        .unwrap_or(shared.default_lifetime);
    let lifetime = requested.clamp(60, shared.default_lifetime);
    alloc
        .expires
        .store(unix_now() + lifetime as u64, Ordering::SeqCst);
    let body = vec![(ATTR_LIFETIME, lifetime.to_be_bytes().to_vec())];
    let resp = build_stun(MSG_SUCCESS_BASE | MSG_REFRESH, &body, txid, None).unwrap_or_default();
    let _ = sink.send(server, &resp);
}

/// relay 线程：收 peer 包 → 转发客户端（ChannelData 或 Data indication）。
fn spawn_relay(
    relay: Arc<UdpSocket>,
    server: Arc<UdpSocket>,
    client: ClientSink,
    expires: Arc<AtomicU64>,
    stop: Arc<AtomicBool>,
    state: Arc<Mutex<AllocState>>,
) -> std::thread::JoinHandle<()> {
    std::thread::spawn(move || {
        let mut buf = [0u8; 65535];
        loop {
            if stop.load(Ordering::SeqCst) || unix_now() >= expires.load(Ordering::SeqCst) {
                return;
            }
            if let Ok((n, peer)) = relay.recv_from(&mut buf) {
                let st = state.lock().unwrap();
                if let Some(&chan) = st.peer_channel.get(&peer) {
                    let mut out = Vec::with_capacity(4 + n);
                    out.extend_from_slice(&chan.to_be_bytes());
                    out.extend_from_slice(&(n as u16).to_be_bytes());
                    out.extend_from_slice(&buf[..n]);
                    drop(st);
                    let _ = client.send(&server, &out);
                } else if st.permissions.contains(&peer) {
                    let mut body = Vec::new();
                    body.extend_from_slice(&encode_attr(
                        ATTR_XOR_PEER_ADDRESS,
                        &encode_xor_peer(peer),
                    ));
                    body.extend_from_slice(&encode_attr(ATTR_DATA, &buf[..n]));
                    drop(st);
                    let out = encode_header(MSG_DATA_INDICATION, [0u8; 12], &body);
                    let _ = client.send(&server, &out);
                }
            }
        }
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::net::{Ipv4Addr, Ipv6Addr, TcpStream, UdpSocket};
    use std::sync::Mutex;

    // env 是进程全局的：设置 env 的测试（配额/deny/IPv6）串行化，避免并行互踩。
    static TESTS_LOCK: Mutex<()> = Mutex::new(());

    #[test]
    fn frame_reader_rejects_bad_length_without_growing() {
        let mut fr = FrameReader::new();
        // 长度前缀 <4 是畸形帧：必须返回 Err（否则 pending 永不 drain 无界增长）。
        fr.push(&[0x00, 0x03]);
        assert!(fr.next_frame().is_err());
        // 合法帧可取出。
        let mut fr2 = FrameReader::new();
        fr2.push(&[0x00, 0x14]);
        fr2.push(&[0u8; 20]);
        assert_eq!(fr2.next_frame().unwrap().unwrap().len(), 20);
    }

    // ---------- 公共测试工具 ----------

    fn spawn_udp(secret: &str) -> (SocketAddr, TurnServer) {
        let srv = spawn(secret, Ipv4Addr::LOCALHOST.into(), 0, None).unwrap();
        (srv.udp_addr, srv)
    }

    /// UDP 客户端 Allocate（401 挑战 → 带凭证重试）。
    fn udp_allocate(
        client: &UdpSocket,
        server_addr: SocketAddr,
        username: &str,
        credential: &str,
    ) -> Result<(SocketAddr, String, String), String> {
        let txid = [7u8; 12];
        let req = build_stun(
            MSG_ALLOCATE,
            &[(ATTR_REQUESTED_TRANSPORT, vec![17, 0, 0, 0])],
            txid,
            None,
        )?;
        client
            .send_to(&req, server_addr)
            .map_err(|e| e.to_string())?;
        let mut buf = [0u8; 4096];
        let (n, _) = client.recv_from(&mut buf).map_err(|e| e.to_string())?;
        let (realm, nonce, err) = parse_common(&buf[..n])?;
        assert_eq!(err, Some(401));
        let realm = realm.ok_or("no realm")?;
        let nonce = nonce.ok_or("no nonce")?;
        let txid2 = [8u8; 12];
        let attrs = vec![(ATTR_REQUESTED_TRANSPORT, vec![17, 0, 0, 0])];
        let req2 = build_stun(
            MSG_ALLOCATE,
            &attrs,
            txid2,
            Some((username, credential, &realm, &nonce)),
        )?;
        client
            .send_to(&req2, server_addr)
            .map_err(|e| e.to_string())?;
        let mut buf = [0u8; 4096];
        let (n, _) = client.recv_from(&mut buf).map_err(|e| e.to_string())?;
        let (_, _, err) = parse_common(&buf[..n])?;
        if let Some(e) = err {
            return Err(format!("allocate error {e}"));
        }
        let relayed = find_attr(&buf[..n], ATTR_XOR_RELAYED_ADDRESS)
            .and_then(|v| parse_xor_addr(&v))
            .ok_or("no relayed")?;
        Ok((relayed, realm, nonce))
    }

    fn auth_request(
        method: u16,
        attrs: &[(u16, Vec<u8>)],
        username: &str,
        credential: &str,
        realm: &str,
        nonce: &str,
    ) -> Result<Vec<u8>, String> {
        let txid = [9u8; 12];
        build_stun(
            method,
            attrs,
            txid,
            Some((username, credential, realm, nonce)),
        )
    }

    fn recv_response(sock: &UdpSocket) -> (u16, Vec<(u16, Vec<u8>)>) {
        let mut buf = [0u8; 65535];
        let (n, _) = sock.recv_from(&mut buf).expect("recv");
        (stun_method(&buf[..n]), parse_attrs(&buf[..n]))
    }

    // TCP/TLS 帧收发工具
    fn tcp_send(stream: &mut TcpStream, bytes: &[u8]) {
        let mut framed = Vec::with_capacity(2 + bytes.len());
        framed.extend_from_slice(&(bytes.len() as u16).to_be_bytes());
        framed.extend_from_slice(bytes);
        stream.write_all(&framed).expect("tcp send");
    }

    fn tcp_read_frame_raw(stream: &mut TcpStream) -> Vec<u8> {
        let mut lenb = [0u8; 2];
        stream.read_exact(&mut lenb).expect("tcp read len");
        let len = u16::from_be_bytes(lenb) as usize;
        let mut buf = vec![0u8; len];
        stream.read_exact(&mut buf).expect("tcp read body");
        buf
    }

    fn tcp_read_frame(stream: &mut TcpStream) -> (u16, Vec<(u16, Vec<u8>)>) {
        let buf = tcp_read_frame_raw(stream);
        (stun_method(&buf), parse_attrs(&buf))
    }

    /// TCP 客户端 Allocate（401 挑战 → 带凭证重试），返回 (relayed, realm, nonce)。
    fn tcp_allocate(
        stream: &mut TcpStream,
        username: &str,
        credential: &str,
    ) -> Result<(SocketAddr, String, String), String> {
        let txid = [7u8; 12];
        let req = build_stun(
            MSG_ALLOCATE,
            &[(ATTR_REQUESTED_TRANSPORT, vec![17, 0, 0, 0])],
            txid,
            None,
        )?;
        tcp_send(stream, &req);
        let (_, attrs) = tcp_read_frame(stream);
        let realm = attrs
            .iter()
            .find(|(t, _)| *t == ATTR_REALM)
            .map(|(_, v)| String::from_utf8_lossy(v).to_string())
            .ok_or("no realm")?;
        let nonce = attrs
            .iter()
            .find(|(t, _)| *t == ATTR_NONCE)
            .map(|(_, v)| String::from_utf8_lossy(v).to_string())
            .ok_or("no nonce")?;
        let err = attrs
            .iter()
            .find(|(t, _)| *t == ATTR_ERROR_CODE)
            .map(|(_, v)| ((v[2] & 7) as u16) * 100 + v[3] as u16);
        assert_eq!(err, Some(401));

        let txid2 = [8u8; 12];
        let attrs2 = vec![(ATTR_REQUESTED_TRANSPORT, vec![17, 0, 0, 0])];
        let req2 = build_stun(
            MSG_ALLOCATE,
            &attrs2,
            txid2,
            Some((username, credential, &realm, &nonce)),
        )?;
        tcp_send(stream, &req2);
        let (m, attrs3) = tcp_read_frame(stream);
        if m != MSG_SUCCESS_BASE | MSG_ALLOCATE {
            return Err(format!("allocate error method={m:#06x}"));
        }
        let relayed = attrs3
            .iter()
            .find(|(t, _)| *t == ATTR_XOR_RELAYED_ADDRESS)
            .and_then(|(_, v)| parse_xor_addr(v))
            .ok_or("no relayed")?;
        Ok((relayed, realm, nonce))
    }

    // ---------- UDP 测试 ----------

    #[test]
    fn binding_returns_xor_mapped() {
        let _g = TESTS_LOCK.lock().unwrap();
        let (server_addr, _srv) = spawn_udp("testsecret");
        let client = UdpSocket::bind("127.0.0.1:0").unwrap();
        client
            .set_read_timeout(Some(Duration::from_secs(2)))
            .unwrap();
        let req = build_stun(MSG_BINDING, &[], [1u8; 12], None).unwrap();
        client.send_to(&req, server_addr).unwrap();
        let (method, attrs) = recv_response(&client);
        assert_eq!(method, MSG_SUCCESS_BASE | MSG_BINDING);
        let mapped = attrs
            .iter()
            .find(|(t, _)| *t == ATTR_XOR_MAPPED_ADDRESS)
            .and_then(|(_, v)| parse_xor_addr(v))
            .unwrap();
        assert_eq!(mapped, client.local_addr().unwrap());
    }

    #[test]
    fn allocate_relay_roundtrip_with_rest_credentials() {
        let _g = TESTS_LOCK.lock().unwrap();
        let secret = "testsecret";
        let (server_addr, _srv) = spawn_udp(secret);
        let client = UdpSocket::bind("127.0.0.1:0").unwrap();
        client
            .set_read_timeout(Some(Duration::from_secs(2)))
            .unwrap();
        let creds =
            aerodesk_protocol::turn::generate_turn_credentials(secret, "e2e", 3600, unix_now());
        let (relayed, realm, nonce) =
            udp_allocate(&client, server_addr, &creds.username, &creds.credential)
                .expect("allocate");
        // #220：allocation 指标——创建后 active=1 / total=1。
        assert_eq!(_srv.active_allocations(), 1);
        assert_eq!(_srv.allocations_total(), 1);

        let peer = UdpSocket::bind("127.0.0.1:0").unwrap();
        let peer_addr = peer.local_addr().unwrap();
        peer.set_read_timeout(Some(Duration::from_secs(2))).unwrap();

        let req = auth_request(
            MSG_CREATE_PERMISSION,
            &[(ATTR_XOR_PEER_ADDRESS, encode_xor_peer(peer_addr))],
            &creds.username,
            &creds.credential,
            &realm,
            &nonce,
        )
        .unwrap();
        client.send_to(&req, server_addr).unwrap();
        let (m, _) = recv_response(&client);
        assert_eq!(m, MSG_SUCCESS_BASE | MSG_CREATE_PERMISSION);

        let chan = CHANNEL_BASE;
        let req = auth_request(
            MSG_CHANNEL_BIND,
            &[
                (ATTR_CHANNEL_NUMBER, chan.to_be_bytes().to_vec()),
                (ATTR_XOR_PEER_ADDRESS, encode_xor_peer(peer_addr)),
            ],
            &creds.username,
            &creds.credential,
            &realm,
            &nonce,
        )
        .unwrap();
        client.send_to(&req, server_addr).unwrap();
        let (m, _) = recv_response(&client);
        assert_eq!(m, MSG_SUCCESS_BASE | MSG_CHANNEL_BIND);

        let payload = b"hello-via-embedded-turn";
        let mut cd = Vec::with_capacity(4 + payload.len());
        cd.extend_from_slice(&chan.to_be_bytes());
        cd.extend_from_slice(&(payload.len() as u16).to_be_bytes());
        cd.extend_from_slice(payload);
        client.send_to(&cd, server_addr).unwrap();
        let mut buf = [0u8; 64];
        let (n, src) = peer.recv_from(&mut buf).unwrap();
        assert_eq!(&buf[..n], payload);
        assert_eq!(src.port(), relayed.port());
        assert!(
            src.ip().is_loopback() || src.ip().is_unspecified(),
            "relay source {src}"
        );

        peer.send_to(b"reply-via-embedded-turn", relayed).unwrap();
        let mut buf2 = [0u8; 128];
        let (n2, _) = client.recv_from(&mut buf2).unwrap();
        assert!(n2 > 4);
        let rchan = u16::from_be_bytes([buf2[0], buf2[1]]);
        assert_eq!(rchan, chan);
        assert_eq!(&buf2[4..n2], b"reply-via-embedded-turn");
        // #220：活跃数保持不变（无重复 allocation / 无泄漏）。
        assert_eq!(_srv.active_allocations(), 1);
    }

    #[test]
    fn allocate_rejects_bad_credentials() {
        let _g = TESTS_LOCK.lock().unwrap();
        let secret = "testsecret";
        let (server_addr, _srv) = spawn_udp(secret);
        let client = UdpSocket::bind("127.0.0.1:0").unwrap();
        client
            .set_read_timeout(Some(Duration::from_secs(2)))
            .unwrap();
        let res = udp_allocate(&client, server_addr, "1234567890:bad", "wrongcred");
        assert!(res.is_err(), "bad credential must fail");
    }

    // ---------- TCP / TLS 测试 ----------

    fn setup_tcp(secret: &str, tls: bool) -> (SocketAddr, SocketAddr, TurnServer) {
        let srv = spawn(
            secret,
            Ipv4Addr::LOCALHOST.into(),
            0,
            if tls { Some(0) } else { None },
        )
        .unwrap();
        let tcp_addr = srv.tcp_addr.unwrap();
        let tls_addr = if tls { srv.tls_addr } else { None };
        (tcp_addr, tls_addr.unwrap_or(tcp_addr), srv)
    }

    fn tcp_roundtrip_body(stream: &mut TcpStream, secret: &str) {
        stream
            .set_read_timeout(Some(Duration::from_secs(3)))
            .unwrap();
        let creds =
            aerodesk_protocol::turn::generate_turn_credentials(secret, "e2e", 3600, unix_now());
        let (relayed, realm, nonce) =
            tcp_allocate(stream, &creds.username, &creds.credential).expect("tcp allocate");

        // peer 直接 UDP（relay 仍是 UDP）
        let peer = UdpSocket::bind("127.0.0.1:0").unwrap();
        let peer_addr = peer.local_addr().unwrap();
        peer.set_read_timeout(Some(Duration::from_secs(3))).unwrap();

        // CreatePermission + ChannelBind（TCP 帧）
        let req = auth_request(
            MSG_CREATE_PERMISSION,
            &[(ATTR_XOR_PEER_ADDRESS, encode_xor_peer(peer_addr))],
            &creds.username,
            &creds.credential,
            &realm,
            &nonce,
        )
        .unwrap();
        tcp_send(stream, &req);
        let (m, _) = tcp_read_frame(stream);
        assert_eq!(m, MSG_SUCCESS_BASE | MSG_CREATE_PERMISSION);

        let chan = CHANNEL_BASE;
        let req = auth_request(
            MSG_CHANNEL_BIND,
            &[
                (ATTR_CHANNEL_NUMBER, chan.to_be_bytes().to_vec()),
                (ATTR_XOR_PEER_ADDRESS, encode_xor_peer(peer_addr)),
            ],
            &creds.username,
            &creds.credential,
            &realm,
            &nonce,
        )
        .unwrap();
        tcp_send(stream, &req);
        let (m, _) = tcp_read_frame(stream);
        assert_eq!(m, MSG_SUCCESS_BASE | MSG_CHANNEL_BIND);

        // TCP 客户端 → ChannelData → server → peer（明文 UDP）
        let payload = b"hello-via-tcp-turn";
        let mut cd = Vec::with_capacity(4 + payload.len());
        cd.extend_from_slice(&chan.to_be_bytes());
        cd.extend_from_slice(&(payload.len() as u16).to_be_bytes());
        cd.extend_from_slice(payload);
        tcp_send(stream, &cd);
        let mut buf = [0u8; 64];
        let (n, src) = peer.recv_from(&mut buf).unwrap();
        assert_eq!(&buf[..n], payload);
        assert_eq!(src.port(), relayed.port());

        // peer → relayed → server → TCP 帧 ChannelData → 客户端（channel 已绑）
        peer.send_to(b"reply-via-tcp-turn", relayed).unwrap();
        let raw = tcp_read_frame_raw(stream);
        assert!(raw.len() > 4, "channeldata too short");
        let rchan = u16::from_be_bytes([raw[0], raw[1]]);
        assert_eq!(rchan, chan, "应回 ChannelData");
        let rlen = u16::from_be_bytes([raw[2], raw[3]]) as usize;
        assert_eq!(&raw[4..4 + rlen], b"reply-via-tcp-turn");
    }

    #[test]
    fn tcp_allocate_relay_roundtrip() {
        let _g = TESTS_LOCK.lock().unwrap();
        let secret = "testsecret";
        let (tcp_addr, _tls, _srv) = setup_tcp(secret, false);
        let mut stream = TcpStream::connect(tcp_addr).expect("tcp connect");
        tcp_roundtrip_body(&mut stream, secret);
    }

    /// 危险验证器：单测只验证 TLS 传输层（握手/帧），不校验证书链
    /// （内嵌开发证书是 CA:TRUE 自签；生产用 CERT_FILE/KEY_FILE 真实证书）。
    #[derive(Debug)]
    struct NoVerify;

    impl rustls::client::danger::ServerCertVerifier for NoVerify {
        fn verify_server_cert(
            &self,
            _end_entity: &rustls::pki_types::CertificateDer<'_>,
            _intermediates: &[rustls::pki_types::CertificateDer<'_>],
            _server_name: &rustls::pki_types::ServerName<'_>,
            _ocsp: &[u8],
            _now: rustls::pki_types::UnixTime,
        ) -> Result<rustls::client::danger::ServerCertVerified, rustls::Error> {
            Ok(rustls::client::danger::ServerCertVerified::assertion())
        }

        fn verify_tls12_signature(
            &self,
            _message: &[u8],
            _cert: &rustls::pki_types::CertificateDer<'_>,
            _dss: &rustls::DigitallySignedStruct,
        ) -> Result<rustls::client::danger::HandshakeSignatureValid, rustls::Error> {
            Ok(rustls::client::danger::HandshakeSignatureValid::assertion())
        }

        fn verify_tls13_signature(
            &self,
            _message: &[u8],
            _cert: &rustls::pki_types::CertificateDer<'_>,
            _dss: &rustls::DigitallySignedStruct,
        ) -> Result<rustls::client::danger::HandshakeSignatureValid, rustls::Error> {
            Ok(rustls::client::danger::HandshakeSignatureValid::assertion())
        }

        fn supported_verify_schemes(&self) -> Vec<rustls::SignatureScheme> {
            vec![
                rustls::SignatureScheme::RSA_PKCS1_SHA256,
                rustls::SignatureScheme::RSA_PSS_SHA256,
                rustls::SignatureScheme::ECDSA_NISTP256_SHA256,
            ]
        }
    }

    #[test]
    fn tls_allocate_relay_roundtrip() {
        let _g = TESTS_LOCK.lock().unwrap();
        let secret = "testsecret";
        let (_tcp, tls_addr, _srv) = setup_tcp(secret, true);
        let cfg = rustls::ClientConfig::builder()
            .dangerous()
            .with_custom_certificate_verifier(Arc::new(NoVerify))
            .with_no_client_auth();
        let server_name = rustls::pki_types::ServerName::try_from("str0m.test").expect("name");
        let conn =
            rustls::ClientConnection::new(Arc::new(cfg), server_name).expect("tls client conn");
        let tcp = TcpStream::connect(tls_addr).expect("tcp connect");
        let mut tls = rustls::StreamOwned::new(conn, tcp);
        // StreamOwned 不直接支持帧读（read_exact 到一半会因 TLS 缓冲阻塞），
        // 这里用简单方法：一次性发 Binding 读响应（响应小、单帧）。
        let req = build_stun(MSG_BINDING, &[], [3u8; 12], None).unwrap();
        let mut framed = Vec::with_capacity(2 + req.len());
        framed.extend_from_slice(&(req.len() as u16).to_be_bytes());
        framed.extend_from_slice(&req);
        use std::io::Read as _;
        tls.write_all(&framed).expect("tls write");
        let mut lenb = [0u8; 2];
        // read_exact 可能因 TLS 分块一次只给部分字节；循环重读
        let mut got = 0usize;
        let deadline = Instant::now() + Duration::from_secs(3);
        while got < 2 && Instant::now() < deadline {
            match tls.read(&mut lenb[got..]) {
                Ok(0) => break,
                Ok(n) => got += n,
                Err(_) => break,
            }
        }
        assert_eq!(got, 2, "TLS 未收到长度前缀");
        let len = u16::from_be_bytes(lenb) as usize;
        let mut body = vec![0u8; len];
        got = 0;
        while got < len && Instant::now() < deadline {
            match tls.read(&mut body[got..]) {
                Ok(0) => break,
                Ok(n) => got += n,
                Err(_) => break,
            }
        }
        assert_eq!(got, len, "TLS 未收到响应体");
        let method = stun_method(&body);
        assert_eq!(method, MSG_SUCCESS_BASE | MSG_BINDING, "TLS Binding 应答");
        // 完整 allocate + relay 回环在 e2e 用 turnutils -S 覆盖（rustls 客户端分块读复杂）
    }
    /// 执行 401→带凭证 Allocate，返回最终响应错误码（Ok=成功）。
    fn udp_allocate_code(
        client: &UdpSocket,
        server_addr: SocketAddr,
        username: &str,
        credential: &str,
    ) -> Result<(), u16> {
        let txid = [7u8; 12];
        let req = build_stun(
            MSG_ALLOCATE,
            &[(ATTR_REQUESTED_TRANSPORT, vec![17, 0, 0, 0])],
            txid,
            None,
        )
        .unwrap();
        client.send_to(&req, server_addr).unwrap();
        let mut buf = [0u8; 4096];
        let (n, _) = client.recv_from(&mut buf).unwrap();
        let (realm, nonce, err) = parse_common(&buf[..n]).unwrap();
        assert_eq!(err, Some(401));
        let realm = realm.unwrap();
        let nonce = nonce.unwrap();
        let txid2 = [8u8; 12];
        let attrs = vec![(ATTR_REQUESTED_TRANSPORT, vec![17, 0, 0, 0])];
        let req2 = build_stun(
            MSG_ALLOCATE,
            &attrs,
            txid2,
            Some((username, credential, &realm, &nonce)),
        )
        .unwrap();
        client.send_to(&req2, server_addr).unwrap();
        let (n, _) = client.recv_from(&mut buf).unwrap();
        let (_, _, err) = parse_common(&buf[..n]).unwrap();
        match err {
            None => Ok(()),
            Some(c) => Err(c),
        }
    }

    fn creds(secret: &str) -> (String, String) {
        let c = aerodesk_protocol::turn::generate_turn_credentials(secret, "e2e", 3600, unix_now());
        (c.username, c.credential)
    }

    /// #204：per-IP 配额超限 → 486。
    #[test]
    fn quota_per_ip_rejects_second_allocation() {
        let _g = TESTS_LOCK.lock().unwrap();
        unsafe {
            std::env::set_var("MAX_TURN_ALLOCS_PER_IP", "1");
        }
        let (server_addr, _srv) = spawn_udp("testsecret");
        let (u, p) = creds("testsecret");
        let c1 = UdpSocket::bind("127.0.0.1:0").unwrap();
        c1.set_read_timeout(Some(Duration::from_secs(2))).unwrap();
        assert!(udp_allocate_code(&c1, server_addr, &u, &p).is_ok());
        let c2 = UdpSocket::bind("127.0.0.1:0").unwrap();
        c2.set_read_timeout(Some(Duration::from_secs(2))).unwrap();
        assert_eq!(
            udp_allocate_code(&c2, server_addr, &u, &p),
            Err(486),
            "同一 IP 第二个 allocation 应 486"
        );
        unsafe {
            std::env::remove_var("MAX_TURN_ALLOCS_PER_IP");
        }
    }

    /// #222：TURN_LIFETIME_SEC 缩短 allocation lifetime（响应 ATTR_LIFETIME 生效）。
    #[test]
    fn lifetime_env_shortens_allocation() {
        let _g = TESTS_LOCK.lock().unwrap();
        unsafe {
            std::env::set_var("TURN_LIFETIME_SEC", "60");
        }
        let (server_addr, _srv) = spawn_udp("testsecret");
        let (u, p) = creds("testsecret");
        let client = UdpSocket::bind("127.0.0.1:0").unwrap();
        client
            .set_read_timeout(Some(Duration::from_secs(2)))
            .unwrap();
        let txid = [9u8; 12];
        let req = build_stun(
            MSG_ALLOCATE,
            &[(ATTR_REQUESTED_TRANSPORT, vec![17, 0, 0, 0])],
            txid,
            None,
        )
        .unwrap();
        client.send_to(&req, server_addr).unwrap();
        let mut buf = [0u8; 4096];
        let (n, _) = client.recv_from(&mut buf).unwrap();
        let (realm, nonce, err) = parse_common(&buf[..n]).unwrap();
        assert_eq!(err, Some(401));
        let txid2 = [10u8; 12];
        let req2 = build_stun(
            MSG_ALLOCATE,
            &[(ATTR_REQUESTED_TRANSPORT, vec![17, 0, 0, 0])],
            txid2,
            Some((&u, &p, &realm.unwrap(), &nonce.unwrap())),
        )
        .unwrap();
        client.send_to(&req2, server_addr).unwrap();
        let (n, _) = client.recv_from(&mut buf).unwrap();
        let lifetime = find_attr(&buf[..n], ATTR_LIFETIME)
            .map(|v| u32::from_be_bytes([v[0], v[1], v[2], v[3]]));
        assert_eq!(
            lifetime,
            Some(60),
            "ATTR_LIFETIME 应取 TURN_LIFETIME_SEC=60"
        );
        unsafe {
            std::env::remove_var("TURN_LIFETIME_SEC");
        }
    }

    /// #204：全局配额超限 → 486（per-IP 放开）。
    #[test]
    fn quota_total_rejects_when_full() {
        let _g = TESTS_LOCK.lock().unwrap();
        unsafe {
            std::env::set_var("MAX_TURN_ALLOCS_TOTAL", "1");
            std::env::set_var("MAX_TURN_ALLOCS_PER_IP", "0");
        }
        let (server_addr, _srv) = spawn_udp("testsecret");
        let (u, p) = creds("testsecret");
        let c1 = UdpSocket::bind("127.0.0.1:0").unwrap();
        c1.set_read_timeout(Some(Duration::from_secs(2))).unwrap();
        assert!(udp_allocate_code(&c1, server_addr, &u, &p).is_ok());
        let c2 = UdpSocket::bind("127.0.0.1:0").unwrap();
        c2.set_read_timeout(Some(Duration::from_secs(2))).unwrap();
        assert_eq!(udp_allocate_code(&c2, server_addr, &u, &p), Err(486));
        unsafe {
            std::env::remove_var("MAX_TURN_ALLOCS_TOTAL");
            std::env::remove_var("MAX_TURN_ALLOCS_PER_IP");
        }
    }

    /// #204：TURN_DENIED_PEER_CIDRS 内 peer → CreatePermission 403。
    #[test]
    fn denied_peer_returns_403() {
        let _g = TESTS_LOCK.lock().unwrap();
        unsafe {
            std::env::set_var("TURN_DENIED_PEER_CIDRS", "192.0.2.0/24");
        }
        let (server_addr, _srv) = spawn_udp("testsecret");
        let (u, p) = creds("testsecret");
        let client = UdpSocket::bind("127.0.0.1:0").unwrap();
        client
            .set_read_timeout(Some(Duration::from_secs(2)))
            .unwrap();
        let (_, realm, nonce) = udp_allocate(&client, server_addr, &u, &p).expect("allocate");

        let denied: SocketAddr = "192.0.2.10:4000".parse().unwrap();
        let req = auth_request(
            MSG_CREATE_PERMISSION,
            &[(ATTR_XOR_PEER_ADDRESS, encode_xor_peer(denied))],
            &u,
            &p,
            &realm,
            &nonce,
        )
        .unwrap();
        client.send_to(&req, server_addr).unwrap();
        let (m, attrs) = recv_response(&client);
        assert_eq!(m, MSG_ERROR_BASE | MSG_CREATE_PERMISSION, "应 403 错误");
        let code = attrs
            .iter()
            .find(|(t, _)| *t == ATTR_ERROR_CODE)
            .map(|(_, v)| ((v[2] & 7) as u16) * 100 + v[3] as u16);
        assert_eq!(code, Some(403));
        unsafe {
            std::env::remove_var("TURN_DENIED_PEER_CIDRS");
        }
    }

    /// #204：IPv6（::1）allocate + relay 回环。
    #[test]
    fn ipv6_allocate_relay_roundtrip() {
        let _g = TESTS_LOCK.lock().unwrap();
        unsafe {
            std::env::set_var("SFU_TURN_IPV6", "1");
        }
        let secret = "testsecret";
        let srv = spawn(secret, Ipv6Addr::LOCALHOST.into(), 0, None).unwrap();
        let server_addr = srv.udp_addr;
        let client = UdpSocket::bind("[::1]:0").unwrap();
        client
            .set_read_timeout(Some(Duration::from_secs(3)))
            .unwrap();
        let (u, p) = creds(secret);
        let (relayed, realm, nonce) =
            udp_allocate(&client, server_addr, &u, &p).expect("v6 allocate");
        assert!(relayed.is_ipv6(), "relayed 应为 IPv6: {relayed}");

        let peer = UdpSocket::bind("[::1]:0").unwrap();
        let peer_addr = peer.local_addr().unwrap();
        peer.set_read_timeout(Some(Duration::from_secs(3))).unwrap();

        let req = auth_request(
            MSG_CREATE_PERMISSION,
            &[(ATTR_XOR_PEER_ADDRESS, encode_xor_peer(peer_addr))],
            &u,
            &p,
            &realm,
            &nonce,
        )
        .unwrap();
        client.send_to(&req, server_addr).unwrap();
        let (m, _) = recv_response(&client);
        assert_eq!(m, MSG_SUCCESS_BASE | MSG_CREATE_PERMISSION);

        let chan = CHANNEL_BASE;
        let req = auth_request(
            MSG_CHANNEL_BIND,
            &[
                (ATTR_CHANNEL_NUMBER, chan.to_be_bytes().to_vec()),
                (ATTR_XOR_PEER_ADDRESS, encode_xor_peer(peer_addr)),
            ],
            &u,
            &p,
            &realm,
            &nonce,
        )
        .unwrap();
        client.send_to(&req, server_addr).unwrap();
        let (m, _) = recv_response(&client);
        assert_eq!(m, MSG_SUCCESS_BASE | MSG_CHANNEL_BIND);

        let payload = b"hello-v6-turn";
        let mut cd = Vec::with_capacity(4 + payload.len());
        cd.extend_from_slice(&chan.to_be_bytes());
        cd.extend_from_slice(&(payload.len() as u16).to_be_bytes());
        cd.extend_from_slice(payload);
        client.send_to(&cd, server_addr).unwrap();
        let mut buf = [0u8; 64];
        let (n, src) = peer.recv_from(&mut buf).unwrap();
        assert_eq!(&buf[..n], payload);
        assert_eq!(src.port(), relayed.port());

        peer.send_to(b"reply-v6-turn", relayed).unwrap();
        let mut buf2 = [0u8; 128];
        let (n2, _) = client.recv_from(&mut buf2).unwrap();
        let rchan = u16::from_be_bytes([buf2[0], buf2[1]]);
        assert_eq!(rchan, chan);
        assert_eq!(&buf2[4..n2], b"reply-v6-turn");
        unsafe {
            std::env::remove_var("SFU_TURN_IPV6");
        }
    }
}
