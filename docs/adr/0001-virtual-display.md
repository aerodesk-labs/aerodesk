# ADR-0001：虚拟显示器方案选型与 Windows 首个落地（#114）

- 状态：已采纳（2026-08-08）
- 关联 Issue：#114（调研）、#3（Windows 适配器落地）、#109（AI 远控联动）、#75（鼠标控制）
- 决策：**Windows 先行，采用 Parsec Virtual Display Driver 0.45 + `parsec-vdd-rust` 0.0.1**，在 `aerodesk-windows` 增加 `vdd` 模块；macOS 用 BetterDisplay CLI、Linux 用 VKMS 作为后续落地平台。

## 背景

当前 aerodesk 直接捕获物理屏（Windows WGC/DXGI、macOS AVFoundation/ScreenCaptureKit、Linux PipeWire/X11），远程会话没有独立输出面：

- 被控端必须依赖物理显示器，分辨率/刷新率受限；
- 测试时捕获物理屏会干扰本机操作（如把光标钉在屏幕中心）；
- AI 远控（#109）需要"看得见"的稳定虚拟输出面，虚拟显示器是其物理基础。

目标：为被控端提供可随会话创建/销毁的独立虚拟显示器（分辨率/刷新率可配置），并让采集器从虚拟屏而非物理屏取帧。

## 候选方案对比

### Windows（首选落地平台）

| 方案 | 签名 | 规格 | 集成成本 | 结论 |
|---|---|---|---|---|
| Parsec VDD（`nomi-san/parsec-vdd`） | 已签名 | IddCx 1.5、4K@240Hz、硬件光标、无 HDR | 低：`parsec-vdd-rust` 提供 Rust API，MIT | **选定** |
| VirtualDrivers/Virtual-Display-Driver | 已签名 | IddCx 1.10、HDR、硬件光标 | 高：定制需 EV/WHQL 签名 | 后续评估 |
| virtual-display-rs | 未签名 | 纯 Rust | 高：Win11 需测试模式/驱动签名 | 不选 |
| usbmmidd_v2 / RustDeskIddDriver / IddSampleDriver | — | 旧/弱 | — | 参考实现 |

Parsec VDD 关键事实（来自上游 README 与社区验证）：

- 驱动版本选 **0.45**（Win10 21H2+，色彩更佳）；用 `nefconw` 管理员安装；
- 已签名，4K@240Hz、硬件光标，无 HDR（HDR 需改 `mm.dll` EDID，不纳入本期）；
- **心跳保活**：客户端需周期性下发 Update（`parsec-vdd-rust` 建议 <100ms 一次），约 1s 不 ping 全部虚拟显示器会被拔出；
- 自定义分辨率走注册表 `HKLM\SOFTWARE\Parsec\vdd`（≤5 项），本期用驱动默认 + 运行时 `ChangeDisplaySettings` 配置；
- 已知坑：与 Parsec Privacy Mode 冲突；Win10 拔中间屏布局缓存 quirk（建议右到左拔）；登录前虚拟屏不生效（需自动登录/服务化）。

### macOS

| 方案 | 说明 |
|---|---|
| BetterDisplay（CLI） | 成熟、脚本化（`betterdisplaycli create -devicetype=virtualscreen ...`，需 BetterDisplay 2.2.x+ 运行），$21.99 买断；集成成本最低 |
| 私有 `CGVirtualDisplay` | Sidecar 同款，脆弱、系统升级可能失效、App Store 上架受限，仅作探索 |
| 硬件诱骗头 | 最稳，但需物理设备，仅兜底 |

### Linux

| 方案 | 说明 |
|---|---|
| VKMS（内核自带） | `modprobe vkms` + `KWIN_DRM_DEVICES=/dev/dri/card1`，无第三方驱动；需作为第二 DRM 设备 |
| krfb-virtualmonitor（KDE Plasma 6） | Sunshine 在用，与 Plasma 绑定 |
| EVDI / xrandr / Xvfb | 备选/兜底 |

## 决策与理由

1. **Windows 先行**：被控端最常驻平台，且只有 Windows 有已签名、可脚本化的虚拟显示器生态；CI 已有 `cargo check -p aerodesk-windows`（windows-latest），实现可被 CI 验证。
2. **Parsec VDD 0.45 + `parsec-vdd-rust` 0.0.1**：已签名（免 EV/WHQL）、4K@240Hz 满足 4K60 目标、MIT、Rust API（add/remove/configure/status，thread-safe 句柄）；`Virtual-Display-Driver` 功能更强但定制需商业签名，留作后续升级路径。
3. **心跳线程内置于管理器**：`vdd_update` 每 100ms 一次，保证虚拟屏在会话存活期不被动拔出；`Drop` 时停心跳并移除全部虚拟屏，避免残留。

## Windows 集成技术方案（aerodesk-windows `vdd` 模块）

```
VirtualDisplayManager
 ├─ new()        : query_device_status(VDD_CLASS_GUID, VDD_HARDWARE_ID)==Ok
 │                 → open_device_handle(VDD_ADAPTER_GUID) → 启动心跳线程(100ms)
 ├─ add_display(w,h,hz) : vdd_add_and_identify_display → change_mode(w,h,hz)
 ├─ remove_display(i)   : vdd_remove_display
 ├─ display_count()
 └─ Drop        : 停心跳 → 移除全部 → close_device_handle
```

- 依赖：`parsec-vdd-rust = "0.0.1"`（Windows only）。其传递依赖 `windows 0.62` 与现有 `aerodesk-windows` 的 `windows 0.58` 并存，Cargo.toml 用 **package 重命名** `windows062 = { package = "windows", version = "0.62", features = ["Win32_Foundation"] }` 仅在本模块命名 `HANDLE`，不升级既有 0.58 代码（避免 DXGI/MF 模块回归）。
- `HANDLE` 非 Send/Sync，心跳线程用 `unsafe impl Send/Sync` 包装（仅用于 DeviceIoControl，Drop 先 join 再 close，线程退出后才关句柄）。
- 非 Windows 平台编译为 stub（`new()` 返回 `Unsupported`），保证 workspace 在 macOS/Linux 可编译测试。
- 会话集成：被控端会话建立时 `new()` + `add_display(3840,2160,60)`（默认 4K60，可配置），会话结束 `Drop` 自动回收；采集器后续可从虚拟屏取帧（与现有 DXGI/WGC 采集器同一接口，本期只交付 VDD 生命周期，取帧接线随 #3 真机验收一起做）。
- 错误处理：驱动未安装/状态异常 → 明确报错提示安装 `nefconw -i`，**不静默回退**（吸取 #11 TLS 静默回退教训）。

## 风险与缓解

| 风险 | 缓解 |
|---|---|
| 驱动未安装/签名被禁 | `new()` 返回 `DriverNotReady` 明确错误；README/ADR 写清安装步骤 |
| 心跳断连导致虚拟屏被拔 | 心跳 100ms（远低于 1s 窗口）；心跳失败即标记错误并回收 |
| Win10 拔中间屏布局缓存 quirk | 按 index 逆序移除（右到左） |
| 登录前不生效 | 文档注明需自动登录/服务化（后续 #109 服务化一并处理） |
| 与 Parsec Privacy Mode 冲突 | 文档提示使用 Parsec 时关闭 Privacy Mode |
| windows 0.58/0.62 并存 | 仅 vdd 模块用 `windows062` 重命名依赖，不扩散 |

## 与 #109 / #75 联动排期

- **#109（AI 远控）**：虚拟显示器是 AI"看得见"的稳定输出面；本批交付 VDD 生命周期后，#109 的虚拟屏接线（会话内起虚拟屏 + 采集虚拟屏帧）纳入其后续里程碑。
- **#75（鼠标控制）**：虚拟输出避免把物理屏光标钉住，是"避免干扰本机指针"的长期解法；本期不改变 #75 现有路径。

## 后续（不在本期）

- Windows 采集器切到虚拟屏取帧（DXGI 枚举虚拟屏输出，真机验证）；
- macOS BetterDisplay CLI 集成设计：**ADR-0002**（已定稿，待实现）；
- Linux VKMS + krfb-virtualmonitor 集成设计：**ADR-0003**（已定稿，待实现）；
- Parsec VDD 自定义分辨率注册表路径（>5 组模式时）。
