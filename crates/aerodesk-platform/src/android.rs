//! AeroDesk Android 适配器（P3 骨架）。
//!
//! 角色：
//! - **观看端**：MediaCodec H.264/HEVC 硬解（`decode`），Surface 渲染由壳层负责
//! - **被控端**（需用户授权）：MediaProjection 屏幕采集（`capture`）、
//!   AccessibilityService 输入注入（`inject`）
//!
//! 所有平台 API 通过 JNI 桥接（`jni` crate / 手写 extern "C"），
//! 本骨架先定义 trait 与数据流，具体 JNI 实现在真机验证阶段补齐
//! （本机无 Android SDK/NDK，`cargo check --target aarch64-linux-android` 通过即可）。

pub mod capture;
pub mod decode;
pub mod encode;
pub mod inject;

/// 解码帧格式（MediaCodec 输出）。
#[derive(Debug, Clone)]
pub struct DecodedFrame {
    /// YUV420 或 RGBA 数据（取决于 MediaCodec 输出格式）。
    pub data: Vec<u8>,
    pub width: u32,
    pub height: u32,
    pub pts_us: i64,
}

/// 采集帧格式（MediaProjection Image → RGBA）。
#[derive(Debug, Clone)]
pub struct CapturedFrame {
    pub rgba: Vec<u8>,
    pub width: u32,
    pub height: u32,
    pub pts_us: i64,
}
pub mod publisher;
pub mod viewer;
