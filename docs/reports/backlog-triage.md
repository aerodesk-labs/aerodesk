# backlog 盘点报告：GitHub 迁入的 21 张卡（2026-09-19）

> 本报告是 2026-09-19 从 GitHub Issues 迁入 walgit 时那 21 张卡（`gh-1` … `gh-613`）的一次性盘点，从协作线程归档而来。**线程仍是权威记录**（`refs/collab/*` 上的签名条目），本文件只是仓库内的归档副本；两者不一致时以线程为准。

## 1. 数据来源

- 盘点写入者：`aerodesk-triage`；后续裁决写入者：`aerodesk-coord`。
- 条目按 oid 从本地对象库读取（`git cat-file -p <oid>`），来源 ref：
  - `refs/collab/meta/snapshot` = `d93796fe3f374f2f22271db24b5f0141088b4204`（已折叠的历史条目，含全部 `aerodesk-triage` 盘点条目）；
  - `refs/collab/inbox/*`（当时未折叠的尾部，含协调者裁决条目）。
- 下表给出「线程 id → issue oid → 盘点/裁决条目 oid」。正文里凡引用线程内容处，均回指本节 oid。
- 本报告**不摘录 review 条目**（review 属线程记录，不是报告内容）。

### 1.1 覆盖的线程与条目 oid

| 线程 id | issue oid | 盘点/裁决条目 oid |
|---|---|---|
| `backlog-triage-imported` | `d1a2499f73fb09061896bdd2af69a1f211303d4d` | 汇总 comment `1e888c7b702e37cb3f80bc83a9ddb7948d32817b` |
| `gh-1` | `13f5c98e119b99d885dab7e0282a1dd5235123db` | comment `012746db1e559aa55089e42aaec9145c911d440b` |
| `gh-2` | `d9b441543684044e259f392d047debe165b066f4` | comment `cc82c522e820c41fc7e1b093c16d3339592e7f35` |
| `gh-4` | `9d98671068e65d07e4a00af06d5896e61200227f` | comment `45e906408b229a2a4c97cd9160c664a40867c54b` |
| `gh-6` | `e3681f740e71afcc45f49b4fe91e9ff8356f454c` | comment `052058ebb03bc9854c1636b9cfd5124f10ef4a5b` |
| `gh-471` | `466680f13bbdf05aef23e1519ecc8c42ba3cd536` | triage status `b96422e8caef349e0e5744aeac8bc4e71c897fc7`；coord comment `6037229ce26c4c84e8ddddb228acaa9737aa9f88`；coord status `ec983c991cf7b748225854dd04cddf02ee0ebdc9` |
| `gh-499` | `6cbae16e4a8adfb2f1c94e13cb5beb8bfd71b41a` | comment `cb96ac386dd8536d9907f458502d51897c1c53dc` |
| `gh-500` | `7034397525ffe4d409ec449551a0298cc7022233` | comment `c16e28385c750c38488422c9bcd43a177afbfce5` |
| `gh-501` | `50734cc11627c744939faa42f9232fcdae7d2f80` | comment `0fb35ae13e12a2819982ef395e8cb39983d6a627` |
| `gh-502` | `ffab200dc0eb7f8e9d30d0ce6a3f874e2595f119` | comment `2723c4a721228e2713d8783b2d7b26cee532997d` |
| `gh-503` | `837b756ef1265884e0242fe5c2a21224035a0cb0` | （仅 issue 根条目，未单独盘点） |
| `gh-508` | `e040d47b744e8930a5968e7dd5b6304a53d03b90` | comment `e1ec5b55460e9928cbc0354ce88b7c49888f7bd1` |
| `gh-514` | `b1edaef182d4e2bf170b67bede1b04e477e9fc0a` | comment `a7f7f874899469aee9220211ed359bf9f6daf710` |
| `gh-518` | `273a9d3435067436a8106ac697ff5fd6596fec53` | triage status `dd1c3f1efaba406d47fab09445c512012bac9104`；triage comment `f3f718c2d876b316d94654fe05475477cb0b1707`；coord comment `8d7ea64755fe398c8240e3bc5a7c835ccdcab6b2`；coord status `56f9df01f408042966dd39dae5c65841c4010939` |
| `gh-584` | `59c37bba718059a1058be33582455f17417d814c` | comment `84d8bdd49640ad20e68b2d4e92d7439216877db8`；status `738ab3a468eb0bdba5044534b41d0567e469cbc6`；coord comment `a72ae99542f3ae9717ddf59b7398c4de97c2532e`；coord status `121cc52791fc164e14df2bd4d5348cb0b467002f` |
| `gh-582` / `gh-583` | `5f6e15e1b3e85dad6a7496040695f67f88c0ac76` / `d9565518ae97fe1bdd5792f1fb5bb02bdd4e0e09` | （仅 issue 根条目） |

## 2. 盘点结论

### 2.1 stale「处理中」标记

- 21 张里只有 **`gh-471`** 与 **`gh-518`** 的标题命中「处理中 / 进行中 / 🔄」字符串，且都带 `[wt-…]` 标记；其余 19 张不声称在途（来源：`1e888c7b`）。
- 判据（确定性）：两者在 `walgit collab board` 上是 unowned open；线程自 issue 后**没有任何 status 条目**；`git worktree list` 只有 main；`git branch -a` 无 `wt-*`（来源：`1e888c7b`、`dd1c3f1e`、`b96422e8`）。
- Jev 类型化结论（`typesafe/jev-1.13-20260917`）：标题声称在途 **p=0.970**；在途证据 **p=0.040** → 标题的在途声称无依据（来源：`1e888c7b`）。
- 处置经过：triage 先各写一条 `status=needs-human`（`gh-471` `b96422e8`、`gh-518` `dd1c3f1e`），理由写「需人确认是否仍在途」。协调者随后按宿主 SKILL.md §3 裁决——「是否仍在途」可由仓库证据判定，属可判事项——**改判两者 `open`** 回 backlog：`gh-471`（`6037229c` + `ec983c99`）、`gh-518`（`8d7ea647` + `56f9df01`）。
- 已知限制：collab 条目 append-only，标题里的 `🔄 [处理中]` 字符串无法移除。**标题是历史，status 是现状。**

### 2.2 P0/P1 现状（盘点时点，main `6c9154d`）

| 卡 | 导入标签 | 盘点时状态 | 备注 |
|---|---|---|---|
| `gh-503` | P1（标题写 P0） | unowned open | 标题/导入标签的优先级不一致，需优先级判定 |
| `gh-582` | P1 | unowned open | 无 owner |
| `gh-583` | P1 | unowned open | 无 owner |
| `gh-584` | P1 | needs-human（owner `gqf2008`） | 后由协调者改判 `blocked`（owner `aerodesk-coord`），见 `a72ae995` + `121cc527` |

来源：`1e888c7b`；`gh-584` 改判见 `a72ae995` + `121cc527`。

## 3. 三族合并 / 母单建议

### 3.1 平台适配族：`gh-1` / `gh-2` / `gh-4` / `gh-6`

- 现状（确定性）：四张都是 unowned open，只有 issue 无 status；本机无对应 worktree/分支。导入正文的清单进度：`gh-1` 6/6、`gh-2` 6/6、`gh-4` 4/4、`gh-6` 1/5。导入标签：`gh-1`/`gh-2` P3.5、`gh-4` P4、`gh-6` P5。
- **建议：不合并**四张卡——各平台验收标准不同，合并会互相污染验收标准。若要收口，另设一张「平台适配」母单/跟踪卡。
- Jev（`typesafe/jev-1.13-20260917`）：umbrella_parent **0.770** · keep_separate_no_merge **0.190** · keep_separate_and_downgrade **0.030**，**置信度 0.710**。
- 来源：`cc82c522`（`gh-2`），同文见 `012746db`（`gh-1`）、`45e90640`（`gh-4`）、`052058eb`（`gh-6`）。
- **已判定的事实**：`gh-1`/`gh-2` 的剩余都是「需真机」验收；`gh-4` 的 Wayland/uinput/VAAPI、`gh-6` 的 NAPI 桥/target/解码/采集均未开始。
- **留待优先级判断**：是否新建母单；`gh-1`/`gh-2` 现在是否有真机可用；`gh-6`（P5）是否现在排人（盘点建议留在 backlog，不要现在排人）。

### 3.2 产品化族：`gh-499` / `gh-500` / `gh-501` / `gh-502`

- 现状（确定性）：四张都是 unowned open，只有 issue 无 status。四张子系统互不相关：`gh-499` 审计事件流 + MCP/本地 LLM；`gh-500` 一次性协助链接；`gh-501` docker-compose 一键部署；`gh-502` macOS CGVirtualDisplay 私有 API spike。导入标签：`gh-499`/`gh-501` 服务端 P2、`gh-500` 客户端 P2、`gh-502` 平台适配 P3。
- 归类口径不一致（事实）：`gh-502` 的导入标签是「平台适配」，盘点把它归入「产品化」组；该口径分歧本身也**未定**。
- **建议：不建议合并**成一张卡（子系统/验收完全不同）；建议保留独立，并把整体后置到 P0/P1 之后。
- Jev（`typesafe/jev-1.13-20260917`）：umbrella_parent **0.510** · keep_separate_no_merge **0.470** · keep_separate_and_downgrade **0.010**，**置信度 0.400**——**低于 0.6 门限**。
- **本组不作结论**：是否另建「产品化」母单、`gh-502` 归哪一族，都**留给人定**。0.400 只是一次低于门限的模型输出，不能读成「应当保留独立」的结论。
- 来源：`cb96ac38`（`gh-499`），同文见 `c16e2838`（`gh-500`）、`0fb35ae1`（`gh-501`）、`2723c4a7`（`gh-502`）。

### 3.3 服务化族：`gh-508` / `gh-514` / `gh-518`

- 现状（确定性）：三张都是 unowned open，只有 issue 无 status。`gh-508` 是母单：正文批次 B1（PR #512 → `5e0f276`）、B2（PR #517 → `8845154`）已合入，B3/B4 未做；正文已把 `gh-518` 列为 B3 子批次，并列 `gh-471`/`gh-472`/`gh-473` 为关联。`gh-514` 是实测缺陷/可观测性问题（ToDesk 虚拟显示驱动致盲：DXGI AcquireNextFrame 永远 WAIT_TIMEOUT、GDI 蓝帧），不是母单的批次交付物。
- **建议：不合并**。`gh-518` 保持为 `gh-508` 的 B3 子批次卡（合并会丢掉 B3 的切片验收）；`gh-514` 作为独立卡保留（缺陷性质与架构母单的批次不同，需要单独修）。
- Jev（`typesafe/jev-1.13-20260917`）：keep_518_as_508_child_514_separate **0.980**，**置信度 0.970**。
- 来源：`e1ec5b55`（`gh-508`），同文见 `a7f7f874`（`gh-514`）、`f3f718c2`（`gh-518`）。
- **已判定的事实**：`gh-518` 的「处理中」是 stale 声称（见 2.1），经协调者改判 `open`，但维持其为 `gh-508` 的 B3 子批次身份（`8d7ea647`、`56f9df01`）。
- **留待优先级判断**：B3/B4 何时排期；`gh-514` 的修复范围（是否换 windows-capture/scrap）、是否单独立项。

## 4. 已判定的事实 vs 留待优先级判断的事项

> 宿主 SKILL.md §3：`needs-human` 仅限**授权 / 优先级 / 外部输入**；能由 owner 判的技术或产品取舍必须判掉、记进 comment、执行掉。本节据此把「已判定的」与「真的人才能定的」分开，不把可判事项写成 `needs-human`。

### 4.1 已判定的事实（可由仓库证据/盘点判定）

- `gh-471`、`gh-518` 标题的「处理中」是**过期陈述**：无 owner、无 worktree、无分支 ⇒ 判定为**未在途**，回 `open`（判词：coord `6037229c` / `8d7ea647`，status `ec983c99` / `56f9df01`）。
- `gh-584` 是**依赖驱动的恢复清单**（A–E 五组），不是判断题：A 组条件随 v0.4.0 已具备（`#598`/`#600`/`#604`/`#606`/`#608`）；B 组待配额语义重设计；C/D 组只能在发版周期的 GitHub Actions macOS e2e 上验证（本机是 Windows，开了也验不了）⇒ 判定为 `blocked`（依赖发版周期），owner=`aerodesk-coord`（coord `a72ae995` + status `121cc527`）。
- 三族「不合并」的**事实性建议**（3.1、3.2 的 keep_separate 分布、3.3 的 0.980）。

### 4.2 留待优先级判断的事项（人定）

- `gh-503` 的 P0/P1 标签冲突（标题 P0 vs 导入标签 P1）；P0/P1 卡（`gh-503`/`gh-582`/`gh-583`）的 owner 指派与排期。
- 是否为平台适配族另设母单；`gh-1`/`gh-2` 现在是否有真机可用。
- 产品化族（`gh-499`/`gh-500`/`gh-501`/`gh-502`）整体收口方式——Jev 置信度 0.400 低于门限，**未作结论**；`gh-502` 归「平台适配」还是「产品化」；这些差异化功能是否真的要做（产品决策）。
- `gh-471`/`gh-518` 是否**重启**（属排期优先级；「是否在途」这一项已判定）。
- `gh-584` 的 A–E 组排期、`gh-514` 的修复范围与是否立项、`gh-582`/`gh-583` 的排期。

## 5. 21 张卡清单（导入标签 + 盘点时线程状态）

| 卡 | 标题 | 导入标签 | 盘点时线程状态 |
|---|---|---|---|
| `gh-1` | iOS App 壳层（观看端） | P3.5, 客户端, 平台适配 | open（仅 issue，无 status） |
| `gh-2` | Android 真机适配（观看端 + 被控端） | P3.5, 客户端, 平台适配 | open（仅 issue） |
| `gh-4` | Linux 适配器（PipeWire + VAAPI + XTest/uinput） | P4, 客户端, 平台适配 | open（仅 issue） |
| `gh-6` | HarmonyOS 适配器（NAPI + AVScreenCapture + OH_VideoDecoder） | P5, 客户端, 平台适配 | open（仅 issue） |
| `gh-8` | 质量验收与 4K60 压测 | P5 | open（仅 issue） |
| `gh-75` | 鼠标控制完善：远程光标/DPI 缩放/修饰键/拖拽 + 平台注入补齐 | 客户端, 平台适配, P2 | open（仅 issue） |
| `gh-471` | 登录界面与锁屏态画面+输入：Winlogon desktop 采集与注入（非登录态 P1） | （导入标签为空） | 标题含 stale「🔄 [处理中]」；triage 置 needs-human → coord 改判 **open** |
| `gh-472` | UAC/Secure Desktop 穿透（非登录态 P2） | （空） | open（仅 issue） |
| `gh-473` | 安装器/升级集成（非登录态 P3） | （空） | open（仅 issue） |
| `gh-499` | AI 审计：MCP 识别高危远程操作 | enhancement, 服务端, P2 | open（仅 issue） |
| `gh-500` | Web 协助链接/邀请远控 | enhancement, 客户端, P2 | open（仅 issue） |
| `gh-501` | 自托管 docker-compose 一键部署包 | enhancement, 服务端, P2 | open（仅 issue） |
| `gh-502` | 被控虚拟屏：CGVirtualDisplay 可行性 spike | enhancement, 平台适配, P3 | open（仅 issue） |
| `gh-503` | P0 基础体验六件套 | enhancement, 客户端, P1 | open（仅 issue）；标题 P0 vs 标签 P1 冲突 |
| `gh-508` | 服务化架构：host 统管会话引擎（母单） | （空） | open（仅 issue）；B1/B2 已合入 |
| `gh-514` | 采集可观测性与多适配器健壮性——ToDesk 虚拟显示驱动致盲实例 | （空） | open（仅 issue） |
| `gh-518` | B3：被控出流迁服务侧——publisher 迁 host 用户态 agent（#508） | （空） | 标题含 stale「🔄 [处理中]」；triage 置 needs-human → coord 改判 **open**（仍是 #508 B3 子批次） |
| `gh-582` | test: NAT srflx/relay 公网实测 | P1 | open（仅 issue） |
| `gh-583` | feat(web): Web viewer ↔ 原生被控端互通 | P1 | open（仅 issue） |
| `gh-584` | chore(ci): 恢复 #553 验收前置暂停/降级的 macOS e2e step | P1 | triage 时 needs-human(owner `gqf2008`) → coord 改判 **blocked**(owner `aerodesk-coord`) |
| `gh-613` | fix(agent): aerodesk-bridge 会话韧性 | bug | open（仅 issue） |

## 6. 归档说明

- 归档动作对应宿主 SKILL.md §5：human-facing 制品要作为仓库文件保存（`docs/` 下），不能只留在 thread body 里。
- 本文件为**只读归档副本**：后续状态变化以 `walgit collab board` / `walgit collab thread <id>` 的实时投影为准，不要求逐条回写本文件。
