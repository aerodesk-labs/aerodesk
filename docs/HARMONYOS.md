# HarmonyOS 适配器（P5 #6）

## 现状

- `crates/aerodesk-ohos`：骨架（capture/decode/inject 模块），Rust 目标
  `aarch64-unknown-linux-ohos` 已通过 rustup 安装
- `cargo check -p aerodesk-ohos --target aarch64-unknown-linux-ohos`
  目前被 **ring 的 C 编译**阻塞：需要 OpenHarmony NDK 的 clang
  （设置 `CC_aarch64_unknown_linux_ohos` 指向 DevEco 自带工具链）

## 前置（真机开发必需）

- DevEco Studio + HarmonyOS SDK（含 OHOS NDK / clang）
- 鸿蒙真机（API 12+）
- 环境变量示例：

```sh
export PATH="/path/to/DevEco/toolchain:$PATH"
export CC_aarch64_unknown_linux_ohos=/path/to/DevEco/ohos-ndk/clang
export AR_aarch64_unknown_linux_ohos=llvm-ar
```

## 方案

1. **NAPI 桥**：`napi-rs` 或手写 NAPI（`napi_env`/`napi_value`）暴露
   `aerodesk-core`（连接/媒体流）给 ArkTS 壳层 —— 与 Android JNI、iOS FFI 同构
2. **观看端**：`OH_VideoDecoder`（H.264/HEVC 硬解）→ XComponent/Surface 渲染
3. **被控端**：`AVScreenCapture` 采集 + `OH_VideoEncoder` 硬编
4. **输入注入**：权限评估 —— `INTERACTIVE_CONTROL` / `INTERCEPT_INPUT_EVENT`
   需企业签名/系统应用通道
5. **UI**：ArkTS 壳 + Rust NAPI；Slint UI 组件库保持可迁移（P5 #7 已定）

## 里程碑

- [x] Rust target 安装（aarch64-unknown-linux-ohos）
- [ ] OHOS SDK 到位后打通 ring 编译（CC 指向 NDK clang）
- [ ] NAPI 桥（version/connect 最小闭环）
- [ ] OH_VideoDecoder 硬解 + 渲染
- [ ] AVScreenCapture 采集 + 硬编
- [ ] 真机验收（鸿蒙观看 macOS 被控端）
