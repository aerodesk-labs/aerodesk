#!/usr/bin/env bash
# #75 输入注入链路：viewer 发输入事件 → SFU → publisher 注入（macOS CGEvent）。
# CI 无辅助功能权限时 CGEvent 静默，但注入路径与日志可验证。
# 用法: scripts/input-e2e.sh [房间]
set -euo pipefail
cd "$(dirname "$0")/.."

ROOM="${1:-input-$(date +%s)}"
export RUST_LOG="${RUST_LOG:-info}"

echo "== 构建"
cargo build -q -p aerodesk-sfu -p aerodesk-signal -p aerodesk-agent

REC="$(mktemp -d)"
echo "== 启动 sfu/signal"
RECORD_DIR="$REC" ./target/debug/aerodesk-sfu >/tmp/input-sfu.log 2>&1 &
SFU_PID=$!
SIP_UDP_PORT=5060 ./target/debug/aerodesk-signal >/tmp/input-sig.log 2>&1 &
SIG_PID=$!
for _ in $(seq 1 50); do
    if nc -z 127.0.0.1 3003 2>/dev/null && nc -z 127.0.0.1 3002 2>/dev/null; then break; fi
    if ! kill -0 "$SFU_PID" 2>/dev/null || ! kill -0 "$SIG_PID" 2>/dev/null; then
        echo "sfu/signal 启动失败"; tail -5 /tmp/input-sfu.log; tail -5 /tmp/input-sig.log; exit 1
    fi
    sleep 0.2
done
sleep 0.3

echo "== 启动 publisher + viewer"
./target/debug/aerodesk-agent --role publisher --encoder x264 \
    --signal ws://127.0.0.1:3003 --room "$ROOM" >/tmp/input-pub.log 2>&1 &
PUB_PID=$!
# --input-script：#75 脚本化轮换发送全部事件类型（MouseMove/Button/Wheel/Key+修饰键）。
./target/debug/aerodesk-agent --role viewer --input-script \
    --signal ws://127.0.0.1:3003 --room "$ROOM" >/tmp/input-view.log 2>&1 &
VIEW_PID=$!
# 等待输入链路完成首轮事件轮换（Key 出现即 Move/Button/Wheel 均已发出，
# 上限 ~8s）；进程异常退出时立即收尾。固定 sleep 在 CI 偶发偏短，改为就绪轮询更稳。
for _ in $(seq 1 40); do
    if grep -qE "inject: seq=.*Key" /tmp/input-pub.log 2>/dev/null; then break; fi
    if ! kill -0 "$PUB_PID" 2>/dev/null || ! kill -0 "$VIEW_PID" 2>/dev/null; then break; fi
    sleep 0.2
done
kill "$PUB_PID" "$VIEW_PID" "$SFU_PID" "$SIG_PID" 2>/dev/null || true
wait 2>/dev/null || true

echo "== 断言"
fail=0
# #75：viewer --input-script 轮换发送全部事件类型 → publisher 注入路径
# （inject 日志证明收到+尝试注入；CI 无辅助功能权限时 CGEvent 静默但路径可证）。
for evt in MouseMove MouseButton Wheel Key; do
    if grep -qE "inject: seq=.*$evt" /tmp/input-pub.log; then
        echo "PASS input event type reached inject: $evt"
    else
        echo "FAIL input event type missing: $evt"; grep "inject" /tmp/input-pub.log | tail -3; fail=1
    fi
done

# #75 坐标值断言：归一化坐标必须原样穿越 SFU + 注入计算（CGEvent 可能因无
# 辅助功能权限静默，但注入前的归一化坐标/增量可验证，且不依赖真实显示器）。
# --input-script 固定发送 MouseMove(0.3,0.4)、Button/Wheel(0.5,0.5)、delta_y=-3。
coords=(
    "MouseMove { x: 0.3, y: 0.4 }"
    "MouseButton { button: Left, state: Pressed, x: 0.5, y: 0.5 }"
    "MouseButton { button: Left, state: Released, x: 0.5, y: 0.5 }"
    "Wheel { x: 0.5, y: 0.5, delta_x: 0.0, delta_y: -3.0 }"
)
for coord in "${coords[@]}"; do
    if grep -qF "$coord" /tmp/input-pub.log; then
        echo "PASS input coord carried: $coord"
    else
        echo "FAIL input coord missing: $coord"; grep "inject" /tmp/input-pub.log | tail -3; fail=1
    fi
done
if ! grep -qE "inject: seq=.*Key.*ctrl: true" /tmp/input-pub.log; then
    echo "FAIL key modifiers (ctrl) not carried"; grep "inject: seq=.*Key" /tmp/input-pub.log | tail -3; fail=1
else
    echo "PASS key modifiers (ctrl) carried through"
fi
if grep -qiE "panic" /tmp/input-pub.log /tmp/input-view.log /tmp/input-sfu.log; then
    echo "FAIL panic in logs"; fail=1
else
    echo "PASS no panics"
fi
exit $fail
