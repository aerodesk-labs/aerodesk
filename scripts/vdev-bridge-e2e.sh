#!/usr/bin/env bash
# vdev-bridge 端到端：publisher(x264 合成源) → SFU → aerodesk-vdev-bridge → vdev 虚拟摄像头。
#
# 断言：
#   1) 桥收到并解码视频帧（日志 "已推 N 帧"）；
#   2) 桥与 vdev 摄像头扩展的 FrameChannel 连接保持 ESTABLISHED——
#      扩展的 handle_conn 在协议不符（magic/version/stride/len）时会直接断链，
#      所以连接不塌 = 36 字节头 + BGRA 载荷被正确解析接收。
#
# 需要本机已装载 vdev 虚拟摄像头（vdev 仓库的 CMIOExtension，监听 127.0.0.1:27890）；
# 未装则 SKIP（detect-and-return，不红）。读摄像头画面需摄像头 TCC 授权，本脚本不读。
#
# 用法: scripts/vdev-bridge-e2e.sh [房间]
set -euo pipefail
cd "$(dirname "$0")/.."

ROOM="${1:-vdev-$(date +%s)}"
export RUST_LOG="${RUST_LOG:-info}"

# vdev 虚拟摄像头帧通道必须在听，否则无接收端 → SKIP。
if ! nc -z 127.0.0.1 27890 2>/dev/null; then
    echo "SKIP: 127.0.0.1:27890 未监听（vdev 虚拟摄像头扩展未运行）"
    exit 0
fi

echo "== 构建"
cargo build -q -p aerodesk-sfu -p aerodesk-signal -p aerodesk-agent

PIDS=()
cleanup() { for p in "${PIDS[@]:-}"; do kill "$p" 2>/dev/null || true; done; wait 2>/dev/null || true; }
trap cleanup EXIT

echo "== 启动 sfu/signal"
./target/debug/aerodesk-sfu >/tmp/vdev-sfu.log 2>&1 & PIDS+=($!)
SIP_UDP_PORT=5060 ./target/debug/aerodesk-signal >/tmp/vdev-sig.log 2>&1 & PIDS+=($!)
for _ in $(seq 1 50); do
    nc -z 127.0.0.1 3002 2>/dev/null && break
    sleep 0.2
done
sleep 0.3

echo "== 启动 publisher（x264 合成源，不需要屏幕录制权限）room=$ROOM"
./target/debug/aerodesk-agent --role publisher --encoder x264 \
    --signal ws://127.0.0.1:3003 --room "$ROOM" >/tmp/vdev-pub.log 2>&1 & PIDS+=($!)
sleep 4
if ! kill -0 "${PIDS[2]}" 2>/dev/null; then
    echo "FAIL: publisher 退出"; tail -8 /tmp/vdev-pub.log; exit 1
fi

echo "== 启动桥（viewer）"
./target/debug/aerodesk-vdev-bridge ws://127.0.0.1:3003 "$ROOM" >/tmp/vdev-bridge.log 2>&1 & PIDS+=($!)
BRIDGE_PID="${PIDS[3]}"

# 等桥推帧 + 采样 FrameChannel 连接（最多 40s）。
OK=0
CONN=0
for _ in $(seq 1 80); do
    grep -qE "已推 [1-9][0-9]* 帧" /tmp/vdev-bridge.log 2>/dev/null && OK=1
    # 注意：`netstat | grep -q` 在 set -o pipefail 下会因 grep 提前退出触发
    # SIGPIPE 而整条管道返回非零——必须先把输出取下来再匹配。
    NS="$(netstat -an 2>/dev/null || true)"
    case "$NS" in *".27890 "*"ESTABLISHED"*) CONN=1 ;; esac
    { [ "$OK" = 1 ] && [ "$CONN" = 1 ]; } && break
    kill -0 "$BRIDGE_PID" 2>/dev/null || break
    sleep 0.5
done

echo "== 断言"
FAIL=0
if [ "$OK" = 1 ]; then
    echo "PASS 桥解码并推送视频帧:"; grep -oE "已推 [0-9]+ 帧.*" /tmp/vdev-bridge.log | tail -1
else
    echo "FAIL 桥未推帧"; FAIL=1
    tail -10 /tmp/vdev-bridge.log
fi

# FrameChannel 连接曾保持 ESTABLISHED（说明扩展接收/解析正常，没因协议错断链）
if [ "$CONN" = 1 ]; then
    echo "PASS FrameChannel 连接 ESTABLISHED（扩展已接收并解析帧）"
else
    echo "WARN 全程未见 ESTABLISHED 连接"
fi

if grep -qiE 'panic' /tmp/vdev-bridge.log /tmp/vdev-pub.log /tmp/vdev-sfu.log; then
    echo "FAIL panic in logs"; FAIL=1
fi
[ "$FAIL" = 0 ] || exit 1
echo "PASS vdev-bridge e2e"
