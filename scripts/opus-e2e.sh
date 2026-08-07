#!/usr/bin/env bash
# #73 Opus 音频链路端到端：publisher --audio --audio-opus（libopus 48kHz）→ SFU → viewer。
# 断言：
#   1. publisher 打开 libopus 编码器（opus encoder opened，证明走 Opus 而非 PCMU）
#   2. 普通 viewer 收到音频帧/字节（AUDIO: N frames X bytes，X>0）
#   3. --mute-audio viewer 下发静音指令、丢弃音频帧（muted=true dropped>0）
# 用法: scripts/opus-e2e.sh [房间] [观察秒数]
set -euo pipefail
cd "$(dirname "$0")/.."

ROOM="${1:-opus-$(date +%s)}"
OBS="${2:-6}"
export RUST_LOG="${RUST_LOG:-info}"

echo "== 构建"
cargo build -q -p aerodesk-sfu -p aerodesk-signal -p aerodesk-cli

REC="$(mktemp -d)"
echo "== 启动 sfu/signal"
RECORD_DIR="$REC" ./target/debug/aerodesk-sfu >/tmp/opus-sfu.log 2>&1 &
SFU_PID=$!
./target/debug/aerodesk-signal >/tmp/opus-sig.log 2>&1 &
SIG_PID=$!
for _ in $(seq 1 50); do
    if nc -z 127.0.0.1 3003 2>/dev/null; then break; fi
    if ! kill -0 "$SIG_PID" 2>/dev/null; then
        echo "signal 服务器启动失败"; cat /tmp/opus-sig.log; exit 1
    fi
    sleep 0.2
done
sleep 0.3

echo "== 启动 publisher（pcap 视频 + --audio --audio-opus）"
./target/debug/aerodesk-cli --role publisher --audio --audio-opus \
    --signal ws://127.0.0.1:3003 --room "$ROOM" >/tmp/opus-pub.log 2>&1 &
PUB_PID=$!
sleep 2

echo "== viewer A（--audio，正常接收 Opus）"
./target/debug/aerodesk-cli --role viewer --audio \
    --signal ws://127.0.0.1:3003 --room "$ROOM" >/tmp/opus-a.log 2>&1 &
A_PID=$!
sleep "$OBS"
kill "$A_PID" 2>/dev/null || true
wait "$A_PID" 2>/dev/null || true

echo "== viewer B（--audio --mute-audio，静音丢弃）"
./target/debug/aerodesk-cli --role viewer --audio --mute-audio \
    --signal ws://127.0.0.1:3003 --room "$ROOM" >/tmp/opus-b.log 2>&1 &
B_PID=$!
sleep "$OBS"
kill "$B_PID" "$PUB_PID" "$SFU_PID" "$SIG_PID" 2>/dev/null || true
wait 2>/dev/null || true

echo "== 断言"
fail=0
# 1) publisher 走 Opus（libopus 编码器打开）
if grep -q "opus encoder opened" /tmp/opus-pub.log; then
    echo "PASS opus encoder opened on publisher"
else
    echo "FAIL opus encoder"; tail -5 /tmp/opus-pub.log; fail=1
fi
# 2) viewer A 收到音频（Opus 帧）
if grep -qE "AUDIO: [1-9][0-9]* frames [1-9]" /tmp/opus-a.log; then
    echo "PASS opus receive (AUDIO frames/bytes > 0)"
else
    echo "FAIL opus receive"; tail -3 /tmp/opus-a.log; fail=1
fi
# 3) viewer B 下发静音并丢弃
if grep -q "audio mute command sent" /tmp/opus-b.log; then
    echo "PASS audio mute command sent"
else
    echo "FAIL mute command"; tail -3 /tmp/opus-b.log; fail=1
fi
if grep -qE "muted=true dropped=[1-9]" /tmp/opus-b.log; then
    echo "PASS opus dropped when muted"
else
    echo "FAIL muted drop"; tail -3 /tmp/opus-b.log; fail=1
fi
# 4) 无 panic
if grep -qiE "panic" /tmp/opus-pub.log /tmp/opus-a.log /tmp/opus-b.log /tmp/opus-sfu.log; then
    echo "FAIL panic in logs"; fail=1
else
    echo "PASS no panics"
fi

exit $fail
