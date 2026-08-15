# 移动端统一 Slint 方案与任务拆解

## 目标

iOS/iPad、Android、HarmonyOS 的端侧 UI/UX 统一走 Slint；native 层只保留
系统能力垫片（权限、系统服务、生命周期）。平台实现已收敛到
`aerodesk-platform::{ios,android,ohos}`，端侧 crate 目前是 FFI/JNI/NAPI 薄壳。

## 当前状态

- `aerodesk-platform`：已包含移动端平台实现模块（ios/android/ohos）。
- `aerodesk-ios`：C ABI 薄壳（`ffi.rs`）+ Slint 宿主（`ui.rs`，支持 server/room 连接/断开）；iOS 壳层默认仍为 SwiftUI，`-slint` 可切到 Rust Slint UI。
- `aerodesk-android`：JNI 薄壳（`jni.rs`）+ Slint 宿主（`ui.rs`），Kotlin 壳层已退化为系统垫片（服务 + NativeActivity）。
- `aerodesk-ohos`：NAPI 薄壳（`napi.rs`），ArkTS 壳层待 DevEco。

## 落地路径

### Android（优先，CI 有 APK build 可验证编译）

1. Rust 端侧：
   - `aerodesk-android` 已增加 Slint 依赖（`backend-android-activity-06` + `renderer-software`）。
   - 已新增 `ui.rs`：`android_main(app: slint::android::AndroidApp)`，调用 `slint::android::init(app)` 并运行 Slint `AndroidAppWindow`；当前已支持输入 server/room 并连接/断开 `ViewerSession`。
   - 后续复用 `aerodesk-desktop/ui/app.slint` 的连接/房间/会话 UI（抽成共享 Slint 组件），并把解码帧渲染接入 Slint。
2. Kotlin 壳层：
   - 已新增 `SlintActivity : NativeActivity`，通过 `android.app.lib_name=aerodesk_android` 加载 Rust Slint 入口。
   - 保留 `ProjectionService`、`InputInjectionService`、`PublisherActivity` 作为系统垫片。
   - `MainActivity`（MediaCodec 观看端调试壳）保留但不再作为 launcher。
3. 构建：
   - `build-android-lib.sh` 继续 `cargo ndk build -p aerodesk-android`。
   - CI `android-apk-build` 覆盖 Rust .so + Gradle APK 编译验证。

### iOS/iPad

1. Rust 端侧：
   - `aerodesk-ios` 增加 Slint 依赖（winit iOS 后端）。
   - 新增 `ui.rs` 启动 Slint `IosAppWindow`；当前已支持输入 server/room 并连接/断开 `ViewerSession`，保留 `ffi.rs` 供原生生命周期调用。
2. Swift 壳层：
   - 替换 `ContentView.swift` 的 SwiftUI，改为承载 Slint 窗口。
   - 保留 App 生命周期、系统权限桥。
3. 构建：
   - `build-ios-lib.sh` 继续产出 xcframework；Xcode build job 覆盖编译。

### HarmonyOS

- Slint 官方无 OHOS 后端，先按 `docs/HARMONYOS.md` 的
  “ArkTS 壳 + Rust NAPI；Slint 组件库可迁移”路线落地。
- 待 Slint 官方/社区 OHOS 后端成熟后，再切标准 Slint 运行时。

## 前置条件

- Android：SDK/NDK、真机或模拟器；Slint Android 模板（android-activity Kotlin 绑定）。
- iOS：Xcode + iPhone/iPad 真机或模拟器；winit iOS 后端。
- HarmonyOS：DevEco Studio + OHOS NDK + 真机。

## 验收

- Android APK 构建通过；真机启动进入 Slint UI，能连接观看/发布。
- iOS 模拟器/真机启动进入 Slint UI，能连接观看；iPad 同一 target 适配。
- OHOS HAP 构建通过；ArkTS 壳能调用 NAPI 并呈现 Slint 组件迁移后的 UI。

## 已知坑

- Android 后端阻塞已解除：`i-slint-backend-android-activity 1.17.1` 已发布，
  `aerodesk-android` 已启用 `backend-android-activity-06`，无需降级 slint。
- Android Slint 后端的 `build.rs` 需要 `android.jar`；CI 与本地构建脚本已显式
  提供 `ANDROID_PLATFORM` / `ANDROID_JAR`（见 `build-android-lib.sh`）。
- iOS 走 `backend-winit`：`i-slint-backend-winit 1.17.1` 已可用，但需 Xcode/真机
  验证 winit iOS 后端与 Swift 生命周期接入。
- iOS Rust 侧 Slint 入口已就位：`aerodesk-ios/src/ui.rs` 导出 `ad_slint_run()`，
  使用 backend-winit 1.17；Swift 侧目前保留 `-slint` 开关，默认仍 SwiftUI，
  待真机验证后切默认 Slint。
- 移动端 Slint 已完成编译级接入，真机生命周期/权限桥接仍需实机验证。
## 跨平台 CI 覆盖

- 桌面三端（macOS/Linux/Windows）：`ci.yml` 的 `test` 矩阵 `cargo clippy --workspace` 编译 `aerodesk_platform::{macos,linux,windows}`。
- iOS/iPad：`ios-app-build` 通过 `build-ios-lib.sh all` 编译真机 + 模拟器两个 slice，覆盖 `aerodesk_platform::ios`。
- Android：`android-apk-build` 通过 `build-android-lib.sh` 编译 arm64 cdylib，覆盖 `aerodesk_platform::android`。
- 手动快速检查：新增 `.github/workflows/platform-cross.yml`，`workflow_dispatch` 触发，仅做 `cargo check`（桌面三端 + iOS device/sim + Android arm64），用于发版前/跨平台改动时快速验证，不重复挂 PR。
- OHOS 未接入 CI：GitHub hosted runner 无 OHOS NDK；需先 `scripts/check-ohos-toolchain.sh` 就绪，再 `cargo check -p aerodesk-ohos --target aarch64-unknown-linux-ohos`。阻塞点是 str0m→dimpl→aws-lc-sys 需要 OHOS C 工具链。

## iOS Slint 宿主开关

- `aerodesk-ios/src/ui.rs` 已导出 `ad_slint_run()`。
- Swift 侧 `AeroDeskApp.swift` 默认仍走 SwiftUI 观看端；启动参数 `-slint` 时在主队列调用 `ad_slint_run()` 进入 Rust Slint UI（实验性，待真机/Xcode 验证 winit iOS 后端）。
