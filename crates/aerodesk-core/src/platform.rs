//! 平台适配抽象：核心只依赖这些 trait，平台差异收敛在各适配器 crate。
//!
//! 平台矩阵（见仓库 Wiki「Platform-Roles」）：
//! - macOS:   ScreenCaptureKit + VideoToolbox + CGEvent + SCK Audio
//! - Windows: DXGI + NVENC/QSV + WASAPI + SendInput
//! - Linux:   PipeWire + VAAPI + XTest/uinput
//! - Android: MediaProjection + MediaCodec + AccessibilityService
//! - iOS:     ReplayKit（仅观看端）
//! - HarmonyOS: AVScreenCapture + 硬件编码 + OH_Input_*
//!
//! 所有实现方必须实现本模块 trait；禁止在各平台 crate 重复定义同名 trait。

use std::any::Any;
use std::sync::Arc;

/// 原始视频帧（平台采集器输出）。
///
/// 三通道并存，按平台能力选一：
/// - `platform`：零拷贝平台帧对象（macOS IOSurface，编码器直接下转使用，不拷贝）
/// - `handle`：平台私有帧句柄（如 NVENC 输入纹理 / MediaCodec buffer id）
/// - `raw`：原始像素，**统一 BGRA32 约定**（无零拷贝通道时使用；
///   macOS/Win DXGI/合成源均 BGRA，Linux 采集在适配器内转 BGRA）
#[derive(Clone)]
pub struct VideoFrame {
    /// 平台零拷贝帧对象（实现方负责 downcast；无则 None）。
    pub platform: Option<Arc<dyn Any + Send>>,
    /// 平台私有帧句柄（NVENC 纹理 / MediaCodec buffer）。
    pub handle: Option<u64>,
    /// 原始像素（platform/handle 都为空时使用）。
    pub raw: Option<Vec<u8>>,
    pub width: u32,
    pub height: u32,
    pub pts_ms: u64,
}

/// 屏幕/窗口采集源（被控端）。
pub trait MediaSource {
    type Error: std::fmt::Display + std::fmt::Debug;
    /// 启动采集。参数为区域/帧率/是否含光标。
    fn start(&mut self, fps: u32, with_cursor: bool) -> Result<(), Self::Error>;
    /// 取下一帧（阻塞或回调）。
    fn next_frame(&mut self) -> Result<Option<VideoFrame>, Self::Error>;
    fn stop(&mut self);
}

/// 硬件/软件编码器（H.264 / HEVC / AV1，按平台能力选择）。
pub trait Encoder {
    type Error: std::fmt::Display + std::fmt::Debug;
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

/// 解码器（观看端）。
pub trait Decoder {
    type Error: std::fmt::Display + std::fmt::Debug;
    fn configure(&mut self, codec: Codec, width: u32, height: u32) -> Result<(), Self::Error>;
    fn decode(&mut self, unit: &EncodedUnit) -> Result<Option<VideoFrame>, Self::Error>;
}

/// 渲染器（观看端）。
pub trait Renderer {
    type Error: std::fmt::Display + std::fmt::Debug;
    fn render(&mut self, frame: &VideoFrame) -> Result<(), Self::Error>;
}

/// 输入注入器（被控端；观看端捕获事件则相反，见 aerodesk-protocol::input）。
pub trait InputInjector {
    type Error: std::fmt::Display + std::fmt::Debug;
    fn inject(&mut self, event: &aerodesk_protocol::input::InputEvent) -> Result<(), Self::Error>;
}

/// 音频播放（观看端）。
pub trait AudioSink {
    fn push_pcm(&mut self, samples: &[i16]);
    fn set_muted(&mut self, muted: bool);
}

/// 音频采集（被控端：系统音频 / 麦克风，输出单声道 f32 样本流）。
pub trait AudioCapturer {
    type Error: std::fmt::Display + std::fmt::Debug;
    /// 排空最多 `max` 个样本（单声道 f32）。
    fn next_samples(&mut self, max: usize) -> Result<Vec<f32>, Self::Error>;
}

/// 系统剪贴板文本读写（双向同步）。
pub trait Clipboard {
    type Error: std::fmt::Display + std::fmt::Debug;
    fn read_text(&mut self) -> Result<Option<String>, Self::Error>;
    fn write_text(&mut self, text: &str) -> Result<(), Self::Error>;
}

/// 光标位置源（被控端：真实光标归一化坐标 0..1，供观看端叠加层）。
pub trait CursorSource {
    fn position_normalized(&mut self) -> Option<(f64, f64)>;
}

/// 便捷 re-export：`use aerodesk_core::platform::*` 同时拿到 Codec/EncodedUnit。
pub use crate::media_pipeline::{Codec, EncodedUnit};
