#!/usr/bin/env bash
# #72 剪贴板双向同步端到端（macOS）：viewer 剪贴板 → file 通道 → publisher
# 落地；publisher 剪贴板变化 → file 通道 → viewer 落地。
# 单机测试：两个进程共享系统剪贴板，用日志断言两个方向都真实走通。
# 用法: scripts/clipboard-e2e.sh [房间]
set -euo pipefail
cd "$(dirname "$0")/.."

ROOM="${1:-clip-$(date +%s)}"
export RUST_LOG="${RUST_LOG:-info}"

echo "== 构建"
cargo build -q -p aerodesk-sfu -p aerodesk-signal -p aerodesk-agent

REC="$(mktemp -d)"
echo "== 启动 sfu/signal"
RECORD_DIR="$REC" ./target/debug/aerodesk-sfu >/tmp/clip-sfu.log 2>&1 &
SFU_PID=$!
SIP_UDP_PORT=5060 ./target/debug/aerodesk-signal >/tmp/clip-sig.log 2>&1 &
SIG_PID=$!
for _ in $(seq 1 50); do
    if nc -z 127.0.0.1 3003 2>/dev/null; then break; fi
    if ! kill -0 "$SIG_PID" 2>/dev/null; then
        echo "signal 服务器启动失败"; cat /tmp/clip-sig.log; exit 1
    fi
    sleep 0.2
done
sleep 0.3

echo "== 预置剪贴板 AAA，启动 viewer + publisher"
printf 'AAA' | pbcopy
./target/debug/aerodesk-agent --role viewer \
    --signal ws://127.0.0.1:3003 --room "$ROOM" >/tmp/clip-view.log 2>&1 &
VIEW_PID=$!
./target/debug/aerodesk-agent --role publisher \
    --signal ws://127.0.0.1:3003 --room "$ROOM" >/tmp/clip-pub.log 2>&1 &
PUB_PID=$!

# 方向1：viewer 轮询到 AAA → file 通道 → publisher 落地
dir1=0
for _ in $(seq 1 50); do
    if grep -q "clipboard: apply 3 chars from remote" /tmp/clip-pub.log 2>/dev/null; then dir1=1; break; fi
    sleep 0.2
done

# 方向2：本机剪贴板改为 BBB → publisher 轮询 → file 通道 → viewer 落地
printf 'BBB' | pbcopy
dir2=0
for _ in $(seq 1 50); do
    if grep -q "clipboard: apply 3 chars from remote" /tmp/clip-view.log 2>/dev/null; then dir2=1; break; fi
    sleep 0.2
done

kill "$VIEW_PID" "$PUB_PID" "$SFU_PID" "$SIG_PID" 2>/dev/null || true
wait 2>/dev/null || true

echo "== 断言"
fail=0
if [ "$dir1" = "1" ]; then
    echo "PASS viewer->publisher clipboard (AAA applied)"
else
    echo "FAIL viewer->publisher not received"; tail -5 /tmp/clip-pub.log; tail -5 /tmp/clip-view.log; fail=1
fi
if [ "$dir2" = "1" ]; then
    echo "PASS publisher->viewer clipboard (BBB applied)"
else
    echo "FAIL publisher->viewer not received"; tail -5 /tmp/clip-pub.log; tail -5 /tmp/clip-view.log; fail=1
fi
if grep -qiE "panic" /tmp/clip-pub.log /tmp/clip-view.log /tmp/clip-sfu.log; then
    echo "FAIL panic in logs"; fail=1
else
    echo "PASS no panics"
fi

exit $fail
