# AeroDesk — Remote Desktop（workspace）

全平台远程桌面的 Rust workspace：**WebRTC SFU 服务端 + 共享协议 + 跨平台客户端核心**。

> 仓库：<https://github.com/aerodesk-labs/aerodesk>
> 任务跟踪：[Issues](https://github.com/aerodesk-labs/aerodesk/issues) ·
> [Projects 看板](https://github.com/orgs/aerodesk-labs/projects/1) ·
> [Discussions](https://github.com/aerodesk-labs/aerodesk/discussions)
> 产品决策记录（平台角色/选型/风险/踩坑）：[Wiki](https://github.com/aerodesk-labs/aerodesk/wiki)
> TURN 中继部署见 [`docs/TURN.md`](docs/TURN.md)。
> 服务端生产化部署（JWT/TLS/多 PoP/录制审计）见 [`docs/DEPLOYMENT.md`](docs/DEPLOYMENT.md)。
> PulseBeam 架构借鉴见 [`docs/borrow-from-pulsebeam.md`](docs/borrow-from-pulsebeam.md)。

服务端与客户端共用 str0m（含 PulseBeam bwe-fixes fork，pin `7db621f`）作为协议底座。

## Workspace 结构

```
aerodesk/
├── crates/
│   ├── aerodesk-sfu/        # SFU 服务端：8 shard × SO_REUSEPORT + UnifiedSocket(UDP/TCP/SSL-TCP 3478)
│   │                        #   + BitrateController/simulcast 选层 + /metrics ✅
│   ├── aerodesk-signal/     # 独立信令：WSS:3001 + WS:3003，房间/认证/TURN 凭证，代理 SFU 内部 API ✅
│   ├── aerodesk-protocol/   # 共享协议：input/signal 消息 + coturn REST 凭证 ✅
│   ├── aerodesk-core/       # 客户端核心：Endpoint(SDP/ICE/DTLS/数据通道) + 信令客户端 + VP8 解析 ✅
│   ├── aerodesk-cli/        # CLI：publisher（pcap/x264/VT/screen 四种源）+ viewer ✅
│   ├── aerodesk-macos/      # macOS 适配器：ScreenCaptureKit 采集 + VT 硬编 + x264 软编 + CGEvent 注入 ✅
│   ├── aerodesk-ios/        # iOS 适配器：VideoToolbox H.264 硬解（AnnexB→CVPixelBuffer）✅
│   ├── aerodesk-android/    # Android 适配器骨架：MediaCodec/MediaProjection/Accessibility 🔨 P3
│   ├── aerodesk-linux/      # Linux 适配器骨架：PipeWire/VAAPI/XTest 🔨 P4
│   ├── aerodesk-windows/    # Windows 适配器骨架：WGC/DXGI + MF + SendInput 🔨 P4
│   ├── aerodesk-ohos/       # HarmonyOS 适配器骨架：AVScreenCapture/OH_VideoDecoder/NAPI 🔨 P4
│   └── x264/                # vendored x264 crate（+sliced_threads/threads 控制）
├── web/index.html           # 浏览器观看端（publisher=屏幕采集受限 / viewer=观看+输入）
├── certs/                   # str0m.test 自签证书（开发用）
└── docs/                    # 规划与调研
```

## 运行

```sh
# 服务端（SFU：UDP/TCP/SSL-TCP 同端口 3478 + 内部 API 3002）
TURN_SECRET=<coturn static-auth-secret> cargo run -p aerodesk-sfu

# 独立信令（WSS 3001 / WS 3003）
cargo run -p aerodesk-signal

# 发布端（macOS 真实屏幕采集，需屏幕录制 + 辅助功能权限）
cargo run -p aerodesk-cli -- --role publisher --encoder screen

# 发布端 simulcast（q/h/f 三层，SFU 选层真实生效；--noisy 用高熵合成源验证码率档位）
cargo run -p aerodesk-cli -- --role publisher --encoder x264 --simulcast --noisy

# 观看端（--layer q|h|f 显式选层；--audio 接收音频，--mute-audio 静音）
cargo run -p aerodesk-cli -- --role viewer --layer f --audio

# 音频：publisher --audio 发送合成 PCMU（G.711 8kHz）；真实系统音频采集待真机接入
# 显示器：publisher --display N 初始采集；viewer --display N 经 control 切换被控端显示器
# 文件传输：publisher --send-file <path>；对端 --recv-dir <dir> 接收（data channel，SHA-256 校验）

# 浏览器：https://<host>:3000/?role=publisher|viewer
```

客户端自动从 `GET /config` 获取 `iceServers`（coturn REST 凭证）。

## 架构

```
browser (viewer) ─┐                    ┌─ browser / iOS / 桌面 (viewer)
native (被控端) ───┼─ WebRTC ─▶ aerodesk-sfu ─┼─ (无重编码，选择性转发)
  Win/macOS/Linux │  UDP/TCP/SSL-TCP   │
  Android/Harmony │  同端口 3478(dev)/443(prod)
                  └── input 数据通道（观看端→被控端）──┘
```

- 信令：WSS（aerodesk-signal :3001）→ Join → offer/answer 代理到 SFU 内部接口（127.0.0.1:3002）
- 媒体：`MediaData` 选择性转发（不重编码）；simulcast 选层（q/h/f）已实现
- 输入：`input` 通道 JSON 事件（协议类型在 `aerodesk-protocol::input`）
- 编码：VideoToolbox 硬编（macOS 主路径）/ x264 软编回退（RGB→I420 4:2:0，单 slice 确定性输出）

## 验证

```sh
cargo test --workspace
cargo clippy --workspace --all-targets
cargo fmt --all -- --check
cargo build -p aerodesk-ios --target aarch64-apple-ios-sim   # iOS 目标编译
```

已验证：UDP/TCP/SSL-TCP 同端口三候选、fake-SSL 握手字节级匹配、信令应答、ICE-TCP candidate、
多核分片房间路由、coturn REST 凭证与真实 allocate、macOS 真实屏幕采集 → VT 硬编 → SFU →
CLI viewer 端到端收流、iOS 解码器（x264 关键帧 + P 帧全序列 8/8 解码，首帧 187ms 会话预热、
后续 ~0.3ms/帧）。

## 平台角色

| 平台 | 被控端 | 观看端 | 状态 |
|---|---|---|---|
| Web | ❌ | ✅ | P0 完成 |
| macOS | ✅ | ✅ | P2 完成（采集/编码/注入） |
| Windows | 🔨 | 🔨 | P4 骨架 |
| Linux | 🔨 | 🔨 | P4 骨架 |
| Android | 🔨 | 🔨 | P3 骨架 |
| iOS | ❌ | ✅ | P3 解码器完成（App 壳层待真机） |
| HarmonyOS | ⚠️ 权限评估 | ✅ | P4 骨架 |

## 路线图

- P0 原型：✅（SFU + Web 双端）
- P1 服务端生产化：✅（多核分片、coturn TURN、独立信令、BitrateController、simulcast 选层、netem 测试、/metrics）
- P2 桌面客户端：✅ 核心 + macOS 适配器完成（真实屏幕采集 E2E 验证）
- P3 移动端：🔨 iOS 硬解完成；Android 适配器骨架
- P4 鸿蒙 + 全平台收口：🔨 适配器骨架就绪；待各平台 SDK 真机实现
