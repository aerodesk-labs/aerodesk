#!/usr/bin/env bash
# #58 音频链路端到端：publisher --audio（合成 PCMU 440Hz）→ SFU → viewer。
# 断言：
#   1. 普通 viewer 收到音频帧/字节（AUDIO: N frames X bytes，X>0）
#   2. --mute-audio viewer 下发静音指令、丢弃音频帧（muted=true dropped>0，bytes=0）
# 用法: scripts/audio-e2e.sh [房间] [观察秒数]
set -euo pipefail
cd "$(dirname "$0")/.."

ROOM="${1:-audio-$(date +%s)}"
OBS="${2:-6}"
export RUST_LOG="${RUST_LOG:-info}"

echo "== 构建"
cargo build -q -p aerodesk-sfu -p aerodesk-signal -p aerodesk-agent

REC="$(mktemp -d)"
echo "== 启动 sfu/signal"
RECORD_DIR="$REC" ./target/debug/aerodesk-sfu >/tmp/audio-sfu.log 2>&1 &
SFU_PID=$!
./target/debug/aerodesk-signal >/tmp/audio-sig.log 2>&1 &
SIG_PID=$!
for _ in $(seq 1 50); do
    if nc -z 127.0.0.1 3003 2>/dev/null; then break; fi
    if ! kill -0 "$SIG_PID" 2>/dev/null; then
        echo "signal 服务器启动失败"; cat /tmp/audio-sig.log; exit 1
    fi
    sleep 0.2
done
sleep 0.3

echo "== 启动 publisher（pcap 视频 + --audio PCMU）"
./target/debug/aerodesk-agent --role publisher --audio \
    --signal ws://127.0.0.1:3003 --room "$ROOM" >/tmp/audio-pub.log 2>&1 &
PUB_PID=$!
sleep 2

echo "== viewer A（--audio，正常接收）"
./target/debug/aerodesk-agent --role viewer --audio \
    --signal ws://127.0.0.1:3003 --room "$ROOM" >/tmp/audio-a.log 2>&1 &
A_PID=$!
# 轮询等待音频到达（CI 慢启动时固定 sleep 会误判；最多 ~40s）
AUDIO_OK=0
for _ in $(seq 1 80); do
    if grep -qE "AUDIO: [1-9][0-9]* frames [1-9]" /tmp/audio-a.log 2>/dev/null; then AUDIO_OK=1; break; fi
    if ! kill -0 "$A_PID" 2>/dev/null; then break; fi
    sleep 0.5
done
kill "$A_PID" 2>/dev/null || true
wait "$A_PID" 2>/dev/null || true

echo "== viewer B（--audio --mute-audio，静音丢弃）"
./target/debug/aerodesk-agent --role viewer --audio --mute-audio \
    --signal ws://127.0.0.1:3003 --room "$ROOM" >/tmp/audio-b.log 2>&1 &
B_PID=$!
MUTE_OK=0
DROP_OK=0
for _ in $(seq 1 80); do
    grep -q "audio mute command sent" /tmp/audio-b.log 2>/dev/null && MUTE_OK=1
    grep -qE "muted=true dropped=[1-9]" /tmp/audio-b.log 2>/dev/null && DROP_OK=1
    if [ "$MUTE_OK" = "1" ] && [ "$DROP_OK" = "1" ]; then break; fi
    if ! kill -0 "$B_PID" 2>/dev/null; then break; fi
    sleep 0.5
done
kill "$B_PID" "$PUB_PID" "$SFU_PID" "$SIG_PID" 2>/dev/null || true
wait 2>/dev/null || true

echo "== 断言"
fail=0
# 1) viewer A 收到音频
if [ "$AUDIO_OK" = "1" ]; then
    echo "PASS audio receive (AUDIO frames/bytes > 0)"
else
    echo "FAIL audio receive"; tail -3 /tmp/audio-a.log; fail=1
fi
# 2) viewer B 下发静音并丢弃
if [ "$MUTE_OK" = "1" ]; then
    echo "PASS audio mute command sent"
else
    echo "FAIL mute command"; tail -3 /tmp/audio-b.log; fail=1
fi
if [ "$DROP_OK" = "1" ]; then
    echo "PASS audio dropped when muted"
else
    echo "FAIL muted drop"; tail -3 /tmp/audio-b.log; fail=1
fi
# 3) 无 panic
if grep -qiE "panic" /tmp/audio-pub.log /tmp/audio-a.log /tmp/audio-b.log /tmp/audio-sfu.log; then
    echo "FAIL panic in logs"; fail=1
else
    echo "PASS no panics"
fi

exit $fail
