//! aerodesk-ipc —— desktop ↔ host loopback IPC（#516 / ADR-0009 B2）。
//!
//! 控制面：JSON 消息（[`Envelope`] 版本化信封 + [`Msg`] 消息集）经
//! [`frame`] 的长度前缀帧在命名管道/Unix socket 上传输。帧面（视频 RGBA）
//! 选型基准见 `docs/IPC_FRAME_BENCHMARK.md`，本 crate 不含媒体数据面。
//!
//! 设计约束（ADR-0008/0009）：std::thread 同步模型，不引入 async 运行时；
//! 依赖保持轻量（不向 desktop/host 拖入 codec/platform）。

pub mod frame;
pub mod proto;

pub use frame::{MAX_FRAME, read_frame, write_frame};
pub use proto::{Envelope, Msg, PROTOCOL_VERSION};
