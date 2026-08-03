# 真机冒烟矩阵与硬件验收状态（#1/#2/#3/#4/#6/#8）

> 软件侧交付已全部合入 main（PR #37–#43）；下表为**真机/干净环境验收**的当前状态。
> 未打勾单元格 = 需要对应硬件，代码已就绪（或明确为后续实现）。

## 矩阵

| 被控端 \\ 观看端 | Web | macOS | iOS | Android | Windows | Linux | HarmonyOS |
|---|---|---|---|---|---|---|---|
| macOS | ✅（web-e2e） | ✅（smoke） | ⬜ 待 iPhone | ⬜ 待 Android | ⬜ 待 Win | ⬜ 待 Linux | ⬜ 待鸿蒙 |
| Windows | ⬜ | ⬜ | ⬜ | ⬜ | ⬜ | ⬜ | ⬜ |
| Linux | ⬜ | ⬜ | ⬜ | ⬜ | ⬜ | ⬜ | ⬜ |
| Android | ⬜ | ⬜ | ⬜ | ⬜ | ⬜ | ⬜ | ⬜ |

## 各 issue 验收门槛

| Issue | 软件侧 | 硬件/环境门槛 |
|---|---|---|
| #1 iOS 壳层 | 解码链路 ✅（#31/#32/#39） | iPhone 真机（A12+）：观看 macOS 流 |
| #2 Android 真机 | 收流/完整帧 ✅（#31） | Android 真机（API 26+）：MediaCodec 渲染 + 被控端 |
| #3 Windows | 采集/注入 ✅、软编（x264 待 Windows 系统库） | Win10/11 真机：MF 编码、DXVA2 解码、端到端 |
| #4 Linux | 采集/注入/软编 ✅ | Linux 真机：VAAPI 解码、Wayland/X11 端到端 |
| #6 HarmonyOS | NAPI 规约 ✅（文档） | DevEco + OHOS NDK + 鸿蒙真机（ring 交叉编译） |
| #8 压测 | 工具链/报告 ✅（#38）、netem ✅ | 干净环境 4K60 基线 + 真机矩阵 |

## 自动验证现状
- CI 三平台（macOS/Ubuntu/Windows）：cargo fmt/clippy/test + macOS e2e smoke ✅
- 本机（共享开发机，负载 8/10）：smoke/loadtest/web-e2e/选层 e2e 均跑通

## 硬件就绪后的验收步骤
1. 真机各端跑 `scripts/smoke.sh`（或对应壳层连接流程），填上表 ✅
2. 干净环境（专用机）跑 `scripts/bench.sh 4 2 60 3840 2160 60` 产出正式压测报告（#8）
3. 更新 ACCEPTANCE.md 与 BENCHMARK.md 基线
