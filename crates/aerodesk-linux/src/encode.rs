//! 编码器：VAAPI 硬编优先，x264 软编回退（共享 aerodesk-softenc）。
//!
//! 硬件路径见 [`crate::vaapi::VaapiEncoder`]（仅 Linux；无 /dev/dri 时回退）。
//! 软编（x264 单线程单 slice）已在本仓库 macOS/CLI 链路验证，跨平台可用。

pub use aerodesk_softenc::EncodedFrame;
pub use aerodesk_softenc::encode::X264Encoder as SoftEncoder;
pub use aerodesk_softenc::rgba_to_rgb;

/// VAAPI 硬编码器（仅 Linux）。
#[cfg(target_os = "linux")]
pub use crate::vaapi::VaapiEncoder;
