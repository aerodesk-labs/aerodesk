//! MediaCodec H.264/HEVC 硬解（骨架）。
//!
//! 实现路径（真机阶段）：
//! 1. JNI 创建 `MediaCodec`，configure(KEY_MIME video/avc 或 video/hevc, Surface)
//! 2. RTP 帧（str0m MediaData，AnnexB）→ `queueInputBuffer`
//! 3. 输出 Surface 直接渲染，或 `getOutputImage` 读回 RGBA
//!
//! 本骨架暴露 trait，供 aerodesk-core 观看端管线调用。

use crate::DecodedFrame;

/// 解码器抽象：Core 只依赖该 trait，平台差异收敛在适配器。
pub trait H264Decoder {
    /// 解码一帧 AnnexB H.264。
    fn decode_annexb(&mut self, data: &[u8], pts_us: i64) -> Option<DecodedFrame>;
}

/// TODO(P3): JNI 桥接到 android.media.MediaCodec。
/// 前置条件：Android SDK/NDK + `aarch64-linux-android`/`armv7-linux-androideabi` target。
pub struct MediaCodecDecoder {
    /// JNI 全局引用（MediaCodec 实例）——真机实现时填充。
    _handle: *mut std::ffi::c_void,
}

impl MediaCodecDecoder {
    pub fn new(width: u32, height: u32, codec: Codec) -> Result<Self, String> {
        // TODO: JNI NewObject(MediaCodec) + configure + start
        let _ = (width, height, codec);
        Err("android: MediaCodec JNI bridge not implemented yet (P3)".into())
    }
}

/// 编码格式。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Codec {
    H264,
    Hevc,
}

impl H264Decoder for MediaCodecDecoder {
    fn decode_annexb(&mut self, data: &[u8], pts_us: i64) -> Option<DecodedFrame> {
        let _ = (data, pts_us);
        None
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn codec_enum_is_stable() {
        assert_ne!(Codec::H264, Codec::Hevc);
    }
}
