# AeroDesk 仓库约定（agent 必读）

> 通用规则仍是 `~/.agents/rules/RULE_*.md`；本文件只写本仓库特有的事实，两者冲突时以本文件为准。

## 仓库位置（2026-09-19 起）

- **canonical = walgit**：`http://127.0.0.1:8081/gqf2008/aerodesk.git`，remote 名 `origin`。
  issue / PR / review / 看板都是 `refs/collab/*` 上的签名条目，读写都用 `walgit collab …`
  （看板定义 `.walgit/board.toml`，完整流程见 `docs/WALGIT.md`）。
- **GitHub = 镜像 + 发版**：<https://github.com/aerodesk-labs/aerodesk>，remote 名 `github`。
  **发布驱动**：发 tag / 发版时显式跑一次 `pwsh -File scripts/github-mirror.ps1 -Once`（幂等），
  把 walgit 的 `main` + `tags` 单向镜像过去（**只发布 main 一个分支**，walgit 上的在途审查分支
  不外流），`refs/collab/*` 永不外流；**不要**用
  `-InstallTask` 装每分钟轮询的计划任务（用户 2026-09-20 明令，会闪控制台窗口，脚本已直接拒绝）。
- GitHub 侧 **Issues / Wiki / Projects / Discussions 已关闭**（PR 保留但按政策停用）。不要直推
  GitHub、不要在 GitHub 开 issue/PR/讨论、不要双推；GitHub 上只存在于一侧的提交会被镜像覆盖。
- 发版：tag 推到 walgit → 镜像到 GitHub → `gh release create <tag>` 触发 `release.yml` 打包
  （`release.yml` 不由 tag push 触发）。

## 开发与验收

- 流程：先同步协作视图（宿主 SKILL.md §0a.5）
  `git fetch origin '+refs/collab/inbox/*:refs/collab/inbox/*' '+refs/collab/meta/*:refs/collab/meta/*'`
  → 协调者建 issue（worker 随后补一条带 owner 的 `status` 认领）→ 基于 `origin/main` 开 worktree →
  提交 → `patch` 条目 → `status: needs-review` → 另一个 principal 独立审查 → 合并推 `origin/main` →
  `merge_result` + `status: closed`。
- inbox 卫生（宿主 SKILL.md Housekeeping / D45）：inbox 涨到约 **50 条**、或每完成一两批时，由
  **协调者**跑一次
  `walgit --config ~/.walgit/walgit.toml collab gc --repo . --actor <coordinator> --key ~/.walgit/keys/<coordinator>.ed25519 --push origin`，
  把 append-only inbox 折叠进签名快照 `refs/collab/meta/snapshot`（幂等、可重跑；**不是**必须定期跑的
  计划任务）。**症状辨识**：collab 命令变慢（本机实测 70–90 秒）看着像服务端/网络慢，其实是 inbox
  未折叠 + 本机单次进程启动昂贵——**先 gc，再怀疑网络**。实测 104 条 inbox 时 `collab ls` 要 69–80 秒，
  gc 后 inbox=0、`collab ls` 降到 20.95 秒，`collab report` 仍 104/104 verified、看板列不变。
- 门禁：本地 `cargo fmt --check` / `cargo clippy -- -D warnings` / 相关 `cargo test`；GitHub CI 只在
  发版节点要求全绿（`RULE_CI常规以本地门禁为准仅发版必需.md`）。
- Conventional Commits，一个提交一个逻辑变更；改动命令、路径或行为时同步更新 README / `docs/`。
- 一个 agent 一个 principal 一把 key，审查者与被审查者必须是不同 principal。命名约定：新 agent 用
  `<proj>-<role>-N`（取该角色已注册的最大编号 +1），先看 `refs/collab/meta/principals` 再取名；
  若自己已持有某个已注册 principal 的 key 就沿用该身份（宿主 SKILL.md §0/§0a）。

## 本机注意事项（Windows）

- walgit 在回环上，不要被代理接管：必要时 `-c http.proxy=` 或 `NO_PROXY=127.0.0.1,localhost`。
- 本机 git 经 Clash 代理访问 GitHub 时端口会漂移（`7890`/`7897` 都出现过），不要把端口写死进
  git 配置；镜像脚本按次探测，手动访问时用 `-c http.proxy=http://127.0.0.1:<当前端口>`。
- walgit CLI 用**安装版**：PowerShell 里 `$env:LOCALAPPDATA\Programs\walgit\walgit.exe`，
  cmd 里 `%LOCALAPPDATA%\Programs\walgit\walgit.exe`。`walgit` 不在 PATH 上，调用时写全路径；
  配置仍是 `~/.walgit/walgit.toml`，key 在 `~/.walgit/keys/`。服务、托盘、service-host 都在同一个
  安装目录里；别去 `~/.walgit/` 找 CLI/服务二进制（那里只有 updater 的安装包）。
- `$env:USERPROFILE\walgit\walgit.exe` 是 **2026-09-19 遗留的孤立旧副本（v0.7.7-accept，105.8 MB）**，
  已于 **2026-09-30 由协调者删除**（删除前扫过进程 / 计划任务 / 注册表 Run 键 / 文本引用 / 快捷方式，
  均无引用）——不要再去找这个文件。可迁移的教训：`walgit service status` 若报「端口上在跑的是新版，
  而本二进制是旧版：很可能是升级前的老进程没退」并建议 stop + start，**先别照做**——用
  `Get-CimInstance Win32_Process -Filter "Name='walgit.exe'"` 看**服务进程实际的 `ExecutablePath`**，
  再判断到底有没有老进程。
