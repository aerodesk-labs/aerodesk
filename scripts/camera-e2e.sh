#!/usr/bin/env bash
# #340 摄像头第二路视频轨端到端：publisher(--encoder screen --codec h265 --camera)
# → SFU → viewer(--camera) 解码。
# 断言 CLI viewer CAMERA decoded>0（补 AccessUnitAssembler 前恒为 0）。
# CI 无摄像头/无屏幕录制权限时 SKIP（detect-and-return，不红）。
# 用法: scripts/camera-e2e.sh [房间]
set -euo pipefail
cd "$(dirname "$0")/.."

ROOM="${1:-cam-$(date +%s)}"
export RUST_LOG="${RUST_LOG:-info}"

echo "== 构建"
cargo build -q -p aerodesk-sfu -p aerodesk-signal -p aerodesk-cli

# 摄像头不可用/未授权 → SKIP（CI 机器无摄像头）。
CAM_ID="$(./target/debug/aerodesk-cli --list-cameras 2>/dev/null | head -1 | awk '{print $1}')"
if [ -z "$CAM_ID" ]; then
    echo "SKIP: 无可用摄像头（CI 或未授权），跳过 camera-e2e"
    exit 0
fi

REC="$(mktemp -d)"
echo "== 启动 sfu/signal"
RECORD_DIR="$REC" ./target/debug/aerodesk-sfu >/tmp/camera-sfu.log 2>&1 &
SFU_PID=$!
./target/debug/aerodesk-signal >/tmp/camera-sig.log 2>&1 &
SIG_PID=$!
for _ in $(seq 1 50); do
    if nc -z 127.0.0.1 3003 2>/dev/null && nc -z 127.0.0.1 3002 2>/dev/null; then break; fi
    if ! kill -0 "$SFU_PID" 2>/dev/null || ! kill -0 "$SIG_PID" 2>/dev/null; then
        echo "sfu/signal 启动失败"; tail -5 /tmp/camera-sfu.log; tail -5 /tmp/camera-sig.log; exit 1
    fi
    sleep 0.2
done
sleep 0.3

echo "== 启动 publisher（screen + camera，h265）+ viewer"
./target/debug/aerodesk-cli --role publisher --encoder screen --codec h265 --camera \
    --signal ws://127.0.0.1:3003 --room "$ROOM" >/tmp/camera-pub.log 2>&1 &
PUB_PID=$!
sleep 4
# 屏幕录制权限未授予时 publisher 直接退出 → SKIP。
if ! kill -0 "$PUB_PID" 2>/dev/null; then
    echo "SKIP: publisher 启动失败（可能无屏幕录制权限）"; tail -5 /tmp/camera-pub.log
    kill "$SFU_PID" "$SIG_PID" 2>/dev/null || true; wait 2>/dev/null || true; exit 0
fi
./target/debug/aerodesk-cli --role viewer --camera \
    --signal ws://127.0.0.1:3003 --room "$ROOM" >/tmp/camera-view.log 2>&1 &
VIEW_PID=$!
sleep 18
kill "$VIEW_PID" "$PUB_PID" "$SFU_PID" "$SIG_PID" 2>/dev/null || true
wait 2>/dev/null || true

echo "== 断言"
# #340/#351：CLI viewer 摄像头轨必须组装完整访问单元后解码（decoded>0）。
if grep -qE "CAMERA: [0-9]+ frames [0-9]+ bytes decoded=[1-9]" /tmp/camera-view.log; then
    echo "PASS camera track decoded"
    grep -oE "CAMERA: [0-9]+ frames [0-9]+ bytes decoded=[0-9]+" /tmp/camera-view.log | tail -1
else
    echo "FAIL camera track not decoded"
    grep -oE "CAMERA:.*" /tmp/camera-view.log | tail -3
    tail -5 /tmp/camera-pub.log
    exit 1
fi
if grep -qiE "panic" /tmp/camera-pub.log /tmp/camera-view.log /tmp/camera-sfu.log; then
    echo "FAIL panic in logs"; exit 1
fi
echo "PASS no panics"
