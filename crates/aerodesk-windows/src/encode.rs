//! 编码器：OpenH264 软编（全平台，aerodesk-softenc）。
//!
//! 硬件路径（后续）：Media Foundation `H.264 Encoder MFT` / HEVC MFT，
//! 或 NVENC/QSV/AMF。OpenH264（BSD）在 Windows 10+ 直接可用，先打通被控端编码链路。

pub use aerodesk_softenc::EncodedFrame;
pub use aerodesk_softenc::openh264enc::OpenH264Encoder as SoftEncoder;
