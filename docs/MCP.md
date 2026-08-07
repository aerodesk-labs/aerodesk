# AeroDesk MCP 工具面（#109）

`aerodesk-mcp` 让任意 MCP 客户端（Claude Desktop / Codex / Cursor 等）通过 AeroDesk
操作远程被控设备：**命令（终端）+ 文件 + 进程 + 键鼠**，构成 AI 远控的完整工具面。

## 运行

```sh
# 先构建（或直接用 release）
cargo build -q --release -p aerodesk-mcp -p aerodesk-cli

# 环境变量
export AERODESK_SIGNAL="ws://<signal-host>:3003"   # 信令地址，默认 ws://127.0.0.1:3003
export AERODESK_ROOM="demo"                        # 房间名（被控端 publisher 所在房间）
export AERODESK_CLI_BIN="$PWD/target/release/aerodesk-cli"

# 启动 MCP server（stdio transport）
./target/release/aerodesk-mcp
```

被控端侧需运行 publisher 并加入同一房间：

```sh
./target/release/aerodesk-cli --role publisher --encoder x264 \
  --signal ws://127.0.0.1:3003 --room demo
```

## 工具清单

| 工具 | 说明 |
|---|---|
| `connect` | 确认目标（signal/room） |
| `run_command` | 执行 shell 命令（危险命令默认拦截；白名单可放行） |
| `read_file` / `write_file` | 读写远程文件（写敏感路径默认禁止） |
| `list_processes` / `kill_process` | 进程管理（pid 0/1 禁止） |
| `mouse_move` / `mouse_click` | 归一化坐标移动/点击（左/右/中键） |
| `type_text` | 逐字符输入（US 布局，自动 Shift） |

## 与主流 agent 接入

### Claude Desktop（claude.ai 桌面版）
`claude mcp add aerodesk -- <绝对路径>/aerodesk-mcp`，然后在对话中自然描述任务：
「用 aerodesk 在 demo 房间的机器上跑 `ls /tmp`，然后截图并移动鼠标到 0.5,0.5 点击」。

### Codex / Cursor（任何支持 stdio MCP 的客户端）
在客户端 MCP 配置中注册命令型 server：
```json
{ "mcpServers": { "aerodesk": { "command": "/abs/path/aerodesk-mcp", "env": { "AERODESK_SIGNAL": "ws://...", "AERODESK_ROOM": "demo", "AERODESK_CLI_BIN": "/abs/path/aerodesk-cli" } } } }
```

### 手动验证（协议合规性）
```sh
printf '%s\n' \
'{"jsonrpc":"2.0","id":1,"method":"initialize","params":{}}' \
'{"jsonrpc":"2.0","id":2,"method":"tools/list"}' \
'{"jsonrpc":"2.0","id":3,"method":"tools/call","params":{"name":"run_command","arguments":{"command":"echo hi"}}}' \
| ./target/debug/aerodesk-mcp
```

## 真实 agent 联调验证（2026-08-07，Claude Code）

本机使用 Claude Code（`claude -p` 无头模式 + `--mcp-config`/`--strict-mcp-config` +
`--allowedTools "mcp__aerodesk__*"`）真实调用 aerodesk MCP：

- 被控端日志（publisher）证明工具调用完整回环：
```
cmd request #139067393: Run { command: "echo hello-from-claude", ... }
cmd response #139067393: Run { exit_code: Some(0), stdout: "hello-from-claude\n", ... }
```
- 说明：命令已由 Claude 端发起、经 MCP/CLI/SFU 到达被控端执行并回传 stdout；
  本机代理模型（deepseek-v4-flash）在限时内未合成最终文字答复（客户端侧行为），
  不影响工具调用成功的事实。正式验收建议使用 Claude/Anthropic 直连模型复跑。

## 权限与审计

- 危险命令/敏感路径写/pid 0-1 默认拦截；白名单 `~/AeroDesk/cmd-allowlist.txt`
  每行一个前缀，`aerodesk-cli --cmd-allowlist add|remove|list` 管理
- 全量审计 `~/AeroDesk/cmd-audit.jsonl`（JSONL），`aerodesk-cli --cmd-audit [n]` 查询
- 端到端回归：`scripts/cmd-e2e.sh`、`scripts/mcp-e2e.sh`（CI macOS）
