//! 屏幕采集（骨架）。
//!
//! Wayland：`org.freedesktop.portal.ScreenCast`（xdg-desktop-portal）+ PipeWire；
//! X11 回退：XRandR 遍历输出 + XShmGetImage。

use crate::CapturedFrame;

/// 采集器抽象（被控端）。
pub trait ScreenCapturer {
    fn next_frame(&mut self) -> Option<CapturedFrame>;
}

/// TODO(P4): PipeWire/X11 实现。
pub struct PipeWireCapturer;

impl PipeWireCapturer {
    pub fn new() -> Result<Self, String> {
        Err("linux: PipeWire capture not implemented yet (P4)".into())
    }
}

impl ScreenCapturer for PipeWireCapturer {
    fn next_frame(&mut self) -> Option<CapturedFrame> {
        None
    }
}
