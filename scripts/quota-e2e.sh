#!/usr/bin/env bash
# quota-e2e.sh —— 信令连接配额（#163）：每房间/全局上限，Join 超限拒绝。
#
# Phase A：MAX_ROOM_CLIENTS=2 → 第 3 个加入同房间被拒（room full）
# Phase B：MAX_TOTAL_CLIENTS=2 → 第 3 个连接（不同房间）被拒（server full）
# 独立端口避免与本机其它 agent 冲突。
set -euo pipefail
cd "$(dirname "$0")/.."
export RUST_LOG="${RUST_LOG:-info}"

echo "== 构建"
cargo build -q -p aerodesk-signal -p aerodesk-cli

fail=0

echo "== Phase A：房间上限 2"
SIGNAL_PORT=14301 SIGNAL_PLAIN_PORT=14303 MAX_ROOM_CLIENTS=2 ./target/debug/aerodesk-signal >/tmp/quota-sig-a.log 2>&1 &
SIGA=$!
for _ in $(seq 1 50); do nc -z 127.0.0.1 14303 2>/dev/null && break; sleep 0.2; done
ROOM_A="quota-a-$(date +%s)"
./target/debug/aerodesk-cli --role publisher --signal ws://127.0.0.1:14303 --room "$ROOM_A" >/tmp/quota-a-pub.log 2>&1 &
PUB_A=$!
sleep 1
./target/debug/aerodesk-cli --role viewer --signal ws://127.0.0.1:14303 --room "$ROOM_A" >/tmp/quota-a-v1.log 2>&1 &
V1=$!
sleep 1
./target/debug/aerodesk-cli --role viewer --signal ws://127.0.0.1:14303 --room "$ROOM_A" >/tmp/quota-a-v2.log 2>&1 || true
V2=$!
sleep 2
kill $PUB_A $V1 $V2 2>/dev/null || true
if grep -q "joined room $ROOM_A" /tmp/quota-a-pub.log && grep -q "joined room $ROOM_A" /tmp/quota-a-v1.log; then
    echo "PASS A: 前 2 个连接加入成功"
else
    echo "FAIL A: 前 2 个加入"; tail -3 /tmp/quota-a-pub.log; tail -3 /tmp/quota-a-v1.log; fail=1
fi
if grep -q "room full" /tmp/quota-a-v2.log; then
    echo "PASS A: 第 3 个被拒（room full）"
else
    echo "FAIL A: 第 3 个未被拒"; tail -5 /tmp/quota-a-v2.log; fail=1
fi
kill $SIGA 2>/dev/null || true

echo "== Phase B：全局上限 2"
SIGNAL_PORT=14401 SIGNAL_PLAIN_PORT=14403 MAX_TOTAL_CLIENTS=2 ./target/debug/aerodesk-signal >/tmp/quota-sig-b.log 2>&1 &
SIGB=$!
for _ in $(seq 1 50); do nc -z 127.0.0.1 14403 2>/dev/null && break; sleep 0.2; done
RB1="quota-b1-$(date +%s)"; RB2="quota-b2-$(date +%s)"; RB3="quota-b3-$(date +%s)"
./target/debug/aerodesk-cli --role publisher --signal ws://127.0.0.1:14403 --room "$RB1" >/tmp/quota-b-p1.log 2>&1 &
P1=$!
sleep 1
./target/debug/aerodesk-cli --role viewer --signal ws://127.0.0.1:14403 --room "$RB2" >/tmp/quota-b-v.log 2>&1 &
V=$!
sleep 1
./target/debug/aerodesk-cli --role publisher --signal ws://127.0.0.1:14403 --room "$RB3" >/tmp/quota-b-p2.log 2>&1 || true
P2=$!
sleep 2
kill $P1 $V $P2 2>/dev/null || true
if grep -q "joined room $RB1" /tmp/quota-b-p1.log && grep -q "joined room $RB2" /tmp/quota-b-v.log; then
    echo "PASS B: 前 2 个连接（不同房间）加入成功"
else
    echo "FAIL B: 前 2 个加入"; tail -3 /tmp/quota-b-p1.log; tail -3 /tmp/quota-b-v.log; fail=1
fi
if grep -q "server full" /tmp/quota-b-p2.log; then
    echo "PASS B: 第 3 个被拒（server full）"
else
    echo "FAIL B: 第 3 个未被拒"; tail -5 /tmp/quota-b-p2.log; fail=1
fi
kill $SIGB 2>/dev/null || true
wait 2>/dev/null || true
exit $fail
