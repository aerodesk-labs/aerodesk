# 生产交付冲刺周（2026-10-08 → 2026-10-15）——Win ↔ macOS 双向

> 触发：ToDesk 授权一周内到期（用户 2026-10-08）。
> 范围口径（用户确认）：**主控端与被控端都是 Windows 与 macOS，双向可用**。
> 本文件是本周的可核对台账：每天的批次、出口门禁、证据形态都在这里。
> 配套任务书与原始响应落在 `/Volumes/DataExt/tmp/aerodesk-delivery-week/`（不进仓库）。

## 1. 阶段与判定（含原始响应路径）

| 项 | 结论 | 概率 / 置信 | 原始响应 |
|---|---|---|---|
| 生命周期阶段 | **验收**（次高「测试」0.25） | p=0.59 / 置信 0.54 | `/var/folders/…/jev-stage-lifecycle.json` |
| 本周主线 | **自用最小闭环**（无人值守 + 公网连通 + 已签名包） | p=0.82 / 置信 0.78 | `/tmp/jev-aerodesk-week-mainline.json` |
| 第一优先项 | 公网连通（次高：mac 无人值守 0.22、跨平台验收 0.17） | p=0.60 / 置信 0.52 ⚠ | `/tmp/jev-aerodesk-week-d2-order.json` |

**⚠ 二次校验（置信 0.52 < 0.6）**：按 `RULE_决策类问题先问jev.md` 第 3 条不直接照做，改用客观事实复核——
公网连通确为前置条件，但它依赖「两端设备 + 两个网络同时在场」的人工窗口；而 macOS 无人值守是**两端里唯一零实现、需要新写代码**的一项（`crates/aerodesk-platform/src/macos*` 内无 launchd/守护实现），关键路径最长。
故**不作为串行顺序采用**，而是拆成两条互不冲突的线并行（`RULE_Issue批次化.md` 允许活跃主线 ≤ 4）：

- **线 A（需人工窗口）**：公网 NAT/TURN 实测 → D2 做本机可自动化的一半（内嵌 TURN + 候选类型观测），D4 做双网络实测。
- **线 B（纯代码，可无人值守推进）**：macOS 无人值守闭环 → D2/D3。

## 2. 现状台账（均可复现）

| # | 能力 | 状态 | 证据 |
|---|---|---|---|
| 1 | 服务端 | 已部署健康，**零真实流量** | `curl http://129.226.150.174:14703/healthz` → `{"clients":0,"pop":"pop-a","rooms":0,"status":"ok"}`；`sip_registrations 0`、`sip_calls_established 0` |
| 2 | 安装包 | v0.4.0 全平台产物已在 Release | `AeroDesk-0.4.0.dmg`、`aerodesk-0.4.0-win64.msi/.zip`、`deb/rpm/tar.gz/AppImage` |
| 3 | 签名/公证流水线 | 可用 | `Build & Release` run `33290049676`（tag v0.4.0）success |
| 4 | 无人值守（口令面） | 已实现 | `SIP_DIGEST_USERS` 固定口令 + `POST /admin/temp-password`：`docs/SIP_SIGNALING.md` §11 |
| 5 | 无人值守（Windows 常驻） | 已实现，**缺实机证据** | `windows/autostart.rs`（HKCU Run）；`crates/aerodesk-host` SYSTEM 服务 + winlogon 登录界面 helper（`docs/PRELOGIN_WINDOWS_SERVICE.md`、`PRELOGIN_WINLOGON_CAPTURE.md`） |
| 6 | 无人值守（macOS 常驻） | **零实现** | 仓库内无 launchd/LaunchAgent/守护；`PRELOGIN_WINDOWS_SERVICE.md` §1 明确 macOS 无原生方案（TCC 按用户授权）不在该方案范围 |
| 7 | 跨平台互通 | **未验** | `docs/DEVICE_MATRIX.md`：只有 mac×mac、win×win 打勾，win×mac / mac×win 均「待」 |
| 8 | 公网 NAT | **未验**（交接项） | `docs/P0_ACCEPTANCE_REPORT_20260824.md` §4/§6 |
| 9 | 合并门禁 | 有洞 | `ci.yml` 仅 `pull_request` 触发；`/branches/main/protection` 的 `required_status_checks.contexts = []`、`checks = []` |
| 10 | 工作项账本 | 无处建 issue | `has_issues: false`；origin 为 GitHub → 走 PR + CODEOWNERS 留痕 |
| 11 | 文档口径 | 有漂移 | `docs/ACCEPTANCE.md` 仍写 `ws://:3003` 的 JSON join 压测（P3 已退役） |

### 明确不在本期交付面（写下来避免范围蔓延）

iOS / Android / HarmonyOS / Linux 真机；macOS **锁屏与未登录态**采集（业界无原生方案，与 `#470` 决策一致）；
UAC Secure Desktop（`#472`）；对外多租户与容量承诺（`#8` 压测基线留后续）。

## 2.5 D1 实测发现（2026-10-08）

| 发现 | 事实 | 处置 |
|---|---|---|
| 本机 UDP 5060 被 `freeswitch`(pid 34635) 占用 | `lsof -nP -iUDP:5060` → `freeswitch 34635`；aerodesk-signal 绑定 `0.0.0.0:5060` 后，内核把回包交给更具体的 `127.0.0.1:5060`，REGISTER 被 FreeSWITCH 回 **403 Forbidden** | 本轮先修 canonical 的 `scripts/smoke.sh`：SIP/ops 端口可用 `SIP_PORT` / `SIGNAL_OPS_PORT` 覆盖（冒烟用 15060/13001） |
| **45 个 e2e 脚本把 `SIP_UDP_PORT=5060` 写死** | `rg -l 5060 scripts/ \| wc -l` → 45；客户端同默认 5060 → 本机跑任一 e2e 都会假红，且错误信息指向「口令错/鉴权」完全误导 | 记入 **D2 批次**（抽公共 helper 统一端口，不在 D1 夹带 45 文件改动） |
| 冷编译实测 | `cargo build -p sfu -p signal -p agent -p desktop` 从空 target 到产出：**0 error**，4 个二进制，约 **9 分钟** | 后续批次可直接复用热 target |
| 冒烟基线 | `SIP_PORT=15060 SIGNAL_OPS_PORT=13001 bash scripts/smoke.sh d1-baseline 8` → `PASS input relay` / `PASS media receive` / `PASS no errors` | D1 出口证据 |

## 2.6 生产节点巡检结论（2026-10-08，只读巡检 `devserver` = 129.226.150.174）

**结论：生产节点跑的是 2026-08-23 的构建，早于服务端 SIP 单栈，与当前 main 客户端不兼容。**

| 事实 | 证据 |
|---|---|
| 进程 | `ps` → `/opt/aerodesk/bin/aerodesk-sfu`、`aerodesk-signal`，启动于 **Aug 23 22:34**（uptime 45 天） |
| 源码版本 | `git -C /opt/aerodesk/src log -1` → `7c2ffc0 2026-08-23 "#552 slice 13"`（P2 中段，早于 P3 服务端拆栈 #604 与 v0.3.0） |
| 二进制无 SIP 监听 | `strings /opt/aerodesk/bin/aerodesk-signal \| grep -c "SIP/UDP 监听已起"` → **0** |
| 公网无 SIP 端口 | 从本机对该机做标准 SIP `OPTIONS`：UDP 5060 无应答、UDP **15060 无应答**；`ss -lntu` 上 `5060` 只绑在私网 `10.3.0.9`（该机上另有 FreeSWITCH） |
| ops 面与文档不一致 | 文档 §6.1 写 `14701` 为 ops HTTPS，实测 `14701` 无响应而 `14703` 返回信号 health（旧字段形：`{"clients":0,"pop":"pop-a","rooms":0,"status":"ok"}`，无 `sip` 字段）；`/devices` 返回 `aerodesk-signal: connect to /ws` |

**含义**：`docs/DEPLOYMENT.md` §6.1 的公共测试节点表已过期；当前客户端（JSON 面已在 v0.3.0 退役）**连不上生产节点**。
所以今晚的跨平台/公网验收有两条路：

- **同局域网**：mac 上起一套服务器（已验证可用），Windows 机直接连它——不经公网，验证互通与无人值守足够。
- **经公网**：必须先把节点重部署到当前 main（`/opt/aerodesk/src` 已装 rustc 1.93.1、sudo 免密可用；SIP 端口需避开该机的 FreeSWITCH 5060，例如 `15060/UDP`），并在云控制台放行该 UDP 端口。**重启共享服务器属于影响他方的动作，需用户授权后执行。**

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
| P0-6 | 交付门禁 | main 的 `required_status_checks.contexts` 非空；本期改动全部走 PR |

## 4. 七天排期（每天 = 1 批次 = 1 PR + checklist + 证据）

| 日 | 线 | 批次 | 出口 |
|---|---|---|---|
| D1 10-08 | 基线 | 冷编译预热 + `scripts/smoke.sh` 端口可覆盖修复 + 服务器连通复核；本文件入库 + `.worktrees/` 补进 `.gitignore` | ✅ 冷编译 0 error / 约 9 分钟；`SIP_PORT=15060` 冒烟三项 PASS（PR #617） |
| D2 10-08 | 门禁+线 A | 本地 e2e 在 macOS 假红的两根因（bash 3.2 吃中文标点 / SIP 5060 写死）+ 本机 TURN/relay 预检 | ✅ `nat-e2e` S0 直连 PASS + S3 强制 relay PASS（allocations=2）；`sip-accept-wss-e2e` 全 PASS（PR #618） |
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
- **生产节点陈旧**（见 2.6）：公网路径依赖节点重部署，而重部署会重启共享服务器并可能需要在云控制台放行 UDP 端口——需用户授权，是本周最大的外部依赖。
- **冷编译**：本机 `target-dir` 在数据盘且当前为空，首次全量编译需计入 D1 墙钟。
- **macOS TCC**：未签名构建会掉屏幕录制权限 → D5 必须用 Release 的已签名+公证包验收。
