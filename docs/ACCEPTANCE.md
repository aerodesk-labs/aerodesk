# 验收矩阵与硬件需求（P5 #8）

目标指标：p99 抖动 <2ms、端到端 40-80ms@4K60；真机冒烟矩阵全平台。

## 硬件需求清单

| 平台 | 硬件 | 用途 | 对应 Issue |
|---|---|---|---|
| macOS | Apple Silicon（本机已有） | 被控端采集/编码、SFU/signal 开发 | #1/#5 |
| iOS | iPhone 真机（A12+） | 观看端验收（解码渲染） | #1 |
| Android | 真机（API 26+，arm64） | 观看+被控验收；NDK 交叉编译已就绪 | #2 |
| Windows | Win10/11 真机 | DXGI 采集/缩放、MF(h264_mf/hevc_mf)+OpenH264 编码、**D3D11VA/DXVA2 解码（已落地）**、WASAPI 音频、SendInput、VDD | #3 |
| Linux | 桌面真机（Wayland/X11） | PipeWire/VAAPI 采集编码 | #4 |
| HarmonyOS | DevEco + 鸿蒙真机（API 12+） | NAPI 桥、AVScreenCapture、OH_VideoDecoder | #6 |
| 服务器 | 多核 + 公网（含 TURN 端口段） | 4K60×N 压测、多 PoP | #5/#8 |
| Windows 虚拟屏 | Win10/11 真机 + Parsec VDD 驱动 | `scripts/windows-vdd-smoke.ps1 -Install`（vdd 模块冒烟） | #3/#140 |
| macOS 虚拟屏 | macOS 真机/无头 + BetterDisplay | `scripts/macos-vdd-smoke.sh` | #3/#140 |
| Linux 虚拟屏 | KDE Plasma 6 / Wayland 真机 | `scripts/linux-vdd-smoke.sh` | #4/#140 |

## 真机冒烟矩阵

| 被控端 \\ 观看端 | Web | macOS | iOS | Android | Windows | Linux |
|---|---|---|---|---|---|---|
| macOS | ✅(已有) | 待 | 待 | 待 | 待 | 待 |
| Windows | ✅(CI Chrome + Windows Edge e2e 回归 2026-08-14 + 远程 SFU 跨机联调 2026-08-15) | 待 | 待 | 待 | ✅(真机) | 待 |
| Linux | 待 | 待 | 待 | 待 | 待 | 待 |
| Android | 待 | 待 | 待 | 待 | 待 | 待 |

## 网络抗性（已自动化，CI 三平台）

- netem：1%/5% 丢包、组合 5% 丢包 + 30ms 延迟 + 15ms 抖动（7 个测试全绿）

## 4K60 压测流程（工具已就绪）

```sh
# 服务端（P3：signal 为 SIP 单栈；loadtest 的 JSON join 面待 SIP 化重写）
RECORD_DIR=... ./target/debug/aerodesk-sfu &
SIGNAL_OPS_PORT=3001 SIP_UDP_PORT=5060 ./target/debug/aerodesk-signal &

# 压测：N 房间 × P 对 @ 4K60（需硬件编码服务器；P3 起冻结——loadtest 走已退役的
# JSON join，SIP 化重写待 #600/#601 合并后进行）
scripts/loadtest.sh 4 2 60 3840 2160 60
# 看 /metrics：clients/rx/tx 包数；RECORD_DIR 录制约 4K 码流用于验证
```

验收输出：压测报告（CPU/带宽/内存/温度）+ p99 抖动/端到端延迟曲线。
