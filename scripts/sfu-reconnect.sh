#!/usr/bin/env bash
# #222 SFU 重连韧性压测：客户端断连循环（直连 + TURN 中继），验证 SFU 会话清理
# 与 TURN allocation 回收（#220 指标）——每轮 SIGKILL 全部客户端（模拟闪断），
# 断言 clients 归零、turn_allocations 归零（TURN 变体用 TURN_LIFETIME_SEC=60 加速
# 过期清扫，默认 120s 等待），全部轮次后无泄漏、无 panic。
# 用法: scripts/sfu-reconnect.sh [cycles] [rooms] [pairs] [turn_relay] [settle_s]
#   默认: 3 1 1 0 120
set -uo pipefail
cd "$(dirname "$0")/.."
export RUST_LOG="${RUST_LOG:-info}"
TARGET_DIR="${CARGO_TARGET_DIR:-$PWD/target}/debug"

CYCLES="${1:-3}"; ROOMS="${2:-1}"; PAIRS="${3:-1}"
TURN_RELAY="${4:-0}"; SETTLE_S="${5:-120}"
TURN_PORT="${TURN_PORT:-14789}"
TURN_SECRET="${TURN_SECRET:-testsecret}"
TURN_LIFETIME_SEC="${TURN_LIFETIME_SEC:-60}"
EXPECTED_CONN=$((ROOMS * PAIRS))
EXPECTED_CLIENTS=$((ROOMS * PAIRS * 2))

fail() { echo "FAIL: $*"; exit 1; }

echo "== 构建"
cargo build -q -p aerodesk-sfu -p aerodesk-signal -p aerodesk-agent
REC="$(mktemp -d)"

if [ "$TURN_RELAY" = "1" ]; then
  export AERODESK_FORCE_RELAY
  AERODESK_FORCE_RELAY=1
  echo "== 启动 SFU + signal（TURN relay：SFU_TURN_PORT=${TURN_PORT}，TURN_LIFETIME_SEC=${TURN_LIFETIME_SEC}）"
  RECORD_DIR="$REC" SFU_MEDIA_PORT=14578 SFU_SIGNAL_PORT=14500 SFU_INTERNAL_PORT=14502 \
    TURN_SECRET="$TURN_SECRET" SFU_TURN_PORT="$TURN_PORT" TURN_LIFETIME_SEC="$TURN_LIFETIME_SEC" \
    "$TARGET_DIR/aerodesk-sfu" >/tmp/reconnect-sfu.log 2>&1 &
  SFU=$!
  SIGNAL_PORT=14501 SIGNAL_PLAIN_PORT=14503 SFU_URL=http://127.0.0.1:14502 \
    TURN_SECRET="$TURN_SECRET" TURN_URLS="turn:127.0.0.1:${TURN_PORT}?transport=udp" \
    "$TARGET_DIR/aerodesk-signal" >/tmp/reconnect-sig.log 2>&1 &
  SIG=$!
else
  export AERODESK_FORCE_RELAY
  AERODESK_FORCE_RELAY=0
  echo "== 启动 SFU + signal（直连模式）"
  RECORD_DIR="$REC" SFU_MEDIA_PORT=14578 SFU_SIGNAL_PORT=14500 SFU_INTERNAL_PORT=14502 \
    "$TARGET_DIR/aerodesk-sfu" >/tmp/reconnect-sfu.log 2>&1 &
  SFU=$!
  SIGNAL_PORT=14501 SIGNAL_PLAIN_PORT=14503 SFU_URL=http://127.0.0.1:14502 \
    "$TARGET_DIR/aerodesk-signal" >/tmp/reconnect-sig.log 2>&1 &
  SIG=$!
fi
for _ in $(seq 1 80); do
  nc -z 127.0.0.1 14502 2>/dev/null && grep -q "SIP/UDP 监听已起" /tmp/reconnect-sig.log 2>/dev/null && break
  sleep 0.2
done
sleep 0.3

metrics() { # 输出: clients turn_alloc turn_alloc_total
  local body
  body=$(curl -s --max-time 2 "http://127.0.0.1:14502/metrics/prometheus" 2>/dev/null) || body=""
  CLI=$(echo "$body" | awk '/^aerodesk_sfu_clients [0-9]+$/{v=$2} END{print v+0}')
  TALLOC=$(echo "$body" | awk '/^aerodesk_sfu_turn_allocations [0-9]+$/{v=$2} END{print v+0}')
  TALLOC_TOTAL=$(echo "$body" | awk '/^aerodesk_sfu_turn_allocations_total [0-9]+$/{v=$2} END{print v+0}')
}

TOTAL_ALLOC_EXPECT=0
for c in $(seq 1 "$CYCLES"); do
  echo "== 轮次 $c/${CYCLES}：启动 ${ROOMS}×${PAIRS} 对客户端"
  rm -f /tmp/load-pub-*.log /tmp/load-view-*.log 2>/dev/null || true
  pids=()
  for r in $(seq 1 "$ROOMS"); do
    for p in $(seq 1 "$PAIRS"); do
      room="rc-r${r}"
      "$TARGET_DIR/aerodesk-agent" --role publisher --signal ws://127.0.0.1:14503 --room "$room" \
        --encoder vt --width 1280 --height 720 --fps 30 --bitrate 2000000 --noisy \
        >"/tmp/load-pub-${r}-${p}.log" 2>&1 &
      pids+=($!)
      "$TARGET_DIR/aerodesk-agent" --role viewer --signal ws://127.0.0.1:14503 --room "$room" \
        >"/tmp/load-view-${r}-${p}.log" 2>&1 &
      pids+=($!)
    done
  done
  ok=0
  for _ in $(seq 1 120); do
    PUB=$(grep -l "ICE connected" /tmp/load-pub-*.log 2>/dev/null | wc -l | tr -d ' ')
    VIEW=$(grep -l "ICE connected" /tmp/load-view-*.log 2>/dev/null | wc -l | tr -d ' ')
    if [ "$PUB" -ge "$EXPECTED_CONN" ] && [ "$VIEW" -ge "$EXPECTED_CONN" ]; then ok=1; break; fi
    sleep 0.5
  done
  [ "$ok" = "1" ] || fail "轮次 ${c} 客户端连接超时 (pub=${PUB:-0}/${EXPECTED_CONN} view=${VIEW:-0}/${EXPECTED_CONN})"
  # clients 指标由分片 5s 心跳更新（负载路由），连接后轮询等待达预期（≤10s）。
  ok=0; CLI=0
  for _ in $(seq 1 20); do
    metrics
    if [ "$CLI" -ge "$EXPECTED_CLIENTS" ]; then ok=1; break; fi
    sleep 0.5
  done
  [ "$ok" = "1" ] || fail "轮次 ${c} clients 未达预期（${CLI}/${EXPECTED_CLIENTS}，10s 内）"
  if [ "$TURN_RELAY" = "1" ]; then
    [ "$TALLOC" -ge "$EXPECTED_CLIENTS" ] || fail "轮次 ${c} turn_allocations=${TALLOC} 预期 >= ${EXPECTED_CLIENTS}"
    TOTAL_ALLOC_EXPECT=$((TOTAL_ALLOC_EXPECT + EXPECTED_CLIENTS))
  fi
  echo "  connected: clients=${CLI} turn_alloc=${TALLOC}/${TALLOC_TOTAL}"

  echo "  SIGKILL 全部客户端（模拟闪断）"
  kill -9 "${pids[@]}" 2>/dev/null || true
  sleep 0.5

  ok=0; CLI=99; TALLOC=99
  for _ in $(seq 1 "$((SETTLE_S * 2))"); do
    metrics
    if [ "$CLI" -eq 0 ] && { [ "$TURN_RELAY" != "1" ] || [ "$TALLOC" -eq 0 ]; }; then ok=1; break; fi
    sleep 0.5
  done
  [ "$ok" = "1" ] || fail "轮次 ${c} 清理超时(${SETTLE_S}s)：clients=${CLI} turn_alloc=${TALLOC}"
  grep -qiE "panic|abort" /tmp/reconnect-sfu.log && fail "轮次 ${c} SFU panic/abort"
  echo "  清理完成: clients=${CLI} turn_alloc=${TALLOC}"
  sleep 1
done

# 最终：无泄漏 + 计数一致
metrics
if [ "$TURN_RELAY" = "1" ]; then
  [ "$TALLOC" -eq 0 ] || fail "最终 turn_allocations=${TALLOC} 未归零（泄漏）"
  [ "$TALLOC_TOTAL" -eq "$TOTAL_ALLOC_EXPECT" ] || fail "最终 turn_allocations_total=${TALLOC_TOTAL} 预期 ${TOTAL_ALLOC_EXPECT}（异常 churn）"
fi
[ "$CLI" -eq 0 ] || fail "最终 clients=${CLI} 未归零"
grep -qiE "panic|abort" /tmp/reconnect-sfu.log && fail "SFU panic/abort"

kill "$SFU" "$SIG" 2>/dev/null || true
wait 2>/dev/null || true
echo "== 重连韧性 PASS（${CYCLES} 轮，直连/中继 turn_relay=${TURN_RELAY}，最终 clients=0 turn_alloc=${TALLOC}/${TALLOC_TOTAL}）=="
