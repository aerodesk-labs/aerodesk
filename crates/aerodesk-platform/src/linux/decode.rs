//! 解码器：VAAPI 硬解优先，OpenH264 软解回退（共享 aerodesk-codec::softenc）。
//!
//! 硬件路径见 [`crate::linux::vaapi::VaapiDecoder`]（仅 Linux；无 /dev/dri 时回退）。
//! 软解跨平台可用（与软编同思路）。

pub use aerodesk_codec::softenc::decode::SoftDecoder;

/// VAAPI 硬解码器（仅 Linux）。
#[cfg(target_os = "linux")]
pub use crate::linux::vaapi::VaapiDecoder;
