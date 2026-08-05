#!/usr/bin/env bash
# #58 画质选层端到端：publisher --simulcast（q/h/f 三层）→ SFU 按 control 通道选层。
# 断言：
#   1. 发布端三层都在推流（关键帧带 rid q/h/f）
#   2. SFU 收到 High/Low 两个显式选层请求
#   3. f 层接收码率显著高于 q 层（分辨率/码率切换真实生效）
#   4. 无 panic
# 用法: scripts/simulcast-e2e.sh [房间] [观察秒数]
# 注意：release 构建（合成源/编码在 debug 下太慢导致码率失真，见 LESSON 性能压测）。
set -euo pipefail
cd "$(dirname "$0")/.."

ROOM="${1:-sim-$(date +%s)}"
OBS="${2:-10}"
export RUST_LOG="${RUST_LOG:-info}"

echo "== 构建（release）"
cargo build --release -q -p aerodesk-sfu -p aerodesk-signal -p aerodesk-cli

REC="$(mktemp -d)"
echo "== 启动 sfu/signal"
RECORD_DIR="$REC" ./target/release/aerodesk-sfu >/tmp/sim-sfu.log 2>&1 &
SFU_PID=$!
./target/release/aerodesk-signal >/tmp/sim-sig.log 2>&1 &
SIG_PID=$!
for _ in $(seq 1 50); do
    if nc -z 127.0.0.1 3003 2>/dev/null; then break; fi
    if ! kill -0 "$SIG_PID" 2>/dev/null; then
        echo "signal 服务器启动失败"; cat /tmp/sim-sig.log; exit 1
    fi
    sleep 0.2
done
sleep 0.3

echo "== 启动 publisher（x264 --simulcast: q/h/f）"
./target/release/aerodesk-cli --role publisher --encoder x264 --simulcast --noisy \
    --signal ws://127.0.0.1:3003 --room "$ROOM" >/tmp/sim-pub.log 2>&1 &
PUB_PID=$!
sleep 2

# 并发两个 viewer：f（清晰）与 q（流畅）同时请求不同层。
# 不用“kill 一个再连下一个”：信令服务器在旧连接被 kill 后有死锁（存量 bug）。
echo "== 并发观察 f/q 层 ${OBS}s"
./target/release/aerodesk-cli --role viewer --layer f \
    --signal ws://127.0.0.1:3003 --room "$ROOM" >/tmp/sim-view-f.log 2>&1 &
F_PID=$!
./target/release/aerodesk-cli --role viewer --layer q \
    --signal ws://127.0.0.1:3003 --room "$ROOM" >/tmp/sim-view-q.log 2>&1 &
Q_PID=$!
sleep "$OBS"

kill "$F_PID" "$Q_PID" "$PUB_PID" "$SFU_PID" "$SIG_PID" 2>/dev/null || true
wait 2>/dev/null || true

echo "== 断言"
fail=0
# 1) 发布端三层都在推流（关键帧带 rid）
for rid in q h f; do
    if grep -qE "keyframe rid=Some\(Rid\($rid\)\)" /tmp/sim-pub.log; then
        echo "PASS publisher sends rid=$rid"
    else
        echo "FAIL publisher rid=$rid not found in /tmp/sim-pub.log"; fail=1
    fi
done
# 2) SFU 收到两个显式选层请求
for layer in High Low; do
    if grep -q "layer request: Some($layer)" /tmp/sim-sfu.log; then
        echo "PASS SFU layer request $layer"
    else
        echo "FAIL SFU layer request $layer missing"; fail=1
    fi
done
# 3) f 层码率显著高于 q 层（release 下编码率接近目标码率）
bytes_f=$(grep -oE "RECEIVED: [0-9]+ frames, [0-9]+ bytes" /tmp/sim-view-f.log | tail -1 | grep -oE "[0-9]+ bytes" | grep -oE "[0-9]+" || echo 0)
bytes_q=$(grep -oE "RECEIVED: [0-9]+ frames, [0-9]+ bytes" /tmp/sim-view-q.log | tail -1 | grep -oE "[0-9]+ bytes" | grep -oE "[0-9]+" || echo 0)
frames_f=$(grep -oE "RECEIVED: [0-9]+ frames" /tmp/sim-view-f.log | tail -1 | grep -oE "[0-9]+" || echo 0)
frames_q=$(grep -oE "RECEIVED: [0-9]+ frames" /tmp/sim-view-q.log | tail -1 | grep -oE "[0-9]+" || echo 0)
echo "  f: $frames_f frames / $bytes_f bytes; q: $frames_q frames / $bytes_q bytes"
if [ "${frames_f:-0}" -gt 0 ] && [ "${frames_q:-0}" -gt 0 ] && [ "${bytes_f:-0}" -gt $((bytes_q * 2)) ] 2>/dev/null; then
    echo "PASS layer switching changes bitrate (f > 2x q)"
else
    echo "FAIL f layer not significantly higher than q"; fail=1
fi
# 4) 无 panic
if grep -qiE "panic" /tmp/sim-pub.log /tmp/sim-view-f.log /tmp/sim-view-q.log /tmp/sim-sfu.log; then
    echo "FAIL panic in logs"; fail=1
else
    echo "PASS no panics"
fi

exit $fail
