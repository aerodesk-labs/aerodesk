#!/usr/bin/env bash
# #74 多 codec 端到端：publisher --encoder ffmpeg --codec <c> → SFU → viewer 收帧。
# 覆盖 h264 / h265(hevc) / vp9 / av1。AV1(SVT) 编码有 ~1s 延迟，观察窗给足。
# 用法: scripts/codec-e2e.sh [房间] [每 codec 观察秒数]
set -euo pipefail
cd "$(dirname "$0")/.."

ROOM="${1:-codec-$(date +%s)}"
OBS="${2:-10}"
export RUST_LOG="${RUST_LOG:-info}"

echo "== 构建"
cargo build -q -p aerodesk-sfu -p aerodesk-signal -p aerodesk-cli

fail=0
for codec in h264 h265 vp9 av1; do
  echo "=== codec=$codec"
  REC="$(mktemp -d)"
  RECORD_DIR="$REC" ./target/debug/aerodesk-sfu >"/tmp/codec-$codec-sfu.log" 2>&1 &
  SFU_PID=$!
  ./target/debug/aerodesk-signal >"/tmp/codec-$codec-sig.log" 2>&1 &
  SIG_PID=$!
  for _ in $(seq 1 50); do
    if nc -z 127.0.0.1 3003 2>/dev/null && nc -z 127.0.0.1 3002 2>/dev/null; then break; fi
    if ! kill -0 "$SFU_PID" 2>/dev/null || ! kill -0 "$SIG_PID" 2>/dev/null; then
      echo "sfu/signal 服务器启动失败"; tail -5 "/tmp/codec-$codec-sfu.log"; tail -5 "/tmp/codec-$codec-sig.log"; exit 1
    fi
    sleep 0.2
  done
  sleep 0.3

  ./target/debug/aerodesk-cli --role publisher --encoder ffmpeg --codec "$codec" \
      --signal ws://127.0.0.1:3003 --room "$ROOM" >"/tmp/codec-$codec-pub.log" 2>&1 &
  PUB_PID=$!
  ./target/debug/aerodesk-cli --role viewer \
      --signal ws://127.0.0.1:3003 --room "$ROOM" >"/tmp/codec-$codec-view.log" 2>&1 &
  VIEW_PID=$!
  sleep "$OBS"
  kill "$PUB_PID" "$VIEW_PID" "$SFU_PID" "$SIG_PID" 2>/dev/null || true
  wait 2>/dev/null || true

  if grep -qE "RECEIVED: [1-9]" "/tmp/codec-$codec-view.log"; then
    echo "PASS codec=$codec viewer received frames"
  else
    echo "FAIL codec=$codec"; tail -4 "/tmp/codec-$codec-view.log"; tail -4 "/tmp/codec-$codec-pub.log"; fail=1
  fi
done
exit $fail
