//! CLI 观看端视频解码器（#3/#8）：Windows 优先 D3D11VA/DXVA2 硬解，
//! 其余平台/编解码回退 FFmpeg 软解。
//!
//! 硬解设备创建失败（无 GPU/驱动）时回退软解；VP9/AV1 仅软解。

use aerodesk_core::media_pipeline::{Codec, EncodedUnit, VideoFrame};

pub enum CliVideoDecoder {
    Soft(aerodesk_ffmpeg::decode::FfmpegDecoder),
    #[cfg(windows)]
    Hw(aerodesk_ffmpeg::hw_decode::FfmpegHwDecoder),
}

impl CliVideoDecoder {
    pub fn new(codec: Codec) -> Result<Self, String> {
        #[cfg(windows)]
        if std::env::var_os("AERODESK_FORCE_SOFT_DECODE").is_none()
            && matches!(codec, Codec::H264 | Codec::Hevc)
        {
            match aerodesk_ffmpeg::hw_decode::FfmpegHwDecoder::new() {
                Ok(hw) => {
                    tracing::info!("cli viewer: D3D11VA/DXVA2 硬解启用（{codec:?}）");
                    return Ok(Self::Hw(hw));
                }
                Err(e) => {
                    tracing::warn!("cli viewer: 硬件解码不可用（{e}），回退 FFmpeg 软解");
                }
            }
        }
        Ok(Self::Soft(aerodesk_ffmpeg::decode::FfmpegDecoder::new(
            codec,
        )?))
    }

    /// 当前解码 codec（软解恒有；硬解在嗅探到 SPS 前为 None）。
    pub fn codec(&self) -> Option<Codec> {
        match self {
            Self::Soft(d) => Some(d.codec()),
            #[cfg(windows)]
            Self::Hw(d) => d.codec(),
        }
    }

    pub fn decode(&mut self, unit: &EncodedUnit) -> Result<Option<VideoFrame>, String> {
        match self {
            Self::Soft(d) => d.decode_unit(unit),
            #[cfg(windows)]
            Self::Hw(d) => aerodesk_core::platform::Decoder::decode(d, unit),
        }
    }
}
