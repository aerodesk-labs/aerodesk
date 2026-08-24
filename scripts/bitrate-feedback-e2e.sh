#!/usr/bin/env bash
# bitrate-feedback-e2e.sh —— 发布端码率反馈回路 e2e（#267）：
# viewer --send-control '{"bitrate":N}' → SFU control 透传 → publisher 收到并
# 解析（真实屏幕发布端调用 Encoder::set_bitrate；合成发布端日志验证）。
set -euo pipefail
cd "$(dirname "$0")/.."

ROOM="brf-$(date +%s)"
export RUST_LOG="${RUST_LOG:-info}"

echo "== 构建"
cargo build -q -p aerodesk-sfu -p aerodesk-signal -p aerodesk-agent

echo "== 启动 sfu + signal（独立端口）"
SFU_MEDIA_PORT=1478 SFU_SIGNAL_PORT=14000 SFU_INTERNAL_PORT=14002 \
  ./target/debug/aerodesk-sfu >/tmp/brf-sfu.log 2>&1 &
SFU=$!
SIGNAL_PORT=14001 SIGNAL_PLAIN_PORT=14003 SFU_URL=http://127.0.0.1:14002 \
  SIP_UDP_PORT=5060 ./target/debug/aerodesk-signal >/tmp/brf-sig.log 2>&1 &
SIG=$!
# 等待 SFU/signal 真正退出（drain 3s），避免下一个 e2e 命中本脚本 14002 残留实例
# （session-api 无 token 403 断言被旧实例 200 击穿，main CI 红）。
trap 'kill $SFU $SIG 2>/dev/null || true; wait $SFU 2>/dev/null || true; wait $SIG 2>/dev/null || true' EXIT
for _ in $(seq 1 50); do
  if nc -z 127.0.0.1 14002 2>/dev/null && nc -z 127.0.0.1 14003 2>/dev/null; then break; fi
  sleep 0.2
done
sleep 0.3

echo "== 合成发布端（vt，无 TCC）"
./target/debug/aerodesk-agent --role publisher --encoder vt --signal ws://127.0.0.1:14003 \
  --room "$ROOM" >/tmp/brf-pub.log 2>&1 &
PUB=$!
# #552 SIP 1:1：viewer 须在 publisher 注册完成后才 INVITE（否则 lookup 未命中
# 走会议桥 SFU——同 linux-native 竞态）——轮询注册就绪（≤15s）。
OK=0
for _ in $(seq 1 30); do
    if grep -q "SIP registered" /tmp/brf-pub.log 2>/dev/null; then OK=1; break; fi
    sleep 0.5
done
if [ "$OK" != "1" ]; then
    echo "FAIL: publisher 未完成 SIP 注册"; tail -8 /tmp/brf-pub.log
    kill "$PUB" 2>/dev/null || true
    exit 1
fi

echo "== viewer 经 control 下发码率反馈"
./target/debug/aerodesk-agent --role viewer --signal ws://127.0.0.1:14003 --room "$ROOM" \
  --send-control '{"bitrate": 2000000}' >/tmp/brf-view.log 2>&1 &
VIEW=$!
sleep 6

# 断言 publisher 收到并解析码率反馈（合成发布端打 "-> "，真实屏幕发布端打 "applied -> "）。
if grep -qE "control: bitrate feedback( applied)? -> 2000000" /tmp/brf-pub.log; then
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
