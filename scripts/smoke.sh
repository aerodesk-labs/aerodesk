#!/usr/bin/env bash
# AeroDesk 端到端冒烟：sfu + signal + publisher + viewer
# 断言：媒体流接收、输入事件穿透（viewer→SFU→publisher）。
# 用法: scripts/smoke.sh [房间] [时长秒]
set -euo pipefail
cd "$(dirname "$0")/.."

ROOM="${1:-smoke-$(date +%s)}"
SECONDS="${2:-6}"
export RUST_LOG="${RUST_LOG:-info}"

echo "== 构建"
cargo build -q -p aerodesk-sfu -p aerodesk-signal -p aerodesk-agent

REC="$(mktemp -d)"
echo "== 启动 sfu/signal"
RECORD_DIR="$REC" ./target/debug/aerodesk-sfu >/tmp/smoke-sfu.log 2>&1 &
SFU_PID=$!
SIP_UDP_PORT=5060 ./target/debug/aerodesk-signal >/tmp/smoke-sig.log 2>&1 &
SIG_PID=$!
# 等待信令服务器就绪（避免负载下启动慢导致 CLI 连接失败；最多 ~10s）。
for _ in $(seq 1 50); do
    if nc -z 127.0.0.1 3003 2>/dev/null; then break; fi
    if ! kill -0 "$SIG_PID" 2>/dev/null; then
        echo "signal 服务器启动失败"; cat /tmp/smoke-sig.log; exit 1
    fi
    sleep 0.2
done
sleep 0.3

echo "== 启动 publisher + viewer"
./target/debug/aerodesk-agent --role publisher --signal ws://127.0.0.1:3003 --room "$ROOM" >/tmp/smoke-pub.log 2>&1 &
PUB_PID=$!
./target/debug/aerodesk-agent --role viewer --signal ws://127.0.0.1:3003 --room "$ROOM" >/tmp/smoke-view.log 2>&1 &
VIEW_PID=$!

sleep "$SECONDS"
kill "$PUB_PID" "$VIEW_PID" "$SFU_PID" "$SIG_PID" 2>/dev/null || true
wait 2>/dev/null || true

echo "== 断言"
fail=0
# 1) 发布端收到输入事件
if grep -q "input: seq=" /tmp/smoke-pub.log; then
  echo "PASS input relay (publisher received InputFrame)"
else
  echo "FAIL input relay"; fail=1
fi
# 2) 观看端收到媒体帧
if grep -qE "RECEIVED: [1-9]" /tmp/smoke-view.log; then
  echo "PASS media receive (viewer got frames)"
else
  echo "FAIL media receive"; fail=1
fi
# 3) 无 auth/panic 错误
if grep -qiE "panic|auth failed" /tmp/smoke-pub.log /tmp/smoke-view.log /tmp/smoke-sfu.log; then
  echo "FAIL unexpected error"; fail=1
else
  echo "PASS no errors"
fi

rm -rf "$REC"
exit $fail
