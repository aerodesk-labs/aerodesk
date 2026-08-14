//! AeroDesk Windows 适配器（被控端 + 观看端）。
//!
//! 角色：被控端 + 观看端。
//! - 采集：Windows Graphics Capture（Win11）/ DXGI Desktop Duplication（Win10 回退）
//! - 编码：Media Foundation H.264/HEVC（或 NVENC/QSV/AMF）
//! - 注入：SendInput / mouse_event + keybd_event
//! - 虚拟显示器：Parsec VDD（ADR-0001，vdd 模块）
//!
//! `windows` crate 依赖仅 Windows 目标启用；非 Windows 主机上本 crate 编译为
//! 纯 trait 骨架（用于 workspace 测试与文档）。

#[cfg(windows)]
pub mod audio_capture;
#[cfg(windows)]
pub mod autostart;
pub mod camera;
pub mod capture;
pub mod cursor;
pub mod decode;
pub mod encode;
pub mod inject;
pub mod vdd;
#[cfg(windows)]
pub mod wake_lock;

/// 采集帧（BGRA，DXGI 输出格式）。
#[derive(Debug, Clone)]
pub struct CapturedFrame {
    pub bgra: Vec<u8>,
    pub width: u32,
    pub height: u32,
    pub pts_us: i64,
}
