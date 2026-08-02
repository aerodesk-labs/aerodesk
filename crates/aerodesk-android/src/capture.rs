//! MediaProjection 屏幕采集（骨架）。
//!
//! 真机路径：`MediaProjectionManager.createScreenCaptureIntent` → 用户授权 →
//! `VirtualDisplay` + `ImageReader` → RGBA 帧 → 编码器（MediaCodec H.264/AV1）。

use crate::CapturedFrame;

/// 采集器抽象（被控端）。
pub trait ScreenCapturer {
    /// 返回下一帧（阻塞或按帧率回调由实现决定）。
    fn next_frame(&mut self) -> Option<CapturedFrame>;
}

/// TODO(P3): MediaProjection + ImageReader 实现。
pub struct MediaProjectionCapturer;

impl MediaProjectionCapturer {
    pub fn new() -> Result<Self, String> {
        Err("android: MediaProjection not implemented yet (P3)".into())
    }
}

impl ScreenCapturer for MediaProjectionCapturer {
    fn next_frame(&mut self) -> Option<CapturedFrame> {
        None
    }
}
