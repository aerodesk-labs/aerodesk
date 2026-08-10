#!/usr/bin/env bash
# #216 M3：桥接编排端到端——BRIDGE_CMD 桥优先接入 + 失败回退 v1 Redirect + 延迟 p99。
#
# 拓扑（本地双 SFU 模拟双 PoP，独立端口 148xx/149xx 避免与其它 e2e 冲突）：
#   PoP-A（pop-a，148xx）: CLI publisher 发布 room
#   PoP-B（pop-b，149xx）: ROOM_POP_MAP 把 bridge-* 钉到 pop-a + POP_URLS + BRIDGE_CMD
# 场景 0（直连基线）：同 PoP-A 的 publisher+viewer LATENCY p99（#8 光标墙钟法）
# 场景 1（桥优先）：PoP-B viewer 加入 → 信令自动 spawn aerodesk-bridge（view pop-a +
#   publish pop-b）→ 就绪后 viewer 本 PoP 接入（无 Redirect）→ 解码跨 PoP 媒体；
#   桥 p99 ≤ 直连 p99×4+500ms（SCTP 每跳 ~150ms，见 BRIDGE.md 实测）
# 场景 2（回退）：PoP-B 信令改用 BRIDGE_CMD=false（桥必失败）→ viewer 加入 →
#   信令回退 v1 Redirect → viewer 自动跟随到 pop-a 直连解码。
set -uo pipefail
cd "$(dirname "$0")/.."
export RUST_LOG="${RUST_LOG:-info}"
TARGET_DIR="${CARGO_TARGET_DIR:-$PWD/target}/debug"

ROOM="bridge-fb-$(date +%s)"
# PoP-A
SIG_A=14800; INT_A=14802; PLAIN_A=14803; MEDIA_A=14878
# PoP-B
SIG_B=14900; INT_B=14902; PLAIN_B=14903; MEDIA_B=14978
AUTH="test-bridge-token"
# 生产认证路径：信令 AUTH_TOKENS 校验；桥经 BRIDGE_AUTH_TOKEN 注入 --auth-token。
BRIDGE_CMD="$TARGET_DIR/aerodesk-bridge --remote-signal ws://127.0.0.1:${PLAIN_A} --local-signal ws://127.0.0.1:${PLAIN_B} --room {room} --auth-token \"\$BRIDGE_AUTH_TOKEN\" --codec h264"

fail() { echo "FAIL: $*"; exit 1; }
cleanup() {
  pkill -f 'aerodesk-bridge' 2>/dev/null || true
  pkill -f 'aerodesk-cli' 2>/dev/null || true
  [ -n "${SFU_A:-}" ] && kill "$SFU_A" 2>/dev/null || true
  [ -n "${SFU_B:-}" ] && kill "$SFU_B" 2>/dev/null || true
  [ -n "${SIG_A_PID:-}" ] && kill "$SIG_A_PID" 2>/dev/null || true
  [ -n "${SIG_B_PID:-}" ] && kill "$SIG_B_PID" 2>/dev/null || true
}
trap cleanup EXIT

latency_p99() { # $1=logfile → 输出 p99（无样本输出 NONE）
  python3 - "$1" <<'PY'
import re, sys
s = open(sys.argv[1]).read()
vals = sorted(int(m) for m in re.findall(r'LATENCY: (\d+) ms', s))
if not vals:
    print("NONE"); raise SystemExit(0)
idx = min(len(vals)-1, int(len(vals)*0.99))
print(vals[idx])
PY
}
latency_count() { grep -c "LATENCY:" "$1" 2>/dev/null || echo 0; }

wait_decoded() { # $1=logfile
  for _ in $(seq 1 240); do
    grep -qE "DECODED: [1-9]" "$1" 2>/dev/null && return 0
    sleep 0.5
  done
  return 1
}

echo "== 构建"
cargo build -q -p aerodesk-sfu -p aerodesk-signal -p aerodesk-cli -p aerodesk-bridge
REC_A="$(mktemp -d)"; REC_B="$(mktemp -d)"

echo "== 启动 PoP-A（148xx）"
RECORD_DIR="$REC_A" SFU_MEDIA_PORT="$MEDIA_A" SFU_SIGNAL_PORT="$SIG_A" SFU_INTERNAL_PORT="$INT_A" \
  "$TARGET_DIR/aerodesk-sfu" >/tmp/bfb-sfu-a.log 2>&1 &
SFU_A=$!
POP_ID=pop-a AUTH_TOKENS="$AUTH" SIGNAL_PORT=14801 SIGNAL_PLAIN_PORT="$PLAIN_A" SFU_URL="http://127.0.0.1:${INT_A}" \
  "$TARGET_DIR/aerodesk-signal" >/tmp/bfb-sig-a.log 2>&1 &
SIG_A_PID=$!
for _ in $(seq 1 80); do nc -z 127.0.0.1 "$PLAIN_A" 2>/dev/null && break; sleep 0.2; done
sleep 0.3

echo "== 场景 0：直连延迟基线（同 PoP-A publisher+viewer，~20s）"
"$TARGET_DIR/aerodesk-cli" --role publisher --signal "ws://127.0.0.1:${PLAIN_A}" --room "$ROOM" --token "$AUTH" \
  --encoder vt --width 1280 --height 720 --fps 30 --bitrate 2000000 --noisy \
  >/tmp/bfb-direct-pub.log 2>&1 &
PUB0=$!
ok=0
for _ in $(seq 1 120); do grep -q "ICE connected" /tmp/bfb-direct-pub.log 2>/dev/null && ok=1 && break; sleep 0.5; done
[ "$ok" = "1" ] || fail "场景0：publisher 未连上"
"$TARGET_DIR/aerodesk-cli" --role viewer --signal "ws://127.0.0.1:${PLAIN_A}" --room "$ROOM" --token "$AUTH" \
  >/tmp/bfb-direct-view.log 2>&1 &
VIEW0=$!
wait_decoded /tmp/bfb-direct-view.log || fail "场景0：直连 viewer 未解码"
for _ in $(seq 1 40); do
  [ "$(latency_count /tmp/bfb-direct-view.log)" -ge 15 ] && break
  sleep 0.5
done
DIRECT_P99=$(latency_p99 /tmp/bfb-direct-view.log)
DIRECT_N=$(latency_count /tmp/bfb-direct-view.log)
echo "  直连基线：samples=${DIRECT_N} p99=${DIRECT_P99}ms"
[ "$DIRECT_P99" != "NONE" ] || fail "场景0：无 LATENCY 样本"
kill "$VIEW0" "$PUB0" 2>/dev/null || true; sleep 1

echo "== 启动 PoP-B（149xx，BRIDGE_CMD 桥优先）"
RECORD_DIR="$REC_B" SFU_MEDIA_PORT="$MEDIA_B" SFU_SIGNAL_PORT="$SIG_B" SFU_INTERNAL_PORT="$INT_B" \
  "$TARGET_DIR/aerodesk-sfu" >/tmp/bfb-sfu-b.log 2>&1 &
SFU_B=$!
POP_ID=pop-b AUTH_TOKENS="$AUTH" ROOM_POP_MAP="bridge-=pop-a" POP_URLS="pop-a=ws://127.0.0.1:${PLAIN_A}" \
  BRIDGE_CMD="$BRIDGE_CMD" BRIDGE_READY_TIMEOUT_SECS=20 BRIDGE_AUTH_TOKEN="$AUTH" \
  SIGNAL_PORT=14901 SIGNAL_PLAIN_PORT="$PLAIN_B" SFU_URL="http://127.0.0.1:${INT_B}" \
  "$TARGET_DIR/aerodesk-signal" >/tmp/bfb-sig-b.log 2>&1 &
SIG_B_PID=$!
for _ in $(seq 1 80); do nc -z 127.0.0.1 "$PLAIN_B" 2>/dev/null && break; sleep 0.2; done
sleep 0.3
grep -q "bridge orchestration enabled" /tmp/bfb-sig-b.log || fail "PoP-B 未启用桥编排（BRIDGE_CMD 未生效）"
echo "  PoP-B bridge orchestration enabled"

echo "== 场景 1：PoP-A publisher + PoP-B viewer（桥优先，不 Redirect）"
"$TARGET_DIR/aerodesk-cli" --role publisher --signal "ws://127.0.0.1:${PLAIN_A}" --room "$ROOM" --token "$AUTH" \
  --encoder vt --width 1280 --height 720 --fps 30 --bitrate 2000000 --noisy \
  >/tmp/bfb-pub-a.log 2>&1 &
PUB_A=$!
ok=0
for _ in $(seq 1 120); do grep -q "ICE connected" /tmp/bfb-pub-a.log 2>/dev/null && ok=1 && break; sleep 0.5; done
[ "$ok" = "1" ] || fail "场景1：PoP-A publisher 未连上"; echo "  publisher connected"

"$TARGET_DIR/aerodesk-cli" --role viewer --signal "ws://127.0.0.1:${PLAIN_B}" --room "$ROOM" --token "$AUTH" \
  >/tmp/bfb-view-b.log 2>&1 &
VIEW_B=$!
wait_decoded /tmp/bfb-view-b.log || fail "场景1：PoP-B viewer 未解码跨 PoP 媒体（见 /tmp/bfb-view-b.log /tmp/bfb-sig-b.log）"
grep -q "signal redirect" /tmp/bfb-view-b.log && fail "场景1：viewer 不应收到 Redirect（桥优先应本 PoP 接入）"
grep -q "bridge ready" /tmp/bfb-sig-b.log || fail "场景1：PoP-B 信令未记录 bridge ready"
grep -q "bridge: spawned" /tmp/bfb-sig-b.log || fail "场景1：PoP-B 信令未自动 spawn 桥"
echo "  场景1 PASS：viewer 本 PoP 接入（无 Redirect），信令自动 spawn 桥并就绪"
DECODED=$(grep -oE "DECODED: [0-9]+" /tmp/bfb-view-b.log | tail -1 | cut -d' ' -f2)
echo "  viewer DECODED=${DECODED}"

echo "== 桥延迟 p99（LATENCY 采样 ≥15，与直连基线对比）"
for _ in $(seq 1 40); do
  [ "$(latency_count /tmp/bfb-view-b.log)" -ge 15 ] && break
  sleep 0.5
done
BRIDGE_P99=$(latency_p99 /tmp/bfb-view-b.log)
BRIDGE_N=$(latency_count /tmp/bfb-view-b.log)
echo "  桥路径：samples=${BRIDGE_N} p99=${BRIDGE_P99}ms（直连基线 p99=${DIRECT_P99}ms）"
[ "$BRIDGE_P99" != "NONE" ] || fail "桥路径无 LATENCY 样本（cursor 链路未通）"
# SCTP 每跳 ~150ms（debug/loopback 实测，见 BRIDGE.md）；桥比直连多 2 跳。
THRESHOLD=$((DIRECT_P99 * 4 + 500))
[ "$BRIDGE_P99" -lt "$THRESHOLD" ] || fail "桥延迟 p99=${BRIDGE_P99}ms ≥ 阈值 ${THRESHOLD}ms（直连 ${DIRECT_P99}ms）"

echo "== 场景 3：桥死亡后新 viewer 加入自动重建桥（自然死亡不触发冷却）"
kill "$VIEW_B" 2>/dev/null || true
sleep 1
pkill -f 'aerodesk-bridge' 2>/dev/null || true
sleep 2
"$TARGET_DIR/aerodesk-cli" --role viewer --signal "ws://127.0.0.1:${PLAIN_B}" --room "$ROOM" --token "$AUTH" \
  >/tmp/bfb-view-b3.log 2>&1 &
VIEW_B=$!
wait_decoded /tmp/bfb-view-b3.log || fail "场景3：桥死亡后 viewer 未恢复解码（见 /tmp/bfb-view-b3.log）"
grep -q "signal redirect" /tmp/bfb-view-b3.log && fail "场景3：重建桥不应触发 Redirect"
SPAWNS=$(grep -c "bridge: spawned" /tmp/bfb-sig-b.log 2>/dev/null || echo 0)
[ "$SPAWNS" -ge 2 ] || fail "场景3：桥未重建（spawn 次数=${SPAWNS}，应 ≥2）"
grep -q "bridge: room $ROOM ready" /tmp/bfb-sig-b.log || fail "场景3：新桥未就绪"
echo "  场景3 PASS：桥死亡后新 viewer 自动重建桥（spawn=${SPAWNS}）并恢复解码"
kill "$VIEW_B" "$PUB_A" 2>/dev/null || true
sleep 1
pkill -f 'aerodesk-bridge' 2>/dev/null || true

echo "== 场景 2：桥失败回退 v1 Redirect"
kill "$VIEW_B" 2>/dev/null || true
sleep 1
pkill -f 'aerodesk-bridge' 2>/dev/null || true
kill "$SIG_B_PID" 2>/dev/null || true; wait "$SIG_B_PID" 2>/dev/null || true
sleep 1
# 重启 PoP-B 信令：BRIDGE_CMD 必失败（false）→ 桥失败 → 回退 Redirect。
POP_ID=pop-b AUTH_TOKENS="$AUTH" ROOM_POP_MAP="bridge-=pop-a" POP_URLS="pop-a=ws://127.0.0.1:${PLAIN_A}" \
  BRIDGE_CMD="false" BRIDGE_READY_TIMEOUT_SECS=5 BRIDGE_FAIL_COOLDOWN_SECS=5 \
  SIGNAL_PORT=14901 SIGNAL_PLAIN_PORT="$PLAIN_B" SFU_URL="http://127.0.0.1:${INT_B}" \
  "$TARGET_DIR/aerodesk-signal" >/tmp/bfb-sig-b2.log 2>&1 &
SIG_B_PID=$!
for _ in $(seq 1 50); do nc -z 127.0.0.1 "$PLAIN_B" 2>/dev/null && break; sleep 0.2; done
# PoP-A publisher 仍在 room（重新起）
"$TARGET_DIR/aerodesk-cli" --role publisher --signal "ws://127.0.0.1:${PLAIN_A}" --room "$ROOM" --token "$AUTH" \
  --encoder vt --width 1280 --height 720 --fps 30 --bitrate 2000000 --noisy \
  >/tmp/bfb-pub-a2.log 2>&1 &
PUB_A=$!
ok=0
for _ in $(seq 1 120); do grep -q "ICE connected" /tmp/bfb-pub-a2.log 2>/dev/null && ok=1 && break; sleep 0.5; done
[ "$ok" = "1" ] || fail "场景2：PoP-A publisher 未连上"

"$TARGET_DIR/aerodesk-cli" --role viewer --signal "ws://127.0.0.1:${PLAIN_B}" --room "$ROOM" --token "$AUTH" \
  >/tmp/bfb-view-b2.log 2>&1 &
VIEW_B=$!
ok=0
for _ in $(seq 1 240); do
  grep -q "signal redirect" /tmp/bfb-view-b2.log 2>/dev/null && ok=1 && break
  sleep 0.5
done
[ "$ok" = "1" ] || fail "场景2：viewer 未收到 Redirect（桥失败应回退 v1）"
wait_decoded /tmp/bfb-view-b2.log || fail "场景2：viewer 跟随 Redirect 到 pop-a 后未解码"
grep -q "fallback redirect" /tmp/bfb-sig-b2.log || fail "场景2：PoP-B 信令未记录 fallback redirect"
echo "  场景2 PASS：桥失败 → v1 Redirect → viewer 自动跟随到 pop-a 解码"

grep -qiE "panic|abort" /tmp/bfb-*.log && fail "发现 panic/abort"

kill "$VIEW_B" "$PUB_A" 2>/dev/null || true
echo "== #216 M3 桥接编排 e2e PASS（桥优先 + 失败回退 + 延迟 p99：直连=${DIRECT_P99}ms 桥=${BRIDGE_P99}ms）=="
