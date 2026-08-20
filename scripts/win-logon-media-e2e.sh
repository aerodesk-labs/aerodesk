#!/usr/bin/env bash
# #471 M2 端到端:登录界面媒体链路(合成源)——服务体(--service-fg --force-media)
# 发布 H264 → SFU → viewer 收帧断言。真机采集源(S0/helper)按实测矩阵 A/B
# 另行人工联调(docs/PRELOGIN_WINLOGON_CAPTURE.md §4)。
# 前置:cargo build -p aerodesk-host -p aerodesk-signal -p aerodesk-sfu -p aerodesk-agent
# (FFmpeg DLL 需在 PATH,见 ci.yml Windows 步骤)。
set -euo pipefail
cd "$(dirname "$0")/.."
BIN="${AERODESK_HOST:-target/debug/aerodesk-host.exe}"
# #522 审查：viewer 断言必须用 aerodesk-agent（host 无 --role viewer 入口，旧
# 脚本用 $BIN 当 viewer 必失败）。
CLI_BIN="${AERODESK_AGENT:-target/debug/aerodesk-agent.exe}"
ROOM="logon-e2e-$$"
CONF="$PROGRAMDATA/AeroDesk/service-settings.json"
[ -f "$BIN" ] || { echo "未找到 $BIN" >&2; exit 1; }
[ -f "$CLI_BIN" ] || { echo "未找到 $CLI_BIN" >&2; exit 1; }

cleanup() {
  kill ${FG_PID:-} ${SIG_PID:-} ${SFU_PID:-} 2>/dev/null || true
  rm -f "$CONF"
}
trap cleanup EXIT

echo "== 写服务配置(room=$ROOM,合成源)"
mkdir -p "$(dirname "$CONF")"
echo "{\"server\":\"ws://127.0.0.1:3003/ws\",\"device_id\":\"$ROOM\"}" > "$CONF"

echo "== 启动 sfu/signal"
# 本机/CI 网卡环境可能导致自动通告地址不可绑,显式通配绑定。
SFU_BIND_ADDRESS=0.0.0.0 ./target/debug/aerodesk-sfu.exe >/tmp/logon-sfu.log 2>&1 &
SFU_PID=$!
./target/debug/aerodesk-signal.exe >/tmp/logon-sig.log 2>&1 &
SIG_PID=$!
sleep 2

echo "== 服务体(--service-fg --force-media,合成源发布)"
"$BIN" --service-fg --force-media >/tmp/logon-fg.log 2>&1 &
FG_PID=$!
sleep 6
grep -q "ICE connected" /tmp/logon-fg.log || { echo "FAIL 服务体媒体未连接"; tail -20 /tmp/logon-fg.log; exit 1; }

echo "== viewer 收流断言"
OUT=$(timeout 12 "$CLI_BIN" --role viewer --signal ws://127.0.0.1:3003 --room "$ROOM" 2>&1 | grep "RECEIVED" | tail -1)
echo "$OUT"
FRAMES=$(echo "$OUT" | sed -E 's/.*RECEIVED: ([0-9]+) frames.*/\1/')
[ -n "$FRAMES" ] && [ "$FRAMES" -gt 0 ] || { echo "FAIL viewer 0 帧"; exit 1; }
echo "PASS:登录界面媒体端到端(viewer 收到 ${FRAMES} 帧)"
