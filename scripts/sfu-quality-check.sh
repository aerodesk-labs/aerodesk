#!/usr/bin/env bash
# #238 SFU 媒体质量指标检查：1×1 直连 + TURN 中继各跑 ~18s，
# 采样 /metrics/prometheus 的 rtt/egress·ingress loss/bwe/qos_clients，
# 断言：qos_clients>0（PeerStats 聚合生效）、loss=0（无丢包）、无 panic。
# 用法: scripts/sfu-quality-check.sh [mode...]   默认: direct turn
set -uo pipefail
cd "$(dirname "$0")/.."
export RUST_LOG="${RUST_LOG:-info}"
TARGET_DIR="${CARGO_TARGET_DIR:-$PWD/target}/debug"

MODES=("$@"); [ ${#MODES[@]} -eq 0 ] && MODES=(direct turn)
fail() { echo "FAIL: $*"; exit 1; }

echo "== 构建"
cargo build -q -p aerodesk-sfu -p aerodesk-signal -p aerodesk-cli

echo "mode     qos_clients   rtt_us   egress_loss   ingress_loss   bwe_tx_bps   errors"
for MODE in "${MODES[@]}"; do
  pkill -f 'aerodesk-(sfu|signal|cli)' 2>/dev/null || true
  sleep 1
  REC="$(mktemp -d)"
  if [ "$MODE" = "turn" ]; then
    export AERODESK_FORCE_RELAY; AERODESK_FORCE_RELAY=1
    RECORD_DIR="$REC" SFU_MEDIA_PORT=14578 SFU_SIGNAL_PORT=14500 SFU_INTERNAL_PORT=14502 \
      TURN_SECRET=testsecret SFU_TURN_PORT=14789 \
      "$TARGET_DIR/aerodesk-sfu" >/tmp/q-sfu.log 2>&1 &
    SFU=$!
    SIGNAL_PORT=14501 SIGNAL_PLAIN_PORT=14503 SFU_URL=http://127.0.0.1:14502 \
      TURN_SECRET=testsecret TURN_URLS="turn:127.0.0.1:14789?transport=udp" \
      "$TARGET_DIR/aerodesk-signal" >/tmp/q-sig.log 2>&1 &
    SIG=$!
  else
    AERODESK_FORCE_RELAY=0
    RECORD_DIR="$REC" SFU_MEDIA_PORT=14578 SFU_SIGNAL_PORT=14500 SFU_INTERNAL_PORT=14502 \
      "$TARGET_DIR/aerodesk-sfu" >/tmp/q-sfu.log 2>&1 &
    SFU=$!
    SIGNAL_PORT=14501 SIGNAL_PLAIN_PORT=14503 SFU_URL=http://127.0.0.1:14502 \
      "$TARGET_DIR/aerodesk-signal" >/tmp/q-sig.log 2>&1 &
    SIG=$!
  fi
  for _ in $(seq 1 80); do
    nc -z 127.0.0.1 14502 2>/dev/null && nc -z 127.0.0.1 14503 2>/dev/null && break
    sleep 0.2
  done
  sleep 0.3
  SIGNAL=ws://127.0.0.1:14503 BIN="$TARGET_DIR/aerodesk-cli" BITRATE=2000000 NOISY=1 \
    ./scripts/loadtest.sh 1 1 18 1280 720 30 >/tmp/q-load.log 2>&1 &
  LOAD=$!
  # 等连接 + 心跳聚合（PeerStats 1s × 心跳 5s）
  sleep 12
  BODY=$(curl -s --max-time 2 "http://127.0.0.1:14502/metrics/prometheus")
  QC=$(echo "$BODY" | awk '/^aerodesk_sfu_qos_clients [0-9]+$/{v=$2} END{print v+0}')
  RTT=$(echo "$BODY" | awk '/^aerodesk_sfu_rtt_us [0-9]+$/{v=$2} END{print v+0}')
  EL=$(echo "$BODY" | awk '/^aerodesk_sfu_egress_loss [0-9.]+$/{v=$2} END{print v+0}')
  IL=$(echo "$BODY" | awk '/^aerodesk_sfu_ingress_loss [0-9.]+$/{v=$2} END{print v+0}')
  BW=$(echo "$BODY" | awk '/^aerodesk_sfu_bwe_tx_bps [0-9]+$/{v=$2} END{print v+0}')
  ERR=$(grep -ciE "panic|abort" /tmp/q-sfu.log /tmp/load-pub-1-1.log /tmp/load-view-1-1.log 2>/dev/null | awk '{s+=$1} END{print s+0}')
  wait "$LOAD" 2>/dev/null || true
  if [ "${QC:-0}" -gt 0 ] && [ "$EL" = "0" ] && [ "$IL" = "0" ] && [ "$ERR" = "0" ]; then
    echo "$MODE     ${QC}           ${RTT}      ${EL}            ${IL}             ${BW}        $ERR"
  else
    echo "$MODE     ${QC}           ${RTT}      ${EL}            ${IL}             ${BW}        $ERR"
    fail "质量指标异常（qos_clients=${QC} loss=${EL}/${IL} errors=${ERR}）"
  fi
  kill "$SFU" "$SIG" 2>/dev/null || true
  wait 2>/dev/null || true
done
echo "== SFU 媒体质量指标检查 PASS（${MODES[*]}：qos_clients>0、loss=0、0 错误）=="
