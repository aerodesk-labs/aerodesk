#!/usr/bin/env bash
# #220 SFU 长稳压测（#215/#218 容量基准延续）：
#   起 SFU+signal（独立端口）→ N 房间 × M 对持续施压 → 每 30s 采样
#   /metrics/prometheus（clients / rx·tx / turn_allocations）→ 看门狗断言：
#   连接保持 == 预期、viewer RECEIVED 帧单调递增（媒体持续送达）、
#   TURN allocation 稳定 == 预期（无泄漏/无过期）、无 disconnect/reconnect/panic。
#   TURN 变体建议 RUN_SECONDS>=660（allocation lifetime 600s，验证 Refresh 越过到期）。
# 用法: scripts/sfu-longrun.sh [rooms] [pairs] [seconds] [width] [height] [fps] [bitrate] [turn_relay]
#   默认: 1 1 600 1280 720 30 2000000
set -uo pipefail
cd "$(dirname "$0")/.."
export RUST_LOG="${RUST_LOG:-info}"
TARGET_DIR="${CARGO_TARGET_DIR:-$PWD/target}/debug"

ROOMS="${1:-1}"; PAIRS="${2:-1}"; RUN_SECONDS="${3:-600}"
W="${4:-1280}"; H="${5:-720}"; FPS="${6:-30}"; BITRATE="${7:-2000000}"
TURN_RELAY="${8:-0}"
TURN_PORT="${TURN_PORT:-14789}"
TURN_SECRET="${TURN_SECRET:-testsecret}"
SAMPLES_SEC=30
STALL_LIMIT_SEC=90
EXPECTED_CONN=$((ROOMS * PAIRS))
EXPECTED_CLIENTS=$((ROOMS * PAIRS * 2))

if [ "$TURN_RELAY" = "1" ] && [ "$RUN_SECONDS" -lt 660 ]; then
  echo "WARN: TURN allocation lifetime=600s，RUN_SECONDS=${RUN_SECONDS} < 660 无法验证 Refresh 越过到期；建议 >=660"
fi

echo "== 构建"
cargo build -q -p aerodesk-sfu -p aerodesk-signal -p aerodesk-agent
REC="$(mktemp -d)"

# 直连/TURN 环境与启动（与 sfu-capacity-bench.sh 一致；#218 已修 TURN_URLS 只给 signal）
if [ "$TURN_RELAY" = "1" ]; then
  export AERODESK_FORCE_RELAY
  AERODESK_FORCE_RELAY=1
  echo "== 启动 SFU + signal（TURN relay 模式：SFU_TURN_PORT=${TURN_PORT}，force-relay，${RUN_SECONDS}s）"
  RECORD_DIR="$REC" SFU_MEDIA_PORT=14578 SFU_SIGNAL_PORT=14500 SFU_INTERNAL_PORT=14502 \
    TURN_SECRET="$TURN_SECRET" SFU_TURN_PORT="$TURN_PORT" \
    "$TARGET_DIR/aerodesk-sfu" >/tmp/longrun-sfu.log 2>&1 &
  SFU=$!
  SIGNAL_PORT=14501 SIGNAL_PLAIN_PORT=14503 SFU_URL=http://127.0.0.1:14502 \
    TURN_SECRET="$TURN_SECRET" TURN_URLS="turn:127.0.0.1:${TURN_PORT}?transport=udp" \
    "$TARGET_DIR/aerodesk-signal" >/tmp/longrun-sig.log 2>&1 &
  SIG=$!
else
  export AERODESK_FORCE_RELAY
  AERODESK_FORCE_RELAY=0
  echo "== 启动 SFU + signal（直连模式，${RUN_SECONDS}s）"
  RECORD_DIR="$REC" SFU_MEDIA_PORT=14578 SFU_SIGNAL_PORT=14500 SFU_INTERNAL_PORT=14502 \
    "$TARGET_DIR/aerodesk-sfu" >/tmp/longrun-sfu.log 2>&1 &
  SFU=$!
  SIGNAL_PORT=14501 SIGNAL_PLAIN_PORT=14503 SFU_URL=http://127.0.0.1:14502 \
    "$TARGET_DIR/aerodesk-signal" >/tmp/longrun-sig.log 2>&1 &
  SIG=$!
fi
for _ in $(seq 1 80); do
  nc -z 127.0.0.1 14502 2>/dev/null && nc -z 127.0.0.1 14503 2>/dev/null && break
  sleep 0.2
done
sleep 0.3

# 采样 + 看门狗（后台）：每 SAMPLES_SEC 秒记录一行 TSV 并做健康断言。
# 列: ts clients rx_bytes tx_bytes rx_packets tx_packets turn_alloc turn_alloc_total frames
S_FILE=/tmp/longrun-samples.tsv
: > "$S_FILE"
(
  start=$(date +%s)
  last_frames=-1
  last_growth=$(date +%s)
  alloc_ok_since=0
  while true; do
    body=$(curl -s --max-time 2 "http://127.0.0.1:14502/metrics/prometheus" 2>/dev/null) || body=""
    cli=$(echo "$body" | awk '/^aerodesk_sfu_clients [0-9]+$/{v=$2} END{print v+0}')
    rxb=$(echo "$body" | awk '/^aerodesk_sfu_rx_bytes_total [0-9]+$/{v=$2} END{print v+0}')
    txb=$(echo "$body" | awk '/^aerodesk_sfu_tx_bytes_total [0-9]+$/{v=$2} END{print v+0}')
    rxp=$(echo "$body" | awk '/^aerodesk_sfu_rx_packets_total [0-9]+$/{v=$2} END{print v+0}')
    txp=$(echo "$body" | awk '/^aerodesk_sfu_tx_packets_total [0-9]+$/{v=$2} END{print v+0}')
    talloc=$(echo "$body" | awk '/^aerodesk_sfu_turn_allocations [0-9]+$/{v=$2} END{print v+0}')
    talloc_total=$(echo "$body" | awk '/^aerodesk_sfu_turn_allocations_total [0-9]+$/{v=$2} END{print v+0}')
    frames=$(grep -h "RECEIVED:" /tmp/load-view-*.log 2>/dev/null | sed -E 's/.*RECEIVED: ([0-9]+) frames.*/\1/' | tail -1)
    frames=${frames:-0}
    echo -e "$(date +%s)\t$cli\t$rxb\t$txb\t$rxp\t$txp\t$talloc\t$talloc_total\t$frames" >> "$S_FILE"

    # 连接数（ICE connected 日志行数 = 成功客户端数；重复打印按行计，>0 即已连）
    PUB_ICE=$(grep -l "ICE connected" /tmp/load-pub-*.log 2>/dev/null | wc -l | tr -d ' ')
    VIEW_ICE=$(grep -l "ICE connected" /tmp/load-view-*.log 2>/dev/null | wc -l | tr -d ' ')
    ERR=$(grep -hiE "panic|abort|ICE disconnected|reconnect" /tmp/longrun-sfu.log /tmp/load-pub-*.log /tmp/load-view-*.log 2>/dev/null | wc -l | tr -d ' ')
    if [ "$ERR" -gt 0 ]; then
      echo "WATCHDOG_FAIL error-markers=${ERR} (panic/abort/disconnect/reconnect)" >> /tmp/longrun-watchdog.log
      exit 1
    fi
    # 连接收敛：60s 内应全部连上，之后任一采样低于预期即失败
    if [ "$(( $(date +%s) - start ))" -gt 60 ]; then
      if [ "$PUB_ICE" -lt "$EXPECTED_CONN" ] || [ "$VIEW_ICE" -lt "$EXPECTED_CONN" ]; then
        echo "WATCHDOG_FAIL conn pub=${PUB_ICE}/${EXPECTED_CONN} view=${VIEW_ICE}/${EXPECTED_CONN}" >> /tmp/longrun-watchdog.log
        exit 1
      fi
    fi
    # 媒体持续送达：帧计数单调，90s 无增长判定卡死
    if [ "$frames" -gt 0 ]; then
      if [ "$frames" -gt "$last_frames" ]; then
        last_growth=$(date +%s)
      elif [ "$(( $(date +%s) - last_growth ))" -gt "$STALL_LIMIT_SEC" ]; then
        echo "WATCHDOG_FAIL frames-stall last=${last_frames} now=${frames}" >> /tmp/longrun-watchdog.log
        exit 1
      fi
      last_frames=$frames
    fi
    # TURN allocation：收敛后应 == 客户端数（force-relay 每客户端 1 allocation）；
    # 连续 2 采样低于预期 → 泄漏/过期。
    if [ "$TURN_RELAY" = "1" ] && [ "$talloc" -ge "$EXPECTED_CLIENTS" ]; then
      alloc_ok_since=$((alloc_ok_since + 1))
    elif [ "$TURN_RELAY" = "1" ] && [ "$alloc_ok_since" -gt 1 ] && [ "$talloc" -lt "$EXPECTED_CLIENTS" ]; then
      echo "WATCHDOG_FAIL turn-alloc drop=${talloc} expected=${EXPECTED_CLIENTS} total=${talloc_total}" >> /tmp/longrun-watchdog.log
      exit 1
    fi
    sleep "$SAMPLES_SEC"
  done
) &
WATCH=$!

echo "== 施压: ${ROOMS} 房间 × ${PAIRS} 对 @ ${W}x${H}/${FPS}fps ${BITRATE}bps，时长 ${RUN_SECONDS}s"
rm -f /tmp/load-pub-*.log /tmp/load-view-*.log /tmp/longrun-watchdog.log 2>/dev/null || true
SIGNAL=ws://127.0.0.1:14503 BIN="$TARGET_DIR/aerodesk-agent" BITRATE="$BITRATE" NOISY=1 \
  ./scripts/loadtest.sh "$ROOMS" "$PAIRS" "$RUN_SECONDS" "$W" "$H" "$FPS" || true

# 看门狗结果
if [ -s /tmp/longrun-watchdog.log ]; then
  echo "== 长稳失败 =="
  cat /tmp/longrun-watchdog.log
  kill "$WATCH" "$SFU" "$SIG" 2>/dev/null || true
  wait 2>/dev/null || true
  exit 1
fi
kill "$WATCH" "$SFU" "$SIG" 2>/dev/null || true
wait 2>/dev/null || true

echo "== 长稳汇总（${ROOMS}x${PAIRS}，${RUN_SECONDS}s，turn_relay=${TURN_RELAY}）=="
FIRST=$(head -2 "$S_FILE" | tail -1); LAST=$(tail -1 "$S_FILE")
IFS=$'\t' read -r t0 c0 rxb0 txb0 rxp0 txp0 ta0 tat0 f0 <<< "$FIRST"
IFS=$'\t' read -r t1 c1 rxb1 txb1 rxp1 txp1 ta1 tat1 f1 <<< "$LAST"
DUR=$((t1-t0)); [ "$DUR" -le 0 ] && DUR=1
echo "运行时长: ${DUR}s；峰值 clients: $(awk -F'\t' 'BEGIN{m=0} $2>m{m=$2} END{print m}' "$S_FILE")"
echo "连接保持: publisher=$(grep -l 'ICE connected' /tmp/load-pub-*.log 2>/dev/null | wc -l | tr -d ' ')/${EXPECTED_CONN} viewer=$(grep -l 'ICE connected' /tmp/load-view-*.log 2>/dev/null | wc -l | tr -d ' ')/${EXPECTED_CONN}"
echo "viewer 帧: 首=${f0} 末=${f1} 增量=$((f1-f0))（持续送达）"
echo "吞吐: rx=$(python3 -c "print(f'{($rxb1-$rxb0)/1024/1024/$DUR:.2f}')")MB/s tx=$(python3 -c "print(f'{($txb1-$txb0)/1024/1024/$DUR:.2f}')")MB/s"
if [ "$TURN_RELAY" = "1" ]; then
  echo "TURN allocation: 活跃=${ta1}（预期 ${EXPECTED_CLIENTS}）累计=${tat1} 稳定保持"
else
  echo "TURN allocation: 不适用（直连模式）"
fi
echo "错误/panic: $(grep -hiE 'panic|abort' /tmp/longrun-sfu.log /tmp/load-pub-*.log /tmp/load-view-*.log 2>/dev/null | wc -l | tr -d ' ')"
echo "== 长稳 PASS =="
