#!/usr/bin/env bash
# #226 SFU 多编码格式端到端转发验收：
#   h264（vt 回归）+ h265/vp9/av1（ffmpeg 软编）各起 SFU+signal → publisher → viewer，
#   断言 viewer DECODED>0（SFU 选择性转发、不重编码），无 panic。
# 用法: scripts/sfu-codec-e2e.sh [codec...]  默认: h264 h265 vp9 av1
set -uo pipefail
cd "$(dirname "$0")/.."
export RUST_LOG="${RUST_LOG:-info}"
TARGET_DIR="${CARGO_TARGET_DIR:-$PWD/target}/debug"

CODECS=("$@"); [ ${#CODECS[@]} -eq 0 ] && CODECS=(h264 h265 vp9 av1)

fail() { echo "FAIL: $*"; exit 1; }

echo "== 构建"
cargo build -q -p aerodesk-sfu -p aerodesk-signal -p aerodesk-agent

echo "codec    encoder   result   frames   decoded   errors"
for C in "${CODECS[@]}"; do
  pkill -f 'aerodesk-(sfu|signal|cli)' 2>/dev/null || true
  sleep 1
  REC="$(mktemp -d)"
  RECORD_DIR="$REC" SFU_MEDIA_PORT=14578 SFU_SIGNAL_PORT=14500 SFU_INTERNAL_PORT=14502 \
    "$TARGET_DIR/aerodesk-sfu" >/tmp/codec-sfu.log 2>&1 &
  SFU=$!
  SIGNAL_PORT=14501 SFU_URL=http://127.0.0.1:14502 \
    "$TARGET_DIR/aerodesk-signal" >/tmp/codec-sig.log 2>&1 &
  SIG=$!
  for _ in $(seq 1 80); do
    nc -z 127.0.0.1 14502 2>/dev/null && nc -z 127.0.0.1 14503 2>/dev/null && break
    sleep 0.2
  done
  sleep 0.3

  ENC=ffmpeg
  EXTRA=()
  if [ "$C" = "h264" ]; then ENC=vt; fi
  "$TARGET_DIR/aerodesk-agent" --role publisher --signal ws://127.0.0.1:14503 --room "codec-${C}" \
    --encoder "$ENC" --codec "$C" --noisy >/tmp/codec-pub.log 2>&1 &
  PUB=$!
  "$TARGET_DIR/aerodesk-agent" --role viewer --signal ws://127.0.0.1:14503 --room "codec-${C}" \
    >/tmp/codec-view.log 2>&1 &
  VIEW=$!

  ok=0
  for _ in $(seq 1 240); do
    if grep -qE "DECODED: [1-9]" /tmp/codec-view.log 2>/dev/null; then ok=1; break; fi
    if ! kill -0 "$PUB" 2>/dev/null || ! kill -0 "$VIEW" 2>/dev/null; then break; fi
    sleep 0.5
  done
  FRAMES=$(grep -oE "RECEIVED: [0-9]+ frames" /tmp/codec-view.log | tail -1)
  DECODED=$(grep -oE "DECODED: [0-9]+" /tmp/codec-view.log | tail -1)
  ERR=$(grep -ciE "panic|abort" /tmp/codec-sfu.log /tmp/codec-pub.log /tmp/codec-view.log 2>/dev/null | awk '{s+=$1} END{print s+0}')
  if [ "$ok" = "1" ] && [ "$ERR" = "0" ]; then
    echo "$C      $ENC      PASS     ${FRAMES:-?}   ${DECODED:-?}     0"
  else
    echo "$C      $ENC      FAIL     ${FRAMES:-0}   ${DECODED:-0}     ${ERR}"
    echo "--- $C publisher tail ---"; tail -5 /tmp/codec-pub.log
    echo "--- $C viewer tail ---"; tail -5 /tmp/codec-view.log
    kill "$VIEW" "$PUB" "$SFU" "$SIG" 2>/dev/null || true
    wait 2>/dev/null || true
    exit 1
  fi
  kill "$VIEW" "$PUB" "$SFU" "$SIG" 2>/dev/null || true
  wait 2>/dev/null || true
done
echo "== SFU 多编码格式转发 PASS（${CODECS[*]}）=="
