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
cargo build -q -p aerodesk-sfu -p aerodesk-signal -p aerodesk-agent

REC="$(mktemp -d)"
echo "== 启动 sfu/signal"
RECORD_DIR="$REC" ./target/debug/aerodesk-sfu >/tmp/opus-sfu.log 2>&1 &
SFU_PID=$!
SIP_UDP_PORT=5060 ./target/debug/aerodesk-signal >/tmp/opus-sig.log 2>&1 &
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
./target/debug/aerodesk-agent --role publisher --audio --audio-opus \
    --signal ws://127.0.0.1:3003 --room "$ROOM" >/tmp/opus-pub.log 2>&1 &
PUB_PID=$!
sleep 2

echo "== viewer A（--audio，正常接收 Opus）"
./target/debug/aerodesk-agent --role viewer --audio \
    --signal ws://127.0.0.1:3003 --room "$ROOM" >/tmp/opus-a.log 2>&1 &
A_PID=$!
# 轮询等待 Opus 音频到达（CI 慢启动时固定 sleep 会误判；最多 ~40s）
OPUS_OK=0
for _ in $(seq 1 80); do
    if grep -qE "AUDIO: [1-9][0-9]* frames [1-9]" /tmp/opus-a.log 2>/dev/null; then OPUS_OK=1; break; fi
    if ! kill -0 "$A_PID" 2>/dev/null; then break; fi
    sleep 0.5
done
kill "$A_PID" 2>/dev/null || true
wait "$A_PID" 2>/dev/null || true

# #552 SIP 1:1：一个 publisher 服务一个呼叫——viewer A 结束后为 viewer B
# 起新配对（旧 publisher 随 A 呼叫结束，BYE 后不再接新 INVITE）。
kill "$PUB_PID" 2>/dev/null || true
wait "$PUB_PID" 2>/dev/null || true
ROOM_B="${ROOM}-b"
echo "== publisher B（viewer B 配对，--audio --audio-opus）"
./target/debug/aerodesk-agent --role publisher --audio --audio-opus \
    --signal ws://127.0.0.1:3003 --room "$ROOM_B" >/tmp/opus-pub-b.log 2>&1 &
PUB_PID=$!
sleep 2

echo "== viewer B（--audio --mute-audio，静音丢弃）"
./target/debug/aerodesk-agent --role viewer --audio --mute-audio \
    --signal ws://127.0.0.1:3003 --room "$ROOM_B" >/tmp/opus-b.log 2>&1 &
B_PID=$!
MUTE_OK=0
DROP_OK=0
for _ in $(seq 1 80); do
    grep -q "audio mute command sent" /tmp/opus-b.log 2>/dev/null && MUTE_OK=1
    grep -qE "muted=true dropped=[1-9]" /tmp/opus-b.log 2>/dev/null && DROP_OK=1
    if [ "$MUTE_OK" = "1" ] && [ "$DROP_OK" = "1" ]; then break; fi
    if ! kill -0 "$B_PID" 2>/dev/null; then break; fi
    sleep 0.5
done
kill "$B_PID" "$PUB_PID" "$SFU_PID" "$SIG_PID" 2>/dev/null || true
wait 2>/dev/null || true

echo "== 断言"
fail=0
# 1) publisher 走 Opus（libopus 编码器打开；A/B 任一路即可）
if grep -q "opus encoder opened" /tmp/opus-pub.log /tmp/opus-pub-b.log 2>/dev/null; then
    echo "PASS opus encoder opened on publisher"
else
    echo "FAIL opus encoder"; tail -5 /tmp/opus-pub.log; fail=1
fi
# 2) viewer A 收到音频（Opus 帧）
if [ "$OPUS_OK" = "1" ]; then
    echo "PASS opus receive (AUDIO frames/bytes > 0)"
else
    echo "FAIL opus receive"; tail -3 /tmp/opus-a.log; fail=1
fi
# 3) viewer B 下发静音并丢弃
if [ "$MUTE_OK" = "1" ]; then
    echo "PASS audio mute command sent"
else
    echo "FAIL mute command"; tail -3 /tmp/opus-b.log; fail=1
fi
if [ "$DROP_OK" = "1" ]; then
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
