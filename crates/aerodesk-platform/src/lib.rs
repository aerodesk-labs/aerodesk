//! AeroDesk 平台实现收敛层。
//!
//! 所有 `aerodesk_core::platform` trait 的平台实现统一在此 crate，
//! 按 target/平台模块组织；端侧 crate 只依赖本 crate，不再直接引用
//! 分散的 `aerodesk-{macos,windows,linux,ios,android,ohos}`。
//!
//! 移动端模块（iOS/Android/HarmonyOS）在此承载平台实现；端侧 crate 保留
//! FFI/JNI/NAPI 薄壳以维持 staticlib/cdylib 产物边界。

#[cfg(target_os = "android")]
pub mod android;
#[cfg(any(target_os = "macos", target_os = "ios"))]
pub mod apple;
#[cfg(target_os = "ios")]
pub mod ios;
#[cfg(target_os = "linux")]
pub mod linux;
#[cfg(target_os = "macos")]
pub mod macos;
#[cfg(target_env = "ohos")]
pub mod ohos;
#[cfg(target_os = "windows")]
pub mod windows;
