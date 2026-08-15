# Android 客户端（P3.5 #2）

双角色：观看端（MediaCodec 硬解）+ 被控端（MediaProjection 采集 + 无障碍注入）。

## 现状（里程碑 1）

- `crates/aerodesk-android`：Rust 核心 + JNI 桥（`jni.rs`）+ Slint 宿主（`ui.rs`，支持 server/room 连接/断开）
  - `version()`：SDK 版本
  - `connect(server, room)`：观看端连接（WSS 信令 + SDP 交换 + ICE 泵，阻塞调用）
- `android/`：Gradle 工程（AGP 8.7.3 / Kotlin 2.0.21 / compileSdk 34 / minSdk 26）
  - `SlintActivity`：launcher（`NativeActivity` 派生，加载 Rust Slint UI）
  - `MainActivity`：MediaCodec 观看端调试壳（非 launcher）
  - 预置 `jniLibs/arm64-v8a/libaerodesk_android.so`
- 验证：`cargo ndk` 交叉编译 ✅；`./gradlew assembleDebug` 产出 APK（内含 .so）✅；CI 已含 Android APK build job（每次 push/PR）

## 构建

```sh
# 1. Rust .so（NDK 27 + cargo-ndk）
scripts/build-android-lib.sh

# 2. APK（JDK 17）
cd android
JAVA_HOME=/opt/homebrew/opt/openjdk@17/libexec/openjdk.jdk/Contents/Home \
ANDROID_HOME="$HOME/Library/Android/sdk" ./gradlew assembleDebug
# 产物: android/app/build/outputs/apk/debug/app-debug.apk
```

## 后续（里程碑 2）

- [ ] MediaCodec 硬解 + Surface 渲染（Kotlin 侧，Rust 提供 AnnexB 流）
- [ ] MediaProjection 采集 → MediaCodec 硬编（被控端）
- [ ] AccessibilityService 输入注入
- [ ] 权限流程 UI（录屏/无障碍）
- [ ] 真机验收（模拟器 IP 用 10.0.2.2 访问宿主机信令）
