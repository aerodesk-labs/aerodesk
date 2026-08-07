#!/usr/bin/env bash
# #8 高码率（大帧）回归：publisher VT 1080p60 --noisy（~5k pps）→ SFU → viewer。
# 回归背景：CLI viewer 每轮只读 1 包 + sleep(2ms)，高 pps 时内核丢包，
# 关键帧永远不完整（0 keyframes / DECODED 0）；修复为排空式读取后应正常。
# 注意：release 构建（debug 合成源/编码太慢导致码率失真，见 LESSON 性能压测）；
# VT 不可用（无 Metal/编码器）时 SKIP（与本仓库 VT 单测跳过策略一致）。
# 断言：
#   1. viewer 收到关键帧（>=1）且 DECODED > 0
#   2. 字节量 >= 5MB（noisy 大帧真实产生高 pps，否则回归失去意义）
#   3. 无 panic
# 用法: scripts/highrate-e2e.sh [房间] [观察秒数]
set -euo pipefail
cd "$(dirname "$0")/.."

ROOM="${1:-highrate-$(date +%s)}"
OBS="${2:-8}"
export RUST_LOG="${RUST_LOG:-info}"

echo "== 构建（release）"
cargo build -q --release -p aerodesk-sfu -p aerodesk-signal -p aerodesk-cli

REC="$(mktemp -d)"
echo "== 启动 sfu/signal"
RECORD_DIR="$REC" ./target/release/aerodesk-sfu >/tmp/highrate-sfu.log 2>&1 &
SFU_PID=$!
./target/release/aerodesk-signal >/tmp/highrate-sig.log 2>&1 &
SIG_PID=$!
for _ in $(seq 1 50); do
    if nc -z 127.0.0.1 3003 2>/dev/null && nc -z 127.0.0.1 3002 2>/dev/null; then break; fi
    if ! kill -0 "$SFU_PID" 2>/dev/null || ! kill -0 "$SIG_PID" 2>/dev/null; then
        echo "sfu/signal 启动失败"; tail -5 /tmp/highrate-sfu.log; tail -5 /tmp/highrate-sig.log; exit 1
    fi
    sleep 0.2
done
sleep 0.3

echo "== 启动 publisher（VT H264 1080p60 --noisy，高熵大帧）+ viewer"
./target/release/aerodesk-cli --role publisher --encoder vt --noisy \
    --width 1920 --height 1080 --fps 60 --bitrate 10000000 \
    --signal ws://127.0.0.1:3003 --room "$ROOM" >/tmp/highrate-pub.log 2>&1 &
PUB_PID=$!
./target/release/aerodesk-cli --role viewer \
    --signal ws://127.0.0.1:3003 --room "$ROOM" >/tmp/highrate-view.log 2>&1 &
VIEW_PID=$!

# VT 不可用时发布端会立即退出（vt init 失败）——SKIP 而非 FAIL（与 VT 单测一致）。
for _ in $(seq 1 25); do
    if grep -aq "VT publisher" /tmp/highrate-pub.log; then break; fi
    if ! kill -0 "$PUB_PID" 2>/dev/null; then
        echo "SKIP: VT 编码器不可用（发布端退出），跳过高码率回归"
        kill "$VIEW_PID" "$SFU_PID" "$SIG_PID" 2>/dev/null || true
        wait 2>/dev/null || true
        exit 0
    fi
    sleep 0.2
done

sleep "$OBS"
kill "$PUB_PID" "$VIEW_PID" "$SFU_PID" "$SIG_PID" 2>/dev/null || true
wait 2>/dev/null || true

echo "== 断言"
fail=0
LAST=$(grep -a "RECEIVED:" /tmp/highrate-view.log | tail -1)
KEYFRAMES=$(echo "$LAST" | sed -E 's/.*, ([0-9]+) keyframes,.*/\1/')
DECODED=$(echo "$LAST" | sed -E 's/.*keyframes, DECODED: ([0-9]+).*/\1/')
FRAMES=$(echo "$LAST" | sed -E 's/.*RECEIVED: ([0-9]+) frames.*/\1/')
BYTES=$(echo "$LAST" | sed -E 's/.*frames, ([0-9]+) bytes.*/\1/')
echo "last: $LAST"
if [ -n "$BYTES" ] && [ "$BYTES" -lt 5000000 ]; then
    echo "FAIL high-rate: 字节量过低（${BYTES}B），noisy 大帧未生效"; fail=1
elif [ -n "$KEYFRAMES" ] && [ "$KEYFRAMES" -ge 1 ] && [ -n "$DECODED" ] && [ "$DECODED" -gt 0 ]; then
    echo "PASS high-rate: ${FRAMES} frames / ${BYTES}B / ${KEYFRAMES} keyframes / DECODED ${DECODED}"
else
    echo "FAIL high-rate: 关键帧/DECODED 未达标（大帧丢包回归）"; tail -3 /tmp/highrate-view.log; fail=1
fi
if grep -qiE "panic" /tmp/highrate-pub.log /tmp/highrate-view.log /tmp/highrate-sfu.log; then
    echo "FAIL panic in logs"; fail=1
else
    echo "PASS no panics"
fi
exit $fail
