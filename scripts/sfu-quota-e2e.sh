#!/usr/bin/env bash
# sfu-quota-e2e.sh —— SFU /start 准入配额（#180）：MAX_ROOM_CLIENTS=1 时第 2 个发布端被 503。
# 独立端口避免与本机其它 agent 冲突。
set -euo pipefail
cd "$(dirname "$0")/.."
export RUST_LOG="${RUST_LOG:-info}"

echo "== 构建"
cargo build -q -p aerodesk-sfu -p aerodesk-signal -p aerodesk-agent

REC="$(mktemp -d)"
ROOM="sfu-q-$(date +%s)"

echo "== 启动 SFU（MAX_ROOM_CLIENTS=1）+ signal"
RECORD_DIR="$REC" MAX_ROOM_CLIENTS=1 \
  SFU_MEDIA_PORT=14578 SFU_SIGNAL_PORT=14500 SFU_INTERNAL_PORT=14502 \
  ./target/debug/aerodesk-sfu >/tmp/sfuq-sfu.log 2>&1 &
echo $! > /tmp/sfuq-sfu.pid
SIGNAL_PORT=14501 SIGNAL_PLAIN_PORT=14503 SFU_URL=http://127.0.0.1:14502 \
  ./target/debug/aerodesk-signal >/tmp/sfuq-sig.log 2>&1 &
echo $! > /tmp/sfuq-sig.pid
for _ in $(seq 1 50); do
    if nc -z 127.0.0.1 14502 2>/dev/null && nc -z 127.0.0.1 14503 2>/dev/null; then break; fi
    sleep 0.2
done
sleep 0.3

echo "== 第 1 个发布端加入（应成功）"
./target/debug/aerodesk-agent --role publisher --encoder x264 --noisy \
    --signal ws://127.0.0.1:14503 --room "$ROOM" >/tmp/sfuq-p1.log 2>&1 &
P1=$!
for _ in $(seq 1 40); do grep -q 'SDP negotiated' /tmp/sfuq-p1.log 2>/dev/null && break; sleep 0.2; done
grep -q 'SDP negotiated' /tmp/sfuq-p1.log && echo "PASS 第 1 个加入成功" || { echo "FAIL 第 1 个加入"; tail -5 /tmp/sfuq-p1.log; exit 1; }

echo "== 第 2 个发布端加入（应被 503 拒绝 room full）"
./target/debug/aerodesk-agent --role publisher --encoder x264 --noisy \
    --signal ws://127.0.0.1:14503 --room "$ROOM" >/tmp/sfuq-p2.log 2>&1 || true
P2=$!
sleep 2
# SFU 日志有 reject 记录（ureq 错误体不含 body，CLI 只显示 503）
if grep -q 'reject /start' /tmp/sfuq-sfu.log && grep -q 'room full' /tmp/sfuq-sfu.log \
   && grep -q '503' /tmp/sfuq-p2.log; then
    echo "PASS 第 2 个被 503 拒绝（room full）"
else
    echo "FAIL 第 2 个未被拒"; tail -5 /tmp/sfuq-p2.log; grep 'reject /start' /tmp/sfuq-sfu.log | tail -2; kill $P1 $P2 2>/dev/null || true
    kill "$(cat /tmp/sfuq-sfu.pid)" "$(cat /tmp/sfuq-sig.pid)" 2>/dev/null || true
    exit 1
fi

kill $P1 $P2 2>/dev/null || true
kill "$(cat /tmp/sfuq-sfu.pid)" "$(cat /tmp/sfuq-sig.pid)" 2>/dev/null || true
wait 2>/dev/null || true
echo "E2E DONE"
