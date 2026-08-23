#!/usr/bin/env bash
# AeroDesk 压测报告生成器：起 sfu/signal → 跑 loadtest → 采样 /metrics 与进程
# CPU/RSS → 输出 JSON + Markdown 报告（#8）。
#
# 用法:
#   scripts/bench.sh [rooms] [pairs] [seconds] [width] [height] [fps]
#   BITRATE=12000000 scripts/bench.sh 1 1 30 3840 2160 60
set -euo pipefail

ROOT="$(cd "$(dirname "$0")/.." && pwd)"
cd "$ROOT"

ROOMS="${1:-1}"
PAIRS="${2:-1}"
DURATION="${3:-30}"  # 注意：不要用 SECONDS（bash 特殊变量，自动递增）
W="${4:-3840}"
H="${5:-2160}"
FPS="${6:-60}"
BITRATE="${BITRATE:-10000000}"
REPORT_DIR="${REPORT_DIR:-/tmp/aerodesk-bench}"
mkdir -p "$REPORT_DIR"

echo "== 构建"
cargo build -q --release -p aerodesk-sfu -p aerodesk-signal -p aerodesk-agent

REC="$(mktemp -d /tmp/bench-rec.XXXX)"
echo "== 启动 sfu/signal"
RECORD_DIR="$REC" ./target/release/aerodesk-sfu >"$REPORT_DIR/sfu.log" 2>&1 &
SFU_PID=$!
SIP_UDP_PORT=5060 ./target/release/aerodesk-signal >"$REPORT_DIR/signal.log" 2>&1 &
SIG_PID=$!
for _ in $(seq 1 50); do
  if nc -z 127.0.0.1 3003 2>/dev/null; then break; fi
  if ! kill -0 "$SFU_PID" 2>/dev/null; then echo "sfu 启动失败"; cat "$REPORT_DIR/sfu.log"; exit 1; fi
  sleep 0.2
done

# ---- 采样循环：/metrics + 进程 CPU/RSS ----
METRICS_CSV="$REPORT_DIR/metrics.csv"
PROC_CSV="$REPORT_DIR/proc.csv"
echo "ts,rx_bytes,tx_bytes,rx_packets,tx_packets,clients" > "$METRICS_CSV"
echo "ts,sfu_cpu,sfu_rss,sig_cpu,sig_rss" > "$PROC_CSV"
sampler() {
  local start; start=$(date +%s)
  while kill -0 "$SFU_PID" 2>/dev/null && [ $(( $(date +%s) - start )) -le "$DURATION" ]; do
    local ts; ts=$(date +%s)
    local m; m=$(curl -s "http://127.0.0.1:3002/metrics" 2>/dev/null || true)
    if [ -n "$m" ]; then
      # /metrics 按分片返回，汇总所有分片。
      local agg
      agg=$(echo "$m" | python3 -c "
import json,sys
try:
    m=json.load(sys.stdin)
except Exception:
    print('0 0 0 0 0'); raise SystemExit
sh=m.get('shards',[])
print(sum(x.get('rx_bytes',0) for x in sh), sum(x.get('tx_bytes',0) for x in sh),
      sum(x.get('rx_packets',0) for x in sh), sum(x.get('tx_packets',0) for x in sh),
      sum(x.get('clients',0) for x in sh))
" 2>/dev/null || echo "0 0 0 0 0")
      echo "$ts,$(echo $agg | tr ' ' ',')" >> "$METRICS_CSV"
    fi
    local sc sr gc gr
    sc=$(ps -o %cpu= -p "$SFU_PID" 2>/dev/null | tr -d ' ' || echo 0)
    sr=$(ps -o rss= -p "$SFU_PID" 2>/dev/null | tr -d ' ' || echo 0)
    gc=$(ps -o %cpu= -p "$SIG_PID" 2>/dev/null | tr -d ' ' || echo 0)
    gr=$(ps -o rss= -p "$SIG_PID" 2>/dev/null | tr -d ' ' || echo 0)
    echo "$ts,$sc,$sr,$gc,$gr" >> "$PROC_CSV"
    sleep 1
  done
}
sampler &
SAMPLER_PID=$!

echo "== 压测: ${ROOMS} 房间 × ${PAIRS} 对 @ ${W}x${H}/${FPS}fps ${BITRATE}bps，${DURATION}s"
BIN="$ROOT/target/release/aerodesk-agent" SIGNAL="ws://127.0.0.1:3003" \
  bash "$ROOT/scripts/loadtest.sh" "$ROOMS" "$PAIRS" "$DURATION" "$W" "$H" "$FPS" \
  > "$REPORT_DIR/loadtest.log" 2>&1 || true

wait "$SAMPLER_PID" 2>/dev/null || true
kill "$SFU_PID" "$SIG_PID" 2>/dev/null || true
wait 2>/dev/null || true

echo "== 汇总"
python3 "$ROOT/scripts/bench_report.py" "$REPORT_DIR" "$ROOMS" "$PAIRS" "$DURATION" "$W" "$H" "$FPS" "$BITRATE"
echo "报告目录: $REPORT_DIR"
