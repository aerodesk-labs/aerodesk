# 非登录态 P1 · 登录界面与锁屏态画面+输入实施方案(#471)

> 依赖:#470(服务化框架,已实现待合);分支 `feat/winlogon-capture` 堆叠于 `worktree-win-service`。
> 目标:主控在**登录界面**看到画面并输入凭据登录;**锁屏态**看到锁屏并输入密码解锁;登录完成无缝切回会话内采集。

## 1. 核心约束(Windows 机制事实)

| 约束 | 后果 |
|---|---|
| 登录界面渲染在 console 会话的 `winsta0\winlogon` desktop | 用户会话进程(现有 `SendInputInjector`)不可见、不可注入 |
| `SendInput` 只作用于**调用进程所在会话** | session 0 服务进程注入无效 |
| 登录界面阶段无用户 token | `WTSQueryUserToken`(#470 M3 用)不可用 |
| UAC Secure Desktop 归 #472 | 本方案只覆盖 winlogon/锁屏 |

## 2. 关键解法:winlogon token + console 会话助手

登录界面阶段 console 会话内恒有 `winlogon.exe`(SYSTEM,winlogon desktop 可达)。标准路径(与 RustDesk/UVNC 同款):

```
aerodesk-service(S0,SYSTEM)——信令/编码/发送(#470 既有)
  │ 枚举 console session 的 winlogon.exe
  │ OpenProcess(PROCESS_QUERY_LIMITED_INFORMATION)→ OpenProcessToken
  │ → DuplicateTokenEx(Primary) → CreateProcessAsUser
  ├─ spawn aerodesk-helper.exe(console 会话,lpDesktop="winsta0\winlogon")
  │    ├─ 采集:DxgiCapturer(复用;多屏/缩放已有)
  │    └─ 注入:SetThreadDesktop(winlogon)→ SendInput(复用 InputInjector trait)
  └─ IPC:named pipe(帧下行 / 输入上行 / 心跳)
```

- **采集归属(实测项 A/B)**:DDA 按适配器 output 抓的是 GPU 输出,服务(S0)直抓登录界面**可能直接可行**(RustDesk 同路径);不可行(黑屏/无更新)则 helper 抓——两条路径共用 `DxgiCapturer`,仅进程位置不同,方案不锁死。
- **注入无争议**:必须 helper(winlogon desktop 的 SetThreadDesktop + SendInput)。
- **编码归属(实测项 C)**:帧在服务进程编码(复用 `aerodesk_codec::encode::FfmpegEncoder`,S0 无 D3D 上下文时自动回退软编,#3 机制已有);helper 只做采集+注入保持轻薄。

## 3. 状态机扩展(#470 D3 之上)

```
NoSession(服务 presence 在线)
  ├─ 主控呼叫 → 接听(本次起 P0 的"不接听"解除)→ spawn helper → 采集/注入管道建立
  ├─ WTS Logon → 杀 helper → spawn desktop exe(#470 既有)→ UserSession
UserSession
  ├─ WTS Lock(#470 已透传事件)→ 路由:锁屏画面实测(D 项)——用户会话采集器
  │   锁屏后 DDA 输出仍有效则不切换;解锁输入框(winlogon desktop)仍需 helper
  │   (用户 token 版)→ 半切换:采集留会话内,输入走 helper
  └─ WTS Logoff → NoSession(服务回位)
```

- **锁屏注意**:锁屏画面(lock screen app)在**用户会话**,解锁凭据框在 **winlogon desktop**——采集与注入的归属可能不同,按实测矩阵 D 定路由。
- **signal 协议 `device_state` 回执**(前置小项):presence/Join 附 `prelogin`/`locked`/`online` 状态,主控 UI 显示"目标设备处于登录界面",替代 P0 的呼叫超时体验。

## 4. 实测矩阵(VM,方案分支先行验证)

| # | 项 | 结果栏 |
|---|---|---|
| A | 服务(S0)直抓登录界面 DDA:有帧/黑屏/无更新 | 待测 |
| B | helper(console 会话)抓登录界面 | 待测(若 A 黑) |
| C | S0 服务进程内 FfmpegEncoder:硬编可用/软编回退 | 待测 |
| D | 锁屏后:用户会话采集器输出 vs helper 输出 | 待测 |
| E | 登录完成后杀 helper 切 desktop 的断流时长 | 待测(目标 <1s) |

## 5. 模块落点

| 模块 | 位置 |
|---|---|
| winlogon token 获取 + helper spawn | `platform/windows/session.rs` 扩展(复用 `spawn_with_token` 思路,token 来源换成 winlogon.exe) |
| helper 进程(采集+注入+pipe 客户端) | `aerodesk-cli` 新子命令 `--logon-helper`(单二进制多角色,与服务同产物) |
| pipe IPC | `platform/windows/` 新 `helper_pipe.rs`(帧/输入/控制帧,长度前缀 + bincode 或手动,先简单) |
| 服务侧路由 | `cli/src/service_run.rs` Supervisor 扩展:接听呼叫 → helper 管道;Lock 事件 → 半切换 |
| device_state | `aerodesk-protocol/signal.rs` + signal/SFU 透传(#467 dc_ready 同款 `#[serde(default)]` 兼容模式) |

## 6. 里程碑

- **M1** winlogon token + helper spawn + pipe 骨架(心跳互通);实测 A/B/C 出结论
- **M2** 登录界面采集→服务编码→SFU→viewer 可见(e2e:viewer 收帧计数)
- **M3** 注入:viewer 输入 → helper SendInput → 登录界面输入框;登录全程(输入凭据→进桌面→无缝切换)VM 手测过
- **M4** 锁屏路由(实测 D)+ device_state 回执 + `scripts/win-logon-e2e.sh`(服务生命周期 + helper 管道自动部分)+ 人工联调清单更新

## 7. 风险

| 风险 | 缓解 |
|---|---|
| S0 直抓黑屏(A 失败) | helper 抓(B 路径),帧过 pipe,延迟 +~1 帧传输 |
| S0 硬编不可用(C) | 软编回退已有(#3);码率档位服务态降档 |
| winlogon token 枚举时机(winlogon.exe 未起/已退) | 轮询重试 + 事件兜底(WTS 通知),helper 崩溃自动重启 |
| 杀 helper 切 desktop 断流 | 主控侧已有关键帧重请求;断流窗口记实测 E |
| 注入到凭据框被 Secure Attention 干扰 | Ctrl+Alt+Del 级操作不做(明确排除,SAS 由 #472 后评估) |

## 8. 验收(对齐 issue #471)

1. VM 重启停登录界面:主控可见画面,键盘输入凭据完成登录,进入桌面无缝切换(<1s 黑屏);
2. 已登录 + Win+L:可见锁屏,可输入密码解锁;
3. 全程服务不重启、信令不断线;`device_state` 在主控侧正确显示;
4. 无回归:已登录正常会话采集/输入与现状一致(CI 全矩阵绿)。
