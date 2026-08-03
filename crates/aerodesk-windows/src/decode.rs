//! 解码器：软解回退（OpenH264，共享 aerodesk-softenc）。
//!
//! 硬件路径（后续）：DXVA2 硬解。OpenH264 在任何 Windows 10+ 上都可用，
//! 先打通观看端解码链路。

pub use aerodesk_softenc::decode::SoftDecoder;
