# 生产交付冲刺周（2026-10-08 → 2026-10-15）——Win ↔ macOS 双向

> 触发：ToDesk 授权一周内到期（用户 2026-10-08）。
> 范围口径（用户确认）：**主控端与被控端都是 Windows 与 macOS，双向可用**。
> 本文件是本周的可核对台账：每天的批次、出口门禁、证据形态都在这里。
> 配套任务书与原始响应落在 `/Volumes/DataExt/tmp/aerodesk-delivery-week/`（不进仓库）。
> **2026-10-08 复核注**：这些路径不在仓库里，外部无法复核 → 本表这三行的「原始响应」按
> **unverified** 对待；要作为可核证据，需要把请求体/响应体（脱敏后）随提交入库。

## 1. 阶段与判定（含原始响应路径）

| 项 | 结论 | 概率 / 置信 | 原始响应（本机临时文件，**不进仓库 → 不可复核**） |
|---|---|---|---|
| 生命周期阶段 | **验收**（次高「测试」0.25） | p=0.59 / 置信 0.54 | `/var/folders/…/jev-stage-lifecycle.json` |
| 本周主线 | **自用最小闭环**（无人值守 + 公网连通 + 已签名包） | p=0.82 / 置信 0.78 | `/tmp/jev-aerodesk-week-mainline.json` |
| 第一优先项 | 公网连通（次高：mac 无人值守 0.22、跨平台验收 0.17） | p=0.60 / 置信 0.52 ⚠ | `/tmp/jev-aerodesk-week-d2-order.json` |

**⚠ 二次校验（置信 0.52 < 0.6）**：按 `RULE_决策类问题先问jev.md` 第 3 条不直接照做，改用客观事实复核——
公网连通确为前置条件，但它依赖「两端设备 + 两个网络同时在场」的人工窗口；而 macOS 无人值守是**两端里唯一零实现、需要新写代码**的一项（`crates/aerodesk-platform/src/macos*` 内无 launchd/守护实现），关键路径最长。
故**不作为串行顺序采用**，而是拆成两条互不冲突的线并行（`RULE_Issue批次化.md` 允许活跃主线 ≤ 4）：

- **线 A（需人工窗口）**：公网 NAT/TURN 实测 → D2 做本机可自动化的一半（内嵌 TURN + 候选类型观测），D4 做双网络实测。
- **线 B（纯代码，可无人值守推进）**：macOS 无人值守闭环 → D2/D3。

## 2. 现状台账（证据已逐条复核；不可复核或已失效者在本行标注）

| # | 能力 | 状态 | 证据 |
|---|---|---|---|
| 1 | 服务端 | 已部署健康，**已有真实流量**；**但跑的不是 main**（见 2.6） | 2026-10-08 15:50 复核：`curl -sk https://129.226.150.174:14701/healthz` → `{"pop":"pop-a","sip":{"tcp":true,"tls":true,"udp":true,"wss":true},"status":"ok"}`；`/metrics/prometheus` → `sip_registrations 4`、`sip_calls_established 5`。（原文引的 `14703` 明文口现已不在监听；`14701` 是 ops **HTTPS**，用 `http://` 探会得空响应 rc=52） |
| 2 | 安装包 | v0.4.0 全平台产物已在 Release | `AeroDesk-0.4.0.dmg`、`aerodesk-0.4.0-win64.msi/.zip`、`deb/rpm/tar.gz/AppImage` |
| 3 | 签名/公证流水线 | 可用 | `Build & Release` run `33290049676`（tag v0.4.0）success |
| 4 | 无人值守（口令面） | 已实现 | `SIP_DIGEST_USERS` 固定口令 + `POST /admin/temp-password`：`docs/SIP_SIGNALING.md` §11 |
| 5 | 无人值守（Windows 常驻） | 自启已实现、**缺实机证据**；**登录界面 helper 只有方案（M1–M4 未做）** | `crates/aerodesk-platform/src/windows/autostart.rs`（HKCU Run）——原文写的 `windows/autostart.rs` **路径不存在**；`crates/aerodesk-host` SYSTEM 服务；winlogon 登录界面 helper 见 `docs/PRELOGIN_WINLOGON_CAPTURE.md`，其中 M1–M4 仍是方案/占位（含 `(P1 挂点)…`），**不得读作「已实现」** |
| 6 | 无人值守（macOS 常驻） | **零实现** | 仓库内无 launchd/LaunchAgent/守护；`PRELOGIN_WINDOWS_SERVICE.md` §1 明确 macOS 无原生方案（TCC 按用户授权）不在该方案范围 |
| 7 | 跨平台互通 | **未验** | `docs/DEVICE_MATRIX.md`：只有 mac×mac、win×win 打勾，win×mac / mac×win 均「待」 |
| 8 | 公网 NAT | **未验**（交接项） | `docs/P0_ACCEPTANCE_REPORT_20260824.md` §4/§6 |
| 9 | 合并门禁 | 有洞 | `ci.yml` 仅 `pull_request` 触发；`/branches/main/protection` 的 `required_status_checks.contexts = []`、`checks = []` |
| 10 | 工作项账本 | 走 **walgit collab** 线程（不是 GitHub PR） | **更正（2026-10-08 复核）**：`git remote get-url origin` → `http://127.0.0.1:8081/gqf2008/aerodesk.git`，**origin 是 walgit**；GitHub（`aerodesk-labs/aerodesk`）只作镜像/发版。原文「origin 为 GitHub」是事实错误，账本按 `RULE_walgit协同记账.md` 走 collab 条目 |
| 11 | 文档口径 | **本条原判已撤回** | 复核：`grep -c 'ws://:3003' docs/ACCEPTANCE.md` → **0**。原文断言由独立评审指出不可复现，本批复核确认不成立；保留本行以留痕 |

### 明确不在本期交付面（写下来避免范围蔓延）

iOS / Android / HarmonyOS / Linux 真机；macOS **锁屏与未登录态**采集（业界无原生方案，与 `#470` 决策一致）；
UAC Secure Desktop（`#472`）；对外多租户与容量承诺（`#8` 压测基线留后续）。

## 2.5 D1 实测发现（2026-10-08）

| 发现 | 事实 | 处置 |
|---|---|---|
| 本机 UDP 5060 被 `freeswitch`(pid 34635) 占用 | `lsof -nP -iUDP:5060` → `freeswitch 34635`；aerodesk-signal 绑定 `0.0.0.0:5060` 后，内核把回包交给更具体的 `127.0.0.1:5060`，REGISTER 被 FreeSWITCH 回 **403 Forbidden** | 本轮先修 canonical 的 `scripts/smoke.sh`：SIP/ops 端口可用 `SIP_PORT` / `SIGNAL_OPS_PORT` 覆盖（冒烟用 15060/13001） |
| **写死 5060 的 e2e 脚本**（原文写 45 个，计数错） | 复核：`git grep -l 5060 -- scripts/ \| wc -l` → **46**（任何形式的出现）；其中真写死 `SIP_UDP_PORT=5060` 的：基线 `35ad688` **38** 个 → D1 修 `smoke.sh` 后 **37** 个 → D2 把 37 个改走 `scripts/lib/e2e-ports.sh`。客户端同默认 5060 → 本机跑任一 e2e 都会假红，且错误信息指向「口令错/鉴权」完全误导 | 记入 **D2 批次**（抽公共 helper 统一端口，不在 D1 夹带大面积脚本改动） |
| 冷编译实测 | `cargo build -p sfu -p signal -p agent -p desktop` 从空 target 到产出：**0 error**，4 个二进制，约 **9 分钟** | 后续批次可直接复用热 target |
| 冒烟基线 | `SIP_PORT=15060 SIGNAL_OPS_PORT=13001 bash scripts/smoke.sh d1-baseline 8` → `PASS input relay` / `PASS media receive` / `PASS no errors` | D1 出口证据 |

## 2.6 生产节点巡检（2026-10-08；**本节当天 13:09 被重部署推翻，以下为更新后的口径**）

**当前结论（2026-10-08 15:50 复核）：节点已在 13:09 重部署并受理真实流量；但它跑的是未合并分支
`feat/sip-default-tcp` 的构建，不是 main。**

| 事实 | 证据 |
|---|---|
| 已是 SIP 单栈四传输 | `ss -lntu` → `0.0.0.0:15060/udp`、`0.0.0.0:15060/tcp`、`0.0.0.0:5061/tcp`、`0.0.0.0:3061/tcp`（该机另有 FreeSWITCH 占 `10.3.0.9:5060` 与 `[::1]:5060`——注意**不是**本开发机那种 `127.0.0.1:5060` 形态） |
| `/healthz` 带 `sip` 四传输 | `curl -sk https://129.226.150.174:14701/healthz` → `{"pop":"pop-a","sip":{"tcp":true,"tls":true,"udp":true,"wss":true},"status":"ok"}`（14701 = ops **HTTPS**；14703 明文口已不监听） |
| 已有真实流量 | `/metrics/prometheus` → `sip_registrations 4`、`sip_calls_established 5` |
| **跑的不是 main** | `healthz` 里的 `tcp` 键与 unit 的 `SIP_TCP_PORT=15060`，在 main（`35ad688`）里都不存在——它们只在未合并的 `feat/sip-default-tcp`（`087083a`/`4629f39`）。独立评审另测：`bin/`+unit mtime **13:09**、进程 **13:09:35** 启动，当前二进制 sha256 与备份里的 `sha256-after.txt` 不同，11:47 那版已被覆盖到 `~/aerodesk-redeploy-20261008-b/*.prev`（该目录原文档未提） |

**已撤回的旧结论（11:24 版，留痕）**：曾判「跑的是 2026-08-23 构建（`7c2ffc0`）、二进制无 SIP 监听、
公网 15060 无应答、客户端连不上」——那是对**重部署前**的观测，13:09 后全部失效。

**另一处必须更正的归因**：本节原写「经公网需在云控制台放行 UDP 端口」。该归因已被同仓
`ops/redeploy-main-20261008`（`85a5fe2`）推翻并复现：外网 SIP/UDP 不通是**本机默认路由落在 Clash TUN**
（`route -n get 129.226.150.174` → `interface utun4`），服务端与安全组都无需改。**同一台机器的同一现象，
两个线程不得并存相反口径**——以 `85a5fe2` 的实测为准。

**含义**：`docs/DEPLOYMENT.md` §6.1 的公共测试节点表已过期（旧字段形 healthz）。今晚的跨平台/公网验收两条路：

- **同局域网**：mac 上起一套服务器（已验证可用），Windows 机直接连它——不经公网，验证互通与无人值守足够。
- **经公网**：节点已重部署（13:09，`15060` TCP+UDP 已放行），但**其构建来自未合并分支**——要用它做验收，
  得先把 `feat/sip-default-tcp` 过评审合入 main 并重部署（或接受「验收对象与 main 不一致」并如实记录）。
  **重启/回滚共享服务器属影响他方的动作，需用户授权后执行。**

另：mac 端真机前置已验证可用——`--encoder screen` 在本机采到 **1470x956 / Hevc**，viewer `DECODED: 100`（TCC 屏幕录制权限已生效），mac 作被控端不需返工。

## 3. P0 清单（每条都要证据，不接受「已完成」口述）

| # | 项 | 出口证据 |
|---|---|---|
| P0-0 | **生产节点与客户端版本对齐**（详见 2.6） | `ss -lntu` 出现 SIP 监听；`/healthz` 带 `sip` 字段；从外网 SIP OPTIONS 得 200 |
| P0-1 | 公网连通：srflx / TURN relayed 实测 + 断 UDP 降级演练 | 两条异地链路拨通；`aerodesk_sfu_turn_allocations` 增长；黑屏时长上限数值 |
| P0-2 | macOS 被控端无人值守：launchd 自启 + 常驻 + 断线自恢复 + 「锁屏/未登录态不可采集」显式化 | 重启 2 次后无人工介入可被 mac 主控拨入出画面；日志留档 |
| P0-3 | Windows 被控端无人值守实机验证（服务 + 登录界面 helper） | 重启后未登录态信令在线；登录后自动切回会话采集；`docs/PRELOGIN_*` 断言逐条打勾 |
| P0-4 | 跨平台互通：win 被控 × mac 主控、mac 被控 × win 主控 | 两个方向各一轮：画面 + 输入 + 剪贴板 + 文件 + 音频（截屏/日志） |
| P0-5 | 已签名安装包 + 权限持久性 | `spctl -a -vv` 通过；授权后重启仍可采集（未签名 dev 构建会掉 TCC，必须用 Release 包） |
| P0-6 | 交付门禁 | main 的 `required_status_checks.contexts` 非空；本期改动全部走 **walgit collab 线程**（issue→patch→review→merge_result→closed；`origin` 是 walgit，不是 GitHub PR——已更正） |

## 4. 七天排期（每天 = 1 批次 = 1 PR + checklist + 证据）

| 日 | 线 | 批次 | 出口 |
|---|---|---|---|
| D1 10-08 | 基线 | 冷编译预热 + `scripts/smoke.sh` 端口可覆盖修复 + 服务器连通复核；本文件入库 + `.worktrees/` 补进 `.gitignore` | ✅ 冷编译 0 error / 约 9 分钟；`SIP_PORT=15060` 冒烟三项 PASS（walgit 线程 `delivery-d1-baseline`。**更正**：原文写的 `PR #617` 实为 `state=CLOSED, mergedAt=null` 的未合并 PR，2026-10-08 复核） |
| D2 10-08 | 门禁+线 A | 本地 e2e 在 macOS 假红的两根因（bash 3.2 吃中文标点 / SIP 5060 写死）+ 本机 TURN/relay 预检 | ✅ `nat-e2e` S0 直连 PASS + S3 强制 relay PASS（allocations=2）；`sip-accept-wss-e2e` 全 PASS（walgit 线程 `delivery-d2-e2e-macos`。**更正**：`PR #618` 同为 `CLOSED, mergedAt=null`） |
| **今晚** 10-08 | C+D | Windows 实机窗口（用户带回机器后）：P0-3 Windows 无人值守 + P0-4 跨平台互通首轮 | 两个方向出画+输入；Windows 重启后无人介入可拨入 |
| D3 10-09 | B | macOS 无人值守收口（launchd 自启 + 常驻 + 断线自恢复 + 锁屏/未登录边界写进手册） | 本机 mac↔mac 无人介入拨入成功 |
| D4 10-10 | A+节点 | P0-0 生产节点重部署（SIP 单栈）+ 双网络公网实测 + 断 UDP→TURN→回退演练 → **兜底决策点（T-3）** | SIP 监听与 OPTIONS 200；两条链路实测数据；黑屏时长数值 |
| D5 10-11 | C | P0-5 签名包真机安装 + TCC/辅助功能跨重启 | `spctl` 输出；重启后采集成功 |
| D6 10-12 | D | P0-4 跨平台互通全功能清单 + 阻断修复 | 两方向全功能清单打勾 |
| D7 10-13 | — | 交付复验（重启矩阵 + 连续两日真实使用）+ 交付手册 + 切流决策 | P0 逐条有证据；手册可照做 |

约束：活跃主线 ≤ 4；每批次独立 worktree + 独立评审（`RULE_开发流程规范.md`、`RULE_审查规范.md`）。

## 5. 风险与兜底

- **兜底（D4 结束判定）**：若 P0-1 或 P0-2 未闭环 → 建议 ToDesk 续费 1 个月做并行过渡，不硬切。
  该决定涉及花钱，由用户本人拍板，模型不代授权。
- **Windows 真机不在本机**（本机 macOS / Xcode 26.5）：用户已确认**今晚（10-08）带回机器**，故 D5 的 Windows 部分提前到今晚；仍需确认 Windows 机与 mac 是否同网段（决定走本地服务器还是公网节点）。
- **生产节点**（见 2.6）：节点已在 13:09 重部署并受理流量，但**跑的是未合并分支的构建**（不是 main），且本仓 `ops/redeploy-main-20261008` 已推翻「需云控制台放行 UDP」的归因（实为本机 Clash TUN）。重启/回滚共享服务器仍需用户授权，是本周最大的外部依赖。
- **冷编译**：本机 `target-dir` 在数据盘且当前为空，首次全量编译需计入 D1 墙钟。
- **macOS TCC**：未签名构建会掉屏幕录制权限 → D5 必须用 Release 的已签名+公证包验收。

## 6. v0.4.1 发布与「三个对象不一致」（2026-10-08，独立评审提出后补齐）

**必须先读这一节再看任何验收结论**：本周同时存在三个不同的「东西」，此前台账没有把它们分开写：

| 对象 | 是什么 | 本文件的相关引用 |
|---|---|---|
| **交付对象** | `main`（walgit 权威）= 本周全部合并后的代码线，现为 **`660fb14`** | 本文件全部「已实现/已修复」的判定对象 |
| **审查对象** | 本周四条线程（sip/D1/D2/ops）的 **diff** + 客户端面（`main` + `v0.4.0..main` 差） | 各线程的 review 条目 |
| **验收对象** | 实际装到机器上的**包**：`v0.4.0`（今晚原计划）或 **`v0.4.1`**（本次新发） | §2.5/§2.6 的实测都是前者之前的状态 |

三者的差异造成的实际后果（客户端独立评审实测）：
- 客户端评审当天（`main` = `7045a64`）实测 `v0.4.0` 与 main 的 desktop/host **逐字节相同**，客户端增量只有
  `aerodesk-vdev-bridge`（716 行）+ 几行；**此后 main 已并入 `feat/sip-default-tcp`**，两者**不再相同**
  （`git diff --stat v0.4.0 660fb14 -- crates/aerodesk-desktop crates/aerodesk-host` = +18/−9，改的正是默认传输 udp→tcp）；
- `v0.4.0` **缺** SIP/UDP 收包缓冲修复（`8192 → 65535`，`64314f0`）：SIP 报文 >8192B 时曾静默失败；
- `v0.4.0` 与其后的 `main` 客户端**都只有 `Udp`/`Tls`**，`SipTransport::Tcp` 直到本周 `feat/sip-default-tcp`
  合入才存在 → 所以「验收对象 ≠ 交付对象」时，客户端与服务端的默认传输会对不上。

**本次发布（对齐三者）**：

| 项 | 值 |
|---|---|
| tag / 发布点 | `v0.4.1` / `main = 660fb14`（+ 其后若干文档修正提交） |
| 发布仓 | `aerodesk-labs/aerodesk`（main 从 `6c9154d` **快进**到 `660fb14`，无 force） |
| Release | https://github.com/aerodesk-labs/aerodesk/releases/tag/v0.4.1 |
| 打包 run | `37764167446`（`release: published` 触发 Build & Release） |
| Windows 产物 | `aerodesk-0.4.1-win64.msi` / `.zip` —— **无代码签名**（`docs/PACKAGING.md` 自述「证书待补」），安装会提示「未知发布者」 |
| macOS 产物 | `AeroDesk-0.4.1.dmg` —— Developer ID 签名 + notarytool 公证 + stapler |

> 因此 P0-5「签名包」这一维**只在 macOS 有可验对象**；Windows 侧今天没有可验对象（要补 OV/EV 证书 + 流水线）。

## 7. 非交付面与已登记的技术债（避免被当成"已交付"）

| 项 | 状态 | 说明 |
|---|---|---|
| `crates/aerodesk-agent/src/bin/aerodesk-vdev-bridge/`（716 行） | **非交付面**；**零审查历史**，已由客户端独立评审覆盖并记录问题 | 不进包（只有 docs/e2e 引用）；三处静默失效**未修**，登记为债：① 推帧错误被 `let _ =` 丢弃（接管后永久停推流）；② 收流循环不查 `is_alive()`、无断流时限；③ AudioUnit `CURRENT_DEVICE/Start` 返回值被忽略。另它**当时**不复用统一配置面（`sip_port: None` → 由 URL scheme 推导，当时 `ws→udp/5060`，非默认端口接不上）
  ——**此条在 sip 合并后已失效**：该推导现在 `ws→tcp`（`connect::derive_sip_transport`，有单测钉住），留痕但标注过期。 |
| `cmd_exec` 的 info 级日志 | **债（另批）** | `agent info!("cmd request #{}: {:?}")` 会把 `WriteFile{data:base64}` / `Chat{text}` 写进日志（`d8afd42` 起，v0.4.0 亦有）→ 需降级/脱敏 |
| Windows host 的口令落盘 | **未验** | 明文写 ProgramData，未见 ACL/DPAPI 收紧（客户端评审标 unverified） |
| Windows 全部行为 | **静态审查** | 本机无法执行 Windows 二进制/服务/登录界面 helper；GNU 交叉 clippy 因缺 Windows FFmpeg 无法跑 |
