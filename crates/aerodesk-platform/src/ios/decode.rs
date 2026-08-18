//! VideoToolbox 硬解（H.264/HEVC）：实现已收敛至 `crate::apple::vt_decode`
//! （#506，macOS/iOS 共享单实现——含会话复用与陈旧回调帧排空，原 iOS 拷贝
//! 每个关键帧重建 VT 会话的规律性卡顿随之消除）；re-export 保持调用路径稳定。

pub use crate::apple::vt_decode::{DecoderKind, H264Decoder, HevcDecoder, detect_codec, to_rgba};
