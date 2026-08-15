//! AeroDesk iOS/iPad 端侧薄壳：仅保留 C ABI，平台实现已迁至
//! [`aerodesk_platform::ios`]。
#![cfg(target_os = "ios")]

pub mod ffi;
