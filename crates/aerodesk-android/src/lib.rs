//! AeroDesk Android 端侧薄壳：仅保留 JNI，平台实现已迁至
//! [`aerodesk_platform::android`]。
#![cfg(target_os = "android")]

pub mod jni;
pub mod ui;
