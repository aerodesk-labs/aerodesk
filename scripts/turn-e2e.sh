#!/usr/bin/env bash
# turn-e2e.sh —— TURN relay 端到端（#191 SFU 内嵌 TURN server）：
#   默认内嵌模式：SFU 单进程提供 TURN+STUN（SFU_TURN_PORT），无 coturn。
#   可选 coturn 模式（TURN_MODE=coturn）：外部 coturn + SFU TURN_URLS 覆盖（兼容老部署）。
# 断言：
#   1) 自研客户端 ↔ TURN server 中继回环（core #[ignore] 测试）
#   2) webrtc-rs turn client 互操作（独立实现）
#   3) CLI 发布/观看端：allocate + relayed 候选 + ICE 连通
# 独立端口避免与本机其它 agent 冲突。
set -euo pipefail
cd "$(dirname "$0")/.."
export RUST_LOG="${RUST_LOG:-info}"

TURN_MODE="${TURN_MODE:-embedded}"
TURN_PORT="${TURN_PORT:-14789}"
TURN_SECRET="${TURN_SECRET:-testsecret}"
ROOM="turn-$(date +%s)"

echo "== 构建"
cargo build -q -p aerodesk-sfu -p aerodesk-signal -p aerodesk-cli -p aerodesk-core
TARGET_DIR="${CARGO_TARGET_DIR:-$PWD/target}/debug"

TURN_PID=""
if [ "$TURN_MODE" = "coturn" ]; then
    echo "== coturn 模式：启动 coturn (127.0.0.1:$TURN_PORT)"
    command -v turnserver >/dev/null || { echo "FAIL: turnserver not found (brew install coturn)"; exit 1; }
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
    echo "PASS coturn 已启动"
    TURN_URLS_OVERRIDE="turn:127.0.0.1:$TURN_PORT?transport=udp"
else
    echo "== 内嵌模式：SFU 单进程提供 TURN+STUN（无 coturn）"
    TURN_URLS_OVERRIDE=""
fi

echo "== 启动 SFU + signal"
REC="$(mktemp -d)"
RECORD_DIR="$REC" TURN_SECRET="$TURN_SECRET" \
  TURN_URLS="$TURN_URLS_OVERRIDE" SFU_TURN_PORT="$TURN_PORT" \
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
if [ "$TURN_MODE" = "embedded" ]; then
    grep -q 'embedded TURN+STUN server listening' /tmp/turn-e2e-sfu.log || { echo "FAIL 内嵌 TURN 未启动"; tail -5 /tmp/turn-e2e-sfu.log; exit 1; }
    echo "PASS SFU 内嵌 TURN server 已启动"
else
    grep -q 'TURN relay configured' /tmp/turn-e2e-sfu.log || { echo "FAIL SFU 未下发 TURN 配置"; tail -5 /tmp/turn-e2e-sfu.log; exit 1; }
fi

echo "== 1) TURN 中继回环（core ignored test ↔ $TURN_MODE server）"
if TURN_E2E_SERVER="127.0.0.1:$TURN_PORT" TURN_E2E_SECRET="$TURN_SECRET" \
   cargo test -q -p aerodesk-core --lib -- --ignored real_coturn_relay_roundtrip >/tmp/turn-e2e-core.log 2>&1; then
    echo "PASS 中继回环（自研 client）"
else
    echo "FAIL 中继回环"; tail -20 /tmp/turn-e2e-core.log
    kill "$(cat /tmp/turn-e2e-sfu.pid)" "$(cat /tmp/turn-e2e-sig.pid)" 2>/dev/null || true
    [ -n "$TURN_PID" ] && kill "$TURN_PID" 2>/dev/null || true
    exit 1
fi

echo "== 2) webrtc-rs turn client 互操作（独立实现 ↔ TURN server）"
if [ "$TURN_MODE" = "embedded" ]; then
    WEBRTC_RS="/tmp/turn-rs/target/debug/examples/turn_client_udp"
    if [ ! -x "$WEBRTC_RS" ]; then
        echo "SKIP: webrtc-rs example 未构建（cd /tmp/turn-rs && cargo build --example turn_client_udp）"
    else
        CRED=$(python3 -c "
import hmac,hashlib,base64,time
now=int(time.time())+3600
u=f'{now}:e2e'
p=base64.b64encode(hmac.new(b'$TURN_SECRET',u.encode(),hashlib.sha1).digest()).decode()
print(f'{u}={p}')
")
        if RUST_LOG=info "$WEBRTC_RS" --host 127.0.0.1 --port "$TURN_PORT" --user "$CRED" \
            --realm aerodesk.io --ping >/tmp/turn-e2e-webrtc.log 2>&1 \
            && grep -q 'relayed-address=' /tmp/turn-e2e-webrtc.log; then
            echo "PASS webrtc-rs 互操作（allocate + ping）"
        else
            echo "FAIL webrtc-rs 互操作"; tail -8 /tmp/turn-e2e-webrtc.log
            kill "$(cat /tmp/turn-e2e-sfu.pid)" "$(cat /tmp/turn-e2e-sig.pid)" 2>/dev/null || true
            [ -n "$TURN_PID" ] && kill "$TURN_PID" 2>/dev/null || true
            exit 1
        fi
    fi
else
    echo "SKIP webrtc-rs（coturn 模式不要求）"
fi

echo "== 3a) 发布端：allocate + relayed 候选 + ICE"
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
    kill "$(cat /tmp/turn-e2e-sfu.pid)" "$(cat /tmp/turn-e2e-sig.pid)" 2>/dev/null || true
    [ -n "$TURN_PID" ] && kill "$TURN_PID" 2>/dev/null || true
    exit 1
fi

echo "== 3b) 观看端：allocate + relayed 候选 + ICE"
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
if [ "$ok" -ne 0 ]; then
    echo "FAIL 观看端未完成 TURN 接入"; tail -8 /tmp/turn-e2e-view.log
else
    echo "PASS 观看端 TURN 接入 + ICE 连通"
fi

kill "$PUB_PID" "$VIEW_PID" 2>/dev/null || true
kill "$(cat /tmp/turn-e2e-sfu.pid)" "$(cat /tmp/turn-e2e-sig.pid)" 2>/dev/null || true
[ -n "$TURN_PID" ] && kill "$TURN_PID" 2>/dev/null || true
wait 2>/dev/null || true
echo "E2E DONE"
