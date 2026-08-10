#!/usr/bin/env bash
# #216 M7：桥接 TURN 中继路径验收（真实 NAT 就绪，#262）。
#
# 双 PoP（150xx/151xx 独立端口）各自启用内嵌 TURN（SFU_TURN_PORT 15079/15179），
# 全部客户端 + bridge 双腿走 AERODESK_FORCE_RELAY=1（force_relay_env）：
#   场景 0：PoP-A 直连基线（TURN relay）延迟 p50/p90/p99
#   场景 1：PoP-A publisher(--audio) → bridge（view PoP-A + publish PoP-B，双腿 relay）
#           → PoP-B viewer 解码跨 PoP 媒体
# 断言：viewer 无 Redirect、DECODED>0、AUDIO>0、LATENCY 样本、双 SFU turn_allocations>0、
#       桥 p99 ≤ 直连 p99×4+500ms。
set -uo pipefail
cd "$(dirname "$0")/.."
export RUST_LOG="${RUST_LOG:-info}"
TARGET_DIR="${CARGO_TARGET_DIR:-$PWD/target}/debug"

ROOM="bridge-turn-$(date +%s)"
AUTH="test-bridge-token"
TURN_SECRET="testsecret"
# PoP-A
SIG_A=15000; INT_A=15002; PLAIN_A=15003; MEDIA_A=15078; TURN_A=15079
# PoP-B
SIG_B=15100; INT_B=15102; PLAIN_B=15103; MEDIA_B=15178; TURN_B=15179
SIG_A_URL="ws://127.0.0.1:${PLAIN_A}"; SIG_B_URL="ws://127.0.0.1:${PLAIN_B}"
BRIDGE_CMD="$TARGET_DIR/aerodesk-bridge --remote-signal ${SIG_A_URL} --local-signal ${SIG_B_URL} --room {room} --auth-token \"\$BRIDGE_AUTH_TOKEN\" --codec h264"

fail() { echo "FAIL: $*"; exit 1; }
cleanup() {
  pkill -f 'aerodesk-bridge' 2>/dev/null || true
  pkill -f 'aerodesk-cli' 2>/dev/null || true
  [ -n "${SFU_A:-}" ] && { kill "$SFU_A" 2>/dev/null || true; wait "$SFU_A" 2>/dev/null || true; }
  [ -n "${SFU_B:-}" ] && { kill "$SFU_B" 2>/dev/null || true; wait "$SFU_B" 2>/dev/null || true; }
  [ -n "${SIG_A_PID:-}" ] && { kill "$SIG_A_PID" 2>/dev/null || true; wait "$SIG_A_PID" 2>/dev/null || true; }
  [ -n "${SIG_B_PID:-}" ] && { kill "$SIG_B_PID" 2>/dev/null || true; wait "$SIG_B_PID" 2>/dev/null || true; }
}
trap cleanup EXIT

latency_stats() {
  python3 - "$1" <<'PY'
import re, sys
s = open(sys.argv[1]).read()
vals = sorted(int(m) for m in re.findall(r'LATENCY: (\d+) ms', s))
if not vals:
    print("NONE NONE NONE"); raise SystemExit(0)
def pct(p):
    return vals[min(len(vals)-1, int(len(vals)*p))]
print(pct(0.50), pct(0.90), pct(0.99))
PY
}
latency_count() { grep -c "LATENCY:" "$1" 2>/dev/null || echo 0; }
wait_decoded() {
  for _ in $(seq 1 240); do
    grep -qE "DECODED: [1-9]" "$1" 2>/dev/null && return 0
    sleep 0.5
  done
  return 1
}

echo "== 构建"
cargo build -q -p aerodesk-sfu -p aerodesk-signal -p aerodesk-cli -p aerodesk-bridge
REC_A="$(mktemp -d)"; REC_B="$(mktemp -d)"

# 所有客户端 + signal（桥子进程继承）都强制 TURN relay。
export AERODESK_FORCE_RELAY=1

echo "== 启动 PoP-A（150xx，TURN ${TURN_A}）"
RECORD_DIR="$REC_A" SFU_MEDIA_PORT="$MEDIA_A" SFU_SIGNAL_PORT="$SIG_A" SFU_INTERNAL_PORT="$INT_A" \
  TURN_SECRET="$TURN_SECRET" SFU_TURN_PORT="$TURN_A" \
  "$TARGET_DIR/aerodesk-sfu" >/tmp/btr-sfu-a.log 2>&1 &
SFU_A=$!
POP_ID=pop-a AUTH_TOKENS="$AUTH" SIGNAL_PORT=15001 SIGNAL_PLAIN_PORT="$PLAIN_A" SFU_URL="http://127.0.0.1:${INT_A}" \
  TURN_SECRET="$TURN_SECRET" TURN_URLS="turn:127.0.0.1:${TURN_A}?transport=udp" \
  "$TARGET_DIR/aerodesk-signal" >/tmp/btr-sig-a.log 2>&1 &
SIG_A_PID=$!
for _ in $(seq 1 80); do nc -z 127.0.0.1 "$PLAIN_A" 2>/dev/null && nc -z 127.0.0.1 "$TURN_A" 2>/dev/null && break; sleep 0.2; done
sleep 0.3

echo "== 场景 0：PoP-A 直连基线（TURN relay）延迟"
"$TARGET_DIR/aerodesk-cli" --role publisher --signal "$SIG_A_URL" --room "$ROOM" --token "$AUTH" \
  --encoder vt --width 1280 --height 720 --fps 30 --bitrate 2000000 --noisy \
  >/tmp/btr-direct-pub.log 2>&1 &
PUB0=$!
ok=0
for _ in $(seq 1 120); do grep -q "ICE connected" /tmp/btr-direct-pub.log 2>/dev/null && ok=1 && break; sleep 0.5; done
[ "$ok" = "1" ] || fail "场景0：publisher 未连上（TURN relay）"
"$TARGET_DIR/aerodesk-cli" --role viewer --signal "$SIG_A_URL" --room "$ROOM" --token "$AUTH" \
  >/tmp/btr-direct-view.log 2>&1 &
VIEW0=$!
wait_decoded /tmp/btr-direct-view.log || fail "场景0：直连 viewer 未解码（TURN relay）"
for _ in $(seq 1 160); do
  [ "$(latency_count /tmp/btr-direct-view.log)" -ge 15 ] && break
  sleep 0.5
done
DIRECT_STATS=$(latency_stats /tmp/btr-direct-view.log)
DIRECT_P99=$(echo "$DIRECT_STATS" | awk '{print $3}')
DIRECT_N=$(latency_count /tmp/btr-direct-view.log)
echo "  直连基线（TURN）：samples=${DIRECT_N} p50/p90/p99=${DIRECT_STATS}ms"
[ "$DIRECT_P99" != "NONE" ] || fail "场景0：无 LATENCY 样本"
kill "$VIEW0" "$PUB0" 2>/dev/null || true; sleep 1

echo "== 启动 PoP-B（151xx，TURN ${TURN_B}，BRIDGE_CMD 桥优先）"
RECORD_DIR="$REC_B" SFU_MEDIA_PORT="$MEDIA_B" SFU_SIGNAL_PORT="$SIG_B" SFU_INTERNAL_PORT="$INT_B" \
  TURN_SECRET="$TURN_SECRET" SFU_TURN_PORT="$TURN_B" \
  "$TARGET_DIR/aerodesk-sfu" >/tmp/btr-sfu-b.log 2>&1 &
SFU_B=$!
POP_ID=pop-b AUTH_TOKENS="$AUTH" ROOM_POP_MAP="bridge-=pop-a" POP_URLS="pop-a=${SIG_A_URL}" \
  BRIDGE_CMD="$BRIDGE_CMD" BRIDGE_READY_TIMEOUT_SECS=20 BRIDGE_AUTH_TOKEN="$AUTH" \
  TURN_SECRET="$TURN_SECRET" TURN_URLS="turn:127.0.0.1:${TURN_B}?transport=udp" \
  SIGNAL_PORT=15101 SIGNAL_PLAIN_PORT="$PLAIN_B" SFU_URL="http://127.0.0.1:${INT_B}" \
  "$TARGET_DIR/aerodesk-signal" >/tmp/btr-sig-b.log 2>&1 &
SIG_B_PID=$!
for _ in $(seq 1 80); do nc -z 127.0.0.1 "$PLAIN_B" 2>/dev/null && nc -z 127.0.0.1 "$TURN_B" 2>/dev/null && break; sleep 0.2; done
sleep 0.3
grep -q "bridge orchestration enabled" /tmp/btr-sig-b.log || fail "PoP-B 未启用桥编排"

echo "== 场景 1：PoP-A publisher(--audio) + bridge（双腿 TURN relay）→ PoP-B viewer"
"$TARGET_DIR/aerodesk-cli" --role publisher --signal "$SIG_A_URL" --room "$ROOM" --token "$AUTH" \
  --encoder vt --width 1280 --height 720 --fps 30 --bitrate 2000000 --noisy --audio \
  >/tmp/btr-pub-a.log 2>&1 &
PUB_A=$!
ok=0
for _ in $(seq 1 120); do grep -q "ICE connected" /tmp/btr-pub-a.log 2>/dev/null && ok=1 && break; sleep 0.5; done
[ "$ok" = "1" ] || fail "场景1：PoP-A publisher 未连上（TURN relay）"

"$TARGET_DIR/aerodesk-cli" --role viewer --signal "$SIG_B_URL" --room "$ROOM" --token "$AUTH" \
  >/tmp/btr-view-b.log 2>&1 &
VIEW_B=$!
wait_decoded /tmp/btr-view-b.log || fail "场景1：PoP-B viewer 未解码跨 PoP 媒体（TURN relay，见 /tmp/btr-view-b.log）"
grep -q "signal redirect" /tmp/btr-view-b.log && fail "场景1：viewer 不应收到 Redirect"
ok=0
for _ in $(seq 1 60); do
  grep -qE "AUDIO: [1-9]" /tmp/btr-view-b.log 2>/dev/null && ok=1 && break
  sleep 0.5
done
[ "$ok" = "1" ] || fail "场景1：viewer 未收到跨 PoP 音频（TURN relay）"
echo "  场景1 PASS：viewer 经桥解码（无 Redirect，AUDIO>0）"
DECODED=$(grep -oE "DECODED: [0-9]+" /tmp/btr-view-b.log | tail -1 | cut -d' ' -f2)
echo "  viewer DECODED=${DECODED}"

echo "== 桥延迟（TURN relay）"
for _ in $(seq 1 160); do
  [ "$(latency_count /tmp/btr-view-b.log)" -ge 15 ] && break
  sleep 0.5
done
BRIDGE_STATS=$(latency_stats /tmp/btr-view-b.log)
BRIDGE_P99=$(echo "$BRIDGE_STATS" | awk '{print $3}')
BRIDGE_N=$(latency_count /tmp/btr-view-b.log)
echo "  桥路径（TURN）：samples=${BRIDGE_N} p50/p90/p99=${BRIDGE_STATS}ms（直连基线 ${DIRECT_STATS}ms）"
[ "$BRIDGE_P99" != "NONE" ] || fail "桥路径无 LATENCY 样本"
THRESHOLD=$((DIRECT_P99 * 4 + 500))
[ "$BRIDGE_P99" -lt "$THRESHOLD" ] || fail "桥延迟 p99=${BRIDGE_P99}ms ≥ 阈值 ${THRESHOLD}ms"

echo "== 双 SFU TURN allocation 断言"
ALLOC_A=$(curl -s --max-time 2 "http://127.0.0.1:${INT_A}/metrics/prometheus" | awk '/^aerodesk_sfu_turn_allocations [0-9]+$/{v=$2} END{print v+0}')
ALLOC_B=$(curl -s --max-time 2 "http://127.0.0.1:${INT_B}/metrics/prometheus" | awk '/^aerodesk_sfu_turn_allocations [0-9]+$/{v=$2} END{print v+0}')
echo "  PoP-A turn_allocations=${ALLOC_A} PoP-B turn_allocations=${ALLOC_B}"
[ "${ALLOC_A:-0}" -gt 0 ] && [ "${ALLOC_B:-0}" -gt 0 ] || fail "TURN allocation 未生效（A=${ALLOC_A} B=${ALLOC_B}）"

grep -qiE "panic|abort" /tmp/btr-*.log && fail "发现 panic/abort"
kill "$VIEW_B" "$PUB_A" 2>/dev/null || true
echo "== #216 M7 桥接 TURN 中继验收 PASS（直连 p50/p90/p99=${DIRECT_STATS}ms 桥=${BRIDGE_STATS}ms；allocations A=${ALLOC_A} B=${ALLOC_B}）=="
