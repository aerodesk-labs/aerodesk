# AeroDesk 产品决策记录（任务跟踪见 GitHub Issues/Projects）

> 本文档只保留**产品决策与背景**（平台约束、架构、选型理由、风险、踩坑记录）。
> **任务状态以 [GitHub Issues](https://github.com/aerodesk-labs/aerodesk/issues) 与
> [Projects 看板](https://github.com/orgs/aerodesk-labs/projects/1) 为准**；
> 当前模块状态见 `README.md` 的平台矩阵。

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
│  UI 层（Slint 一套 / ArkTS 原生壳兜底）                                      │
│  ─────────────────────────────────────────────────                          │
│  Rust 核心 aerodesk-core（跨平台）：                                           │
│   · 端点：str0m（发布端/观看端，与 SFU 同栈）                                 │
│   · 媒体管线抽象：采集→编码→RTP | RTP→解码→渲染（平台适配器实现）              │
│   · 输入事件协议 + 注入适配器                                                  │
│   · 信令客户端 + TURN 凭证管理                                                 │
│  ─────────────────────────────────────────────────                          │
│  平台适配层：采集/编码/解码/渲染/输入注入（原生 API）                         │
└──────────────────────────────────────────────────────────────────────────┘
                    │ WebRTC (UDP/TCP/SSL-TCP) + 信令 (WSS)
                    ▼
┌──────────────────────────── 服务端 ────────────────────────────────────────┐
│  信令服务：认证/房间/会话 + TURN 凭证下发（coturn REST API）                 │
│  aerodesk-sfu：多核分片(SO_REUSEPORT) + UnifiedSocket + BitrateController   │
│          + simulcast/SVC 选层 + 数据通道转发 + 录制/审计(可选)               │
│  coturn：TURN 中继（企业网 443 兜底）                                        │
│  部署：边缘 PoP（靠近用户），水平扩展                                         │
└──────────────────────────────────────────────────────────────────────────┘
```

## 三、技术选型（决策记录）

| 层 | 选型 | 理由 |
|---|---|---|
| 协议核心 | **str0m（含 bwe-fixes fork，pin 7db621f）** | SFU 与客户端同栈；纯 Rust 全平台（含鸿蒙 aarch64-unknown-linux-ohos） |
| 客户端 UI | **Slint**（Rust 原生，备选 ArkTS 原生壳） | 与 Rust 核心零 FFI 桥接；Win/macOS/Linux/Android/iOS/Web(WASM) 官方支持；HarmonyOS 用 ArkTS 壳兜底（2026-08 决策：不用 Flutter） |
| 服务端 SFU | 自研 aerodesk-sfu（多核分片架构） | PulseBeam 已证明该路线 |
| TURN | coturn | 标准、成熟、REST 凭证 |
| 编码（发布端） | **AV1 优先 → HEVC → H.264 兜底**（平台硬件编码器） | AV1 免专利费；HEVC 兼容 iOS；H.264 老设备兜底 |
| 解码（观看端） | 平台硬件解码（DXVA/VideoToolbox/MediaCodec/VAAPI/鸿蒙） | 4K60 必需 |
| 输入事件协议 | 统一定义：鼠标/键盘/触控/滚轮/剪贴板/文件拖拽 | 平台无关，版本化 |
| 信令 | WSS + JSON（房间/认证/ICE 交换） | 已独立为 aerodesk-signal |

## 四、关键风险与对策

| 风险 | 影响 | 对策 |
|---|---|---|
| iOS 无输入注入 API | iOS 不能做被控端 | 明确产品边界：iOS 仅观看端 |
| HarmonyOS 注入需系统权限（INTERCEPT_INPUT_EVENT） | 普通应用不可注入 | 企业签名/系统应用通道；先做观看端 |
| HarmonyOS WebRTC 原生支持有限 | 不能直接用系统 WebRTC | str0m Rust 核心编译 ohos；NAPI 桥接 |
| Slint 未官方支持 ohos target | 鸿蒙 UI 交付风险 | ArkTS 原生壳 + Rust NAPI，UI 设计保持 Slint 可迁移 |
| Web 被控端不可能 | 浏览器不能作为被控端 | 产品定义明确；Web 仅观看端 |
| Wayland 采集/注入 | Linux 兼容性 | PipeWire portal + XWayland 回退 |
| 硬件编码器 AV1 覆盖不均 | 老设备无法 AV1 | HEVC/H.264 自动降级链 |
| HEVC 专利授权费 | 商业化成本 | AV1 优先策略 + 法律评估 |

## 五、代码复用策略

```
aerodesk-core（crate，平台无关）        ← 服务端与客户端共享 str0m 依赖与 BWE 修复
  ├─ rtc_endpoint：发布/观看端点（str0m）
  ├─ media_pipeline：帧↔RTP 抽象（编码器/解码器 trait）
  ├─ input_protocol：输入事件编解码（版本化协议）
  └─ signaling_client：信令 + TURN 凭证
平台适配 crates（薄）：
  aerodesk-windows / aerodesk-macos / aerodesk-linux / aerodesk-android
  / aerodesk-ios / aerodesk-ohos / web(JS 侧)
aerodesk-sfu（服务端）                 ← 与客户端共享协议/媒体类型定义
```

**原则**：协议内核一份；媒体管线抽象一份；平台差异全部收敛到适配器 trait；输入协议一份定义（各平台只实现注入/捕获）。

## 六、已踩坑记录（平台适配必读）

1. **VideoToolbox H.264 硬解对 AnnexB 末 NAL 截断极敏感**：解析器必须保留最后一个
   NAL 的完整尾部（含 CABAC 收尾字节），截断 2-3 字节即稳定报
   `kVTVideoDecoderMalfunctionErr(-12909)`（与 profile/SEI/多 slice 无关）。
   回归测试：`parses_last_nal_without_truncation`。
2. **x264 输入必须是 4:2:0**：直接喂 RGB 会编码成 High 4:4:4（SPS profile_idc=0xF4），
   VideoToolbox 不支持。`X264Encoder` 已内置 RGB24→I420 转换（BT.601）。
3. **实时编码不要用帧级多线程**：x264 帧线程会缓冲前若干帧导致首帧延迟；
   软编路径固定单线程单 slice（输出确定且立即出帧）。
4. **SEI 不进解码样本**：x264/ffmpeg 的 buffering_period/pic_timing SEI（数百字节）
   在部分硬解上会触发 -12909；解码样本只含 VCL NAL（1..=5），SPS/PPS 走 format description。

## 七、任务跟踪入口

- **Issues**：<https://github.com/aerodesk-labs/aerodesk/issues>（#1 iOS 壳层、#2 Android、#3 Windows、#4 Linux、#5 服务端收口、#6 HarmonyOS、#7 Slint UI、#8 质量验收）
- **Projects 看板**：<https://github.com/orgs/aerodesk-labs/projects/1>（按里程碑 P3.5/P4/P5 + Status 分组）
- **质量目标（验收口径）**：p99 抖动 <2ms；端到端延迟 40-80ms@4K60；真机冒烟矩阵全绿
