//! 编码器：软编回退（x264，共享 aerodesk-softenc）。
//!
//! 硬件路径（后续）：VAAPI H.264/AV1。软编（x264 单线程单 slice）已在本仓库
//! macOS/CLI 链路验证，跨平台可用。

pub use aerodesk_softenc::encode::{EncodedFrame, X264Encoder as SoftEncoder};
pub use aerodesk_softenc::rgba_to_rgb;
