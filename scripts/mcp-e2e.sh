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
echo "== 启动 sfu/signal + publisher（含 --recv-dir 供大文件上传落盘）"
RECORD_DIR="$REC" ./target/debug/aerodesk-sfu >/tmp/mcp-sfu.log 2>&1 &
SFU_PID=$!
./target/debug/aerodesk-signal >/tmp/mcp-sig.log 2>&1 &
SIG_PID=$!
for _ in $(seq 1 50); do
    if nc -z 127.0.0.1 3003 2>/dev/null && nc -z 127.0.0.1 3002 2>/dev/null; then break; fi
    sleep 0.2
done
sleep 0.3
DIR="/tmp/aerodesk-mcp-file-$ROOM"
mkdir -p "$DIR/recv"
./target/debug/aerodesk-cli --role publisher --encoder x264 \
    --signal ws://127.0.0.1:3003 --room "$ROOM" --recv-dir "$DIR/recv" >/tmp/mcp-pub.log 2>&1 &
PUB_PID=$!
sleep 2

# #122：5MB 本地文件（上传源 + 下载校验）
dd if=/dev/urandom of="$DIR/upload-5m.bin" bs=1M count=5 2>/dev/null

echo "== 驱动 MCP stdio 会话"
cat > /tmp/mcp-in.txt <<INEOF
{"jsonrpc":"2.0","id":1,"method":"initialize","params":{"protocolVersion":"2025-06-18","capabilities":{},"clientInfo":{"name":"e2e","version":"1"}}}
{"jsonrpc":"2.0","method":"notifications/initialized"}
{"jsonrpc":"2.0","id":2,"method":"tools/list"}
{"jsonrpc":"2.0","id":3,"method":"tools/call","params":{"name":"run_command","arguments":{"command":"echo hello-from-mcp"}}}
{"jsonrpc":"2.0","id":4,"method":"tools/call","params":{"name":"write_file","arguments":{"path":"$DIR/hello.txt","content":"hello-mcp-file"}}}
{"jsonrpc":"2.0","id":5,"method":"tools/call","params":{"name":"read_file","arguments":{"path":"$DIR/hello.txt"}}}
{"jsonrpc":"2.0","id":6,"method":"tools/call","params":{"name":"list_processes","arguments":{}}}
{"jsonrpc":"2.0","id":7,"method":"tools/call","params":{"name":"mouse_move","arguments":{"x":0.5,"y":0.5}}}
{"jsonrpc":"2.0","id":8,"method":"tools/call","params":{"name":"type_text","arguments":{"text":"hello-123"}}}
{"jsonrpc":"2.0","id":9,"method":"tools/call","params":{"name":"upload_file","arguments":{"local_path":"$DIR/upload-5m.bin"}}}
{"jsonrpc":"2.0","id":10,"method":"tools/call","params":{"name":"download_file","arguments":{"remote_path":"$DIR/recv/upload-5m.bin"}}}
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
# 6) mouse_move（CLI 发送成功即 ok；注入本身受 macOS 辅助功能权限约束）
if grep -q "mouse_move ok" /tmp/mcp-out.txt; then
    echo "PASS mouse_move"
else
    echo "FAIL mouse_move"; tail -8 /tmp/mcp-out.txt; fail=1
fi
# 7) type_text（逐字符按键序列）
if grep -q "type_text ok" /tmp/mcp-out.txt; then
    echo "PASS type_text"
else
    echo "FAIL type_text"; tail -8 /tmp/mcp-out.txt; fail=1
fi
# 8) 大文件上传（5MB → 被控端 recv 目录）
if grep -q "uploaded: upload-5m.bin (5242880 bytes)" /tmp/mcp-out.txt && [ -f "$DIR/recv/upload-5m.bin" ]; then
    echo "PASS upload_file（5MB 落盘被控端）"
else
    echo "FAIL upload_file"; tail -6 /tmp/mcp-out.txt; fail=1
fi
# 9) 大文件下载（从被控端拉回，sha256 一致）
DL_HASH=$(grep -oE "downloaded: .*sha256=[0-9a-f]{64}" /tmp/mcp-out.txt | grep -oE "[0-9a-f]{64}" | tail -1)
SRC_HASH=$(shasum -a 256 "$DIR/upload-5m.bin" | awk '{print $1}')
if [ -n "$DL_HASH" ] && [ "$DL_HASH" = "$SRC_HASH" ]; then
    echo "PASS download_file（5MB sha256 一致）"
else
    echo "FAIL download_file"; tail -6 /tmp/mcp-out.txt; fail=1
fi
# 6) 无 panic
if grep -qiE "panic" /tmp/mcp-out.txt /tmp/mcp-err.txt /tmp/mcp-pub.log /tmp/mcp-sfu.log; then
    echo "FAIL panic"; fail=1
else
    echo "PASS no panics"
fi

exit $fail
