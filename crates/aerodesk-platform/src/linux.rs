//! AeroDesk Linux 适配器（P4 骨架）。
//!
//! 角色：被控端 + 观看端。
//! - 采集：PipeWire Portal（Wayland）/ XRandR+XShm（X11 回退）
//! - 编码：VAAPI 硬编（`vaapi` 模块；软编 x264 回退）
//! - 解码：VAAPI 硬解（`vaapi` 模块；软解 OpenH264 回退）
//! - 注入：XTest（X11）/ uinput（Wayland）
//!
//! 平台 API 通过 FFI（libpipewire / libva / libX11 / libevdev）；VAAPI 走
//! FFmpeg `AVHWDeviceContext`（libavutil），uinput 走 `/dev/uinput` ioctl。

/// PipeWire 系统音频采集（仅 Linux + feature `pipewire`）。
#[cfg(all(target_os = "linux", feature = "pipewire"))]
pub mod audio;
/// V4L2 摄像头（仅 Linux）。
#[cfg(target_os = "linux")]
pub mod camera;
pub mod capture;
/// Linux 远程命令执行器（#330「bash」平台抽象）。
pub mod cmd;
/// 被控端光标读取（仅 Linux；X11 QueryPointer，Wayland 无 X11 时返回 None）。
#[cfg(target_os = "linux")]
pub mod cursor;
pub mod encode;
/// 文件选择器（观看端「发送文件」；zenity/kdialog，零依赖）。
pub mod file_picker;
pub mod inject;
/// 系统通知（notify-send）。
pub mod notifier;
/// Wayland portal RemoteDesktop 输入注入（仅 Linux + feature `pipewire`）。
#[cfg(all(target_os = "linux", feature = "pipewire"))]
pub mod portal_inject;
/// VAAPI 硬编/硬解（仅 Linux；设备不可用时上层回退软编/软解）。
#[cfg(target_os = "linux")]
pub mod vaapi;
/// Linux 保持唤醒（#334「SystemWakeLock」平台抽象）。
#[cfg(target_os = "linux")]
pub mod wake_lock;

/// 采集帧（BGRA32，core `VideoFrame.raw` 约定，与编码器输入对齐）。
#[derive(Debug, Clone)]
pub struct CapturedFrame {
    pub bgra: Vec<u8>,
    pub width: u32,
    pub height: u32,
    pub pts_us: i64,
}
