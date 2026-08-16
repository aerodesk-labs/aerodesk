//! CLI 观看端视频解码器（#3/#8）：Windows 优先 DXVA2 硬解，
//! 失败回退 OpenH264 软解；其余平台继续走 FFmpeg 软解。
//!
//! DXVA2 支持 H.264/HEVC；VP9/AV1 在 Windows 上暂不回退（CLI 观看路径
//! 默认协商 H.264/HEVC，VP9/AV1 仍由非 Windows FFmpeg 软解覆盖）。

use aerodesk_core::media_pipeline::{Codec, EncodedUnit, VideoFrame};

#[cfg(windows)]
use aerodesk_platform::windows::decode::Dxva2Decoder;
#[cfg(windows)]
use aerodesk_softenc::decode::SoftDecoder;

pub enum CliVideoDecoder {
    #[cfg(not(windows))]
    Soft(aerodesk_ffmpeg::decode::FfmpegDecoder),
    #[cfg(windows)]
    Dxva2(Dxva2Decoder),
    #[cfg(windows)]
    Soft { inner: SoftDecoder, codec: Codec },
}

impl CliVideoDecoder {
    pub fn new(codec: Codec) -> Result<Self, String> {
        #[cfg(windows)]
        {
            if std::env::var_os("AERODESK_FORCE_SOFT_DECODE").is_none()
                && matches!(codec, Codec::H264 | Codec::Hevc)
            {
                match Dxva2Decoder::new() {
                    Ok(hw) => {
                        tracing::info!("cli viewer: DXVA2 硬解启用（{codec:?}）");
                        return Ok(Self::Dxva2(hw));
                    }
                    Err(e) => {
                        tracing::warn!("cli viewer: DXVA2 不可用（{e}），回退 OpenH264 软解");
                    }
                }
            }

            if codec != Codec::H264 {
                return Err(format!(
                    "OpenH264 回退仅支持 H.264，当前 codec 为 {codec:?}"
                ));
            }
            let inner = SoftDecoder::new()?;
            return Ok(Self::Soft { inner, codec });
        }

        #[cfg(not(windows))]
        {
            Ok(Self::Soft(aerodesk_ffmpeg::decode::FfmpegDecoder::new(
                codec,
            )?))
        }
    }

    /// 当前解码 codec（软解恒有；硬解在嗅探到 SPS 前为 None）。
    pub fn codec(&self) -> Option<Codec> {
        match self {
            #[cfg(not(windows))]
            Self::Soft(d) => Some(d.codec()),
            #[cfg(windows)]
            Self::Dxva2(d) => d.codec(),
            #[cfg(windows)]
            Self::Soft { codec, .. } => Some(*codec),
        }
    }

    pub fn decode(&mut self, unit: &EncodedUnit) -> Result<Option<VideoFrame>, String> {
        match self {
            #[cfg(not(windows))]
            Self::Soft(d) => d.decode_unit(unit),
            #[cfg(windows)]
            Self::Dxva2(d) => aerodesk_core::platform::Decoder::decode(d, unit),
            #[cfg(windows)]
            Self::Soft { inner, .. } => aerodesk_core::platform::Decoder::decode(inner, unit),
        }
    }
}
