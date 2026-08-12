#!/usr/bin/env bash
# session-api-e2e.sh —— SFU 会话管理 API（#240）：房间/客户端列表 + 踢人。
#
# 拓扑：sfu(INTERNAL_TOKEN) + signal + publisher + viewer
# 流程：403 → GET /session/rooms → GET /session/clients?room= → 踢 publisher →
#       断言客户端数回落、/healthz 计数回落、audit.log 含 session/kick。
set -euo pipefail
cd "$(dirname "$0")/.."

ROOM="sess-$(date +%s)"
export RUST_LOG="${RUST_LOG:-info}"
TOKEN="test-token"
REC="$(mktemp -d)"

fail() { echo "FAIL: $*"; exit 1; }
jget() { python3 -c "import sys,json; v=json.load(sys.stdin); print(eval(sys.argv[1]))" "$1" 2>/dev/null || echo ""; }

echo "== 构建"
cargo build -q -p aerodesk-sfu -p aerodesk-signal -p aerodesk-cli

echo "== 启动 sfu（独立端口 1478/14000/14002）+ signal（14001/14003）"
RECORD_DIR="$REC" RECORD_ON_DEMAND=1 INTERNAL_TOKEN="$TOKEN" \
  SFU_MEDIA_PORT=1478 SFU_SIGNAL_PORT=14000 SFU_INTERNAL_PORT=14002 \
  ./target/debug/aerodesk-sfu >/tmp/sess-sfu.log 2>&1 &
SFU=$!
SIGNAL_PORT=14001 SIGNAL_PLAIN_PORT=14003 SFU_URL=http://127.0.0.1:14002 SFU_TOKEN="$TOKEN" \
  ./target/debug/aerodesk-signal >/tmp/sess-sig.log 2>&1 &
SIG=$!
trap 'kill $SFU $SIG 2>/dev/null || true; wait 2>/dev/null || true' EXIT
for _ in $(seq 1 50); do
    if nc -z 127.0.0.1 14002 2>/dev/null && nc -z 127.0.0.1 14003 2>/dev/null; then break; fi
    sleep 0.2
done
sleep 0.3

echo "== 无 token 应 403"
CODE=$(curl -s -o /dev/null -w '%{http_code}' "http://127.0.0.1:14002/session/rooms")
[ "$CODE" = "403" ] && echo "PASS unauthenticated 403" || fail "expected 403 got $CODE"
CODE=$(curl -s -o /dev/null -w '%{http_code}' -X POST "http://127.0.0.1:14002/session/kick?room=x&client=1")
[ "$CODE" = "403" ] && echo "PASS kick unauthenticated 403" || fail "expected 403 got $CODE"

echo "== 加入 publisher + viewer"
./target/debug/aerodesk-cli --role publisher --encoder x264 --noisy \
    --signal ws://127.0.0.1:14003 --room "$ROOM" >/tmp/sess-pub.log 2>&1 &
PUB=$!
sleep 2
./target/debug/aerodesk-cli --role viewer \
    --signal ws://127.0.0.1:14003 --room "$ROOM" >/tmp/sess-view.log 2>&1 &
VIEW=$!
sleep 5

echo "== 房间列表应包含 $ROOM 且 clients=2"
ROOMS=$(curl -s -H "X-Internal-Token: $TOKEN" "http://127.0.0.1:14002/session/rooms")
echo "$ROOMS" | grep -q "$ROOM" || fail "rooms 缺房间: $ROOMS"
CNT=$(echo "$ROOMS" | jget "next(r for r in v['rooms'] if r['room']=='$ROOM')['clients']")
[ "$CNT" = "2" ] && echo "PASS rooms clients=2" || fail "expected clients=2 got $CNT"

echo "== 客户端明细：2 个客户端，角色 publisher/viewer 各一"
CLIENTS=$(curl -s -H "X-Internal-Token: $TOKEN" "http://127.0.0.1:14002/session/clients?room=$ROOM")
N=$(echo "$CLIENTS" | jget "len(v['clients'])")
[ "$N" = "2" ] || fail "expected 2 clients got $N"
ROLES=$(echo "$CLIENTS" | jget "sorted(c['role'] for c in v['clients'])")
[ "$ROLES" = "['publisher', 'viewer']" ] || fail "roles 异常: $ROLES"
PID=$(echo "$CLIENTS" | jget "[c['id'] for c in v['clients'] if c['role']=='publisher'][0]")
echo "PASS clients=$N roles=$ROLES publisher_id=$PID"

echo "== 参数/不存在校验"
# #249：省略 client = room 级踢人（#249）→ 对空房间幂等 200 kicked=0。
CODE=$(curl -s -o /dev/null -w '%{http_code}' -X POST -H "X-Internal-Token: $TOKEN" "http://127.0.0.1:14002/session/kick?room=empty-room-$ROOM")
[ "$CODE" = "200" ] || fail "room-kick 空房间应 200，got $CODE"
curl -s -X POST -H "X-Internal-Token: $TOKEN" "http://127.0.0.1:14002/session/kick?room=empty-room-$ROOM"   | grep -q '"kicked":0' || fail "room-kick 空房间应 kicked=0"
echo "PASS room-kick 空房间 200 kicked=0"
CODE=$(curl -s -o /dev/null -w '%{http_code}' -X POST -H "X-Internal-Token: $TOKEN" "http://127.0.0.1:14002/session/kick?room=$ROOM&client=999999")
[ "$CODE" = "404" ] && echo "PASS kick unknown client 404" || fail "expected 404 got $CODE"

echo "== 踢 publisher"
curl -s -X POST -H "X-Internal-Token: $TOKEN" "http://127.0.0.1:14002/session/kick?room=$ROOM&client=$PID" \
  | grep -q '"kicked":true' || fail "kick 返回异常"
for _ in $(seq 1 30); do
    CNT=$(curl -s -H "X-Internal-Token: $TOKEN" "http://127.0.0.1:14002/session/clients?room=$ROOM" \
        | jget "len(v['clients'])")
    [ "${CNT:-}" = "1" ] && break
    sleep 0.2
done
[ "$CNT" = "1" ] && echo "PASS clients 回落到 1（publisher 已断开）" || fail "kick 后仍有 $CNT 客户端"

echo "== 幂等：再踢同 id 应 404"
CODE=$(curl -s -o /dev/null -w '%{http_code}' -X POST -H "X-Internal-Token: $TOKEN" "http://127.0.0.1:14002/session/kick?room=$ROOM&client=$PID")
[ "$CODE" = "404" ] && echo "PASS kick idempotent 404" || fail "expected 404 got $CODE"

echo "== audit.log 含 session/kick 事件（含 403/404）"
AUDIT="$REC/audit.log"
grep -q '"action":"session/kick".*"status":200' "$AUDIT" || fail "audit 缺 session/kick 200"
grep -q '"action":"session/kick".*"status":403' "$AUDIT" \
  && grep -q '"action":"session/kick".*"status":404' "$AUDIT" \
  && echo "PASS audit session/kick + 403 + 404" || fail "audit 缺 403/404 留痕"

echo "== /healthz clients 回落"
sleep 6  # 等心跳（5s）刷新 metrics
H=$(curl -sk "https://127.0.0.1:14000/healthz")
echo "$H" | grep -q '"clients":1' && echo "PASS healthz clients=1" || { echo "healthz=$H"; fail "healthz 未回落"; }

kill "$VIEW" "$PUB" "$SFU" "$SIG" 2>/dev/null || true
wait 2>/dev/null || true
echo "== 会话管理 API e2e PASS =="
