//! 解码器：软解回退（OpenH264，共享 aerodesk-softenc）。
//!
//! 硬件路径（后续）：VAAPI 硬解。软解跨平台可用（与软编同思路）。

pub use aerodesk_softenc::decode::SoftDecoder;
