//! 解码器：Windows 观看端优先 DXVA2 硬解，失败回退 OpenH264 软解。
//!
//! DXVA2 路径只在 Windows 编译；非 Windows 主机仍可 re-export OpenH264
//! 软解，保证 workspace 的 host check 与测试可用。

pub use aerodesk_ffmpeg::softenc::decode::SoftDecoder;

#[cfg(windows)]
mod dxva2 {
    use aerodesk_core::media_pipeline::{Codec, EncodedUnit};
    use aerodesk_core::platform::VideoFrame;

    /// Windows 观看端硬件解码器（DXVA2 / D3D11VA，H.264/HEVC → RGBA）。
    ///
    /// 底层复用 `aerodesk_ffmpeg::hw_decode::FfmpegHwDecoder` 的 raw FFI 实现；
    /// 构造失败（无 GPU/驱动）时由 CLI/UI 回退 OpenH264 软解。
    pub struct Dxva2Decoder {
        inner: aerodesk_ffmpeg::hw_decode::FfmpegHwDecoder,
    }

    impl Dxva2Decoder {
        pub fn new() -> Result<Self, String> {
            Ok(Self {
                inner: aerodesk_ffmpeg::hw_decode::FfmpegHwDecoder::new()?,
            })
        }

        /// 当前解码 codec；硬解在嗅探到 SPS 前为 `None`。
        pub fn codec(&self) -> Option<Codec> {
            self.inner.codec()
        }
    }

    impl aerodesk_core::platform::Decoder for Dxva2Decoder {
        type Error = String;

        fn configure(&mut self, codec: Codec, width: u32, height: u32) -> Result<(), Self::Error> {
            aerodesk_core::platform::Decoder::configure(&mut self.inner, codec, width, height)
        }

        fn decode(&mut self, unit: &EncodedUnit) -> Result<Option<VideoFrame>, Self::Error> {
            aerodesk_core::platform::Decoder::decode(&mut self.inner, unit)
        }
    }

    impl std::fmt::Debug for Dxva2Decoder {
        fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
            f.debug_struct("Dxva2Decoder").finish_non_exhaustive()
        }
    }
}

#[cfg(windows)]
pub use dxva2::Dxva2Decoder;
