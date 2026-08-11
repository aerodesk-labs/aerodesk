//! 摄像头源（前瞻：远端摄像头转发）。
//!
//! macOS 实现依赖 AVFoundation（AVCaptureDevice），当前未接入；定义 core
//! `CameraSource` trait 实现骨架，真机批次（P4）用 AVFoundation 补齐。

/// 核心 `CameraSource` 实现骨架（macOS）。
pub struct MacCamera;

impl aerodesk_core::platform::CameraSource for MacCamera {
    type Error = String;

    fn start(&mut self, _width: u32, _height: u32, _fps: u32) -> Result<(), Self::Error> {
        Err("macos: camera source 依赖 AVFoundation，尚未接入（P4）".into())
    }

    fn next_frame(&mut self) -> Result<Option<aerodesk_core::platform::CameraFrame>, Self::Error> {
        Err("macos: camera source 依赖 AVFoundation，尚未接入（P4）".into())
    }

    fn stop(&mut self) {}
}
