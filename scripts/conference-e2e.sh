#!/usr/bin/env bash
# #598 v0.4 §4.1 多方升级链路 e2e：publisher（1:1 UAS） + 2 viewer → 302 升级 → 全员 SFU。
#
# 时序（docs/SIP_SIGNALING.md §4.1）：V2 INVITE 被控端 → 被控端 302（无 Contact，
# 对端确定性推导 view AoR）+ BYE(cause=302) 通知 V1 → 被控端转会议发布方向
# （INVITE view-AoR，SFU role=publisher）→ V1/V2 重拨 view-AoR 入会。
#
# 验证锚点：
#   publisher: "publisher: 收到升级信号"（1:1 结束，转会议发布方向）
#   viewer1:   "viewer: 收到升级信号" + "viewer: 跟随升级重拨会议 AoR（view-AD-CONF1）"
#   viewer2:   "viewer: 跟随升级重拨会议 AoR（view-AD-CONF1）"
#   signal:    "SIP 会议 INVITE → SFU 桥（方向判定）" 且 role=publisher 与 role=viewer 各至少一次
#   SFU:       view-AD-CONF1 房间 3 客户端入会；双方 viewer 经 SFU 收到 cursor（data channel）
#
# 用法: scripts/conference-e2e.sh [房间] [观察秒数]
set -uo pipefail
cd "$(dirname "$0")/.."

ROOM="${1:-AD-CONF1}"
VROOM="view-$ROOM"
OBS="${2:-12}"
# ffmpeg-sys-next 9 行为：FFMPEG_DIR 一旦「已设置」（含空串/不存在路径）即放弃
# pkg-config 直查该路径——历史默认值是提交者本机 Windows 路径，曾在 macOS 构建
# 失败。Windows CI 由 ci.yml 注入 BtbN 路径；本地开发用 AERO_FFMPEG_DIR 回退；
# 其余情况不导出（走 pkg-config/brew/apt 发现）。
FFMPEG_DIR="${FFMPEG_DIR:-${AERO_FFMPEG_DIR:-}}"
if [ -n "$FFMPEG_DIR" ] && [ ! -d "$FFMPEG_DIR" ]; then
    echo "WARN: FFMPEG_DIR=$FFMPEG_DIR 不存在，忽略（回退 pkg-config 发现）"
    FFMPEG_DIR=""
fi
if [ -n "$FFMPEG_DIR" ]; then export FFMPEG_DIR; else unset FFMPEG_DIR; fi

echo "== 构建"
cargo build -q -p aerodesk-sfu -p aerodesk-signal -p aerodesk-agent

REC="$(mktemp -d)"
SFU_LOG=/tmp/conf-sfu.log
SIG_LOG=/tmp/conf-sig.log
PUB_LOG=/tmp/conf-pub.log
V1_LOG=/tmp/conf-v1.log
V2_LOG=/tmp/conf-v2.log
rm -f "$SFU_LOG" "$SIG_LOG" "$PUB_LOG" "$V1_LOG" "$V2_LOG"

echo "== 启动 sfu/signal"
# SFU_HOST_ADDRESS=127.0.0.1：本机 e2e 通告环回候选（默认通告网卡 IP，回环
# 客户端 ICE 不可达——Windows 实测 SFU 侧 10049 发送失败、viewer ICE 超时）。
RECORD_DIR="$REC" SFU_HOST_ADDRESS=127.0.0.1 ./target/debug/aerodesk-sfu >"$SFU_LOG" 2>&1 &
SFU_PID=$!
SIP_UDP_PORT=5060 ./target/debug/aerodesk-signal >"$SIG_LOG" 2>&1 &
SIG_PID=$!
for _ in $(seq 1 50); do
    if grep -q "SIP/UDP 监听已起" "$SIG_LOG" 2>/dev/null; then break; fi
    if ! kill -0 "$SFU_PID" 2>/dev/null || ! kill -0 "$SIG_PID" 2>/dev/null; then
        echo "sfu/signal 启动失败"; tail -5 "$SFU_LOG"; tail -5 "$SIG_LOG"; exit 1
    fi
    sleep 0.2
done
sleep 0.3

cleanup() {
    kill "$PUB_PID" "$V1_PID" "$V2_PID" "$SFU_PID" "$SIG_PID" 2>/dev/null
    sleep 1
    kill -9 "$PUB_PID" "$V1_PID" "$V2_PID" "$SFU_PID" "$SIG_PID" 2>/dev/null
}
trap cleanup EXIT

echo "== 启动 publisher（pcap 1:1 UAS），等 SIP 注册"
./target/debug/aerodesk-agent --role publisher --signal ws://127.0.0.1:3003 --room "$ROOM" >"$PUB_LOG" 2>&1 &
PUB_PID=$!
OK=0
for _ in $(seq 1 30); do
    if grep -q "SIP registered" "$PUB_LOG" 2>/dev/null; then OK=1; break; fi
    sleep 0.5
done
if [ "$OK" != "1" ]; then
    echo "FAIL: publisher 未完成 SIP 注册"; tail -8 "$PUB_LOG"; exit 1
fi

echo "== 启动 viewer1（1:1 P2P）"
./target/debug/aerodesk-agent --role viewer --signal ws://127.0.0.1:3003 --room "$ROOM" --reconnect >"$V1_LOG" 2>&1 &
V1_PID=$!
sleep 3

echo "== 启动 viewer2（触发升级）"
./target/debug/aerodesk-agent --role viewer --signal ws://127.0.0.1:3003 --room "$ROOM" --reconnect >"$V2_LOG" 2>&1 &
V2_PID=$!
sleep "$OBS"

echo "=== 检查日志 ==="
FAIL=0
# 日志含 ANSI 色码，先剥色再匹配。
strip_ansi() { sed -r 's/\x1B\[[0-9;]*[mK]//g'; }
check() {
    if strip_ansi <"$1" | grep -q "$2"; then echo "PASS: $1 -> $2"; else echo "FAIL: $1 缺 $2"; FAIL=1; fi
}
check "$PUB_LOG" "publisher: 收到升级信号"
check "$V1_LOG" "viewer: 收到升级信号"
check "$V1_LOG" "viewer: 跟随升级重拨会议 AoR（$VROOM）"
check "$V2_LOG" "viewer: 跟随升级重拨会议 AoR（$VROOM）"
# 会议桥方向判定：发布端 sendrecv → publisher、观看端 recvonly → viewer。
if grep -q "SIP 会议 INVITE → SFU 桥（方向判定）" "$SIG_LOG"; then
    echo "PASS: signal 会议桥方向判定"
else
    echo "FAIL: signal 未见会议桥方向判定"; FAIL=1
fi
if strip_ansi <"$SIG_LOG" | grep -q "role=publisher"; then echo "PASS: 发布端判定 publisher"; else echo "FAIL: 缺 publisher 判定"; FAIL=1; fi
if strip_ansi <"$SIG_LOG" | grep -q "role=viewer"; then echo "PASS: 观看端判定 viewer"; else echo "FAIL: 缺 viewer 判定"; FAIL=1; fi
# SFU 会议房间三方入会。
JOINS=$(strip_ansi <"$SFU_LOG" | grep -c "joined room $VROOM" || true)
if [ "${JOINS:-0}" -ge 3 ]; then echo "PASS: SFU 房间 $VROOM 三方入会（$JOINS）"; else echo "FAIL: SFU 入会不足 3（$JOINS）"; FAIL=1; fi
# 会议内端到端媒体证据：viewer 经 SFU 收到 cursor（data channel 30Hz 轨迹）。
if strip_ansi <"$V1_LOG" | grep -qE "CURSOR: x="; then echo "PASS: viewer1 经 SFU 收到 cursor"; else echo "FAIL: viewer1 无 cursor（会议媒体未达）"; FAIL=1; fi
if strip_ansi <"$V2_LOG" | grep -qE "CURSOR: x="; then echo "PASS: viewer2 经 SFU 收到 cursor"; else echo "FAIL: viewer2 无 cursor（会议媒体未达）"; FAIL=1; fi
for f in "$SFU_LOG" "$SIG_LOG" "$PUB_LOG" "$V1_LOG" "$V2_LOG"; do
    if strip_ansi <"$f" | grep -qi "panic"; then echo "FAIL: $f 有 panic"; tail -5 "$f"; FAIL=1; fi
done

echo "=== 日志摘录 ==="
grep -hE "收到升级信号|跟随升级重拨" "$PUB_LOG" "$V1_LOG" "$V2_LOG" | head -5
grep -hE "方向判定" "$SIG_LOG" | head -4
grep -hE "joined room" "$SFU_LOG" | head -4
exit $FAIL
