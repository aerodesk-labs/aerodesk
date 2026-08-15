//! H.264 软编转发：实现移入共享 crate `aerodesk-softenc::encode`（#3/#4 各平台复用）。

pub use aerodesk_softenc::EncodedFrame;
pub use aerodesk_softenc::encode::{X264Encoder, rgb_to_i420};
