//! 客户端连接流程（SIP 单栈，#598 P4：WSS join 面已退役）。
//!
//! `connect_viewer_sip` / `connect_publisher_sip`：REGISTER → 呼叫/等被叫 → ICE 泵。
//! 供 CLI/UI/移动端壳层复用。

use std::net::UdpSocket;
use std::time::Duration;

use crate::platform::Codec;
use crate::protocol::signal::Role;

/// force-relay（#201/#218）：`AERODESK_FORCE_RELAY=1|true` 时 ICE 只通告
/// relayed 候选、跳过 host 候选；供 CLI 与 core 连接路径共用。
pub fn force_relay_env() -> bool {
    std::env::var("AERODESK_FORCE_RELAY")
        .ok()
        .is_some_and(|v| v == "1" || v.eq_ignore_ascii_case("true"))
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

/// #552 移动端（iOS/Android/OHOS）观看端 SIP 会话句柄：持有 SipCallLink 的
/// 看护线程至 Drop（链路存活 → BYE/注销不提前触发）；会话循环只驱动
/// endpoint/socket。
pub struct SipViewerSession {
    #[allow(dead_code)]
    pub call_id: String,
}

/// 客户端 UAC 连接公共实现（`connect_viewer_sip` 与 #598 v0.4 会议发布原语
/// 共用）：REGISTER → INVITE（目标设备/房间或会议 AoR）→ Answered → ICE
/// 收敛 → 看护线程持有 link 至进程退出。
///
/// - `video_sendonly`：true = 发布方向（`add_video`/`add_audio`，SFU 会议
///   role=publisher）；false = 观看方向（recvonly，SFU role=viewer）；
/// - `redirect_302`：true = 被控端语义——媒体期间的新 INVITE 一律回 302
///   （§4.1 被控端已在会议态时后续观看 INVITE 直接 302；无 Contact 由对端
///   按 §4.1 确定性推导 view AoR 重拨）；false = 仅记日志（观看端无人呼叫
///   本端 AoR）。
#[allow(clippy::too_many_arguments)] // 与 connect_viewer_sip 同参数面；内部私有实现
fn connect_sip_uac(
    server: &str,
    target_device: &str,
    device_id: &str,
    token: Option<&str>,
    force_relay: bool,
    with_audio: bool,
    with_camera: bool,
    video_sendonly: bool,
    redirect_302: bool,
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

    // 媒体轨方向：发布（sendonly 语义，SFU role=publisher）或观看（recvonly）。
    if video_sendonly {
        endpoint.add_video();
        if with_audio {
            endpoint.add_audio();
        }
    } else {
        endpoint.add_video_recvonly();
        if with_audio {
            endpoint.add_audio_recvonly();
        }
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
    link.call(
        target_device,
        &call_id,
        &offer_json,
        call_password.as_deref(),
    )
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
    // endpoint/socket）；后到 trickle 候选忽略（候选内联）。被控端会议语义
    // （redirect_302）：媒体期间新 INVITE 一律 302（§4.1——对端按确定性推导
    // view AoR 重拨，本端不解析 Contact）。
    std::thread::Builder::new()
        .name("sip-link-watch".into())
        .spawn(move || {
            loop {
                let _ = link.poll();
                for ev in link.take_events() {
                    match ev {
                        crate::sip_link::SipLinkEvent::PeerHangup { call_id, .. } => {
                            tracing::info!("SIP 对端挂断：{call_id}");
                        }
                        crate::sip_link::SipLinkEvent::IncomingCall { call_id, .. }
                            if redirect_302 =>
                        {
                            let _ = link.redirect_to_sfu(&call_id);
                        }
                        _ => {}
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
    let device_id = format!("agent-viewer-{}", std::process::id());
    connect_sip_uac(
        server,
        room,
        device_id.as_str(),
        token,
        force_relay,
        with_audio,
        with_camera,
        false, // 观看方向（recvonly，SFU role=viewer）
        false, // 观看端不重定向新 INVITE（无人呼叫观看端 AoR）
        sip_transport,
        sip_port,
    )
}

/// #598 v0.4 被控端入会（发布方向，§4.1 多方升级时序）：
/// REGISTER（device_id 即 AoR/用户名，与 1:1 UAS 阶段同身份）→ INVITE 会议
/// AoR（`sip:view-<device_id>@<domain>` 确定性推导，域与 REGISTER 同源）→
/// signal 会议桥按 offer 方向判定 role=publisher → SFU 以 UAS 应答 → ICE 收敛。
///
/// UAC 形态镜像 [`connect_viewer_sip`]（传输推导/注册等待/ICE 收敛/看护线程
/// 同一套实现），差异：视频（与可选音频）为**发送方向**；看护线程对媒体期间
/// 的新 INVITE 回 302（§4.1：被控端已在 SFU 态时后续观看 INVITE 一律直接
/// 302；rsipstack reject 无 Contact——对端按 §4.1 确定性推导 view AoR 重拨）。
///
/// 消费者：agent CLI publisher 升级后的会议阶段。返回（会话句柄, endpoint,
/// socket, video_mid, audio_mid, camera_mid=None）。
pub fn connect_publisher_sip_conference(
    server: &str,
    device_id: &str,
    token: Option<&str>,
    force_relay: bool,
    with_audio: bool,
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
    // 会议 AoR user 部分（§4.1 确定性推导）；域由 from_parts
    // （AERO_SIP_DOMAIN/缺省）统一决定，与 REGISTER/路由同源。
    let target = format!("view-{device_id}");
    connect_sip_uac(
        server,
        &target,
        device_id,
        token,
        force_relay,
        with_audio,
        false,
        true, // 发布方向（sendonly 语义，SFU role=publisher）
        true, // 被控端已在会议态：新 INVITE 一律 302
        sip_transport,
        sip_port,
    )
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
    // 就绪栅栏日志（与 agent CLI 同字面量——win-logon/bridge e2e grep 锚点）。
    tracing::info!("SIP registered: {device_id}");

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
