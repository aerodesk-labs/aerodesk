# 仓库托管与协作：walgit 主仓 · GitHub 镜像

> 迁移日期：2026-09-19。本文件是「仓库在哪里、协作在哪里、发版怎么走」的单一事实来源；
> 与 `.walgit/board.toml`、`scripts/github-mirror.ps1` 一起构成托管约定。

## 1. 谁是真源

| 角色 | 位置 | 说明 |
|---|---|---|
| **canonical（真源）** | walgit：`http://127.0.0.1:8081/gqf2008/aerodesk.git`（remote 名 `origin`） | 本机 walgit 服务（Windows 托盘，配置 `~/.walgit/walgit.toml`），桶与 macOS 部署共用，mac 侧同样可见 |
| **镜像 + 发版** | GitHub：<https://github.com/aerodesk-labs/aerodesk>（remote 名 `github`） | 由镜像循环单向同步 `heads` + `tags`；承载 Actions CI 与 Release 产物 |
| **协作** | walgit `refs/collab/*`（issue / PR / review / 看板 / CI 结果） | 全部是签名条目，读用 `walgit collab ls|thread|pr|board|report`，写用 `walgit collab entry` |

GitHub 侧于 2026-09-19 关闭（`gh api` 实测 `has_issues=false`、`has_wiki=false`、
`has_projects=false`、`has_discussions=false`）：任务跟踪、产品决策记录、看板、讨论全部搬到 walgit，
GitHub 只保留 **Actions（CI/发版）** 与 **Releases**。Pull Request 功能 GitHub 不提供关闭开关，
按项目政策停用（不要再在 GitHub 上开 PR）。

`refs/collab/*` **永不镜像**：镜像脚本两端的 refspec 只覆盖 `refs/heads/*` 与 `refs/tags/*`。

## 2. 克隆与日常拉取

```sh
# 主仓（walgit 在本机回环上，注意不要让代理接管 127.0.0.1）
git clone http://127.0.0.1:8081/gqf2008/aerodesk.git

# 已有检出：origin 指 walgit，github 只是镜像
git fetch origin --prune          # 日常
git switch main && git merge --ff-only origin/main
```

若全局 git 配了 `http.proxy`，回环地址要用 `-c http.proxy=` 或 `NO_PROXY=127.0.0.1,localhost`
绕开；本仓库的镜像脚本已内置这条。

GitHub 只读镜像可以照常 clone，但它不是提交入口：

```sh
git clone https://github.com/aerodesk-labs/aerodesk.git   # 只能看，发版产物在这里
```

## 3. 协作：issue / PR / 审查 / 看板都在 walgit

看板定义在 [`.walgit/board.toml`](../.walgit/board.toml)，投影规则在里面有注释；**移动卡片是追加一条
签名 `status` 条目，不是改文件**。

身份：一个 agent / 一个人一个 principal 一把 Ed25519 key（`~/.walgit/keys/<principal>.ed25519`），
首次使用先注册：

```sh
walgit --config ~/.walgit/walgit.toml collab principal-register \
  --repo . --principal <principal> --key ~/.walgit/keys/<principal>.ed25519 --push origin
```

标准流（每条命令的 oid 输出就是下一条的 `--parent`）：

```sh
W() { walgit --config ~/.walgit/walgit.toml collab entry --repo . "$@"; }

# 1) 建单（issue 根条目）
W --kind issue --id <thread> --actor <principal> \
  --body '{"title":"<标题>","body":"目标 / 范围 / 验收标准"}' \
  --key ~/.walgit/keys/<principal>.ed25519 --push origin

# 2) 建单即认领：issue 之后必须补一条带 owner 的 status，否则看板上是「未分配 open」
W --kind status --id <thread> --actor <principal> --parent <issue-oid> \
  --body '{"status":"in-progress","owner":"<principal>","worktree":"wt-<thread>","branch":"feat/<thread>","work":"<一句话计划>"}' \
  --key ~/.walgit/keys/<principal>.ed25519 --push origin

# 3) 在 worktree 里干活（基于 origin/main 开，一个单元一个 worktree/branch）
git worktree add ../aerodesk-wt-<thread> -b feat/<thread> origin/main

# 4) 挂实现 + 请审
W --kind patch --id <thread> --actor <principal> --parent <status-oid> \
  --base refs/heads/main --head refs/heads/feat/<thread> \
  --body '{"title":"<提交标题>","message":"<改了什么、为什么>"}' \
  --key ~/.walgit/keys/<principal>.ed25519 --push origin
W --kind status --id <thread> --actor <principal> --parent <patch-oid> \
  --body '{"status":"needs-review","owner":"<principal>","worktree":"wt-<thread>","branch":"feat/<thread>","work":"待独立审查"}' \
  --key ~/.walgit/keys/<principal>.ed25519 --push origin

# 5) 独立审查（必须换一个 principal 签名；作者自己的 approve 不算数）
W --kind review --id <thread> --actor <reviewer> --parent <status-oid> \
  --body '{"decision":"approve","agent":"<reviewer>","note":"<位置 / 问题 / 建议 / 可复现验证>"}' \
  --key ~/.walgit/keys/<reviewer>.ed25519 --push origin

# 6) 合并并在线程里收尾（协调者；先本地 fast-forward，再推 origin）
git switch main && git merge --ff-only feat/<thread> && git push origin main
W --kind merge_result --id <thread> --actor <coordinator> --parent <review-oid> \
  --body '{"oid":"<merged-oid>","result":"merged","note":"<合并说明>"}' \
  --key ~/.walgit/keys/<coordinator>.ed25519 --push origin
W --kind merge_result --id <thread> --actor <coordinator> --parent <merge-oid> \
  --body '{"merged":true,"oid":"<merged-oid>","note":"<结项摘要>"}' \
  --key ~/.walgit/keys/<coordinator>.ed25519 --push origin
W --kind status --id <thread> --actor <coordinator> --parent <merged-entry-oid> \
  --body '{"status":"closed","owner":"<coordinator>","worktree":"wt-<thread>","branch":"main","work":"已合并并通过验证"}' \
  --key ~/.walgit/keys/<coordinator>.ed25519 --push origin

walgit --config ~/.walgit/walgit.toml collab board     # 看板
walgit --config ~/.walgit/walgit.toml collab report    # 线程/PR/验签/活动总览
```

门禁仍然按 `~/.agents/rules/RULE_*.md`：本地 fmt/clippy/test 为准（CI 仅发版必需）、
worktree 起步、独立审查后才合并、改行为/命令同步改文档。

## 4. 镜像循环（walgit → GitHub）

[`scripts/github-mirror.ps1`](../scripts/github-mirror.ps1)：维护裸镜像仓
`~/.walgit/mirror/aerodesk.git`，每轮 `fetch origin --prune`（walgit → 镜像）后
`push github`（镜像 → GitHub），refspec 只覆盖 `heads` + `tags`，默认带 `+` 强制覆盖
（纯镜像语义：GitHub 只是副本，不接受任何只存在于 GitHub 的提交；需要「有分叉就报错」时加
`-NoForce`）。推送**默认不删** GitHub 侧独有的 ref，`-Prune` 才让镜像与 walgit 完全对齐
（会删除这些 ref，先看 `-Status` 列出的清单）。

```powershell
pwsh -File scripts/github-mirror.ps1 -Once          # 手动同步一轮
pwsh -File scripts/github-mirror.ps1                # 前台常驻循环（Ctrl+C 退出）
pwsh -File scripts/github-mirror.ps1 -InstallTask   # 注册计划任务（每 60 秒一轮）
pwsh -File scripts/github-mirror.ps1 -UninstallTask
pwsh -File scripts/github-mirror.ps1 -Status        # 任务状态 + 日志尾部 + GitHub 独有 ref 清单
pwsh -File scripts/github-mirror.ps1 -Once -Prune   # 完全对齐（删除 GitHub 独有的分支/标签）
```

迁移前 GitHub 上还留着 13 个 PR 时代的旧分支（`ci/*`、`fix/*`、`release/v0.2.0`、`wip/*`、
`worktree-wf_*`），它们的提交不在 walgit 里，因此默认保留、不会被镜像循环删除；确认无用后
用一次 `-Prune` 清理即可（之后计划任务保持默认参数，新分支由 walgit 侧决定）。

- 计划任务：`walgit-sync-github-aerodesk`（每 60 秒跑一次 `-Once`，锁文件防重入）。
- 日志：`~/.walgit/sync-to-github-aerodesk.log`（超过 5 MiB 自动轮转成 `.log.1`）。
- 代理：GitHub 走本机 Clash 代理且端口会漂移，脚本按 `-ProxyPort` → `AERODESK_GITHUB_PROXY` /
  `HTTPS_PROXY` → 注册表 `ProxyServer` → 常见端口探测的顺序自动选，并显式用
  `http.sslBackend=openssl` + HTTP/1.1（Windows schannel 经本地代理握手不稳）。
- 凭据：沿用全局 credential helper（本机为 `gh auth git-credential`），脚本不存任何凭据。

## 5. 发版（GitHub 仍是打包流水线）

`release.yml` 由 **release published**（或 `workflow_dispatch`）触发，**不由 tag push 触发**
（#433 去掉 push tags 以免双跑），所以顺序是：

```sh
# 1) 在 walgit 合并 main（含签名 merge_result），推 origin/main
# 2) 打 tag 并推到 walgit
git tag v0.5.0 && git push origin v0.5.0
# 3) 等镜像循环把 tag 推到 GitHub（pwsh -File scripts/github-mirror.ps1 -Once 可立即触发）
# 4) 在 GitHub 建 release —— 这一步才启动全平台打包（macOS DMG / Linux deb / Windows installer）
gh release create v0.5.0 --generate-notes
```

发版是本项目唯一「必须 CI 全绿」的节点（见 `RULE_CI常规以本地门禁为准仅发版必需.md`）。

## 6. 规矩

- 不要在 GitHub 上开 issue / wiki / project / discussion —— 这些入口已关闭，写了没人看。
- 不要直接 push GitHub、不要双推；GitHub 侧的分叉会被下一轮镜像覆盖（或按 `-NoForce` 报错）。
- 不要 push / 删除 GitHub 上的 `refs/collab/*` —— 协作记录只属于 walgit。
- 直推 `main` 前先确认分支保护（GitHub main 仍要求 PR + codeowner 评审，管理员可绕过；
  镜像推送依赖管理员权限，换推送账号时先确认）。
