#!/usr/bin/env bash
# multipop-e2e.sh —— 多 PoP 房间路由 v2（P3 SIP 单栈 302+Contact 版，#146/#154）。
#
# 拓扑（双 PoP 各自显式 SIP UDP 端口）：
#   PoP A: sfu-a(media 3478/internal 3002) + signal-a(SIP/UDP 5060 / ops 3001)
#   PoP B: sfu-b(media 3479/internal 3007) + signal-b(SIP/UDP 5070 / ops 3006)
#   房间归属经共享注册表预登记为 pop-b（静态钉住的 P3 等价物——静态前缀表已随
#   JSON 面退役）；POP_SIP_URLS="pop-b=127.0.0.1:5070" 提供 302 Contact 载体。
# viewer 经 signal-a（SIP/UDP 5060）INVITE eu-* 房间 → 注册表命中 pop-b →
# 服务端回 302+Contact(<sip:room@127.0.0.1:5070>) → 客户端跟随重 INVITE PoP-B
# → 会议桥入 sfu-b。
#
# 注意：客户端 302 跟随（RedirectedTo/redirect_target）随 #600 合并——此前本脚本
# 只能验证到「服务端 302 决策 + Contact 发送」，CI 维持 if:false。
set -euo pipefail
cd "$(dirname "$0")/.."

ROOM="eu-room-$(date +%s)"
REG="/tmp/multipop-e2e-$(date +%s).json"
export RUST_LOG="${RUST_LOG:-info}"

echo "== 构建"
cargo build -q -p aerodesk-sfu -p aerodesk-signal -p aerodesk-agent

# 预登记房间归属（镜像另一 PoP 已登记的共享注册表形态）。
NOW=$(date +%s)
printf '{"%s":{"pop":"pop-b","updated_at":%s}}\n' "$ROOM" "$NOW" > "$REG"

REC="$(mktemp -d)"
echo "== 启动 PoP A + PoP B"
RECORD_DIR="$REC/a" ./target/debug/aerodesk-sfu >/tmp/mpop-sfu-a.log 2>&1 &
SFU_A=$!
SFU_MEDIA_PORT=3479 SFU_SIGNAL_PORT=3005 SFU_INTERNAL_PORT=3007 RECORD_DIR="$REC/b" ./target/debug/aerodesk-sfu >/tmp/mpop-sfu-b.log 2>&1 &
SFU_B=$!
SIGNAL_OPS_PORT=3001 SIP_UDP_PORT=5060 POP_ID=pop-a POP_REGISTRY_FILE="$REG" \
  POP_SIP_URLS="pop-b=127.0.0.1:5070" SFU_URL=http://127.0.0.1:3002 \
  ./target/debug/aerodesk-signal >/tmp/mpop-sig-a.log 2>&1 &
SIG_A=$!
SIGNAL_OPS_PORT=3006 SIP_UDP_PORT=5070 POP_ID=pop-b POP_REGISTRY_FILE="$REG" \
  SFU_URL=http://127.0.0.1:3007 \
  ./target/debug/aerodesk-signal >/tmp/mpop-sig-b.log 2>&1 &
SIG_B=$!

for _ in $(seq 1 50); do
    if nc -z 127.0.0.1 3002 2>/dev/null && nc -z 127.0.0.1 3007 2>/dev/null \
       && nc -z 127.0.0.1 3001 2>/dev/null && nc -z 127.0.0.1 3006 2>/dev/null; then
        break
    fi
    sleep 0.2
done
sleep 0.3

echo "== publisher 经 signal-a 发布房间（应被 302 引导到 pop-b）"
AERO_SIP_PORT=5060 ./target/debug/aerodesk-agent --role publisher --encoder x264 --noisy \
    --signal ws://127.0.0.1:3001 --room "$ROOM" >/tmp/mpop-pub.log 2>&1 &
PUB=$!
AERO_SIP_PORT=5060 ./target/debug/aerodesk-agent --role viewer \
    --signal ws://127.0.0.1:3001 --room "$ROOM" >/tmp/mpop-view.log 2>&1 &
VIEW=$!
sleep 10
kill "$PUB" "$VIEW" "$SFU_A" "$SFU_B" "$SIG_A" "$SIG_B" 2>/dev/null || true
wait 2>/dev/null || true

echo "== 断言"
fail=0
# 1) signal-a 302 决策（房间归属 pop-b，措辞沿用「room ... -> pop pop-b」）
if grep -qE "room .* -> pop pop-b" /tmp/mpop-sig-a.log; then
    echo "PASS signal-a redirect eu-* -> pop-b"
else
    echo "FAIL no redirect in signal-a"; tail -5 /tmp/mpop-sig-a.log; fail=1
fi
# 2) signal-a 发出 302+Contact（P3 事务层路径）
if grep -q "302 redirect" /tmp/mpop-sig-a.log; then
    echo "PASS signal-a sent 302+Contact"
else
    echo "FAIL no 302+Contact sent"; tail -5 /tmp/mpop-sig-a.log; fail=1
fi
# 3) 目标 PoP 受理 REGISTER（客户端 #600 跟随后重注册 PoP-B）
if grep -q "SIP 注册" /tmp/mpop-sig-b.log; then
    echo "PASS client registered to signal-b (pop-b)"
else
    echo "FAIL no registration on signal-b"; tail -5 /tmp/mpop-sig-b.log; fail=1
fi
# 4) 媒体经 PoP-B 会议桥到达 sfu-b（ICE 建链 + 解码帧）
if grep -q "ICE connected" /tmp/mpop-view.log \
   && grep -qE "DECODED: [1-9]" /tmp/mpop-view.log; then
    echo "PASS media session on sfu-b via pop-b (ICE + decoded frames)"
else
    echo "FAIL no media on sfu-b"; tail -8 /tmp/mpop-view.log; fail=1
fi
# 5) 无 panic
if grep -qiE "panic" /tmp/mpop-*.log; then
    echo "FAIL panic in logs"; fail=1
else
    echo "PASS no panics"
fi
rm -f "$REG"
exit $fail
