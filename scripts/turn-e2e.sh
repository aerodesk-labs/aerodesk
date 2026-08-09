#!/usr/bin/env bash
# turn-e2e.sh —— TURN relay 端到端（#157 M2）：
#   1) 真实 coturn 中继回环（aerodesk-core 忽略测试）
#   2) SFU 下发 TURN 配置 → CLI 客户端 allocate + offer 携带 relayed 候选 + ICE 连通
# 独立端口避免与本机其它 agent 冲突。
set -euo pipefail
cd "$(dirname "$0")/.."
export RUST_LOG="${RUST_LOG:-info}"

TURN_PORT="${TURN_PORT:-14789}"
TURN_SECRET="${TURN_SECRET:-testsecret}"
ROOM="turn-$(date +%s)"

echo "== 检查 turnserver"
command -v turnserver >/dev/null || { echo "FAIL: turnserver not found (brew install coturn)"; exit 1; }

echo "== 构建"
cargo build -q -p aerodesk-sfu -p aerodesk-signal -p aerodesk-cli -p aerodesk-core
TARGET_DIR="${CARGO_TARGET_DIR:-$PWD/target}/debug"

echo "== 启动 coturn (127.0.0.1:${TURN_PORT})"
# --allow-loopback-peers：本地 e2e peer 是 127.0.0.1（生产用真实 IP，无需该选项）
turnserver -n --use-auth-secret --static-auth-secret="$TURN_SECRET" \
  --realm=aerodesk.io --no-tls --no-dtls --fingerprint --allow-loopback-peers \
  --listening-port="$TURN_PORT" --listening-ip=127.0.0.1 \
  --min-port=49152 --max-port=49200 >/tmp/turn-e2e-turn.log 2>&1 &
TURN_PID=$!
for _ in $(seq 1 50); do
    if nc -z 127.0.0.1 "$TURN_PORT" 2>/dev/null; then break; fi
    sleep 0.2
done
sleep 0.3

echo "== 1) 真实 coturn 中继回环（core ignored test）"
if TURN_E2E_SERVER="127.0.0.1:$TURN_PORT" TURN_E2E_SECRET="$TURN_SECRET" \
   cargo test -q -p aerodesk-core --lib -- --ignored real_coturn_relay_roundtrip >/tmp/turn-e2e-core.log 2>&1; then
    echo "PASS coturn 中继回环"
else
    echo "FAIL coturn 中继回环"; tail -20 /tmp/turn-e2e-core.log
    kill "$TURN_PID" 2>/dev/null || true
    exit 1
fi

echo "== 启动 SFU（TURN_SECRET 下发）+ signal"
REC="$(mktemp -d)"
RECORD_DIR="$REC" TURN_SECRET="$TURN_SECRET" \
  TURN_URLS="turn:127.0.0.1:$TURN_PORT?transport=udp" \
  SFU_MEDIA_PORT=14578 SFU_SIGNAL_PORT=14500 SFU_INTERNAL_PORT=14502 \
  "$TARGET_DIR"/aerodesk-sfu >/tmp/turn-e2e-sfu.log 2>&1 &
echo $! > /tmp/turn-e2e-sfu.pid
SIGNAL_PORT=14501 SIGNAL_PLAIN_PORT=14503 SFU_URL=http://127.0.0.1:14502 \
  TURN_SECRET="$TURN_SECRET" TURN_URLS="turn:127.0.0.1:$TURN_PORT?transport=udp" \
  "$TARGET_DIR"/aerodesk-signal >/tmp/turn-e2e-sig.log 2>&1 &
echo $! > /tmp/turn-e2e-sig.pid
for _ in $(seq 1 50); do
    if nc -z 127.0.0.1 14502 2>/dev/null && nc -z 127.0.0.1 14503 2>/dev/null; then break; fi
    sleep 0.2
done
sleep 0.3
grep -q 'TURN relay configured' /tmp/turn-e2e-sfu.log || { echo "FAIL SFU 未下发 TURN 配置"; tail -5 /tmp/turn-e2e-sfu.log; kill "$TURN_PID" "$(cat /tmp/turn-e2e-sfu.pid)" "$(cat /tmp/turn-e2e-sig.pid)" 2>/dev/null || true; exit 1; }

echo "== 2a) 发布端：allocate + relayed 候选 + ICE"
"$TARGET_DIR"/aerodesk-cli --role publisher --encoder x264 --noisy \
  --signal ws://127.0.0.1:14503 --room "$ROOM" >/tmp/turn-e2e-pub.log 2>&1 &
PUB_PID=$!
ok=1
for _ in $(seq 1 60); do
    if grep -q 'TURN allocation ok' /tmp/turn-e2e-pub.log 2>/dev/null \
       && grep -q 'relayed candidate' /tmp/turn-e2e-pub.log 2>/dev/null \
       && grep -q 'SDP negotiated' /tmp/turn-e2e-pub.log 2>/dev/null \
       && grep -q 'ICE connected' /tmp/turn-e2e-pub.log 2>/dev/null; then ok=0; break; fi
    sleep 0.3
done
if [ "$ok" -eq 0 ]; then
    echo "PASS 发布端 TURN 接入 + ICE 连通"
else
    echo "FAIL 发布端未完成 TURN 接入"; tail -8 /tmp/turn-e2e-pub.log
    kill "$PUB_PID" 2>/dev/null || true
    kill "$TURN_PID" "$(cat /tmp/turn-e2e-sfu.pid)" "$(cat /tmp/turn-e2e-sig.pid)" 2>/dev/null || true
    exit 1
fi

echo "== 2b) 观看端：allocate + relayed 候选 + ICE"
"$TARGET_DIR"/aerodesk-cli --role viewer --signal ws://127.0.0.1:14503 --room "$ROOM" >/tmp/turn-e2e-view.log 2>&1 &
VIEW_PID=$!
ok=1
for _ in $(seq 1 60); do
    if grep -q 'TURN allocation ok' /tmp/turn-e2e-view.log 2>/dev/null \
       && grep -q 'relayed candidate' /tmp/turn-e2e-view.log 2>/dev/null \
       && grep -q 'SDP negotiated' /tmp/turn-e2e-view.log 2>/dev/null \
       && grep -q 'ICE connected' /tmp/turn-e2e-view.log 2>/dev/null; then ok=0; break; fi
    sleep 0.3
done
if [ "$ok" -eq 0 ]; then
    echo "PASS 观看端 TURN 接入 + ICE 连通"
else
    echo "FAIL 观看端未完成 TURN 接入"; tail -8 /tmp/turn-e2e-view.log
    kill "$VIEW_PID" 2>/dev/null || true
fi

kill "$PUB_PID" "$VIEW_PID" 2>/dev/null || true
kill "$TURN_PID" "$(cat /tmp/turn-e2e-sfu.pid)" "$(cat /tmp/turn-e2e-sig.pid)" 2>/dev/null || true
wait 2>/dev/null || true
echo "E2E DONE"
