#!/usr/bin/env bash
# #215 SFU 容量压测基准（#8 方法论）：
#   起 SFU+signal（独立端口）→ N 房间 × M 对施压 → 采样 /metrics/prometheus →
#   输出 吞吐(MB/s)/pps/峰值连接/媒体帧到达/ICE 成功率/错误。
# 用法: scripts/sfu-capacity-bench.sh [rooms] [pairs] [seconds] [width] [height] [fps] [bitrate]
#   默认: 2 2 20 1280 720 30 2000000
set -uo pipefail
cd "$(dirname "$0")/.."
export RUST_LOG="${RUST_LOG:-info}"
TARGET_DIR="${CARGO_TARGET_DIR:-$PWD/target}/debug"

ROOMS="${1:-2}"; PAIRS="${2:-2}"; SECONDS="${3:-20}"
W="${4:-1280}"; H="${5:-720}"; FPS="${6:-30}"; BITRATE="${7:-2000000}"

echo "== 构建"
cargo build -q -p aerodesk-sfu -p aerodesk-signal -p aerodesk-cli
REC="$(mktemp -d)"
echo "== 启动 SFU + signal（独立端口 14500-14503）"
RECORD_DIR="$REC" SFU_MEDIA_PORT=14578 SFU_SIGNAL_PORT=14500 SFU_INTERNAL_PORT=14502 \
  "$TARGET_DIR/aerodesk-sfu" >/tmp/cap-sfu.log 2>&1 &
SFU=$!
SIGNAL_PORT=14501 SIGNAL_PLAIN_PORT=14503 SFU_URL=http://127.0.0.1:14502 \
  "$TARGET_DIR/aerodesk-signal" >/tmp/cap-sig.log 2>&1 &
SIG=$!
for _ in $(seq 1 80); do
  nc -z 127.0.0.1 14502 2>/dev/null && nc -z 127.0.0.1 14503 2>/dev/null && break
  sleep 0.2
done
sleep 0.3

# metrics 采样（后台）：每 2s 记录 totals + 峰值 clients
M_FILE=/tmp/cap-metrics.tsv
: > "$M_FILE"
(
  while true; do
    body=$(curl -s --max-time 2 "http://127.0.0.1:14502/metrics/prometheus" 2>/dev/null) || body=""
    cli=$(echo "$body" | awk '/^aerodesk_sfu_clients [0-9]+$/{v=$2} END{print v+0}')
    rxb=$(echo "$body" | awk '/^aerodesk_sfu_rx_bytes_total [0-9]+$/{v=$2} END{print v+0}')
    txb=$(echo "$body" | awk '/^aerodesk_sfu_tx_bytes_total [0-9]+$/{v=$2} END{print v+0}')
    rxp=$(echo "$body" | awk '/^aerodesk_sfu_rx_packets_total [0-9]+$/{v=$2} END{print v+0}')
    txp=$(echo "$body" | awk '/^aerodesk_sfu_tx_packets_total [0-9]+$/{v=$2} END{print v+0}')
    echo -e "$(date +%s)\t$cli\t$rxb\t$txb\t$rxp\t$txp" >> "$M_FILE"
    sleep 2
  done
) &
SAMPLER=$!

echo "== 施压: ${ROOMS} 房间 × ${PAIRS} 对 @ ${W}x${H}/${FPS}fps ${BITRATE}bps，时长 ${SECONDS}s"
# 清理旧压测日志
rm -f /tmp/load-pub-*.log /tmp/load-view-*.log 2>/dev/null || true
SIGNAL=ws://127.0.0.1:14503 BIN="$TARGET_DIR/aerodesk-cli" BITRATE="$BITRATE" NOISY=1 \
  ./scripts/loadtest.sh "$ROOMS" "$PAIRS" "$SECONDS" "$W" "$H" "$FPS" || true
kill "$SAMPLER" 2>/dev/null || true

echo "== 汇总"
# 采样区间（取第一个与最后一个有效样本）
FIRST=$(head -2 "$M_FILE" | tail -1); LAST=$(tail -1 "$M_FILE")
IFS=$'\t' read -r t0 c0 rxb0 txb0 rxp0 txp0 <<< "$FIRST"
IFS=$'\t' read -r t1 c1 rxb1 txb1 rxp1 txp1 <<< "$LAST"
DUR=$((t1-t0)); [ "$DUR" -le 0 ] && DUR=1
RXMB=$(python3 -c "print(f'{($rxb1-$rxb0)/1024/1024:.1f}')")
TXMB=$(python3 -c "print(f'{($txb1-$txb0)/1024/1024:.1f}')")
RXMBPS=$(python3 -c "print(f'{($rxb1-$rxb0)/1024/1024/$DUR:.2f}')")
TXMBPS=$(python3 -c "print(f'{($txb1-$txb0)/1024/1024/$DUR:.2f}')")
RXPS=$(python3 -c "print(f'{($rxp1-$rxp0)/$DUR:.0f}')")
TXPS=$(python3 -c "print(f'{($txp1-$txp0)/$DUR:.0f}')")
PEAK=$(awk -F'\t' 'BEGIN{m=0} $2>m{m=$2} END{print m}' "$M_FILE")

PUB_ICE=$(grep -l "ICE connected" /tmp/load-pub-*.log 2>/dev/null | wc -l | tr -d ' ')
VIEW_ICE=$(grep -l "ICE connected" /tmp/load-view-*.log 2>/dev/null | wc -l | tr -d ' ')
VIEW_FRAMES=$(grep -h "RECEIVED:" /tmp/load-view-*.log 2>/dev/null | sed -E 's/.*RECEIVED: ([0-9]+) frames.*/\1/' | paste -sd+ - | bc 2>/dev/null || echo 0)
ERRORS=$(grep -hiE "panic|abort|auth failed" /tmp/cap-sfu.log /tmp/load-pub-*.log /tmp/load-view-*.log 2>/dev/null | wc -l | tr -d ' ')

echo "配置: ${ROOMS}×${PAIRS} @ ${W}x${H}/${FPS} ${BITRATE}bps ${SECONDS}s"
echo "峰值并发 clients: $PEAK"
echo "连接成功: publisher=$PUB_ICE viewer=$VIEW_ICE (目标 $((ROOMS*PAIRS)))"
echo "viewer 媒体帧合计: $VIEW_FRAMES"
echo "吞吐: rx=${RXMB}MB(${RXMBPS}MB/s) tx=${TXMB}MB(${TXMBPS}MB/s)"
echo "包速率: rx=${RXPS}pps tx=${TXPS}pps"
echo "错误/panic: $ERRORS"
echo "--- 分片负载（最终采样）---"
curl -s --max-time 2 "http://127.0.0.1:14502/metrics/prometheus" 2>/dev/null | grep -E '^aerodesk_sfu_(clients|rx_bytes_total|tx_bytes_total)\{shard=' || true

kill "$SFU" "$SIG" 2>/dev/null || true
wait 2>/dev/null || true
echo "== 完成 =="
