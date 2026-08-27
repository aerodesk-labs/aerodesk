#!/usr/bin/env bash
# #75 远程光标端到端：publisher（合成光标轨迹）→ SFU → viewer CURSOR 日志。
# 断言：viewer 收到至少 2 个不同 x 坐标（证明轨迹在持续流动），无 panic。
# 用法: scripts/cursor-e2e.sh [房间]
set -euo pipefail
cd "$(dirname "$0")/.."

ROOM="${1:-cur-$(date +%s)}"
export RUST_LOG="${RUST_LOG:-info}"

echo "== 构建"
cargo build -q -p aerodesk-sfu -p aerodesk-signal -p aerodesk-agent

REC="$(mktemp -d)"
echo "== 启动 sfu/signal"
RECORD_DIR="$REC" ./target/debug/aerodesk-sfu >/tmp/cur-sfu.log 2>&1 &
SFU_PID=$!
SIP_UDP_PORT=5060 ./target/debug/aerodesk-signal >/tmp/cur-sig.log 2>&1 &
SIG_PID=$!
for _ in $(seq 1 50); do
    if grep -q "SIP/UDP 监听已起" /tmp/cur-sig.log 2>/dev/null; then break; fi
    if ! kill -0 "$SIG_PID" 2>/dev/null; then
        echo "signal 服务器启动失败"; cat /tmp/cur-sig.log; exit 1
    fi
    sleep 0.2
done
sleep 0.3

# #584 SIP 1:1：publisher 先注册被叫、viewer 后呼入（viewer 先起时 INVITE 无绑定
# 走会议桥，光标轨迹链路建不起来）。
echo "== 启动 publisher（合成光标）+ viewer"
./target/debug/aerodesk-agent --role publisher \
    --signal ws://127.0.0.1:3003 --room "$ROOM" >/tmp/cur-pub.log 2>&1 &
PUB_PID=$!
sleep 2
./target/debug/aerodesk-agent --role viewer \
    --signal ws://127.0.0.1:3003 --room "$ROOM" >/tmp/cur-view.log 2>&1 &
VIEW_PID=$!

# 等待至少 2 个不同 x 坐标（viewer 1s 节流打点，20s 足够覆盖正弦轨迹）
seen=0
for _ in $(seq 1 100); do
    if ! distinct=$(grep -oE "CURSOR: x=[0-9.]+" /tmp/cur-view.log 2>/dev/null \
        | awk -F'x=' '{print $2}' | sort -u | wc -l | tr -d ' '); then
        distinct=0
    fi
    if [ "${distinct:-0}" -ge 2 ]; then seen=1; break; fi
    sleep 0.2
done

kill "$VIEW_PID" "$PUB_PID" "$SFU_PID" "$SIG_PID" 2>/dev/null || true
wait 2>/dev/null || true

echo "== 断言"
fail=0
if [ "$seen" = "1" ]; then
    echo "PASS cursor positions streamed (>=2 distinct x)"
else
    echo "FAIL cursor not received"; tail -5 /tmp/cur-view.log; tail -5 /tmp/cur-pub.log; fail=1
fi
if grep -qiE "panic" /tmp/cur-view.log /tmp/cur-pub.log /tmp/cur-sfu.log; then
    echo "FAIL panic in logs"; fail=1
else
    echo "PASS no panics"
fi

exit $fail
