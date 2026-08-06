//! FFmpeg video decoder (#74): H.264 / H.265 / VP9 / AV1 -> RGBA.
//! Hardware decoder preferred automatically; FFmpeg unified interface.

use ffmpeg_next as ffmpeg;
use ffmpeg_next::codec::packet::Packet;
use ffmpeg_next::format::Pixel;
use ffmpeg_next::frame::Video;
use ffmpeg_next::software::scaling::{Context as ScalingContext, flag::Flags as ScalingFlags};

use aerodesk_core::media_pipeline::{Codec, EncodedUnit, VideoFrame};

fn codec_id(codec: Codec) -> ffmpeg_next::codec::Id {
    match codec {
        Codec::H264 => ffmpeg_next::codec::Id::H264,
        Codec::Hevc => ffmpeg_next::codec::Id::HEVC,
        Codec::Vp9 => ffmpeg_next::codec::Id::VP9,
        Codec::Av1 => ffmpeg_next::codec::Id::AV1,
        other => panic!("ffmpeg decoder unsupported codec: {other:?}"),
    }
}

/// FFmpeg video decoder (packet in -> RGBA frame out).
pub struct FfmpegDecoder {
    decoder: ffmpeg_next::decoder::Video,
    scaler: Option<ScalingContext>,
    width: u32,
    height: u32,
    codec: Codec,
}

impl FfmpegDecoder {
    pub fn new(codec: Codec) -> Result<Self, String> {
        crate::encode::init();
        let id = codec_id(codec);
        let ffmpeg_codec =
            ffmpeg::decoder::find(id).ok_or_else(|| format!("decoder not found: {id:?}"))?;
        // new_with_codec + decoder().video() auto-opens the decoder; SPS/PPS in
        // the first keyframe configure width/height/format.
        let decoder = ffmpeg::codec::context::Context::new_with_codec(ffmpeg_codec)
            .decoder()
            .video()
            .map_err(|e| format!("decoder open: {e}"))?;
        Ok(Self {
            decoder,
            scaler: None,
            width: 0,
            height: 0,
            codec,
        })
    }

    pub fn codec(&self) -> Codec {
        self.codec
    }

    pub fn width(&self) -> u32 {
        self.width
    }

    pub fn height(&self) -> u32 {
        self.height
    }

    /// Decode one encoded unit; None when more input is needed (EAGAIN).
    pub fn decode_unit(&mut self, unit: &EncodedUnit) -> Result<Option<VideoFrame>, String> {
        let mut packet = Packet::new(unit.data.len());
        if let Some(d) = packet.data_mut() {
            d.copy_from_slice(&unit.data);
        }
        self.decoder
            .send_packet(&packet)
            .map_err(|e| format!("send_packet: {e}"))?;
        let mut frame = Video::empty();
        match self.decoder.receive_frame(&mut frame) {
            Ok(()) => {
                let w = frame.width() as usize;
                let h = frame.height() as usize;
                if self.scaler.is_none() {
                    self.scaler = Some(
                        ScalingContext::get(
                            frame.format(),
                            w as u32,
                            h as u32,
                            Pixel::RGBA,
                            w as u32,
                            h as u32,
                            ScalingFlags::BILINEAR,
                        )
                        .map_err(|e| format!("scaler: {e}"))?,
                    );
                }
                let mut rgba = Video::empty();
                self.scaler
                    .as_mut()
                    .unwrap()
                    .run(&frame, &mut rgba)
                    .map_err(|e| format!("scale: {e}"))?;
                let mut raw = vec![0u8; w * h * 4];
                let src = rgba.data(0);
                unsafe {
                    std::ptr::copy_nonoverlapping(src.as_ptr(), raw.as_mut_ptr(), raw.len());
                }
                self.width = w as u32;
                self.height = h as u32;
                Ok(Some(VideoFrame {
                    handle: None,
                    raw: Some(raw),
                    width: w as u32,
                    height: h as u32,
                    pts_ms: 0,
                }))
            }
            Err(e) => match e {
                ffmpeg::Error::Eof => Ok(None),
                ffmpeg::Error::Other { errno } if errno.abs() == 11 => Ok(None), // EAGAIN
                e => Err(format!("receive_frame: {e:?}")),
            },
        }
    }
}

impl std::fmt::Debug for FfmpegDecoder {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("FfmpegDecoder")
            .field("codec", &self.codec)
            .field("width", &self.width)
            .field("height", &self.height)
            .finish()
    }
}
