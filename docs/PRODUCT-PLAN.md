# 远程桌面产品统筹规划（全平台）

> 目标：Web + Windows + macOS + Linux + Android + iOS + HarmonyOS（及将来桌面版本）
> 统一架构：一套 Rust 核心 + 平台薄壳 + WebRTC 标准协议 + str0m SFU

## 一、平台角色矩阵（硬约束）

| 平台 | 被控端（采集+编码+输入注入） | 观看端（解码渲染+输入捕获） | 说明 |
|---|---|---|---|
| Web/浏览器 | ❌ 沙箱无注入，getDisplayMedia 受限 | ✅ | 只能做观看端 |
| Windows | ✅ DXGI + NVENC/QSV/AMF + SendInput | ✅ | 首选被控端 |
| macOS | ✅ ScreenCaptureKit + VideoToolbox + CGEvent(辅助功能授权) | ✅ | 首选被控端 |
| Linux | ✅ PipeWire/XRandR + VAAPI/NVENC + XTest/uinput | ✅ | Wayland 需 portal |
| Android | ⚠️ MediaProjection + MediaCodec + AccessibilityService（需用户授权） | ✅ | 可被控，体验受授权限制 |
| iOS | ❌ 无公开全局输入注入 API | ✅ | **只能观看端** |
| HarmonyOS | ⚠️ AVScreenCapture + 硬编 ✅；注入需系统权限（INTERCEPT_INPUT_EVENT） | ✅ | 注入权限是风险项 |
| 将来桌面版 | 复用桌面三平台能力 | ✅ | 多显示器/触屏优化 |

**结论：产品形态 = "桌面/部分移动端作为被控端，全平台作为观看端"；Web 与 iOS 永不作为被控端。**

## 二、总体架构

```
┌────────────────────────── 客户端（每平台一个薄壳） ──────────────────────────┐
│  UI 层（Flutter 一套 / 原生壳）                                              │
│  ─────────────────────────────────────────────────                          │
│  Rust 核心 aerodesk-core（跨平台，FFI/NAPI/JSI 暴露）：                             │
│   · 端点：str0m（发布端/观看端，与 SFU 同栈）                                 │
│   · 媒体管线抽象：采集→编码→RTP | RTP→解码→渲染（平台适配器实现）              │
│   · 输入事件协议（统一二进制/JSON 定义）+ 注入适配器                          │
│   · 信令客户端 + TURN 凭证管理 + 自升级/日志                                  │
│  ─────────────────────────────────────────────────                          │
│  平台适配层：采集/编码/解码/渲染/输入注入/电源管理（原生 API）                │
└──────────────────────────────────────────────────────────────────────────┘
                    │ WebRTC (UDP/TCP/SSL-TCP) + 信令 (WSS)
                    ▼
┌──────────────────────────── 服务端 ────────────────────────────────────────┐
│  信令服务：认证/房间/会话 + TURN 凭证下发（coturn REST API）                 │
│  aerodesk-sfu：多核分片(SO_REUSEPORT) + UnifiedSocket + BitrateController          │
│          + simulcast/SVC 选层 + 数据通道转发 + 录制/审计(可选)               │
│  coturn：TURN 中继（企业网 443 兜底）                                        │
│  可观测性：metrics/tracing + 会话质量报表                                     │
│  部署：边缘 PoP（靠近用户），水平扩展                                         │
└──────────────────────────────────────────────────────────────────────────┘
```

## 三、技术选型

| 层 | 选型 | 理由 |
|---|---|---|
| 协议核心 | **str0m（含 bwe-fixes fork）** | SFU 与客户端同栈，一套代码两种角色；纯 Rust 全平台（含鸿蒙 rustup 目标 aarch64-unknown-linux-ohos） |
| 客户端 UI | Flutter（备选 Qt/原生） | 一套 UI 覆盖 7 平台；flutter_ohos 社区适配；Web 可跑 |
| 服务端 SFU | 自研 aerodesk-sfu（多核分片架构） | 见 borrow-from-pulsebeam.md；PulseBeam 已证明该路线 |
| TURN | coturn | 标准、成熟、REST 凭证 |
| 编码（发布端） | **AV1 优先 → HEVC → H.264 兜底**（各平台硬件编码器） | AV1 免专利费；HEVC 兼容 iOS；H.264 老设备兜底 |
| 解码（观看端） | 平台硬件解码（DXVA/VideoToolbox/MediaCodec/VAAPI/鸿蒙） | 4K60 必需 |
| 输入事件协议 | 统一定义：鼠标/键盘/触控/滚轮/剪贴板/文件拖拽 | 平台无关，版本化 |
| 信令 | WSS + JSON（房间/认证/ICE 交换） | 与 SFU 同进程起步，后续独立 |

## 四、分阶段路线图

| 阶段 | 目标 | 关键交付 | 依赖 |
|---|---|---|---|
| **P0 当前** | SFU 原型 + Web 验证 | aerodesk-sfu 单线程 SFU、UnifiedSocket(3478)、ICE-TCP/SSL-TCP、Web publisher/viewer | 已完成（待浏览器双端实测） |
| **P1 服务端生产化** | 可商用的服务端底座 | 多核分片(SO_REUSEPORT) + 批量接收；coturn 接入；信令服务（认证/房间/TURN 凭证）；BitrateController + simulcast/SVC 选层；turmoil 模拟测试；metrics | P0 |
| **P2 桌面客户端** | Windows/macOS/Linux 被控端+观看端 | ✅ aerodesk-core + aerodesk-macos 完成（ScreenCaptureKit 真实采集 → VT 硬编 → SFU → CLI viewer E2E；CGEvent 注入）；🔜 Windows/Linux 适配器（P4 骨架已建） | P1 |
| **P3 移动端** | Android 双角色、iOS 观看端 | ✅ aerodesk-ios 解码器完成（VideoToolbox H.264 硬解，x264 关键帧+P 帧全序列测试）；🔜 Android 适配器骨架（MediaCodec/MediaProjection/Accessibility，需 NDK 真机实现）；移动网络优化 | P2 |
| **P4 鸿蒙 + 全平台收口** | HarmonyOS 双角色、Web 观看端完善 | 🔨 适配器骨架已建（aerodesk-linux/windows/ohos）；鸿蒙 NAPI + AVScreenCapture + OH_Input_*（权限评估）；Windows WGC/DXGI + MF；Linux PipeWire/VAAPI/XTest；Web 观看体验完善 | P3 |

每个阶段验收标准：自动化测试（模拟器确定性测试）+ 真机冒烟 + 质量指标（p99 抖动 <2ms、端到端延迟 40-80ms@4K60）。

## 五、关键风险与对策

| 风险 | 影响 | 对策 |
|---|---|---|
| iOS 无输入注入 API | iOS 不能做被控端 | 明确产品边界：iOS 仅观看端 |
| HarmonyOS 注入需系统权限（INTERCEPT_INPUT_EVENT） | 普通应用不可注入 | 企业签名/系统应用通道；先做观看端，被控端待权限评估 |
| HarmonyOS WebRTC 原生支持有限 | 不能直接用系统 WebRTC | **str0m Rust 核心编译 ohos 是优势**；NAPI 桥接验证列入 P4 前置任务 |
| Web 被控端不可能 | 浏览器不能作为被控端 | 产品定义明确；Web 仅观看端 |
| Wayland 采集/注入 | Linux 兼容性 | PipeWire portal + XWayland 回退 |
| 硬件编码器 AV1 覆盖不均 | 老设备无法 AV1 | HEVC/H.264 自动降级链 |
| HEVC 专利授权费 | 商业化成本 | AV1 优先策略 + 法律评估 |
| flutter_ohos 成熟度 | 鸿蒙 UI 交付风险 | 备选 ArkTS 原生壳 + Rust NAPI |

## 六、代码复用策略（统筹核心）

```
aerodesk-core（crate，平台无关）        ← 服务端与客户端共享 str0m 依赖与 BWE 修复
  ├─ rtc_endpoint：发布/观看端点（str0m）
  ├─ media_pipeline：帧↔RTP 抽象（编码器/解码器 trait）
  ├─ input_protocol：输入事件编解码（版本化协议）
  └─ signaling_client：信令 + TURN 凭证
平台适配 crates（薄）：
  aerodesk-dxgi / aerodesk-macos / aerodesk-pipewire / aerodesk-android / aerodesk-ios / aerodesk-ohos / aerodesk-web(JS 侧)
aerodesk-sfu（服务端）                 ← 与客户端共享协议/媒体类型定义
```

**原则**：协议内核一份（str0m + bwe-fixes）；媒体管线抽象一份；平台差异全部收敛到适配器 trait；输入协议一份定义（各平台只实现注入/捕获）。

## 七、当前状态速览

| 模块 | 状态 |
|---|---|
| aerodesk-sfu（分片/UnifiedSocket/选层/metrics） | ✅ 完成 |
| coturn TURN（REST 凭证 + /config 下发） | ✅ 完成 |
| aerodesk-signal（WSS/WS 信令） | ✅ 完成 |
| aerodesk-core（Endpoint/信令客户端/VP8） | ✅ 完成 |
| aerodesk-macos（SCK 采集/VT 硬编/x264/CGEvent） | ✅ 完成（E2E 验证） |
| aerodesk-ios（VT H.264 硬解） | ✅ 解码库完成；App 壳层待做 |
| aerodesk-android / linux / windows / ohos | 🔨 骨架（trait + 数据流） |
| web/index.html | ✅ P0 原型 |

## 八、下一步执行计划

### P3.5 移动端收口（优先）
1. **iOS App 壳层（观看端）** — [#1](https://github.com/aerodesk-labs/aerodesk/issues/1)
2. **Android 真机适配（观看端 + 被控端）** — [#2](https://github.com/aerodesk-labs/aerodesk/issues/2)

### P4 桌面补齐（次优先）
3. **Windows 适配器** — [#3](https://github.com/aerodesk-labs/aerodesk/issues/3)
4. **Linux 适配器** — [#4](https://github.com/aerodesk-labs/aerodesk/issues/4)
5. **服务端收口** — [#5](https://github.com/aerodesk-labs/aerodesk/issues/5)

### P5 鸿蒙 + 产品化
6. **HarmonyOS 适配器** — [#6](https://github.com/aerodesk-labs/aerodesk/issues/6)
7. **Flutter UI 壳（7 平台）** — [#7](https://github.com/aerodesk-labs/aerodesk/issues/7)
8. **质量验收与 4K60 压测** — [#8](https://github.com/aerodesk-labs/aerodesk/issues/8)

> 所有任务在 GitHub Issues 跟踪（按里程碑 P3.5/P4/P5 分桶），
> 本文件只保留计划背景与坑位记录，任务状态以 Issues/Projects 为准。

## 九、已踩坑记录（平台适配必读）

1. **VideoToolbox H.264 硬解对 AnnexB 末 NAL 截断极敏感**：解析器必须保留最后一个
   NAL 的完整尾部（含 CABAC 收尾字节），截断 2-3 字节即稳定报
   `kVTVideoDecoderMalfunctionErr(-12909)`（症状与 profile/SEI/多 slice 无关，
   排查时全部排除了）。回归测试：`parses_last_nal_without_truncation`。
2. **x264 输入必须是 4:2:0**：直接喂 RGB 会编码成 High 4:4:4（SPS profile_idc=0xF4），
   VideoToolbox 不支持。`X264Encoder` 已内置 RGB24→I420 转换（BT.601）。
3. **实时编码不要用帧级多线程**：x264 帧线程会缓冲前若干帧导致首帧延迟；
   软编路径固定单线程单 slice（输出确定且立即出帧）。
4. **SEI 不进解码样本**：x264/ffmpeg 的 buffering_period/pic_timing SEI（数百字节）
   在部分硬解上会触发 -12909；解码样本只含 VCL NAL（1..=5），SPS/PPS 走 format description。
