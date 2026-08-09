//! TURN 客户端（RFC 5766 长时凭证）——应用层 TURN 传输（#157 M2）。
//!
//! M1：`allocate` 完成 Allocate 拿到 relayed 地址（401→REALM/NONCE 重试）。
//! M2：`TurnTransport`——Allocate 后持久化 realm/nonce，按需 CreatePermission +
//! ChannelBind + ChannelData 收发、Data indication 解析、定期 Refresh。
//! 与 str0m 的集成在 `media_socket`（应用层双路径收发，不 patch str0m fork）。

use std::collections::HashMap;
use std::io;
use std::net::{IpAddr, Ipv4Addr, SocketAddr, ToSocketAddrs, UdpSocket};
use std::time::{Duration, Instant};

use aerodesk_protocol::turn::codec::*;

/// Refresh 间隔（allocation 默认 lifetime 600s，按 RFC 建议在到期前刷新）。
const REFRESH_INTERVAL: Duration = Duration::from_secs(300);
/// 关键请求（permission/channel bind）超时。
const REQUEST_TIMEOUT: Duration = Duration::from_secs(2);
/// 响应匹配时最多跳过的无关包（Data indication 等）。
const MAX_RESPONSE_SKIP: usize = 8;

/// 解析 TURN URL（`turn:host:port?transport=udp`）。仅支持 UDP；`turns:`/TCP 返回 None。
pub fn parse_turn_url(url: &str) -> Option<(String, u16)> {
    let rest = url.strip_prefix("turn:")?;
    let hostport = rest.split('?').next()?;
    let (host, port) = match hostport.rsplit_once(':') {
        Some((h, p)) => (h, p.parse().ok()?),
        None => (hostport, 3478),
    };
    if host.is_empty() {
        return None;
    }
    Some((host.to_string(), port))
}

/// 由信令下发的 TURN 配置建立传输（失败返回 None：直连兜底）。
pub fn setup_turn(
    turn: &aerodesk_protocol::signal::TurnConfig,
    loopback: bool,
) -> Option<TurnTransport> {
    let (host, port) = turn.urls.iter().find_map(|u| parse_turn_url(u))?;
    let server = (host.as_str(), port)
        .to_socket_addrs()
        .ok()?
        .find(|a| a.is_ipv4())?;
    let bind_ip: IpAddr = if loopback {
        Ipv4Addr::LOCALHOST.into()
    } else {
        Ipv4Addr::UNSPECIFIED.into()
    };
    match TurnTransport::connect(
        server,
        &turn.username,
        &turn.credential,
        bind_ip,
        Duration::from_secs(3),
    ) {
        Ok(tt) => {
            tracing::info!("TURN allocation ok: relayed={}", tt.relayed_addr());
            Some(tt)
        }
        Err(e) => {
            tracing::warn!("TURN setup failed (fallback direct): {e}");
            None
        }
    }
}

/// TURN 传输（UDP allocation + ChannelData 数据通路）。
///
/// 线程模型：与直连 UDP 同线程轮询；`recv_packet` 非阻塞（读超时返回
/// `Ok(None)`），`ensure_peer`/`refresh` 使用同步短事务（内部临时调整读超时）。
pub struct TurnTransport {
    socket: UdpSocket,
    server: SocketAddr,
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
            socket,
            server,
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

        // 1) 无凭证 Allocate → 401 拿 REALM/NONCE
        let txid = random_txid();
        let req = build_stun(MSG_ALLOCATE, &allocate_attrs(), txid, None)?;
        t.socket
            .send_to(&req, server)
            .map_err(|e| format!("send allocate (unauth): {e}"))?;
        let (resp, _) = recv_stun_matching(&t.socket, txid, MAX_RESPONSE_SKIP)?;
        let (realm, nonce, err) = parse_common(&resp)?;
        if err != Some(401) {
            return Err(format!("allocate expected 401, got error={err:?}"));
        }
        t.realm = realm.ok_or("no realm in 401")?;
        t.nonce = nonce.ok_or("no nonce in 401")?;

        // 2) 带长时凭证重试（438 stale nonce 由 request 自动重试）
        let (resp2, _) = t.request(MSG_ALLOCATE, allocate_attrs())?;
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
        let relayed = parse_xor_addr(&relayed_attr).ok_or("bad XOR-RELAYED-ADDRESS")?;
        t.relayed_addr = relayed;
        Ok(t)
    }

    /// TURN 服务器分配的 relayed 地址（对端发往该地址即中继到本端）。
    pub fn relayed_addr(&self) -> SocketAddr {
        self.relayed_addr
    }

    /// allocation socket 的本地地址。
    pub fn local_addr(&self) -> io::Result<SocketAddr> {
        self.socket.local_addr()
    }

    pub fn set_read_timeout(&self, t: Option<Duration>) -> io::Result<()> {
        self.socket.set_read_timeout(t)
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
        self.socket
            .send_to(&pkt, self.server)
            .map_err(|e| format!("turn send: {e}"))?;
        Ok(())
    }

    /// 接收一帧对端数据（Data indication / ChannelData 还原为 (peer, 载荷长度)）。
    /// 无数据（读超时）返回 `Ok(None)`；控制类响应内部消化后返回 `Ok(None)`。
    pub fn recv_packet(&mut self, buf: &mut [u8]) -> io::Result<Option<(SocketAddr, usize)>> {
        let (n, _) = match self.socket.recv_from(buf) {
            Ok(v) => v,
            Err(e)
                if e.kind() == io::ErrorKind::WouldBlock || e.kind() == io::ErrorKind::TimedOut =>
            {
                return Ok(None);
            }
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

    /// 到期时发送 Refresh 维持 allocation。非阻塞语义：使用当前读超时（快速失败），
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

    /// 发送带长时凭证的请求并等待匹配响应（REQUEST_TIMEOUT，关键路径）；
    /// 438（stale nonce）自动用新 nonce 重试。
    fn request(
        &mut self,
        method: u16,
        attrs: Vec<(u16, Vec<u8>)>,
    ) -> Result<(Vec<u8>, SocketAddr), String> {
        self.request_impl(method, attrs, Some(REQUEST_TIMEOUT))
    }

    /// 快速请求：不覆盖当前读超时（refresh 用，失败下轮重试，不阻塞媒体泵）。
    fn request_fast(
        &mut self,
        method: u16,
        attrs: Vec<(u16, Vec<u8>)>,
    ) -> Result<(Vec<u8>, SocketAddr), String> {
        self.request_impl(method, attrs, None)
    }

    fn request_impl(
        &mut self,
        method: u16,
        attrs: Vec<(u16, Vec<u8>)>,
        timeout: Option<Duration>,
    ) -> Result<(Vec<u8>, SocketAddr), String> {
        let prev = self.socket.read_timeout().map_err(|e| e.to_string())?;
        if let Some(t) = timeout {
            self.socket.set_read_timeout(Some(t)).ok();
        }
        let result = (|| {
            for attempt in 0..MAX_RESPONSE_SKIP {
                let txid = random_txid();
                let auth = (
                    self.username.as_str(),
                    self.password.as_str(),
                    self.realm.as_str(),
                    self.nonce.as_str(),
                );
                let req = build_stun(method, &attrs, txid, Some(auth))?;
                self.socket
                    .send_to(&req, self.server)
                    .map_err(|e| format!("turn send {method:#06x}: {e}"))?;
                let (resp, from) = recv_stun_matching(&self.socket, txid, MAX_RESPONSE_SKIP)?;
                let (new_realm, new_nonce, err) = parse_common(&resp)?;
                if let Some(r) = new_realm {
                    self.realm = r;
                }
                if let Some(n) = new_nonce {
                    self.nonce = n;
                }
                if err == Some(438) && attempt + 1 < MAX_RESPONSE_SKIP {
                    continue; // stale nonce：换新 nonce 重试
                }
                if err.is_some() && err != Some(401) && err != Some(438) {
                    return Err(format!("TURN {method:#06x} error: {err:?}"));
                }
                return Ok((resp, from));
            }
            Err("TURN request retries exhausted".into())
        })();
        self.socket.set_read_timeout(prev).ok();
        result
    }
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

/// 接收与 txid 匹配的 STUN 响应；跳过无关包（Data indication 等），最多 `max_skip` 个。
fn recv_stun_matching(
    socket: &UdpSocket,
    txid: [u8; 12],
    max_skip: usize,
) -> Result<(Vec<u8>, SocketAddr), String> {
    let mut buf = [0u8; 2048];
    for _ in 0..max_skip {
        let (n, from) = socket
            .recv_from(&mut buf)
            .map_err(|e| format!("recv: {e}"))?;
        let pkt = buf[..n].to_vec();
        if pkt.len() < 20 {
            continue;
        }
        if pkt[4..8] != STUN_MAGIC.to_be_bytes() {
            continue;
        }
        if pkt[8..20] == txid {
            return Ok((pkt, from));
        }
        // 其它事务（Data indication / 并发请求响应）：跳过。
    }
    Err("STUN response timeout (no matching txid)".into())
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
    use std::net::UdpSocket;
    use std::sync::Arc;
    use std::sync::atomic::{AtomicUsize, Ordering};

    /// mock TURN 服务器：401 → 校验 MI → 按方法回 200；ChannelData → Data indication 回显。
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
                    // 401 + REALM/NONCE
                    let mut body = Vec::new();
                    body.extend_from_slice(&encode_attr(ATTR_REALM, b"aerodesk.test"));
                    body.extend_from_slice(&encode_attr(ATTR_NONCE, b"nonce-1"));
                    body.extend_from_slice(&encode_attr(ATTR_ERROR_CODE, &[0, 0, 4, 1]));
                    let msg = encode_header(MSG_ERROR_BASE | 0x0003, txid, &body);
                    let _ = sock.send_to(&msg, from);
                    continue;
                }
                // 校验 MI：HMAC 输入不含 MI 属性头 + 值（与客户端 build_stun 对称）。
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
}

#[cfg(test)]
mod tests {
    use super::testutil::start_mock;
    use super::*;
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

    /// 真实 coturn 中继回环（scripts/turn-e2e.sh 启动 turnserver 后运行）：
    /// 客户端 ChannelData → TURN 服务器 → 对端 UDP；对端发往 relayed 地址 → 回到客户端。
    #[test]
    #[ignore = "requires real coturn (run via scripts/turn-e2e.sh)"]
    fn real_coturn_relay_roundtrip() {
        let Ok(server) = std::env::var("TURN_E2E_SERVER") else {
            eprintln!("skip: TURN_E2E_SERVER unset");
            return;
        };
        let Ok(server) = server.parse::<SocketAddr>() else {
            eprintln!("skip: bad TURN_E2E_SERVER={server}");
            return;
        };
        let secret = std::env::var("TURN_E2E_SECRET").unwrap_or_else(|_| "testsecret".into());
        let now = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .expect("time")
            .as_secs();
        let creds = aerodesk_protocol::turn::generate_turn_credentials(&secret, "e2e", 3600, now);
        let mut tt = TurnTransport::connect(
            server,
            &creds.username,
            &creds.credential,
            Ipv4Addr::LOCALHOST.into(),
            Duration::from_secs(3),
        )
        .expect("turn connect");

        let peer = UdpSocket::bind("127.0.0.1:0").expect("peer bind");
        let peer_addr = peer.local_addr().expect("peer addr");
        peer.set_read_timeout(Some(Duration::from_secs(3)))
            .expect("peer timeout");

        // 客户端 → TURN → 对端
        tt.send_to(peer_addr, b"hello-via-turn").expect("turn send");
        let mut buf = [0u8; 64];
        let (n, from) = peer.recv_from(&mut buf).expect("peer recv");
        assert_eq!(&buf[..n], b"hello-via-turn");
        // 对端从 TURN 服务器中继收到（源端口 = relayed 端口；源 IP 取决于 server 绑定方式）
        assert_eq!(from.port(), tt.relayed_addr().port());
        assert!(
            from.ip().is_loopback() || from.ip().is_unspecified(),
            "relay source {from}"
        );

        // 对端 → relayed 地址 → TURN → 客户端
        peer.send_to(b"reply-via-turn", tt.relayed_addr())
            .expect("peer send");
        let mut buf2 = [0u8; 64];
        let (peer2, n2) = tt
            .recv_packet(&mut buf2)
            .expect("turn recv")
            .expect("packet");
        assert_eq!(peer2, peer_addr);
        assert_eq!(&buf2[..n2], b"reply-via-turn");
    }

    #[test]
    fn parse_turn_url_variants() {
        assert_eq!(
            parse_turn_url("turn:turn.example.com:3478?transport=udp"),
            Some(("turn.example.com".to_string(), 3478))
        );
        assert_eq!(
            parse_turn_url("turn:127.0.0.1"),
            Some(("127.0.0.1".to_string(), 3478))
        );
        assert_eq!(parse_turn_url("turns:host:5349?transport=tcp"), None);
        assert_eq!(parse_turn_url(""), None);
    }

    #[test]
    fn md5_key_hmac_matches_reference() {
        // 与 Python hashlib/hmac 参考值一致（RFC 5766 长时凭证 key = MD5(user:realm:pass)）
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
        // RFC 2202 test case 1: key=0x0b*20, data="Hi There"
        let key = [0x0bu8; 20];
        let mac = hmac_sha1(&key, b"Hi There");
        let expect: [u8; 20] = [
            0xb6, 0x17, 0x31, 0x86, 0x55, 0x05, 0x72, 0x64, 0xe2, 0x8b, 0xc0, 0xb6, 0xfb, 0x37,
            0x8c, 0x8e, 0xf1, 0x46, 0xbe, 0x00,
        ];
        assert_eq!(mac, expect);
    }
}
