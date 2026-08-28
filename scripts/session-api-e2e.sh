#!/usr/bin/env bash
# session-api-e2e.sh —— SFU 会话管理 API（#240）：房间/客户端列表 + 踢人。
#
# 拓扑：sfu(INTERNAL_TOKEN) + signal + publisher + viewer
# 流程：403 → GET /session/rooms → GET /session/clients?room= → 踢 publisher →
#       断言客户端数回落、/healthz 计数回落、audit.log 含 session/kick。
set -euo pipefail
cd "$(dirname "$0")/.."
ROOT="$(pwd)"

ROOM="sess-$(date +%s)"
export RUST_LOG="${RUST_LOG:-info}"
TOKEN="test-token"
REC="$(mktemp -d)"

fail() { echo "FAIL: $*"; exit 1; }
jget() { python3 -c "import sys,json; v=json.load(sys.stdin); print(eval(sys.argv[1]))" "$1" 2>/dev/null || echo ""; }

echo "== 构建"
cargo build -q -p aerodesk-sfu -p aerodesk-signal -p aerodesk-agent

# 前置 e2e 可能残留 sfu/signal 占 14000-14003（无 INTERNAL_TOKEN）→ 先清理，
# 否则无 token 403 断言会打到旧实例返回 200（CI 测试隔离）。
pkill -f "aerodesk-sfu|aerodesk-signal" 2>/dev/null || true
sleep 1

echo "== 启动 sfu（独立端口 1478/14000/14002）+ signal（14001/14003）"
RECORD_DIR="$REC" RECORD_ON_DEMAND=1 INTERNAL_TOKEN="$TOKEN" \
  SFU_MEDIA_PORT=1478 SFU_SIGNAL_PORT=14000 SFU_INTERNAL_PORT=14002 \
  ./target/debug/aerodesk-sfu >/tmp/sess-sfu.log 2>&1 &
SFU=$!
SIGNAL_PORT=14001 SFU_URL=http://127.0.0.1:14002 SFU_TOKEN="$TOKEN" \
  SIP_UDP_PORT=5060 ./target/debug/aerodesk-signal >/tmp/sess-sig.log 2>&1 &
SIG=$!
for _ in $(seq 1 50); do
    if nc -z 127.0.0.1 14002 2>/dev/null && grep -q "SIP/UDP 监听已起" /tmp/sess-sig.log 2>/dev/null; then break; fi
    sleep 0.2
done

echo "== 无 token 应 403"
# 等 INTERNAL_TOKEN 生效：前一 e2e（bitrate-feedback）SFU 无 token 且退出
# 后有 3s drain 窗口，TCP 就绪探测可能命中旧实例（无 token → 200）。
# 必须等到「无 token 返回 403」才认为已连到本脚本的受保护实例。
CODE=000
for _ in $(seq 1 50); do
    CODE=$(curl -s -o /dev/null -w '%{http_code}' "http://127.0.0.1:14002/session/rooms")
    [ "$CODE" = "403" ] && break
    sleep 0.2
done
[ "$CODE" = "403" ] && echo "PASS unauthenticated 403" || fail "expected 403 got $CODE (旧实例残留或 SFU 未绑定 14002)"
CODE=$(curl -s -o /dev/null -w '%{http_code}' -X POST "http://127.0.0.1:14002/session/kick?room=x&client=1")
[ "$CODE" = "403" ] && echo "PASS kick unauthenticated 403" || fail "expected 403 got $CODE"

echo "== 加入 2 个 viewer（SIP 会议桥 → SFU）"
# #552 后原生端 1:1 P2P 不经 SFU——会议桥（无绑定房间 INVITE）是原生端进 SFU
# 的唯一稳定路径；2 个 CLI viewer 纯 CLI 无浏览器依赖（clients=2 断言保持）。
./target/debug/aerodesk-agent --role viewer \
    --signal ws://127.0.0.1:14003 --room "$ROOM" >/tmp/sess-view.log 2>&1 &
VIEW=$!
sleep 2
./target/debug/aerodesk-agent --role viewer \
    --signal ws://127.0.0.1:14003 --room "$ROOM" >/tmp/sess-view2.log 2>&1 &
VIEW2=$!
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
[ "$ROLES" = "['viewer', 'viewer']" ] || fail "roles 异常: $ROLES"
PID=$(echo "$CLIENTS" | jget "v['clients'][0]['id']")
echo "PASS clients=$N roles=$ROLES kick_target_id=$PID"

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
[ "$CNT" = "1" ] && echo "PASS clients 回落到 1（被踢客户端已断开）" || fail "kick 后仍有 $CNT 客户端"

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

kill "$VIEW" "$VIEW2" "$SFU" "$SIG" 2>/dev/null || true
wait 2>/dev/null || true
echo "== 会话管理 API e2e PASS =="
