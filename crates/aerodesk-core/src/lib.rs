//! rd-core — 跨平台远程桌面客户端核心（Rust）。
//!
//! 平台无关的协议/媒体抽象层。P2 起填充实现。
//!
//! ```text
//! aerodesk-core
//! ├── endpoint   str0m 发布/观看端点（Sans-I/O 封装）
//! ├── signaling  WSS 信令客户端（aerodesk-protocol::signal）
//! └── media      媒体管线抽象 + VP8 测试媒体源
//! ```

pub mod access_unit;
pub mod avsync;
pub mod endpoint;
pub mod media;
pub mod media_pipeline;
pub mod pcmu;
pub mod signaling;

pub use endpoint::{ClientEvent, Endpoint};
pub mod connect;
