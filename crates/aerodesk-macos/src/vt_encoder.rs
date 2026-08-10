//! VideoToolbox 硬件编码器（H.264，IOSurface 输入，零拷贝路径）。
//!
//! 输入 BGRA IOSurface（与 ScreenCaptureKit 输出格式一致），输出 AnnexB
//! H.264（str0m packetizer 兼容）。

use apple_cf::iosurface::{IOSurface, IOSurfaceLockOptions};
use apple_cf::raw::{
    CMVideoFormatDescriptionGetH264ParameterSetAtIndex,
    CMVideoFormatDescriptionGetHEVCParameterSetAtIndex,
};
use videotoolbox::Codec;
use videotoolbox::compression::{CompressionSession, CompressionSessionBuilder, EncodedFrame};

const BGRA: u32 = 0x42475241; // 'BGRA'

/// VideoToolbox H.264 编码器。
pub struct VtEncoder {
    session: CompressionSession,
    width: u32,
    height: u32,
    pts: i64,
    /// 每帧 RTP 时间戳步进（90kHz / fps；#8 压测发现固定 3000 会把 60fps 压成 30fps）。
    pts_inc: i64,
    /// 从 format description 提取的 SPS/PPS（H.264）/ VPS/SPS/PPS（HEVC）。
    /// VT 关键帧码流默认不含参数集；接收端硬解必须要有（#29/#74）。
    sps: Option<Vec<u8>>,
    pps: Option<Vec<u8>>,
    vps: Option<Vec<u8>>,
    codec: Codec,
    /// 下一帧强制关键帧（SFU 关键帧请求后置位，编码一帧后清除）。
    force_keyframe_pending: bool,
}

impl VtEncoder {
    /// H.264 编码器（兼容旧调用点）。
    pub fn new(width: u32, height: u32, fps: u32, bitrate_bps: u32) -> Result<Self, String> {
        Self::new_with_codec(width, height, fps, bitrate_bps, Codec::H264)
    }

    /// 指定 codec 的 VideoToolbox 硬编（H.264 / HEVC）。
    pub fn new_with_codec(
        width: u32,
        height: u32,
        fps: u32,
        bitrate_bps: u32,
        codec: Codec,
    ) -> Result<Self, String> {
        let session = CompressionSessionBuilder::new(width as i32, height as i32, codec)
            .with_real_time(true)
            .with_average_bit_rate(bitrate_bps as i32)
            .with_expected_frame_rate(fps as f64)
            .with_max_keyframe_interval((fps * 2) as i32)
            .build()
            .map_err(|e| format!("vt init ({codec:?}): {e:?}"))?;
        Ok(Self {
            session,
            width,
            height,
            pts: 0,
            pts_inc: (90_000 / fps.max(1)) as i64,
            sps: None,
            pps: None,
            vps: None,
            codec,
            force_keyframe_pending: false,
        })
    }

    pub fn codec(&self) -> Codec {
        self.codec
    }

    /// 请求下一帧编码为关键帧（IDR）。SFU 关键帧请求 / 新 viewer 加入时调用。
    pub fn force_keyframe(&mut self) -> Result<(), String> {
        self.force_keyframe_pending = true;
        Ok(())
    }

    /// 编码一帧；force_keyframe_pending 时经帧选项强制 IDR（编码后清除）。
    fn do_encode(&mut self, surface: &IOSurface) -> Result<EncodedFrame, String> {
        let force = std::mem::take(&mut self.force_keyframe_pending);
        let r = if force {
            self.session
                .encode_force_keyframe(surface, (self.pts, 90_000))
                .map_err(|e| format!("vt encode force keyframe: {e:?}"))
        } else {
            self.session
                .encode(surface, (self.pts, 90_000))
                .map_err(|e| format!("vt encode: {e:?}"))
        }?;
        Ok(r)
    }

    /// 本机是否有 VT HEVC 硬编（64x64 探针）。
    pub fn hevc_encoder_available() -> bool {
        Self::new_with_codec(64, 64, 30, 400_000, Codec::HEVC).is_ok()
    }

    /// 从编码输出的 format description 提取参数集（只在首帧做一次）。
    fn ensure_parameter_sets(&mut self, frame: &EncodedFrame) {
        let done = match self.codec {
            Codec::H264 => self.sps.is_some() && self.pps.is_some(),
            _ => self.vps.is_some() && self.sps.is_some() && self.pps.is_some(),
        };
        if done {
            return;
        }
        let Some(sample) = frame.cm_sample_buffer() else {
            return;
        };
        let Some(fmt) = sample.format_description() else {
            return;
        };
        let mut get = |index: usize| -> Option<Vec<u8>> {
            let mut ptr: *const u8 = std::ptr::null();
            let mut size = 0usize;
            let mut count = 0usize;
            let mut nal_len = 0i32;
            let status = unsafe {
                match self.codec {
                    Codec::H264 => CMVideoFormatDescriptionGetH264ParameterSetAtIndex(
                        fmt.as_ptr().cast(),
                        index,
                        &mut ptr,
                        &mut size,
                        &mut count,
                        &mut nal_len,
                    ),
                    _ => CMVideoFormatDescriptionGetHEVCParameterSetAtIndex(
                        fmt.as_ptr().cast(),
                        index,
                        &mut ptr,
                        &mut size,
                        &mut count,
                        &mut nal_len,
                    ),
                }
            };
            if status != 0 || ptr.is_null() || size == 0 {
                return None;
            }
            Some(unsafe { std::slice::from_raw_parts(ptr, size) }.to_vec())
        };
        match self.codec {
            Codec::H264 => {
                self.sps = get(0);
                self.pps = get(1);
            }
            _ => {
                // HEVC 参数集顺序：VPS(0)/SPS(1)/PPS(2)
                self.vps = get(0);
                self.sps = get(1);
                self.pps = get(2);
            }
        }
    }

    /// AVCC 数据里是否含关键帧 NAL（H.264 IDR=5；HEVC IDR/BLA/CRA=16..=21）。
    fn is_keyframe_avcc(&self, data: &[u8]) -> bool {
        let mut i = 0;
        while i + 4 <= data.len() {
            let len = u32::from_be_bytes([data[i], data[i + 1], data[i + 2], data[i + 3]]) as usize;
            i += 4;
            if i + len > data.len() {
                return false;
            }
            if len > 0 {
                let ty = if self.codec == Codec::H264 {
                    data[i] & 0x1F
                } else {
                    (data[i] >> 1) & 0x3F
                };
                if (self.codec == Codec::H264 && ty == 5)
                    || (self.codec != Codec::H264 && (16..=21).contains(&ty))
                {
                    return true;
                }
            }
            i += len;
        }
        false
    }

    /// 输出 AnnexB：关键帧前置参数集（接收端硬解依赖，见 #29/#74）。
    pub fn to_annexb(&self, frame: &EncodedFrame) -> Vec<u8> {
        let mut out = Vec::with_capacity(frame.data.len() + 64);
        if self.is_keyframe_avcc(&frame.data) {
            if self.codec != Codec::H264 {
                if let Some(vps) = &self.vps {
                    out.extend_from_slice(&[0, 0, 0, 1]);
                    out.extend_from_slice(vps);
                }
            }
            if let Some(sps) = &self.sps {
                out.extend_from_slice(&[0, 0, 0, 1]);
                out.extend_from_slice(sps);
            }
            if let Some(pps) = &self.pps {
                out.extend_from_slice(&[0, 0, 0, 1]);
                out.extend_from_slice(pps);
            }
        }
        out.extend_from_slice(&avcc_to_annexb(&frame.data));
        out
    }

    /// 零拷贝编码：直接硬编 ScreenCaptureKit 输出的 IOSurface。
    pub fn encode_surface(&mut self, surface: &IOSurface) -> Result<Option<EncodedFrame>, String> {
        let frame = self.do_encode(surface)?;
        self.pts += self.pts_inc;
        self.ensure_parameter_sets(&frame);
        if frame.data.is_empty() {
            return Ok(None);
        }
        Ok(Some(frame))
    }

    /// 编码一帧 BGRA 像素（写 IOSurface → 硬编）。
    pub fn encode_bgra(&mut self, bgra: &[u8]) -> Result<Option<EncodedFrame>, String> {
        if bgra.len() != (self.width * self.height * 4) as usize {
            return Err("bgra size mismatch".into());
        }
        let surface = IOSurface::create(self.width as usize, self.height as usize, BGRA, 4)
            .ok_or("iosurface create")?;
        {
            let mut guard = surface
                .lock(IOSurfaceLockOptions::NONE)
                .map_err(|_| "iosurface lock")?;
            let dst = guard.base_address_mut().ok_or("base address")?;
            unsafe {
                std::ptr::copy_nonoverlapping(bgra.as_ptr(), dst, bgra.len());
            }
        }
        let frame = self.do_encode(&surface)?;
        self.pts += self.pts_inc; // 90kHz / fps
        self.ensure_parameter_sets(&frame);
        if frame.data.is_empty() {
            return Ok(None);
        }
        Ok(Some(frame))
    }
}

/// VideoToolbox 输出 AVCC（4 字节长度前缀）→ str0m 需要的 AnnexB（起始码）。
pub fn avcc_to_annexb(avcc: &[u8]) -> Vec<u8> {
    let mut out = Vec::with_capacity(avcc.len() + 64);
    let mut i = 0;
    while i + 4 <= avcc.len() {
        let len = u32::from_be_bytes([avcc[i], avcc[i + 1], avcc[i + 2], avcc[i + 3]]) as usize;
        i += 4;
        if i + len > avcc.len() {
            break;
        }
        out.extend_from_slice(&[0, 0, 0, 1]);
        out.extend_from_slice(&avcc[i..i + len]);
        i += len;
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn avcc_to_annexb_conversion() {
        // 两个 NAL：3 字节和 2 字节
        let avcc = [0, 0, 0, 3, 0x67, 0x01, 0x02, 0, 0, 0, 2, 0x68, 0x03];
        let annexb = avcc_to_annexb(&avcc);
        assert_eq!(&annexb[..4], &[0, 0, 0, 1]);
        assert_eq!(&annexb[4..7], &[0x67, 0x01, 0x02]);
        assert_eq!(&annexb[7..11], &[0, 0, 0, 1]);
        assert_eq!(&annexb[11..13], &[0x68, 0x03]);
    }

    /// #74 HEVC 硬编回环：VT HEVC 编码 → VT HEVC 硬解 → RGBA。
    #[test]
    fn vt_hevc_encode_decode_roundtrip() {
        use crate::decode::{HevcDecoder, to_rgba};
        use crate::synthetic::SyntheticSource;

        let (w, h) = (320u32, 180u32);
        let Ok(mut enc) = VtEncoder::new_with_codec(w, h, 30, 1_000_000, Codec::HEVC) else {
            eprintln!("skip: 本机无 VT HEVC 编码器");
            return;
        };
        let mut src = SyntheticSource::new(w, h);
        let mut decoder = HevcDecoder::new();
        let mut decoded = None;
        for _ in 0..12 {
            let Some(frame) = enc.encode_bgra(&src.next_frame_bgra()).expect("encode") else {
                continue;
            };
            let annexb = enc.to_annexb(&frame);
            if let Ok(Some(buf)) = decoder.decode_annexb(&annexb, 0) {
                decoded = Some(buf);
                break;
            }
        }
        let buf = decoded.expect("应在若干帧内解出 HEVC 硬编帧");
        let (rgba, dw, dh) = to_rgba(&buf).expect("rgba");
        assert_eq!((dw, dh), (w as usize, h as usize));
        assert!(
            rgba.chunks_exact(4).any(|p| p[3] == 255),
            "alpha 应全不透明"
        );
    }

    #[test]
    fn vt_encodes_synthetic_bgra() {
        // 跳过：无硬件加速环境（CI/虚拟机）可能失败；本机有 Metal 时通过。
        let Ok(mut enc) = VtEncoder::new(320, 180, 30, 800_000) else {
            eprintln!("VideoToolbox unavailable, skipping");
            return;
        };
        let mut frame = vec![0u8; 320 * 180 * 4];
        for (i, px) in frame.chunks_exact_mut(4).enumerate() {
            let v = (i / 100) as u8;
            px.copy_from_slice(&[v, v, v, 255]);
        }
        let out = enc.encode_bgra(&frame).expect("encode");
        let Some(out) = out else {
            return;
        };
        let annexb = avcc_to_annexb(&out.data);
        assert!(
            annexb.windows(4).any(|w| w == [0, 0, 0, 1]),
            "expected AnnexB start codes"
        );
    }
}
