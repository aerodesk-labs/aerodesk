//! AeroDesk Linux 适配器（P4 骨架）。
//!
//! 角色：被控端 + 观看端。
//! - 采集：PipeWire Portal（Wayland）/ XRandR+XShm（X11 回退）
//! - 编码：VAAPI（H.264/AV1）硬编；解码：VAAPI/VDPAU
//! - 注入：XTest（X11）/ uinput（Wayland）
//!
//! 平台 API 通过 FFI（libpipewire / libva / libX11 / libevdev），
//! 本骨架先定 trait 与数据流，Linux 真机阶段补齐实现。

pub mod capture;
pub mod encode;
pub mod inject;

/// 采集帧（RGBA，与编码器输入对齐）。
#[derive(Debug, Clone)]
pub struct CapturedFrame {
    pub rgba: Vec<u8>,
    pub width: u32,
    pub height: u32,
    pub pts_us: i64,
}
