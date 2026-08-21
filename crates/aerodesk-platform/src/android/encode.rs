//! MediaCodec 硬编（骨架）：实现 core `Encoder` trait，JNI 真机阶段补齐。

use aerodesk_core::platform::Codec;

/// TODO(P3)：JNI 桥接到 android.media.MediaCodec 硬编。
/// 前置条件：Android SDK/NDK + `aarch64-linux-android`/`armv7-linux-androideabi` target。
pub struct MediaCodecEncoder {
    width: u32,
    height: u32,
    codec: Codec,
}

impl MediaCodecEncoder {
    pub fn new(_width: u32, _height: u32, _codec: Codec) -> Result<Self, String> {
        Err("android: MediaCodec encoder JNI bridge not implemented yet (P3)".into())
    }
}

impl aerodesk_core::platform::Encoder for MediaCodecEncoder {
    type Error = String;

    fn configure(
        &mut self,
        codec: Codec,
        width: u32,
        height: u32,
        _fps: u32,
    ) -> Result<(), Self::Error> {
        self.codec = codec;
        self.width = width;
        self.height = height;
        Err("android: MediaCodec encoder JNI bridge not implemented yet (P3)".into())
    }

    fn encode(
        &mut self,
        _frame: &aerodesk_core::platform::VideoFrame,
    ) -> Result<Option<aerodesk_core::platform::EncodedUnit>, Self::Error> {
        Err("android: MediaCodec encoder JNI bridge not implemented yet (P3)".into())
    }

    fn request_keyframe(&mut self) {}

    fn set_bitrate(&mut self, _bitrate_bps: u64, _fps: u32) {}
}
