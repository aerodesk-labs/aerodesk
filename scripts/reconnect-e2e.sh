#!/usr/bin/env bash
# reconnect-e2e.sh —— 客户端自动重连（#173）：
#   Phase A 中途断线：viewer 收流 → 杀 SFU+signal → 重启 → viewer 自动重连并恢复解码帧
#   Phase B 启动重试：先起 SFU + viewer（signal 未起 → 连接失败 → 退避重试），后起 signal → 连上
# 独立端口避免与本机其它 agent 冲突；PID 文件管理避免误杀他人进程。
set -euo pipefail
cd "$(dirname "$0")/.."
export RUST_LOG="${RUST_LOG:-info}"

echo "== 构建"
cargo build -q -p aerodesk-sfu -p aerodesk-signal -p aerodesk-agent

REC="$(mktemp -d)"
SFU_TOK="rec-token"

start_sfu() {
    INTERNAL_TOKEN="$SFU_TOK" RECORD_DIR="$REC" \
      SFU_MEDIA_PORT=14578 SFU_SIGNAL_PORT=14500 SFU_INTERNAL_PORT=14502 \
      ./target/debug/aerodesk-sfu >/tmp/rec-sfu.log 2>&1 &
    echo $! > /tmp/rec-sfu.pid
}
start_signal() {
    SIGNAL_PORT=14501 SFU_URL=http://127.0.0.1:14502 SFU_TOKEN="$SFU_TOK" \
      SIP_UDP_PORT=5060 ./target/debug/aerodesk-signal >/tmp/rec-sig.log 2>&1 &
    echo $! > /tmp/rec-sig.pid
}
stop_services() {
    if [ -f /tmp/rec-sfu.pid ]; then kill "$(cat /tmp/rec-sfu.pid)" 2>/dev/null || true; rm -f /tmp/rec-sfu.pid; fi
    if [ -f /tmp/rec-sig.pid ]; then kill "$(cat /tmp/rec-sig.pid)" 2>/dev/null || true; rm -f /tmp/rec-sig.pid; fi
    # 等端口真正释放（优雅关闭可能需 ~1s，否则重启会 AddrInUse）
    for p in 14578 14502 14503; do
        for _ in $(seq 1 50); do
            nc -z 127.0.0.1 "$p" 2>/dev/null || break
            sleep 0.2
        done
    done
}
wait_port() { # wait_port <port>
    for _ in $(seq 1 50); do nc -z 127.0.0.1 "$1" 2>/dev/null && return 0; sleep 0.2; done
    return 1
}

fail=0

echo "== Phase A：中途断线自动重连"
start_sfu; start_signal
wait_port 14502 && wait_port 14503 || { echo "FAIL A: 服务未就绪"; exit 1; }
ROOM="rec-$(date +%s)"
./target/debug/aerodesk-agent --role publisher --encoder x264 --noisy \
    --signal ws://127.0.0.1:14503 --room "$ROOM" >/tmp/rec-pub.log 2>&1 &
PUB=$!
./target/debug/aerodesk-agent --role viewer --reconnect --reconnect-max 5 \
    --signal ws://127.0.0.1:14503 --room "$ROOM" >/tmp/rec-view.log 2>&1 &
VIEW=$!
FRAMES=0
for _ in $(seq 1 60); do
    F=$(grep -oE 'RECEIVED: [0-9]+' /tmp/rec-view.log 2>/dev/null | tail -1 | awk '{print $2}' || echo 0)
    [ "${F:-0}" -gt 5 ] && { FRAMES=$F; break; }
    sleep 0.5
done
if [ "$FRAMES" -le 5 ]; then
    echo "FAIL A: viewer 初始未收到帧"; tail -5 /tmp/rec-view.log; fail=1
else
    echo "PASS A: 初始收帧 $FRAMES"
fi
# 杀服务 + publisher（#553 SIP 1:1：P2P 直连不经服务——viewer 只在对端
# （publisher）死亡时断线，重连语义需连 publisher 一起杀）。
kill "$PUB" 2>/dev/null || true
wait "$PUB" 2>/dev/null || true
stop_services
sleep 3
start_sfu; start_signal
wait_port 14502 && wait_port 14503 || { echo "FAIL A: 重启后服务未就绪"; fail=1; }
# 重启 publisher（旧发布端已随杀服务窗口终止，viewer 的对端死亡 → 断线重连）
./target/debug/aerodesk-agent --role publisher --encoder x264 --noisy \
    --signal ws://127.0.0.1:14503 --room "$ROOM" >/tmp/rec-pub2.log 2>&1 &
PUB2=$!
RECON=0
for _ in $(seq 1 90); do
    N=$(grep -c 'SDP negotiated' /tmp/rec-view.log 2>/dev/null || echo 0)
    [ "$N" -ge 2 ] && { RECON=1; break; }
    sleep 0.5
done
[ "$RECON" = "1" ] && echo "PASS A: viewer 自动重连（SDP negotiated x2）" || { echo "FAIL A: 未重连"; tail -8 /tmp/rec-view.log; fail=1; }
FRAMES2=0
for _ in $(seq 1 60); do
    F=$(grep -oE 'RECEIVED: [0-9]+' /tmp/rec-view.log 2>/dev/null | tail -1 | awk '{print $2}' || echo 0)
    [ "${F:-0}" -gt "$FRAMES" ] && { FRAMES2=$F; break; }
    sleep 0.5
done
if [ "$FRAMES2" -gt "$FRAMES" ]; then
    echo "PASS A: 重连后恢复收帧 $FRAMES -> $FRAMES2"
else
    echo "FAIL A: 重连后未恢复收帧"; tail -5 /tmp/rec-view.log; fail=1
fi
kill $PUB $VIEW $PUB2 2>/dev/null || true
stop_services

echo "== Phase B：启动重试（signal 后起）"
start_sfu
wait_port 14502
ROOM2="rec-b-$(date +%s)"
./target/debug/aerodesk-agent --role viewer --reconnect --reconnect-max 8 \
    --signal ws://127.0.0.1:14503 --room "$ROOM2" >/tmp/recb-view.log 2>&1 &
VIEWB=$!
sleep 2
start_signal
wait_port 14503
./target/debug/aerodesk-agent --role publisher --encoder x264 --noisy \
    --signal ws://127.0.0.1:14503 --room "$ROOM2" >/tmp/recb-pub.log 2>&1 &
PUBB=$!
OK=0
for _ in $(seq 1 60); do
    grep -q 'SDP negotiated' /tmp/recb-view.log 2>/dev/null && { OK=1; break; }
    sleep 0.5
done
[ "$OK" = "1" ] && echo "PASS B: 启动重试成功（signal 后起也能连上）" || { echo "FAIL B"; tail -8 /tmp/recb-view.log; fail=1; }
kill $VIEWB $PUBB 2>/dev/null || true
stop_services
wait 2>/dev/null || true
exit $fail
