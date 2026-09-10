# 远端 WebRTC 流 → 本机 vdev 虚拟摄像头 + 声卡

把远端（另一台机器 / 云端）的 WebRTC 音视频流，变成本机可用的虚拟设备：
对方画面 → 本机 vdev 虚拟摄像头，对方声音 → 本机 vdev 虚拟声卡。

工具：**`aerodesk-vdev-bridge`**，位于本仓库
`crates/aerodesk-agent/src/bin/aerodesk-vdev-bridge/`（macOS）。
2026-09-10 从 vdev 仓库迁来，理由见文末。

## 数据流

```
远端发布端                              本机（aerodesk-vdev-bridge + vdev）
  │                                      │
  │  WebRTC（H264/HEVC + Opus）          │
  └──────── aerodesk SFU ────────────────┤
                                         │
        视频 track ──► str0m 收流 ──► depacketize ──► FFmpeg 解码（→ RGBA → BGRA）
        音频 track ──► str0m 收流 ──► Opus 解码 ──► PCM
                                         │
   视频 BGRA ──► FrameChannel(127.0.0.1:27890) ──► vdev 虚拟摄像头
                                                    └─ 设备侧滤镜（美颜/背景替换）
   音频 PCM  ──► AudioUnit ──► vdev-audio 虚拟声卡输出
```

注意：**滤镜不在本工具里**，它归 vdev 设备侧（`vdev-camera-ext`）——
虚拟摄像头对收到的帧自己成像。这样任何推帧方都享受同一套处理，
推流方不必各自再装一份滤镜依赖。

## 复用点

| 环节 | 复用 | 位置 |
|---|---|---|
| WebRTC 收流 | str0m（aerodesk fork） | `aerodesk-core` |
| SIP 信令 / ICE | `connect_viewer_sip`（REGISTER → INVITE 房间 → ICE 收敛） | `aerodesk-core/connect.rs` |
| 视频解码 | `FfmpegDecoder`（H264/HEVC/VP9/AV1） | `aerodesk-codec/decode.rs` |
| 音频解码 | `OpusDecoder` → PCM | `aerodesk-codec/audio.rs` |
| 推摄像头 | FrameChannel TCP 协议（36 字节头 + BGRA） | vdev `crates/vdev-camera-ext/src/frame_channel.rs` |
| 写声卡 | AudioUnit HALOutput → `vdev-audio` 设备 | 本工具 `audio.rs` |

## 用法

```bash
# 1. 起 aerodesk SFU + signal
# 2. 起本机 vdev 虚拟摄像头（vdev 仓库，扩展加载后监听 27890）
# 3. 起桥（本仓库根目录）
cargo run -p aerodesk-agent --bin aerodesk-vdev-bridge -- <signal_server> <room> [auth]

# 环境变量：
#   VDEV_WIDTH / VDEV_HEIGHT   输出分辨率（默认 1920x1080）
```

滤镜（设备侧，配在 **vdev 扩展**的环境里）：

```bash
# VDEV_FILTER="brightness,contrast,saturation,green,sharpen,beauty,whiten"
# VDEV_BG=blur    开启背景模糊（Vision 人像分割）
```

## 实现进度

- ✅ 视频桥：str0m 收流 → NAL 组装 → FfmpegDecoder → RGBA→BGRA 缩放 → FrameChannel
- ✅ 音频桥：Opus 解码 → SPSC ring → AudioUnit → vdev-audio 声卡
- ✅ 端到端验证通过：aerodesk publisher(`--noisy` x264 + `--audio-opus`) → SFU → 桥
  → H264 解码 420+ 帧(1920x1080) 推摄像头、Opus 解码 700+ 帧(960 样本/帧) 出声卡
- ✅ 设备侧滤镜（vdev-camera-ext）：VDEV_FILTER / VDEV_BG

## 为什么在本仓库而不是 vdev

它本质是「aerodesk 的观众端 + 一个本地设备 sink」：耗的是 aerodesk 的重依赖
（str0m / SIP / 编解码），而 vdev 那侧只是设备。放在 vdev 里会让 vdev 主仓库
被迫依赖 aerodesk，并随上游漂移而编不过——2026-09-10 一次实测就暴露三处
（ffmpeg-next 8→9、str0m git rev 不同源、`connect_live_role` 改名
`connect_viewer_sip`）。

而它对设备侧的接口全是**协议 / 系统 API**：
- 视频：TCP `127.0.0.1:27890` 的 36 字节头 + BGRA（跨进程协议，不是 crate 依赖）
- 音频：CoreAudio AudioUnit 写「vdev-audio」设备（系统 API）

所以本工具**不依赖任何 vdev crate**。滤镜之所以先下移到设备侧，就是为了保住这条
——否则本仓库反过来要 path 依赖 vdev 的 `vdev-filter`，那只是把耦合掉了个方向。
