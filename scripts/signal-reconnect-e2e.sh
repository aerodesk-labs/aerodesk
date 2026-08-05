#!/usr/bin/env bash
# #66 回归：连接 A 建立后强制断开（SIGTERM），新连接 B 必须能正常 Join。
# 旧实现会死锁（清理广播持 rooms 锁去锁 A 的 ws；A 的读线程持 ws 锁阻塞），
# 导致 B 的 Join 永远拿不到 rooms 锁。
# 用法: scripts/signal-reconnect-e2e.sh [房间]
set -euo pipefail
cd "$(dirname "$0")/.."

ROOM="${1:-rec-$(date +%s)}"
export RUST_LOG="${RUST_LOG:-info}"

echo "== 构建"
cargo build -q -p aerodesk-sfu -p aerodesk-signal -p aerodesk-cli

REC="$(mktemp -d)"
echo "== 启动 sfu/signal"
RECORD_DIR="$REC" ./target/debug/aerodesk-sfu >/tmp/rec-sfu.log 2>&1 &
SFU_PID=$!
./target/debug/aerodesk-signal >/tmp/rec-sig.log 2>&1 &
SIG_PID=$!
for _ in $(seq 1 50); do
    if nc -z 127.0.0.1 3003 2>/dev/null; then break; fi
    if ! kill -0 "$SIG_PID" 2>/dev/null; then
        echo "signal 服务器启动失败"; cat /tmp/rec-sig.log; exit 1
    fi
    sleep 0.2
done
sleep 0.3

echo "== 连接 A（join + SDP 交换后保持在线）"
./target/debug/aerodesk-cli --role viewer --signal ws://127.0.0.1:3003 --room "$ROOM" >/tmp/rec-a.log 2>&1 &
A_PID=$!
# 等待 A 完成 join + SDP 交换（进入阻塞读状态），最多 10s
ok=0
for _ in $(seq 1 50); do
    if grep -q "SDP negotiated" /tmp/rec-a.log 2>/dev/null; then ok=1; break; fi
    if ! kill -0 "$A_PID" 2>/dev/null; then break; fi
    sleep 0.2
done
if [ "$ok" != "1" ]; then
    echo "FAIL viewer A 未能完成连接"; cat /tmp/rec-a.log; kill "$A_PID" "$SFU_PID" "$SIG_PID" 2>/dev/null || true; exit 1
fi
echo "== 强制断开 A（SIGTERM）"
kill "$A_PID" 2>/dev/null || true
wait "$A_PID" 2>/dev/null || true
sleep 1

echo "== 连接 B（必须能 Join）"
./target/debug/aerodesk-cli --role viewer --signal ws://127.0.0.1:3003 --room "$ROOM" >/tmp/rec-b.log 2>&1 &
B_PID=$!
joined=0
for _ in $(seq 1 50); do
    if grep -q "joined room" /tmp/rec-b.log 2>/dev/null; then joined=1; break; fi
    if ! kill -0 "$B_PID" 2>/dev/null; then break; fi
    sleep 0.2
done
kill "$B_PID" "$SFU_PID" "$SIG_PID" 2>/dev/null || true
wait 2>/dev/null || true

echo "== 断言"
fail=0
if [ "$joined" = "1" ]; then
    echo "PASS reconnect join (B joined after A killed)"
else
    echo "FAIL reconnect join"; echo "--- B log:"; cat /tmp/rec-b.log; fail=1
fi
if grep -q "left room" /tmp/rec-sig.log 2>/dev/null; then
    echo "PASS cleanup ran (A removed from room)"
else
    echo "WARN cleanup log missing (A 可能未加入房间)"
fi
exit $fail
