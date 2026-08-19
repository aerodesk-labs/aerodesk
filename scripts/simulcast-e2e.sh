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
OBS="${2:-12}"
export RUST_LOG="${RUST_LOG:-info}"

echo "== 构建（release）"
cargo build --release -q -p aerodesk-sfu -p aerodesk-signal -p aerodesk-agent

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

# 先起两个 viewer 并等它们登记选层（High/Low），再启动 publisher：
# 让 viewer 赶上 publisher 的首个关键帧（#0），避免“迟到 viewer 等下一
# 个关键帧”造成 f 层偶发 0 帧（#66 排查结论）。
echo "== 启动 viewer f/q（先加入并登记选层）"
./target/release/aerodesk-agent --role viewer --layer f \
    --signal ws://127.0.0.1:3003 --room "$ROOM" >/tmp/sim-view-f.log 2>&1 &
F_PID=$!
./target/release/aerodesk-agent --role viewer --layer q \
    --signal ws://127.0.0.1:3003 --room "$ROOM" >/tmp/sim-view-q.log 2>&1 &
Q_PID=$!
ready=0
for _ in $(seq 1 50); do
    if grep -q "layer request sent" /tmp/sim-view-f.log 2>/dev/null \
        && grep -q "layer request sent" /tmp/sim-view-q.log 2>/dev/null; then
        ready=1; break
    fi
    sleep 0.2
done
if [ "$ready" != "1" ]; then
    echo "FAIL viewer 未能在 10s 内登记选层"
    echo "--- f log:"; cat /tmp/sim-view-f.log 2>/dev/null | tail -5
    echo "--- q log:"; cat /tmp/sim-view-q.log 2>/dev/null | tail -5
    echo "--- sig log:"; tail -5 /tmp/sim-sig.log 2>/dev/null
    kill "$F_PID" "$Q_PID" "$SFU_PID" "$SIG_PID" 2>/dev/null || true
    exit 1
fi

echo "== 启动 publisher（x264 --simulcast --noisy: q/h/f）"
./target/release/aerodesk-agent --role publisher --encoder x264 --simulcast --noisy \
    --signal ws://127.0.0.1:3003 --room "$ROOM" >/tmp/sim-pub.log 2>&1 &
PUB_PID=$!
echo "== 观察 f/q 层 ${OBS}s"
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
# 3) f 层单帧平均大小显著高于 q 层（分辨率/码率切换真实生效）。
# 用“平均帧大小”而不是总字节：共享机上 f 层软编帧率低，总字节会被帧率
# 稀释（#66 排查结论），单帧大小反映真实的分辨率/码率梯度。
bytes_f=$(grep -oE "RECEIVED: [0-9]+ frames, [0-9]+ bytes" /tmp/sim-view-f.log | tail -1 | grep -oE "[0-9]+ bytes" | grep -oE "[0-9]+" || echo 0)
bytes_q=$(grep -oE "RECEIVED: [0-9]+ frames, [0-9]+ bytes" /tmp/sim-view-q.log | tail -1 | grep -oE "[0-9]+ bytes" | grep -oE "[0-9]+" || echo 0)
frames_f=$(grep -oE "RECEIVED: [0-9]+ frames" /tmp/sim-view-f.log | tail -1 | grep -oE "[0-9]+" || echo 0)
frames_q=$(grep -oE "RECEIVED: [0-9]+ frames" /tmp/sim-view-q.log | tail -1 | grep -oE "[0-9]+" || echo 0)
avg_f=$(( bytes_f / (frames_f > 0 ? frames_f : 1) ))
avg_q=$(( bytes_q / (frames_q > 0 ? frames_q : 1) ))
echo "  f: $frames_f frames / $bytes_f bytes (avg ${avg_f}B); q: $frames_q frames / $bytes_q bytes (avg ${avg_q}B)"
if [ "${frames_f:-0}" -gt 0 ] && [ "${frames_q:-0}" -gt 0 ] && [ "$avg_f" -gt $((avg_q * 2)) ] 2>/dev/null; then
    echo "PASS layer switching changes bitrate/resolution (f avg > 2x q avg)"
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
