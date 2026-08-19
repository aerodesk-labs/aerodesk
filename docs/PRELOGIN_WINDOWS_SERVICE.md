# 非登录态远程控制 · Windows 服务化实施方案(#470)

> 关联:#470(P0,本方案)/ #471 登录界面与锁屏画面+输入 / #472 UAC 穿透 / #473 安装器集成
> 状态:实施中(worktree `win-service`,分支 `worktree-win-service`,基线 main `63fbb0f`)
> 决策背景:深度 = 登录界面 + 锁屏 + UAC,Windows 先行;macOS 业界无原生方案(TCC 按用户授权),锁屏态另列,不在本方案范围。

## 1. 目标与非目标

**目标(P0,#470)**
1. 被控端以 Windows Service(SYSTEM)常驻:开机即起,不依赖用户登录;
2. 登录界面阶段设备**信令在线、可被呼叫**(媒体不可用,主控侧明确感知);
3. 用户登录后,服务 spawn 会话内进程,被控行为与现状一致;注销后服务自动回位在线;
4. CLI 管理子命令:安装/卸载/查询服务(管理员);
5. 服务日志(文件 + Windows 事件日志)。

**非目标(明确排除)**
- 登录界面/锁屏的画面与输入(#471);
- UAC Secure Desktop(#472);
- 安装器/升级链路(#473,本方案只提供 CLI 安装能力);
- macOS / Linux 任何形态;
- 主控端 UI 适配(仅复用现有「设备在线」展示)。

## 2. 现状盘点(代码级)

| 现状 | 位置 | 与本方案关系 |
|---|---|---|
| 单进程 UI,启动即连信令常驻(#450) | `aerodesk-desktop/src/main.rs:1443` `spawn_signal_presence` → `aerodesk_core::signal_presence` | presence 模块可直接被服务进程复用 |
| DXGI 采集 / 输入注入 / 权限 | `aerodesk-platform/src/windows/{capture,inject,permissions}.rs` | #471 复用,本方案不动 |
| HKCU Run 自启(登录后) | `aerodesk-platform/src/windows/autostart.rs`(#3/#417) | 保留为低权限选项,需与服务共存策略 |
| 配置:用户级 JSON | `aerodesk-desktop/src/main.rs:3852` `~/.aerodesk-settings.json` | SYSTEM 服务无用户 HOME,**必须新增机器级配置** |
| 无任何系统级常驻 | — | 本方案补齐 |

依赖现状:`aerodesk-agent` 已依赖 `aerodesk-core` + `aerodesk-platform` ✓;workspace 已有 `windows` crate 0.54;`windows-service` 需新增(社区标准、MIT,SCM 样板成熟)。

## 3. 架构设计

### 3.1 进程拓扑

```
开机 → SCM 启动 aerodesk-service(SYSTEM,Session 0)
        ├─ 读 %ProgramData%\AeroDesk\service-settings.json
        ├─ SignalPresence 常驻(device-id 与 UI 一致,auto_accept=false)
        ├─ 会话仲裁线程(WTS 事件驱动)
        │     ├─ 无 Active 会话(登录界面):服务信令在线(唯一在线方)
        │     ├─ WTS_LOGON   → WTSQueryUserToken → CreateProcessAsUser
        │     │                spawn 会话内 aerodesk-desktop.exe
        │     │                → 服务断开信令(让位)
        │     ├─ WTS_LOGOFF  → 服务重连信令(回位)
        │     └─ Lock/Unlock:记录状态,P1 扩展点
        ├─ (P1 挂点)winlogon desktop 采集线程占位 #471
        └─ 日志:文件(%ProgramData%\AeroDesk\logs)+ 事件日志

登录后 → aerodesk-desktop.exe(用户会话,现状行为:自带 presence #450)
```

### 3.2 关键决策

| # | 决策 | 选择 | 理由 |
|---|---|---|---|
| D1 | 服务入口形态 | **`aerodesk-agent` 新增 `--service` 运行模式** + `--install-service` 等子命令（#492 后拆分至 `aerodesk-host` 宿主二进制） | cli 已依赖 core+platform、已有 autostart 子命令形态;**不进 aerodesk-desktop**(会把 slint 拖进 SYSTEM 进程) |
| D2 | 服务配置归属 | **`%ProgramData%\AeroDesk\service-settings.json`(机器级)**,UI/CLI 安装与修改设置时同步写入 | SYSTEM 无用户 HOME;禁止依赖 `USERPROFILE`(服务态未定义)。字段:`server_default` / `device_id` / `token_default` / `inc_*` |
| D3 | 信令让位策略 | 状态机两态:**NoSession(服务在线)** ⇄ **UserSession(服务离线,会话进程在线)**,WTS_LOGON/LOGOFF 驱动 | 避免同 device-id 双 join 造成 SFU/会话管理双客户端;切换窗口数秒离线可接受(P1 媒体直发时再细化) |
| D4 | spawn 会话进程 | `CreateProcessAsUser` + `CreateEnvironmentBlock`(不 LoadUserProfile,desktop exe 自行处理),exe 取服务自身同目录 | 最小权限/最小依赖;后续加 `--minimized` 参数避免登录后弹窗打断 |
| D5 | 模块落点 | `platform/windows/service.rs`(SCM 生命周期)+ `platform/windows/session.rs`(WTS 枚举/事件/spawn)+ cli `--service` 主循环 + core `signal_presence` 原样复用 | 平台细节进 platform,业务编排进 cli,零改动 core |
| D6 | SCM 样板 | `windows-service` crate + `windows` crate(WTS API:`Win32_System_RemoteDesktop`,需 SE_TCB,服务持有) | 不手写 FFI;版本与树内 windows 0.54 收敛(实施时核对 feature gates) |
| D7 | 与 HKCU 自启共存 | `--install-service` 检测 HKCU 项并提示移除(不强制);UI 侧文案属 #473 | 双实例风险 = 同 device-id 双在线,先由让位状态机兜底 |

### 3.3 登录前呼叫流(P0 体验边界)

主控 join → 信令可达(presence 在线)→ 呼叫:服务 `auto_accept=false` 不应答媒体 → 主控侧超时提示「目标设备处于登录前状态」。**P0 不改 signal 协议**;协议级设备状态回执(`device_state=prelogin`)列为 #471 的前置小项,不阻塞本方案验收。

## 4. 实施步骤(里程碑,每步后 rustfmt/cargo test/clippy 自验)

**M1 SCM 服务骨架**
- [x] 引入 `windows-service`;`platform/windows/service.rs`:`install/remove/start/stop/status` + `run_service`(控制处理器 + 心跳)
- [x] cli:`--install-service/--remove-service/--service-status`(管理员检测,参照 autostart 子命令)
- [x] 日志初始化(文件 + 事件日志)
- 验收:`sc query` 全生命周期;非管理员安装被明确拒绝。

**M2 服务内信令常驻**
- [x] ProgramData 配置读写模块(缺省回退编译默认 + 事件日志告警)
- [x] `--service` 主循环:SignalPresence 启动/健康检查/重连
- 验收:装服务 → 注销到登录界面 → 信令侧设备在线(手测脚本断言 presence 注册)。

**M3 WTS 会话仲裁**
- [x] `platform/windows/session.rs`:`WTSEnumerateSessions`、`WTSRegisterSessionNotificationEx`(服务侧)、`WTSQueryUserToken`、`CreateProcessAsUser` 包装(非服务上下文 → detect-and-return,见测试策略)
- [x] 状态机(D3):LOGON 让位 / LOGOFF 回位;spawn `aerodesk-desktop.exe`
- 验收:登录 → desktop 进程起来、服务信令断开;注销 → 服务重新在线;全程 `sc query` RUNNING。

**M4 收尾**
- [x] HKCU 共存提示(D7)
- [x] `scripts/win-service-e2e.ps1`:安装→(注销/无会话)→断言在线→登录→断言让位→卸载
- [x] README/docs 服务章节;PR 按 #470 验收清单逐条对照

## 5. 测试与验证策略

- **单元**:service/session 状态机纯逻辑可测;WTS/SCM 包装在非服务上下文用「检测条件 → 不满足则 stderr 打印并 return」(RULE_可达性第 3 条,禁 skip 凑绿);
- **CI**:GitHub Windows runner 具管理员权限,新增 job step 跑 `--install-service → --service-status → --remove-service` 冒烟 + `--service` 短跑(信令连测试容器,若 CI 无信令则断言启动/心跳);
- **手测**(VM/物理机,脚本无法覆盖的):真实登录界面在线性、Logon/Logoff 让位切换、重启后全程;
- 本地门禁:`cargo test -p aerodesk-platform -p aerodesk-agent`、clippy、rustfmt(worktree 内,提交前必过)。

## 6. 风险与缓解

| 风险 | 缓解 |
|---|---|
| `windows-service`/`windows` crate 版本/feature 收敛冲突 | M1 首件事核对树内 0.54 feature gates,必要时统一升级 |
| `CreateProcessAsUser` 细节(SE_TCB、环境块、桌面权限) | 先做最小 spike(手动 token 复现),再固化 session.rs;失败路径事件日志留痕 |
| 让位窗口主控短暂断线 | 产品预期内,文档标注;#471 细化 |
| 服务态误用用户路径(HOME/USERPROFILE) | D2 强制 ProgramData;代码评审 checklist |
| 与并行工作冲突(wt-dc-ready-fix #467) | 无文件交集;merge 顺序无依赖 |

## 7. 后续路线挂点

- **#471**:服务拓扑已留「winlogon 采集线程占位」;`session.rs` 的 Lock/Unlock 事件即锁屏态路由依据;前置小项 = signal 协议 `device_state` 回执;
- **#472**:复用 #471 的 `SetThreadDesktop` 切换机制,新增 secure desktop 激活检测;
- **#473**:InnoSetup `[Run]` 调 `--install-service`(本方案 CLI 即最终入口),UI 文案与同步策略在彼处实现。

## 8. 验收映射(#470 验收标准 → 本方案)

| #470 验收 | 由 |
|---|---|
| 1 重启停登录界面信令在线、可呼叫 | M2 + M3(手测脚本) |
| 2 登录后会话进程恢复被控 | M3 |
| 3 注销/锁屏服务不掉线、状态正确 | M3 状态机 |
| 4 `--remove-service` 无残留 | M1 + M4 e2e |
| 5 本地门禁全绿、服务路径手测记录附 PR | 每里程碑自验 + M4 |

## 9. 使用与运维（#470 落地后）

```powershell
# 安装（管理员 PowerShell；装好即启动，并从用户设置同步机器级配置）
aerodesk-host.exe --install-service
aerodesk-host.exe --service-status      # 运行中 pid=…
aerodesk-host.exe --service-config      # %ProgramData%\AeroDesk\service-settings.json 生效值
aerodesk-host.exe --remove-service      # 停止并移除
```

- **配置**：`%ProgramData%\AeroDesk\service-settings.json`（server/device_id/token/spawn_ui/ui_exe），
  安装时自 `~/.aerodesk-settings.json` 同步；手改后 30s 内热重载。
- **日志**：`%ProgramData%\AeroDesk\logs\service.log`（ProgramData 不可用回退 stderr）+
  Windows 事件日志（source=AeroDeskService；未注册消息 DLL 时“找不到描述”属预期）。
- **生命周期冒烟**：`scripts/win-service-e2e.sh`（Git Bash，管理员）。

### 人工联调清单（VM/物理机，脚本不可自动化）

1. 装服务 → 注销 → 登录界面：signal 日志应见设备 Join 在线（NoSession 态）；
2. 登录界面输入凭据登录：服务日志「WTS Logon→让位」，桌面端被 spawn；
3. 注销：服务日志「WTS Logoff→回位 NoSession」，信令重新在线；
4. 重启整机重复 1–2（AutoStart 生效）。
