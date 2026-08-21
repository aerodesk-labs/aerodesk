//! VAAPI 硬件编解码（Linux，FFmpeg `AVHWDeviceContext` 直连 /dev/dri）。
//!
//! 编码：`h264_vaapi` / `hevc_vaapi` / `av1_vaapi`（按可用性逐个尝试），
//! 输入 core 约定的 BGRA32 → NV12（软缩放）→ VAAPI surface → 编码包。
//! 解码：设置 `hw_device_ctx` 后由 FFmpeg 自动选 VAAPI hwaccel，
//! 收到 VAAPI surface 后 `av_hwframe_transfer_data` 回读 NV12 → RGBA。
//!
//! 本模块仅 Linux 编译；设备不可用（无 /dev/dri、驱动缺失）时 `new()` 返回
//! Err，由上层回退到软编/软解（aerodesk-codec::softenc）。
//!
//! 设备路径：`AERODESK_VAAPI_DEVICE` 环境变量覆盖，默认 `/dev/dri/renderD128`。

use std::collections::VecDeque;
use std::ffi::CString;

use ffmpeg_next as ffmpeg;
use ffmpeg_next::codec::packet::Packet;
use ffmpeg_next::format::Pixel;
use ffmpeg_next::frame::Video;
use ffmpeg_next::software::scaling::{Context as ScalingContext, flag::Flags as ScalingFlags};

use aerodesk_core::platform::VideoFrame;
use aerodesk_core::platform::{Codec, EncodedUnit};

fn vaapi_device_path() -> String {
    std::env::var("AERODESK_VAAPI_DEVICE").unwrap_or_else(|_| "/dev/dri/renderD128".to_string())
}

fn ffmpeg_init() {
    use std::sync::Once;
    static INIT: Once = Once::new();
    INIT.call_once(|| {
        let _ = ffmpeg::init();
    });
}

/// RAII 包装 `AVBufferRef*`（device_ctx / hw_frames_ctx）。
struct BufferRef(*mut ffmpeg::ffi::AVBufferRef);

impl BufferRef {
    fn new(ptr: *mut ffmpeg::ffi::AVBufferRef) -> Self {
        Self(ptr)
    }

    fn as_ptr(&self) -> *mut ffmpeg::ffi::AVBufferRef {
        self.0
    }
}

impl Drop for BufferRef {
    fn drop(&mut self) {
        if !self.0.is_null() {
            unsafe {
                ffmpeg::ffi::av_buffer_unref(&mut self.0);
            }
        }
    }
}

// SAFETY: AVBufferRef 是引用计数的不透明对象；本模块只在单线程调用链中使用，
// 且 BufferRef 通过 Drop 精确 unref，跨线程移动不会破坏计数。
unsafe impl Send for BufferRef {}

/// 创建 VAAPI 硬件设备。
fn create_device() -> Result<BufferRef, String> {
    let path = CString::new(vaapi_device_path())
        .map_err(|_| "vaapi device path contains NUL".to_string())?;
    let mut device: *mut ffmpeg::ffi::AVBufferRef = std::ptr::null_mut();
    let ret = unsafe {
        ffmpeg::ffi::av_hwdevice_ctx_create(
            &mut device,
            ffmpeg::ffi::AVHWDeviceType::AV_HWDEVICE_TYPE_VAAPI,
            path.as_ptr(),
            std::ptr::null_mut(),
            0,
        )
    };
    if ret < 0 || device.is_null() {
        return Err(format!(
            "vaapi device create ({}): {}",
            vaapi_device_path(),
            ffmpeg::Error::from(ret)
        ));
    }
    Ok(BufferRef::new(device))
}

/// 为编码器创建 VAAPI hw_frames_ctx（sw_format=NV12，encoder 侧上传用）。
fn create_encoder_frames(device: &BufferRef, width: u32, height: u32) -> Result<BufferRef, String> {
    unsafe {
        let mut frames = ffmpeg::ffi::av_hwframe_ctx_alloc(device.as_ptr());
        if frames.is_null() {
            return Err("vaapi hwframe_ctx_alloc failed".into());
        }
        let hwfc = (*frames).data as *mut ffmpeg::ffi::AVHWFramesContext;
        if hwfc.is_null() {
            ffmpeg::ffi::av_buffer_unref(&mut frames);
            return Err("vaapi hwframe ctx data is null".into());
        }
        (*hwfc).format = ffmpeg::format::Pixel::VAAPI.into();
        (*hwfc).sw_format = ffmpeg::format::Pixel::NV12.into();
        (*hwfc).width = width as libc::c_int;
        (*hwfc).height = height as libc::c_int;
        (*hwfc).initial_pool_size = 16;
        let ret = ffmpeg::ffi::av_hwframe_ctx_init(frames);
        if ret < 0 {
            ffmpeg::ffi::av_buffer_unref(&mut frames);
            return Err(format!(
                "vaapi hwframe_ctx_init: {}",
                ffmpeg::Error::from(ret)
            ));
        }
        Ok(BufferRef::new(frames))
    }
}

/// 编码器名（按可用性尝试；VAAPI 无则返回 Err 由上层回退软编）。
fn vaapi_encoder_names(codec: Codec) -> &'static [&'static str] {
    match codec {
        Codec::H264 => &["h264_vaapi"],
        Codec::Hevc => &["hevc_vaapi"],
        Codec::Av1 => &["av1_vaapi"],
        _other => &[], // VP9 VAAPI 编码器不通用，保持软编
    }
}

/// VAAPI H.264/HEVC/AV1 硬编码器（BGRA 输入 → AnnexB/OBU 编码包）。
pub struct VaapiEncoder {
    encoder: ffmpeg_next::encoder::Video,
    scaler: ScalingContext,
    /// 编码器内部缓冲（VAAPI 驱动/FFmpeg 有延迟时按序补出）。
    pending: VecDeque<EncodedUnit>,
    frames: BufferRef,
    _device: BufferRef,
    width: u32,
    height: u32,
    fps: u32,
    pts: i64,
    keyframe_pending: bool,
    codec: Codec,
}

impl VaapiEncoder {
    pub fn new(
        width: u32,
        height: u32,
        fps: u32,
        bitrate_bps: u64,
        codec: Codec,
    ) -> Result<Self, String> {
        ffmpeg_init();
        let names = vaapi_encoder_names(codec);
        if names.is_empty() {
            return Err(format!("VAAPI encoder unsupported codec: {codec:?}"));
        }
        let mut last_err = String::new();
        for name in names {
            match Self::open_named(name, width, height, fps, bitrate_bps, codec) {
                Ok(enc) => {
                    tracing::info!(
                        "vaapi encoder opened: {name} ({}x{}@{} {bitrate_bps}bps)",
                        width,
                        height,
                        fps
                    );
                    return Ok(enc);
                }
                Err(e) => {
                    last_err = format!("{name}: {e}");
                    tracing::warn!("vaapi encoder {name} failed: {e}");
                }
            }
        }
        Err(format!(
            "no VAAPI encoder available for {codec:?}: {last_err}"
        ))
    }

    fn open_named(
        name: &str,
        width: u32,
        height: u32,
        fps: u32,
        bitrate_bps: u64,
        codec: Codec,
    ) -> Result<Self, String> {
        let codec_obj = ffmpeg::encoder::find_by_name(name)
            .ok_or_else(|| format!("encoder not found: {name}"))?;
        let device = create_device()?;
        let frames = create_encoder_frames(&device, width, height)?;

        let mut ctx = ffmpeg::codec::context::Context::new_with_codec(codec_obj);
        {
            let p = unsafe { &mut *ctx.as_mut_ptr() };
            // SAFETY: AVBufferRef 引用计数递增；与 BufferRef/AVCodecContext 各自持有一份引用，
            // 在 Drop/avcodec_free_context 时对称释放。
            p.hw_device_ctx = unsafe { ffmpeg::ffi::av_buffer_ref(device.as_ptr()) };
            p.hw_frames_ctx = unsafe { ffmpeg::ffi::av_buffer_ref(frames.as_ptr()) };
        }
        let mut venc = ctx
            .encoder()
            .video()
            .map_err(|e| format!("encoder video: {e}"))?;
        venc.set_width(width);
        venc.set_height(height);
        venc.set_format(Pixel::VAAPI);
        venc.set_time_base(ffmpeg::Rational(1, fps as i32));
        venc.set_frame_rate(Some(ffmpeg::Rational(fps as i32, 1)));
        venc.set_bit_rate(bitrate_bps as usize);
        // 不设 rc_mode：驱动/平台对 CBR 支持不一，留 FFmpeg 默认（避免 open 失败）。
        let encoder = venc.open().map_err(|e| format!("encoder open: {e}"))?;

        let scaler = ScalingContext::get(
            Pixel::BGRA,
            width,
            height,
            Pixel::NV12,
            width,
            height,
            ScalingFlags::BILINEAR,
        )
        .map_err(|e| format!("scaler: {e}"))?;

        Ok(Self {
            encoder,
            scaler,
            pending: VecDeque::new(),
            frames,
            _device: device,
            width,
            height,
            fps,
            pts: 0,
            keyframe_pending: false,
            codec,
        })
    }

    /// 编码一帧 BGRA32（core `VideoFrame.raw` 约定），返回编码包。
    pub fn encode_bgra(&mut self, bgra: &[u8]) -> Result<Option<EncodedUnit>, String> {
        if bgra.len() != (self.width * self.height * 4) as usize {
            return Err("bgra size mismatch".into());
        }
        let mut src = Video::new(Pixel::BGRA, self.width, self.height);
        unsafe {
            std::ptr::copy_nonoverlapping(bgra.as_ptr(), src.data_mut(0).as_mut_ptr(), bgra.len());
        }
        let mut nv12 = Video::empty();
        self.scaler
            .run(&src, &mut nv12)
            .map_err(|e| format!("scale: {e}"))?;

        // 申请 VAAPI surface 并上传 NV12。
        let mut hw = Video::empty();
        let ret =
            unsafe { ffmpeg::ffi::av_hwframe_get_buffer(self.frames.as_ptr(), hw.as_mut_ptr(), 0) };
        if ret < 0 {
            return Err(format!(
                "av_hwframe_get_buffer: {}",
                ffmpeg::Error::from(ret)
            ));
        }
        let ret =
            unsafe { ffmpeg::ffi::av_hwframe_transfer_data(hw.as_mut_ptr(), nv12.as_ptr(), 0) };
        if ret < 0 {
            return Err(format!(
                "av_hwframe_transfer_data: {}",
                ffmpeg::Error::from(ret)
            ));
        }

        hw.set_pts(Some(self.pts));
        if self.keyframe_pending {
            hw.set_kind(ffmpeg::picture::Type::I);
            self.keyframe_pending = false;
        }
        self.encoder
            .send_frame(&hw)
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

    pub fn request_keyframe(&mut self) {
        self.keyframe_pending = true;
    }

    pub fn codec(&self) -> Codec {
        self.codec
    }
}

impl Drop for VaapiEncoder {
    fn drop(&mut self) {
        let _ = self.encoder.send_eof();
    }
}

impl std::fmt::Debug for VaapiEncoder {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("VaapiEncoder")
            .field("codec", &self.codec)
            .field("width", &self.width)
            .field("height", &self.height)
            .field("fps", &self.fps)
            .finish()
    }
}

impl aerodesk_core::platform::Encoder for VaapiEncoder {
    type Error = String;

    fn configure(
        &mut self,
        codec: Codec,
        width: u32,
        height: u32,
        fps: u32,
    ) -> Result<(), Self::Error> {
        // VAAPI 参数不可原地变更；重建（默认 1.5Mbps 与软编一致）。
        *self = VaapiEncoder::new(width, height, fps, 1_500_000, codec)?;
        Ok(())
    }

    fn encode(&mut self, frame: &VideoFrame) -> Result<Option<EncodedUnit>, Self::Error> {
        let Some(raw) = &frame.raw else {
            return Err("vaapi encoder requires raw BGRA frame".into());
        };
        self.encode_bgra(raw)
    }

    fn request_keyframe(&mut self) {
        self.request_keyframe();
    }

    fn set_bitrate(&mut self, _bitrate_bps: u64, _fps: u32) {
        // VAAPI 码率在 open 时固定；宿主可用 configure 重建。
    }
}

fn decoder_id(codec: Codec) -> ffmpeg::codec::Id {
    match codec {
        Codec::H264 => ffmpeg::codec::Id::H264,
        Codec::Hevc => ffmpeg::codec::Id::HEVC,
        Codec::Vp9 => ffmpeg::codec::Id::VP9,
        Codec::Av1 => ffmpeg::codec::Id::AV1,
        other => panic!("vaapi decoder unsupported codec: {other:?}"),
    }
}

/// VAAPI 硬解码器（H.264/HEVC/VP9/AV1 → RGBA）。
pub struct VaapiDecoder {
    decoder: ffmpeg_next::decoder::Video,
    scaler: Option<ScalingContext>,
    _device: BufferRef,
    width: u32,
    height: u32,
    codec: Codec,
}

impl VaapiDecoder {
    pub fn new(codec: Codec) -> Result<Self, String> {
        ffmpeg_init();
        let id = decoder_id(codec);
        let ffmpeg_codec =
            ffmpeg::decoder::find(id).ok_or_else(|| format!("decoder not found: {id:?}"))?;
        let device = create_device()?;

        let mut decoder_ctx =
            ffmpeg::codec::context::Context::new_with_codec(ffmpeg_codec).decoder();
        {
            let p = unsafe { &mut *decoder_ctx.as_mut_ptr() };
            // SAFETY: AVBufferRef 引用计数递增；decoder context 在 avcodec_free_context 时释放。
            p.hw_device_ctx = unsafe { ffmpeg::ffi::av_buffer_ref(device.as_ptr()) };
        }
        let decoder = decoder_ctx
            .video()
            .map_err(|e| format!("vaapi decoder open: {e}"))?;

        Ok(Self {
            decoder,
            scaler: None,
            _device: device,
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

    /// 解码一个编码单元；硬件 surface → NV12 → RGBA。
    pub fn decode_unit(&mut self, unit: &EncodedUnit) -> Result<Option<VideoFrame>, String> {
        let mut packet = Packet::new(unit.data.len());
        if let Some(d) = packet.data_mut() {
            d.copy_from_slice(&unit.data);
        }
        self.decoder
            .send_packet(&packet)
            .map_err(|e| format!("send_packet: {e}"))?;
        let mut hw = Video::empty();
        match self.decoder.receive_frame(&mut hw) {
            Ok(()) => {
                // 硬件帧回读到软件 NV12。
                let mut sw = Video::empty();
                let ret = unsafe {
                    ffmpeg::ffi::av_hwframe_transfer_data(sw.as_mut_ptr(), hw.as_ptr(), 0)
                };
                if ret < 0 {
                    return Err(format!(
                        "av_hwframe_transfer_data: {}",
                        ffmpeg::Error::from(ret)
                    ));
                }
                let w = hw.width() as usize;
                let h = hw.height() as usize;
                if self.scaler.is_none() {
                    self.scaler = Some(
                        ScalingContext::get(
                            sw.format(),
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
                let mut rgba_video = Video::empty();
                self.scaler
                    .as_mut()
                    .unwrap()
                    .run(&sw, &mut rgba_video)
                    .map_err(|e| format!("scale: {e}"))?;
                let mut raw = vec![0u8; w * h * 4];
                let src = rgba_video.data(0);
                unsafe {
                    std::ptr::copy_nonoverlapping(src.as_ptr(), raw.as_mut_ptr(), raw.len());
                }
                self.width = w as u32;
                self.height = h as u32;
                Ok(Some(VideoFrame {
                    platform: None,
                    handle: None,
                    raw: Some(raw),
                    width: w as u32,
                    height: h as u32,
                    pts_ms: unit.pts_ms,
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

impl std::fmt::Debug for VaapiDecoder {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("VaapiDecoder")
            .field("codec", &self.codec)
            .field("width", &self.width)
            .field("height", &self.height)
            .finish()
    }
}

impl aerodesk_core::platform::Decoder for VaapiDecoder {
    type Error = String;

    fn configure(&mut self, codec: Codec, _width: u32, _height: u32) -> Result<(), Self::Error> {
        if codec != self.codec {
            *self = VaapiDecoder::new(codec)?;
        }
        Ok(())
    }

    fn decode(&mut self, unit: &EncodedUnit) -> Result<Option<VideoFrame>, Self::Error> {
        self.decode_unit(unit)
    }
}

/// VAAPI 设备是否可用（运行级测试/上层选路用）。
pub fn vaapi_available() -> bool {
    create_device().is_ok()
}

#[cfg(test)]
mod tests {
    use super::*;

    /// 无 /dev/dri（CI 容器）时跳过；有设备时验证编码器可打开。
    #[test]
    fn encoder_opens_when_device_present() {
        if !vaapi_available() {
            eprintln!("SKIP: 无 VAAPI 设备");
            return;
        }
        let enc = VaapiEncoder::new(320, 180, 30, 800_000, Codec::H264).expect("vaapi encoder");
        assert_eq!(enc.width, 320);
        assert_eq!(enc.height, 180);
        assert_eq!(enc.codec(), Codec::H264);
    }

    /// 无设备时 new() 必须返回 Err（供上层回退软编，而不是 panic）。
    #[test]
    fn encoder_fails_cleanly_without_device() {
        if vaapi_available() {
            eprintln!("SKIP: 有 VAAPI 设备（Err 分支无法稳定触发）");
            return;
        }
        let err = VaapiEncoder::new(320, 180, 30, 800_000, Codec::H264).unwrap_err();
        assert!(err.contains("vaapi"), "错误信息应说明 VAAPI 不可用: {err}");
    }
}
