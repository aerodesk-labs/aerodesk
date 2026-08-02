//! aerodesk-core — 跨平台远程桌面客户端核心（Rust）。
//!
//! 平台无关的协议/媒体抽象层，P2 桌面客户端起填充实现。各平台只实现
//! 本 crate 定义的 trait（采集/编码/解码/渲染/注入），协议逻辑全部复用。
//!
//! ```text
//! aerodesk-core
//! ├── rtc_endpoint   str0m 发布/观看端点（与 aerodesk-sfu 同栈）
//! ├── media_pipeline 帧↔RTP 抽象（采集/编码/解码/渲染/注入 trait）
//! └── signaling_client 信令客户端（aerodesk-protocol::signal）
//! ```

pub mod media_pipeline;
pub mod rtc_endpoint;
pub mod signaling_client;

pub use media_pipeline::{Decoder, Encoder, InputInjector, MediaSource, Renderer, VideoFrame};
pub use rtc_endpoint::EndpointRole;
pub use signaling_client::SignalClient;
