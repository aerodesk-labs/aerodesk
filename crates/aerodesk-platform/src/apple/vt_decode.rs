//! VideoToolbox 硬件解码器（H.264/HEVC，macOS/iOS 共享，#506 收敛）。
//!
//! 输入 AnnexB（str0m MediaData.data），输出 CVPixelBuffer；`to_rgba` 转 RGBA
//! 供 UI/FFI。关键帧含参数集时**仅当参数集变化**才重建 format description +
//! VT 会话（编码端每个关键帧都前置参数集，无脑重建会产生 ~GOP 周期卡顿）。
//!
//! 合并自原 `macos/decode.rs`（会话复用 + 陈旧回调帧排空）与 `ios/decode.rs`
//! （`detect_codec`/`DecoderKind` + core `Decoder` trait 实现）。

use std::sync::mpsc;

use apple_cf::cm::CMSampleBuffer;
use apple_cf::cm::format_description::CMFormatDescription;
use apple_cf::cv::CVPixelBuffer;
use apple_cf::raw::{
    CMBlockBufferCreateWithMemoryBlock, CMBlockBufferRef, CMFormatDescriptionRef,
    CMSampleBufferCreate, CMSampleBufferRef, CMVideoFormatDescriptionCreateFromH264ParameterSets,
    CMVideoFormatDescriptionCreateFromHEVCParameterSets,
};
use videotoolbox::Codec;
use videotoolbox::decompression::{DecodedFrame, DecompressionSession};

/// 从 AnnexB 数据切分 NAL 单元，返回**含 NAL 头**的完整单元。
/// H.264 头 1 字节；HEVC 头 2 字节——类型解析交给调用方按 codec 处理。
fn split_annexb(data: &[u8]) -> Vec<&[u8]> {
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
            if j > start {
                out.push(&data[start..j]);
            }
            i = j;
        } else {
            i += 1;
        }
    }
    out
}

/// H.264 NAL 类型（低 5 位）。
fn h264_type(nal: &[u8]) -> u8 {
    nal.first().copied().unwrap_or(0) & 0x1F
}

/// HEVC NAL 类型（2 字节头：forbidden_zero_bit(1) + type(6) + layer/tid(9)）。
fn hevc_type(nal: &[u8]) -> u8 {
    (nal.first().copied().unwrap_or(0) >> 1) & 0x3F
}

/// 从 AnnexB 数据解析 H.264 NAL 单元，返回 (type, payload)。
fn parse_annexb_nal(data: &[u8]) -> Vec<(u8, &[u8])> {
    split_annexb(data)
        .into_iter()
        .filter(|nal| !nal.is_empty())
        .map(|nal| (h264_type(nal), nal))
        .collect()
}

/// 视频 codec 识别（按关键帧参数集 NAL 类型，H264 7/8，HEVC 32/33/34）。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum DecoderKind {
    H264,
    Hevc,
}

/// 从首帧/关键帧 AnnexB 数据自动识别 codec；无参数集时返回 None（等关键帧）。
pub fn detect_codec(data: &[u8]) -> Option<DecoderKind> {
    for nal in split_annexb(data) {
        if nal.is_empty() {
            continue;
        }
        match h264_type(nal) {
            7 | 8 => return Some(DecoderKind::H264),
            _ => {}
        }
        match hevc_type(nal) {
            32 | 33 | 34 => return Some(DecoderKind::Hevc),
            _ => {}
        }
    }
    None
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

/// 从 VPS/SPS/PPS 构造 HEVC CMVideoFormatDescription。
fn build_hevc_format_description(
    vps: &[u8],
    sps: &[u8],
    pps: &[u8],
) -> Result<CMFormatDescription, String> {
    let mut fmt_out: CMFormatDescriptionRef = std::ptr::null();
    let sets = [vps.as_ptr(), sps.as_ptr(), pps.as_ptr()];
    let sizes = [vps.len(), sps.len(), pps.len()];
    let status = unsafe {
        CMVideoFormatDescriptionCreateFromHEVCParameterSets(
            std::ptr::null_mut(),
            3,
            sets.as_ptr(),
            sizes.as_ptr(),
            4, // NAL 长度前缀（AVCC 风格样本）
            std::ptr::null_mut(),
            &mut fmt_out,
        )
    };
    if status != 0 || fmt_out.is_null() {
        return Err(format!("hevc format description: {status}"));
    }
    CMFormatDescription::from_raw(fmt_out.cast_mut().cast::<std::ffi::c_void>())
        .ok_or_else(|| "hevc fmt desc null".to_string())
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
    /// 上次建会话用的参数集：编码端每个关键帧都前置 SPS/PPS，
    /// 内容未变（分辨率/档位不变）时跳过重建——VT 会话创建开销大，
    /// 无脑重建会变成 ~GOP 周期一次的规律性卡顿。
    last_sps: Vec<u8>,
    last_pps: Vec<u8>,
}

impl Default for H264Decoder {
    fn default() -> Self {
        Self::new()
    }
}

impl H264Decoder {
    pub fn new() -> Self {
        Self {
            session: None,
            format: None,
            rx: mpsc::channel().1,
            pending: None,
            last_sps: Vec::new(),
            last_pps: Vec::new(),
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

        // 关键帧（含 SPS/PPS）：参数集变化（或首帧）才重建 format description + session。
        if let (Some(sps), Some(pps)) = (sps, pps) {
            let unchanged = self.session.is_some() && self.last_sps == sps && self.last_pps == pps;
            if !unchanged {
                let fmt = build_format_description(sps, pps)?;
                let (tx, rx) = mpsc::channel();
                let session = DecompressionSession::new(&fmt, move |frame: DecodedFrame| {
                    let _ = tx.send(frame);
                })
                .map_err(|e| format!("decompress session: {e:?}"))?;
                self.format = Some(fmt);
                self.session = Some(session);
                self.rx = rx;
                self.last_sps = sps.to_vec();
                self.last_pps = pps.to_vec();
            }
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

        // 丢弃上一帧超时后残留的旧回调帧（防旧帧污染当前帧）。
        while self.rx.try_recv().is_ok() {}
        session
            .decode(sample)
            .map_err(|e| format!("decode: {e:?}"))?;

        // 取回调帧（解码异步，阻塞等待）。超时从 2s 降到 150ms：
        // ① 不让媒体循环被单帧解码卡死 2s；② 超时报 Err 供上层触发 PLI。
        match self.rx.recv_timeout(std::time::Duration::from_millis(150)) {
            Ok(frame) => Ok(frame.image_buffer),
            Err(e) => Err(format!("decode timeout: {e:?}")),
        }
    }
}

/// H.265/HEVC 硬件解码器（VideoToolbox）。
///
/// 输入 AnnexB HEVC（str0m MediaData.data），输出 CVPixelBuffer。
/// 关键帧（含 VPS/SPS/PPS）且参数集**变化**时才重建 CMVideoFormatDescription
///（同 H264：编码端每个关键帧都前置参数集，无脑重建有周期性卡顿）。
pub struct HevcDecoder {
    session: Option<DecompressionSession>,
    format: Option<CMFormatDescription>,
    rx: mpsc::Receiver<DecodedFrame>,
    pending: Option<(CMSampleBuffer, Vec<u8>)>,
    last_vps: Vec<u8>,
    last_sps: Vec<u8>,
    last_pps: Vec<u8>,
}

impl Default for HevcDecoder {
    fn default() -> Self {
        Self::new()
    }
}

impl HevcDecoder {
    pub fn new() -> Self {
        Self {
            session: None,
            format: None,
            rx: mpsc::channel().1,
            pending: None,
            last_vps: Vec::new(),
            last_sps: Vec::new(),
            last_pps: Vec::new(),
        }
    }

    pub fn is_hardware_supported() -> bool {
        DecompressionSession::is_hardware_decode_supported(Codec::HEVC)
    }

    /// 解码一帧 AnnexB HEVC。关键帧（含 VPS/SPS/PPS）触发 format description 重建。
    pub fn decode_annexb(
        &mut self,
        data: &[u8],
        _pts: i64,
    ) -> Result<Option<CVPixelBuffer>, String> {
        let nals = split_annexb(data);
        if nals.is_empty() {
            return Ok(None);
        }

        let mut vps: Option<&[u8]> = None;
        let mut sps: Option<&[u8]> = None;
        let mut pps: Option<&[u8]> = None;
        for nal in &nals {
            match hevc_type(nal) {
                32 => vps = Some(nal),
                33 => sps = Some(nal),
                34 => pps = Some(nal),
                _ => {}
            }
        }

        // 关键帧（VPS/SPS/PPS 齐）：参数集变化（或首帧）才重建 format description + session。
        if let (Some(vps), Some(sps), Some(pps)) = (vps, sps, pps) {
            let unchanged = self.session.is_some()
                && self.last_vps == vps
                && self.last_sps == sps
                && self.last_pps == pps;
            if !unchanged {
                let fmt = build_hevc_format_description(vps, sps, pps)?;
                let (tx, rx) = mpsc::channel();
                let session = DecompressionSession::new(&fmt, move |frame: DecodedFrame| {
                    let _ = tx.send(frame);
                })
                .map_err(|e| format!("hevc decompress session: {e:?}"))?;
                self.format = Some(fmt);
                self.session = Some(session);
                self.rx = rx;
                self.last_vps = vps.to_vec();
                self.last_sps = sps.to_vec();
                self.last_pps = pps.to_vec();
            }
        }

        // AVCC 化 VCL NAL（0..=31）。VPS/SPS/PPS 已在 format description 中；
        // AUD/SEI 等非 VCL（>=32）必须剔除（同 H.264 经验）。
        let mut avcc = Vec::with_capacity(data.len() + 8);
        for nal in &nals {
            if hevc_type(nal) > 31 {
                continue;
            }
            let len = (nal.len() as u32).to_be_bytes();
            avcc.extend_from_slice(&len);
            avcc.extend_from_slice(nal);
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

        // 丢弃上一帧超时后残留的旧回调帧（防旧帧污染当前帧）。
        while self.rx.try_recv().is_ok() {}
        session
            .decode(sample)
            .map_err(|e| format!("hevc decode: {e:?}"))?;

        // 超时 150ms 并报 Err（供上层触发 PLI），而非静默 None。
        match self.rx.recv_timeout(std::time::Duration::from_millis(150)) {
            Ok(frame) => Ok(frame.image_buffer),
            Err(e) => Err(format!("hevc decode timeout: {e:?}")),
        }
    }
}

/// 核心 `Decoder` 实现：H.264 硬解（trait 路径输出 RGBA raw；Swift 仍走 FFI）。
impl aerodesk_core::platform::Decoder for H264Decoder {
    type Error = String;

    fn configure(
        &mut self,
        _codec: aerodesk_core::platform::Codec,
        _w: u32,
        _h: u32,
    ) -> Result<(), Self::Error> {
        Ok(())
    }

    fn decode(
        &mut self,
        unit: &aerodesk_core::platform::EncodedUnit,
    ) -> Result<Option<aerodesk_core::platform::VideoFrame>, Self::Error> {
        let pts_us = unit.pts_ms.saturating_mul(1000) as i64;
        match self.decode_annexb(&unit.data, pts_us) {
            Ok(Some(buf)) => {
                Ok(
                    to_rgba(&buf).map(|(raw, w, h)| aerodesk_core::platform::VideoFrame {
                        platform: None,
                        handle: None,
                        raw: Some(raw),
                        width: w as u32,
                        height: h as u32,
                        pts_ms: unit.pts_ms,
                    }),
                )
            }
            Ok(None) => Ok(None),
            Err(e) => Err(e),
        }
    }
}

/// 核心 `Decoder` 实现：HEVC/H.265 硬解（trait 路径输出 RGBA raw）。
impl aerodesk_core::platform::Decoder for HevcDecoder {
    type Error = String;

    fn configure(
        &mut self,
        _codec: aerodesk_core::platform::Codec,
        _w: u32,
        _h: u32,
    ) -> Result<(), Self::Error> {
        Ok(())
    }

    fn decode(
        &mut self,
        unit: &aerodesk_core::platform::EncodedUnit,
    ) -> Result<Option<aerodesk_core::platform::VideoFrame>, Self::Error> {
        let pts_us = unit.pts_ms.saturating_mul(1000) as i64;
        match self.decode_annexb(&unit.data, pts_us) {
            Ok(Some(buf)) => {
                Ok(
                    to_rgba(&buf).map(|(raw, w, h)| aerodesk_core::platform::VideoFrame {
                        platform: None,
                        handle: None,
                        raw: Some(raw),
                        width: w as u32,
                        height: h as u32,
                        pts_ms: unit.pts_ms,
                    }),
                )
            }
            Ok(None) => Ok(None),
            Err(e) => Err(e),
        }
    }
}

// ---------- UI 渲染辅助：CVPixelBuffer → RGBA ----------

/// BT.601 YCbCr → RGB（支持有限/全范围；供 to_rgba 与单测使用）。
/// full_range=true：Y 0..255、Cb/Cr 128±112 直接映射；
/// full_range=false（'420v' 视频/有限范围）：Y 16..235、Cb/Cr 16..240 先缩放。
pub fn yuv_to_rgb(y: f32, u: f32, v: f32, full_range: bool) -> (u8, u8, u8) {
    let (y_scale, y_off, c_scale) = if full_range {
        (1.0, 0.0, 1.0)
    } else {
        (255.0 / 219.0, 16.0, 255.0 / 224.0)
    };
    let yv = (y - y_off) * y_scale;
    let uc = (u - 128.0) * c_scale;
    let vc = (v - 128.0) * c_scale;
    let r = (yv + 1.402 * vc).clamp(0.0, 255.0) as u8;
    let g = (yv - 0.344136 * uc - 0.714136 * vc).clamp(0.0, 255.0) as u8;
    let b = (yv + 1.772 * uc).clamp(0.0, 255.0) as u8;
    (r, g, b)
}

/// 把解码输出的 CVPixelBuffer 转成 RGBA（供 Slint Image::from_rgba8 / FFI）。
/// 支持 BGRA（'BGRA'）与 NV12（'420f' 全范围 / '420v' 视频有限范围，BT.601）。
/// 返回 (rgba, width, height)。
pub fn to_rgba(buf: &CVPixelBuffer) -> Option<(Vec<u8>, usize, usize)> {
    let w = buf.width();
    let h = buf.height();
    if w == 0 || h == 0 {
        return None;
    }
    let guard = buf.lock_read_only().ok()?;
    let base = guard.base_address();
    if base.is_null() {
        return None;
    }
    let fmt = buf.pixel_format();
    const BGRA: u32 = 0x42475241;
    const NV12_F: u32 = 0x32342066; // '420f' 全范围
    const NV12_V: u32 = 0x34323076; // '420v' 视频/有限范围（VideoToolbox 解码默认）
    match fmt {
        BGRA => {
            let stride = buf.bytes_per_row();
            let mut rgba = vec![0u8; w * h * 4];
            for y in 0..h {
                let row = unsafe { std::slice::from_raw_parts(base.add(y * stride), w * 4) };
                let out = &mut rgba[y * w * 4..(y + 1) * w * 4];
                for x in 0..w {
                    out[x * 4] = row[x * 4 + 2];
                    out[x * 4 + 1] = row[x * 4 + 1];
                    out[x * 4 + 2] = row[x * 4];
                    out[x * 4 + 3] = 255;
                }
            }
            Some((rgba, w, h))
        }
        NV12_F | NV12_V => {
            let y_stride = buf.bytes_per_row_of_plane(0);
            let uv_stride = buf.bytes_per_row_of_plane(1);
            let y_plane = unsafe {
                std::slice::from_raw_parts(guard.base_address_of_plane(0)?, y_stride * h)
            };
            let uv_plane = unsafe {
                let uv_base = guard.base_address_of_plane(1)?;
                std::slice::from_raw_parts(uv_base, uv_stride * buf.height_of_plane(1))
            };
            let mut rgba = vec![0u8; w * h * 4];
            let full_range = fmt == NV12_F;
            for y in 0..h {
                for x in 0..w {
                    let yv = y_plane[y * y_stride + x] as f32;
                    let uv_off = (y / 2) * uv_stride + (x / 2) * 2;
                    let (r, g, b) = yuv_to_rgb(
                        yv,
                        uv_plane[uv_off] as f32,
                        uv_plane[uv_off + 1] as f32,
                        full_range,
                    );
                    let o = (y * w + x) * 4;
                    rgba[o] = r;
                    rgba[o + 1] = g;
                    rgba[o + 2] = b;
                    rgba[o + 3] = 255;
                }
            }
            Some((rgba, w, h))
        }
        _ => None,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use aerodesk_codec::softenc::encode::X264Encoder;

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
    fn detect_codec_from_parameter_sets() {
        // H.264 SPS/PPS 关键帧头
        let h264 = [0, 0, 0, 1, 0x67, 0x64, 1, 2, 3, 0, 0, 1, 0x68, 0xee, 0x00];
        assert_eq!(detect_codec(&h264), Some(DecoderKind::H264));
        // HEVC VPS/SPS/PPS 关键帧头（2 字节 NAL 头：40 01 = VPS）
        let hevc = [
            0, 0, 0, 1, 0x40, 0x01, 0x42, 0x01, 0x44, 0x01, 0x00, 0, 0, 1, 0x26, 0x01,
        ];
        assert_eq!(detect_codec(&hevc), Some(DecoderKind::Hevc));
        // 纯 P 帧无参数集 → None（等关键帧）
        assert_eq!(detect_codec(&[0, 0, 0, 1, 0x02, 0xaa]), None);
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

    /// #277 跨平台抽象：泛型 Decoder 驱动 VideoToolbox H.264 硬解。
    #[test]
    fn generic_decoder_trait_decodes_h264() {
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

        let mut enc = X264Encoder::new(320, 180, 30, 500).expect("x264");
        let mut dec = H264Decoder::new();
        let mut frame = vec![0u8; 320 * 180 * 3];
        let mut units = Vec::new();
        for i in 0..8u32 {
            for (j, px) in frame.iter_mut().enumerate() {
                *px = (i * 30 + (j as u32 / 100)) as u8;
            }
            if let Some(out) = enc.encode(&frame).expect("encode") {
                units.push(aerodesk_core::platform::EncodedUnit {
                    data: out.data,
                    keyframe: out.keyframe,
                    pts_ms: (i * 33) as u64,
                    rtp_timestamp: 0,
                });
            }
        }
        let n = count_frames(&mut dec, &units);
        assert!(n >= 1, "泛型 Decoder 应解出帧，got {n}");
    }

    #[test]
    fn decodes_hevc_frames() {
        // libx265/hevc_videotoolbox 关键帧 + P 帧 → VideoToolbox HEVC 硬解。
        // macOS 宿主机上 videotoolbox 可解 HEVC（iOS 同 API）。
        use aerodesk_codec::encode::FfmpegEncoder;
        use aerodesk_core::platform::Codec;
        let mut enc =
            FfmpegEncoder::new(320, 180, 30, 1_000_000, Codec::Hevc).expect("hevc encoder");
        enc.request_keyframe();
        let mut decoder = HevcDecoder::new();
        let mut frame = vec![0u8; 320 * 180 * 4];
        let mut decoded_frames = 0;
        let mut saw_keyframe = false;

        for i in 0..8 {
            for (j, px) in frame.iter_mut().enumerate() {
                *px = (i * 30 + j / 100) as u8;
            }
            let Some(unit) = enc.encode_bgra(&frame).expect("encode") else {
                continue;
            };
            if unit.keyframe {
                saw_keyframe = true;
            }
            match decoder.decode_annexb(&unit.data, (i * 3000) as i64) {
                Ok(Some(pb)) => {
                    decoded_frames += 1;
                    assert!(pb.width() > 0 && pb.height() > 0, "decoded buffer has size");
                }
                Ok(None) => {}
                Err(e) => panic!("hevc decode error: {e}"),
            }
        }
        assert!(saw_keyframe, "expected at least one HEVC keyframe");
        assert!(
            decoded_frames >= 1,
            "expected HEVC frames decoded, got {decoded_frames}"
        );
    }

    #[test]
    fn full_range_black_and_white() {
        let (r, g, b) = yuv_to_rgb(0.0, 128.0, 128.0, true);
        assert_eq!((r, g, b), (0, 0, 0));
        let (r, g, b) = yuv_to_rgb(255.0, 128.0, 128.0, true);
        assert_eq!((r, g, b), (255, 255, 255));
    }

    #[test]
    fn limited_range_maps_16_235_to_0_255() {
        // '420v' 有限范围：Y=16 黑、Y=235 白（不缩放会发灰）。
        let (r, g, b) = yuv_to_rgb(16.0, 128.0, 128.0, false);
        assert!(
            r <= 2 && g <= 2 && b <= 2,
            "limited black should be ~0, got {r},{g},{b}"
        );
        let (r, g, b) = yuv_to_rgb(235.0, 128.0, 128.0, false);
        assert!(
            r >= 253 && g >= 253 && b >= 253,
            "limited white should be ~255, got {r},{g},{b}"
        );
    }

    #[test]
    fn full_range_red_chroma() {
        // BT.601 全范围红：Y=76, Cb=84, Cr=255 → R≈255, G/B≈0
        let (r, g, b) = yuv_to_rgb(76.0, 84.0, 255.0, true);
        assert!(r >= 250, "R should be ~255, got {r}");
        assert!(g <= 5 && b <= 5, "G/B should be ~0, got {g},{b}");
    }
}
