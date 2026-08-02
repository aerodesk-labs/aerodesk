//! VideoToolbox H.264 硬件解码器。
//!
//! 输入 AnnexB H.264（str0m MediaData.data），输出 CVPixelBuffer。
//! 关键帧时重建 CMVideoFormatDescription（SPS/PPS）。

use std::sync::mpsc;

use apple_cf::cm::CMSampleBuffer;
use apple_cf::cm::format_description::CMFormatDescription;
use apple_cf::cv::CVPixelBuffer;
use apple_cf::raw::{
    CMBlockBufferCreateWithMemoryBlock, CMBlockBufferRef, CMFormatDescriptionRef,
    CMSampleBufferCreate, CMSampleBufferRef, CMVideoFormatDescriptionCreateFromH264ParameterSets,
};
use videotoolbox::Codec;
use videotoolbox::decompression::{DecodedFrame, DecompressionSession};

/// 从 AnnexB 数据解析 NAL 单元，返回 (type, payload)。
fn parse_annexb_nal(data: &[u8]) -> Vec<(u8, &[u8])> {
    let mut out = Vec::new();
    let mut i = 0;
    while i + 3 <= data.len() {
        // 起始码 00 00 01（4 字节 00 00 00 01 会在 i+1 处命中同一模式）
        if data[i] == 0 && data[i + 1] == 0 && data[i + 2] == 1 {
            let start = i + 3;
            let mut j = start;
            while j + 3 <= data.len() {
                if data[j] == 0 && data[j + 1] == 0 && data[j + 2] == 1 {
                    break;
                }
                j += 1;
            }
            // 最后一个 NAL 没有后续起始码：循环因越界退出，j 停在 len-2，
            // 必须补到 len，否则末尾 2 字节（如 CABAC 收尾数据）会被截断。
            if j + 3 > data.len() {
                j = data.len();
            }
            let payload = &data[start..j];
            if !payload.is_empty() {
                out.push((payload[0] & 0x1F, payload));
            }
            i = j;
        } else {
            i += 1;
        }
    }
    out
}

/// 从 SPS/PPS 构造 CMVideoFormatDescription（用 apple_cf 包装，retain 生命周期手动管理）。
fn build_format_description(sps: &[u8], pps: &[u8]) -> Result<CMFormatDescription, String> {
    let mut fmt_out: CMFormatDescriptionRef = std::ptr::null();
    let sets = [sps.as_ptr(), pps.as_ptr()];
    let sizes = [sps.len(), pps.len()];
    let status = unsafe {
        CMVideoFormatDescriptionCreateFromH264ParameterSets(
            std::ptr::null_mut(),
            2,
            sets.as_ptr(),
            sizes.as_ptr(),
            4, // NAL 长度前缀（AVCC 风格样本）
            &mut fmt_out,
        )
    };
    if status != 0 || fmt_out.is_null() {
        return Err(format!("format description: {status}"));
    }
    // apple_cf from_raw 不 retain——这里 Create 已 +1，直接包装（所有权转移）
    CMFormatDescription::from_raw(fmt_out.cast_mut().cast::<std::ffi::c_void>())
        .ok_or_else(|| "fmt desc null".to_string())
}

/// 构造视频样本（AVCC 风格：4 字节长度前缀 + NAL）。
fn build_sample_buffer(
    format: &CMFormatDescription,
    avcc_data: &[u8],
    _pts: i64,
) -> Result<CMSampleBuffer, String> {
    // 1. CMBlockBuffer（拷贝数据：解码是异步的，样本必须持有自己的数据副本，
    //    避免回调晚于下一帧时悬垂引用）
    let mut bb_out: CMBlockBufferRef = std::ptr::null_mut();
    let status = unsafe {
        CMBlockBufferCreateWithMemoryBlock(
            std::ptr::null_mut(),
            avcc_data.as_ptr() as *mut std::ffi::c_void,
            avcc_data.len(),
            std::ptr::null_mut(),
            std::ptr::null(),
            0,
            avcc_data.len(),
            2, // kCMBlockBufferAlwaysCopyDataFlag：块缓冲持有数据副本
            &mut bb_out,
        )
    };
    if status != 0 || bb_out.is_null() {
        return Err(format!("block buffer: {status}"));
    }

    // 2. CMSampleBuffer（timing 可选：传 0 条目）
    let mut sb_out: CMSampleBufferRef = std::ptr::null_mut();
    let status = unsafe {
        CMSampleBufferCreate(
            std::ptr::null_mut(),
            bb_out,
            1, // dataReady
            None,
            std::ptr::null_mut(),
            format.as_ptr().cast(),
            1, // numSamples
            0, // numSampleTimingEntries（可选）
            std::ptr::null(),
            1, // numSampleSizeEntries
            &avcc_data.len(),
            &mut sb_out,
        )
    };
    if status != 0 || sb_out.is_null() {
        return Err(format!("sample buffer: {status}"));
    }
    CMSampleBuffer::from_raw(sb_out.cast::<std::ffi::c_void>())
        .ok_or_else(|| "sample buffer null".to_string())
}

/// H.264 硬件解码器。
pub struct H264Decoder {
    session: Option<DecompressionSession>,
    format: Option<CMFormatDescription>,
    rx: mpsc::Receiver<DecodedFrame>,
    /// 解码是异步的：sample buffer 引用的内存必须活到回调后。
    pending: Option<(CMSampleBuffer, Vec<u8>)>,
}

impl Default for H264Decoder {
    fn default() -> Self {
        Self::new()
    }
}

impl H264Decoder {
    pub fn new() -> Self {
        let (tx, rx) = mpsc::channel();
        // 占位 session：等第一个关键帧的 SPS/PPS 后重建
        let _ = tx;
        Self {
            session: None,
            format: None,
            rx,
            pending: None,
        }
    }

    pub fn is_hardware_supported() -> bool {
        DecompressionSession::is_hardware_decode_supported(Codec::H264)
    }

    /// 解码一帧 AnnexB H.264。关键帧触发 format description 重建。
    pub fn decode_annexb(
        &mut self,
        data: &[u8],
        _pts: i64,
    ) -> Result<Option<CVPixelBuffer>, String> {
        let nals = parse_annexb_nal(data);
        if nals.is_empty() {
            return Ok(None);
        }

        let mut avcc = Vec::with_capacity(data.len() + 8);
        let mut sps: Option<&[u8]> = None;
        let mut pps: Option<&[u8]> = None;

        for (ty, payload) in &nals {
            match ty {
                7 => sps = Some(payload),
                8 => pps = Some(payload),
                _ => {}
            }
        }

        // 关键帧（含 SPS/PPS）：重建 format description + session
        if let (Some(sps), Some(pps)) = (sps, pps) {
            let fmt = build_format_description(sps, pps)?;
            let (tx, rx) = mpsc::channel();
            let session = DecompressionSession::new(&fmt, move |frame: DecodedFrame| {
                let _ = tx.send(frame);
            })
            .map_err(|e| format!("decompress session: {e:?}"))?;
            self.format = Some(fmt);
            self.session = Some(session);
            self.rx = rx;
        }

        // AVCC 化 VCL NAL（1..=5）。SPS/PPS 已在 format description 中；SEI/AUD 等
        // 非 VCL NAL 必须剔除——x264 等编码器输出的 buffering_period/pic_timing SEI
        // 在本机 VideoToolbox 硬解下会导致 kVTVideoDecoderMalfunctionErr(-12909)。
        for (ty, payload) in &nals {
            if !(1..=5).contains(ty) {
                continue;
            }
            let len = (payload.len() as u32).to_be_bytes();
            avcc.extend_from_slice(&len);
            avcc.extend_from_slice(payload);
        }

        let Some(session) = &self.session else {
            return Ok(None); // 等关键帧
        };
        let Some(format) = &self.format else {
            return Ok(None);
        };

        let sample = build_sample_buffer(format, &avcc, _pts)?;
        self.pending = Some((sample, avcc));
        let (sample, _) = self.pending.as_ref().unwrap();
        session
            .decode(sample)
            .map_err(|e| format!("decode: {e:?}"))?;

        // 取回调帧（解码异步，阻塞等待）
        match self.rx.recv_timeout(std::time::Duration::from_millis(2000)) {
            Ok(frame) => Ok(frame.image_buffer),
            Err(_) => Ok(None),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use aerodesk_macos::encoder::X264Encoder;

    #[test]
    fn parses_last_nal_without_truncation() {
        // 回归：最后一个 NAL 的末尾 2 字节不能被截断（曾导致 VideoToolbox -12909）
        let data = [
            0, 0, 0, 1, 0x67, 0x64, 1, 2, 3, 0, 0, 1, 0x65, 0xaa, 0xbb, 0xcc, 0xdd, 0xee,
        ];
        let nals = parse_annexb_nal(&data);
        assert_eq!(nals.len(), 2, "SPS + IDR expected");
        assert_eq!(nals[0].0, 7);
        assert_eq!(nals[1].0, 5);
        // 最后一个 NAL 必须完整（含末尾 2 字节）
        assert_eq!(nals[1].1, &[0x65, 0xaa, 0xbb, 0xcc, 0xdd, 0xee]);
    }

    #[test]
    fn decodes_x264_frames() {
        // 生成 H.264 关键帧 + 若干 P 帧（模拟真实会话：关键帧重建 session，P 帧续解）
        let mut enc = X264Encoder::new(320, 180, 30, 500).expect("x264");
        let mut decoder = H264Decoder::new();
        let mut frame = vec![0u8; 320 * 180 * 3];
        let mut decoded_frames = 0;
        let mut saw_keyframe = false;

        for i in 0..8 {
            for (j, px) in frame.iter_mut().enumerate() {
                *px = (i * 30 + j / 100) as u8;
            }
            if let Some(out) = enc.encode(&frame).expect("encode") {
                if out.keyframe {
                    saw_keyframe = true;
                }
                match decoder.decode_annexb(&out.data, (i * 3000) as i64) {
                    Ok(Some(pb)) => {
                        decoded_frames += 1;
                        assert!(pb.width() > 0 && pb.height() > 0, "decoded buffer has size");
                    }
                    Ok(None) => {}
                    Err(e) => panic!("decode error: {e}"),
                }
            }
        }
        assert!(saw_keyframe, "expected at least one keyframe");
        // 关键帧 + 全部 P 帧都应解出（真实会话中首帧起即为连续画面）
        assert_eq!(
            decoded_frames, 8,
            "expected all 8 frames decoded, got {decoded_frames}"
        );
    }
}
