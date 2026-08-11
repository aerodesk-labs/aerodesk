//! 解码器：VAAPI 硬解优先，OpenH264 软解回退（共享 aerodesk-softenc）。
//!
//! 硬件路径见 [`crate::vaapi::VaapiDecoder`]（仅 Linux；无 /dev/dri 时回退）。
//! 软解跨平台可用（与软编同思路）。

pub use aerodesk_softenc::decode::SoftDecoder;

/// VAAPI 硬解码器（仅 Linux）。
#[cfg(target_os = "linux")]
pub use crate::vaapi::VaapiDecoder;
