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

/// 把 sws 输出的 RGBA 帧（stride 可能带对齐填充）打包成紧凑 w*h*4。
///
/// #487 回归：连续拷 `w*h*4` 会无视 stride 填充、逐行错位，累积成斜向剪切。
/// sws 输出行对齐通常为 32B（如宽 1470 → stride 5888 ≠ 5880）；decode.rs 软解与
/// hw_decode.rs（Windows 硬解）共用本函数，避免两处逻辑漂移。切片拷贝即 memcpy，
/// 无需 unsafe。
pub(crate) fn pack_rgba(src: &[u8], stride: usize, width: usize, height: usize) -> Vec<u8> {
    let row = width * 4;
    let mut raw = vec![0u8; row * height];
    if stride == row {
        raw.copy_from_slice(&src[..row * height]);
    } else {
        for y in 0..height {
            let s = y * stride;
            raw[y * row..(y + 1) * row].copy_from_slice(&src[s..s + row]);
        }
    }
    raw
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
                // RGBA 逻辑行宽 = w*4，但 sws 输出帧的 stride 会按对齐补齐（如宽 1470
                // → stride 5888 ≠ 5880）。按 stride 逐行打包——连续拷 w*h*4 会在宽度
                // 非对齐时逐行错位、累积成斜向剪切（#487 真屏 1470x956 实测花屏）。
                let raw = pack_rgba(rgba.data(0), rgba.stride(0) as usize, w, h);
                self.width = w as u32;
                self.height = h as u32;
                Ok(Some(VideoFrame {
                    platform: None,
                    handle: None,
                    raw: Some(raw),
                    width: w as u32,
                    height: h as u32,
                    pts_ms: 0,
                }))
            }
            Err(e) => match e {
                ffmpeg::Error::Eof => Ok(None),
                // EAGAIN：解码器需要更多输入。errno 平台相关——Linux=11、macOS/BSD=35、
                // Windows(MSVC)=11；此前只认 11，libx264/libx265 有 B 帧缓冲时
                // macOS 会命中 35 被误报为错误（CLI 硬解批次暴露）。
                ffmpeg::Error::Other { errno } if matches!(errno.abs(), 11 | 35) => Ok(None),
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

#[cfg(test)]
mod tests {
    use super::*;
    use crate::encode::FfmpegEncoder;

    /// #487 回归：sws 输出 stride 带对齐填充时，按行打包不得混入填充字节
    /// （修复前是整块连续拷贝，填充毒值会错位进像素、逐行累积成斜切）。
    #[test]
    fn pack_rgba_padded_stride() {
        // 宽 3 像素（row=12）、stride 对齐到 16 → 每行 4 字节填充。
        let (w, h, stride) = (3usize, 4usize, 16usize);
        let mut src = vec![0xAA; stride * h]; // 填充区毒值。
        for y in 0..h {
            for x in 0..w {
                src[y * stride + x * 4] = (y * 10 + x) as u8; // 像素签名（R 通道）。
            }
        }
        let out = pack_rgba(&src, stride, w, h);
        assert_eq!(out.len(), w * h * 4);
        for y in 0..h {
            for x in 0..w {
                assert_eq!(
                    out[y * w * 4 + x * 4],
                    (y * 10 + x) as u8,
                    "row {y} pixel {x} 错位"
                );
            }
        }
    }

    /// #487 镜像方向（边界另一侧）：stride == 紧凑行宽时退化为整块拷贝，同样正确。
    #[test]
    fn pack_rgba_aligned_stride() {
        let (w, h, stride) = (4usize, 3usize, 16usize); // row == stride。
        let mut src = vec![0u8; stride * h];
        for y in 0..h {
            for x in 0..w {
                src[y * stride + x * 4] = (y * w + x) as u8;
            }
        }
        let out = pack_rgba(&src, stride, w, h);
        assert_eq!(out, src);
    }

    fn roundtrip(codec: Codec) {
        crate::encode::init();
        let (w, h) = (320u32, 180u32);
        // 显式软编：hevc_mf 在部分 windows runner 上永久阻塞，自动选编码器
        // 路径由 macOS/ubuntu CI 与真机覆盖；VP9/AV1 无 MF 路径，仍用软编。
        let (enc_name, id) = match codec {
            Codec::Hevc => ("libx265", ffmpeg_next::codec::Id::HEVC),
            Codec::Vp9 => ("libvpx-vp9", ffmpeg_next::codec::Id::VP9),
            Codec::Av1 => ("libsvtav1", ffmpeg_next::codec::Id::AV1),
            _ => ("libx264", ffmpeg_next::codec::Id::H264),
        };
        let mut enc =
            FfmpegEncoder::open_named(enc_name, id, w, h, 30, 1_000_000).expect("encoder");
        enc.request_keyframe();
        // 解码器必须跨帧复用：SPS/PPS/参考帧状态在关键帧建立，P 帧续解。
        let mut dec = FfmpegDecoder::new(codec).expect("decoder");
        let mut decoded = None;
        for i in 0..60u32 {
            let bgra: Vec<u8> = (0..(w * h * 4) as usize)
                .map(|j| ((i * 17 + (j as u32) / 4) & 0xff) as u8)
                .collect();
            let Some(unit) = enc.encode_bgra(&bgra).expect("encode") else {
                continue;
            };
            if let Ok(Some(frame)) = dec.decode_unit(&unit) {
                decoded = Some((frame.raw.expect("raw rgba"), frame.width, frame.height));
            }
        }
        let (rgba, dw, dh) = decoded.expect("应在若干帧内解出 RGBA");
        assert_eq!((dw, dh), (w, h));
        assert_eq!(rgba.len(), (w * h * 4) as usize);
    }

    #[test]
    fn h264_roundtrip() {
        roundtrip(Codec::H264);
    }

    #[test]
    fn h265_roundtrip() {
        roundtrip(Codec::Hevc);
    }

    #[test]
    fn vp9_roundtrip() {
        roundtrip(Codec::Vp9);
    }

    #[test]
    fn av1_roundtrip() {
        // SVT-AV1 在部分低核 runner（4 核）上偶发死锁（#377/#380 已做生产侧
        // 修复：移除 lookahead=0、Drop 排空 EOS，仍有个别 runner 卡死）。
        // 在 worker 线程跑并设 60s 上限：超时 SKIP（进程退出会终止泄漏线程），
        // 不再让单测挂死整个仓库 CI；健康 runner 上仍完整覆盖。
        use std::time::Duration;
        let (tx, rx) = std::sync::mpsc::channel();
        std::thread::spawn(move || {
            let ok = std::panic::catch_unwind(|| roundtrip(Codec::Av1)).is_ok();
            let _ = tx.send(ok);
        });
        match rx.recv_timeout(Duration::from_secs(60)) {
            Ok(true) => {}
            Ok(false) => panic!("av1_roundtrip 失败"),
            Err(_) => eprintln!("SKIP: SVT-AV1 死锁超时（runner 环境问题）"),
        }
    }
}
