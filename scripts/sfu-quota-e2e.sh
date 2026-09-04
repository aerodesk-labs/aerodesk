#!/usr/bin/env bash
# sfu-quota-e2e.sh —— SFU /start 准入配额（#180；#584 SIP 会议桥形态重写）：
# MAX_ROOM_CLIENTS=1 时同会议房间的第 2 个客户端被 503。
# 触达路径（v0.4 §4）：viewer INVITE 非设备 AoR（会议房间）→ signal 会议桥
# 代理 SFU /start；配额拒绝（room full）由 SFU 以 HTTP 503 回应，signal 透传为
# SIP 503，客户端报「SIP 呼叫被拒（503）」。
# 旧 JSON 形态（SIGNAL_PLAIN_PORT + publisher 直连 SFU /start）随 P3 退役；
# 本版为纯会议路径，无需 publisher（空会议房 recvonly 入会即可触达 /start）。
set -euo pipefail
cd "$(dirname "$0")/.."
export RUST_LOG="${RUST_LOG:-info}"
TARGET_DIR="${CARGO_TARGET_DIR:-$PWD/target}/debug"

fail() { echo "FAIL: $*"; exit 1; }
cleanup() {
  pkill -f 'aerodesk-agent' 2>/dev/null || true
  [ -n "${SFU_PID:-}" ] && kill "$SFU_PID" 2>/dev/null || true
  [ -n "${SIG_PID:-}" ] && kill "$SIG_PID" 2>/dev/null || true
}
trap cleanup EXIT

echo "== 构建"
cargo build -q -p aerodesk-sfu -p aerodesk-signal -p aerodesk-agent

REC="$(mktemp -d)"
ROOM="sfu-q-$(date +%s)"
# 独立端口避免与本机其它 agent 冲突（沿用旧版 145xx 段；SIP 面 14560）
OPS_PORT=14501; SFU_INT=14502; SFU_SIG=14500; SFU_MEDIA=14578; SIP_PORT=14560

echo "== 启动 SFU（MAX_ROOM_CLIENTS=1）+ signal（SIP_UDP_PORT=${SIP_PORT}）"
RECORD_DIR="$REC" MAX_ROOM_CLIENTS=1 \
  SFU_MEDIA_PORT="$SFU_MEDIA" SFU_SIGNAL_PORT="$SFU_SIG" SFU_INTERNAL_PORT="$SFU_INT" \
  "$TARGET_DIR/aerodesk-sfu" >/tmp/sfuq-sfu.log 2>&1 &
SFU_PID=$!
SIGNAL_OPS_PORT="$OPS_PORT" SFU_URL="http://127.0.0.1:${SFU_INT}" \
  SIP_UDP_PORT="$SIP_PORT" "$TARGET_DIR/aerodesk-signal" >/tmp/sfuq-sig.log 2>&1 &
SIG_PID=$!
ok=0
for _ in $(seq 1 50); do
  (exec 3<>/dev/tcp/127.0.0.1/"$SFU_INT") 2>/dev/null \
    && grep -q "SIP/UDP 监听已起" /tmp/sfuq-sig.log 2>/dev/null && { ok=1; break; }
  sleep 0.2
done
[ "$ok" = "1" ] || fail "服务未就绪（见 /tmp/sfuq-sfu.log /tmp/sfuq-sig.log）"

echo "== viewer1 入会（应成功：SFU 房间第 1 个客户端）"
AERO_SIP_PORT="$SIP_PORT" "$TARGET_DIR/aerodesk-agent" --role viewer \
  --signal "ws://127.0.0.1:${OPS_PORT}" --room "$ROOM" >/tmp/sfuq-v1.log 2>&1 &
V1=$!
ok=0
for _ in $(seq 1 60); do
  grep -q "ICE connected" /tmp/sfuq-v1.log 2>/dev/null && { ok=1; break; }
  grep -q "session error" /tmp/sfuq-v1.log 2>/dev/null && break
  sleep 0.5
done
[ "$ok" = "1" ] || fail "viewer1 未接通会议（见 /tmp/sfuq-v1.log）"
grep -q "POST /start room=$ROOM" /tmp/sfuq-sfu.log || fail "viewer1 未经会议桥触达 SFU /start"
echo "  viewer1 已入会（会议桥 → SFU /start → ICE connected）"

echo "== viewer2 入会（应被配额拒绝：503）"
AERO_SIP_PORT="$SIP_PORT" "$TARGET_DIR/aerodesk-agent" --role viewer \
  --signal "ws://127.0.0.1:${OPS_PORT}" --room "$ROOM" >/tmp/sfuq-v2.log 2>&1 &
V2=$!
ok=0
for _ in $(seq 1 60); do
  grep -q "呼叫被拒（503）" /tmp/sfuq-v2.log 2>/dev/null && { ok=1; break; }
  sleep 0.5
done
[ "$ok" = "1" ] || fail "viewer2 未收到 503（见 /tmp/sfuq-v2.log）"

echo "== 断言"
grep -q "reject /start room=$ROOM: room full" /tmp/sfuq-sfu.log \
  || fail "SFU 未记录配额拒绝（room full）"
grep -q "SFU 桥接失败" /tmp/sfuq-sig.log || fail "signal 未透传 503（会议桥报错日志缺失）"
echo "  SFU room full → signal SIP 503 → 客户端「呼叫被拒（503）」全链符合预期"

echo "== SFU /start 准入配额 PASS（MAX_ROOM_CLIENTS=1：第 2 个会议客户端被 503）=="
