//! 1:1 P2P 呼叫媒体核心（#552「媒体 P2P→TURN→SFU」的第一跳）。
//!
//! SIP 信令面（[`crate::sip_link::SipCallLink`]）只传 SDP 字符串与候选；
//! 本模块接媒体面：offer/answer 以**对端**为协商对象（不再强行经 SFU），
//! ICE 候选默认内联进 SDP，后到候选可经信令 `TrickleCandidate` 注入
//! （fork 的 `Rtc::add_remote_candidate`，见 str0m lib.rs trickle 用法）。
//!
//! 媒体核心不 import SIP 类型：候选以线格式字符串进出（`candidate:`
//! 属性串，RFC 5245 §15.1，与 `TrickleCandidate::candidate` 同形），
//! SDP 为 str0m `SdpOffer/SdpAnswer` 的 JSON 序列化（与 WSS/SFU 路径一致）。
//!
//! 调用方（desktop/agent/CLI）用法：
//! - 主叫（Caller）：`new(Caller…)` → `create_offer()` → `link.call(…, offer)`
//!   → 收到 `Answered(answer)` → `accept_answer()` → 循环 `poll()`；
//! - 被叫（Callee）：收到 `IncomingCall(offer)` → `new(Callee…)` →
//!   `accept_offer()` → `link.accept(…, answer)` → 循环 `poll()`；
//! - 候选：`inline_candidates=true`（默认）全部内联；trickle-only 流程把
//!   `add_local_candidate()` 返回的属性串经 `link.send_trickle()` 转发，
//!   对端 `add_remote_candidate()` 注入（往返保序由信令面保证）。
//!
//! 方向语义：Callee 不预配媒体轨——str0m 的 `accept_offer` 按 offer 逐
//! m-line 反演方向（`m.direction().invert()`），被叫发/收自动匹配主叫
//! 的收/发；`device_role/with_audio/with_camera` 在 Caller 侧生效。
//!
//! 会话就绪契约：`ice_connected()` 仅表示 ICE 选路完成，DTLS 握手还需
//! 数个往返才结束（SRTP 密钥随之就绪）。**早于就绪写媒体帧会被对端以
//! 「无 SRTP 接收上下文」静默丢弃**（PCMU/Opus 无重传，帧一去不返）。
//! 调用方以「收到首个 `ChannelOpen` 事件」为会话就绪信号再写流。

use std::net::{IpAddr, SocketAddr, UdpSocket};
use std::time::{Duration, Instant};

use str0m::Output;
use str0m::change::{SdpAnswer, SdpOffer};
use str0m::net::Protocol;

use crate::connect::discover_local_ip;
use crate::endpoint::{ClientEvent, Endpoint};
use crate::media_socket::MediaSocket;
use crate::platform::Codec;
use crate::protocol::signal::Role;
use crate::turn_client::TurnTransport;

/// 本端在呼叫中的角色（Caller=发 offer 方，Callee=出 answer 方，RFC 3264 尊称）。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum P2pRole {
    Caller,
    Callee,
}

/// P2P 呼叫媒体错误。
#[derive(Debug, thiserror::Error)]
pub enum P2pError {
    #[error("setup: {0}")]
    Setup(String),
    #[error("sdp: {0}")]
    Sdp(String),
    #[error("io: {0}")]
    Io(String),
}

/// P2P 呼叫媒体配置（`TurnTransport` 无 Debug/Clone：按值移交，不派生）。
pub struct P2pCallConfig {
    pub role: P2pRole,
    /// 设备角色（仅 Caller 侧生效：Publisher 发流 / Viewer 收流；
    /// Callee 的方向由 offer 反演，无需配置）。
    pub device_role: Role,
    /// 视频 codec（None=默认全开；双侧建议一致）。
    pub codec: Option<Codec>,
    /// 是否协商音频（仅 Caller 侧生效）。
    pub with_audio: bool,
    /// 是否协商第二路视频（摄像头，仅 Caller 侧生效）。
    pub with_camera: bool,
    /// 只通告 relayed（TURN）候选（NAT/模拟器下防直连黑洞，#201 同款语义）。
    pub force_relay: bool,
    /// 媒体 socket 绑定地址（回环测试 127.0.0.1:0；真机 0.0.0.0:0）。
    pub bind: SocketAddr,
    /// TURN 中继（None=直连）。
    pub turn: Option<TurnTransport>,
    /// 候选是否内联进 SDP；false = 协商后经信令 trickle 补充。
    pub inline_candidates: bool,
}

/// [`P2pCall::create_offer`] 的产物：SDP 字符串 + 本端各轨 mid（发送/编码用）。
#[derive(Debug, Clone)]
pub struct P2pOffer {
    pub sdp: String,
    pub video_mid: Option<str0m::media::Mid>,
    pub audio_mid: Option<str0m::media::Mid>,
    pub camera_mid: Option<str0m::media::Mid>,
}

/// 1:1 P2P 呼叫媒体（Sans-I/O：由调用方驱动 `poll()` 泵）。
pub struct P2pCall {
    force_relay: bool,
    endpoint: Endpoint,
    socket: MediaSocket,
    pending: Option<str0m::change::SdpPendingOffer>,
    bytes_received: u64,
}

impl P2pCall {
    /// 建端：绑定媒体 socket + 本地候选（inline）+ 媒体轨（Caller 侧）。
    pub fn new(cfg: P2pCallConfig) -> Result<Self, P2pError> {
        let direct = UdpSocket::bind(cfg.bind)
            .map_err(|e| P2pError::Setup(format!("udp bind {}: {e}", cfg.bind)))?;
        let bind_addr = direct
            .local_addr()
            .map_err(|e| P2pError::Setup(format!("udp local_addr: {e}")))?;
        let mut endpoint = match cfg.codec {
            None => Endpoint::new(),
            Some(c) => Endpoint::new_with_codec(c),
        };
        if cfg.inline_candidates {
            for ip in media_candidates(bind_addr, !bind_addr.ip().is_loopback()) {
                if cfg.force_relay {
                    continue; // #201：只通告 relayed，避免直连路径在 NAT 下丢媒体
                }
                let addr = SocketAddr::new(ip, bind_addr.port());
                tracing::debug!("p2p local candidate {addr}");
                if let Err(e) = endpoint.add_local_candidate(addr, Protocol::Udp) {
                    return Err(P2pError::Setup(format!("candidate: {e:?}")));
                }
            }
            if let Some(tt) = cfg.turn.as_ref() {
                let relayed = tt.relayed_addr();
                if let Ok(la) = tt.local_addr() {
                    let local_ip = media_candidates(bind_addr, !bind_addr.ip().is_loopback())
                        .first()
                        .copied()
                        .unwrap_or(IpAddr::V4(std::net::Ipv4Addr::LOCALHOST));
                    let local = SocketAddr::new(local_ip, la.port());
                    tracing::info!("p2p relayed candidate {relayed} (local {local})");
                    if let Err(e) = endpoint.add_relay_candidate(relayed, local) {
                        tracing::warn!("p2p relay candidate rejected (TURN disabled): {e:?}");
                    }
                }
            }
        }
        // 媒体轨：仅 Caller 需显式配置（Callee 由 offer 反演）。
        if cfg.role == P2pRole::Caller {
            match cfg.device_role {
                Role::Viewer => {
                    endpoint.add_video_recvonly();
                    if cfg.with_audio {
                        endpoint.add_audio_recvonly();
                    }
                    if cfg.with_camera {
                        endpoint.add_camera_recvonly();
                    }
                }
                _ => {
                    endpoint.add_video();
                    if cfg.with_audio {
                        endpoint.add_audio();
                    }
                    if cfg.with_camera {
                        endpoint.add_camera();
                    }
                }
            }
        }
        Ok(Self {
            force_relay: cfg.force_relay,
            endpoint,
            socket: MediaSocket::new(direct, cfg.turn),
            pending: None,
            bytes_received: 0,
        })
    }

    /// 主叫：创建 offer（含双端一致全套数据通道：input/cursor/file/cmd/…，
    /// 与 SFU 路径同构——1:1 会话同样需要控制/光标/传输能力）。
    pub fn create_offer(&mut self) -> Result<P2pOffer, P2pError> {
        let (offer, pending, video_mid, audio_mid, camera_mid) = self
            .endpoint
            .create_offer()
            .map_err(|e| P2pError::Sdp(format!("offer: {e:?}")))?;
        let sdp = serde_json::to_string(&offer)
            .map_err(|e| P2pError::Sdp(format!("offer serialize: {e}")))?;
        self.pending = Some(pending);
        Ok(P2pOffer {
            sdp,
            video_mid,
            audio_mid,
            camera_mid,
        })
    }

    /// 被叫：接受 offer → answer（m-line 方向按 offer 反演）。
    pub fn accept_offer(&mut self, offer_sdp: &str) -> Result<String, P2pError> {
        let offer: SdpOffer = serde_json::from_str(offer_sdp)
            .map_err(|e| P2pError::Sdp(format!("offer parse: {e}")))?;
        let answer = self
            .endpoint
            .accept_offer(offer)
            .map_err(|e| P2pError::Sdp(format!("answer: {e:?}")))?;
        serde_json::to_string(&answer).map_err(|e| P2pError::Sdp(format!("answer serialize: {e}")))
    }

    /// 主叫：接受 answer。
    pub fn accept_answer(&mut self, answer_sdp: &str) -> Result<(), P2pError> {
        let answer: SdpAnswer = serde_json::from_str(answer_sdp)
            .map_err(|e| P2pError::Sdp(format!("answer parse: {e}")))?;
        let pending = self
            .pending
            .take()
            .ok_or_else(|| P2pError::Sdp("无待决 offer（先 create_offer）".into()))?;
        self.endpoint
            .accept_answer(pending, answer)
            .map_err(|e| P2pError::Sdp(format!("accept: {e:?}")))
    }

    /// 追加本地候选（trickle-only 流程或 else ICE restart 后）：
    /// 返回 `candidate:` 属性串，调用方经信令转发给对端。
    pub fn add_local_candidate(&mut self, addr: SocketAddr) -> Result<String, P2pError> {
        let sdp = self
            .endpoint
            .add_local_candidate(addr, Protocol::Udp)
            .map_err(|e| P2pError::Setup(format!("candidate {addr}: {e:?}")))?;
        Ok(sdp)
    }

    /// 注入对端后到候选（sdpfrag 的 `candidate:` 属性串）。
    pub fn add_remote_candidate(&mut self, sdp_candidate: &str) -> Result<(), P2pError> {
        self.endpoint
            .add_remote_candidate(sdp_candidate)
            .map_err(P2pError::Setup)
    }

    /// 泵一轮：排空收包 → 时间推进 → 排空输出（Transmit/Timeout）。
    /// 空转代价 ≤1ms（回环/本地立即返回）。
    pub fn poll(&mut self) -> Result<(), P2pError> {
        // 排空式读取（LESSON_客户端网络泵需排空式读取避免SCTP-ACK饥饿）：
        // 首包等待 ≤1ms，积压时连续排空直至 WouldBlock。
        self.socket
            .set_read_timeout(Some(Duration::from_millis(1)))
            .ok();
        let mut buf = [0u8; 2048];
        for _ in 0..128 {
            match self.socket.recv_from(&mut buf) {
                Ok((n, source)) => {
                    self.bytes_received = self.bytes_received.saturating_add(n as u64);
                    let Ok(contents) = buf[..n].try_into() else {
                        continue;
                    };
                    let _ = self.endpoint.handle_input(str0m::Input::Receive(
                        Instant::now(),
                        str0m::net::Receive {
                            proto: Protocol::Udp,
                            source,
                            destination: self.socket.local_addr().unwrap_or(source),
                            contents,
                        },
                    ));
                }
                Err(e)
                    if e.kind() == std::io::ErrorKind::WouldBlock
                        || e.kind() == std::io::ErrorKind::TimedOut =>
                {
                    break;
                }
                Err(e) => return Err(P2pError::Io(e.to_string())),
            }
        }
        let _ = self.endpoint.handle_timeout(Instant::now());
        while let Some(out) = self.endpoint.poll_output() {
            match out {
                Output::Transmit(t) => {
                    let _ = self.socket.send_to(&t.contents, t.destination);
                }
                // 关键：Timeout 必须退出本轮排空（回喂会反复返回同一 Timeout
                // → 100% CPU 死循环，connect 主循环同款处理）。
                Output::Timeout(_) => break,
                // Endpoint 包装层已把 Event 转 ClientEvent 队列——包装未漏，仅类型穷尽。
                Output::Event(_) => {}
            }
        }
        Ok(())
    }

    /// 下一个客户端事件（IceConnected/Media/ChannelOpen/…，与会话层同形）。
    pub fn poll_event(&mut self) -> Option<ClientEvent> {
        self.endpoint.poll_event()
    }

    pub fn ice_connected(&self) -> bool {
        self.endpoint.ice_connected()
    }

    pub fn is_alive(&self) -> bool {
        self.endpoint.is_alive()
    }

    /// 本端媒体 socket 地址（candidate 构造/诊断用）。
    pub fn local_addr(&self) -> SocketAddr {
        self.socket
            .local_addr()
            .unwrap_or_else(|_| SocketAddr::new(IpAddr::V4(std::net::Ipv4Addr::LOCALHOST), 0))
    }

    /// 累计收到的媒体字节（诊断/测试断言用）。
    pub fn bytes_received(&self) -> u64 {
        self.bytes_received
    }

    /// str0m 端点（发送帧/读写通道由调用方经此驱动）。
    pub fn endpoint(&mut self) -> &mut Endpoint {
        &mut self.endpoint
    }

    /// 媒体 socket（诊断：TURN 路径查询等）。
    pub fn socket(&self) -> &MediaSocket {
        &self.socket
    }

    pub fn force_relay(&self) -> bool {
        self.force_relay
    }
}

/// 由绑定地址推导 ICE host candidate 候选 IP（语义与 connect 路径一致：
/// 通配符绑定不产生 candidate，改探测出口 IP；都失败回退 127.0.0.1）。
fn media_candidates(bind_addr: SocketAddr, discover_external: bool) -> Vec<IpAddr> {
    let mut candidates = Vec::new();
    let ip = bind_addr.ip();
    if !ip.is_unspecified() {
        candidates.push(ip);
    }
    if discover_external
        && let Some(local) = discover_local_ip()
        && !candidates.contains(&local)
    {
        candidates.push(local);
    }
    if candidates.is_empty() {
        candidates.push(IpAddr::V4(std::net::Ipv4Addr::LOCALHOST));
    }
    candidates
}

/// 从 offer JSON（`{"type":"offer","sdp":"<SDP 文本>"}`）取**第一个视频 m-line**
/// 的 mid——Callee 侧发送视频帧用：m-line 的 mid 双侧一致，answer 按方向反演但
/// 不改 mid。offer 不含视频（纯音频/数据通道）返回 None。
pub fn offer_video_mid(offer_sdp: &str) -> Option<str0m::media::Mid> {
    let v: serde_json::Value = serde_json::from_str(offer_sdp).ok()?;
    let sdp = v.get("sdp")?.as_str()?;
    let mut lines = sdp.lines();
    while let Some(line) = lines.next() {
        if line.starts_with("m=video") {
            for l in lines.by_ref() {
                if let Some(mid) = l.strip_prefix("a=mid:") {
                    return Some(str0m::media::Mid::from(mid));
                }
            }
            return None;
        }
    }
    None
}

#[cfg(test)]
mod tests {
    use super::*;

    fn call_config(role: P2pRole, device_role: Role, inline: bool) -> P2pCallConfig {
        P2pCallConfig {
            role,
            device_role,
            codec: None,
            with_audio: true,
            with_camera: false,
            force_relay: false,
            bind: "127.0.0.1:0".parse().unwrap(),
            turn: None,
            inline_candidates: inline,
        }
    }

    fn pump_pair(
        caller: &mut P2pCall,
        callee: &mut P2pCall,
        timeout: Duration,
    ) -> Vec<ClientEvent> {
        let deadline = Instant::now() + timeout;
        let mut events = Vec::new();
        while Instant::now() < deadline {
            let _ = caller.poll();
            let _ = callee.poll();
            while let Some(ev) = caller.poll_event() {
                events.push(ev);
            }
            while let Some(ev) = callee.poll_event() {
                events.push(ev);
            }
            if caller.ice_connected() && callee.ice_connected() {
                return events;
            }
            std::thread::sleep(Duration::from_millis(2));
        }
        events
    }

    /// 内联候选：回环 UDP 双侧 ICE 建链 + DTLS 存活 + 双向有包。
    #[test]
    fn p2p_ice_connects_loopback_inline() {
        let mut caller = P2pCall::new(call_config(P2pRole::Caller, Role::Viewer, true)).unwrap();
        let mut callee = P2pCall::new(call_config(P2pRole::Callee, Role::Publisher, true)).unwrap();
        let offer = caller.create_offer().unwrap();
        let answer = callee.accept_offer(&offer.sdp).unwrap();
        caller.accept_answer(&answer).unwrap();
        let events = pump_pair(&mut caller, &mut callee, Duration::from_secs(10));
        assert!(
            caller.ice_connected() && callee.ice_connected(),
            "10s 内应 ICE 建链（events={events:?}）"
        );
        assert!(caller.is_alive() && callee.is_alive());
        // ICE/DTLS/RTP 双向均有包（STUN 亦计入——建链证据且双向非零）。
        assert!(caller.bytes_received() > 0, "caller 应有收包");
        assert!(callee.bytes_received() > 0, "callee 应有收包");
    }

    /// trickle-only：协商时不带候选，连接建立后经
    /// add_local_candidate → add_remote_candidate 往返注入，ICE 仍建链。
    #[test]
    fn p2p_ice_connects_via_trickle_candidates() {
        let mut caller = P2pCall::new(call_config(P2pRole::Caller, Role::Viewer, false)).unwrap();
        let mut callee =
            P2pCall::new(call_config(P2pRole::Callee, Role::Publisher, false)).unwrap();
        let offer = caller.create_offer().unwrap();
        let answer = callee.accept_offer(&offer.sdp).unwrap();
        caller.accept_answer(&answer).unwrap();
        // 协商完成后才出现候选（模拟 srflx/relay 后到 + 信令 sdpfrag 往返）。
        let caller_cand = caller.add_local_candidate(caller.local_addr()).unwrap();
        let callee_cand = callee.add_local_candidate(callee.local_addr()).unwrap();
        assert!(caller_cand.starts_with("candidate:"));
        assert!(callee_cand.starts_with("candidate:"));
        callee.add_remote_candidate(&caller_cand).unwrap();
        caller.add_remote_candidate(&callee_cand).unwrap();
        let events = pump_pair(&mut caller, &mut callee, Duration::from_secs(10));
        assert!(
            caller.ice_connected() && callee.ice_connected(),
            "trickle-only 应能建链（events={events:?}）"
        );
    }

    /// 泵到会话就绪：两侧 ICE 建链且有数据通道打开（= DTLS 完成 = SRTP 密钥
    /// 就绪）。ICE connected 只代表选路完成，DTLS 握手还需数个往返才结束——
    /// 早于 DTLS 就绪写媒体会被对端以「无 SRTP 接收上下文」丢弃且无重传。
    fn pump_pair_ready(
        caller: &mut P2pCall,
        callee: &mut P2pCall,
        timeout: Duration,
    ) -> Vec<ClientEvent> {
        let deadline = Instant::now() + timeout;
        let mut events = Vec::new();
        let mut channels_open = (0u32, 0u32);
        while Instant::now() < deadline {
            let _ = caller.poll();
            let _ = callee.poll();
            while let Some(ev) = caller.poll_event() {
                if matches!(ev, ClientEvent::ChannelOpen(..)) {
                    channels_open.0 += 1;
                }
                events.push(ev);
            }
            while let Some(ev) = callee.poll_event() {
                if matches!(ev, ClientEvent::ChannelOpen(..)) {
                    channels_open.1 += 1;
                }
                events.push(ev);
            }
            if caller.ice_connected()
                && callee.ice_connected()
                && channels_open.0 >= 1
                && channels_open.1 >= 1
            {
                return events;
            }
            std::thread::sleep(Duration::from_millis(2));
        }
        events
    }

    /// 媒体载荷端到端：被叫（Publisher）经协商 mid 发 PCMU 帧，主叫（Viewer）
    /// 收到 ClientEvent::Media——DTLS/SRTP 载荷链路贯通（不依赖编解码器）。
    #[test]
    fn p2p_media_payload_flows_after_connect() {
        let mut caller = P2pCall::new(call_config(P2pRole::Caller, Role::Viewer, true)).unwrap();
        let mut callee = P2pCall::new(call_config(P2pRole::Callee, Role::Publisher, true)).unwrap();
        let offer = caller.create_offer().unwrap();
        let answer = callee.accept_offer(&offer.sdp).unwrap();
        caller.accept_answer(&answer).unwrap();
        // 会话就绪（DTLS/SRTP 密钥就绪）后再写媒体——早写会被对端丢帧（无重传）。
        let events = pump_pair_ready(&mut caller, &mut callee, Duration::from_secs(10));
        assert!(
            caller.ice_connected() && callee.ice_connected(),
            "10s 内应 ICE 建链（events={events:?}）"
        );
        let audio_mid = offer.audio_mid.expect("with_audio=true 应有音频 mid");
        let frame: std::sync::Arc<[u8]> = std::sync::Arc::from(vec![0u8; 160]);
        callee
            .endpoint()
            .send_audio_frame(
                audio_mid,
                frame,
                str0m::media::MediaTime::new(0, str0m::media::Frequency::EIGHT_KHZ),
            )
            .expect("写音频帧");
        // 泵到 Media 事件出现（5s 兜底）。
        let deadline = Instant::now() + Duration::from_secs(5);
        let mut got_media = false;
        while Instant::now() < deadline {
            let _ = caller.poll();
            let _ = callee.poll();
            while let Some(ev) = caller.poll_event() {
                if matches!(ev, ClientEvent::Media(_)) {
                    got_media = true;
                }
            }
            while callee.poll_event().is_some() {}
            if got_media {
                break;
            }
            std::thread::sleep(Duration::from_millis(2));
        }
        assert!(got_media, "主叫应收到被叫的 PCMU 媒体事件");
    }

    /// offer 视频 mid 推导：Callee 侧发送视频帧用（与主叫 create_offer 的
    /// video_mid 一致——mid 双侧同值）。
    #[test]
    fn offer_video_mid_matches_caller_video_mid() {
        let mut caller = P2pCall::new(call_config(P2pRole::Caller, Role::Viewer, true)).unwrap();
        let offer = caller.create_offer().unwrap();
        assert_eq!(offer_video_mid(&offer.sdp), offer.video_mid);
        // 畸形/纯 SDP 无视频输入：None 分支健壮性。
        assert_eq!(offer_video_mid("not-json"), None);
        assert_eq!(offer_video_mid(r#"{"type":"offer","sdp":"v=0\r\n"}"#), None);
    }
}
