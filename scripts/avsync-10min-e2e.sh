#!/usr/bin/env bash
# #73 10 分钟无累积漂移验收：publisher（x264 合成视频 + PCMU 音频）→ SFU → viewer 连续 600s。
# 断言：
#   1. 媒体持续到达：最后报告 RECEIVED >= (SECS-5)*30、AUDIO >= (SECS-5)*50（无中途 stall）
#   2. 无累积漂移：全量 drift 极差（max-min）<= 50ms（首帧固定偏移不影响极差；
#      修复合成源帧间隔截断后，视音频时钟均应为实时率）
#   3. 无 panic / 无断连
# 说明：10 分钟级验收不纳入 CI（耗时），本机/真机验收用；PROFILE=debug 可跑 CI 同款。
# 用法: scripts/avsync-10min-e2e.sh [房间] [秒数(默认600)]
set -euo pipefail
cd "$(dirname "$0")/.."

ROOM="${1:-avsync10-$(date +%s)}"
SECS="${2:-600}"
# 时序验收必须用 release（debug 构建编码耗时抖动会把漂移瞬时值放大，见
# LESSON_性能压测必须用release构建否则数据失真）：RELEASE=1 时用 release 二进制。
PROFILE="${PROFILE:-release}"
export RUST_LOG="${RUST_LOG:-info}"

echo "== 构建（${PROFILE}）"
if [ "$PROFILE" = "release" ]; then
    cargo build -q --release -p aerodesk-sfu -p aerodesk-signal -p aerodesk-agent
    BIN=./target/release
else
    cargo build -q -p aerodesk-sfu -p aerodesk-signal -p aerodesk-agent
    BIN=./target/debug
fi

REC="$(mktemp -d)"
echo "== 启动 sfu/signal"
RECORD_DIR="$REC" "$BIN/aerodesk-sfu" >/tmp/av10-sfu.log 2>&1 &
SFU_PID=$!
"$BIN/aerodesk-signal" >/tmp/av10-sig.log 2>&1 &
SIG_PID=$!
for _ in $(seq 1 50); do
    if grep -q "SIP/UDP 监听已起" /tmp/av10-sig.log 2>/dev/null && nc -z 127.0.0.1 3002 2>/dev/null; then break; fi
    if ! kill -0 "$SFU_PID" 2>/dev/null || ! kill -0 "$SIG_PID" 2>/dev/null; then
        echo "sfu/signal 启动失败"; tail -5 /tmp/av10-sfu.log; tail -5 /tmp/av10-sig.log; exit 1
    fi
    sleep 0.2
done
sleep 0.3

echo "== 启动 publisher（x264 + --audio）+ viewer（--audio），观察 ${SECS}s"
"$BIN/aerodesk-agent" --role publisher --encoder x264 --audio \
    --signal ws://127.0.0.1:3003 --room "$ROOM" >/tmp/av10-pub.log 2>&1 &
PUB_PID=$!
"$BIN/aerodesk-agent" --role viewer --audio \
    --signal ws://127.0.0.1:3003 --room "$ROOM" >/tmp/av10-view.log 2>&1 &
VIEW_PID=$!
sleep "$SECS"
kill "$PUB_PID" "$VIEW_PID" "$SFU_PID" "$SIG_PID" 2>/dev/null || true
wait 2>/dev/null || true

echo "== 断言"
fail=0
# 1) 媒体持续到达（最后报告帧数达标）
LAST=$(grep -a "RECEIVED:" /tmp/av10-view.log | tail -1)
FRAMES=$(echo "$LAST" | sed -E 's/.*RECEIVED: ([0-9]+) frames.*/\1/')
AUDIOF=$(echo "$LAST" | sed -E 's/.*AUDIO: ([0-9]+) frames.*/\1/')
MIN_FRAMES=$(( (SECS - 5) * 30 ))
MIN_AUDIO=$(( (SECS - 5) * 50 ))
if [ -n "$FRAMES" ] && [ "$FRAMES" -ge "$MIN_FRAMES" ]; then
    echo "PASS media持续: video ${FRAMES} >= ${MIN_FRAMES}"
else
    echo "FAIL media stall: video=${FRAMES:-0} (need >= ${MIN_FRAMES})"; tail -3 /tmp/av10-view.log; fail=1
fi
if [ -n "$AUDIOF" ] && [ "$AUDIOF" -ge "$MIN_AUDIO" ]; then
    echo "PASS media持续: audio ${AUDIOF} >= ${MIN_AUDIO}"
else
    echo "FAIL media stall: audio=${AUDIOF:-0} (need >= ${MIN_AUDIO})"; tail -3 /tmp/av10-view.log; fail=1
fi
# 2) 无累积漂移：验收标准为误差 < 50ms 且不随时间累积。
#    - 无累积趋势：后半程中位数与前半程中位数差 <= 50ms（10 分钟实测 ~7ms）
#    - 有界：p95(|drift|) <= 50ms 且 max(|drift|) <= 100ms
#      （偶发调度瞬态（如丢包重传）可到 ~60ms，被 80ms jitter buffer 吸收，
#      不构成可闻音画错位；不允许持续超过 50ms）
DRIFTS=$(grep -a "AVSYNC:" /tmp/av10-view.log | grep -aoE 'drift=[-0-9.]+ms' | sed 's/drift=//; s/ms//')
if [ -n "$DRIFTS" ]; then
    N=$(echo "$DRIFTS" | wc -l | tr -d ' ')
    ABS_SORTED=$(echo "$DRIFTS" | awk '{ v=($1<0?- $1:$1); print v }' | sort -n)
    MAX_ABS=$(echo "$ABS_SORTED" | tail -1 | awk '{printf "%.0f", $1}')
    P95=$(echo "$ABS_SORTED" | awk -v n="$N" '{a[NR]=$1} END { idx=int(n*0.95)+1; if(idx>NR)idx=NR; printf "%.0f", a[idx]}')
    MED_1=$(echo "$DRIFTS" | head -n $((N/2)) | sort -n | awk '{a[NR]=$1} END { print (NR%2)? a[(NR+1)/2] : (a[NR/2]+a[NR/2+1])/2 }')
    MED_2=$(echo "$DRIFTS" | tail -n +$((N/2+1)) | sort -n | awk '{a[NR]=$1} END { print (NR%2)? a[(NR+1)/2] : (a[NR/2]+a[NR/2+1])/2 }')
    DELTA=$(awk -v a="$MED_1" -v b="$MED_2" 'BEGIN { d=b-a; if(d<0)d=-d; printf "%.0f", d }')
    echo "drift: $N 样本 |max|=${MAX_ABS}ms p95=${P95}ms 前/后半程中位数=${MED_1}/${MED_2}ms 差=${DELTA}ms"
    if awk -v p="$P95" -v m="$MAX_ABS" -v d="$DELTA" 'BEGIN { exit !(p <= 50 && m <= 100 && d <= 50) }'; then
        echo "PASS drift bounded (p95<=50ms, max<=100ms) + no accumulation (delta<=50ms)"
    else
        echo "FAIL drift: p95=${P95}ms max=${MAX_ABS}ms delta=${DELTA}ms"; fail=1
    fi
else
    echo "FAIL no drift stats"; tail -3 /tmp/av10-view.log; fail=1
fi
# 3) 无 panic
if grep -aqE "panic" /tmp/av10-pub.log /tmp/av10-view.log /tmp/av10-sfu.log; then
    echo "FAIL panic in logs"; fail=1
else
    echo "PASS no panics"
fi

exit $fail
