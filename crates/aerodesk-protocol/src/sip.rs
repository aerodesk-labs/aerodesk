//! SIP 信令语义层（#549 设计基线 / #550 协议层 / docs/SIP_SIGNALING.md）。
//!
//! 本模块是 SignalMessage ↔ SIP 的**纯数据映射层**：不触碰传输与事务
//! （rsipstack 的 dialog/transaction 集成在 #551 signal 侧与客户端侧落地），
//! 只提供：
//! - 身份与寻址（设备 ID ↔ AoR）；
//! - 13 个 SignalMessage 变体的 SIP 承载意图（全量覆盖，见 [`sip_intent`]）；
//! - 拒绝/错误码 ↔ SIP 响应码双向映射（规范 §3）；
//! - 严格子集判定（未列方法一律 501，规范 §6）；
//! - Trickle ICE body（`application/trickle-ice-sdpfrag`，RFC 8840）最小编解码；
//! - rsipstack 解析往返 smoke（依赖引入的编译期/运行期验证）。
//!
//! 约束（#549 评论定稿）：SIP 类型不外泄到媒体核心——core 媒体层只拿
//! SDP/ICE 参数；本模块是唯一的 SignalMessage↔SIP 翻译点。

use crate::signal::SignalMessage;
use rsipstack::rsip;

/// 协议版本（User-Agent 携带，双栈期版本协商用）。
pub const PROTOCOL_VERSION: &str = "aerodesk-sip/0.1";

/// P2P 能力 option-tag：`Require: aerodesk.p2p` → 不识别的对端回 420。
pub const P2P_OPTION_TAG: &str = "aerodesk.p2p";

/// SFU 作为 UAS 的 AoR 用户名（回退会话拨 `sip:sfu@<domain>`）。
pub const SFU_AOR_USER: &str = "sfu";

/// Trickle ICE body 的 Content-Type（RFC 8840）。
pub const TRICKLE_ICE_CONTENT_TYPE: &str = "application/trickle-ice-sdpfrag";

/// 默认 SIP 域（部署可覆盖；AoR 仅作路由键，不做 DNS 解析）。
pub const DEFAULT_DOMAIN: &str = "aerodesk.local";

// ---------------------------------------------------------------------------
// 身份与寻址（规范 §1）
// ---------------------------------------------------------------------------

/// 设备 ID → AoR（`sip:<device-id>@<domain>`）。
pub fn device_aor(device_id: &str, domain: &str) -> String {
    format!("sip:{device_id}@{domain}")
}

/// AoR/URI → 设备 ID（取 userinfo 的 user 部分；非 sip: 形如 None）。
pub fn device_from_uri(uri: &str) -> Option<&str> {
    let rest = uri
        .strip_prefix("sip:")
        .or_else(|| uri.strip_prefix("sips:"))?;
    let user = rest.split('@').next()?;
    // user 部分可能带参数（;transport=...）或密码（:pw@），本协议均不用。
    let user = user.split([';', ':']).next()?;
    if user.is_empty() { None } else { Some(user) }
}

// ---------------------------------------------------------------------------
// 错误码 ↔ SIP 响应码（规范 §3）
// ---------------------------------------------------------------------------

/// error_code → SIP 响应码（reject/失败语义）。
/// 未知码回 500（实现侧 bug，不该发生——调用方应只用表内码）。
pub fn error_code_to_status(code: &str) -> u16 {
    match code {
        "user_rejected" => 603,    // Decline
        "busy" => 486,             // Busy Here
        "offline" => 480,          // Temporarily Unavailable（注册过期不可达）
        "timeout" => 408,          // Request Timeout（proxy 生成）
        "control_disabled" => 403, // Forbidden（未开启被控，#545 策略拒绝）
        _ => 500,
    }
}

/// SIP 响应码 → error_code。404 与 480 同归 offline（AoR 不存在/注册过期）；
/// 487（CANCEL 后 Request Terminated）归 timeout——主叫视角取消与超时同态。
/// 非拒绝类响应码（2xx/1xx 等）返回 None。
pub fn status_to_error_code(status: u16) -> Option<&'static str> {
    match status {
        603 => Some("user_rejected"),
        486 => Some("busy"),
        480 | 404 => Some("offline"),
        408 | 487 => Some("timeout"),
        403 => Some("control_disabled"),
        _ => None,
    }
}

// ---------------------------------------------------------------------------
// 严格子集（规范 §6）：未列方法一律 501
// ---------------------------------------------------------------------------

/// 本协议实现的方法子集。其余（SUBSCRIBE/NOTIFY/PRACK/UPDATE/REFER/…）
/// 在 signal 入口直接回 501 Not Implemented。
pub fn is_implemented_method(m: &rsip::Method) -> bool {
    use rsip::Method::*;
    matches!(m, Register | Invite | Ack | Bye | Cancel | Info | Options)
}

/// 同 [`is_implemented_method`] 的常量形式：endpoint `Allow` 头与 501 门禁共用
///（signal 侧与 #552 客户端侧共用，避免两处清单漂移）。
pub const IMPLEMENTED_METHODS: &[rsip::Method] = &[
    rsip::Method::Register,
    rsip::Method::Invite,
    rsip::Method::Ack,
    rsip::Method::Bye,
    rsip::Method::Cancel,
    rsip::Method::Info,
    rsip::Method::Options,
];

// ---------------------------------------------------------------------------
// SignalMessage → SIP 承载意图（规范 §2，13 变体全量覆盖）
// ---------------------------------------------------------------------------

/// 每个 SignalMessage 变体在 SIP 面的承载方式。纯描述性枚举——
/// 供 signal/客户端的状态机消费，也是映射完整性的测试钩子。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SipIntent {
    /// Ping：无 SIP 对应——传输层 keepalive + Session-Timer 取代（规范 §2.1）。
    NoSipTransportKeepalive,
    /// Join → REGISTER（Digest 质询-应答由事务层完成）。
    Register,
    /// Joined → REGISTER 的 200 OK（peers 不下发；TURN 走 /config HTTP）。
    RegisterOk,
    /// Redirect → 302 Moved Temporarily（Contact = 目标 PoP），P0 可后置。
    Redirect302,
    /// Description → INVITE / 200 OK 的 SDP body（重协商 = re-INVITE）。
    InviteSdpBody,
    /// IceCandidate → INFO（application/trickle-ice-sdpfrag）。
    InfoTrickleIce,
    /// PeerLeft → 对话内 BYE；presence 离线另由注册过期/注销表达。
    ByeOrUnregister,
    /// Call → INVITE（call_id → Call-ID；timeout_ms → Expires）。
    Invite,
    /// CallRinging → 180 Ringing。
    Ringing180,
    /// CallAccepted → 200 OK + ACK（免授权静默接听 = 直接 200）。
    Ok200,
    /// CallRejected → 4xx/6xx（规范 §3 映射）+ JSON 正文携带 error_code。
    RejectStatus,
    /// Hangup → BYE（Reason 头，RFC 3326）。
    Bye,
    /// Error → 400/4xx/5xx + Warning；畸形报文 400（#542 口径）。
    ErrorStatus,
}

/// SignalMessage 变体 → SIP 承载意图。**映射完整性由单测保证**：
/// 新增变体必须在此处分支，否则编译失败（match 无通配）。
pub fn sip_intent(msg: &SignalMessage) -> SipIntent {
    match msg {
        SignalMessage::Ping => SipIntent::NoSipTransportKeepalive,
        SignalMessage::Join { .. } => SipIntent::Register,
        SignalMessage::Joined { .. } => SipIntent::RegisterOk,
        SignalMessage::Redirect { .. } => SipIntent::Redirect302,
        SignalMessage::Description { .. } => SipIntent::InviteSdpBody,
        SignalMessage::IceCandidate { .. } => SipIntent::InfoTrickleIce,
        SignalMessage::PeerLeft { .. } => SipIntent::ByeOrUnregister,
        SignalMessage::Call { .. } => SipIntent::Invite,
        SignalMessage::CallRinging { .. } => SipIntent::Ringing180,
        SignalMessage::CallAccepted { .. } => SipIntent::Ok200,
        SignalMessage::CallRejected { .. } => SipIntent::RejectStatus,
        SignalMessage::Hangup { .. } => SipIntent::Bye,
        SignalMessage::Error { .. } => SipIntent::ErrorStatus,
    }
}

// ---------------------------------------------------------------------------
// Trickle ICE body（RFC 8840 application/trickle-ice-sdpfrag）最小编解码
// ---------------------------------------------------------------------------

/// 一个 trickle 候选（与 SignalMessage::IceCandidate 的 candidate 字符串对应，
/// sdpMid/sdpMLineIndex 为 WebRTC 侧定位媒体线的索引）。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TrickleCandidate {
    pub candidate: String,
    pub sdp_mid: Option<String>,
    pub sdp_m_line_index: Option<u16>,
}

/// 编码为 sdpfrag 正文（行式，CRLF 结尾）。
pub fn encode_trickle(c: &TrickleCandidate) -> String {
    let mut s = String::new();
    if let Some(mid) = &c.sdp_mid {
        s.push_str(&format!("a=mid:{mid}\r\n"));
    }
    if let Some(idx) = c.sdp_m_line_index {
        s.push_str(&format!("a=m-line-index:{idx}\r\n"));
    }
    // candidate 属性行：入参允许带或不带 "a=candidate:"/"candidate:" 前缀。
    let cand = c.candidate.strip_prefix("a=").unwrap_or(&c.candidate);
    let cand = cand.strip_prefix("candidate:").unwrap_or(cand);
    s.push_str(&format!("a=candidate:{cand}\r\n"));
    s
}

/// 解码 sdpfrag 正文。宽松解析：只认 a=mid / a=m-line-index / a=candidate 行，
/// 其余行忽略（前向兼容 RFC 8840 的伪 m 线扩展）。
pub fn decode_trickle(body: &str) -> Option<TrickleCandidate> {
    let mut candidate = None;
    let mut sdp_mid = None;
    let mut sdp_m_line_index = None;
    for line in body.lines() {
        let line = line.trim_end_matches('\r');
        if let Some(v) = line.strip_prefix("a=mid:") {
            sdp_mid = Some(v.to_string());
        } else if let Some(v) = line.strip_prefix("a=m-line-index:") {
            sdp_m_line_index = v.parse().ok();
        } else if let Some(v) = line.strip_prefix("a=candidate:") {
            candidate = Some(v.to_string());
        }
    }
    candidate.map(|c| TrickleCandidate {
        candidate: c,
        sdp_mid,
        sdp_m_line_index,
    })
}

// ---------------------------------------------------------------------------
// rsipstack 集成 smoke：INVITE 构造 → rsip 解析往返
// ---------------------------------------------------------------------------

/// 构造最小 INVITE 文本（Call/CallAccepted 链的 SIP 面骨架）。
/// 仅用于协议层自测与后续 #551 的报文样例；生产构造器在 signal/客户端侧。
pub fn build_invite_skeleton(
    from_aor: &str,
    to_aor: &str,
    call_id: &str,
    offer_sdp: &str,
) -> String {
    format!(
        "INVITE {to_aor} SIP/2.0\r\n\
         Via: SIP/2.0/TLS client.local;branch=z9hG4bK-{call_id}\r\n\
         From: <{from_aor}>;tag=from-{call_id}\r\n\
         To: <{to_aor}>\r\n\
         Call-ID: {call_id}\r\n\
         CSeq: 1 INVITE\r\n\
         Contact: <{from_aor};transport=tls>\r\n\
         User-Agent: {PROTOCOL_VERSION}\r\n\
         Supported: {P2P_OPTION_TAG}\r\n\
         Content-Type: application/sdp\r\n\
         Content-Length: {}\r\n\r\n{offer_sdp}",
        offer_sdp.len()
    )
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::signal::{PeerInfo, Role, TurnConfig};
    use rsip::HeadersExt;

    // -- 寻址 --

    #[test]
    fn aor_roundtrip() {
        let aor = device_aor("AD-01AB3C", DEFAULT_DOMAIN);
        assert_eq!(aor, "sip:AD-01AB3C@aerodesk.local");
        assert_eq!(device_from_uri(&aor), Some("AD-01AB3C"));
        assert_eq!(device_from_uri("sips:AD-X@d.example"), Some("AD-X"));
        assert_eq!(device_from_uri("AD-01AB3C"), None); // 无 scheme
        assert_eq!(device_from_uri("sip:@d"), None); // 空 user
    }

    // -- 错误码双向映射（规范 §3 全量）--

    #[test]
    fn error_code_status_mapping_matches_spec() {
        let table = [
            ("user_rejected", 603u16),
            ("busy", 486),
            ("offline", 480),
            ("timeout", 408),
            ("control_disabled", 403),
        ];
        for (code, status) in table {
            assert_eq!(error_code_to_status(code), status, "{code}");
            assert_eq!(status_to_error_code(status), Some(code), "{status}");
        }
        // 同义码归并
        assert_eq!(status_to_error_code(404), Some("offline"));
        assert_eq!(status_to_error_code(487), Some("timeout"));
        // 非拒绝码不映射
        assert_eq!(status_to_error_code(200), None);
        assert_eq!(status_to_error_code(180), None);
    }

    // -- 严格子集（规范 §6）--

    #[test]
    fn method_subset_501_discipline() {
        use rsip::Method::*;
        for m in [Register, Invite, Ack, Bye, Cancel, Info, Options] {
            assert!(is_implemented_method(&m), "{m:?} 应实现");
        }
        for m in [Subscribe, Notify, PRack, Update, Refer, Publish, Message] {
            assert!(!is_implemented_method(&m), "{m:?} 应 501");
        }
    }

    // -- 映射全量覆盖（13/13 变体）--

    #[test]
    fn sip_intent_covers_every_variant() {
        let cases: Vec<(SignalMessage, SipIntent)> = vec![
            (SignalMessage::Ping, SipIntent::NoSipTransportKeepalive),
            (
                SignalMessage::Join {
                    room: "r".into(),
                    role: Role::Publisher,
                    auth_token: None,
                    dc_ready: false,
                },
                SipIntent::Register,
            ),
            (
                SignalMessage::Joined {
                    peer_id: "p".into(),
                    peers: vec![PeerInfo {
                        peer_id: "q".into(),
                        role: Role::Viewer,
                    }],
                    turn: Some(TurnConfig {
                        urls: vec!["turn:t".into()],
                        username: "u".into(),
                        credential: "c".into(),
                    }),
                },
                SipIntent::RegisterOk,
            ),
            (
                SignalMessage::Redirect {
                    pop: "eu".into(),
                    url: "wss://eu".into(),
                    reason: None,
                },
                SipIntent::Redirect302,
            ),
            (
                SignalMessage::Description {
                    from: "a".into(),
                    to: "b".into(),
                    description: "sdp".into(),
                },
                SipIntent::InviteSdpBody,
            ),
            (
                SignalMessage::IceCandidate {
                    from: "a".into(),
                    to: "b".into(),
                    candidate: "c".into(),
                },
                SipIntent::InfoTrickleIce,
            ),
            (
                SignalMessage::PeerLeft {
                    peer_id: "p".into(),
                },
                SipIntent::ByeOrUnregister,
            ),
            (
                SignalMessage::Call {
                    from: "a".into(),
                    target: "b".into(),
                    call_id: "c1".into(),
                    timeout_ms: Some(30_000),
                },
                SipIntent::Invite,
            ),
            (
                SignalMessage::CallRinging {
                    from: "b".into(),
                    to: "a".into(),
                    call_id: "c1".into(),
                },
                SipIntent::Ringing180,
            ),
            (
                SignalMessage::CallAccepted {
                    from: "b".into(),
                    to: "a".into(),
                    call_id: "c1".into(),
                },
                SipIntent::Ok200,
            ),
            (
                SignalMessage::CallRejected {
                    from: "b".into(),
                    to: "a".into(),
                    call_id: "c1".into(),
                    reason: None,
                    error_code: Some("control_disabled".into()),
                },
                SipIntent::RejectStatus,
            ),
            (
                SignalMessage::Hangup {
                    from: "a".into(),
                    to: "b".into(),
                    call_id: "c1".into(),
                    reason: Some("done".into()),
                },
                SipIntent::Bye,
            ),
            (
                SignalMessage::Error {
                    message: "x".into(),
                },
                SipIntent::ErrorStatus,
            ),
        ];
        assert_eq!(cases.len(), 13, "SignalMessage 变体数漂移——映射表需同步");
        for (msg, want) in cases {
            assert_eq!(sip_intent(&msg), want, "{msg:?}");
        }
    }

    // -- Trickle ICE body --

    #[test]
    fn trickle_roundtrip() {
        let c = TrickleCandidate {
            candidate: "candidate:1 1 UDP 2130706431 192.0.2.1 3478 typ host".into(),
            sdp_mid: Some("0".into()),
            sdp_m_line_index: Some(0),
        };
        let body = encode_trickle(&c);
        let back = decode_trickle(&body).expect("decode");
        assert_eq!(back.sdp_mid.as_deref(), Some("0"));
        assert_eq!(back.sdp_m_line_index, Some(0));
        assert_eq!(back.candidate, "1 1 UDP 2130706431 192.0.2.1 3478 typ host");
        assert!(decode_trickle("a=mid:0\r\n").is_none()); // 无 candidate 行
    }

    // -- rsipstack 集成 smoke --

    #[test]
    fn invite_skeleton_parses_with_rsipstack() {
        let sdp = "v=0\r\no=- 0 0 IN IP4 127.0.0.1\r\ns=-\r\nt=0 0\r\n";
        let text = build_invite_skeleton(
            &device_aor("AD-CALLER", DEFAULT_DOMAIN),
            &device_aor("AD-CALLEE", DEFAULT_DOMAIN),
            "call-42",
            sdp,
        );
        let req = rsip::Request::try_from(text.as_str()).expect("rsip 解析 INVITE");
        assert!(matches!(req.method, rsip::Method::Invite));
        assert_eq!(req.call_id_header().expect("Call-ID").value(), "call-42");
        // SDP body 端到端透传（signal 不解析，规范 §2.5）。
        assert_eq!(String::from_utf8_lossy(req.body()), sdp);
    }
}
