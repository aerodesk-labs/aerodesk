# ADR-0002：macOS 虚拟显示器集成设计（BetterDisplay CLI）

- 状态：已采纳（设计稿，2026-08-08；实现待 macOS 真机/无头 Mac 验收）
- 关联 Issue：#114（调研）、#3/#4（平台适配）、#109（AI 远控）、#75（鼠标控制）
- 上游决策：ADR-0001（Windows 先行，Parsec VDD）；本文档为 macOS 落地设计

## 背景

macOS 被控端目前直接捕获物理屏（ScreenCaptureKit/AVFoundation）。远程会话需要独立输出面：
无头 Mac 没有屏幕、有头 Mac 捕获物理屏会干扰本机操作、AI 远控（#109）需要稳定虚拟输出。

## 候选对比

| 方案 | 性质 | 规格/成本 | 集成成本 | 结论 |
|---|---|---|---|---|
| **BetterDisplay CLI**（waydabber/BetterDisplay） | 闭源付费 app（$21.99 买断）+ `betterdisplaycli` | 任意数量虚拟屏（dummy）、无头 Mac 兼容、URL schema/HTTP/CLI | 低：子进程调用 | **选定** |
| 私有 `CGVirtualDisplay` | 系统私有 API（Sidecar 同款） | 脆弱、系统升级可能失效、App Store 受限 | 高 | 仅探索 |
| 硬件诱骗头（HDMI dummy plug） | 物理设备 | 最稳 | 需硬件 | 兜底 |

BetterDisplay CLI 关键事实（已核对官方讨论/文档）：
- 创建虚拟屏：`betterdisplaycli create -devicetype=virtualscreen -virtualscreenname=<name> -aspectWidth=16 -aspectHeight=9`
  （宽高比为 16:9；分辨率/刷新率由 app 内虚拟屏配置或 `-width/-height/-refresh` 等参数决定，实现时以实际 CLI 版本为准）
- **前置条件**：BetterDisplay app 必须在运行（2.2.x+ 提供 CLI/自动化能力）；app 未运行 → CLI 报错
- 支持 headless Mac（远程访问场景官方支持）；可创建多个虚拟屏
- 虚拟屏作为普通显示器出现在系统显示器列表 → ScreenCaptureKit 可枚举/采集

## 集成设计（aerodesk-macos `vdd` 模块，未来实现）

```
VddManager (aerodesk-macos)
 ├─ new()      : 检测 BetterDisplay 是否运行（pgrep/`betterdisplaycli` 探测）→ 未运行报明确错误
 ├─ add(w,h,hz): 子进程调用 betterdisplaycli create（-devicetype=virtualscreen
 │                -virtualscreenname=aerodesk-<session> -aspectWidth=… -aspectHeight=…）
 │               → 轮询系统显示器列表（CGGetActiveDisplayList）确认虚拟屏出现
 ├─ remove()   : betterdisplaycli 删除对应虚拟屏（按 name/display id）；失败重试
 └─ Drop       : 会话结束删除虚拟屏，避免残留
```

- 采集联动：虚拟屏出现后，ScreenCaptureKit（`SCShareableContent` 显示器列表）会包含它，
  被控端采集器选择该虚拟屏输出（与物理屏同一采集接口，按 display id 选择）
- 会话集成：被控端会话建立时 `new() + add(3840,2160,60)`（默认 4K60，可配置），
  会话结束 `Drop` 自动删除
- 错误处理：BetterDisplay 未安装/未运行 → `DriverNotReady` 式明确错误（不静默回退，吸取 #11 教训）；
  create 超时（默认 10s）→ 报错并回滚
- 进程封装：CLI 为子进程调用（`std::process::Command`），超时/退出码严格校验

## 风险与缓解

| 风险 | 缓解 |
|---|---|
| 依赖第三方闭源 app（付费、需运行） | 文档明示前置条件；安装/授权脚本；兜底硬件诱骗头 |
| CLI 参数随版本变化 | 封装层隔离 + 版本探测（`betterdisplaycli --version`），实现时锁定验证版本 |
| 虚拟屏分辨率/刷新率受 app 配置限制 | 用 app 预置虚拟屏模板 + CLI 指定宽高比；验收矩阵覆盖 4K60 |
| 系统升级/隐私权限变化 | ScreenCaptureKit 权限引导已存在（#1/#75）；升级后回归 |
| 无头 Mac 无 UI | BetterDisplay 官方支持 headless 虚拟屏（远程访问场景） |

## 与 #109 / #75 联动

- #109：虚拟屏是 AI"看得见"的稳定输出面；macOS 虚拟屏接入后，#109 的会话内起虚拟屏 + 采集虚拟屏帧接线纳入其里程碑
- #75：虚拟输出避免把物理屏光标钉住（长期解法）

## 验收（未来，需 macOS 真机/无头 Mac）

- [ ] 有头 Mac：会话内创建虚拟屏 → ScreenCaptureKit 采集虚拟屏 → 远端可观看；会话结束虚拟屏消失
- [ ] 无头 Mac：无物理屏时虚拟屏可创建并被采集
- [ ] BetterDisplay 未运行时给出明确错误，不 panic 不残留
- [ ] 4K60 虚拟屏采集端到端可用（配合 #8 验收）
