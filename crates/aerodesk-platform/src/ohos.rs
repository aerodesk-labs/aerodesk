//! AeroDesk HarmonyOS 适配器（P4 骨架）。
//!
//! 角色：
//! - **观看端**：OH_VideoDecoder（H.264/HEVC 硬解）→ 渲染（XComponent/Surface）
//! - **被控端**（权限评估中）：AVScreenCapture 采集 + OH_VideoEncoder 硬编；
//!   输入注入需系统权限 `INTERACTIVE_CONTROL`/`INTERCEPT_INPUT_EVENT`（企业签名）
//!
//! 桥接：NAPI（手写，见 [`napi`]）暴露 aerodesk-core 到 ArkTS 壳层；
//! 媒体收发循环与 Android/iOS 同构（[`viewer`] / [`publisher`]）。
//! str0m Rust 核心可编译 `aarch64-unknown-linux-ohos`（rustup target）。

pub mod capture;
pub mod decode;
pub mod inject;
pub mod napi;
pub mod publisher;
pub mod viewer;
