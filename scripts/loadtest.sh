#!/usr/bin/env bash
# AeroDesk 压测：N 房间 × P 对（发布端 + 观看端）。
# 前置：先起 sfu + signal（见 docs/DEPLOYMENT.md）；本脚本只负责施压。
#
# 用法：
#   scripts/loadtest.sh [rooms] [pairs] [seconds] [width] [height] [fps]
#   JWT_TOKEN=<jwt> scripts/loadtest.sh 2 2 10 1920 1080 30
set -euo pipefail

ROOMS="${1:-2}"
PAIRS="${2:-1}"
RUN_SECONDS="${3:-10}"
W="${4:-1920}"
H="${5:-1080}"
FPS="${6:-30}"
SIGNAL="${SIGNAL:-ws://127.0.0.1:3003}"
BIN="${BIN:-$(cd "$(dirname "$0")/.." && pwd)/target/debug/aerodesk-cli}"
BITRATE="${BITRATE:-8000000}"
# #8 高熵合成源（NOISY=1）：码率贴近目标档位，避免彩条源过度可压缩导致
# 吞吐失真（实测彩条 4K 只有 ~0.3Mbps）。
NOISY="${NOISY:-0}"
TOKEN="${JWT_TOKEN:-}"

pids=()
cleanup() { [ ${#pids[@]} -gt 0 ] && kill "${pids[@]}" 2>/dev/null || true; }
trap cleanup EXIT

TARG=""
[ -n "$TOKEN" ] && TARG="--token $TOKEN"

echo "== 压测开始: ${ROOMS} 房间 × ${PAIRS} 对 @ ${W}x${H}/${FPS}fps ${BITRATE}bps，时长 ${RUN_SECONDS}s"
for r in $(seq 1 "$ROOMS"); do
  for p in $(seq 1 "$PAIRS"); do
    room="load-r${r}"
    "$BIN" --role publisher --signal "$SIGNAL" --room "$room" --encoder vt \
      --width "$W" --height "$H" --fps "$FPS" --bitrate "$BITRATE" \
      $([ "$NOISY" = "1" ] && echo --noisy) $TARG \
      >"/tmp/load-pub-${r}-${p}.log" 2>&1 &
    pids+=($!)
    "$BIN" --role viewer --signal "$SIGNAL" --room "$room" $TARG \
      >"/tmp/load-view-${r}-${p}.log" 2>&1 &
    pids+=($!)
  done
done

sleep "$RUN_SECONDS"
cleanup
wait 2>/dev/null || true

echo "== 发布端: $(grep -l 'ICE connected' /tmp/load-pub-*.log 2>/dev/null | wc -l | tr -d ' ') 个连接成功"
echo "== 观看端: $(grep -l 'ICE connected' /tmp/load-view-*.log 2>/dev/null | wc -l | tr -d ' ') 个连接成功"
echo "== 错误样例 =="
grep -hiE "error|panic|auth failed" /tmp/load-pub-*.log /tmp/load-view-*.log 2>/dev/null | head -5 || true
echo "== 完成 =="
