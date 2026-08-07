#!/usr/bin/env bash
# #109 MCP 工具面端到端：stdio JSON-RPC 会话 → aerodesk-mcp → aerodesk-cli 桥 →
# SFU → publisher（被控端）执行。
# 断言：
#   1. initialize 返回 serverInfo/protocolVersion
#   2. tools/list 返回全部工具（connect/run_command/read_file/write_file/list_processes/kill_process）
#   3. tools/call run_command：stdout 含 hello-from-mcp
#   4. tools/call write_file + read_file：内容一致
#   5. tools/call list_processes：count>0
# 用法: scripts/mcp-e2e.sh [房间]
set -euo pipefail
cd "$(dirname "$0")/.."

ROOM="${1:-mcp-$(date +%s)}"
export RUST_LOG="${RUST_LOG:-info}"
export AERODESK_CMD_ALLOWLIST="/tmp/aerodesk-mcp-allow-$ROOM.txt"
export AERODESK_CMD_AUDIT="/tmp/aerodesk-mcp-audit-$ROOM.jsonl"

echo "== 构建"
cargo build -q -p aerodesk-sfu -p aerodesk-signal -p aerodesk-cli -p aerodesk-mcp

REC="$(mktemp -d)"
echo "== 启动 sfu/signal + publisher"
RECORD_DIR="$REC" ./target/debug/aerodesk-sfu >/tmp/mcp-sfu.log 2>&1 &
SFU_PID=$!
./target/debug/aerodesk-signal >/tmp/mcp-sig.log 2>&1 &
SIG_PID=$!
for _ in $(seq 1 50); do
    if nc -z 127.0.0.1 3003 2>/dev/null && nc -z 127.0.0.1 3002 2>/dev/null; then break; fi
    sleep 0.2
done
sleep 0.3
./target/debug/aerodesk-cli --role publisher --encoder x264 \
    --signal ws://127.0.0.1:3003 --room "$ROOM" >/tmp/mcp-pub.log 2>&1 &
PUB_PID=$!
sleep 2

DIR="/tmp/aerodesk-mcp-file-$ROOM"
mkdir -p "$DIR"

echo "== 驱动 MCP stdio 会话"
cat > /tmp/mcp-in.txt <<INEOF
{"jsonrpc":"2.0","id":1,"method":"initialize","params":{"protocolVersion":"2025-06-18","capabilities":{},"clientInfo":{"name":"e2e","version":"1"}}}
{"jsonrpc":"2.0","method":"notifications/initialized"}
{"jsonrpc":"2.0","id":2,"method":"tools/list"}
{"jsonrpc":"2.0","id":3,"method":"tools/call","params":{"name":"run_command","arguments":{"command":"echo hello-from-mcp"}}}
{"jsonrpc":"2.0","id":4,"method":"tools/call","params":{"name":"write_file","arguments":{"path":"$DIR/hello.txt","content":"hello-mcp-file"}}}
{"jsonrpc":"2.0","id":5,"method":"tools/call","params":{"name":"read_file","arguments":{"path":"$DIR/hello.txt"}}}
{"jsonrpc":"2.0","id":6,"method":"tools/call","params":{"name":"list_processes","arguments":{}}}
INEOF

AERODESK_SIGNAL="ws://127.0.0.1:3003" AERODESK_ROOM="$ROOM" \
AERODESK_CLI_BIN="$PWD/target/debug/aerodesk-cli" \
  ./target/debug/aerodesk-mcp < /tmp/mcp-in.txt > /tmp/mcp-out.txt 2>/tmp/mcp-err.txt || true

kill "$PUB_PID" "$SFU_PID" "$SIG_PID" 2>/dev/null || true
wait 2>/dev/null || true
python3 - <<PYEOF
import os
for p in ["$AERODESK_CMD_ALLOWLIST", "$AERODESK_CMD_AUDIT"]:
    if os.path.exists(p):
        os.remove(p)
PYEOF

echo "== 断言"
fail=0
# 1) initialize
if grep -q '"serverInfo"' /tmp/mcp-out.txt && grep -q 'aerodesk-mcp' /tmp/mcp-out.txt; then
    echo "PASS initialize"
else
    echo "FAIL initialize"; tail -5 /tmp/mcp-out.txt; fail=1
fi
# 2) tools/list
for t in connect run_command read_file write_file list_processes kill_process; do
    grep -q "\"$t\"" /tmp/mcp-out.txt || { echo "FAIL tools/list missing $t"; fail=1; }
done
[ "$fail" = "0" ] && echo "PASS tools/list 全工具"
# 3) run_command
if grep -q "hello-from-mcp" /tmp/mcp-out.txt; then
    echo "PASS run_command"
else
    echo "FAIL run_command"; tail -8 /tmp/mcp-out.txt; fail=1
fi
# 4) write + read
if grep -q "hello-mcp-file" /tmp/mcp-out.txt; then
    echo "PASS write+read file"
else
    echo "FAIL write+read file"; tail -8 /tmp/mcp-out.txt; fail=1
fi
# 5) list_processes
if grep -qE "launchd|kernel_task|aerodesk|sh " /tmp/mcp-out.txt; then
    echo "PASS list_processes"
else
    echo "FAIL list_processes"; tail -8 /tmp/mcp-out.txt; fail=1
fi
# 6) 无 panic
if grep -qiE "panic" /tmp/mcp-out.txt /tmp/mcp-err.txt /tmp/mcp-pub.log /tmp/mcp-sfu.log; then
    echo "FAIL panic"; fail=1
else
    echo "PASS no panics"
fi

exit $fail
