//! AVScreenCapture 采集（骨架）。
//!
//! 真机路径：`OH_AVScreenCapture`（屏幕采集）+ `OH_VideoEncoder`（编码），
//! 采集回调 → 编码器 → RTP。需要 `ohos.permission.CAPTURE_SCREEN`。

/// 采集帧（RGBA）。
#[derive(Debug, Clone)]
pub struct CapturedFrame {
    pub rgba: Vec<u8>,
    pub width: u32,
    pub height: u32,
    pub pts_us: i64,
}

/// 采集器抽象（被控端）。
pub trait ScreenCapturer {
    fn next_frame(&mut self) -> Option<CapturedFrame>;
}

/// TODO(P4): OH_AVScreenCapture 实现（OpenHarmony SDK + ohos target）。
pub struct AvScreenCapturer;

impl AvScreenCapturer {
    pub fn new() -> Result<Self, String> {
        Err("ohos: AVScreenCapture not implemented yet (P4)".into())
    }
}

impl ScreenCapturer for AvScreenCapturer {
    fn next_frame(&mut self) -> Option<CapturedFrame> {
        None
    }
}
