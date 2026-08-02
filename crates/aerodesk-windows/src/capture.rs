//! 屏幕采集（骨架）。
//!
//! 真机路径：`GraphicsCaptureSession`（Windows.Graphics.Capture，Win10 1903+）
//! 或 `IDXGIOutputDuplication`（Win8+ 回退）；D3D11 纹理 → CPU 读回 BGRA。

use crate::CapturedFrame;

/// 采集器抽象（被控端）。
pub trait ScreenCapturer {
    fn next_frame(&mut self) -> Option<CapturedFrame>;
}

/// TODO(P4): Windows.Graphics.Capture / DXGI 实现（需 Windows 构建机验证）。
pub struct GraphicsCaptureCapturer;

impl GraphicsCaptureCapturer {
    pub fn new() -> Result<Self, String> {
        Err("windows: GraphicsCapture not implemented yet (P4)".into())
    }
}

impl ScreenCapturer for GraphicsCaptureCapturer {
    fn next_frame(&mut self) -> Option<CapturedFrame> {
        None
    }
}
