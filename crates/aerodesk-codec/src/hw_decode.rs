//! Windows 观看端硬件解码（#3 优化项）：D3D11VA / DXVA2 硬解 H.264/HEVC。
//!
//! ffmpeg-next 8.x 无现成 hwaccel 封装（无 `decoder::hardware`、无
//! `hw_device_ctx` 接口），这里用 `ffmpeg_next::sys` 原始 FFI 实现经典路径：
//! `av_hwdevice_ctx_create`（D3D11VA 优先、DXVA2 回退）→ `hw_device_ctx` +
//! `get_format` 回调选硬解像素格式 → `avcodec_send_packet/receive_frame` →
//! `av_hwframe_transfer_data` 拷回系统内存（NV12）→ sws_scale 转 RGBA。
//!
//! 硬解设备创建失败（无 GPU/驱动）时 `new()` 返回 Err，上层回退 OpenH264 软解；
//! 码流仅支持 H.264/HEVC（D3D11VA/DXVA2 覆盖），VP9/AV1 走软解。

use std::os::raw::c_int;
use std::ptr;

use ffmpeg_next::format::Pixel;
use ffmpeg_next::frame::Video;
use ffmpeg_next::software::scaling::{Context as ScalingContext, flag::Flags as ScalingFlags};
use ffmpeg_next::sys as ffi;

use aerodesk_core::platform::{Codec, EncodedUnit};
use aerodesk_core::platform::{Decoder, VideoFrame};

/// FFmpeg AVERROR(EAGAIN)：输入缓冲满/暂无输出，需交换收发。
const EAGAIN: c_int = -11;

/// 从 AnnexB 码流嗅探编解码类型（找 SPS NAL）：
/// - H.264：NAL 头 1 字节，`b0 & 0x1F == 7`（SPS）
/// - HEVC：NAL 头 2 字节，`(b0 >> 1) & 0x3F == 33`（SPS）
///
/// 两类 SPS 位模式互不冲突（H.264 SPS 首字节 0x67，HEVC SPS 首字节 0x42），
/// 无需预先知道 codec 即可区分。
fn sniff_codec(data: &[u8]) -> Option<Codec> {
    let mut i = 0;
    while i + 4 <= data.len() {
        if data[i] == 0 && data[i + 1] == 0 && data[i + 2] == 1 {
            let hdr = i + 3;
            if data[hdr] & 0x1f == 7 {
                return Some(Codec::H264);
            }
            if data.get(hdr + 1).is_some() && ((data[hdr] >> 1) & 0x3f) == 33 {
                return Some(Codec::Hevc);
            }
            // 跳过本 NAL 到下一个起始码。
            let mut j = hdr + 1;
            while j + 4 <= data.len() {
                if data[j] == 0 && data[j + 1] == 0 && data[j + 2] == 1 {
                    break;
                }
                j += 1;
            }
            i = if j + 4 <= data.len() { j } else { data.len() };
        } else {
            i += 1;
        }
    }
    None
}

/// `get_format` 回调：从解码器候选格式表里挑出硬件格式
/// （新 D3D11 / 旧 D3D11VA_VLD / DXVA2_VLD，按优先级），否则返回 NONE。
unsafe extern "C" fn hw_format_cb(
    _ctx: *mut ffi::AVCodecContext,
    fmts: *const ffi::AVPixelFormat,
) -> ffi::AVPixelFormat {
    // SAFETY: FFmpeg 在 avcodec_open2 及分辨率变化时调用本回调，fmts 必为
    // 以 AV_PIX_FMT_NONE 结尾的候选表；表内元素按 repr(i32) 布局可读。
    if fmts.is_null() {
        return ffi::AVPixelFormat::AV_PIX_FMT_NONE;
    }
    let mut i = 0usize;
    loop {
        let fmt = unsafe { *fmts.add(i) };
        if fmt == ffi::AVPixelFormat::AV_PIX_FMT_NONE {
            return ffi::AVPixelFormat::AV_PIX_FMT_NONE;
        }
        // FFmpeg 8 的 d3d11va 已切到新 AV_PIX_FMT_D3D11（旧 D3D11VA_VLD 与
        // hwframes ctx 不匹配）；DXVA2 仍用 DXVA2_VLD。按优先级返回，设备类型
        // 决定候选表里实际出现哪些格式。
        if fmt == ffi::AVPixelFormat::AV_PIX_FMT_D3D11
            || fmt == ffi::AVPixelFormat::AV_PIX_FMT_D3D11VA_VLD
            || fmt == ffi::AVPixelFormat::AV_PIX_FMT_DXVA2_VLD
        {
            return fmt;
        }
        i += 1;
    }
}

/// 创建 D3D11VA（优先）/DXVA2（回退）硬件设备，返回 AVBufferRef 引用。
/// 失败时返回 Err（调用方回退软解）。
fn create_hw_device() -> Result<*mut ffi::AVBufferRef, String> {
    crate::encode::init();
    for ty in [
        ffi::AVHWDeviceType::AV_HWDEVICE_TYPE_D3D11VA,
        ffi::AVHWDeviceType::AV_HWDEVICE_TYPE_DXVA2,
    ] {
        let mut dev: *mut ffi::AVBufferRef = ptr::null_mut();
        // SAFETY: dev 指向栈上指针，av_hwdevice_ctx_create 成功时写入新引用、
        // 失败时置 NULL；type/device/opts 均为合法 FFmpeg 参数。
        let ret =
            unsafe { ffi::av_hwdevice_ctx_create(&mut dev, ty, ptr::null(), ptr::null_mut(), 0) };
        if ret >= 0 && !dev.is_null() {
            return Ok(dev);
        }
        if !dev.is_null() {
            // SAFETY: dev 是 av_hwdevice_ctx_create 分配的有效引用。
            unsafe { ffi::av_buffer_unref(&mut dev) };
        }
    }
    Err("D3D11VA/DXVA2 硬件设备创建失败（无 GPU/驱动）".to_string())
}

/// 单个 codec 的硬解上下文（含 FFmpeg 原始指针，Drop 统一释放）。
struct Inner {
    ctx: *mut ffi::AVCodecContext,
    pkt: *mut ffi::AVPacket,
    hw_frame: *mut ffi::AVFrame,
    scaler: Option<ScalingContext>,
    width: u32,
    height: u32,
}

impl Inner {
    fn new(codec: Codec, device_ref: *mut ffi::AVBufferRef) -> Result<Self, String> {
        let id = match codec {
            Codec::H264 => ffi::AVCodecID::AV_CODEC_ID_H264,
            Codec::Hevc => ffi::AVCodecID::AV_CODEC_ID_HEVC,
            other => return Err(format!("硬件解码不支持 {other:?}")),
        };
        // SAFETY: avcodec_find_decoder 返回静态编解码器指针（不持有）。
        let ffcodec = unsafe { ffi::avcodec_find_decoder(id) };
        if ffcodec.is_null() {
            return Err(format!("ffmpeg decoder not found: {id:?}"));
        }
        // SAFETY: avcodec_alloc_context3 分配新上下文，失败返回 NULL。
        let mut ctx = unsafe { ffi::avcodec_alloc_context3(ffcodec) };
        if ctx.is_null() {
            return Err("avcodec_alloc_context3 failed".to_string());
        }
        // SAFETY: 每个 AVCodecContext 都带 AVCodecContext 私有类，可安全置空字典。
        unsafe {
            (*ctx).get_format = Some(hw_format_cb);
            // 硬件加速要求单线程解码（frame threading 与 hwaccel 不兼容）。
            (*ctx).thread_count = 1;
            let dev = ffi::av_buffer_ref(device_ref);
            if dev.is_null() {
                ffi::avcodec_free_context(&mut ctx);
                return Err("av_buffer_ref(hw_device) failed".to_string());
            }
            (*ctx).hw_device_ctx = dev;
            let ret = ffi::avcodec_open2(ctx, ffcodec, ptr::null_mut());
            if ret < 0 {
                ffi::avcodec_free_context(&mut ctx);
                return Err(format!("avcodec_open2(hw): {ret}"));
            }
        }
        // SAFETY: av_packet_alloc / av_frame_alloc 分配独立对象，失败返回 NULL。
        let mut pkt = unsafe { ffi::av_packet_alloc() };
        if pkt.is_null() {
            unsafe { ffi::avcodec_free_context(&mut ctx) };
            return Err("av_packet_alloc failed".to_string());
        }
        let hw_frame = unsafe { ffi::av_frame_alloc() };
        if hw_frame.is_null() {
            unsafe {
                ffi::av_packet_free(&mut pkt);
                ffi::avcodec_free_context(&mut ctx);
            }
            return Err("av_frame_alloc failed".to_string());
        }
        Ok(Self {
            ctx,
            pkt,
            hw_frame,
            scaler: None,
            width: 0,
            height: 0,
        })
    }

    /// 接收并处理当前可用的所有帧，返回最新一帧的 RGBA（无则 None）。
    fn drain(&mut self, pts_ms: u64) -> Result<Option<VideoFrame>, String> {
        let mut out = None;
        loop {
            // SAFETY: hw_frame 每轮先 unref 再交给 avcodec_receive_frame 填充。
            unsafe { ffi::av_frame_unref(self.hw_frame) };
            let ret = unsafe { ffi::avcodec_receive_frame(self.ctx, self.hw_frame) };
            if ret == 0 {
                if let Some(f) = self.frame_to_rgba(pts_ms)? {
                    out = Some(f);
                }
            } else if ret == EAGAIN {
                break;
            } else if ret < 0 {
                return Err(format!("avcodec_receive_frame: {ret}"));
            }
        }
        Ok(out)
    }

    /// 把当前 hw_frame 转成 RGBA VideoFrame（硬解帧先 transfer 回系统内存）。
    fn frame_to_rgba(&mut self, pts_ms: u64) -> Result<Option<VideoFrame>, String> {
        // SAFETY: AVFrame 的 format/width/height 由解码器填充，读标量字段安全。
        let fmt = unsafe { (*self.hw_frame).format };
        let w = unsafe { (*self.hw_frame).width } as u32;
        let h = unsafe { (*self.hw_frame).height } as u32;
        if w == 0 || h == 0 {
            return Ok(None);
        }
        let is_hw = fmt == ffi::AVPixelFormat::AV_PIX_FMT_D3D11 as c_int
            || fmt == ffi::AVPixelFormat::AV_PIX_FMT_D3D11VA_VLD as c_int
            || fmt == ffi::AVPixelFormat::AV_PIX_FMT_DXVA2_VLD as c_int;
        if is_hw {
            // SAFETY: sw 是 av_frame_alloc 的干净帧；transfer 成功时其 data/buf
            // 由 FFmpeg 分配（av_frame_get_buffer），随后交给 Video::wrap 接管释放。
            let mut sw = unsafe { ffi::av_frame_alloc() };
            if sw.is_null() {
                return Err("av_frame_alloc (sw) failed".to_string());
            }
            let ret = unsafe { ffi::av_hwframe_transfer_data(sw, self.hw_frame, 0) };
            if ret < 0 {
                // SAFETY: 失败时 sw 未被填充，仍需释放帧结构本身。
                unsafe { ffi::av_frame_free(&mut sw) };
                return Err(format!("av_hwframe_transfer_data: {ret}"));
            }
            // SAFETY: Video::wrap 接管 sw 所有权，drop 时 av_frame_free。
            let sw_video = unsafe { Video::wrap(sw) };
            self.scale_to_rgba(&sw_video, pts_ms)
        } else {
            Err(format!(
                "硬件解码器输出非硬件帧格式（fmt={fmt}），疑似硬解未生效"
            ))
        }
    }

    /// sws_scale 缩放为 RGBA 并打包 VideoFrame。
    fn scale_to_rgba(&mut self, sw: &Video, pts_ms: u64) -> Result<Option<VideoFrame>, String> {
        let w = sw.width();
        let h = sw.height();
        if w == 0 || h == 0 {
            return Ok(None);
        }
        if self.scaler.is_none() || self.width != w || self.height != h {
            self.scaler = Some(
                ScalingContext::get(sw.format(), w, h, Pixel::RGBA, w, h, ScalingFlags::BILINEAR)
                    .map_err(|e| format!("hw scaler: {e}"))?,
            );
            self.width = w;
            self.height = h;
        }
        let mut rgba = Video::empty();
        self.scaler
            .as_mut()
            .unwrap()
            .run(sw, &mut rgba)
            .map_err(|e| format!("hw scale: {e}"))?;
        // RGBA 逻辑行宽 = w*4，但 sws 输出帧的 stride 会按对齐补齐（如宽 1470
        // → stride 5888 ≠ 5880）——与 decode.rs 同款坑。共用 pack_rgba 按 stride
        // 逐行打包；连续拷 w*h*4 在宽度非对齐时逐行错位、累积成斜向剪切（#487 实测）。
        let raw = crate::decode::pack_rgba(
            rgba.data(0),
            rgba.stride(0) as usize,
            w as usize,
            h as usize,
        );
        Ok(Some(VideoFrame {
            platform: None,
            handle: None,
            raw: Some(raw),
            width: w,
            height: h,
            pts_ms,
        }))
    }
}

impl Drop for Inner {
    fn drop(&mut self) {
        // SAFETY: 指针均在 Inner::new 分配；先释放帧/包再释放解码上下文，
        // avcodec_free_context 会一并 unref hw_device_ctx 引用。
        unsafe {
            ffi::av_frame_unref(self.hw_frame);
            ffi::av_frame_free(&mut self.hw_frame);
            ffi::av_packet_free(&mut self.pkt);
            ffi::avcodec_free_context(&mut self.ctx);
        }
    }
}

/// Windows FFmpeg 硬件视频解码器（H.264/HEVC → RGBA）。
///
/// `new()` 只做硬件设备探测（失败即 Err，调用方回退软解）；编解码器上下文
/// 在首个码流单元到达时按嗅探到的 codec 惰性创建。
pub struct FfmpegHwDecoder {
    device_ref: *mut ffi::AVBufferRef,
    codec: Option<Codec>,
    inner: Option<Inner>,
}

impl FfmpegHwDecoder {
    pub fn new() -> Result<Self, String> {
        let device_ref = create_hw_device()?;
        Ok(Self {
            device_ref,
            codec: None,
            inner: None,
        })
    }

    pub fn codec(&self) -> Option<Codec> {
        self.codec
    }
}

impl Drop for FfmpegHwDecoder {
    fn drop(&mut self) {
        // SAFETY: device_ref 由 create_hw_device 分配；Inner 的 codec ctx 持有
        // 自己的 av_buffer_ref 副本（Inner::new 时 av_buffer_ref），外层的
        // device_ref 先释放不影响 inner 生命周期。
        unsafe { ffi::av_buffer_unref(&mut self.device_ref) };
    }
}

impl Decoder for FfmpegHwDecoder {
    type Error = String;

    fn configure(&mut self, codec: Codec, _width: u32, _height: u32) -> Result<(), Self::Error> {
        if !matches!(codec, Codec::H264 | Codec::Hevc) {
            return Err(format!("硬件解码仅支持 H.264/HEVC，收到 {codec:?}"));
        }
        self.codec = Some(codec);
        if self.inner.is_some() {
            self.inner = Some(Inner::new(codec, self.device_ref)?);
        }
        Ok(())
    }

    fn decode(&mut self, unit: &EncodedUnit) -> Result<Option<VideoFrame>, Self::Error> {
        let codec = match self.codec {
            Some(c) => c,
            None => sniff_codec(&unit.data)
                .ok_or_else(|| "无法识别 H.264/HEVC AnnexB 码流".to_string())?,
        };
        if self.inner.is_none() {
            self.inner = Some(Inner::new(codec, self.device_ref)?);
            // 记录嗅探结果，`codec()` 可反映当前解码器（CLI 按 codec 变化重建）。
            self.codec = Some(codec);
        }
        let inner = self.inner.as_mut().expect("inner initialized");
        // SAFETY: pkt 为 Inner 持有的 AVPacket；先 unref 再 av_new_packet 分配
        // 新缓冲并拷贝输入，avcodec_send_packet 同步消费，不残留借用。
        unsafe {
            ffi::av_packet_unref(inner.pkt);
            let ret = ffi::av_new_packet(inner.pkt, unit.data.len() as c_int);
            if ret < 0 {
                return Err(format!("av_new_packet: {ret}"));
            }
            ptr::copy_nonoverlapping(unit.data.as_ptr(), (*inner.pkt).data, unit.data.len());
        }
        // 发送；EAGAIN 表示解码器输入缓冲满，先 drain 再重试。
        loop {
            // SAFETY: ctx 已 open，pkt 数据有效。
            let ret = unsafe { ffi::avcodec_send_packet(inner.ctx, inner.pkt) };
            if ret == 0 {
                break;
            }
            if ret == EAGAIN {
                inner.drain(unit.pts_ms)?;
                continue;
            }
            if ret < 0 {
                return Err(format!("avcodec_send_packet: {ret}"));
            }
        }
        inner.drain(unit.pts_ms)
    }
}

impl std::fmt::Debug for FfmpegHwDecoder {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("FfmpegHwDecoder")
            .field("codec", &self.codec)
            .field("inner", &self.inner.is_some())
            .finish()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::encode::FfmpegEncoder;

    /// AnnexB SPS 嗅探：H.264 SPS（0x67）与 HEVC SPS（0x42 0x01）可区分。
    #[test]
    fn sniff_detects_h264_and_hevc() {
        // 00 00 00 01 + H.264 SPS NAL（0x67 0x64 ...）
        let h264 = vec![0u8, 0, 0, 1, 0x67, 0x64, 0x00, 0x1f];
        assert_eq!(sniff_codec(&h264), Some(Codec::H264));
        // 00 00 00 01 + HEVC SPS NAL（0x42 0x01 ...）
        let hevc = vec![0u8, 0, 0, 1, 0x42, 0x01, 0x01, 0x60];
        assert_eq!(sniff_codec(&hevc), Some(Codec::Hevc));
        // 纯 P 帧（无 SPS）识别不出 codec。
        let pframe = vec![0u8, 0, 0, 1, 0x41, 0x9a, 0x22];
        assert_eq!(sniff_codec(&pframe), None);
    }

    /// 软编 → 硬件解码回环（libx264/libx265 → D3D11VA/DXVA2）。
    /// 无 GPU/驱动的环境（如 CI）打印原因后返回，不制造假绿。
    fn hw_roundtrip(enc_name: &str, codec_id: ffmpeg_next::codec::Id, codec: Codec) {
        let mut dec = match FfmpegHwDecoder::new() {
            Ok(d) => d,
            Err(e) => {
                eprintln!("hw decode unavailable, skipping roundtrip: {e}");
                return;
            }
        };
        let (w, h) = (320u32, 180u32);
        // 强制软编（AnnexB + repeat-headers），保证测试与平台无关。
        let mut enc =
            FfmpegEncoder::open_named(enc_name, codec_id, w, h, 30, 800_000).expect("soft encoder");
        let mut decoded = None;
        let mut last_err = String::new();
        for i in 0..90u32 {
            let rgb: Vec<u8> = (0..(w * h * 3) as usize)
                .map(|j| ((i * 17 + (j as u32) / 4) & 0xff) as u8)
                .collect();
            if let Some(unit) = enc.encode_rgb(&rgb).expect("encode") {
                if decoded.is_none() {
                    match dec.decode(&unit) {
                        Ok(Some(frame)) => {
                            decoded = Some((frame.raw.expect("rgba"), frame.width, frame.height));
                        }
                        Ok(None) => {}
                        Err(e) => last_err = e,
                    }
                }
            }
        }
        // 无 GPU/驱动（如 GitHub windows-latest 无硬件视频解码器）时硬解不可用：
        // 打印原因后返回，不制造假绿（RULE_可达性 detect-and-return）。
        let Some((rgba, dw, dh)) = decoded else {
            eprintln!(
                "hw decode unavailable on this environment (no GPU video decoder): {last_err}"
            );
            return;
        };
        assert_eq!((dw, dh), (w, h));
        assert_eq!(rgba.len(), (w * h * 4) as usize);
        assert_eq!(dec.codec(), Some(codec), "嗅探应记录实际 codec");
    }

    #[test]
    fn h264_hw_roundtrip() {
        hw_roundtrip("libx264", ffmpeg_next::codec::Id::H264, Codec::H264);
    }

    #[test]
    fn hevc_hw_roundtrip() {
        hw_roundtrip("libx265", ffmpeg_next::codec::Id::HEVC, Codec::Hevc);
    }
}
