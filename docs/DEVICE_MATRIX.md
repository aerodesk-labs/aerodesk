# 真机冒烟矩阵与硬件验收状态（#1/#2/#3/#4/#6/#8）

> 软件侧（主控端 + 被控端）已全部合入 main 并经 CI 自测（截至 2026-08-12，main 全绿）；
> 平台抽象（core `platform` trait + 泛型发布/观看管线）已完成并合入（#277/#278/#279）。
> 下表为**真机/干净环境验收**的当前状态。
> 未打勾单元格 = 需要对应硬件，代码已就绪（或明确为后续实现）。

## 矩阵

| 被控端 \\ 观看端 | Web | macOS | iOS | Android | Windows | Linux | HarmonyOS |
|---|---|---|---|---|---|---|---|
| macOS | ✅（web-e2e：观看/发布/文件/重连） | ✅（smoke + UI e2e；默认 h265 硬编 + 真实系统音频 #274/#276） | ✅（iOS 模拟器 e2e：H.265 硬解观看 macOS 流 #275 + 摄像头第二轨 #328/#340） | ⬜ 待 Android | ⬜ 待 Win | ⬜ 待 Linux | ⬜ 待鸿蒙 |
| Windows | ✅（Windows Edge e2e #409：观看+输入回传；DXGI 采集 + SendInput 注入 + 剪贴板 #281 + 远程光标 #406 + 显示器切换 #408 + BWE 码率反馈 #410 + 剪贴板注入 #411 + Windows 摄像头 #414；**远程 SFU 跨机联调 2026-08-15：合成源 viewer DECODED 187，Windows 被控端 RTP/光标/输入/剪贴板全通**，CI 编译/e2e 守护） | ⬜ | ⬜ | ⬜ | ✅（真机） | ⬜ | ⬜ |
| Linux | ⬜（X11/Wayland(PipeWire) 采集 + XTest/uinput/portal 注入 + VAAPI 硬编/硬解 + PipeWire 系统音频 + V4L2 摄像头 + 真实光标 + 剪贴板（文本/图片/注入）+ FilePicker/Notifier/SystemWakeLock/CommandExecutor #282/#283/#284/#286/#307/#311/#313/#317/#320/#323/#375/#386/#392/#394，CI 编译/e2e 守护） | ⬜ | ⬜ | ⬜ | ⬜ | ⬜ | ⬜ |
| Android | ⬜（MediaProjection + MediaCodec + 无障碍注入代码就绪；**模拟器经 TURN relay 已出帧解码（#201/#203）**，真机验收待设备） | ⬜ | ⬜ | ⬜ | ⬜ | ⬜ | ⬜ |

## 各 issue 验收门槛

| Issue | 软件侧（已合入 main） | 硬件/环境门槛 |
|---|---|---|
| #1 iOS 壳层 | 壳 + **H.264/H.265 硬解 + 音视频分流 + PCMU 播放 + iPad 支持 + 设置持久化**（#275）；模拟器 e2e（含 h265 观看 macOS） | iPhone 真机（A12+）：观看 macOS 流 |
| #2 Android 真机 | 观看端 MediaCodec 渲染 + 被控端 MediaProjection/硬编/无障碍注入 + Android 14 前台服务（#156/#165/#187）；APK CI 守护 | Android 真机（API 26+）：端到端画面 + 输入 |
| #3 Windows | DXGI 采集/缩放 + MF(h264_mf/hevc_mf) 硬编 + DXVA2 硬解（#378/#383/#405）+ WASAPI 音频（#321）+ SendInput 注入 + 剪贴板文本/图片（#281/#383/#393）+ 远程光标（#406）+ 显示器切换（#408）+ BWE 码率反馈（#410）+ 剪贴板注入（#411）+ 开机自启（#402）+ Windows 摄像头（#414）+ VDD（#159/#188）；Windows UI e2e + Windows Edge e2e（#409）+ 显示器切换 e2e（#413）CI 守护；远程 SFU 跨机联调已通（2026-08-15） | Win10/11 真机：端到端 + 多显示器切换验收 |
| #4 Linux | X11/Wayland(PipeWire) 采集 + XTest/uinput/portal 注入 + x264/OpenH264 回退 + **VAAPI 硬编/硬解优先（#282/#284）** + **Wayland/PipeWire 采集（#286）** + 剪贴板文本（#283）+ **CLI 被控端（#307/#311）** + **PipeWire 系统音频（#317）** + **portal 注入（#320）** + 图片剪贴板（#323）+ **SystemWakeLock/CommandExecutor（#375）** + **V4L2 摄像头（#386）** + **真实光标（#392）** + **FilePicker/Notifier/剪贴板注入（#394）** + linux-native-e2e（含 CURSOR 断言）CI 守护 | Linux 真机：X11/Wayland 端到端 + VAAPI/uinput 真机验收 |
| #6 HarmonyOS | NAPI 规约 ✅（docs/HARMONYOS.md，tmp/ohos-check） | DevEco + OHOS NDK + 鸿蒙真机（ring 交叉编译） |
| #75 鼠标控制 | 远程光标渲染 ✅（#86）、输入全事件 e2e ✅（#95）、高 DPI/多显示器坐标映射 ✅（#105）、远端光标叠加默认关（对齐 RustDesk/TeamViewer，#274） | 多显示器真机高 DPI 验证 + Windows/Linux/Android 注入真机验收 |
| #271 剪贴板 | **macOS/Windows/Linux 文本双向同步**（#281/#283 + 既有 macOS）；**macOS 图片读写已合**（#300 NSPasteboard PNGf），Linux 图片已合（#323）；富文本待做 | — |
| #277 平台抽象 | **core `platform` trait 全部实现 + 平台重复 trait 收敛 + 消费方泛型化**（publisher_generic + run_viewer_generic，#278/#279）：MediaSource/Encoder/Decoder/Renderer/InputInjector/AudioSink/AudioCapturer/Clipboard/CursorSource/Permissions/CameraSource/FilePicker/AppShell/VirtualDisplay/Notifier/CommandExecutor（#330）；键盘映射 macOS/Windows VK/Linux keysym | 平台真机批次（Windows WASAPI→Windows Codex、Linux PipeWire、Android JNI、macOS AVFoundation 摄像头） |
| #330 平台抽象第五轮 | **CommandExecutor（bash/远程命令）trait 化**（#330）：core `platform` 新增 CommandExecutor（run_command/read_file/write_file/list_processes/kill_process），`cmd_exec` 收敛为策略层（危险拦截/白名单/审计）+ 原始执行委托；core 提供 DefaultCommandExecutor（unix sh -c / Windows cmd /C），macOS 适配器 `MacCommandExecutor` 实现 trait；Windows/Linux 适配器由各自 agent 补充 | — |
| #334 平台抽象第六轮 | **SystemWakeLock（保持唤醒）trait 化**（#334）：core `platform` 新增 SystemWakeLock/WakeGuard + 默认 Noop；macOS `MacSystemWakeLock`（caffeinate -d/-i）替代 `capture::KeepAwake`，CLI 发布端接入；Windows SetThreadExecutionState / Linux systemd-inhibit 由各自 agent 补充 | — |
| #8 压测 | 工具链/报告 ✅（#38）、netem ✅ | 干净环境 4K60 基线 + 真机矩阵 |
| #58 工具栏媒体控制 | 画质选层 ✅、音频链路 ✅（PCMU/Opus → SFU → viewer + UI 静音）、**真实系统音频采集已接入**（SCK audio → Opus/PCMU，#276）、显示器切换 ✅（viewer `--display N` → SFU control 转发 → publisher 重建采集，`scripts/display-e2e.sh` 守护）；e2e 已接入 macOS CI | 多显示器真机切换验收 |

## 虚拟显示器冒烟（ADR-0001/0002/0003，#140）

> 虚拟屏为被控端独立输出面（AI 远控 #109 的基础）；硬件到位后按平台一条命令冒烟。

| 平台 | 方案 | 冒烟命令 | 状态 |
|---|---|---|---|
| Windows | Parsec VDD + `aerodesk-windows` vdd 模块 | `scripts/windows-vdd-smoke.ps1 -Install`（管理员，先 `cargo build -p aerodesk-windows --release --examples`） | 代码 ✅ / 待 Win 真机 |
| macOS | BetterDisplay CLI | `scripts/macos-vdd-smoke.sh`（需 BetterDisplay 2.2.x+ 运行） | 设计 ✅ / 待 mac 真机/无头 |
| Linux | VKMS + krfb-virtualmonitor | `scripts/linux-vdd-smoke.sh`（KDE Plasma 6 / Wayland） | 设计 ✅ / 待 Linux 真机 |

## 自动验证现状（2026-08-15，main 全绿；#274-#284/#307-#323/#375-#394/#402-#414 已合入）
- CI 三平台（macOS/Ubuntu/Windows）：cargo fmt/clippy/test 全绿 ✅
- macOS e2e：web 观看/发布/文件上传/自动重连、SFU 准入配额、audio/simulcast/display、cursor、record、multipop/popreg、bridge-fallback ✅（#280 修复 SCTP abort 误判后稳定）
- Windows/Linux UI e2e（viewer 真实媒体 + 输入；Linux 走 VAAPI 硬解优先 #284）✅；iOS 模拟器 e2e（viewer 解码，含 h265 #275；本地 PUBLISHER_CAMERA=1 验证摄像头第二轨 #340）✅；Android APK 构建 ✅
- **远程 SFU 跨机联调（129.226.150.174:14703/14778，2026-08-15）**：本机 Windows 客户端 → 远端 signal/SFU → 本地 viewer——合成源 h264_mf `RECEIVED 375 frames / 4 keyframes / DECODED 187` ✅；Windows 被控端（DXGI）RTP 跨机流动（112 frames/259KB）+ 真实光标回传 + 输入回传注入 + 剪贴板回传 ✅；远端 8/13 旧二进制（DTLS 超时）已升级到 main release（旧件备份保留）
- 平台抽象 CI：#278/#279 全平台编译 + e2e 全绿（Windows/Linux 真机编译验证键盘映射/MediaSource/InputInjector）
- 本机（共享开发机）：smoke/web-file/web-reconnect 多次全 PASS ✅

## 硬件就绪后的验收 runbook

> 通用：每项验收都要贴证据（截图/日志/报告链接）到对应 issue，填上表 ✅ 后关闭。
> 信令地址 UI/CLI 会自动补协议与 `/ws` 路径（`aerodesk_core::signaling::normalize_signal_url`），
> 可只填 `host:port`；公网默认 `wss://`，局域网明文自建请显式写 `ws://<LAN IP>:3003`。

### 0. 宿主机（被控端所在机器）起服务
```sh
cargo build -q -p aerodesk-sfu -p aerodesk-signal -p aerodesk-cli
RECORD_DIR=/tmp/aerodesk-acceptance ./target/debug/aerodesk-sfu   # 分片服务，:3002/metrics
./target/debug/aerodesk-signal                                     # WS :3003（明文，开发）/ WSS :3001（TLS）
```
- 真机连宿主机用 **LAN IP**（不是 127.0.0.1）；放行端口：3001/3003（WS/WSS）、3478/5349（TURN，见 docs/TURN.md）、WebRTC UDP 动态端口。
- 无 JWT 时可不设 `JWT_SECRET`；生产按 docs/DEPLOYMENT.md 配置。
- 快速全链路自检（本机）：`scripts/smoke.sh`（sfu+signal+publisher+viewer 自动起停并断言媒体/输入）。

### 1. macOS × macOS（已 ✅，PR #49，无需外部硬件）
```sh
./target/debug/aerodesk-cli --role publisher --encoder screen --signal ws://127.0.0.1:3003 --room accept
./target/debug/aerodesk-cli --role viewer    --signal ws://127.0.0.1:3003 --room accept
```
或桌面 UI：`cargo run -p aerodesk-ui`（连接页填 `127.0.0.1:3003`）。

### 2. iOS 真机（#1，iPhone A12+）
1. 构建：`bash scripts/build-ios-lib.sh all` → `cd ios && xcodegen generate`（双 slice 已由 CI 守护）
2. Xcode 打开 `ios/AeroDesk.xcodeproj`：选真机 + 开发者签名 → Run（或 Archive 出 ipa/TestFlight）
3. App 内填：服务器 `ws://<宿主机LAN IP>:3003`、房间 `accept` → 连接（支持 H.264/H.265 硬解 + 音频播放 #275；iPad 全方向/分屏）
4. 验收：看到 macOS 被控端画面（默认 h265 硬编）；拖动/点击/键盘回传生效；能听到被控端系统音频（发布端 `--audio`）；`AVSampleBufferDisplayLayer` 低延迟路径
5. 证据：截图 + 无报错日志 → 关 #1

### 3. Android 真机（#2，API 26+）
1. 构建：`bash scripts/build-android-lib.sh` → `cd android && JAVA_HOME=… ANDROID_HOME=… ./gradlew assembleDebug`（在 APFS 卷；exFAT 限制见 PACKAGING.md）
2. 安装：`adb install app/build/outputs/apk/debug/app-debug.apk`
3. 观看端：填 `ws://<LAN IP>:3003` + 房间 → 连接（MediaCodec 硬解 + Surface 渲染）
4. 被控端：PublisherActivity 请求 MediaProjection 录屏授权；系统设置开启 AeroDesk 无障碍服务（输入注入）
5. 验收：观看端出画 + 输入回传；被控端采集推流被其它观看端接收
6. 证据：截图 + logcat 无 crash → 关 #2

### 4. Windows 真机（#3，Win10/11）
1. 构建：`cargo build -p aerodesk-cli --release`（Windows 工具链；OpenH264 软编/软解已接入）
2. 被控：`aerodesk-cli.exe --role publisher --signal ws://<host>:3003 --room accept`（DXGI 采集 + SendInput 注入）
3. 观看：`aerodesk-cli.exe --role viewer --signal ws://<host>:3003 --room accept`（DXVA2 硬解优先 #405，OpenH264 软解回退）
4. 验收：双向画面 + 输入回传；记录编码/解码器与帧率
5. 证据：截图 + 日志 → 关 #3

### 5. Linux 真机（#4）
1. 构建：`cargo build -p aerodesk-cli --release`（依赖 libx11/xkbcommon/x264-dev，见 CI 系统依赖）
2. 被控：publisher（X11/Wayland-PipeWire 采集 + XTest/uinput/portal 注入 + VAAPI 硬编/硬解 + V4L2 摄像头 `--camera`/`--list-cameras`）/ 观看：viewer（VAAPI 硬解优先，OpenH264 回退；远程光标 CURSOR 断言）
3. Wayland 会话：PipeWire 采集 + portal 注入已实现（需 xdg-desktop-portal 授权）；uinput 注入需 root/udev 规则（XTest 免权限）
4. 证据：截图 + 日志 → 关 #4

### 6. HarmonyOS（#6，DevEco + OHOS NDK + 真机）
1. DevEco Studio 打开仓库，配置 OHOS NDK；`export CC_aarch64_unknown_linux_ohos=<NDK>/llvm/bin/clang`
2. `cargo check -p aerodesk-ohos --target aarch64-unknown-linux-ohos`（ring 需要 NDK 工具链，当前阻塞点）
3. 按 docs/HARMONYOS.md 的 NAPI 规约实现 ArkTS 桥（connectViewer/takeFrame/…）→ 真机验收
4. 证据 → 关 #6

### 7. Web 观看（已 ✅ web-e2e）
浏览器打开 `web/index.html`（或部署静态站），填 `ws://<host>:3003` + 房间；观看/发布/输入与原生端互通。

### 7.5 #58 工具栏媒体控制验收（画质/音频/显示器）

软件侧已全部合入（PR #65/#68/#69，三个 e2e 已接入 macOS CI 守护）：
```sh
# 画质选层（SFU 按 rid 转发，f 平均帧 ≈9x q）
./scripts/simulcast-e2e.sh
# 音频（publisher 合成 PCMU → SFU → viewer；--mute-audio 静音丢弃）
./scripts/audio-e2e.sh
# 显示器控制链路（viewer --display N → SFU control 转发 → publisher）
./scripts/display-e2e.sh
```
真机验收（对应 issue 评论贴证据）：
1. **音频真实采集/播放**：macOS 端 SCK 系统音频采集已接入（`--audio`，#276），观看端出声且「音频」按钮静音后无声；iOS 观看端已支持 PCMU 播放（#275）
2. **多显示器切换**：macOS 被控端 ≥2 显示器，`--encoder screen --display N` 或 viewer `--display N` 切换后画面/码率变化
3. 证据 → 关 #58

### 8. #8 4K60 压测（干净专用机）
1. 干净机标准：load < 2、无共享服务、专用网卡；被控/观看可同机或跨机
2. `scripts/bench.sh 4 2 60 3840 2160 60`（`BITRATE=12000000` 可选），产物在 `$REPORT_DIR`（JSON+Markdown）
3. 用 `scripts/loadtest.sh`/netem 场景补真机矩阵；更新 BENCHMARK.md 基线 → 关 #8

### 9. 收尾
- 每项验收在矩阵表填 ✅（日期/设备型号/环境）；对应 issue 评论贴证据后关闭
- 更新 docs/ACCEPTANCE.md 与 BENCHMARK.md 基线
