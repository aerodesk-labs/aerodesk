//! 客户端连接流程（观看端/被控端共用）。
//!
//! WSS 信令 join → SDP 交换 → ICE 泵。供 CLI/UI/移动端壳层复用。

use std::net::UdpSocket;
use std::time::Duration;

use crate::media_socket::MediaSocket;
use crate::protocol::signal::{Role, SignalMessage};
use crate::signaling::WsSignalClient;
use crate::turn_client::setup_turn;
use str0m::net::Protocol;

use crate::platform::Codec;

/// 连接结果摘要。
#[derive(Debug, Clone)]
pub struct ConnectResult {
    pub room: String,
    pub peer_id: String,
    pub ice_connected: bool,
}

/// 连接链路错误（#487 自审批次 4）：此前全链路 `Result<_, String>`，调用方
/// （desktop/CLI/session）只能整串显示、无法稳定分支。集中类型化后：
/// 认证失败（提示检查凭据）、房间满（提示换房）、超时（提示重试）可 match
/// 变体；分类在 connect 内集中一次完成，服务器文案变更只改此处。
#[derive(Debug, thiserror::Error)]
pub enum ConnectError {
    /// 信令 WS 连接失败（IO/握手/协议）。
    #[error("signal connect: {0}")]
    Signal(String),
    /// 认证被拒（服务器 `auth failed`，契约字面量见 signal main）。
    #[error("认证失败: {0}")]
    Auth(String),
    /// Join 被服务器拒绝（room full / server full / 重定向循环等）。
    #[error("join: {0}")]
    Join(String),
    /// SDP 交换失败（SFU answer 超时/拒绝/解析）。
    #[error("answer: {0}")]
    Sdp(String),
    /// 连接建立总超时兜底（#487：半开连接/无响应防无限阻塞）。
    #[error("连接超时（{0}s）：信令/SFU 无响应")]
    Timeout(u64),
    /// 环境/设置失败（udp 绑定、线程创建、连接线程 panic）。
    #[error("{0}")]
    Setup(String),
}

/// join 拒绝文案分类：`auth failed` 是信号服务固定拒绝文案（signal main
/// 字面量）——集中映射到 Auth，其余（room full / server full / 重定向循环
/// 等）进 Join。服务器文案变更只改此处（有契约测试守护）。
fn classify_join_error(e: String) -> ConnectError {
    if e.contains("auth") {
        ConnectError::Auth(e)
    } else {
        ConnectError::Join(e)
    }
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
    /// #216 M6：音频 mid（桥跨 PoP 转发用；CLI 不用此字段）。
    pub audio_mid: Option<str0m::media::Mid>,
    /// 第二路视频轨（摄像头，观看端 recvonly；未请求时 None）。
    pub camera_mid: Option<str0m::media::Mid>,
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
pub(crate) fn discover_local_ip() -> Option<std::net::IpAddr> {
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
pub(crate) fn discover_local_ip() -> Option<std::net::IpAddr> {
    let probe = UdpSocket::bind("0.0.0.0:0").ok()?;
    probe.connect("8.8.8.8:80").ok()?;
    probe.local_addr().ok().map(|a| a.ip())
}

/// 连接并保留活跃会话（观看端）。
pub fn connect_live(server: &str, room: &str) -> Result<LiveSession, ConnectError> {
    connect_live_role(server, room, Role::Viewer, None)
}

/// 观看端 + force-relay（#201）：ICE 只通告 relayed 候选。
/// 唯一消费者为 Android（#201/#218 模拟器 NAT 场景）。
pub fn connect_live_forced(
    server: &str,
    room: &str,
    force_relay: bool,
) -> Result<LiveSession, ConnectError> {
    connect_live_role_impl(
        server,
        room,
        Role::Viewer,
        None,
        force_relay || force_relay_env(),
        None,
        false, // force-relay 路径不协商音频，避免 SDP 行为变化
        false,
    )
}

/// 连接并保留活跃会话（任意角色）。`auth` 为 JWT/静态 token（可选）。
pub fn connect_live_role(
    server: &str,
    room: &str,
    role: Role,
    auth: Option<&str>,
) -> Result<LiveSession, ConnectError> {
    connect_live_role_impl(
        server,
        room,
        role,
        auth,
        force_relay_env(),
        None,
        false,
        false,
    )
}

/// 连接并保留活跃会话，可选请求第二路视频轨（摄像头）。
/// `camera=true` 时 offer 增加一个 recvonly（viewer）/sendrecv（publisher）
/// 视频 m-line，返回 `LiveSession::camera_mid`。
pub fn connect_live_role_with_camera(
    server: &str,
    room: &str,
    role: Role,
    auth: Option<&str>,
    camera: bool,
) -> Result<LiveSession, ConnectError> {
    connect_live_role_impl(
        server,
        room,
        role,
        auth,
        force_relay_env(),
        None,
        false,
        camera,
    )
}

/// force-relay（#201/#218）：`AERODESK_FORCE_RELAY=1|true` 时 ICE 只通告
/// relayed 候选、跳过 host 候选；供 CLI 与 core 连接路径共用。
pub fn force_relay_env() -> bool {
    std::env::var("AERODESK_FORCE_RELAY")
        .ok()
        .is_some_and(|v| v == "1" || v.eq_ignore_ascii_case("true"))
}

/// 从 SDP 回答 JSON 中剔除回环（127.0.0.1 / ::1）候选条目；解析失败原样返回。
/// 形状无关：递归遍历 Value，删除任何"含 candidate: 且含回环地址"的字符串元素。
fn strip_loopback_remote_candidates(answer_json: &str) -> String {
    fn is_loopback_candidate(s: &str) -> bool {
        s.contains("candidate:") && (s.contains(" 127.0.0.1 ") || s.contains(" ::1 "))
    }
    fn walk(v: &mut serde_json::Value) {
        match v {
            serde_json::Value::Array(items) => {
                items.retain(|i| i.as_str().is_none_or(|s| !is_loopback_candidate(s)));
                for i in items {
                    walk(i);
                }
            }
            serde_json::Value::Object(map) => {
                for val in map.values_mut() {
                    walk(val);
                }
            }
            _ => {}
        }
    }
    let Ok(mut v) = serde_json::from_str::<serde_json::Value>(answer_json) else {
        return answer_json.to_string();
    };
    walk(&mut v);
    serde_json::to_string(&v).unwrap_or_else(|_| answer_json.to_string())
}

/// 连接并保留活跃会话（指定视频 codec；#216 桥接客户端复用）。
/// `codec=None` 用默认（与 `connect_live_role` 一致）。
pub fn connect_live_role_codec(
    server: &str,
    room: &str,
    role: Role,
    auth: Option<&str>,
    codec: Option<Codec>,
) -> Result<LiveSession, ConnectError> {
    connect_live_role_impl(
        server,
        room,
        role,
        auth,
        force_relay_env(),
        codec,
        true,
        false,
    )
}

/// 带总超时的连接（#487 审查）：TCP 握手/Join 应答/SDP 交换在异常网络
/// （半开连接、信令无响应）下会无限阻塞，且阻塞无法被停止标志中断——
/// 调用方（被控端 run_publisher）卡死时 UI 无失败提示、线程无法退出。
/// 子线程 + `timeout` 兜底：超时返回 Err，调用方立即提示并退出。
/// 正常路径耗时约 1-5s（WS 握手 + join + SDP 交换），ICE 泵不计入
/// （ICE 超时返回 Ok 而非阻塞）。macos_media 观看端已有同款 20s 保护。
pub fn connect_live_role_codec_timeout(
    server: &str,
    room: &str,
    role: Role,
    auth: Option<&str>,
    codec: Option<Codec>,
    timeout: Duration,
) -> Result<LiveSession, ConnectError> {
    let (tx, rx) = std::sync::mpsc::channel::<Result<LiveSession, ConnectError>>();
    let srv = server.to_string();
    let rm = room.to_string();
    let auth = auth.map(|s| s.to_string());
    // 数据通道收发链（str0m/SCTP）调用栈深，放大线程栈防溢出（RULE 同款）。
    if std::thread::Builder::new()
        .stack_size(16 * 1024 * 1024)
        .spawn(move || {
            let r = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
                connect_live_role_codec(&srv, &rm, role, auth.as_deref(), codec)
            }));
            let _ =
                tx.send(r.unwrap_or_else(|_| Err(ConnectError::Setup("connect panicked".into()))));
        })
        .is_err()
    {
        return Err(ConnectError::Setup("无法创建连接线程".into()));
    }
    rx.recv_timeout(timeout)
        .map_err(|_| ConnectError::Timeout(timeout.as_secs()))?
}

#[allow(clippy::too_many_arguments)] // 内部实现：角色/鉴权/中继/音频/摄像头等开关收敛一处
fn connect_live_role_impl(
    server: &str,
    room: &str,
    role: Role,
    auth: Option<&str>,
    force_relay: bool,
    codec: Option<Codec>,
    // #216 M6：是否协商音频 track（仅桥接客户端需要，CLI/移动端保持原 SDP）。
    with_audio: bool,
    // 是否协商第二路视频轨（摄像头；观看端 recvonly）。
    with_camera: bool,
) -> Result<LiveSession, ConnectError> {
    let mut signal =
        WsSignalClient::connect(server).map_err(|e| ConnectError::Signal(e.to_string()))?;
    let (peer_id, turn) = signal.join(room, role, auth).map_err(classify_join_error)?;

    // #539/#456 呼叫发起：主控（viewer）连接时通知房间内被叫端（Publisher）
    // 弹窗确认——被叫端接受后才出流采集。Call 是通知性质（不阻塞媒体协商）：
    // 被叫端拒绝/超时则主控看到黑屏（错误码经 call_rejected 返回）。
    if role == Role::Viewer {
        let _ = signal.send_signal(SignalMessage::Call {
            from: peer_id.clone(),
            target: room.to_string(),
            call_id: format!("call-{peer_id}"),
            timeout_ms: Some(30_000),
        });
    }

    // #1：iOS（含模拟器）下通配符绑定 + 0.0.0.0 candidate 会被 str0m 拒绝，且
    // 模拟器 UDP 收不到发往 LAN IP/0.0.0.0 绑定 socket 的包。当信令地址是
    // loopback（本地开发/模拟器，媒体 SFU 同机）时直接绑定 127.0.0.1（与 CLI
    // 完全一致）；远端信令（真机场景）才绑定 0.0.0.0 并通告出口 IP candidate。
    let loopback_signal =
        server.contains("127.0.0.1") || server.contains("localhost") || server.contains("::1");
    // #157 M2：join 返回 TURN 配置时建立中继传输（失败仅告警，直连兜底）。
    let turn_transport = turn.as_ref().and_then(|tc| setup_turn(tc, loopback_signal));
    let direct = if loopback_signal {
        UdpSocket::bind("127.0.0.1:0").map_err(|e| ConnectError::Setup(format!("udp bind: {e}")))?
    } else {
        UdpSocket::bind("0.0.0.0:0").map_err(|e| ConnectError::Setup(format!("udp bind: {e}")))?
    };
    let bind_addr = direct
        .local_addr()
        .map_err(|e| ConnectError::Setup(format!("udp bind: {e}")))?;
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
    let mut endpoint = match codec {
        None => crate::Endpoint::new(),
        Some(Codec::H264) => crate::Endpoint::new_with_codec(Codec::H264),
        Some(c) => crate::Endpoint::new_with_codec(c),
    };
    for ip in &candidates {
        if force_relay {
            continue; // #201：只通告 relayed 候选，避免直连路径在 NAT/模拟器下丢媒体
        }
        let addr = std::net::SocketAddr::new(*ip, bind_addr.port());
        tracing::debug!("local candidate {addr}");
        endpoint
            .add_local_candidate(addr, Protocol::Udp)
            .map_err(|e| ConnectError::Setup(format!("candidate: {e:?}")))?;
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
            tracing::info!("relayed candidate {relayed} (local {local}) force_relay={force_relay}");
            if let Err(e) = endpoint.add_relay_candidate(relayed, local) {
                tracing::warn!("relay candidate rejected (TURN disabled): {e:?}");
            }
        }
    }
    // #12：viewer 的 offer 用 recvonly（SFU 拒绝 viewer 发布媒体）。
    // #216 M6：桥跨 PoP 音频转发需要双腿都协商音频 track（viewer recvonly /
    // publisher send）；仅 bridge（connect_live_role_codec）开启，CLI/移动端
    // 保持原 SDP 行为（with_audio=false）。
    if role == Role::Viewer {
        endpoint.add_video_recvonly();
        if with_audio {
            endpoint.add_audio_recvonly();
        }
    } else {
        endpoint.add_video();
        if with_audio {
            endpoint.add_audio();
        }
    }
    // 摄像头第二路视频轨（观看端 recvonly；被控端未发布时 m-line 保持 inactive）。
    if with_camera {
        if role == Role::Viewer {
            endpoint.add_camera_recvonly();
        } else {
            endpoint.add_camera();
        }
    }
    let (offer, pending, video_mid, audio_mid, camera_mid) = endpoint
        .create_offer()
        .map_err(|e| ConnectError::Sdp(format!("offer: {e:?}")))?;
    let offer_json = serde_json::to_string(&offer)
        .map_err(|e| ConnectError::Sdp(format!("offer serialize: {e}")))?;
    let answer_json = signal
        .exchange_description(&offer_json)
        .map_err(|e| ConnectError::Sdp(format!("answer: {e}")))?;
    // 非回环信令时剔除回答里的回环候选：SFU 为同机客户端附带 127.0.0.1 候选
    // （#216 桥/本机 CLI），远端客户端拿到后 ICE 可能把发送对端切到本机回环
    // ——发布端媒体全丢进黑洞（观看端仅收流不受影响）。对远端客户端而言，
    // 服务器的回环候选永无意义，剔除无条件正确。
    let answer_json = if loopback_signal {
        answer_json
    } else {
        strip_loopback_remote_candidates(&answer_json)
    };
    let answer: str0m::change::SdpAnswer = serde_json::from_str(&answer_json)
        .map_err(|e| ConnectError::Sdp(format!("answer parse: {e}")))?;
    endpoint
        .accept_answer(pending, answer)
        .map_err(|e| ConnectError::Sdp(format!("accept: {e:?}")))?;

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
        audio_mid,
        camera_mid,
        room: room.to_string(),
        peer_id,
        ice_connected,
    })
}

/// 观看端连接：WSS join → SDP 交换 → ICE 泵（5s 超时）。
/// #552 移动端（iOS/Android/OHOS）观看端 SIP 会话句柄：持有 SipCallLink 的
/// 看护线程至 Drop（链路存活 → BYE/注销不提前触发）；会话循环只驱动
/// endpoint/socket。
pub struct SipViewerSession {
    #[allow(dead_code)]
    pub call_id: String,
}

/// #552 移动端观看端 SIP 连接（agent connect_inner 的 viewer 路径收敛到 core，
/// 三平台共用）：REGISTER → INVITE 房间 → Answered → ICE 收敛。
///
/// - `server`：信令 URL（`ws://`→SIP/UDP 5060，`wss://`→SIP/TLS 5061；
///   显式 `sip_transport`/`sip_port` 覆盖）；
/// - `token`：Digest 口令（= 现有 auth token，§8 迁移期同一凭据）；
/// - `force_relay`：只通告 TURN 候选（#201 NAT/模拟器语义）；
/// - `with_audio`/`with_camera`：额外 recvonly 轨（未协商侧 m-line inactive）。
///
/// 注册等待覆盖两轮 SIP UDP 事务重传（~75s，见 #578 教训）；INVITE 等待 30s。
/// 探测本机出接口 IP（绑 0.0.0.0 连公共地址后取 local_addr；失败回退 loopback）。
fn egress_ip(port: u16) -> std::net::SocketAddr {
    use std::net::{IpAddr, Ipv4Addr, SocketAddr};
    if let Ok(probe) = std::net::UdpSocket::bind("0.0.0.0:0") {
        if probe.connect("8.8.8.8:53").is_ok()
            && let Ok(la) = probe.local_addr()
            && !la.ip().is_unspecified()
            && !la.ip().is_loopback()
        {
            return SocketAddr::new(la.ip(), port);
        }
    }
    SocketAddr::new(IpAddr::V4(Ipv4Addr::LOCALHOST), port)
}

pub fn connect_viewer_sip(
    server: &str,
    room: &str,
    token: Option<&str>,
    force_relay: bool,
    with_audio: bool,
    with_camera: bool,
    sip_transport: Option<&str>,
    sip_port: Option<u16>,
) -> Result<
    (
        SipViewerSession,
        crate::Endpoint,
        crate::media_socket::MediaSocket,
        str0m::media::Mid,
        Option<str0m::media::Mid>,
        Option<str0m::media::Mid>,
    ),
    String,
> {
    use str0m::net::Protocol;

    let device_id = format!("agent-viewer-{}", std::process::id());
    // 传输推导：显式参数 > URL scheme（wss=TLS/ws=UDP；无 scheme 按 ws）。
    let transport = sip_transport.unwrap_or({
        #[allow(clippy::match_like_matches_macro)]
        let tls = server.starts_with("wss");
        if tls { "tls" } else { "udp" }
    });
    let mut cfg = crate::sip_link::SipLinkConfig::from_parts(
        server,
        &device_id,
        token.unwrap_or(""),
        transport,
        sip_port.unwrap_or(0),
        "",
        "",
    )?;
    if cfg.transport == crate::protocol::sip_client::SipTransport::Tls && cfg.tls.is_none() {
        // from_parts 已注系统根；此处仅防御（理论不可达）。
        cfg.tls = Some(crate::protocol::sip_client::SipTlsConfig {
            ca_certs: crate::protocol::sip_client::system_ca_pem(),
            sni_hostname: None,
            client_cert: None,
            client_key: None,
        });
    }
    let mut link = crate::sip_link::SipCallLink::new(cfg);
    link.start();
    {
        let deadline = std::time::Instant::now() + std::time::Duration::from_secs(75);
        loop {
            let st = link.poll();
            if st.is_online() {
                break;
            }
            if std::time::Instant::now() >= deadline {
                return Err(format!("SIP 注册未完成（75s）：{st:?}"));
            }
            std::thread::sleep(std::time::Duration::from_millis(100));
        }
    }

    // 媒体 socket + 候选（agent connect_inner 同款；TURN 走 AERO_TURN_* env，
    // 移动端暂无本地 TURN 配置面——直连/后续切片接平台配置）。
    let loopback =
        server.contains("127.0.0.1") || server.contains("localhost") || server.contains("::1");
    let direct = std::net::UdpSocket::bind(if loopback { "127.0.0.1:0" } else { "0.0.0.0:0" })
        .map_err(|e| format!("bind udp: {e}"))?;
    let addr = direct.local_addr().map_err(|e| e.to_string())?;
    let turn = crate::turn_client::p2p_turn_transport(
        &std::env::var("AERO_TURN_URLS").unwrap_or_default(),
        &std::env::var("AERO_TURN_USERNAME").unwrap_or_default(),
        &std::env::var("AERO_TURN_CREDENTIAL").unwrap_or_default(),
    );
    let mut socket = crate::media_socket::MediaSocket::new(direct, turn);
    let mut endpoint = crate::Endpoint::new();
    let mut host_candidate = addr;
    if addr.ip().is_unspecified() {
        host_candidate = egress_ip(addr.port());
    }
    if !force_relay {
        endpoint
            .add_local_candidate(host_candidate, Protocol::Udp)
            .map_err(|e| format!("candidate: {e:?}"))?;
    }
    if let Some(tt) = socket.turn() {
        let relayed = tt.relayed_addr();
        if let Ok(la) = tt.local_addr() {
            let local = std::net::SocketAddr::new(host_candidate.ip(), la.port());
            if let Err(e) = endpoint.add_relay_candidate(relayed, local) {
                tracing::warn!("relay candidate rejected: {e:?}");
            }
        }
    }

    // 观看端轨：recvonly（含可选音频/摄像头）。
    endpoint.add_video_recvonly();
    if with_audio {
        endpoint.add_audio_recvonly();
    }
    if with_camera {
        endpoint.add_camera_recvonly();
    }
    let (offer, pending, video_mid, audio_mid, camera_mid) = endpoint
        .create_offer()
        .map_err(|e| format!("offer: {e:?}"))?;
    let offer_json = serde_json::to_string(&offer).map_err(|e| e.to_string())?;
    let call_id = format!("c-{}", std::process::id());
    // #503-4 呼叫授权口令：AERO_CALL_PASSWORD（被叫设备固定/临时密码）——
    // signal 对 INVITE 做 407 质询时以该口令应答；未配置则不附凭据。
    let call_password = std::env::var("AERO_CALL_PASSWORD")
        .ok()
        .filter(|s| !s.is_empty());
    link.call(room, &call_id, &offer_json, call_password.as_deref())
        .map_err(|e| format!("SIP INVITE: {e}"))?;
    let answer_json = {
        let deadline = std::time::Instant::now() + std::time::Duration::from_secs(30);
        let mut got: Result<String, String> = Err("SIP INVITE 无应答（30s）".into());
        'ans: while std::time::Instant::now() < deadline {
            let _ = link.poll();
            for ev in link.take_events() {
                match ev {
                    crate::sip_link::SipLinkEvent::Answered { answer_sdp, .. } => {
                        got = Ok(answer_sdp);
                        break 'ans;
                    }
                    crate::sip_link::SipLinkEvent::Rejected { status, .. } => {
                        got = Err(format!("SIP 呼叫被拒（{status}）"));
                        break 'ans;
                    }
                    _ => {}
                }
            }
            std::thread::sleep(std::time::Duration::from_millis(50));
        }
        got?
    };
    let answer: str0m::change::SdpAnswer =
        serde_json::from_str(&answer_json).map_err(|e| format!("answer parse: {e}"))?;
    endpoint
        .accept_answer(pending, answer)
        .map_err(|e| format!("accept answer: {e:?}"))?;

    // ICE 收敛（TURN 5-12s → 15s 窗口；事件队列留给会话循环）。
    {
        let deadline = std::time::Instant::now()
            + std::time::Duration::from_secs(if socket.turn().is_some() { 15 } else { 5 });
        while std::time::Instant::now() < deadline && endpoint.is_alive() {
            socket
                .set_read_timeout(Some(std::time::Duration::from_millis(10)))
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
                        destination: socket.local_addr().unwrap_or(source),
                        contents,
                    },
                ));
            }
            let _ = endpoint.handle_timeout(std::time::Instant::now());
            while let Some(out) = endpoint.poll_output() {
                match out {
                    str0m::Output::Transmit(t) => {
                        let _ = socket.send_to(&t.contents, t.destination);
                    }
                    str0m::Output::Timeout(_) => break,
                    str0m::Output::Event(_) => {}
                }
            }
            if endpoint.ice_connected() {
                break;
            }
        }
        if !endpoint.ice_connected() {
            return Err("ICE 连接超时（直连 5s / TURN 15s 未建立）".into());
        }
    }

    // 信令看护线程：持有 link 至进程退出（Drop 即 BYE/注销——会话循环只驱动
    // endpoint/socket）；后到 trickle 候选忽略（候选内联）。
    std::thread::Builder::new()
        .name("sip-link-watch".into())
        .spawn(move || {
            loop {
                let _ = link.poll();
                for ev in link.take_events() {
                    if let crate::sip_link::SipLinkEvent::PeerHangup { call_id, .. } = ev {
                        tracing::info!("SIP 对端挂断：{call_id}");
                    }
                }
                std::thread::sleep(std::time::Duration::from_millis(200));
            }
        })
        .ok();

    let video_mid = video_mid.ok_or("no video mid")?;
    Ok((
        SipViewerSession { call_id },
        endpoint,
        socket,
        video_mid,
        audio_mid,
        camera_mid,
    ))
}

/// #598 P1a 被控端 SIP 连接原语（UAS 形态，替代 `connect_live_role` Publisher 路径）：
/// REGISTER（device_id 即 AoR/用户名）→ 等首个 INVITE → 免授权静默接听
/// （P2pCall Callee 反演 offer → answer）→ ICE 收敛。
///
/// 消费者：跨 PoP 桥 pub 腿、host auto_publish 登录媒体、Android/OHOS 被控端。
/// 会话语义与 CLI agent connect_inner 的 publisher 路径一致——一次原语只承接
/// 一个呼叫（桥/host/移动端当前均为单会话场景）；后续 INVITE 由看护线程拒绝
/// （busy），不在此扩展多呼叫复用。`codec` 透传 P2pCallConfig（None=默认 H264）。
///
/// 返回 [`P2pCall`]（调用方驱动 `poll()` 泵媒体，endpoint/socket 经访问器取用）
/// 与 video/audio mid；SipCallLink 移入看护线程持有至进程退出（Drop 即 BYE/注销），
/// 与 `connect_viewer_sip` 同形。
pub fn connect_publisher_sip(
    server: &str,
    device_id: &str,
    token: Option<&str>,
    force_relay: bool,
    sip_transport: Option<&str>,
    sip_port: Option<u16>,
    codec: Option<crate::platform::Codec>,
) -> Result<
    (
        crate::p2p_call::P2pCall,
        Option<str0m::media::Mid>,
        Option<str0m::media::Mid>,
    ),
    String,
> {
    // 传输推导：显式参数 > URL scheme（wss=TLS/ws=UDP；无 scheme 按 ws）。
    let transport = sip_transport.unwrap_or({
        #[allow(clippy::match_like_matches_macro)]
        let tls = server.starts_with("wss");
        if tls { "tls" } else { "udp" }
    });
    let mut cfg = crate::sip_link::SipLinkConfig::from_parts(
        server,
        device_id,
        token.unwrap_or(""),
        transport,
        sip_port.unwrap_or(0),
        "",
        "",
    )?;
    if cfg.transport == crate::protocol::sip_client::SipTransport::Tls && cfg.tls.is_none() {
        // from_parts 已注系统根；此处仅防御（理论不可达）。
        cfg.tls = Some(crate::protocol::sip_client::SipTlsConfig {
            ca_certs: crate::protocol::sip_client::system_ca_pem(),
            sni_hostname: None,
            client_cert: None,
            client_key: None,
        });
    }
    let mut link = crate::sip_link::SipCallLink::new(cfg);
    link.start();
    {
        let deadline = std::time::Instant::now() + std::time::Duration::from_secs(75);
        loop {
            let st = link.poll();
            if st.is_online() {
                break;
            }
            if std::time::Instant::now() >= deadline {
                return Err(format!("SIP 注册未完成（75s）：{st:?}"));
            }
            std::thread::sleep(std::time::Duration::from_millis(100));
        }
    }

    // 等待来电并免授权静默接听（desktop UAS 流的收敛版）：IncomingCall →
    // P2pCall(Callee, Publisher) accept_offer → accept(answer)。等 INVITE 300s
    // （agent CLI 同款）；被叫无门禁——「开启被控」开关属上层 UI 语义。
    const INCOMING_TIMEOUT: Duration = Duration::from_secs(300);
    let (call_id, answer_sdp) = {
        let deadline = std::time::Instant::now() + INCOMING_TIMEOUT;
        let mut got: Result<(String, String), String> =
            Err("等待来电超时（300s 内无 INVITE）".into());
        'wait: while std::time::Instant::now() < deadline {
            let _ = link.poll();
            for ev in link.take_events() {
                match ev {
                    crate::sip_link::SipLinkEvent::IncomingCall {
                        call_id, offer_sdp, ..
                    } => {
                        got = Ok((call_id, offer_sdp));
                        break 'wait;
                    }
                    crate::sip_link::SipLinkEvent::Rejected { status, .. } => {
                        got = Err(format!("注册被拒（{status}）"));
                        break 'wait;
                    }
                    _ => {}
                }
            }
            std::thread::sleep(std::time::Duration::from_millis(100));
        }
        got?
    };
    let mut p2p = crate::p2p_call::P2pCall::new(crate::p2p_call::P2pCallConfig {
        role: crate::p2p_call::P2pRole::Callee,
        device_role: Role::Publisher,
        codec,
        with_audio: false,
        with_camera: false,
        force_relay,
        bind: "0.0.0.0:0".parse().unwrap(),
        turn: crate::turn_client::p2p_turn_transport(
            &std::env::var("AERO_TURN_URLS").unwrap_or_default(),
            &std::env::var("AERO_TURN_USERNAME").unwrap_or_default(),
            &std::env::var("AERO_TURN_CREDENTIAL").unwrap_or_default(),
        ),
        inline_candidates: true,
    })
    .map_err(|e| format!("被叫媒体端点创建失败：{e}"))?;
    let answer = p2p.accept_offer(&answer_sdp).map_err(|e| {
        let _ = link.reject(&call_id, "internal");
        format!("accept_offer 失败：{e}")
    })?;
    let video_mid = crate::p2p_call::offer_video_mid(&answer_sdp);
    let audio_mid = crate::p2p_call::offer_audio_mid(&answer_sdp);
    link.accept(&call_id, &answer)
        .map_err(|e| format!("SIP 接听失败：{e}"))?;

    // ICE 收敛（TURN 5-12s → 15s 窗口；与 viewer 同款口径）。
    {
        let budget = if p2p.socket().turn().is_some() { 15 } else { 5 };
        let deadline = std::time::Instant::now() + std::time::Duration::from_secs(budget);
        while std::time::Instant::now() < deadline && p2p.is_alive() {
            p2p.poll().map_err(|e| e.to_string())?;
            if p2p.ice_connected() {
                break;
            }
            std::thread::sleep(std::time::Duration::from_millis(10));
        }
        if !p2p.ice_connected() {
            return Err(format!(
                "ICE 连接超时（直连 5s / TURN 15s 未建立，budget={budget}s）"
            ));
        }
    }

    // 信令看护线程：持有 link 至进程退出；后到 INVITE 拒 busy，PeerHangup 记日志
    // （会话循环只驱动 P2pCall——与 viewer 看护职责对称）。
    std::thread::Builder::new()
        .name("sip-pub-watch".into())
        .spawn(move || {
            loop {
                let _ = link.poll();
                for ev in link.take_events() {
                    match ev {
                        crate::sip_link::SipLinkEvent::PeerHangup { call_id, .. } => {
                            tracing::info!("SIP 对端挂断：{call_id}");
                        }
                        crate::sip_link::SipLinkEvent::IncomingCall { call_id, .. } => {
                            let _ = link.reject(&call_id, "busy");
                        }
                        _ => {}
                    }
                }
                std::thread::sleep(std::time::Duration::from_millis(200));
            }
        })
        .ok();

    Ok((p2p, video_mid, audio_mid))
}

pub fn connect_viewer(server: &str, room: &str) -> Result<ConnectResult, ConnectError> {
    let live = connect_live_role(server, room, Role::Viewer, None)?;
    Ok(ConnectResult {
        room: live.room.clone(),
        peer_id: live.peer_id.clone(),
        ice_connected: live.ice_connected,
    })
}

#[cfg(test)]
mod tests {
    use super::{ConnectError, classify_join_error, strip_loopback_remote_candidates};

    #[test]
    fn strips_loopback_candidates_and_keeps_the_rest() {
        let json = r#"{"sdp":{"media":[{"candidates":[
            "candidate:1 1 UDP 2130706431 127.0.0.1 14778 typ host",
            "candidate:2 1 UDP 2130706431 129.226.150.174 14778 typ host",
            "candidate:3 1 UDP 2130706431 ::1 14778 typ host"
        ],"mid":"0"}]},"type":"answer"}"#;
        let out = strip_loopback_remote_candidates(json);
        assert!(!out.contains("127.0.0.1"), "回环候选应被剔除: {out}");
        assert!(!out.contains("::1"), "v6 回环候选应被剔除: {out}");
        assert!(out.contains("129.226.150.174"), "公网候选必须保留: {out}");
    }

    #[test]
    fn keeps_non_candidate_mentions_and_invalid_json_passthrough() {
        // 非候选字符串里的回环地址（如描述文本）不得误删
        let json = r#"{"note":"server at 127.0.0.1 is loopback","candidates":["candidate:1 1 UDP 1 10.0.0.2 9 typ host"]}"#;
        let out = strip_loopback_remote_candidates(json);
        assert!(
            out.contains("server at 127.0.0.1"),
            "非候选文本应保留: {out}"
        );
        assert!(out.contains("10.0.0.2"));
        // 非法 JSON 原样返回
        assert_eq!(strip_loopback_remote_candidates("not json"), "not json");
    }

    #[test]
    fn classify_join_error_auth_vs_other() {
        // signal main 的拒绝文案契约：auth failed → Auth；其余 → Join。
        // 服务器文案变更时此测试红，提醒同步分类（集中一处，见 classify_join_error）。
        assert!(matches!(
            classify_join_error("auth failed".into()),
            ConnectError::Auth(_)
        ));
        assert!(matches!(
            classify_join_error("room full".into()),
            ConnectError::Join(_)
        ));
        assert!(matches!(
            classify_join_error("server full".into()),
            ConnectError::Join(_)
        ));
        assert!(matches!(
            classify_join_error("too many signal redirects".into()),
            ConnectError::Join(_)
        ));
    }

    #[test]
    fn connect_error_displays_are_stable() {
        // Display 契约：调用方（desktop/CLI/session）按此显示，勿静默改文案。
        assert_eq!(
            ConnectError::Auth("auth failed".into()).to_string(),
            "认证失败: auth failed"
        );
        assert_eq!(
            ConnectError::Timeout(30).to_string(),
            "连接超时（30s）：信令/SFU 无响应"
        );
        assert_eq!(
            ConnectError::Join("room full".into()).to_string(),
            "join: room full"
        );
        assert_eq!(
            ConnectError::Signal("tls handshake".into()).to_string(),
            "signal connect: tls handshake"
        );
    }
}
