//! FFmpeg 视频编码器（#74）：H.264 / H.265 / VP9 / AV1。
//! 硬编优先（macOS VideoToolbox H.264/HEVC），否则 FFmpeg 软编回退。

use std::collections::VecDeque;
use std::time::{Duration, Instant};

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
        // #3 Windows MF 硬件编码：FFmpeg 的 h264_mf/hevc_mf 封装 Media Foundation
        // H.264/HEVC Encoder MFT（Win10+ 自带）；不存在的编码器 find_by_name 返回
        // None 自动回退：macOS→videotoolbox、Windows→mf、其他→libx264/libx265。
        Codec::H264 => (
            &["h264_videotoolbox", "h264_mf", "libx264"],
            ffmpeg_next::codec::Id::H264,
        ),
        Codec::Hevc => (
            &["hevc_videotoolbox", "hevc_mf", "libx265"],
            ffmpeg_next::codec::Id::HEVC,
        ),
        Codec::Vp9 => (&["libvpx-vp9"], ffmpeg_next::codec::Id::VP9),
        Codec::Av1 => (&["libsvtav1"], ffmpeg_next::codec::Id::AV1),
        other => panic!("ffmpeg encoder unsupported codec: {other:?}"),
    }
}

/// 切分 AnnexB NAL，返回 (类型, 含起始码的 NAL)。
fn split_annexb(codec_id: ffmpeg_next::codec::Id, data: &[u8]) -> Vec<(u8, Vec<u8>)> {
    let mut out = Vec::new();
    let mut i = 0;
    while i + 3 <= data.len() {
        if data[i] == 0 && data[i + 1] == 0 && data[i + 2] == 1 {
            let start = i + 3;
            let mut j = start;
            while j + 3 <= data.len() {
                if data[j] == 0 && data[j + 1] == 0 && data[j + 2] == 1 {
                    break;
                }
                j += 1;
            }
            if j + 3 > data.len() {
                j = data.len();
            }
            if j > start {
                let nal = &data[start - 3..j];
                let ty = match codec_id {
                    ffmpeg_next::codec::Id::HEVC => (nal[3] >> 1) & 0x3F,
                    _ => nal[3] & 0x1F,
                };
                out.push((ty, nal.to_vec()));
            }
            i = j;
        } else {
            i += 1;
        }
    }
    out
}

/// 是否参数集 NAL（H264: SPS/PPS；HEVC: VPS/SPS/PPS）。
fn is_param_set_nal(codec_id: ffmpeg_next::codec::Id, ty: u8) -> bool {
    match codec_id {
        ffmpeg_next::codec::Id::HEVC => matches!(ty, 32 | 33 | 34),
        _ => matches!(ty, 7 | 8),
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
    /// #267：码率自适应节流状态（变化 >20% 且 ≥1s 才重建）。
    last_bitrate_bps: u64,
    last_bitrate_at: Instant,
    /// 首个关键帧提取的参数集（H264 SPS/PPS / HEVC VPS/SPS/PPS，AnnexB 含起始码）。
    /// hevc_videotoolbox 等硬编不重复参数集，后续关键帧需要前置才能让晚加入
    /// viewer（iOS VideoToolbox 硬解）建出 format description。
    param_sets: Vec<Vec<u8>>,
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
        // #3 晚加入 viewer：2 秒 GOP（自然 IDR 间隔），配合 repeat-headers
        // 保证 viewer 最迟 2s 拿到带 SPS/PPS 的关键帧可解码（libx264 默认
        // keyint=250 ≈ 8s@30fps 太久；pict_type=I 的 PLI 强制在某些版本无效）。
        ctx.set_gop(fps.saturating_mul(2).max(1));
        let mut dict = ffmpeg::Dictionary::new();
        if id == ffmpeg::codec::Id::VP9 {
            // libvpx-vp9 默认 lookahead 会吞帧：关掉 lag-in-frames 才能逐帧输出。
            dict.set("deadline", "realtime");
            dict.set("cpu-used", "4");
            dict.set("lag-in-frames", "0");
        } else if id == ffmpeg::codec::Id::AV1 {
            // SVT-AV1：仅设 preset=8。不要传 `svtav1-params: lookahead=0`——
            // SVT 会忽略它并强制 lookahead=25（日志可见），且在部分 CPU 核数
            // （如 4 核 runner）下会死锁导致编码永不返回（main CI 挂起根因，
            // `decode::tests::av1_roundtrip` 卡 >60s）。默认 lookahead 下
            // 编码器仍有少量内部缓冲，由 pending 队列吸收（见 encode_rgb）。
            dict.set("preset", "8");
        } else if id == ffmpeg::codec::Id::H264 {
            dict.set("preset", "veryfast");
            // #3 晚加入 viewer 解码修复：libx264 默认 repeat-headers=0，
            // SPS/PPS 仅在流首 IDR 内联一次；viewer 错过首关键帧后收不到
            // 参数集（non-existing PPS 0 referenced），无法解码。每个 IDR
            // 重复 SPS/PPS 保证任意时刻加入都能解码。
            // 远程桌面低延迟：tune=zerolatency（threads=1/bframes=0/no-lookahead）
            // 消除 libx264 多线程帧缓冲延迟（实测首帧延迟 ~20 帧），PLI 后
            // 重建编码器下一帧即出 IDR。repeat-headers 保证每个 IDR 带 SPS/PPS。
            dict.set("tune", "zerolatency");
            dict.set("x264-params", "repeat-headers=1");
        } else if id == ffmpeg::codec::Id::HEVC {
            dict.set("preset", "veryfast");
            dict.set("x265-params", "repeat-headers=1");
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
            last_bitrate_bps: 0,
            last_bitrate_at: Instant::now(),
            param_sets: Vec::new(),
        })
    }

    /// 关键帧参数集注入：缓存首个关键帧的参数集，后续关键帧缺参数集时前置。
    /// hevc_videotoolbox/h264_videotoolbox 不重复参数集（repeat-headers 仅软编
    /// 生效），晚加入的 viewer（尤其 iOS VideoToolbox 硬解，只在看到参数集时
    /// 建 format description）需要每个关键帧自包含才能解码。
    fn keyframe_data(&mut self, packet: &ffmpeg_next::codec::packet::Packet) -> Vec<u8> {
        let raw = packet.data().unwrap_or(&[]);
        if !packet.is_key() {
            return raw.to_vec();
        }
        let nals = split_annexb(self.ffmpeg_id, raw);
        let has_param = nals
            .iter()
            .any(|(ty, _)| is_param_set_nal(self.ffmpeg_id, *ty));
        if self.param_sets.is_empty() && has_param {
            self.param_sets = nals
                .into_iter()
                .filter(|(ty, _)| is_param_set_nal(self.ffmpeg_id, *ty))
                .map(|(_, nal)| nal)
                .collect();
            return raw.to_vec();
        }
        if !self.param_sets.is_empty() && !has_param {
            let mut out = Vec::with_capacity(
                raw.len() + self.param_sets.iter().map(|n| n.len()).sum::<usize>(),
            );
            for n in &self.param_sets {
                out.extend_from_slice(n);
            }
            out.extend_from_slice(raw);
            return out;
        }
        raw.to_vec()
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
            let data = self.keyframe_data(&packet);
            self.pending.push_back(EncodedUnit {
                data,
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
            let data = self.keyframe_data(&packet);
            self.pending.push_back(EncodedUnit {
                data,
                keyframe: packet.is_key(),
                pts_ms,
                rtp_timestamp: (self.pts * 90_000 / self.fps.max(1) as i64) as u32,
            });
        }
        Ok(self.pending.pop_front())
    }

    /// 请求关键帧（下一帧设为 I 帧）。
    pub fn request_keyframe(&mut self) {
        // #3：libx264 对 frame->pict_type=I 的 PLI 提示在 FFmpeg 8.1 实测无效
        // （仍输出 P 帧）；重建编码器是最可靠强制 IDR 的方式（新 encoder 首帧
        // 必为 IDR，且 repeat-headers 带 SPS/PPS）。与 #267 set_bitrate 重建同模式。
        let codec = self.codec();
        // 未调过 set_bitrate 时 last_bitrate_bps 为 0（open 参数未留存）；
        // 用与 open 相同的默认码率兜底（1.5 Mbps 参考值，见 FfmpegEncoder::new 调用方）。
        let bps = self.last_bitrate_bps.max(1_500_000);
        let (w, h, fps) = (self.width, self.height, self.fps);
        match FfmpegEncoder::new(w, h, fps, bps, codec) {
            Ok(enc) => *self = enc,
            Err(e) => tracing::warn!("ffmpeg keyframe rebuild failed: {e}"),
        }
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
        // 必须排空到 EOS：只 send_eof 不 drain 时 SVT-AV1 内部线程不释放，
        // 同进程下一个 SVT 实例会死锁（main CI `av1_roundtrip` 挂起根因——
        // loopback_all_codecs 的 AV1 实例 drop 后，av1_roundtrip 的新实例卡死）。
        let _ = self.encoder.send_eof();
        let mut packet = Packet::empty();
        while let Ok(()) = self.encoder.receive_packet(&mut packet) {}
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

/// 核心 `Encoder` 实现（软编路径：VideoFrame.raw BGRA → 编码包）。
impl aerodesk_core::platform::Encoder for FfmpegEncoder {
    type Error = String;

    fn configure(
        &mut self,
        codec: Codec,
        width: u32,
        height: u32,
        fps: u32,
    ) -> Result<(), Self::Error> {
        // 重建编码器（FFmpeg 软编参数不可原地变更；默认 1.5Mbps 与合成源一致）。
        *self = FfmpegEncoder::new(width, height, fps, 1_500_000, codec)?;
        Ok(())
    }

    fn encode(
        &mut self,
        frame: &aerodesk_core::platform::VideoFrame,
    ) -> Result<Option<aerodesk_core::media_pipeline::EncodedUnit>, Self::Error> {
        let Some(raw) = &frame.raw else {
            return Err("ffmpeg encoder requires raw BGRA frame".into());
        };
        self.encode_bgra(raw).map_err(|e| e.to_string())
    }

    fn request_keyframe(&mut self) {
        self.request_keyframe();
    }

    fn set_bitrate(&mut self, bitrate_bps: u64, _fps: u32) {
        // #267：FFmpeg 码率在 open 时固定，改码率需重建（代价高）。
        // 节流：变化 >20% 且距上次 ≥1s 才重建，避免 BWE 抖动风暴打爆编码器。
        let now = Instant::now();
        let changed = self.last_bitrate_bps == 0
            || bitrate_bps.abs_diff(self.last_bitrate_bps) > self.last_bitrate_bps / 5;
        // 首条反馈（last_bitrate_bps==0）不受 1s 节流限制，否则 BWE 只降一次档时永远不生效。
        if changed
            && (self.last_bitrate_bps == 0
                || now.duration_since(self.last_bitrate_at) >= Duration::from_secs(1))
        {
            let codec = self.codec();
            let (w, h, fps) = (self.width, self.height, self.fps);
            self.last_bitrate_bps = bitrate_bps;
            self.last_bitrate_at = now;
            if let Ok(mut enc) = FfmpegEncoder::new(w, h, fps, bitrate_bps, codec) {
                // 重建会替换 self，必须把节流状态带到新编码器：新编码器
                // last_bitrate_bps=0 会把下一条反馈当首条免节流（#303），
                // BWE 抖动时 1s 内多次重建打爆编码器（实测 0.2s 内连建两次）。
                enc.last_bitrate_bps = bitrate_bps;
                enc.last_bitrate_at = now;
                *self = enc;
            } else {
                tracing::warn!("ffmpeg set_bitrate rebuild failed");
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// #3 PLI 立即响应：request_keyframe 后下一帧必须为关键帧。
    /// （libx264 对 pict_type=I 提示无效；通过重建编码器强制 IDR。）
    #[test]
    fn request_keyframe_yields_immediate_idr() {
        let mut enc =
            FfmpegEncoder::open_named("libx264", ffmpeg::codec::Id::H264, 320, 180, 30, 500_000)
                .expect("libx264 encoder");
        let rgb = vec![128u8; 320 * 180 * 3];
        // 出首帧关键帧（zerolatency 下第 0 帧即 IDR）。
        for _ in 0..30 {
            if let Some(unit) = enc.encode_rgb(&rgb).expect("encode") {
                if unit.keyframe {
                    break;
                }
            }
        }
        enc.request_keyframe();
        let mut got_key = false;
        for _ in 0..5 {
            if let Some(unit) = enc.encode_rgb(&rgb).expect("encode") {
                if unit.keyframe {
                    got_key = true;
                    break;
                }
            }
        }
        assert!(got_key, "PLI 后 5 帧内应产出关键帧");
    }

    /// #3 晚加入 viewer 解码回归：**后续**关键帧（PLI 后）必须内联 SPS/PPS。
    /// libx264 默认 repeat-headers=0 时仅首 IDR 带参数集；viewer 错过首帧后
    /// 请求关键帧得到的 IDR 不含 SPS/PPS（non-existing PPS 0 referenced），
    /// 无法解码。repeat-headers=1 保证每个 IDR 都带参数集。
    #[test]
    fn h264_second_keyframe_contains_sps_pps() {
        // 强制 libx264 软编（macOS 默认硬编 h264_videotoolbox 输出格式不同，
        // 本测试验证软编路径的参数集行为；hardware 路径由 codec e2e 覆盖）。
        let mut enc =
            FfmpegEncoder::open_named("libx264", ffmpeg::codec::Id::H264, 320, 180, 30, 500_000)
                .expect("libx264 encoder");
        let rgb = vec![128u8; 320 * 180 * 3];
        // 晚加入 viewer：等待自然 GOP 的第二个关键帧（2s GOP = 60 帧@30fps），
        // 必须内联 SPS/PPS（repeat-headers=1），否则 viewer 仍无法解码。
        let mut second: Option<Vec<u8>> = None;
        let mut keyframes = 0;
        for _ in 0..(30 * 4) {
            if let Some(unit) = enc.encode_rgb(&rgb).expect("encode") {
                if unit.keyframe {
                    keyframes += 1;
                    if keyframes == 2 {
                        second = Some(unit.data);
                        break;
                    }
                }
            }
        }
        let data = second.expect("2s GOP 内应产出第二个关键帧");
        assert!(
            data.windows(5).any(|w| w == [0, 0, 0, 1, 0x67]),
            "SPS NAL missing in 2nd keyframe (repeat-headers?): {} bytes",
            data.len()
        );
        assert!(
            data.windows(5).any(|w| w == [0, 0, 0, 1, 0x68]),
            "PPS NAL missing in 2nd keyframe (repeat-headers?): {} bytes",
            data.len()
        );
    }

    #[test]
    fn hevc_keyframes_carry_parameter_sets() {
        // hevc_videotoolbox 不重复参数集；keyframe_data 需在后续关键帧前置
        // VPS/SPS/PPS（晚加入 viewer/iOS 硬解才能建 format description）。
        let mut enc = FfmpegEncoder::new(320, 180, 30, 1_000_000, Codec::Hevc).expect("hevc");
        let mut frame = vec![0u8; 320 * 180 * 4];
        let mut second: Option<Vec<u8>> = None;
        let mut keyframes = 0;
        for i in 0..(30 * 4) {
            for (j, px) in frame.iter_mut().enumerate() {
                *px = (i as u8).wrapping_add((j / 100) as u8);
            }
            if let Some(unit) = enc.encode_bgra(&frame).expect("encode") {
                if unit.keyframe {
                    keyframes += 1;
                    if keyframes == 2 {
                        second = Some(unit.data);
                        break;
                    }
                }
            }
        }
        let data = second.expect("2s GOP 内应产出第二个关键帧");
        let nals = split_annexb(ffmpeg_next::codec::Id::HEVC, &data);
        assert!(
            nals.iter().any(|(t, _)| *t == 32),
            "VPS NAL missing in 2nd HEVC keyframe: {} bytes",
            data.len()
        );
        assert!(
            nals.iter().any(|(t, _)| *t == 33),
            "SPS NAL missing in 2nd HEVC keyframe: {} bytes",
            data.len()
        );
        assert!(
            nals.iter().any(|(t, _)| *t == 34),
            "PPS NAL missing in 2nd HEVC keyframe: {} bytes",
            data.len()
        );
    }
}
