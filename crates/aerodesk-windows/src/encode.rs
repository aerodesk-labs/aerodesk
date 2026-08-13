//! 编码器：Media Foundation H.264 硬编优先，OpenH264 软编回退。
//!
//! Windows 被控端编码链：DXGI 输出 BGRA32 → `bgra_to_nv12` → Media Foundation
//! H.264 Encoder MFT（优先枚举硬件 MFT，失败再枚举软件 MFT）→ AnnexB 码流。
//! MF 初始化或编码器创建失败时，由宿主（aerodesk-cli）回退 OpenH264 软编。

pub use aerodesk_softenc::EncodedFrame;
pub use aerodesk_softenc::openh264enc::OpenH264Encoder as SoftEncoder;

#[cfg(windows)]
use aerodesk_core::media_pipeline::{Codec, EncodedUnit};
#[cfg(windows)]
use aerodesk_core::platform::{Encoder, VideoFrame};

/// Media Foundation H.264 编码器（仅 Windows）。
///
/// 非 Windows 目标上保留同签名骨架，`new` 返回 `Err`，保证 workspace 全平台可编译。
#[cfg(windows)]
pub struct MfH264Encoder {
    transform: windows::Win32::Media::MediaFoundation::IMFTransform,
    width: u32,
    height: u32,
    fps: u32,
    bitrate_bps: u64,
    header: Option<Vec<u8>>,
    header_sent: bool,
    force_keyframe: bool,
}

#[cfg(not(windows))]
pub struct MfH264Encoder;

#[cfg(windows)]
impl MfH264Encoder {
    /// `bitrate_kbps` 与 OpenH264 软编构造参数保持一致（CLI 使用 8_000）。
    pub fn new(width: u32, height: u32, fps: u32, bitrate_kbps: u64) -> Result<Self, String> {
        use windows::Win32::Media::MediaFoundation::{
            MF_MT_AVG_BITRATE, MF_MT_FRAME_RATE, MF_MT_FRAME_SIZE, MF_MT_INTERLACE_MODE,
            MF_MT_MAJOR_TYPE, MF_MT_SUBTYPE, MFCreateMediaType, MFMediaType_Video,
            MFT_ENUM_FLAG_ASYNCMFT, MFT_ENUM_FLAG_HARDWARE, MFT_ENUM_FLAG_SORTANDFILTER,
            MFT_ENUM_FLAG_SYNCMFT, MFT_MESSAGE_NOTIFY_START_OF_STREAM, MFT_REGISTER_TYPE_INFO,
            MFVideoFormat_H264, MFVideoFormat_NV12, MFVideoInterlace_Progressive,
        };

        if width == 0 || height == 0 || fps == 0 {
            return Err(
                "Media Foundation H.264 encoder requires non-zero dimensions and fps".into(),
            );
        }

        ensure_mf_startup()?;
        let bitrate_bps = bitrate_kbps.saturating_mul(1_000);

        // 优先硬件 MFT；无硬件编码器时回退到系统软件 MFT。
        let input_info = MFT_REGISTER_TYPE_INFO {
            guidMajorType: MFMediaType_Video,
            guidSubtype: MFVideoFormat_NV12,
        };
        let output_info = MFT_REGISTER_TYPE_INFO {
            guidMajorType: MFMediaType_Video,
            guidSubtype: MFVideoFormat_H264,
        };
        let transform = unsafe {
            match enumerate_h264_encoder(
                MFT_ENUM_FLAG_HARDWARE
                    | MFT_ENUM_FLAG_SYNCMFT
                    | MFT_ENUM_FLAG_ASYNCMFT
                    | MFT_ENUM_FLAG_SORTANDFILTER,
                &input_info,
                &output_info,
            ) {
                Some(t) => t,
                None => enumerate_h264_encoder(
                    MFT_ENUM_FLAG_SYNCMFT | MFT_ENUM_FLAG_ASYNCMFT | MFT_ENUM_FLAG_SORTANDFILTER,
                    &input_info,
                    &output_info,
                )
                .ok_or("no Media Foundation H.264 encoder MFT available")?,
            }
        };

        // 输入：NV12。CLI/DXGI 输出 BGRA32，进入 MFT 前在 encode() 内转换。
        let input_type =
            unsafe { MFCreateMediaType() }.map_err(|e| format!("MFCreateMediaType(input): {e}"))?;
        unsafe {
            input_type
                .SetGUID(&MF_MT_MAJOR_TYPE, &MFMediaType_Video)
                .and_then(|_| input_type.SetGUID(&MF_MT_SUBTYPE, &MFVideoFormat_NV12))
                .and_then(|_| {
                    input_type.SetUINT64(&MF_MT_FRAME_SIZE, ((width as u64) << 32) | height as u64)
                })
                .and_then(|_| input_type.SetUINT64(&MF_MT_FRAME_RATE, ((fps as u64) << 32) | 1))
                .and_then(|_| {
                    input_type
                        .SetUINT32(&MF_MT_INTERLACE_MODE, MFVideoInterlace_Progressive.0 as u32)
                })
                .map_err(|e| format!("configure input media type: {e}"))?;
            transform
                .SetInputType(0, &input_type, 0)
                .map_err(|e| format!("SetInputType: {e}"))?;
        }

        // 输出：H.264 AnnexB（MF_MT_MPEG_SEQUENCE_HEADER 提供 SPS/PPS，按需前置）。
        let output_type = unsafe { MFCreateMediaType() }
            .map_err(|e| format!("MFCreateMediaType(output): {e}"))?;
        unsafe {
            output_type
                .SetGUID(&MF_MT_MAJOR_TYPE, &MFMediaType_Video)
                .and_then(|_| output_type.SetGUID(&MF_MT_SUBTYPE, &MFVideoFormat_H264))
                .and_then(|_| {
                    output_type.SetUINT64(&MF_MT_FRAME_SIZE, ((width as u64) << 32) | height as u64)
                })
                .and_then(|_| output_type.SetUINT64(&MF_MT_FRAME_RATE, ((fps as u64) << 32) | 1))
                .and_then(|_| output_type.SetUINT32(&MF_MT_AVG_BITRATE, bitrate_bps as u32))
                .and_then(|_| {
                    output_type
                        .SetUINT32(&MF_MT_INTERLACE_MODE, MFVideoInterlace_Progressive.0 as u32)
                })
                .map_err(|e| format!("configure output media type: {e}"))?;
            transform
                .SetOutputType(0, &output_type, 0)
                .map_err(|e| format!("SetOutputType: {e}"))?;
        }

        let header = unsafe { read_sequence_header(&transform) };

        unsafe {
            transform
                .ProcessMessage(MFT_MESSAGE_NOTIFY_START_OF_STREAM, 0)
                .map_err(|e| format!("ProcessMessage(START_OF_STREAM): {e}"))?;
        }

        Ok(Self {
            transform,
            width,
            height,
            fps,
            bitrate_bps,
            header,
            header_sent: false,
            force_keyframe: false,
        })
    }

    fn encode_bgra(
        &mut self,
        bgra: &[u8],
        width: u32,
        height: u32,
        pts_ms: u64,
    ) -> Result<Option<EncodedUnit>, String> {
        use windows::Win32::Media::MediaFoundation::{
            MF_E_TRANSFORM_NEED_MORE_INPUT, MFCreateMemoryBuffer, MFCreateSample,
            MFT_MESSAGE_NOTIFY_START_OF_STREAM, MFT_OUTPUT_DATA_BUFFER,
            MFT_OUTPUT_DATA_BUFFER_NO_SAMPLE, MFT_OUTPUT_STREAM_PROVIDES_SAMPLES,
        };
        use windows::core::Interface;

        if width != self.width || height != self.height {
            return Err(format!(
                "frame size {width}x{height} does not match encoder {}x{}",
                self.width, self.height
            ));
        }
        if bgra.len() < (width as usize) * (height as usize) * 4 {
            return Err("BGRA frame buffer is too small".into());
        }

        let nv12 = bgra_to_nv12(bgra, width, height);
        let input_sample =
            unsafe { MFCreateSample() }.map_err(|e| format!("MFCreateSample(input): {e}"))?;
        let input_buffer = unsafe { MFCreateMemoryBuffer(nv12.len() as u32) }
            .map_err(|e| format!("MFCreateMemoryBuffer(input): {e}"))?;
        unsafe {
            let mut ptr = std::ptr::null_mut();
            let mut max_len = 0u32;
            input_buffer
                .Lock(&mut ptr, Some(&mut max_len), None)
                .map_err(|e| format!("Lock(input buffer): {e}"))?;
            std::ptr::copy_nonoverlapping(nv12.as_ptr(), ptr, nv12.len());
            input_buffer
                .Unlock()
                .map_err(|e| format!("Unlock(input buffer): {e}"))?;
            input_buffer
                .SetCurrentLength(nv12.len() as u32)
                .map_err(|e| format!("SetCurrentLength(input buffer): {e}"))?;
            input_sample
                .AddBuffer(&input_buffer)
                .map_err(|e| format!("AddBuffer(input sample): {e}"))?;
            input_sample
                .SetSampleDuration(10_000_000 / self.fps as i64)
                .map_err(|e| format!("SetSampleDuration(input sample): {e}"))?;
            if self.force_keyframe {
                // 尽力请求 IDR；实际是否出关键帧仍由 NAL type 5 判定。
                let _ = self
                    .transform
                    .ProcessMessage(MFT_MESSAGE_NOTIFY_START_OF_STREAM, 0);
                self.force_keyframe = false;
            }
            self.transform
                .ProcessInput(0, &input_sample, 0)
                .map_err(|e| format!("ProcessInput: {e}"))?;
        }

        let out_info = unsafe { self.transform.GetOutputStreamInfo(0) }
            .map_err(|e| format!("GetOutputStreamInfo: {e}"))?;
        let output_sample =
            if (out_info.dwFlags & (MFT_OUTPUT_STREAM_PROVIDES_SAMPLES.0 as u32)) == 0 {
                let output_size = (self.width as usize * self.height as usize * 3 / 2)
                    .max(1_048_576)
                    .min(u32::MAX as usize) as u32;
                let sample = unsafe { MFCreateSample() }
                    .map_err(|e| format!("MFCreateSample(output): {e}"))?;
                let output_buffer = unsafe { MFCreateMemoryBuffer(output_size) }
                    .map_err(|e| format!("MFCreateMemoryBuffer(output): {e}"))?;
                unsafe {
                    sample
                        .AddBuffer(&output_buffer)
                        .map_err(|e| format!("AddBuffer(output sample): {e}"))?;
                }
                Some(sample)
            } else {
                None
            };

        let mut out_buf = MFT_OUTPUT_DATA_BUFFER {
            dwStreamID: 0,
            pSample: std::mem::ManuallyDrop::new(output_sample),
            dwStatus: 0,
            pEvents: std::mem::ManuallyDrop::new(None),
        };
        let mut status = 0u32;
        let process_result =
            unsafe { self.transform.ProcessOutput(0, &mut [out_buf], &mut status) };
        if let Err(e) = process_result {
            if e.code() == MF_E_TRANSFORM_NEED_MORE_INPUT {
                return Ok(None);
            }
            return Err(format!("ProcessOutput: {e}"));
        }

        if (status & (MFT_OUTPUT_DATA_BUFFER_NO_SAMPLE.0 as u32)) != 0 {
            return Ok(None);
        }
        let sample = match (*out_buf.pSample).as_ref() {
            Some(sample) => sample.clone(),
            None => return Ok(None),
        };
        let buffer = unsafe { sample.ConvertToContiguousBuffer() }
            .map_err(|e| format!("ConvertToContiguousBuffer: {e}"))?;
        let nal = unsafe {
            let mut ptr = std::ptr::null_mut();
            let mut max_len = 0u32;
            let mut current_len = 0u32;
            buffer
                .Lock(&mut ptr, Some(&mut max_len), Some(&mut current_len))
                .map_err(|e| format!("Lock(output buffer): {e}"))?;
            let bytes = std::slice::from_raw_parts(ptr, current_len as usize).to_vec();
            buffer
                .Unlock()
                .map_err(|e| format!("Unlock(output buffer): {e}"))?;
            bytes
        };

        let mut data = ensure_annexb(&nal);
        let keyframe = is_annexb_keyframe(&data);
        if keyframe && !self.header_sent {
            if let Some(header) = &self.header {
                let mut prefixed = ensure_annexb(header);
                prefixed.extend_from_slice(&data);
                data = prefixed;
            }
            self.header_sent = true;
        }

        Ok(Some(EncodedUnit {
            data,
            keyframe,
            pts_ms,
            rtp_timestamp: 0,
        }))
    }
}

#[cfg(windows)]
impl Encoder for MfH264Encoder {
    type Error = String;

    fn configure(
        &mut self,
        codec: Codec,
        width: u32,
        height: u32,
        fps: u32,
    ) -> Result<(), Self::Error> {
        if codec != Codec::H264 {
            return Err("Media Foundation H.264 encoder only supports H264".into());
        }
        if width != self.width || height != self.height || fps != self.fps {
            return Err(format!(
                "Media Foundation H.264 encoder does not support dynamic reconfiguration ({}x{}@{} -> {}x{}@{})",
                self.width, self.height, self.fps, width, height, fps
            ));
        }
        Ok(())
    }

    fn encode(&mut self, frame: &VideoFrame) -> Result<Option<EncodedUnit>, Self::Error> {
        let Some(raw) = frame.raw.as_deref() else {
            return Err("Media Foundation H.264 encoder requires BGRA raw frames".into());
        };
        self.encode_bgra(raw, frame.width, frame.height, frame.pts_ms)
    }

    fn request_keyframe(&mut self) {
        self.force_keyframe = true;
    }

    fn set_bitrate(&mut self, bitrate_bps: u64, _fps: u32) {
        // 当前 MF MFT 的目标码率在 SetOutputType 时协商；运行期动态码率需要 ICodecAPI，
        // windows crate 未绑定该接口，这里仅更新记录值，保持 trait 兼容。
        self.bitrate_bps = bitrate_bps;
    }
}

#[cfg(not(windows))]
impl MfH264Encoder {
    /// 非 Windows 骨架：Media Foundation 仅 Windows 可用。
    pub fn new(_width: u32, _height: u32, _fps: u32, _bitrate_kbps: u64) -> Result<Self, String> {
        Err("Media Foundation H.264 encoder is only available on Windows".into())
    }
}

#[cfg(not(windows))]
impl aerodesk_core::platform::Encoder for MfH264Encoder {
    type Error = String;

    fn configure(
        &mut self,
        _codec: aerodesk_core::media_pipeline::Codec,
        _width: u32,
        _height: u32,
        _fps: u32,
    ) -> Result<(), Self::Error> {
        Err("Media Foundation H.264 encoder is only available on Windows".into())
    }

    fn encode(
        &mut self,
        _frame: &aerodesk_core::platform::VideoFrame,
    ) -> Result<Option<aerodesk_core::media_pipeline::EncodedUnit>, Self::Error> {
        Err("Media Foundation H.264 encoder is only available on Windows".into())
    }

    fn request_keyframe(&mut self) {}

    fn set_bitrate(&mut self, _bitrate_bps: u64, _fps: u32) {}
}

#[cfg(windows)]
fn ensure_mf_startup() -> Result<(), String> {
    use std::sync::OnceLock;
    use windows::Win32::Media::MediaFoundation::{MF_VERSION, MFSTARTUP_FULL, MFStartup};
    use windows::Win32::System::Com::{COINIT_MULTITHREADED, CoInitializeEx};

    static MF_STARTUP: OnceLock<Result<(), String>> = OnceLock::new();
    MF_STARTUP
        .get_or_init(|| unsafe {
            // MF 在 CLI 进程中只需初始化一次。若当前线程已初始化 STA，返回
            // RPC_E_CHANGED_MODE；本编码器要求 MTA，忽略该结果继续 MFStartup。
            let _ = CoInitializeEx(None, COINIT_MULTITHREADED);
            MFStartup(MF_VERSION, MFSTARTUP_FULL).map_err(|e| format!("MFStartup: {e}"))
        })
        .clone()
}

#[cfg(windows)]
unsafe fn enumerate_h264_encoder(
    flags: windows::Win32::Media::MediaFoundation::MFT_ENUM_FLAG,
    input_info: &windows::Win32::Media::MediaFoundation::MFT_REGISTER_TYPE_INFO,
    output_info: &windows::Win32::Media::MediaFoundation::MFT_REGISTER_TYPE_INFO,
) -> Option<windows::Win32::Media::MediaFoundation::IMFTransform> {
    use windows::Win32::Media::MediaFoundation::MFTEnumEx;
    use windows::Win32::System::Com::CoTaskMemFree;

    let mut activates: *mut Option<windows::Win32::Media::MediaFoundation::IMFActivate> =
        std::ptr::null_mut();
    let mut count = 0u32;
    // SAFETY: MFTEnumEx 输出由 CoTaskMemFree 释放；actives 为 null 时不会解引用。
    if MFTEnumEx(
        windows::Win32::Media::MediaFoundation::MFT_CATEGORY_VIDEO_ENCODER,
        flags,
        Some(input_info as *const _),
        Some(output_info as *const _),
        &mut activates,
        &mut count,
    )
    .is_err()
    {
        return None;
    }
    // SAFETY: MFTEnumEx 成功后 activates 指向 count 个 IMFActivate 数组。
    let first = if count > 0 {
        std::slice::from_raw_parts(activates, count as usize)
            .first()
            .and_then(|activate| activate.as_ref().cloned())
    } else {
        None
    };
    // SAFETY: 释放 MFTEnumEx 分配的数组。
    CoTaskMemFree(Some(activates as *const core::ffi::c_void));
    first.and_then(|activate| {
        // SAFETY: IMFActivate::ActivateObject 返回指定接口，调用后 COM 自动管理引用。
        activate
            .ActivateObject::<windows::Win32::Media::MediaFoundation::IMFTransform>()
            .ok()
    })
}

#[cfg(windows)]
unsafe fn read_sequence_header(
    transform: &windows::Win32::Media::MediaFoundation::IMFTransform,
) -> Option<Vec<u8>> {
    let output_type = transform.GetOutputCurrentType(0).ok()?;
    let size = output_type
        .GetBlobSize(&windows::Win32::Media::MediaFoundation::MF_MT_MPEG_SEQUENCE_HEADER)
        .ok()?;
    if size == 0 {
        return None;
    }
    let mut header = vec![0u8; size as usize];
    output_type
        .GetBlob(
            &windows::Win32::Media::MediaFoundation::MF_MT_MPEG_SEQUENCE_HEADER,
            &mut header,
            None,
        )
        .ok()?;
    Some(header)
}

#[cfg(windows)]
fn is_annexb_keyframe(data: &[u8]) -> bool {
    let mut i = 0usize;
    while i + 4 <= data.len() {
        if data[i..].starts_with(&[0, 0, 0, 1]) {
            return i + 5 <= data.len() && (data[i + 4] & 0x1f) == 5;
        }
        if data[i..].starts_with(&[0, 0, 1]) {
            return i + 4 <= data.len() && (data[i + 3] & 0x1f) == 5;
        }
        i += 1;
    }
    false
}

/// BGRA32（DXGI 输出）→ NV12（BT.601 limited range）。
///
/// 不涉及平台 API，可跨平台单测。输入按 `BGRA` 字节顺序。
pub fn bgra_to_nv12(bgra: &[u8], width: u32, height: u32) -> Vec<u8> {
    let w = width as usize;
    let h = height as usize;
    if bgra.len() < w * h * 4 {
        return Vec::new();
    }
    let uv_width = (w + 1) / 2;
    let uv_height = (h + 1) / 2;
    let uv_stride = w;
    let mut nv12 = vec![0u8; w * h + uv_stride * uv_height];

    for y in 0..h {
        for x in 0..w {
            let i = (y * w + x) * 4;
            let b = bgra[i] as i32;
            let g = bgra[i + 1] as i32;
            let r = bgra[i + 2] as i32;
            nv12[y * w + x] = (((66 * r + 129 * g + 25 * b + 128) >> 8) + 16).clamp(0, 255) as u8;
        }
    }

    for by in 0..uv_height {
        for bx in 0..uv_width {
            let mut r_sum = 0i32;
            let mut g_sum = 0i32;
            let mut b_sum = 0i32;
            let mut count = 0i32;
            for dy in 0..2 {
                let y = (by * 2 + dy).min(h - 1);
                for dx in 0..2 {
                    let x = (bx * 2 + dx).min(w - 1);
                    let i = (y * w + x) * 4;
                    r_sum += bgra[i + 2] as i32;
                    g_sum += bgra[i + 1] as i32;
                    b_sum += bgra[i] as i32;
                    count += 1;
                }
            }
            if count == 0 {
                continue;
            }
            let r = r_sum / count;
            let g = g_sum / count;
            let b = b_sum / count;
            let u = (((112 * b - 74 * g - 38 * r + 128) >> 8) + 128).clamp(0, 255) as u8;
            let v = (((112 * r - 94 * g - 18 * b + 128) >> 8) + 128).clamp(0, 255) as u8;
            let dst = w * h + by * uv_stride + bx * 2;
            nv12[dst] = u;
            nv12[dst + 1] = v;
        }
    }

    nv12
}

/// 将 MF H.264 输出规范化为 AnnexB（str0m 依赖 start code）。
///
/// 若数据已带 `00 00 01` / `00 00 00 01` 起始码，原样返回；
/// 否则按 AVCC 4 字节长度前缀拆 NAL 并补起始码；无法识别时退化为单 NAL。
pub fn ensure_annexb(data: &[u8]) -> Vec<u8> {
    if data.starts_with(&[0, 0, 1]) || data.starts_with(&[0, 0, 0, 1]) {
        return data.to_vec();
    }
    if data.len() < 4 {
        let mut out = Vec::with_capacity(data.len() + 4);
        out.extend_from_slice(&[0, 0, 0, 1]);
        out.extend_from_slice(data);
        return out;
    }

    let first_len = u32::from_be_bytes([data[0], data[1], data[2], data[3]]) as usize;
    if first_len == 0 || first_len + 4 > data.len() {
        let mut out = Vec::with_capacity(data.len() + 4);
        out.extend_from_slice(&[0, 0, 0, 1]);
        out.extend_from_slice(data);
        return out;
    }

    let mut out = Vec::with_capacity(data.len() + 8);
    let mut pos = 0usize;
    while pos + 4 <= data.len() {
        let nal_len =
            u32::from_be_bytes([data[pos], data[pos + 1], data[pos + 2], data[pos + 3]]) as usize;
        if nal_len == 0 {
            break;
        }
        let nal_start = pos + 4;
        if nal_start + nal_len > data.len() {
            break;
        }
        out.extend_from_slice(&[0, 0, 0, 1]);
        out.extend_from_slice(&data[nal_start..nal_start + nal_len]);
        pos = nal_start + nal_len;
    }
    if out.is_empty() {
        let mut fallback = Vec::with_capacity(data.len() + 4);
        fallback.extend_from_slice(&[0, 0, 0, 1]);
        fallback.extend_from_slice(data);
        return fallback;
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn bgra_to_nv12_has_expected_planes() {
        let width = 4u32;
        let height = 2u32;
        let bgra = [128u8, 128, 128, 255].repeat((width * height) as usize);
        let nv12 = bgra_to_nv12(&bgra, width, height);
        assert_eq!(nv12.len(), (width as usize) * (height as usize) * 3 / 2);
        // 灰色像素不应出现非法色度偏移。
        assert!(nv12.iter().all(|v| (16..=235).contains(v)));
    }

    #[test]
    fn ensure_annexb_passthrough_start_code() {
        let data = [0, 0, 0, 1, 0x65, 0x88];
        assert_eq!(ensure_annexb(&data), data.to_vec());
    }

    #[test]
    fn ensure_annexb_converts_avcc_length_prefix() {
        let data = [0, 0, 0, 2, 0x65, 0x88, 0, 0, 0, 2, 0x41, 0x9a];
        assert_eq!(
            ensure_annexb(&data),
            vec![0, 0, 0, 1, 0x65, 0x88, 0, 0, 0, 1, 0x41, 0x9a]
        );
    }
}
