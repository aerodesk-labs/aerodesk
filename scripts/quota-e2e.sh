#!/usr/bin/env bash
# quota-e2e.sh —— 信令连接配额（#163）：每房间/全局上限，Join 超限拒绝。
#
# Phase A：MAX_ROOM_CLIENTS=2 → 第 3 个加入同房间被拒（room full）
# Phase B：MAX_TOTAL_CLIENTS=2 → 第 3 个连接（不同房间）被拒（server full）
# 独立端口避免与本机其它 agent 冲突。
set -euo pipefail
cd "$(dirname "$0")/.."
export RUST_LOG="${RUST_LOG:-info}"

echo "== 构建"
cargo build -q -p aerodesk-sfu -p aerodesk-signal -p aerodesk-agent

# 独立 SFU：让已加入客户端保持连接（无 SFU 时 SDP 失败会立即断开释放名额，导致第 3 个被误接受）
SFU_TOKEN="quota-token"
REC="$(mktemp -d)"
INTERNAL_TOKEN="$SFU_TOKEN" RECORD_DIR="$REC" \
  SFU_MEDIA_PORT=14578 SFU_SIGNAL_PORT=14500 SFU_INTERNAL_PORT=14502 \
  ./target/debug/aerodesk-sfu >/tmp/quota-sfu.log 2>&1 &
SFU=$!
for _ in $(seq 1 50); do nc -z 127.0.0.1 14502 2>/dev/null && break; sleep 0.2; done

# 轮询等待（CI 负载下 CLI 启动/加入可能较慢，固定 sleep 会误判）
wait_joined() {
    local log="$1" room="$2"
    for _ in $(seq 1 100); do
        grep -q "joined room $room" "$log" 2>/dev/null && return 0
        sleep 0.2
    done
    return 1
}
wait_rejected() {
    local log="$1" pat="$2"
    for _ in $(seq 1 100); do
        grep -qE "$pat" "$log" 2>/dev/null && return 0
        sleep 0.2
    done
    return 1
}

fail=0

echo "== Phase A：房间上限 2"
SIP_UDP_PORT=5060 SIGNAL_PORT=14301 SIGNAL_PLAIN_PORT=14303 MAX_ROOM_CLIENTS=2 SFU_URL=http://127.0.0.1:14502 SFU_TOKEN="$SFU_TOKEN" ./target/debug/aerodesk-signal >/tmp/quota-sig-a.log 2>&1 &
SIGA=$!
for _ in $(seq 1 50); do grep -q "SIP/UDP 监听已起" /tmp/quota-sig-a.log 2>/dev/null && break; sleep 0.2; done
ROOM_A="quota-a-$(date +%s)"
./target/debug/aerodesk-agent --role publisher --encoder x264 --noisy --signal ws://127.0.0.1:14303 --room "$ROOM_A" >/tmp/quota-a-pub.log 2>&1 &
PUB_A=$!
wait_joined /tmp/quota-a-pub.log "$ROOM_A" || { echo "FAIL A: publisher 未加入"; tail -3 /tmp/quota-a-pub.log; fail=1; }
./target/debug/aerodesk-agent --role viewer --signal ws://127.0.0.1:14303 --room "$ROOM_A" >/tmp/quota-a-v1.log 2>&1 &
V1=$!
wait_joined /tmp/quota-a-v1.log "$ROOM_A" || { echo "FAIL A: viewer1 未加入"; tail -3 /tmp/quota-a-v1.log; fail=1; }
./target/debug/aerodesk-agent --role viewer --signal ws://127.0.0.1:14303 --room "$ROOM_A" >/tmp/quota-a-v2.log 2>&1 || true
V2=$!
wait_rejected /tmp/quota-a-v2.log "room full" || { echo "FAIL A: 第 3 个未被拒"; tail -5 /tmp/quota-a-v2.log; fail=1; }
kill $PUB_A $V1 $V2 2>/dev/null || true
if grep -q "joined room $ROOM_A" /tmp/quota-a-pub.log && grep -q "joined room $ROOM_A" /tmp/quota-a-v1.log; then
    echo "PASS A: 前 2 个连接加入成功"
else
    echo "FAIL A: 前 2 个加入"; tail -3 /tmp/quota-a-pub.log; tail -3 /tmp/quota-a-v1.log; fail=1
fi
if grep -q "room full" /tmp/quota-a-v2.log; then
    echo "PASS A: 第 3 个被拒（room full）"
else
    echo "FAIL A: 第 3 个未被拒"; tail -5 /tmp/quota-a-v2.log; fail=1
fi
kill $SIGA 2>/dev/null || true

echo "== Phase B：全局上限 2"
SIP_UDP_PORT=5060 SIGNAL_PORT=14401 SIGNAL_PLAIN_PORT=14403 MAX_TOTAL_CLIENTS=2 SFU_URL=http://127.0.0.1:14502 SFU_TOKEN="$SFU_TOKEN" ./target/debug/aerodesk-signal >/tmp/quota-sig-b.log 2>&1 &
SIGB=$!
for _ in $(seq 1 50); do grep -q "SIP/UDP 监听已起" /tmp/quota-sig-b.log 2>/dev/null && break; sleep 0.2; done
RB1="quota-b1-$(date +%s)"; RB2="quota-b2-$(date +%s)"; RB3="quota-b3-$(date +%s)"
./target/debug/aerodesk-agent --role publisher --encoder x264 --noisy --signal ws://127.0.0.1:14403 --room "$RB1" >/tmp/quota-b-p1.log 2>&1 &
P1=$!
wait_joined /tmp/quota-b-p1.log "$RB1" || { echo "FAIL B: p1 未加入"; tail -3 /tmp/quota-b-p1.log; fail=1; }
./target/debug/aerodesk-agent --role viewer --signal ws://127.0.0.1:14403 --room "$RB2" >/tmp/quota-b-v.log 2>&1 &
V=$!
wait_joined /tmp/quota-b-v.log "$RB2" || { echo "FAIL B: viewer 未加入"; tail -3 /tmp/quota-b-v.log; fail=1; }
./target/debug/aerodesk-agent --role publisher --encoder x264 --noisy --signal ws://127.0.0.1:14403 --room "$RB3" >/tmp/quota-b-p2.log 2>&1 || true
P2=$!
wait_rejected /tmp/quota-b-p2.log "server full" || { echo "FAIL B: 第 3 个未被拒"; tail -5 /tmp/quota-b-p2.log; fail=1; }
kill $P1 $V $P2 2>/dev/null || true
if grep -q "joined room $RB1" /tmp/quota-b-p1.log && grep -q "joined room $RB2" /tmp/quota-b-v.log; then
    echo "PASS B: 前 2 个连接（不同房间）加入成功"
else
    echo "FAIL B: 前 2 个加入"; tail -3 /tmp/quota-b-p1.log; tail -3 /tmp/quota-b-v.log; fail=1
fi
if grep -q "server full" /tmp/quota-b-p2.log; then
    echo "PASS B: 第 3 个被拒（server full）"
else
    echo "FAIL B: 第 3 个未被拒"; tail -5 /tmp/quota-b-p2.log; fail=1
fi
echo "== Phase C：JWT per-user 配额（max_conns=1）"
SIGNAL_PORT=14701 SIGNAL_PLAIN_PORT=14703 JWT_SECRET=uq-secret \
  SFU_URL=http://127.0.0.1:14502 SFU_TOKEN="$SFU_TOKEN" \
  SIP_UDP_PORT=5060 ./target/debug/aerodesk-signal >/tmp/quota-sig-c.log 2>&1 &
SIGC=$!
for _ in $(seq 1 50); do grep -q "SIP/UDP 监听已起" /tmp/quota-sig-c.log 2>/dev/null && break; sleep 0.2; done
RC="uc-$(date +%s)"
TOK=$(JWT_SECRET=uq-secret ./target/debug/aerodesk-agent --issue-token --user u1 --room '*' --role '*' --ttl 600 --max-conns 1)
./target/debug/aerodesk-agent --role viewer --token "$TOK" --signal ws://127.0.0.1:14703 --room "$RC" >/tmp/quota-c-v1.log 2>&1 &
C1=$!
wait_joined /tmp/quota-c-v1.log "$RC" || { echo "FAIL C: client1 未加入"; tail -3 /tmp/quota-c-v1.log; fail=1; }
./target/debug/aerodesk-agent --role viewer --token "$TOK" --signal ws://127.0.0.1:14703 --room "$RC" >/tmp/quota-c-v2.log 2>&1 || true
C2=$!
wait_rejected /tmp/quota-c-v2.log "user quota exceeded" || { echo "FAIL C: 同用户第 2 连接未被拒"; tail -5 /tmp/quota-c-v2.log; fail=1; }
kill $C1 $C2 $SIGC 2>/dev/null || true
echo "PASS C: 同用户第 2 连接被拒（user quota exceeded）"

kill $SIGB $SFU 2>/dev/null || true
wait 2>/dev/null || true
exit $fail
