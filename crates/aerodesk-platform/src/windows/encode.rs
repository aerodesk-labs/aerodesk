//! 编码器：OpenH264 软编 re-export（共享 `aerodesk-codec::softenc`）。
//!
//! Windows 被控端硬编走 `aerodesk-codec` 的 `FfmpegEncoder`（h264_mf 优先、
//! 一帧探测回退，调用方见 cli `publisher_capture_windows` 与 desktop
//! `generic_publisher`）；本模块只保留软编 re-export 作为无 FFmpeg 环境的兜底。
//!
//! 历史：手写 Media Foundation H.264 编码器（`MfH264Encoder`）已随 #506 删除——
//! 与 `FfmpegEncoder` 的 h264_mf 路径同源重复且全仓零调用方。

pub use aerodesk_codec::softenc::EncodedFrame;
pub use aerodesk_codec::softenc::openh264enc::OpenH264Encoder as SoftEncoder;
