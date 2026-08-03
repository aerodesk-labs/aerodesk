//! AeroDesk macOS 平台适配器。
//!
//! - [`encoder`]：H.264 编码器（x264，AnnexB 输出）
//! - [`synthetic`]：合成测试帧源（彩条 + 移动方块，无需采集权限）
//! - [`inject`]：CGEvent 鼠标/键盘注入（被控端输入）
//!
//! 屏幕采集（ScreenCaptureKit → VideoToolbox 硬编）为 P2 后续项：
//! screen-capture-kit crate 已就绪，接入时注意 macOS 屏幕录制权限（TCC）。

pub mod capture;
pub mod encoder;
pub mod inject;
pub mod permissions;
pub mod synthetic;
pub mod vt_encoder;
