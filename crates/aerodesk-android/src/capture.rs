//! MediaProjection 屏幕采集（骨架）。
//!
//! 真机路径：`MediaProjectionManager.createScreenCaptureIntent` → 用户授权 →
//! `VirtualDisplay` + `ImageReader` → RGBA 帧 → 编码器（MediaCodec H.264/AV1）。

use crate::CapturedFrame;

/// TODO(P3): MediaProjection + ImageReader 实现。
pub struct MediaProjectionCapturer;

impl MediaProjectionCapturer {
    pub fn new() -> Result<Self, String> {
        Err("android: MediaProjection not implemented yet (P3)".into())
    }
}

impl aerodesk_core::platform::MediaSource for MediaProjectionCapturer {
    type Error = String;

    fn start(&mut self, _fps: u32, _with_cursor: bool) -> Result<(), Self::Error> {
        Ok(())
    }

    fn next_frame(&mut self) -> Result<Option<aerodesk_core::platform::VideoFrame>, Self::Error> {
        Ok(None)
    }

    fn stop(&mut self) {}
}
