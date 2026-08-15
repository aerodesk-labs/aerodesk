//! 编码器：VAAPI 硬编优先，x264 软编回退（共享 aerodesk-softenc）。
//!
//! 硬件路径见 [`crate::linux::vaapi::VaapiEncoder`]（仅 Linux；无 /dev/dri 时回退）。
//! 输入统一为 core 约定的 BGRA32（`VideoFrame.raw`；X11 采集已输出 BGRA）。
//! 软编（x264 单线程单 slice）已在本仓库 macOS/CLI 链路验证，跨平台可用。

pub use aerodesk_softenc::EncodedFrame;
pub use aerodesk_softenc::encode::X264Encoder as SoftEncoder;

/// VAAPI 硬编码器（仅 Linux）。
#[cfg(target_os = "linux")]
pub use crate::linux::vaapi::VaapiEncoder;
