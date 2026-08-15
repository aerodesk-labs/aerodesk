//! AeroDesk 平台实现收敛层。
//!
//! 所有 `aerodesk_core::platform` trait 的平台实现统一在此 crate，
//! 按 target/平台模块组织；端侧 crate 只依赖本 crate，不再直接引用
//! 分散的 `aerodesk-{macos,windows,linux}`。
//!
//! 阶段 1 先收敛桌面三端纯库实现；移动端（iOS/Android/HarmonyOS）因有
//! FFI/JNI/NAPI 产物边界，后续端侧阶段再迁入。

#[cfg(target_os = "linux")]
pub mod linux;
#[cfg(target_os = "macos")]
pub mod macos;
#[cfg(target_os = "windows")]
pub mod windows;
// 移动端模块文件已就位，但阶段 1 不启用，避免与原 FFI crate 重复暴露符号。
// #[cfg(target_os = "ios")]
// pub mod ios;
// #[cfg(target_os = "android")]
// pub mod android;
// #[cfg(target_env = "ohos")]
// pub mod ohos;
