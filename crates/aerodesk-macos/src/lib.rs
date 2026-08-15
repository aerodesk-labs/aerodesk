//! AeroDesk macOS 平台适配器（兼容 re-export 壳）。
//!
//! 实现已迁移至 [`aerodesk_platform::macos`]；本 crate 保留原包名，
//! 供现有消费方（aerodesk-cli / aerodesk-desktop）无感过渡。
#![cfg(target_os = "macos")]

pub use aerodesk_platform::macos::*;
