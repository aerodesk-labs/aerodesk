# HarmonyOS 适配器（P5 #6）

## 现状

- `crates/aerodesk-ohos`：骨架（capture/decode/inject/viewer/publisher/napi），
  Rust 目标 `aarch64-unknown-linux-ohos` 已通过 rustup 安装
- 无 OHOS NDK 的机器上，`cargo check -p aerodesk-ohos --target aarch64-unknown-linux-ohos`
  已可用 Zig 作为 C 编译器跑通（见下文「无 NDK 的 check 通道」）
- 最终 `cdylib` 链接仍被阻塞：`cargo build --target aarch64-unknown-linux-ohos`
  在链接 `.so` 时调用 `cc`（Apple clang），缺少 OHOS NDK 的链接器/sysroot
- 先跑 `scripts/check-ohos-toolchain.sh` 检测正式工具链是否就绪

## 前置（真机开发必需）

- DevEco Studio + HarmonyOS SDK（含 OHOS NDK / clang）
- 鸿蒙真机（API 12+）
- 环境变量示例（完整，含 AR/RANLIB；`scripts/check-ohos-toolchain.sh` 会检测）：

```sh
export PATH="/path/to/DevEco/ohos-ndk/llvm/bin:$PATH"
export CC_aarch64_unknown_linux_ohos="/path/to/DevEco/ohos-ndk/llvm/bin/clang"
export AR_aarch64_unknown_linux_ohos="/path/to/DevEco/ohos-ndk/llvm/bin/llvm-ar"
export RANLIB_aarch64_unknown_linux_ohos="/path/to/DevEco/ohos-ndk/llvm/bin/llvm-ranlib"
# 必要时（缺 sysroot 报错时）：
export CFLAGS_aarch64_unknown_linux_ohos="--sysroot=/path/to/DevEco/ohos-ndk/sysroot"
```

## 无 NDK 的 check 通道（2026-08-13 补充）

在只有 Zig（>= 0.16）而没有 OHOS NDK 的开发机上，用仓库内脚本执行：

```sh
bash crates/aerodesk-ohos/tools/cargo-check-ohos.sh
```

原理：

- ring / aws-lc-sys 的 C 编译步骤只缺一个能给
  `aarch64-unknown-linux-ohos` 提供 libc 头文件的 C 编译器；
  `zig cc` 的 `aarch64-linux-musl` 可先顶上（OpenHarmony libc 本身也基于 musl）
- `tools/zig-cc-ohos` 会把 `--target=aarch64-unknown-linux-ohos` 重写为
  `--target=aarch64-linux-musl`，并丢弃 Zig 不支持的 `-Wp,-U_FORTIFY_SOURCE` 等参数
- AR/RANLIB 使用 macOS 自带 `/usr/bin/ar`、`/usr/bin/ranlib`（Zig 的 `ar cq`
  不能直接创建 archive，Apple ar 已实测可用）

该通道只能证明 **Rust 侧代码与 C 源码在 OHOS target 下可编译通过**，
不能替代 NDK：`cargo build` 生成最终 `.so` 仍会失败在链接阶段。
CI 若无 ohos runner，建议接 `cargo-check-ohos.sh` 作为可达性检查。

## 方案

1. **NAPI 桥**：手写 NAPI（`napi_env`/`napi_value`）暴露
   `aerodesk-core`（连接/媒体流）给 ArkTS 壳层 —— 与 Android JNI、iOS FFI 同构
2. **观看端**：`OH_VideoDecoder`（H.264/HEVC 硬解）→ XComponent/Surface 渲染
3. **被控端**：`AVScreenCapture` 采集 + `OH_VideoEncoder` 硬编
4. **输入注入**：权限评估 —— `INTERACTIVE_CONTROL` / `INTERCEPT_INPUT_EVENT`
   需企业签名/系统应用通道
5. **UI**：ArkTS 壳 + Rust NAPI；Slint UI 组件库保持可迁移（P5 #7 已定）

## 里程碑

- [x] Rust target 安装（aarch64-unknown-linux-ohos）
- [x] 无 NDK 的 `cargo check` 通道（Zig 编译 ring/aws-lc-sys）
- [x] NAPI 桥骨架（connectViewer/takeFrame/disconnect/startPublish/injectInput；未真机验证）
- [ ] OHOS SDK 到位后正式链接 `.so`（CC/AR/RANLIB 指向 NDK）
- [ ] OH_VideoDecoder 硬解 + 渲染
- [ ] AVScreenCapture 采集 + 硬编
- [ ] 真机验收（鸿蒙观看 macOS 被控端）

## NAPI 桥接口规约（2026-08-04，骨架已实现，待 DevEco 工具链落地后真机验证）

与 Android JNI / iOS FFI 同构，暴露 aerodesk-core 给 ArkTS 壳层：

```ts
// 观看端
export function connectViewer(server: string, room: string, token?: string): number; // 返回 session id
export function takeFrame(session: number): Uint8Array;   // 取最新完整访问单元（AnnexB）
export function disconnect(session: number): void;

// 被控端（后续）
export function startPublish(server: string, room: string, token?: string): number;
export function injectInput(json: string): boolean;       // InputFrame JSON → 注入
```

- 观看端：ArkTS 侧拿 `takeFrame` 的 AnnexB 喂 `OH_VideoDecoder`（AVCC 化 + SPS/PPS 首帧），
  输出 Surface 给 XComponent 渲染
- 被控端：AVScreenCapture 输出 → Rust 侧编码/打包 → str0m RTP；注入走
  `OH_Input`/系统能力（企业签名通道）
- Rust 侧复用 `aerodesk-core::connect`（含 AccessUnitAssembler，与 iOS/Android 同管线）

## 交叉编译状态

- 根因：str0m 的 `dimpl` 依赖无条件带 `aws-lc-rs → aws-lc-sys`（BoringSSL C 构建），
  core 的 rustls 另带 `ring`；两者交叉编译都需要 C 工具链
- aws-lc-sys 本身支持 OHOS（构建脚本已识别 `OHOS_ARCH=arm64-v8a`），
  正式构建缺的是 OHOS NDK 的 clang/链接器/sysroot
- 无 NDK 临时通道见上文；最终 `cargo build -p aerodesk-ohos --target aarch64-unknown-linux-ohos`
  在链接 `.so` 时报 `ld: unknown options: --version-script=...` 等，属预期阻塞
- 检测脚本：`scripts/check-ohos-toolchain.sh`（target/CC/AR/RANLIB/最小 C 冒烟）
