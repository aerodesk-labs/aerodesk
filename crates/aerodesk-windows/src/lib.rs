//! AeroDesk Windows 平台适配器（兼容 re-export 壳）。
//!
//! 实现已迁移至 [`aerodesk_platform::windows`]；本 crate 保留原包名，
//! 供现有消费方无感过渡。
#![cfg(windows)]

pub use aerodesk_platform::windows::*;
