#!/usr/bin/env bash
# bitrate-feedback-e2e.sh —— 发布端码率反馈回路 e2e（#267）：
# viewer --send-control '{"bitrate":N}' → SFU control 透传 → publisher 收到并
# 解析（真实屏幕发布端调用 Encoder::set_bitrate；合成发布端日志验证）。
set -euo pipefail
cd "$(dirname "$0")/.."

ROOM="brf-$(date +%s)"
export RUST_LOG="${RUST_LOG:-info}"

echo "== 构建"
cargo build -q -p aerodesk-sfu -p aerodesk-signal -p aerodesk-cli

echo "== 启动 sfu + signal（独立端口）"
# INTERNAL_TOKEN 必须设置：否则残留/误连时内网 API 无鉴权直接 200（session-api 曾误判）。
TOKEN="test-token"
INTERNAL_TOKEN="$TOKEN" SFU_MEDIA_PORT=1478 SFU_SIGNAL_PORT=14000 SFU_INTERNAL_PORT=14002 \
  ./target/debug/aerodesk-sfu >/tmp/brf-sfu.log 2>&1 &
SFU=$!
SIGNAL_PORT=14001 SIGNAL_PLAIN_PORT=14003 SFU_URL=http://127.0.0.1:14002 SFU_TOKEN="$TOKEN" \
  ./target/debug/aerodesk-signal >/tmp/brf-sig.log 2>&1 &
SIG=$!
trap 'kill $SFU $SIG 2>/dev/null || true' EXIT
for _ in $(seq 1 50); do
  if nc -z 127.0.0.1 14002 2>/dev/null && nc -z 127.0.0.1 14003 2>/dev/null; then break; fi
  sleep 0.2
done
sleep 0.3

echo "== 合成发布端（vt，无 TCC）"
./target/debug/aerodesk-cli --role publisher --encoder vt --signal ws://127.0.0.1:14003 \
  --room "$ROOM" >/tmp/brf-pub.log 2>&1 &
PUB=$!

echo "== viewer 经 control 下发码率反馈"
./target/debug/aerodesk-cli --role viewer --signal ws://127.0.0.1:14003 --room "$ROOM" \
  --send-control '{"bitrate": 2000000}' >/tmp/brf-view.log 2>&1 &
VIEW=$!
sleep 6

# 断言 publisher 收到并解析码率反馈。
if grep -q "control: bitrate feedback -> 2000000" /tmp/brf-pub.log; then
  echo "PASS: publisher 收到码率反馈（2000000 bps）"
else
  echo "FAIL: publisher 未收到码率反馈；日志："
  tail -5 /tmp/brf-pub.log
  kill "$PUB" "$VIEW" 2>/dev/null || true
  exit 1
fi
# 断言 viewer 已发出。
grep -q "control command sent" /tmp/brf-view.log || { echo "WARN: viewer 未见发出日志"; }

kill "$PUB" "$VIEW" 2>/dev/null || true
echo "BITRATE-FEEDBACK E2E PASS"
