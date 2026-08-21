//! MediaCodec H.264/HEVC 硬解（骨架）。
//!
//! 实现路径（真机阶段）：
//! 1. JNI 创建 `MediaCodec`，configure(KEY_MIME video/avc 或 video/hevc, Surface)
//! 2. RTP 帧（str0m MediaData，AnnexB）→ `queueInputBuffer`
//! 3. 输出 Surface 直接渲染，或 `getOutputImage` 读回 RGBA
//!
//! 本骨架实现 aerodesk-core 的 [`Decoder`] trait，平台差异收敛在适配器。

use aerodesk_core::platform::Codec;

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

impl aerodesk_core::platform::Decoder for MediaCodecDecoder {
    type Error = String;

    fn configure(&mut self, _codec: Codec, _width: u32, _height: u32) -> Result<(), Self::Error> {
        Err("android: MediaCodec JNI bridge not implemented yet (P3)".into())
    }

    fn decode(
        &mut self,
        _unit: &aerodesk_core::platform::EncodedUnit,
    ) -> Result<Option<aerodesk_core::platform::VideoFrame>, Self::Error> {
        Ok(None)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn core_codec_is_shared() {
        // 平台不再重复定义 Codec：直接使用 core 枚举。
        assert_ne!(Codec::H264, Codec::Hevc);
    }
}
