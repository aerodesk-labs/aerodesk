#!/usr/bin/env bash
# #598 P0：标准 SIP 客户端（RFC 7118 WSS）端到端验收线。
# 起signal（SIP_WSS_PORT=3061 + SIP_UDP_PORT=5060 + 显式 Digest 用户），
# 跑 scripts/sip-accept-wss.py 完整呼叫闭环：REGISTER(Digest)×2 → INVITE →
# 100+200(SDP answer) → ACK。此前该脚本零调用方（web-sip-wss-design.md §5 任务8
# 前置）——浏览器 SIP 化（P2）依赖此线的报文形态背书。
# 依赖：python3 + websockets 包（缺失时自动 pip 安装）、nc。
set -euo pipefail
ROOT="$(cd "$(dirname "$0")/.." && pwd)"
cd "$ROOT"

echo "== [1/4] 构建 signal"
cargo build -q -p aerodesk-signal

echo "== [2/4] 启动 signal（SIP 双传输 + 显式 Digest 用户）"
SIP_WSS_PORT=3061 SIP_UDP_PORT=5060 \
  SIP_DIGEST_USERS="accept-wss-a=pass-a,accept-wss-b=pass-b" \
  "$ROOT/target/debug/aerodesk-signal" >/tmp/sip-accept-wss-sig.log 2>&1 &
SIG=$!
trap 'kill "$SIG" 2>/dev/null || true' EXIT
OK=0
for _ in $(seq 1 50); do
    # WSS 监听是 TLS-over-TCP，TCP connect 可探；SIP/UDP 无连接不可探。
    # 用 bash 内建 /dev/tcp 而非 nc：Windows(Git Bash)/Linux runner 通吃。
    if grep -q "SIP 信令端点已启动" /tmp/sip-accept-wss-sig.log 2>/dev/null \
        && (exec 3<>/dev/tcp/127.0.0.1/3061) 2>/dev/null; then OK=1; break; fi
    sleep 0.2
done
if [ "$OK" != "1" ]; then echo "FAIL: signal 未就绪"; tail -20 /tmp/sip-accept-wss-sig.log; exit 1; fi

echo "== [3/4] 校验 python websockets 依赖"
if ! python3 -c 'import websockets' 2>/dev/null; then
    python3 -m pip install --quiet websockets 2>/dev/null \
        || python3 -m pip install --quiet --break-system-packages websockets
fi

echo "== [4/4] 标准客户端 WSS 呼叫闭环"
python3 scripts/sip-accept-wss.py

# signal 无 panic（script 内 FAIL 会 exit 1 到不了这里）
if grep -qiE "panic" /tmp/sip-accept-wss-sig.log; then
    echo "FAIL: signal panic"; tail -20 /tmp/sip-accept-wss-sig.log; exit 1
fi

kill "$SIG" 2>/dev/null || true
trap - EXIT
echo "E2E DONE"
