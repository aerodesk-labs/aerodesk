#!/usr/bin/env bash
# #75 输入注入链路：viewer 发输入事件 → SFU → publisher 注入（macOS CGEvent）。
# CI 无辅助功能权限时 CGEvent 静默，但注入路径与日志可验证。
# 用法: scripts/input-e2e.sh [房间]
set -euo pipefail
cd "$(dirname "$0")/.."

ROOM="${1:-input-$(date +%s)}"
export RUST_LOG="${RUST_LOG:-info}"

echo "== 构建"
cargo build -q -p aerodesk-sfu -p aerodesk-signal -p aerodesk-cli

REC="$(mktemp -d)"
echo "== 启动 sfu/signal"
RECORD_DIR="$REC" ./target/debug/aerodesk-sfu >/tmp/input-sfu.log 2>&1 &
SFU_PID=$!
./target/debug/aerodesk-signal >/tmp/input-sig.log 2>&1 &
SIG_PID=$!
for _ in $(seq 1 50); do
    if nc -z 127.0.0.1 3003 2>/dev/null && nc -z 127.0.0.1 3002 2>/dev/null; then break; fi
    if ! kill -0 "$SFU_PID" 2>/dev/null || ! kill -0 "$SIG_PID" 2>/dev/null; then
        echo "sfu/signal 启动失败"; tail -5 /tmp/input-sfu.log; tail -5 /tmp/input-sig.log; exit 1
    fi
    sleep 0.2
done
sleep 0.3

echo "== 启动 publisher + viewer"
./target/debug/aerodesk-cli --role publisher --encoder x264 \
    --signal ws://127.0.0.1:3003 --room "$ROOM" >/tmp/input-pub.log 2>&1 &
PUB_PID=$!
./target/debug/aerodesk-cli --role viewer \
    --signal ws://127.0.0.1:3003 --room "$ROOM" >/tmp/input-view.log 2>&1 &
VIEW_PID=$!
sleep 6
kill "$PUB_PID" "$VIEW_PID" "$SFU_PID" "$SIG_PID" 2>/dev/null || true
wait 2>/dev/null || true

echo "== 断言"
fail=0
# viewer 持续发送 MouseMove（模拟输入）→ publisher 注入（inject 日志证明收到+尝试注入）
if grep -qE "inject: seq=" /tmp/input-pub.log; then
    echo "PASS publisher received input and injected (macOS CGEvent)"
else
    echo "FAIL inject path"; tail -5 /tmp/input-pub.log; fail=1
fi
if grep -qiE "panic" /tmp/input-pub.log /tmp/input-view.log /tmp/input-sfu.log; then
    echo "FAIL panic in logs"; fail=1
else
    echo "PASS no panics"
fi
exit $fail
