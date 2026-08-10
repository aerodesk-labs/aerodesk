//! FFmpeg 视频编码器（#74）：H.264 / H.265 / VP9 / AV1。
//! 硬编优先（macOS VideoToolbox H.264/HEVC），否则 FFmpeg 软编回退。

use std::collections::VecDeque;

use ffmpeg_next as ffmpeg;
use ffmpeg_next::codec::packet::Packet;
use ffmpeg_next::format::Pixel;
use ffmpeg_next::frame::Video;
use ffmpeg_next::software::scaling::{Context as ScalingContext, flag::Flags as ScalingFlags};

use aerodesk_core::media_pipeline::{Codec, EncodedUnit};

/// FFmpeg 全局初始化（进程一次）。
pub(crate) fn init() {
    use std::sync::Once;
    static INIT: Once = Once::new();
    INIT.call_once(|| {
        let _ = ffmpeg::init();
    });
}

/// 编码器名（硬编优先，软编回退）。
fn encoder_names(codec: Codec) -> (&'static [&'static str], ffmpeg_next::codec::Id) {
    match codec {
        Codec::H264 => (
            &["h264_videotoolbox", "libx264"],
            ffmpeg_next::codec::Id::H264,
        ),
        Codec::Hevc => (
            &["hevc_videotoolbox", "libx265"],
            ffmpeg_next::codec::Id::HEVC,
        ),
        Codec::Vp9 => (&["libvpx-vp9"], ffmpeg_next::codec::Id::VP9),
        Codec::Av1 => (&["libsvtav1"], ffmpeg_next::codec::Id::AV1),
        other => panic!("ffmpeg encoder unsupported codec: {other:?}"),
    }
}

/// FFmpeg 视频编码器（RGB24 输入 → 编码包）。
pub struct FfmpegEncoder {
    encoder: ffmpeg_next::encoder::Video,
    scaler: ScalingContext,
    /// BGRA → YUV420P（屏幕采集 IOSurface 输入，见 encode_bgra）。
    bgra_scaler: ScalingContext,
    width: u32,
    height: u32,
    fps: u32,
    pts: i64,
    keyframe_pending: bool,
    ffmpeg_id: ffmpeg_next::codec::Id,
    /// 编码器缓冲产出（AV1/SVT 等有内部延迟，先入队再按序返回）。
    pending: VecDeque<EncodedUnit>,
}

impl FfmpegEncoder {
    pub fn new(
        width: u32,
        height: u32,
        fps: u32,
        bitrate_bps: u64,
        codec: Codec,
    ) -> Result<Self, String> {
        init();
        let (names, id) = encoder_names(codec);
        let mut last_err = String::new();
        for name in names {
            match Self::open_named(name, id, width, height, fps, bitrate_bps) {
                Ok(enc) => {
                    tracing::info!(
                        "ffmpeg encoder opened: {name} ({}x{}@{} {bitrate_bps}bps)",
                        width,
                        height,
                        fps
                    );
                    return Ok(enc);
                }
                Err(e) => {
                    last_err = format!("{name}: {e}");
                    tracing::warn!("ffmpeg encoder {name} failed: {e}");
                }
            }
        }
        Err(format!("no encoder available for {codec:?}: {last_err}"))
    }

    fn open_named(
        name: &str,
        id: ffmpeg_next::codec::Id,
        width: u32,
        height: u32,
        fps: u32,
        bitrate_bps: u64,
    ) -> Result<Self, ffmpeg::Error> {
        let codec = ffmpeg::encoder::find_by_name(name).ok_or(ffmpeg::Error::EncoderNotFound)?;
        let mut ctx = ffmpeg::codec::context::Context::new_with_codec(codec)
            .encoder()
            .video()?;
        ctx.set_width(width);
        ctx.set_height(height);
        ctx.set_format(Pixel::YUV420P);
        ctx.set_time_base(ffmpeg::Rational(1, fps as i32));
        ctx.set_frame_rate(Some(ffmpeg::Rational(fps as i32, 1)));
        ctx.set_bit_rate(bitrate_bps as usize);
        let mut dict = ffmpeg::Dictionary::new();
        if id == ffmpeg::codec::Id::VP9 {
            // libvpx-vp9 默认 lookahead 会吞帧：关掉 lag-in-frames 才能逐帧输出。
            dict.set("deadline", "realtime");
            dict.set("cpu-used", "4");
            dict.set("lag-in-frames", "0");
        } else if id == ffmpeg::codec::Id::AV1 {
            // SVT-AV1：lookahead=0 降低编码延迟；仍有少量内部缓冲，
            // 由 pending 队列吸收（见 encode_rgb）。
            dict.set("preset", "8");
            dict.set("svtav1-params", "lookahead=0");
        } else {
            dict.set("preset", "veryfast");
        }
        let encoder = ctx.open_with(dict)?;
        let scaler = ScalingContext::get(
            Pixel::RGB24,
            width,
            height,
            Pixel::YUV420P,
            width,
            height,
            ScalingFlags::BILINEAR,
        )?;
        let bgra_scaler = ScalingContext::get(
            Pixel::BGRA,
            width,
            height,
            Pixel::YUV420P,
            width,
            height,
            ScalingFlags::BILINEAR,
        )?;
        Ok(Self {
            encoder,
            scaler,
            bgra_scaler,
            width,
            height,
            fps,
            pts: 0,
            keyframe_pending: false,
            ffmpeg_id: id,
            pending: VecDeque::new(),
        })
    }

    /// 编码一帧 RGB24，返回编码包（AnnexB/OBU，str0m 按 codec 分包）。
    pub fn encode_rgb(&mut self, rgb: &[u8]) -> Result<Option<EncodedUnit>, String> {
        if rgb.len() != (self.width * self.height * 3) as usize {
            return Err("rgb size mismatch".into());
        }
        let mut src = Video::new(Pixel::RGB24, self.width, self.height);
        let dst = src.data_mut(0);
        unsafe {
            std::ptr::copy_nonoverlapping(rgb.as_ptr(), dst.as_mut_ptr(), rgb.len());
        }
        src.set_pts(Some(self.pts));
        if self.keyframe_pending {
            src.set_kind(ffmpeg::picture::Type::I);
            self.keyframe_pending = false;
        }
        let mut yuv = Video::empty();
        self.scaler
            .run(&src, &mut yuv)
            .map_err(|e| format!("scale: {e}"))?;
        yuv.set_pts(src.pts());
        self.encoder
            .send_frame(&yuv)
            .map_err(|e| format!("send_frame: {e}"))?;
        self.pts += 1;

        // 排空所有已产出包（编码器延迟帧会在此补出），按序返回。
        let mut packet = Packet::empty();
        while let Ok(()) = self.encoder.receive_packet(&mut packet) {
            let pts_ms = (self.pts * 1000 / self.fps.max(1) as i64) as u64;
            self.pending.push_back(EncodedUnit {
                data: packet.data().unwrap_or(&[]).to_vec(),
                keyframe: packet.is_key(),
                pts_ms,
                rtp_timestamp: (self.pts * 90_000 / self.fps.max(1) as i64) as u32,
            });
        }
        Ok(self.pending.pop_front())
    }

    /// 编码一帧 BGRA32（屏幕采集 IOSurface 读取结果），返回编码包。
    pub fn encode_bgra(&mut self, bgra: &[u8]) -> Result<Option<EncodedUnit>, String> {
        if bgra.len() != (self.width * self.height * 4) as usize {
            return Err("bgra size mismatch".into());
        }
        let mut src = Video::new(Pixel::BGRA, self.width, self.height);
        unsafe {
            std::ptr::copy_nonoverlapping(bgra.as_ptr(), src.data_mut(0).as_mut_ptr(), bgra.len());
        }
        src.set_pts(Some(self.pts));
        if self.keyframe_pending {
            src.set_kind(ffmpeg::picture::Type::I);
            self.keyframe_pending = false;
        }
        let mut yuv = Video::empty();
        self.bgra_scaler
            .run(&src, &mut yuv)
            .map_err(|e| format!("scale: {e}"))?;
        yuv.set_pts(src.pts());
        self.encoder
            .send_frame(&yuv)
            .map_err(|e| format!("send_frame: {e}"))?;
        self.pts += 1;
        let mut packet = Packet::empty();
        while let Ok(()) = self.encoder.receive_packet(&mut packet) {
            let pts_ms = (self.pts * 1000 / self.fps.max(1) as i64) as u64;
            self.pending.push_back(EncodedUnit {
                data: packet.data().unwrap_or(&[]).to_vec(),
                keyframe: packet.is_key(),
                pts_ms,
                rtp_timestamp: (self.pts * 90_000 / self.fps.max(1) as i64) as u32,
            });
        }
        Ok(self.pending.pop_front())
    }

    /// 请求关键帧（下一帧设为 I 帧）。
    pub fn request_keyframe(&mut self) {
        self.keyframe_pending = true;
    }

    pub fn codec(&self) -> Codec {
        match self.ffmpeg_id {
            ffmpeg::codec::Id::H264 => Codec::H264,
            ffmpeg::codec::Id::HEVC => Codec::Hevc,
            ffmpeg::codec::Id::VP9 => Codec::Vp9,
            ffmpeg::codec::Id::AV1 => Codec::Av1,
            _ => Codec::H264,
        }
    }
}

impl Drop for FfmpegEncoder {
    fn drop(&mut self) {
        // 让编码器正常收尾（SVT-AV1 会在未 EOS 时打印告警）。
        let _ = self.encoder.send_eof();
    }
}

impl std::fmt::Debug for FfmpegEncoder {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("FfmpegEncoder")
            .field("width", &self.width)
            .field("height", &self.height)
            .field("fps", &self.fps)
            .finish()
    }
}
