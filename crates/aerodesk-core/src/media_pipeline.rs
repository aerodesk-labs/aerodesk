//! 媒体管线抽象：平台适配器实现，核心只依赖这些 trait。
//!
//! 平台矩阵（见仓库 Wiki「Platform-Roles」）：
//! - Windows: DXGI + NVENC/QSV + WASAPI + SendInput
//! - macOS:   ScreenCaptureKit + VideoToolbox + CGEvent
//! - Linux:   PipeWire + VAAPI + XTest/uinput
//! - Android: MediaProjection + MediaCodec + AccessibilityService
//! - iOS:     ReplayKit（仅观看端）
//! - HarmonyOS: AVScreenCapture + 硬件编码 + OH_Input_*

/// 原始视频帧（平台采集器输出，格式由 codec 决定）。
#[derive(Debug, Clone)]
pub struct VideoFrame {
    /// 平台私有帧句柄（如 NVENC 输入纹理 / MediaCodec buffer）。
    pub handle: Option<u64>,
    /// 原始像素（handle 为空时使用）。
    pub raw: Option<Vec<u8>>,
    pub width: u32,
    pub height: u32,
    pub pts_ms: u64,
}

/// 屏幕/窗口采集源。
pub trait MediaSource {
    type Error: std::error::Error;
    /// 启动采集。参数为区域/帧率/是否含光标。
    fn start(&mut self, fps: u32, with_cursor: bool) -> Result<(), Self::Error>;
    /// 取下一帧（阻塞或回调）。
    fn next_frame(&mut self) -> Result<Option<VideoFrame>, Self::Error>;
    fn stop(&mut self);
}

/// 硬件/软件编码器（H.264 / HEVC / AV1，按平台能力选择）。
pub trait Encoder {
    type Error: std::error::Error;
    fn configure(
        &mut self,
        codec: Codec,
        width: u32,
        height: u32,
        fps: u32,
    ) -> Result<(), Self::Error>;
    /// 编码一帧，产出 RTP 负载前的编码单元（AnnexB/AVCC/OBU...）。
    fn encode(&mut self, frame: &VideoFrame) -> Result<Option<EncodedUnit>, Self::Error>;
    /// 请求关键帧。
    fn request_keyframe(&mut self);
    /// 目标码率/帧率（BitrateController 或远端反馈驱动）。
    fn set_bitrate(&mut self, bitrate_bps: u64, fps: u32);
}

/// 编码输出单元。
#[derive(Debug, Clone)]
pub struct EncodedUnit {
    pub data: Vec<u8>,
    pub keyframe: bool,
    pub pts_ms: u64,
    /// 编码帧对应的 RTP 时间戳增量。
    pub rtp_timestamp: u32,
}

/// 解码器。
pub trait Decoder {
    type Error: std::error::Error;
    fn configure(&mut self, codec: Codec, width: u32, height: u32) -> Result<(), Self::Error>;
    fn decode(&mut self, unit: &EncodedUnit) -> Result<Option<VideoFrame>, Self::Error>;
}

/// 渲染器（观看端）。
pub trait Renderer {
    type Error: std::error::Error;
    fn render(&mut self, frame: &VideoFrame) -> Result<(), Self::Error>;
}

/// 输入注入器（被控端；观看端捕获事件则相反，见 aerodesk-protocol::input）。
pub trait InputInjector {
    type Error: std::error::Error;
    fn inject(&mut self, event: &aerodesk_protocol::input::InputEvent) -> Result<(), Self::Error>;
}

/// 编码格式。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Codec {
    H264,
    Hevc,
    Av1,
    Vp8,
    Vp9,
}
