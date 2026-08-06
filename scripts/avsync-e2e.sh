#!/usr/bin/env bash
# #73 A/V 同步链路：publisher（视频+音频）→ SFU → viewer，AVSYNC 统计（漂移/jitter）。
# 真实播放需 macOS 音频设备，CI 验证同步机制与统计。
# 用法: scripts/avsync-e2e.sh [房间] [观察秒数]
set -euo pipefail
cd "$(dirname "$0")/.."

ROOM="${1:-avsync-$(date +%s)}"
OBS="${2:-6}"
export RUST_LOG="${RUST_LOG:-info}"

echo "== 构建"
cargo build -q -p aerodesk-sfu -p aerodesk-signal -p aerodesk-cli

REC="$(mktemp -d)"
echo "== 启动 sfu/signal"
RECORD_DIR="$REC" ./target/debug/aerodesk-sfu >/tmp/av-sfu.log 2>&1 &
SFU_PID=$!
./target/debug/aerodesk-signal >/tmp/av-sig.log 2>&1 &
SIG_PID=$!
for _ in $(seq 1 50); do
    if nc -z 127.0.0.1 3003 2>/dev/null && nc -z 127.0.0.1 3002 2>/dev/null; then break; fi
    if ! kill -0 "$SFU_PID" 2>/dev/null || ! kill -0 "$SIG_PID" 2>/dev/null; then
        echo "sfu/signal 启动失败"; tail -5 /tmp/av-sfu.log; tail -5 /tmp/av-sig.log; exit 1
    fi
    sleep 0.2
done
sleep 0.3

echo "== 启动 publisher（视频 + --audio）+ viewer（--audio）"
# 连续视频源（x264 合成，避免 pcap 48 帧发完导致漂移统计假象）
./target/debug/aerodesk-cli --role publisher --encoder x264 --audio \
    --signal ws://127.0.0.1:3003 --room "$ROOM" >/tmp/av-pub.log 2>&1 &
PUB_PID=$!
./target/debug/aerodesk-cli --role viewer --audio \
    --signal ws://127.0.0.1:3003 --room "$ROOM" >/tmp/av-view.log 2>&1 &
VIEW_PID=$!
sleep "$OBS"
kill "$PUB_PID" "$VIEW_PID" "$SFU_PID" "$SIG_PID" 2>/dev/null || true
wait 2>/dev/null || true

echo "== 断言"
fail=0
# 1) 视频与音频都收到
if grep -qE "RECEIVED: [1-9]" /tmp/av-view.log && grep -qE "AUDIO: [1-9]" /tmp/av-view.log; then
    echo "PASS video+audio received"
else
    echo "FAIL media receive"; tail -3 /tmp/av-view.log; fail=1
fi
# 2) AVSYNC 统计出现（音频/视频时间轴 + 漂移）
if grep -q "AVSYNC:" /tmp/av-view.log; then
    echo "PASS AVSYNC stats"
else
    echo "FAIL AVSYNC"; tail -3 /tmp/av-view.log; fail=1
fi
# 3) 漂移有界（±500ms 内；首帧对齐后应接近 0）
DRIFT=$(grep -oE 'drift=[-0-9.]+ms' /tmp/av-view.log | tail -1 | sed 's/drift=//; s/ms//')
if [ -n "$DRIFT" ] && awk -v d="$DRIFT" 'BEGIN { exit !(d >= -500 && d <= 500) }'; then
    echo "PASS drift bounded (${DRIFT}ms)"
else
    echo "FAIL drift out of bounds: ${DRIFT:-none}"; tail -3 /tmp/av-view.log; fail=1
fi
# 4) jitter buffer 工作（播放计数 > 0）
if grep -qE "played=[1-9]" /tmp/av-view.log; then
    echo "PASS jitter buffer played"
else
    echo "FAIL jitter played"; tail -3 /tmp/av-view.log; fail=1
fi
# 5) 无 panic
if grep -qiE "panic" /tmp/av-pub.log /tmp/av-view.log /tmp/av-sfu.log; then
    echo "FAIL panic in logs"; fail=1
else
    echo "PASS no panics"
fi
exit $fail
