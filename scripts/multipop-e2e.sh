#!/usr/bin/env bash
# multipop-e2e.sh —— 多 PoP 房间路由 v1（#146）：本地双 PoP 验证信令重定向。
#
# 拓扑：
#   PoP A: sfu-a(media 3478/public 3000/internal 3002) + signal-a(WSS 3001 / plain 3003)
#   PoP B: sfu-b(media 3479/public 3005/internal 3007) + signal-b(WSS 3004 / plain 3006)
#   ROOM_POP_MAP "eu-=pop-b"：eu-* 房间钉到 pop-b
# 客户端先连 signal-a（3003）→ 收到 Redirect(pop-b, ws://127.0.0.1:3006/ws) →
# 自动重连 signal-b → join → SDP 经 signal-b 代理到 sfu-b。
set -euo pipefail
cd "$(dirname "$0")/.."

ROOM="eu-room-$(date +%s)"
export RUST_LOG="${RUST_LOG:-info}"

echo "== 构建"
cargo build -q -p aerodesk-sfu -p aerodesk-signal -p aerodesk-cli

REC="$(mktemp -d)"
echo "== 启动 PoP A + PoP B"
RECORD_DIR="$REC/a" ./target/debug/aerodesk-sfu >/tmp/mpop-sfu-a.log 2>&1 &
SFU_A=$!
SFU_MEDIA_PORT=3479 SFU_SIGNAL_PORT=3005 SFU_INTERNAL_PORT=3007 RECORD_DIR="$REC/b" ./target/debug/aerodesk-sfu >/tmp/mpop-sfu-b.log 2>&1 &
SFU_B=$!
POP_ID=pop-a ROOM_POP_MAP="eu-=pop-b" POP_URLS="pop-b=ws://127.0.0.1:3006/ws" ./target/debug/aerodesk-signal >/tmp/mpop-sig-a.log 2>&1 &
SIG_A=$!
POP_ID=pop-b ROOM_POP_MAP="eu-=pop-b" POP_URLS="pop-a=ws://127.0.0.1:3001/ws" SIGNAL_PORT=3004 SIGNAL_PLAIN_PORT=3006 SFU_URL=http://127.0.0.1:3007 ./target/debug/aerodesk-signal >/tmp/mpop-sig-b.log 2>&1 &
SIG_B=$!

for _ in $(seq 1 50); do
    if nc -z 127.0.0.1 3002 2>/dev/null && nc -z 127.0.0.1 3007 2>/dev/null \
       && nc -z 127.0.0.1 3003 2>/dev/null && nc -z 127.0.0.1 3006 2>/dev/null; then
        break
    fi
    sleep 0.2
done
sleep 0.3

echo "== publisher + viewer 连 signal-a（应被重定向到 pop-b）"
./target/debug/aerodesk-cli --role publisher --encoder x264 --noisy \
    --signal ws://127.0.0.1:3003 --room "$ROOM" >/tmp/mpop-pub.log 2>&1 &
PUB=$!
./target/debug/aerodesk-cli --role viewer \
    --signal ws://127.0.0.1:3003 --room "$ROOM" >/tmp/mpop-view.log 2>&1 &
VIEW=$!
sleep 10
kill "$PUB" "$VIEW" "$SFU_A" "$SFU_B" "$SIG_A" "$SIG_B" 2>/dev/null || true
wait 2>/dev/null || true

echo "== 断言"
fail=0
# 1) signal-a 返回了重定向（房间 eu-* 钉到 pop-b）
if grep -q "pinned to pop pop-b" /tmp/mpop-sig-a.log; then
    echo "PASS signal-a redirect eu-* -> pop-b"
else
    echo "FAIL no redirect in signal-a"; tail -5 /tmp/mpop-sig-a.log; fail=1
fi
# 2) 客户端重连到 signal-b（signal-b 出现新会话）
if grep -q "session open" /tmp/mpop-sig-b.log; then
    echo "PASS client reconnected to signal-b"
else
    echo "FAIL no session on signal-b"; tail -5 /tmp/mpop-sig-b.log; fail=1
fi
# 3) SDP 最终到达 PoP B 的 SFU（joined room 在 sfu-b 日志）
if grep -q "joined room $ROOM" /tmp/mpop-sfu-b.log; then
    echo "PASS media session reached sfu-b (room $ROOM)"
else
    echo "FAIL room not joined on sfu-b"; tail -8 /tmp/mpop-sfu-b.log; fail=1
fi
# 4) 无 panic
if grep -qiE "panic" /tmp/mpop-*.log; then
    echo "FAIL panic in logs"; fail=1
else
    echo "PASS no panics"
fi
exit $fail
