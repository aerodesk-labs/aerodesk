#!/usr/bin/env bash
# #58 显示器切换控制链路：viewer --display N → SFU control 转发 → publisher。
# 真实采集切换需要多显示器硬件（单显示器下 publisher 会报错并保持当前），
# CI 只验证指令转发链路。
# 用法: scripts/display-e2e.sh [房间]
set -euo pipefail
cd "$(dirname "$0")/.."

ROOM="${1:-disp-$(date +%s)}"
export RUST_LOG="${RUST_LOG:-info}"

echo "== 构建"
cargo build -q -p aerodesk-sfu -p aerodesk-signal -p aerodesk-agent

REC="$(mktemp -d)"
echo "== 启动 sfu/signal"
RECORD_DIR="$REC" ./target/debug/aerodesk-sfu >/tmp/disp-sfu.log 2>&1 &
SFU_PID=$!
./target/debug/aerodesk-signal >/tmp/disp-sig.log 2>&1 &
SIG_PID=$!
for _ in $(seq 1 50); do
    if nc -z 127.0.0.1 3003 2>/dev/null; then break; fi
    if ! kill -0 "$SIG_PID" 2>/dev/null; then
        echo "signal 服务器启动失败"; cat /tmp/disp-sig.log; exit 1
    fi
    sleep 0.2
done
sleep 0.3

echo "== 启动 publisher（x264 合成，验证控制转发）"
./target/debug/aerodesk-agent --role publisher --encoder x264 \
    --signal ws://127.0.0.1:3003 --room "$ROOM" >/tmp/disp-pub.log 2>&1 &
PUB_PID=$!
sleep 2

echo "== viewer（--display 1 下发切换指令）"
./target/debug/aerodesk-agent --role viewer --display 1 \
    --signal ws://127.0.0.1:3003 --room "$ROOM" >/tmp/disp-view.log 2>&1 &
V_PID=$!
sleep 5
kill "$V_PID" "$PUB_PID" "$SFU_PID" "$SIG_PID" 2>/dev/null || true
wait 2>/dev/null || true

echo "== 断言"
fail=0
# 1) viewer 下发指令
if grep -q "display switch command sent: 1" /tmp/disp-view.log; then
    echo "PASS viewer sent display command"
else
    echo "FAIL viewer display command"; tail -3 /tmp/disp-view.log; fail=1
fi
# 2) SFU 收到并转发
if grep -q "display request: 1" /tmp/disp-sfu.log; then
    echo "PASS SFU received display request"
else
    echo "FAIL SFU display request"; grep -i control /tmp/disp-sfu.log | tail -3; fail=1
fi
# 3) publisher 收到转发
if grep -q "control: display switch request -> display 1" /tmp/disp-pub.log; then
    echo "PASS publisher received display switch"
else
    echo "FAIL publisher display switch"; tail -3 /tmp/disp-pub.log; fail=1
fi
# 4) 无 panic
if grep -qiE "panic" /tmp/disp-pub.log /tmp/disp-view.log /tmp/disp-sfu.log; then
    echo "FAIL panic in logs"; fail=1
else
    echo "PASS no panics"
fi

exit $fail
