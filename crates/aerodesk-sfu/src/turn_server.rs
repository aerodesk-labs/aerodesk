//! SFU 内嵌 TURN+STUN server（#191：替代 coturn 侧车）。
//!
//! 架构：单线程控制面（UDP 监听，处理 Binding/Allocate/CreatePermission/ChannelBind/
//! Send/ChannelData/Refresh）+ 每 allocation 一个 relay 线程（peer → 客户端转发）。
//!
//! 认证：TURN_SECRET REST 模式（与 coturn 兼容）——username=`<expiry>:<userid>`，
//! credential=base64(HMAC-SHA1(secret, username))；SFU 下发的 TurnConfig 可直接使用。
//!
//! 数据面：客户端 → peer 走 Send indication / ChannelData；peer → 客户端走
//! ChannelData（已绑 channel）或 Data indication（仅有 permission）。

use std::collections::{HashMap, HashSet};
use std::io;
use std::net::{IpAddr, SocketAddr, UdpSocket};
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
/// 控制面读超时（轮询粒度）。
const POLL_TIMEOUT: Duration = Duration::from_millis(50);
/// 时钟偏差容忍（秒）。
const CLOCK_SKEW: u64 = 300;

/// allocation 共享状态（server 线程写、relay 线程读）。
struct AllocState {
    channels: HashMap<u16, SocketAddr>,
    peer_channel: HashMap<SocketAddr, u16>,
    permissions: HashSet<SocketAddr>,
}

struct Allocation {
    relay: Arc<UdpSocket>,
    relayed: SocketAddr,
    expires: Arc<AtomicU64>,
    stop: Arc<AtomicBool>,
    state: Arc<Mutex<AllocState>>,
}

/// 控制面上下文（收敛 handler 参数）。
struct Ctx<'a> {
    server: &'a UdpSocket,
    allocations: &'a mut HashMap<SocketAddr, Allocation>,
    secret: &'a str,
    realm: &'a str,
    nonce: &'a str,
    host: IpAddr,
}

/// 启动内嵌 TURN+STUN server（阻塞线程）。返回实际绑定地址（port 传 0 自动分配）。
/// `host_addr` 为本机对外地址（relay 绑定该 IP）。
pub fn spawn(
    secret: &str,
    host_addr: IpAddr,
    port: u16,
) -> io::Result<(SocketAddr, std::thread::JoinHandle<()>)> {
    // 绑定 0.0.0.0（loopback + 所有接口可达）；对外地址用 host_addr 上报。
    let socket = UdpSocket::bind((std::net::Ipv4Addr::UNSPECIFIED, port))?;
    socket.set_read_timeout(Some(POLL_TIMEOUT))?;
    let bound = socket.local_addr()?;
    let addr = SocketAddr::new(host_addr, bound.port());
    let realm = std::env::var("TURN_REALM").unwrap_or_else(|_| DEFAULT_REALM.to_string());
    let nonce = format!(
        "{:016x}",
        SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .map(|d| d.as_nanos())
            .unwrap_or(0)
    );
    info!("embedded TURN+STUN server listening on {addr} (realm={realm})");
    let secret = secret.to_string();
    Ok((
        addr,
        std::thread::spawn(move || run(socket, host_addr, secret, realm, nonce)),
    ))
}

fn unix_now() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0)
}

fn run(socket: UdpSocket, host: IpAddr, secret: String, realm: String, nonce: String) {
    let mut allocations: HashMap<SocketAddr, Allocation> = HashMap::new();
    let mut last_sweep = Instant::now();
    let mut buf = [0u8; 65535];
    loop {
        if let Ok((n, from)) = socket.recv_from(&mut buf) {
            let mut ctx = Ctx {
                server: &socket,
                allocations: &mut allocations,
                secret: &secret,
                realm: &realm,
                nonce: &nonce,
                host,
            };
            handle_packet(&mut ctx, &buf[..n], from);
        }
        if last_sweep.elapsed() >= SWEEP_INTERVAL {
            sweep(&mut allocations);
            last_sweep = Instant::now();
        }
    }
}

fn sweep(allocations: &mut HashMap<SocketAddr, Allocation>) {
    let now = unix_now();
    let expired: Vec<SocketAddr> = allocations
        .iter()
        .filter(|(_, a)| now >= a.expires.load(Ordering::SeqCst))
        .map(|(k, _)| *k)
        .collect();
    for k in expired {
        if let Some(a) = allocations.remove(&k) {
            a.stop.store(true, Ordering::SeqCst);
            debug!("TURN allocation expired: client={k} relayed={}", a.relayed);
        }
    }
}

fn handle_packet(ctx: &mut Ctx, pkt: &[u8], from: SocketAddr) {
    // ChannelData：channel 0x4000-0x7FFF（首两 bit 01）。
    if pkt.len() >= 4 && (pkt[0] & 0xc0) == 0x40 {
        let Some(alloc) = ctx.allocations.get(&from) else {
            return;
        };
        let chan = u16::from_be_bytes([pkt[0], pkt[1]]);
        let len = u16::from_be_bytes([pkt[2], pkt[3]]) as usize;
        let payload = &pkt[4..(4 + len).min(pkt.len())];
        let peer = {
            let st = alloc.state.lock().unwrap();
            st.channels.get(&chan).copied()
        };
        if let Some(peer) = peer {
            let _ = alloc.relay.send_to(payload, peer);
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
            // STUN Binding：返回 XOR-MAPPED-ADDRESS。
            let body = vec![(ATTR_XOR_MAPPED_ADDRESS, encode_xor_peer(from))];
            let resp =
                build_stun(MSG_SUCCESS_BASE | MSG_BINDING, &body, txid, None).unwrap_or_default();
            let _ = ctx.server.send_to(&resp, from);
        }
        MSG_ALLOCATE => handle_allocate(ctx, pkt, from, txid),
        MSG_CREATE_PERMISSION => handle_permission(ctx, pkt, from, txid, false),
        MSG_CHANNEL_BIND => handle_permission(ctx, pkt, from, txid, true),
        MSG_SEND => handle_send(ctx, pkt, from),
        MSG_REFRESH => handle_refresh(ctx, pkt, from, txid),
        _ => {
            debug!("TURN: unhandled method {method:#06x} from {from}");
        }
    }
}

/// 401/438 等错误响应（带 REALM/NONCE 用于挑战）。
fn send_error(ctx: &Ctx, to: SocketAddr, txid: [u8; 12], method: u16, code: u16, challenge: bool) {
    let mut body = vec![(ATTR_ERROR_CODE, encode_error_code(code))];
    if challenge {
        body.push((ATTR_REALM, ctx.realm.as_bytes().to_vec()));
        body.push((ATTR_NONCE, ctx.nonce.as_bytes().to_vec()));
    }
    let resp = build_stun(MSG_ERROR_BASE | method, &body, txid, None).unwrap_or_default();
    let _ = ctx.server.send_to(&resp, to);
}

/// 校验带 MI 的请求认证（REST secret）：返回 Ok(username) 或错误码。
fn check_auth(ctx: &Ctx, pkt: &[u8]) -> Result<String, u16> {
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
    if req_realm != ctx.realm {
        debug!(
            "TURN auth: realm mismatch req={req_realm:?} want={}",
            ctx.realm
        );
        return Err(401);
    }
    if req_nonce != ctx.nonce {
        debug!(
            "TURN auth: nonce mismatch req={req_nonce:?} want={}",
            ctx.nonce
        );
        return Err(438);
    }
    if username.is_empty() || username.split_once(':').is_none() {
        debug!("TURN auth: bad username {username:?}");
        return Err(401);
    }
    // 由 secret 重算期望 credential，再用它校验 MI；并校验 expiry。
    let expected = aerodesk_protocol::turn::turn_credential(ctx.secret, &username);
    if !verify_message_integrity(pkt, &username, ctx.realm, &expected) {
        debug!("TURN auth: MI mismatch user={username} expected_cred={expected}");
        return Err(401);
    }
    if !aerodesk_protocol::turn::verify_turn_credential(
        ctx.secret,
        &username,
        &expected,
        unix_now(),
        CLOCK_SKEW,
    ) {
        debug!("TURN auth: credential expired/invalid user={username}");
        return Err(401);
    }
    Ok(username)
}

fn handle_allocate(ctx: &mut Ctx, pkt: &[u8], from: SocketAddr, txid: [u8; 12]) {
    if ctx.allocations.contains_key(&from) {
        send_error(ctx, from, txid, MSG_ALLOCATE, 437, false);
        return;
    }
    // REQUESTED-TRANSPORT：仅 UDP（17）。
    let transport = find_attr(pkt, ATTR_REQUESTED_TRANSPORT)
        .and_then(|v| v.first().copied())
        .unwrap_or(0);
    if transport != 17 {
        send_error(ctx, from, txid, MSG_ALLOCATE, 442, false);
        return;
    }
    let username = match check_auth(ctx, pkt) {
        Ok(u) => u,
        Err(code) => {
            send_error(ctx, from, txid, MSG_ALLOCATE, code, true);
            return;
        }
    };
    // 创建 relay socket（绑定 0.0.0.0，端口由内核分配）；relayed 地址上报 ctx.host:port。
    let Ok(relay) = UdpSocket::bind((std::net::Ipv4Addr::UNSPECIFIED, 0)) else {
        warn!("TURN relay bind failed");
        send_error(ctx, from, txid, MSG_ALLOCATE, 500, false);
        return;
    };
    let Ok(relay_port) = relay.local_addr().map(|a| a.port()) else {
        return;
    };
    let relayed = SocketAddr::new(ctx.host, relay_port);
    let expires = Arc::new(AtomicU64::new(unix_now() + DEFAULT_LIFETIME as u64));
    let stop = Arc::new(AtomicBool::new(false));
    let state = Arc::new(Mutex::new(AllocState {
        channels: HashMap::new(),
        peer_channel: HashMap::new(),
        permissions: HashSet::new(),
    }));
    let relay_arc = Arc::new(relay);
    let Ok(server_sock) = ctx.server.try_clone() else {
        return;
    };
    let server_arc = Arc::new(server_sock);
    // relay 线程自管理生命周期（stop/expires 过期退出）。
    let _relay_thread = spawn_relay(
        relay_arc.clone(),
        server_arc,
        from,
        expires.clone(),
        stop.clone(),
        state.clone(),
    );
    ctx.allocations.insert(
        from,
        Allocation {
            relay: relay_arc,
            relayed,
            expires,
            stop,
            state,
        },
    );
    debug!("TURN allocation: client={from} user={username} relayed={relayed}");
    let body = vec![
        (ATTR_XOR_RELAYED_ADDRESS, encode_xor_peer(relayed)),
        (ATTR_LIFETIME, DEFAULT_LIFETIME.to_be_bytes().to_vec()),
    ];
    let resp = build_stun(MSG_SUCCESS_BASE | MSG_ALLOCATE, &body, txid, None).unwrap_or_default();
    let _ = ctx.server.send_to(&resp, from);
}

fn handle_permission(
    ctx: &mut Ctx,
    pkt: &[u8],
    from: SocketAddr,
    txid: [u8; 12],
    channel_bind: bool,
) {
    let method = if channel_bind {
        MSG_CHANNEL_BIND
    } else {
        MSG_CREATE_PERMISSION
    };
    let Some(alloc) = ctx.allocations.get(&from) else {
        return;
    };
    if check_auth(ctx, pkt).is_err() {
        send_error(ctx, from, txid, method, 401, true);
        return;
    }
    let Some(peer_val) = find_attr(pkt, ATTR_XOR_PEER_ADDRESS) else {
        send_error(ctx, from, txid, method, 400, false);
        return;
    };
    let Some(peer) = parse_xor_addr(&peer_val) else {
        send_error(ctx, from, txid, method, 400, false);
        return;
    };
    let mut st = alloc.state.lock().unwrap();
    if channel_bind {
        let Some(chan_val) = find_attr(pkt, ATTR_CHANNEL_NUMBER) else {
            return;
        };
        let chan = u16::from_be_bytes([chan_val[0], chan_val[1]]);
        if !(CHANNEL_BASE..=CHANNEL_MAX).contains(&chan) {
            send_error(ctx, from, txid, method, 400, false);
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
    let resp = build_stun(MSG_SUCCESS_BASE | method, &[], txid, None).unwrap_or_default();
    let _ = ctx.server.send_to(&resp, from);
}

fn handle_send(ctx: &mut Ctx, pkt: &[u8], from: SocketAddr) {
    let Some(alloc) = ctx.allocations.get(&from) else {
        return;
    };
    if check_auth(ctx, pkt).is_err() {
        return; // Send 是指示（无响应），失败静默丢弃
    }
    let Some(peer_val) = find_attr(pkt, ATTR_XOR_PEER_ADDRESS).and_then(|v| parse_xor_addr(&v))
    else {
        return;
    };
    let Some(data) = find_attr(pkt, ATTR_DATA) else {
        return;
    };
    let st = alloc.state.lock().unwrap();
    let permitted = st.permissions.contains(&peer_val);
    drop(st);
    if permitted {
        let _ = alloc.relay.send_to(&data, peer_val);
    }
}

fn handle_refresh(ctx: &mut Ctx, pkt: &[u8], from: SocketAddr, txid: [u8; 12]) {
    let Some(alloc) = ctx.allocations.get(&from) else {
        return;
    };
    if check_auth(ctx, pkt).is_err() {
        send_error(ctx, from, txid, MSG_REFRESH, 401, true);
        return;
    }
    let requested = find_attr(pkt, ATTR_LIFETIME)
        .map(|v| u32::from_be_bytes(v[..4].try_into().unwrap_or([0; 4])))
        .unwrap_or(DEFAULT_LIFETIME);
    let lifetime = requested.clamp(60, DEFAULT_LIFETIME);
    alloc
        .expires
        .store(unix_now() + lifetime as u64, Ordering::SeqCst);
    let body = vec![(ATTR_LIFETIME, lifetime.to_be_bytes().to_vec())];
    let resp = build_stun(MSG_SUCCESS_BASE | MSG_REFRESH, &body, txid, None).unwrap_or_default();
    let _ = ctx.server.send_to(&resp, from);
}

/// relay 线程：收 peer 包 → 转发客户端（ChannelData 或 Data indication）。
fn spawn_relay(
    relay: Arc<UdpSocket>,
    server: Arc<UdpSocket>,
    client: SocketAddr,
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
                    // ChannelData 回客户端
                    let mut out = Vec::with_capacity(4 + n);
                    out.extend_from_slice(&chan.to_be_bytes());
                    out.extend_from_slice(&(n as u16).to_be_bytes());
                    out.extend_from_slice(&buf[..n]);
                    drop(st);
                    let _ = server.send_to(&out, client);
                } else if st.permissions.contains(&peer) {
                    // Data indication
                    let mut body = Vec::new();
                    body.extend_from_slice(&encode_attr(
                        ATTR_XOR_PEER_ADDRESS,
                        &encode_xor_peer(peer),
                    ));
                    body.extend_from_slice(&encode_attr(ATTR_DATA, &buf[..n]));
                    drop(st);
                    let out = encode_header(MSG_DATA_INDICATION, [0u8; 12], &body);
                    let _ = server.send_to(&out, client);
                }
            }
        }
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::net::{Ipv4Addr, UdpSocket};

    /// 最小测试客户端：401 挑战 → Allocate → CreatePermission → ChannelBind →
    /// ChannelData 发送；peer 回包经 server 转回（ChannelData）。
    fn allocate(
        client: &UdpSocket,
        server_addr: SocketAddr,
        username: &str,
        credential: &str,
    ) -> Result<(SocketAddr, String, String), String> {
        // 1) 无凭证 Allocate → 401
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
        // 2) 带凭证重试
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

    #[test]
    fn binding_returns_xor_mapped() {
        let (server_addr, _h) = spawn("testsecret", Ipv4Addr::LOCALHOST.into(), 0).unwrap();
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
        let secret = "testsecret";
        let (server_addr, _h) = spawn(secret, Ipv4Addr::LOCALHOST.into(), 0).unwrap();

        let client = UdpSocket::bind("127.0.0.1:0").unwrap();
        client
            .set_read_timeout(Some(Duration::from_secs(2)))
            .unwrap();
        let creds =
            aerodesk_protocol::turn::generate_turn_credentials(secret, "e2e", 3600, unix_now());
        let (relayed, realm, nonce) =
            allocate(&client, server_addr, &creds.username, &creds.credential).expect("allocate");

        // peer 直接 UDP（模拟被控端/SFU）
        let peer = UdpSocket::bind("127.0.0.1:0").unwrap();
        let peer_addr = peer.local_addr().unwrap();
        peer.set_read_timeout(Some(Duration::from_secs(2))).unwrap();

        // CreatePermission + ChannelBind
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

        // 客户端 → ChannelData → server → peer（peer 收到明文 UDP，源为 relayed）
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

        // peer → relayed → server → ChannelData → client
        peer.send_to(b"reply-via-embedded-turn", relayed).unwrap();
        let mut buf2 = [0u8; 128];
        let (n2, _) = client.recv_from(&mut buf2).unwrap();
        assert!(n2 > 4);
        let rchan = u16::from_be_bytes([buf2[0], buf2[1]]);
        assert_eq!(rchan, chan);
        assert_eq!(&buf2[4..n2], b"reply-via-embedded-turn");
    }

    #[test]
    fn allocate_rejects_bad_credentials() {
        let secret = "testsecret";
        let (server_addr, _h) = spawn(secret, Ipv4Addr::LOCALHOST.into(), 0).unwrap();
        let client = UdpSocket::bind("127.0.0.1:0").unwrap();
        client
            .set_read_timeout(Some(Duration::from_secs(2)))
            .unwrap();
        let res = allocate(&client, server_addr, "1234567890:bad", "wrongcred");
        assert!(res.is_err(), "bad credential must fail");
    }
}
