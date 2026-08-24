//! rd-core — 跨平台远程桌面客户端核心（Rust）。
//!
//! 平台无关的协议/媒体抽象层。P2 起填充实现。
//!
//! ```text
//! aerodesk-core
//! ├── endpoint   str0m 发布/观看端点（Sans-I/O 封装）
//! ├── chat      聊天消息收发（#458，`chat` data channel）
//! ├── signaling         WSS 信令客户端（protocol::signal）
//! ├── protocol   跨端共享协议类型（原 aerodesk-protocol，2026-08-19 并入）
//! ├── signal_presence   常驻信令连接管理器（状态机 + 自动重连）
//! ├── signal_call       被叫侧呼叫状态机（响铃/接听/挂断/超时）
//! ├── p2p_call    1:1 P2P 呼叫媒体核心（#552：offer/answer 以对端为协商对象）
//! └── media      媒体管线抽象 + VP8 测试媒体源
//! ```

pub use aerodesk_protocol::access_unit;
pub mod audio_sink;
pub mod avsync;
pub mod chat;
pub mod clipboard;
pub mod cmd_exec;
pub mod endpoint;
pub mod file_transfer;
pub mod media;
pub mod media_socket;
pub mod p2p_call;
pub mod pcmu;
pub mod platform;
pub mod protocol;
pub mod signaling;
pub mod sip_link;
pub mod synthetic;

pub use endpoint::{ClientEvent, Endpoint};
pub mod connect;

pub mod turn_client;
pub mod util;
