//! 控制面消息集 v1（docs/IPC_PROTOCOL.md §2/§3 的代码形态）。
//!
//! 线格式：`{"v":1,"kind":"hello", ...}`——[`Envelope`] 提供主版本字段 `v`，
//! [`Msg`] 以 `kind` 为内部标签做 snake_case 判别。兼容规则（§2）：
//! 未知字段一律忽略（serde 默认行为）；未知 `kind` 由连接层先经
//! [`probe_envelope`] 探出后回 `error{code:"unknown_kind"}`，不断连。
//!
//! `cmd.req` 的 schema 即 aerodesk-protocol 的 `CmdRequest`，但本 crate 只以
//! [`serde_json::Value`] 透传、不建依赖边——避免把 jsonwebtoken 等 crypto
//! 依赖拖进 IPC 层；desktop/host 两侧各自做 typed 编解码。

use serde::{Deserialize, Serialize};

/// 协议主版本（docs/IPC_PROTOCOL.md §2）。不兼容变更 +1；
/// 新增可选字段/新 kind 为同版本演进。
pub const PROTOCOL_VERSION: u32 = 1;

/// 版本化信封：`v` + 拍平的 `kind` 消息体。
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Envelope {
    pub v: u32,
    #[serde(flatten)]
    pub msg: Msg,
}

impl Envelope {
    pub fn new(msg: Msg) -> Self {
        Self {
            v: PROTOCOL_VERSION,
            msg,
        }
    }

    /// 序列化为帧载荷（JSON 字节）。
    pub fn to_json(&self) -> serde_json::Result<Vec<u8>> {
        serde_json::to_vec(self)
    }

    /// 从帧载荷解析。未知 `kind` / 畸形 JSON 在此报错——连接层若需区分
    /// 「未知 kind（可回错误帧续传）」与「帧损坏（关连接）」，先调
    /// [`probe_envelope`]。
    pub fn from_json(bytes: &[u8]) -> serde_json::Result<Self> {
        serde_json::from_slice(bytes)
    }
}

/// 轻量探针：只取 `v` 与 `kind`，用于未知 kind → error 帧的兼容路径（§2 规则 2/3）。
pub fn probe_envelope(bytes: &[u8]) -> serde_json::Result<(u32, String)> {
    #[derive(Deserialize)]
    struct Probe {
        v: u32,
        kind: String,
    }
    let p: Probe = serde_json::from_slice(bytes)?;
    Ok((p.v, p.kind))
}

/// 版本协商：取客户端 `[min_v, max_v]` 与本端主版本的交集（§2 规则 3）。
/// 有交集返回本端选定版本（恒为 [`PROTOCOL_VERSION`]，本端单版本），
/// 无交集返回 `None`（连接层回 `version_unsupported` 后关闭）。
pub fn negotiate_version(min_v: u32, max_v: u32) -> Option<u32> {
    (min_v <= PROTOCOL_VERSION && PROTOCOL_VERSION <= max_v).then_some(PROTOCOL_VERSION)
}

/// 控制面消息集 v1。`session` 为 host 分配的会话 ID（u64，贯穿日志）。
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum Msg {
    // ---- 握手与保活（§3.1）----
    /// C2S 连接建立后首帧。
    Hello {
        client: ClientKind,
        client_version: String,
        min_v: u32,
        max_v: u32,
    },
    /// S2C 握手应答，携带现存会话清单（UI 重开挂回入口，B4 用）。
    /// 协商结果即本帧信封的 `v`，不再单独携带。
    Welcome {
        server_version: String,
        sessions: Vec<SessionSummary>,
    },
    Ping {
        nonce: u64,
        sent_ms: u64,
    },
    Pong {
        nonce: u64,
        sent_ms: u64,
    },
    /// 协议级错误；致命错误后跟连接关闭。
    Error {
        code: String,
        message: String,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        session: Option<u64>,
    },

    // ---- 会话控制 C2S（§3.2）----
    Connect {
        room: String,
        server: String,
        token: String,
        mode: ConnectMode,
    },
    Disconnect {
        session: u64,
    },
    /// input data channel 原样转发。
    Input {
        session: u64,
        payload: String,
    },
    /// control 通道（选层/切显示器）。
    Control {
        session: u64,
        payload: String,
    },
    /// 终端命令通道；`req` schema 为 aerodesk_core::protocol::CmdRequest（透传）。
    Cmd {
        session: u64,
        req: serde_json::Value,
    },
    FileCmd {
        session: u64,
        cmd: FileCmdMsg,
    },
    ChatSend {
        session: u64,
        text: String,
    },
    /// 会话句柄上的 Atomic 开关热调；四字段均可选，缺省即不动。
    Tune {
        session: u64,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        muted: Option<bool>,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        volume: Option<f32>,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        show_camera: Option<bool>,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        view_only: Option<bool>,
    },

    // ---- 被控控制 C2S（§3.3）----
    PublisherStart {
        server: String,
        room: String,
        token: String,
        audio: bool,
        mouse: bool,
        view_only: bool,
    },
    PublisherStop,
    /// #456 授权开关（presence 接听策略）。
    PresenceSet {
        enabled: bool,
    },
    /// desktop 对 `incoming_call` 的应答。
    CallAnswer {
        call_id: String,
        accept: bool,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        reason: Option<String>,
    },

    // ---- S2C 会话建立（§3.2 回应）----
    SessionOpened {
        session: u64,
    },

    // ---- S2C 会话事件（§3.4，与 aerodesk-session `SessionUi` 逐法对应）----
    MainStatus {
        msg: String,
    },
    ConnState {
        state: i32,
    },
    Log {
        msg: String,
    },
    SessionStatus {
        session: u64,
        msg: String,
    },
    Joined {
        session: u64,
    },
    Cleanup {
        session: u64,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        terminal: Option<String>,
    },
    RemoteCursor {
        session: u64,
        x: f32,
        y: f32,
    },
    RecentAdd {
        room: String,
        server: String,
    },
    TerminalOutput {
        session: u64,
        text: String,
    },
    ChatMessage {
        session: u64,
        sender: String,
        text: String,
        own: bool,
    },
    MessageWindowStatus {
        session: u64,
        status: String,
    },
    FileWindowProgress {
        session: u64,
        progress: f32,
        label: String,
        status: String,
    },
    FileWindowClear {
        session: u64,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        status: Option<String>,
    },
    /// macOS 文案面。
    MainSessionStatus {
        msg: String,
    },
    FileProgress {
        session: u64,
        progress: f32,
        label: String,
    },
    CameraAvailable {
        session: u64,
        available: bool,
    },

    // ---- S2C 被控/presence 事件（§3.5）----
    PublisherEvent {
        state: PublisherState,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        msg: Option<String>,
    },
    /// signal_status / signal_online / presence_active 三件套。
    PresenceStatus {
        text: String,
        online: bool,
        active: bool,
    },
    /// #456 呼叫转入。
    IncomingCall {
        from: String,
        call_id: String,
    },
}

/// `hello.client`：客户端种类。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ClientKind {
    Desktop,
    Cli,
}

/// `welcome.sessions[]`：现存会话摘要。
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct SessionSummary {
    pub session: u64,
    pub room: String,
    pub state: String,
}

/// `connect.mode`。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ConnectMode {
    Control,
    View,
    Camera,
}

/// `file_cmd.cmd`：映射 aerodesk-session `FileCmd` 枚举（路径/图片跨进程
/// 只能以纯数据形态传输；图片 PNG 字节 base64 内联，大帧上限见 frame 模块）。
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(tag = "cmd", rename_all = "snake_case")]
pub enum FileCmdMsg {
    SendFile { path: String },
    SendClipboard { text: String },
    SendClipboardImage { png_b64: String },
    Cancel,
}

/// `publisher_event.state`：映射 aerodesk-session `PublisherEvent` 四态。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum PublisherState {
    Starting,
    Status,
    StartFailed,
    Stopped,
}

#[cfg(test)]
mod tests {
    use super::*;

    fn roundtrip(msg: Msg) {
        let env = Envelope::new(msg);
        let bytes = env.to_json().unwrap();
        let back = Envelope::from_json(&bytes).unwrap();
        assert_eq!(env, back);
    }

    #[test]
    fn roundtrip_all_variants() {
        let cases = vec![
            Msg::Hello {
                client: ClientKind::Desktop,
                client_version: "0.1.0".into(),
                min_v: 1,
                max_v: 1,
            },
            Msg::Welcome {
                server_version: "0.1.0".into(),
                sessions: vec![SessionSummary {
                    session: 7,
                    room: "r".into(),
                    state: "joined".into(),
                }],
            },
            Msg::Ping {
                nonce: 42,
                sent_ms: 1_700_000_000_000,
            },
            Msg::Pong {
                nonce: 42,
                sent_ms: 1_700_000_000_000,
            },
            Msg::Error {
                code: "unknown_kind".into(),
                message: "nope".into(),
                session: None,
            },
            Msg::Error {
                code: "session_gone".into(),
                message: "gone".into(),
                session: Some(3),
            },
            Msg::Connect {
                room: "r".into(),
                server: "wss://s".into(),
                token: "t".into(),
                mode: ConnectMode::Control,
            },
            Msg::Disconnect { session: 1 },
            Msg::Input {
                session: 1,
                payload: "{}".into(),
            },
            Msg::Control {
                session: 1,
                payload: "{}".into(),
            },
            Msg::Cmd {
                session: 1,
                req: serde_json::json!({"id": 1, "action": {"type": "run", "command": "ls"}}),
            },
            Msg::FileCmd {
                session: 1,
                cmd: FileCmdMsg::SendFile {
                    path: "C:\\a.bin".into(),
                },
            },
            Msg::FileCmd {
                session: 1,
                cmd: FileCmdMsg::SendClipboard { text: "hi".into() },
            },
            Msg::FileCmd {
                session: 1,
                cmd: FileCmdMsg::SendClipboardImage {
                    png_b64: "aGk=".into(),
                },
            },
            Msg::FileCmd {
                session: 1,
                cmd: FileCmdMsg::Cancel,
            },
            Msg::ChatSend {
                session: 1,
                text: "hello".into(),
            },
            Msg::Tune {
                session: 1,
                muted: Some(true),
                volume: None,
                show_camera: Some(false),
                view_only: None,
            },
            Msg::PublisherStart {
                server: "wss://s".into(),
                room: "r".into(),
                token: "t".into(),
                audio: true,
                mouse: true,
                view_only: false,
            },
            Msg::PublisherStop,
            Msg::PresenceSet { enabled: true },
            Msg::CallAnswer {
                call_id: "c1".into(),
                accept: false,
                reason: Some("busy".into()),
            },
            Msg::SessionOpened { session: 9 },
            Msg::MainStatus { msg: "m".into() },
            Msg::ConnState { state: 2 },
            Msg::Log { msg: "l".into() },
            Msg::SessionStatus {
                session: 1,
                msg: "s".into(),
            },
            Msg::Joined { session: 1 },
            Msg::Cleanup {
                session: 1,
                terminal: Some("done".into()),
            },
            Msg::Cleanup {
                session: 1,
                terminal: None,
            },
            Msg::RemoteCursor {
                session: 1,
                x: 0.5,
                y: 0.25,
            },
            Msg::RecentAdd {
                room: "r".into(),
                server: "s".into(),
            },
            Msg::TerminalOutput {
                session: 1,
                text: "out".into(),
            },
            Msg::ChatMessage {
                session: 1,
                sender: "peer".into(),
                text: "hi".into(),
                own: false,
            },
            Msg::MessageWindowStatus {
                session: 1,
                status: "open".into(),
            },
            Msg::FileWindowProgress {
                session: 1,
                progress: 0.5,
                label: "a.bin".into(),
                status: "sending".into(),
            },
            Msg::FileWindowClear {
                session: 1,
                status: None,
            },
            Msg::MainSessionStatus { msg: "m".into() },
            Msg::FileProgress {
                session: 1,
                progress: 1.0,
                label: "a.bin".into(),
            },
            Msg::CameraAvailable {
                session: 1,
                available: true,
            },
            Msg::PublisherEvent {
                state: PublisherState::Status,
                msg: Some("已在线".into()),
            },
            Msg::PublisherEvent {
                state: PublisherState::Stopped,
                msg: None,
            },
            Msg::PresenceStatus {
                text: "在线".into(),
                online: true,
                active: false,
            },
            Msg::IncomingCall {
                from: "peer".into(),
                call_id: "c1".into(),
            },
        ];
        assert_eq!(cases.len(), 43, "消息集覆盖计数（增删 kind 时同步）");
        for msg in cases {
            roundtrip(msg);
        }
    }

    #[test]
    fn wire_shape_is_flat_envelope_with_snake_kind() {
        let bytes = Envelope::new(Msg::Joined { session: 5 }).to_json().unwrap();
        let v: serde_json::Value = serde_json::from_slice(&bytes).unwrap();
        assert_eq!(
            v,
            serde_json::json!({"v": 1, "kind": "joined", "session": 5})
        );
    }

    #[test]
    fn unknown_fields_are_tolerated() {
        // 新字段前向兼容（§2 规则 1）：v2 假想帧多了 extra 字段，v1 端照常解析。
        let raw = br#"{"v":1,"kind":"joined","session":5,"extra":"ignored"}"#;
        let env = Envelope::from_json(raw).unwrap();
        assert_eq!(env.msg, Msg::Joined { session: 5 });
        // 可选字段缺省 → None。
        let raw = br#"{"v":1,"kind":"tune","session":1,"muted":true}"#;
        let env = Envelope::from_json(raw).unwrap();
        assert_eq!(
            env.msg,
            Msg::Tune {
                session: 1,
                muted: Some(true),
                volume: None,
                show_camera: None,
                view_only: None,
            }
        );
    }

    #[test]
    fn unknown_kind_is_probeable_then_rejected() {
        // §2 规则 2：连接层先探出 kind 回 error 帧，完整解析必然失败。
        let raw = br#"{"v":1,"kind":"future_thing","x":1}"#;
        let (v, kind) = probe_envelope(raw).unwrap();
        assert_eq!((v, kind.as_str()), (1, "future_thing"));
        assert!(Envelope::from_json(raw).is_err());
    }

    #[test]
    fn version_negotiation() {
        assert_eq!(negotiate_version(1, 1), Some(PROTOCOL_VERSION));
        assert_eq!(negotiate_version(1, 3), Some(PROTOCOL_VERSION));
        assert_eq!(negotiate_version(2, 3), None);
        assert_eq!(negotiate_version(0, 0), None);
    }
}
