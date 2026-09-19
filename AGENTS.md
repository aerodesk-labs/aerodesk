# AeroDesk 仓库约定（agent 必读）

> 通用规则仍是 `~/.agents/rules/RULE_*.md`；本文件只写本仓库特有的事实，两者冲突时以本文件为准。

## 仓库位置（2026-09-19 起）

- **canonical = walgit**：`http://127.0.0.1:8081/gqf2008/aerodesk.git`，remote 名 `origin`。
  issue / PR / review / 看板都是 `refs/collab/*` 上的签名条目，读写都用 `walgit collab …`
  （看板定义 `.walgit/board.toml`，完整流程见 `docs/WALGIT.md`）。
- **GitHub = 镜像 + 发版**：<https://github.com/aerodesk-labs/aerodesk>，remote 名 `github`。
  `scripts/github-mirror.ps1`（计划任务 `walgit-sync-github-aerodesk`，60 秒一轮）把 walgit 的
  `heads` + `tags` 单向镜像过去，`refs/collab/*` 永不外流。
- GitHub 侧 **Issues / Wiki / Projects / Discussions 已关闭**（PR 保留但按政策停用）。不要直推
  GitHub、不要在 GitHub 开 issue/PR/讨论、不要双推；GitHub 上只存在于一侧的提交会被镜像覆盖。
- 发版：tag 推到 walgit → 镜像到 GitHub → `gh release create <tag>` 触发 `release.yml` 打包
  （`release.yml` 不由 tag push 触发）。

## 开发与验收

- 流程：walgit 建 issue（随后补一条带 owner 的 `status` 认领）→ 基于 `origin/main` 开 worktree →
  提交 → `patch` 条目 → `status: needs-review` → 另一个 principal 独立审查 → 合并推 `origin/main` →
  `merge_result` + `status: closed`。
- 门禁：本地 `cargo fmt --check` / `cargo clippy -- -D warnings` / 相关 `cargo test`；GitHub CI 只在
  发版节点要求全绿（`RULE_CI常规以本地门禁为准仅发版必需.md`）。
- Conventional Commits，一个提交一个逻辑变更；改动命令、路径或行为时同步更新 README / `docs/`。
- 一个 agent 一个 principal 一把 key，审查者与被审查者必须是不同 principal。

## 本机注意事项（Windows）

- walgit 在回环上，不要被代理接管：必要时 `-c http.proxy=` 或 `NO_PROXY=127.0.0.1,localhost`。
- 本机 git 经 Clash 代理访问 GitHub 时端口会漂移（`7890`/`7897` 都出现过），不要把端口写死进
  git 配置；镜像脚本按次探测，手动访问时用 `-c http.proxy=http://127.0.0.1:<当前端口>`。
