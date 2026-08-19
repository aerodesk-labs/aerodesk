//! aerodesk-ipc —— desktop ↔ host loopback IPC（#516 / ADR-0009 B2）。
//!
//! 控制面：JSON 消息（[`Envelope`] 版本化信封 + [`Msg`] 消息集）经
//! [`frame`] 的长度前缀帧在命名管道/Unix socket（[`transport`]）上传输。
//! 帧面（视频 RGBA）：F1 共享内存环形通道（[`framebuf`]，Windows 先行），
//! 选型基准见 `docs/IPC_FRAME_BENCHMARK.md`。
//!
//! 设计约束（ADR-0008/0009）：std::thread 同步模型，不引入 async 运行时；
//! 依赖保持轻量（不向 desktop/host 拖入 codec/platform）。

pub mod frame;
#[cfg(windows)]
pub mod framebuf;
pub mod proto;
pub mod transport;

pub use frame::{MAX_FRAME, read_frame, write_frame};
#[cfg(windows)]
pub use framebuf::{FrameMeta, FrameRingReader, FrameRingWriter};
pub use proto::{Envelope, Msg, PROTOCOL_VERSION};
pub use transport::{Conn, ConnWriter, HandshakeError, Listener, RecvError};
