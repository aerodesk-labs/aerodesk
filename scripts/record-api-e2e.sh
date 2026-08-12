#!/usr/bin/env bash
# record-api-e2e.sh —— SFU 按需录制 API（#160）：内部接口按房间 start/stop + 状态。
#
# 拓扑：sfu(RECORD_ON_DEMAND=1, RECORD_DIR, INTERNAL_TOKEN=test) + signal + publisher
# 流程：POST /record/start?room=rr → publisher 推流 → GET /record/status →
#       POST /record/stop?room=rr → 断言 .adrec + meta.json + audit.log。
set -euo pipefail
cd "$(dirname "$0")/.."

ROOM="rec-api-$(date +%s)"
export RUST_LOG="${RUST_LOG:-info}"
TOKEN="test-token"
REC="$(mktemp -d)"

echo "== 构建"
cargo build -q -p aerodesk-sfu -p aerodesk-signal -p aerodesk-cli

echo "== 启动 sfu（按需录制，独立端口 1478/14000/14002）+ signal（14001/14003）"
# 独立端口避免与本机其它 agent 的 sfu/signal 冲突（#146 SFU 端口可配）
RECORD_DIR="$REC" RECORD_ON_DEMAND=1 INTERNAL_TOKEN="$TOKEN" \
  SFU_MEDIA_PORT=1478 SFU_SIGNAL_PORT=14000 SFU_INTERNAL_PORT=14002 \
  ./target/debug/aerodesk-sfu >/tmp/recapi-sfu.log 2>&1 &
SFU=$!
SIGNAL_PORT=14001 SIGNAL_PLAIN_PORT=14003 SFU_URL=http://127.0.0.1:14002 SFU_TOKEN="$TOKEN" ./target/debug/aerodesk-signal >/tmp/recapi-sig.log 2>&1 &
SIG=$!
trap 'kill $SFU $SIG 2>/dev/null || true; wait 2>/dev/null || true' EXIT
# 同 session-api：先等 1478/14002/14003 释放，避免命中前序残留 SFU 或绑定失败。
for _ in $(seq 1 100); do
    if ! nc -z 127.0.0.1 1478 2>/dev/null && ! nc -z 127.0.0.1 14002 2>/dev/null && ! nc -z 127.0.0.1 14003 2>/dev/null; then break; fi
    sleep 0.2
done
for _ in $(seq 1 50); do
    if nc -z 127.0.0.1 14002 2>/dev/null && nc -z 127.0.0.1 14003 2>/dev/null; then break; fi
    sleep 0.2
done
sleep 0.3
kill -0 "$SFU" 2>/dev/null || { echo "--- /tmp/recapi-sfu.log ---"; tail -20 /tmp/recapi-sfu.log; fail "sfu 启动失败（端口可能被残留进程占用）"; }

echo "== 无 token 应 403"
CODE=$(curl -s -o /dev/null -w '%{http_code}' -X POST "http://127.0.0.1:14002/record/start?room=zz")
[ "$CODE" = "403" ] && echo "PASS unauthenticated 403" || { echo "FAIL expected 403 got $CODE"; exit 1; }

echo "== start 录制房间 $ROOM"
curl -s -X POST -H "X-Internal-Token: $TOKEN" "http://127.0.0.1:14002/record/start?room=$ROOM" | grep -q '"started":true' \
  && echo "PASS start ok" || { echo "FAIL start"; exit 1; }

echo "== #240 审计：record_api 403/200 + room_start source=api + recordings gauge"
AUDIT="$REC/audit.log"
grep -q '"action":"record/start".*"status":403' "$AUDIT" \
  && echo "PASS audit record_api 403" || { echo "FAIL audit 403"; exit 1; }
grep -q '"action":"record/start".*"status":200' "$AUDIT" \
  && echo "PASS audit record_api 200" || { echo "FAIL audit 200"; exit 1; }
grep -q '"source":"api"' "$AUDIT" && echo "PASS audit room_start source=api" || { echo "FAIL source=api"; exit 1; }
BODY=$(curl -s -H "X-Internal-Token: $TOKEN" "http://127.0.0.1:14002/metrics/prometheus")
echo "$BODY" | grep -q '^aerodesk_sfu_recordings_active 1$' \
  && echo "PASS recordings_active=1" || { echo "FAIL recordings_active"; exit 1; }

echo "== publisher 推流 4s"
./target/debug/aerodesk-cli --role publisher --encoder x264 --noisy \
    --signal ws://127.0.0.1:14003 --room "$ROOM" >/tmp/recapi-pub.log 2>&1 &
PUB=$!
sleep 4
kill "$PUB" 2>/dev/null || true

echo "== status 应包含房间"
curl -s -H "X-Internal-Token: $TOKEN" "http://127.0.0.1:14002/record/status" | grep -q "$ROOM" \
  && echo "PASS status contains room" || { echo "FAIL status"; curl -s -H "X-Internal-Token: $TOKEN" "http://127.0.0.1:14002/record/status"; exit 1; }

echo "== stop 录制"
curl -s -X POST -H "X-Internal-Token: $TOKEN" "http://127.0.0.1:14002/record/stop?room=$ROOM" | grep -q '"stopped":true' \
  && echo "PASS stop ok" || { echo "FAIL stop"; exit 1; }

echo "== #240 审计：record_api stop + room_end duration + recordings gauge 回落"
grep -q '"action":"record/stop".*"status":200' "$AUDIT" \
  && echo "PASS audit record_api stop" || { echo "FAIL audit stop"; exit 1; }
grep -q '"duration_us"' "$AUDIT" && echo "PASS audit room_end duration_us" || { echo "FAIL duration_us"; exit 1; }
BODY=$(curl -s -H "X-Internal-Token: $TOKEN" "http://127.0.0.1:14002/metrics/prometheus")
echo "$BODY" | grep -q '^aerodesk_sfu_recordings_active 0$' \
  && echo "PASS recordings_active=0 after stop" || { echo "FAIL recordings_active reset"; exit 1; }

echo "== 断言产物"
FAIL=0
ADREC="$REC/$ROOM.adrec"
META="$REC/$ROOM.meta.json"
[ -s "$ADREC" ] && echo "PASS adrec exists non-empty" || { echo "FAIL adrec"; FAIL=1; }
if [ -f "$META" ] && grep -q '"packets": [1-9]' "$META"; then
    echo "PASS meta.json packets>0"
else
    echo "FAIL meta packets"; cat "$META" 2>/dev/null; FAIL=1
fi
grep -q room_start "$REC/audit.log" && grep -q room_end "$REC/audit.log" \
  && echo "PASS audit start/end" || { echo "FAIL audit"; FAIL=1; }

kill "$SFU" "$SIG" 2>/dev/null || true
wait 2>/dev/null || true
exit $FAIL
