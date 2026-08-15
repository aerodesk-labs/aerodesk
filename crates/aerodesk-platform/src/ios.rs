//! AeroDesk iOS 适配器：VideoToolbox H.264 硬解。
//!
//! 观看端解码路径：str0m MediaData（AnnexB）→ 本 crate → CVPixelBuffer。
//! 渲染（AVSampleBufferDisplayLayer/Metal）与网络（NWConnection/BSD socket）
//! 由 App 壳层实现（P3 后续）。

pub mod decode;

pub use decode::H264Decoder;
pub mod viewer;
