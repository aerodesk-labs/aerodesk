//! 媒体管线抽象：平台适配器实现，核心只依赖这些 trait。
//!
//! 平台矩阵（见仓库 Wiki「Platform-Roles」）：
//! - Windows: DXGI + NVENC/QSV + WASAPI + SendInput
//! - macOS:   ScreenCaptureKit + VideoToolbox + CGEvent
//! - Linux:   PipeWire + VAAPI + XTest/uinput
//! - Android: MediaProjection + MediaCodec + AccessibilityService
//! - iOS:     ReplayKit（仅观看端）
//! - HarmonyOS: AVScreenCapture + 硬件编码 + OH_Input_*

/// 平台适配抽象：MediaSource/Encoder/Decoder/Renderer/InputInjector/Audio*/Clipboard/CursorSource
/// 统一收敛在 [`crate::platform`]，本模块只保留媒体数据与编码格式。
pub use crate::platform::{
    AudioCapturer, AudioSink, Clipboard, CursorSource, Decoder, Encoder, InputInjector,
    MediaSource, Renderer, VideoFrame,
};

/// 编码输出单元。
#[derive(Debug, Clone)]
pub struct EncodedUnit {
    pub data: Vec<u8>,
    pub keyframe: bool,
    pub pts_ms: u64,
    /// 编码帧对应的 RTP 时间戳增量。
    pub rtp_timestamp: u32,
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
