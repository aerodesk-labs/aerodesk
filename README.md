# AeroDesk — Remote Desktop（workspace）

全平台远程桌面的 Rust workspace：**WebRTC SFU 服务端 + 共享协议 + 跨平台客户端核心**。

> 仓库：<https://github.com/aerodesk-labs/aerodesk>
> 任务跟踪：[Issues](https://github.com/aerodesk-labs/aerodesk/issues) ·
> [Projects 看板](https://github.com/orgs/aerodesk-labs/projects/1) ·
> [Discussions](https://github.com/aerodesk-labs/aerodesk/discussions)
> 产品决策记录（平台角色/选型/风险/踩坑）：[Wiki](https://github.com/aerodesk-labs/aerodesk/wiki)
> TURN 中继部署见 [`docs/TURN.md`](docs/TURN.md)。
> 服务端生产化部署（JWT/TLS/多 PoP/录制审计）见 [`docs/DEPLOYMENT.md`](docs/DEPLOYMENT.md)。
> 运维 dashboard（房间/客户端/录制/负载/TURN 可视化）见 [`docs/ADMIN.md`](docs/ADMIN.md)。
> PulseBeam 架构借鉴见 [`docs/borrow-from-pulsebeam.md`](docs/borrow-from-pulsebeam.md)。

服务端与客户端共用 str0m（aerodesk-labs 派生：PulseBeam bwe-fixes + dimpl DTLS 接收队列扩容，pin `a9c8de7`，见 `Cargo.toml` 注释）作为协议底座。

## Workspace 结构

```
aerodesk/
aerodesk/
aerodesk/
├── crates/
│   ├── aerodesk-sfu/        # SFU 服务端：8 shard × SO_REUSEPORT + UnifiedSocket(UDP/TCP/SSL-TCP 3478)
│   │                        #   + BitrateController/simulcast 选层 + /healthz + /metrics[/prometheus] ✅
│   ├── aerodesk-signal/     # 独立信令：WSS:3001 + WS:3003，房间/认证/TURN 凭证，代理 SFU 内部 API ✅
│   ├── aerodesk-protocol/   # 共享协议：input/signal 消息 + coturn REST 凭证 ✅
│   ├── aerodesk-core/       # 客户端核心：Endpoint(SDP/ICE/DTLS/数据通道) + 信令客户端 + VP8 解析 ✅
│   │                        #   platform trait：MediaSource/Encoder/Decoder/Renderer/InputInjector/
│   │                        #   AudioSink/AudioCapturer/Clipboard/CursorSource/Permissions/CameraSource/
│   │                        #   FilePicker/AppShell/VirtualDisplay/Notifier/CommandExecutor（#330）/SystemWakeLock（#334）
│   ├── aerodesk-platform/   # 平台实现收敛层：macos/windows/linux/ios/android/ohos 各平台 trait 实现 ✅
│   ├── aerodesk-desktop/    # 桌面端侧 UI/UX（Slint，Win/macOS/Linux）✅
│   ├── aerodesk-agent/        # agent：publisher（pcap/x264/VT/screen 四种源）+ viewer ✅
│   ├── aerodesk-ios/        # iOS/iPad FFI 薄壳 + Slint 宿主（平台实现已迁 aerodesk-platform）✅
│   ├── aerodesk-android/    # Android JNI 薄壳 + Slint 宿主（平台实现已迁 aerodesk-platform）🔨 P3
│   ├── aerodesk-ohos/       # HarmonyOS NAPI 薄壳（平台实现已迁 aerodesk-platform）🔨 P4
│   └── x264/                # vendored x264 crate（+sliced_threads/threads 控制）
├── web/index.html           # 浏览器观看端（publisher=屏幕采集受限 / viewer=观看+输入）
├── certs/                   # str0m.test 自签证书（开发用）
└── docs/                    # 规划与调研
## 运行

```sh
# 服务端（SFU：UDP/TCP/SSL-TCP 同端口 3478 + 内部 API 3002）
TURN_SECRET=<coturn static-auth-secret> cargo run -p aerodesk-sfu

# 独立信令（WSS 3001 / WS 3003）
cargo run -p aerodesk-signal

# 发布端（macOS 真实屏幕采集，需屏幕录制 + 辅助功能权限）
cargo run -p aerodesk-agent -- --role publisher --encoder screen

# 发布端 simulcast（q/h/f 三层，SFU 选层真实生效；--noisy 用高熵合成源验证码率档位）
cargo run -p aerodesk-agent -- --role publisher --encoder x264 --simulcast --noisy

# 观看端（--layer q|h|f 显式选层；--audio 接收音频，--mute-audio 静音）
cargo run -p aerodesk-agent -- --role viewer --layer f --audio
# #173 自动重连（中途断线/服务重启后指数退避重连，--reconnect-max 默认 5 次）
cargo run -p aerodesk-agent -- --role viewer --reconnect
# #175 Web 端自动重连：WS/ICE 断开后浏览器自动退避重连（≤5 次，成功重置；手动断开不重连）

# 音频：publisher --audio 发送合成 PCMU（G.711 8kHz）；--audio-opus 用 Opus（48kHz，libopus）
#       （#73）；真实系统音频采集待真机接入
# 显示器：publisher --display N 初始采集；viewer --display N 经 control 切换被控端显示器
# 文件传输：publisher --send-file <path>；对端 --recv-dir <dir> 接收（data channel，SHA-256 校验）
# 多 codec：#74 --encoder ffmpeg --codec h264|h265|vp9|av1（FFmpeg，硬编优先；AV1 有 ~1s 编码延迟）
# 输入注入：#75 viewer 输入 → SFU → publisher macOS CGEvent（坐标归一化→屏幕点，Wheel/修饰键/完整键码）
# 远程命令：#109 viewer --run-command "ls -la" → SFU → 被控端执行并回传 stdout/stderr/exit
#       （执行器按 platform::CommandExecutor 抽象：#330，core 默认 sh -c/cmd /C，macOS MacCommandExecutor）
#       （危险命令默认拦截，白名单 $HOME/AeroDesk/cmd-allowlist.txt，审计 $HOME/AeroDesk/cmd-audit.jsonl）
#       文件/进程：--read-file <path> / --write-file <path> <content> / --list-processes / --kill-pid <pid>
#       （写文件敏感路径默认禁止；kill pid 0/1 默认禁止）
#       权限/审计管理（本地）：--cmd-allowlist list|add <prefix>|remove <prefix> / --cmd-audit [n]
#       （桌面 UI 设置页「AI 远控」也可管理白名单与审计）
# MCP 工具面：#109 提供 aerodesk-mcp（stdio JSON-RPC）——tools: connect/run_command/
#       read_file/write_file/list_processes/kill_process，经 aerodesk-agent 桥接操作被控端
#       （AERODESK_SIGNAL/AERODESK_ROOM/AERODESK_AGENT_BIN 环境变量配置）
#       键鼠工具：mouse_move / mouse_click / type_text（经 --send-input / --type-text）
#       大文件：#122 MCP download_file/upload_file（file 通道；viewer --request-file 下载，
#       --send-file 上传；被控端 --recv-dir 落盘；CLI 主线程 32MB 栈规避 sctp 深调用溢出）
#       MCP 接入文档见 docs/MCP.md
# A/V 同步：#73 viewer AVSYNC 时间轴/漂移统计 + 音频 jitter buffer（PCMU/Opus 真实播放 macOS 已通；会话工具栏音量滑块）

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
| Windows | ✅ | ✅ | 被控端+观看端完整：DXGI 采集/缩放、MF(h264_mf/hevc_mf)+OpenH264 编码、WASAPI 音频、SendInput/VDD、唤醒锁、开机自启（HKCU Run）、原生 Win32 剪贴板（文本/图片）、D3D11VA/DXVA2 硬解（UI+CLI）、Opus/PCMU 播放、远程光标；本机端到端（4K30 硬编→硬解）验证；便携 ZIP + MSI 安装器 |
| Linux | ✅ | ✅ | 被控端+观看端完整：X11/Wayland(PipeWire) 采集、VAAPI 硬编/硬解 + x264/OpenH264 回退、XTest/uinput/portal 注入、PipeWire 系统音频、V4L2 摄像头（--camera/--list-cameras）、真实光标（X11 QueryPointer）、剪贴板（文本/图片 + 注入）、FilePicker（zenity/kdialog）、Notifier（notify-send）、SystemWakeLock（systemd-inhibit）、CommandExecutor（sh -c）；linux-native-e2e（含 CURSOR 断言）CI 守护；打包 deb/tar.gz/rpm/AppImage |
| Android | 🔨 | 🔨 | P3 骨架 |
| iOS | ❌ | ✅ | P3 解码器完成（App 壳层待真机） |
| HarmonyOS | ⚠️ 权限评估 | ✅ | P4 骨架 |

## 路线图

- P0 原型：✅（SFU + Web 双端）
- P1 服务端生产化：✅（多核分片、coturn TURN、独立信令、BitrateController、simulcast 选层、netem 测试、/metrics）
- P2 桌面客户端：✅ 核心 + macOS 适配器完成（真实屏幕采集 E2E 验证）
- P3 移动端：🔨 iOS 硬解完成；Android 适配器骨架
- P4 鸿蒙 + 全平台收口：🔨 适配器骨架就绪；待各平台 SDK 真机实现
