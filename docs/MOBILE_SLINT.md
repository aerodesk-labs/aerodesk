# 移动端统一 Slint 方案与任务拆解

## 目标

iOS/iPad、Android、HarmonyOS 的端侧 UI/UX 统一走 Slint；native 层只保留
系统能力垫片（权限、系统服务、生命周期）。平台实现已收敛到
`aerodesk-platform::{ios,android,ohos}`，端侧 crate 目前是 FFI/JNI/NAPI 薄壳。

## 当前状态

- `aerodesk-platform`：已包含移动端平台实现模块（ios/android/ohos）。
- `aerodesk-ios`：C ABI 薄壳（`ffi.rs`），iOS 壳层仍为 SwiftUI。
- `aerodesk-android`：JNI 薄壳（`jni.rs`），Android 壳层仍为 Kotlin Activity/XML。
- `aerodesk-ohos`：NAPI 薄壳（`napi.rs`），ArkTS 壳层待 DevEco。

## 落地路径

### Android（优先，CI 有 APK build 可验证编译）

1. Rust 端侧：
   - `aerodesk-android` 增加 Slint 依赖（`backend-android-activity-06`）。
   - 新增 `ui.rs`：`android_main(app: slint::android::AndroidApp)`，
     调用 `slint::android::init(app)`，创建 Slint `AppWindow` 并运行。
   - 复用 `aerodesk-desktop/ui/app.slint` 的连接/房间/会话 UI（抽成共享 Slint 组件）。
2. Kotlin 壳层：
   - `MainActivity` 改为 `NativeActivity` 派生类，调用 `RustAndroidApp.run()`。
   - 保留 `ProjectionService`、`InputInjectionService` 作为系统垫片。
   - 移除 XML 布局中的 UI 部分；保留服务声明。
3. 构建：
   - `build-android-lib.sh` 继续 `cargo ndk build -p aerodesk-android`。
   - CI `android-apk-build` 覆盖 Rust .so + Gradle APK 编译验证。

### iOS/iPad

1. Rust 端侧：
   - `aerodesk-ios` 增加 Slint 依赖（winit iOS 后端）。
   - 新增 `ui.rs` 启动 Slint `AppWindow`；保留 `ffi.rs` 供原生生命周期调用。
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

- 当前 workspace 锁定 `slint 1.17.1`，但其 `backend-android-activity-06`
  feature 要求 `i-slint-backend-android-activity = "=1.17.1"`，crates.io
  尚未发布该后端 1.17.1（仅 1.10.0 等）。落地 Android Slint 前需先完成
  Slint 与 Android 后端版本选型（降级 slint 或等待后端发布）。- iOS 走 `backend-winit`：`i-slint-backend-winit 1.17.1` 已可用，但需 Xcode/
  真机验证 winit iOS 后端与 Swift 生命周期接入。- 更具体的阻塞：桌面 `aerodesk-desktop` 依赖 slint 1.17 的 `system-tray`
  feature（1.10 无此 feature），而 Android 后端只有 1.10 可用；同一
  workspace 内无法同时选择两个 1.x 版本。落地 Android Slint 需二选一：
  桌面降级并去掉 system-tray，或等待官方发布 1.17 的 Android 后端。- 降级路径也不可行：slint 1.10 缺少 `Weak::upgrade_in_event_loop`
  （desktop 4 处使用）、`system-tray`、`raw-window-handle-06` 通道。
  因此移动端 Slint 需等官方发布 slint 1.17 的 Android 后端，或接受
  桌面端大改（换事件循环模型 + 去托盘 + 重做窗口聚焦）。- iOS Rust 侧 Slint 入口已就位：`aerodesk-ios/src/ui.rs` 导出
  `ad_slint_run()`，使用 backend-winit 1.17。Swift 侧切换为 Slint 宿主
  待 Xcode/真机接入。