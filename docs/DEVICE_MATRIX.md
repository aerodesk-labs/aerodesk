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
| #58 工具栏媒体控制 | 画质选层 ✅（publisher `--simulcast` q/h/f + SFU 按 rid 转发，`scripts/simulcast-e2e.sh` 守护）；音频/显示器待接入 | 音频链路 + 多显示器切换（macOS 先行） |

## 自动验证现状
- CI 三平台（macOS/Ubuntu/Windows）：cargo fmt/clippy/test + macOS e2e smoke ✅
- CI 移动端构建回归：iOS App 双 slice（模拟器 + 真机，未签名）+ Android APK（Rust .so + Gradle assembleDebug）✅
- 本机（共享开发机，负载 8/10）：smoke/loadtest/web-e2e/选层 e2e 均跑通

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
3. App 内填：服务器 `ws://<宿主机LAN IP>:3003`、房间 `accept` → 连接
4. 验收：看到 macOS 被控端画面；拖动/点击/键盘回传生效（被控端光标与应用响应）；`AVSampleBufferDisplayLayer` 低延迟路径
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
3. 观看：`aerodesk-cli.exe --role viewer --signal ws://<host>:3003 --room accept`（DXVA2 硬解）
4. 验收：双向画面 + 输入回传；记录编码/解码器与帧率
5. 证据：截图 + 日志 → 关 #3

### 5. Linux 真机（#4）
1. 构建：`cargo build -p aerodesk-cli --release`（依赖 libx11/xkbcommon/x264-dev，见 CI 系统依赖）
2. 被控：publisher（X11 采集 + XTest 注入）/ 观看：viewer（VAAPI 解码）
3. Wayland 会话：PipeWire 采集未实现，先以 X11/XWayland 会话验收；uinput 注入需 root/udev 规则（当前用 XTest）
4. 证据：截图 + 日志 → 关 #4

### 6. HarmonyOS（#6，DevEco + OHOS NDK + 真机）
1. DevEco Studio 打开仓库，配置 OHOS NDK；`export CC_aarch64_unknown_linux_ohos=<NDK>/llvm/bin/clang`
2. `cargo check -p aerodesk-ohos --target aarch64-unknown-linux-ohos`（ring 需要 NDK 工具链，当前阻塞点）
3. 按 docs/HARMONYOS.md 的 NAPI 规约实现 ArkTS 桥（connectViewer/takeFrame/…）→ 真机验收
4. 证据 → 关 #6

### 7. Web 观看（已 ✅ web-e2e）
浏览器打开 `web/index.html`（或部署静态站），填 `ws://<host>:3003` + 房间；观看/发布/输入与原生端互通。

### 8. #8 4K60 压测（干净专用机）
1. 干净机标准：load < 2、无共享服务、专用网卡；被控/观看可同机或跨机
2. `scripts/bench.sh 4 2 60 3840 2160 60`（`BITRATE=12000000` 可选），产物在 `$REPORT_DIR`（JSON+Markdown）
3. 用 `scripts/loadtest.sh`/netem 场景补真机矩阵；更新 BENCHMARK.md 基线 → 关 #8

### 9. 收尾
- 每项验收在矩阵表填 ✅（日期/设备型号/环境）；对应 issue 评论贴证据后关闭
- 更新 docs/ACCEPTANCE.md 与 BENCHMARK.md 基线
