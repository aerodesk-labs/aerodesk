//! 客户端连接流程（观看端/被控端共用）。
//!
//! WSS 信令 join → SDP 交换 → ICE 泵。供 CLI/UI/移动端壳层复用。

use std::net::UdpSocket;
use std::time::Duration;

use crate::media_socket::MediaSocket;
use crate::signaling::WsSignalClient;
use crate::turn_client::setup_turn;
use aerodesk_protocol::signal::Role;
use str0m::net::Protocol;

/// 连接结果摘要。
#[derive(Debug, Clone)]
pub struct ConnectResult {
    pub room: String,
    pub peer_id: String,
    pub ice_connected: bool,
}

impl ConnectResult {
    pub fn summary(&self) -> String {
        format!(
            "peer={} room={} sdp=ok ice={}",
            self.peer_id,
            self.room,
            if self.ice_connected {
                "connected"
            } else {
                "pending(5s 超时)"
            }
        )
    }
}

/// 活跃连接（保留 signal/endpoint/socket，供媒体循环使用）。
pub struct LiveSession {
    pub signal: WsSignalClient,
    pub endpoint: crate::Endpoint,
    pub socket: MediaSocket,
    pub video_mid: Option<str0m::media::Mid>,
    pub room: String,
    pub peer_id: String,
    pub ice_connected: bool,
}

impl LiveSession {
    pub fn summary(&self) -> String {
        format!(
            "peer={} room={} sdp=ok ice={}",
            self.peer_id,
            self.room,
            if self.ice_connected {
                "connected"
            } else {
                "pending(5s 超时)"
            }
        )
    }
}

/// 枚举本机 IPv4 接口，选一个可作为 ICE host candidate 的地址：
/// 排除回环、链路本地（169.254）、未指定地址、隧道接口（utun/tun/gpd，
/// 含 Clash TUN 假 IP 198.18.0.0/15）——这些地址对端不可达。
/// 优先私有网段（LAN），否则取第一个非排除的全局地址；都没有则回退 127.0.0.1。
#[cfg(unix)]
fn discover_local_ip() -> Option<std::net::IpAddr> {
    unsafe {
        let mut ifap: *mut libc::ifaddrs = std::ptr::null_mut();
        if libc::getifaddrs(&mut ifap) != 0 || ifap.is_null() {
            return None;
        }
        let mut best: Option<std::net::Ipv4Addr> = None;
        let mut first: Option<std::net::Ipv4Addr> = None;
        let mut p = ifap;
        while !p.is_null() {
            let ifa = &*p;
            if !ifa.ifa_addr.is_null()
                && (*ifa.ifa_addr).sa_family == libc::AF_INET as libc::sa_family_t
            {
                let sin = &*(ifa.ifa_addr as *const libc::sockaddr_in);
                let ip = std::net::Ipv4Addr::from(u32::from_be(sin.sin_addr.s_addr));
                let name = std::ffi::CStr::from_ptr(ifa.ifa_name)
                    .to_string_lossy()
                    .into_owned();
                let oct = ip.octets();
                let is_tunnel = name.starts_with("utun")
                    || name.starts_with("tun")
                    || name.starts_with("gpd")
                    || name.starts_with("ipsec")
                    || name.starts_with("ppp");
                // Clash TUN fake-ip 段（198.18.0.0/15）：macOS 上会劫持全部出站，
                // 但该地址对端不可达，必须排除。
                let is_fake_ip = oct[0] == 198 && oct[1] == 18;
                let is_link_local = oct[0] == 169 && oct[1] == 254;
                if !ip.is_loopback()
                    && !ip.is_unspecified()
                    && !is_link_local
                    && !is_fake_ip
                    && !is_tunnel
                {
                    if first.is_none() {
                        first = Some(ip);
                    }
                    if ip.is_private() {
                        best = Some(ip);
                    }
                }
            }
            p = ifa.ifa_next;
        }
        libc::freeifaddrs(ifap);
        let ip = best.or(first).unwrap_or(std::net::Ipv4Addr::LOCALHOST);
        Some(std::net::IpAddr::V4(ip))
    }
}

/// 非 Unix 平台（Windows 等）：临时 UDP socket connect 到公共地址探测出口 IP；
/// 失败回退 127.0.0.1。
#[cfg(not(unix))]
fn discover_local_ip() -> Option<std::net::IpAddr> {
    let probe = UdpSocket::bind("0.0.0.0:0").ok()?;
    probe.connect("8.8.8.8:80").ok()?;
    probe.local_addr().ok().map(|a| a.ip())
}

/// 连接并保留活跃会话（观看端）。
pub fn connect_live(server: &str, room: &str) -> Result<LiveSession, String> {
    connect_live_role(server, room, Role::Viewer, None)
}

/// 观看端 + force-relay（#201）：ICE 只通告 relayed 候选。
pub fn connect_live_forced(
    server: &str,
    room: &str,
    force_relay: bool,
) -> Result<LiveSession, String> {
    connect_live_role_forced(server, room, Role::Viewer, None, force_relay)
}

/// 连接并保留活跃会话（任意角色）。`auth` 为 JWT/静态 token（可选）。
pub fn connect_live_role(
    server: &str,
    room: &str,
    role: Role,
    auth: Option<&str>,
) -> Result<LiveSession, String> {
    connect_live_role_impl(server, room, role, auth, force_relay_env())
}

/// force-relay（#201）：ICE 只通告 relayed 候选、跳过 host 候选。
/// 适用于：NAT 下直连候选"假通"（连通性检查能过但媒体入站被丢，如 qemu
/// slirp 模拟器），或要求媒体必须走 TURN 的部署。
pub fn connect_live_role_forced(
    server: &str,
    room: &str,
    role: Role,
    auth: Option<&str>,
    force_relay: bool,
) -> Result<LiveSession, String> {
    connect_live_role_impl(server, room, role, auth, force_relay || force_relay_env())
}

fn force_relay_env() -> bool {
    std::env::var("AERODESK_FORCE_RELAY")
        .ok()
        .is_some_and(|v| v == "1" || v.eq_ignore_ascii_case("true"))
}

fn connect_live_role_impl(
    server: &str,
    room: &str,
    role: Role,
    auth: Option<&str>,
    force_relay: bool,
) -> Result<LiveSession, String> {
    let mut signal = WsSignalClient::connect(server).map_err(|e| format!("signal connect: {e}"))?;
    let (peer_id, turn) = signal
        .join(room, role, auth)
        .map_err(|e| format!("join: {e}"))?;

    // #1：iOS（含模拟器）下通配符绑定 + 0.0.0.0 candidate 会被 str0m 拒绝，且
    // 模拟器 UDP 收不到发往 LAN IP/0.0.0.0 绑定 socket 的包。当信令地址是
    // loopback（本地开发/模拟器，媒体 SFU 同机）时直接绑定 127.0.0.1（与 CLI
    // 完全一致）；远端信令（真机场景）才绑定 0.0.0.0 并通告出口 IP candidate。
    let loopback_signal =
        server.contains("127.0.0.1") || server.contains("localhost") || server.contains("::1");
    // #157 M2：join 返回 TURN 配置时建立中继传输（失败仅告警，直连兜底）。
    let turn_transport = turn.as_ref().and_then(|tc| setup_turn(tc, loopback_signal));
    let direct = if loopback_signal {
        UdpSocket::bind("127.0.0.1:0").map_err(|e| format!("udp bind: {e}"))?
    } else {
        UdpSocket::bind("0.0.0.0:0").map_err(|e| format!("udp bind: {e}"))?
    };
    let bind_addr = direct.local_addr().map_err(|e| e.to_string())?;
    // 通配符绑定（0.0.0.0）的 local_addr 不能作为 candidate（str0m 拒绝）。
    let mut candidates = Vec::new();
    if bind_addr.ip() != std::net::IpAddr::V4(std::net::Ipv4Addr::UNSPECIFIED)
        && bind_addr.ip() != std::net::IpAddr::V6(std::net::Ipv6Addr::UNSPECIFIED)
    {
        candidates.push(bind_addr.ip());
    }
    if !loopback_signal
        && let Some(ip) = discover_local_ip()
        && !candidates.contains(&ip)
    {
        candidates.push(ip);
    }
    if candidates.is_empty() {
        candidates.push(std::net::IpAddr::V4(std::net::Ipv4Addr::LOCALHOST));
    }
    let mut socket = MediaSocket::new(direct, turn_transport);
    let mut endpoint = crate::Endpoint::new();
    for ip in &candidates {
        if force_relay {
            continue; // #201：只通告 relayed 候选，避免直连路径在 NAT/模拟器下丢媒体
        }
        let addr = std::net::SocketAddr::new(*ip, bind_addr.port());
        tracing::debug!("local candidate {addr}");
        endpoint
            .add_local_candidate(addr, Protocol::Udp)
            .map_err(|e| format!("candidate: {e:?}"))?;
    }
    // #157 M2：relayed 候选加入 offer（`typ relay`），ICE 按优先级直连优先、TURN 兜底。
    if let Some(tt) = socket.turn() {
        let relayed = tt.relayed_addr();
        if let Ok(la) = tt.local_addr() {
            let local_ip = candidates
                .first()
                .copied()
                .unwrap_or(std::net::IpAddr::V4(std::net::Ipv4Addr::LOCALHOST));
            let local = std::net::SocketAddr::new(local_ip, la.port());
            tracing::info!(
                "relayed candidate {relayed} (local {local}) force_relay={force_relay}"
            );
            if let Err(e) = endpoint.add_relay_candidate(relayed, local) {
                tracing::warn!("relay candidate rejected (TURN disabled): {e:?}");
            }
        }
    }
    // #12：viewer 的 offer 用 recvonly（SFU 拒绝 viewer 发布媒体）。
    if role == Role::Viewer {
        endpoint.add_video_recvonly();
    } else {
        endpoint.add_video();
    }
    let (offer, pending, video_mid, _audio_mid) = endpoint
        .create_offer()
        .map_err(|e| format!("offer: {e:?}"))?;
    let offer_json = serde_json::to_string(&offer).map_err(|e| e.to_string())?;
    let answer_json = signal
        .exchange_description(&offer_json)
        .map_err(|e| format!("answer: {e}"))?;
    let answer: str0m::change::SdpAnswer =
        serde_json::from_str(&answer_json).map_err(|e| format!("answer parse: {e}"))?;
    endpoint
        .accept_answer(pending, answer)
        .map_err(|e| format!("accept: {e:?}"))?;

    tracing::debug!("connect_live_role: SDP exchanged, entering ICE loop");
    let mut ice_connected = false;
    let ice_timeout = if socket.turn().is_some() { 10 } else { 5 };
    let deadline = std::time::Instant::now() + Duration::from_secs(ice_timeout);
    while std::time::Instant::now() < deadline {
        socket
            .set_read_timeout(Some(Duration::from_millis(10)))
            .ok();
        let mut buf = [0u8; 2048];
        if let Ok((n, source)) = socket.recv_from(&mut buf)
            && let Ok(contents) = buf[..n].try_into()
        {
            let _ = endpoint.handle_input(str0m::Input::Receive(
                std::time::Instant::now(),
                str0m::net::Receive {
                    proto: Protocol::Udp,
                    source,
                    destination: socket.local_addr().unwrap(),
                    contents,
                },
            ));
        }
        let _ = endpoint.handle_timeout(std::time::Instant::now());
        while let Some(output) = endpoint.poll_output() {
            match output {
                str0m::Output::Transmit(t) => {
                    let _ = socket.send_to(&t.contents, t.destination);
                }
                // 关键：遇到 Timeout 必须退出本轮排空（或回喂 handle_input(Timeout)），
                // 否则 str0m 会反复返回同一个 Timeout → 100% CPU 死循环，
                // connect_live_role 永不返回（iOS viewer pump 线程无法启动）。
                // CLI 主循环同样 break（见 main.rs poll_output）。
                str0m::Output::Timeout(_) => break,
                str0m::Output::Event(_) => {}
            }
        }
        while let Some(ev) = endpoint.poll_event() {
            if let crate::endpoint::ClientEvent::IceConnected = ev {
                ice_connected = true;
                break;
            }
        }
        if ice_connected {
            break;
        }
    }

    Ok(LiveSession {
        signal,
        endpoint,
        socket,
        video_mid,
        room: room.to_string(),
        peer_id,
        ice_connected,
    })
}

/// 观看端连接：WSS join → SDP 交换 → ICE 泵（5s 超时）。/// 观看端连接：WSS join → SDP 交换 → ICE 泵（5s 超时）。
pub fn connect_viewer(server: &str, room: &str) -> Result<ConnectResult, String> {
    connect_viewer_auth(server, room, None)
}

/// 观看端连接（带认证 token）。
pub fn connect_viewer_auth(
    server: &str,
    room: &str,
    auth: Option<&str>,
) -> Result<ConnectResult, String> {
    connect_role(server, room, Role::Viewer, auth)
}

/// 发布端连接（参数化角色）。
pub fn connect(server: &str, room: &str, role: Role) -> Result<ConnectResult, String> {
    connect_role(server, room, role, None)
}

/// 连接（任意角色 + 认证 token）。
pub fn connect_role(
    server: &str,
    room: &str,
    role: Role,
    auth: Option<&str>,
) -> Result<ConnectResult, String> {
    let live = connect_live_role(server, room, role, auth)?;
    Ok(ConnectResult {
        room: live.room.clone(),
        peer_id: live.peer_id.clone(),
        ice_connected: live.ice_connected,
    })
}
