#!/usr/bin/env bash
# #216 M9：多房间桥并发 + BRIDGE_MAX_RUNNING 上限回退验收（#272）。
#
# 本地双 PoP（141xx/142xx 独立端口）：
#   场景 A（并发）：R1/R2 各一个 PoP-A publisher + PoP-B viewer → 两桥并发
#     （signal spawn 2 个、每房独立）；断言两 viewer 均解码、无 Redirect、
#     双 SFU clients 数正确。
#   场景 B（上限回退）：PoP-B 信令 BRIDGE_MAX_RUNNING=1 → R1 桥优先、R2 viewer
#     收到 Redirect 并跟随到 PoP-A 直连解码。
set -uo pipefail
cd "$(dirname "$0")/.."
export RUST_LOG="${RUST_LOG:-info}"
TARGET_DIR="${CARGO_TARGET_DIR:-$PWD/target}/debug"

ROOM1="mroom-$(date +%s)-r1"; ROOM2="mroom-$(date +%s)-r2"
AUTH="test-bridge-token"
# PoP-A
SIG_A=14100; INT_A=14102; PLAIN_A=14103; MEDIA_A=14178
# PoP-B
SIG_B=14200; INT_B=14202; PLAIN_B=14203; MEDIA_B=14278
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

wait_decoded() { # $1=logfile
  for _ in $(seq 1 240); do
    grep -qE "DECODED: [1-9]" "$1" 2>/dev/null && return 0
    sleep 0.5
  done
  return 1
}
wait_log() { # $1=logfile $2=pattern $3=iterations(默认120)
  local n="${3:-120}"
  for _ in $(seq 1 "$n"); do
    grep -q "$2" "$1" 2>/dev/null && return 0
    sleep 0.5
  done
  return 1
}
clients_of() { # $1=内部端口 -> 总客户端数
  curl -s --max-time 2 "http://127.0.0.1:$1/metrics/prometheus" | awk '/^aerodesk_sfu_clients [0-9]+$/{v=$2} END{print v+0}'
}

echo "== 构建"
cargo build -q -p aerodesk-sfu -p aerodesk-signal -p aerodesk-cli -p aerodesk-bridge || fail "构建失败"
REC_A="$(mktemp -d)"; REC_B="$(mktemp -d)"

echo "== 启动 PoP-A（141xx）+ PoP-B（142xx，BRIDGE_CMD 桥优先）"
RECORD_DIR="$REC_A" SFU_MEDIA_PORT="$MEDIA_A" SFU_SIGNAL_PORT="$SIG_A" SFU_INTERNAL_PORT="$INT_A" \
  "$TARGET_DIR/aerodesk-sfu" >/tmp/bmr-sfu-a.log 2>&1 &
SFU_A=$!
POP_ID=pop-a AUTH_TOKENS="$AUTH" SIGNAL_PORT=14101 SIGNAL_PLAIN_PORT="$PLAIN_A" SFU_URL="http://127.0.0.1:${INT_A}" \
  "$TARGET_DIR/aerodesk-signal" >/tmp/bmr-sig-a.log 2>&1 &
SIG_A_PID=$!
RECORD_DIR="$REC_B" SFU_MEDIA_PORT="$MEDIA_B" SFU_SIGNAL_PORT="$SIG_B" SFU_INTERNAL_PORT="$INT_B" \
  "$TARGET_DIR/aerodesk-sfu" >/tmp/bmr-sfu-b.log 2>&1 &
SFU_B=$!
POP_ID=pop-b AUTH_TOKENS="$AUTH" ROOM_POP_MAP="mroom-=pop-a" POP_URLS="pop-a=${SIG_A_URL}" \
  BRIDGE_CMD="$BRIDGE_CMD" BRIDGE_READY_TIMEOUT_SECS=20 BRIDGE_AUTH_TOKEN="$AUTH" \
  SIGNAL_PORT=14201 SIGNAL_PLAIN_PORT="$PLAIN_B" SFU_URL="http://127.0.0.1:${INT_B}" \
  "$TARGET_DIR/aerodesk-signal" >/tmp/bmr-sig-b.log 2>&1 &
SIG_B_PID=$!
for _ in $(seq 1 80); do
  nc -z 127.0.0.1 "$PLAIN_A" 2>/dev/null && nc -z 127.0.0.1 "$PLAIN_B" 2>/dev/null && break
  sleep 0.2
done
sleep 0.3

start_pub() { # $1=room $2=log
  "$TARGET_DIR/aerodesk-cli" --role publisher --signal "$SIG_A_URL" --room "$1" --token "$AUTH" \
    --encoder vt --width 1280 --height 720 --fps 30 --bitrate 2000000 --noisy \
    >"$2" 2>&1 &
  echo $!
}
start_view() { # $1=room $2=log
  "$TARGET_DIR/aerodesk-cli" --role viewer --signal "$SIG_B_URL" --room "$1" --token "$AUTH" \
    >"$2" 2>&1 &
  echo $!
}

echo "== 场景 A：双房间并发桥"
PUB1=$(start_pub "$ROOM1" /tmp/bmr-pub1.log)
PUB2=$(start_pub "$ROOM2" /tmp/bmr-pub2.log)
for _ in $(seq 1 120); do
  grep -q "ICE connected" /tmp/bmr-pub1.log 2>/dev/null && grep -q "ICE connected" /tmp/bmr-pub2.log 2>/dev/null && break
  sleep 0.5
done
grep -q "ICE connected" /tmp/bmr-pub1.log || fail "publisher R1 未连上"
grep -q "ICE connected" /tmp/bmr-pub2.log || fail "publisher R2 未连上"
VIEW1=$(start_view "$ROOM1" /tmp/bmr-view1.log)
VIEW2=$(start_view "$ROOM2" /tmp/bmr-view2.log)
wait_decoded /tmp/bmr-view1.log || fail "场景A：R1 viewer 未解码（见 /tmp/bmr-view1.log）"
wait_decoded /tmp/bmr-view2.log || fail "场景A：R2 viewer 未解码（见 /tmp/bmr-view2.log）"
grep -q "signal redirect" /tmp/bmr-view1.log && fail "场景A：R1 不应 Redirect"
grep -q "signal redirect" /tmp/bmr-view2.log && fail "场景A：R2 不应 Redirect"
# 每房独立 spawn：sig-b 里 R1、R2 各至少一次 spawn。
grep -q "spawned for room $ROOM1" /tmp/bmr-sig-b.log || fail "场景A：R1 未 spawn 桥"
grep -q "spawned for room $ROOM2" /tmp/bmr-sig-b.log || fail "场景A：R2 未 spawn 桥"
SPAWNS=$(grep -c "bridge: spawned" /tmp/bmr-sig-b.log 2>/dev/null || echo 0)
[ "${SPAWNS:-0}" -ge 2 ] || fail "场景A：桥 spawn=${SPAWNS} 应 ≥2"
# 双 SFU 客户端数：A=2 publisher + 2 bridge-viewer 腿 = 4；B=2 bridge-pub 腿 + 2 viewer = 4。
ok=0
for _ in $(seq 1 20); do
  CA=$(clients_of "$INT_A"); CB=$(clients_of "$INT_B")
  [ "${CA:-0}" -ge 4 ] && [ "${CB:-0}" -ge 4 ] && ok=1 && break
  sleep 0.5
done
[ "$ok" = "1" ] || fail "场景A：clients A=${CA} B=${CB}（应各 ≥4）"
echo "  场景A PASS：双房间并发桥 spawn=${SPAWNS}，两 viewer 各自解码（A=${CA} B=${CB}）"

kill "$VIEW1" "$VIEW2" "$PUB1" "$PUB2" 2>/dev/null || true
sleep 1
pkill -f 'aerodesk-bridge' 2>/dev/null || true
kill "$SIG_B_PID" 2>/dev/null || true; wait "$SIG_B_PID" 2>/dev/null || true
sleep 1

echo "== 场景 B：BRIDGE_MAX_RUNNING=1 → R2 回退 Redirect"
ROOM1="mroom-$(date +%s)-b1"; ROOM2="mroom-$(date +%s)-b2"
POP_ID=pop-b AUTH_TOKENS="$AUTH" ROOM_POP_MAP="mroom-=pop-a" POP_URLS="pop-a=${SIG_A_URL}" \
  BRIDGE_CMD="$BRIDGE_CMD" BRIDGE_READY_TIMEOUT_SECS=20 BRIDGE_AUTH_TOKEN="$AUTH" \
  BRIDGE_MAX_RUNNING=1 \
  SIGNAL_PORT=14201 SIGNAL_PLAIN_PORT="$PLAIN_B" SFU_URL="http://127.0.0.1:${INT_B}" \
  "$TARGET_DIR/aerodesk-signal" >/tmp/bmr-sig-b2.log 2>&1 &
SIG_B_PID=$!
for _ in $(seq 1 50); do nc -z 127.0.0.1 "$PLAIN_B" 2>/dev/null && break; sleep 0.2; done
sleep 0.3

PUB1=$(start_pub "$ROOM1" /tmp/bmr-pub1b.log)
PUB2=$(start_pub "$ROOM2" /tmp/bmr-pub2b.log)
for _ in $(seq 1 120); do
  grep -q "ICE connected" /tmp/bmr-pub1b.log 2>/dev/null && grep -q "ICE connected" /tmp/bmr-pub2b.log 2>/dev/null && break
  sleep 0.5
done
VIEW1=$(start_view "$ROOM1" /tmp/bmr-view1b.log)
wait_decoded /tmp/bmr-view1b.log || fail "场景B：R1（桥优先）未解码"
grep -q "signal redirect" /tmp/bmr-view1b.log && fail "场景B：R1 不应 Redirect"
# R2：上限=1 → Redirect 并跟随到 PoP-A 直连解码。
VIEW2=$(start_view "$ROOM2" /tmp/bmr-view2b.log)
wait_log /tmp/bmr-view2b.log "signal redirect" 240 || fail "场景B：R2 未收到 Redirect（上限未生效）"
wait_decoded /tmp/bmr-view2b.log || fail "场景B：R2 跟随 Redirect 后未解码"
# 只有 R1 的桥被 spawn；上限告警出现。
grep -q "spawned for room $ROOM1" /tmp/bmr-sig-b2.log || fail "场景B：R1 未 spawn 桥"
grep -q "spawned for room $ROOM2" /tmp/bmr-sig-b2.log && fail "场景B：R2 不应 spawn 桥（上限=1）"
grep -qE "running bridges .* >= max|fallback redirect" /tmp/bmr-sig-b2.log || fail "场景B：未出现上限/回退日志"
echo "  场景B PASS：R1 桥优先、R2 上限回退 Redirect 并直连 PoP-A 解码"

grep -qiE "panic|abort" /tmp/bmr-*.log && fail "发现 panic/abort"
kill "$VIEW1" "$VIEW2" "$PUB1" "$PUB2" 2>/dev/null || true
echo "== #216 M9 多房间桥并发 + 上限回退 PASS =="
