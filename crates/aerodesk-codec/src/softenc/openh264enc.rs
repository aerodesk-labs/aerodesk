//! OpenH264 软编（全平台；Windows 无系统 x264 时的编码回退，#3）。
//!
//! 输入 RGBA，输出 AnnexB H.264（关键帧含 SPS/PPS，与 x264 软编同接口）。

use openh264::OpenH264API;
use openh264::encoder::{BitRate, Encoder, EncoderConfig, FrameRate};
use openh264::formats::YUVBuffer;

/// RGBA → I420（打包顺序 Y + U + V，BT.601 有限范围）。
pub fn rgba_to_i420_packed(rgba: &[u8], width: usize, height: usize) -> Vec<u8> {
    assert_eq!(rgba.len(), width * height * 4, "rgba size mismatch");
    let mut out = vec![0u8; width * height + (width / 2) * (height / 2) * 2];
    let y_off = 0usize;
    let u_off = width * height;
    let v_off = u_off + (width / 2) * (height / 2);
    for row in 0..height {
        for col in 0..width {
            let i = (row * width + col) * 4;
            let (r, g, b) = (rgba[i] as i32, rgba[i + 1] as i32, rgba[i + 2] as i32);
            let yv = ((66 * r + 129 * g + 25 * b + 128) >> 8) + 16;
            out[y_off + row * width + col] = yv.clamp(0, 255) as u8;
            if row % 2 == 0 && col % 2 == 0 {
                let uv = ((row / 2) * (width / 2) + col / 2) as usize;
                let u = ((-38 * r - 74 * g + 112 * b + 128) >> 8) + 128;
                let v = ((112 * r - 94 * g - 18 * b + 128) >> 8) + 128;
                out[u_off + uv] = u.clamp(0, 255) as u8;
                out[v_off + uv] = v.clamp(0, 255) as u8;
            }
        }
    }
    out
}

/// OpenH264 H.264 编码器（RGBA 输入 → AnnexB 输出）。
pub struct OpenH264Encoder {
    encoder: Encoder,
    width: usize,
    height: usize,
}

impl OpenH264Encoder {
    pub fn new(width: u32, height: u32, fps: u32, bitrate_kbps: u32) -> Result<Self, String> {
        assert!(
            width % 2 == 0 && height % 2 == 0,
            "I420 needs even dimensions"
        );
        let config = EncoderConfig::new()
            .bitrate(BitRate::from_bps(bitrate_kbps.saturating_mul(1000)))
            .max_frame_rate(FrameRate::from_hz(fps as f32))
            .skip_frames(false);
        let encoder = Encoder::with_api_config(OpenH264API::from_source(), config)
            .map_err(|e| format!("openh264 encoder init: {e}"))?;
        Ok(Self {
            encoder,
            width: width as usize,
            height: height as usize,
        })
    }

    /// 编码一帧 RGBA，输出 AnnexB；关键帧含 SPS/PPS。
    pub fn encode_rgba(&mut self, rgba: &[u8]) -> Result<Option<super::EncodedFrame>, String> {
        let yuv = rgba_to_i420_packed(rgba, self.width, self.height);
        let buf = YUVBuffer::from_vec(yuv, self.width, self.height);
        let stream = self
            .encoder
            .encode(&buf)
            .map_err(|e| format!("openh264 encode: {e}"))?;
        let data = stream.to_vec();
        if data.is_empty() {
            return Ok(None);
        }
        // 关键帧判定：AnnexB 数据中任一 NAL 为 IDR（type 5）。
        let mut keyframe = false;
        let mut i = 0usize;
        while i + 3 <= data.len() {
            if data[i] == 0 && data[i + 1] == 0 && data[i + 2] == 1 {
                let start = if i + 3 < data.len() && data[i + 3] == 0 {
                    i + 4
                } else {
                    i + 3
                };
                if start < data.len() && (data[start] & 0x1F) == 5 {
                    keyframe = true;
                    break;
                }
                i = start;
            } else {
                i += 1;
            }
        }
        Ok(Some(super::EncodedFrame {
            data,
            keyframe,
            pts: 0,
        }))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::softenc::decode::SoftDecoder;

    /// OpenH264 编码 → OpenH264 解码回环（Windows/Linux 软编软解验证）。
    /// #277 跨平台抽象：泛型函数只依赖 core `Encoder`/`Decoder` trait 即可
    /// 完成 OpenH264 编解码（Windows/Linux/Android 软编软解回退路径）。
    #[test]
    fn generic_encoder_decoder_trait_roundtrip() {
        fn encode_frames<E: aerodesk_core::platform::Encoder>(
            enc: &mut E,
            w: u32,
            h: u32,
        ) -> Vec<aerodesk_core::platform::EncodedUnit> {
            enc.configure(aerodesk_core::platform::Codec::H264, w, h, 30)
                .expect("configure");
            let mut frame = vec![0u8; (w * h * 4) as usize];
            let mut units = Vec::new();
            for i in 0..8u32 {
                for (j, px) in frame.iter_mut().enumerate() {
                    *px = (i * 25 + (j as u32 / 400)) as u8;
                }
                let vf = aerodesk_core::platform::VideoFrame {
                    platform: None,
                    handle: None,
                    raw: Some(frame.clone()),
                    width: w,
                    height: h,
                    pts_ms: (i * 33) as u64,
                };
                if let Some(u) = enc.encode(&vf).expect("encode") {
                    units.push(u);
                }
            }
            units
        }

        fn count_frames<D: aerodesk_core::platform::Decoder>(
            dec: &mut D,
            units: &[aerodesk_core::platform::EncodedUnit],
        ) -> usize {
            let mut n = 0;
            for u in units {
                if let Ok(Some(_)) = dec.decode(u) {
                    n += 1;
                }
            }
            n
        }

        let (w, h) = (320u32, 180u32);
        let mut enc = OpenH264Encoder::new(w, h, 30, 500).expect("encoder");
        let units = encode_frames(&mut enc, w, h);
        assert!(units.iter().any(|u| u.keyframe), "应有关键帧");
        let mut dec = SoftDecoder::new().expect("decoder");
        let n = count_frames(&mut dec, &units);
        assert!(n >= 1, "泛型 Decoder 应解出帧，got {n}");
    }

    #[test]
    fn openh264_encode_decode_roundtrip() {
        let (w, h) = (320u32, 180u32);
        let mut enc = OpenH264Encoder::new(w, h, 30, 500).expect("openh264 encoder");
        let mut dec = SoftDecoder::new().expect("openh264 decoder");

        let mut frame = vec![0u8; (w * h * 4) as usize];
        let mut decoded = None;
        for i in 0..12u8 {
            for (j, px) in frame.iter_mut().enumerate() {
                *px = (i * 25 + (j / 400) as u8).wrapping_add(0);
            }
            if let Some(out) = enc.encode_rgba(&frame).expect("encode") {
                if let Some((rgba, dw, dh)) = dec.decode_rgba(&out.data).expect("decode") {
                    decoded = Some((rgba, dw, dh));
                    break;
                }
            }
        }
        let (rgba, dw, dh) = decoded.expect("应在若干帧内解码出 RGBA");
        assert_eq!((dw, dh), (w as usize, h as usize));
        assert_eq!(rgba.len(), (w * h * 4) as usize);
        assert!(rgba.chunks_exact(4).any(|p| p[3] == 255));
    }
}

/// 核心 `Encoder` 实现（OpenH264 软编，H.264；全平台回退）。
/// `VideoFrame.raw` 按 core 约定为 BGRA32，此处转 RGBA 后编码。
impl aerodesk_core::platform::Encoder for OpenH264Encoder {
    type Error = String;

    fn configure(
        &mut self,
        codec: aerodesk_core::platform::Codec,
        width: u32,
        height: u32,
        fps: u32,
    ) -> Result<(), Self::Error> {
        if codec != aerodesk_core::platform::Codec::H264 {
            return Err(format!("openh264 仅支持 H.264，收到 {codec:?}"));
        }
        *self = Self::new(width, height, fps, 1500)?;
        Ok(())
    }

    fn encode(
        &mut self,
        frame: &aerodesk_core::platform::VideoFrame,
    ) -> Result<Option<aerodesk_core::platform::EncodedUnit>, Self::Error> {
        let Some(raw) = &frame.raw else {
            return Err("openh264 encoder requires raw BGRA frame".into());
        };
        let rgba = super::bgra_to_rgba(raw);
        let Some(out) = self.encode_rgba(&rgba)? else {
            return Ok(None);
        };
        Ok(Some(aerodesk_core::platform::EncodedUnit {
            data: out.data,
            keyframe: out.keyframe,
            pts_ms: frame.pts_ms,
            rtp_timestamp: 0,
        }))
    }

    fn request_keyframe(&mut self) {
        // openh264 wrapper 未暴露强制 IDR；GOP 内自然 IDR（≤2s）可接受。
    }

    fn set_bitrate(&mut self, _bitrate_bps: u64, _fps: u32) {}
}
