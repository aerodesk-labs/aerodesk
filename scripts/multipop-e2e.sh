#!/usr/bin/env bash
# multipop-e2e.sh —— 多 PoP 房间路由 SIP 302+Contact 版（#600，#146/#154 延续）。
#
# 拓扑（双 PoP 显式端口，避免默认 TLS/WSS 互撞与残留进程污染）：
#   PoP A: sfu-a(media 3478/internal 3002) + signal-a(SIP UDP 5060 / TLS 5061 /
#          WSS 3061 / ops 3001)
#   PoP B: sfu-b(media 3479/internal 3007) + signal-b(SIP UDP 5070 / TLS 5071 /
#          WSS 3071 / ops 3006)
#   房间归属经共享注册表文件**预登记**为 pop-b（静态钉住形态）；A 的
#   POP_SIP_URLS="pop-b=127.0.0.1:5070" 提供 302 Contact 载体。
#
# 流程（#600 客户端 302 跟随已落地，全链 SIP）：
#   viewer 在 A REGISTER + INVITE 房间 → A 注册表命中 pop-b →
#   服务端 302+Contact(<sip:room@127.0.0.1:5070>) → 客户端跟随换拨 B →
#   B 会议桥 → sfu-b 建会话（无发布者 0 帧——换拨语义验证目标：成功连到
#   B 的会议桥，媒体断言见 web-conference-e2e.sh）。
#
# 注意：publisher 角色不参与本脚本——SIP 形态 publisher 是 UAS 被叫（注册
#   房间名为设备 AoR），INVITE 会被 registrar 命中走 1:1 转发，**绕过** 302
#   决策（旧 JSON「publisher join 触发重定向」语义在 SIP 下不存在）。
set -euo pipefail
# 残留清理：双 PoP 端口 + 前序 e2e 残留 signal 会致 bind 失败（CI 实测
# WSS/TLS AddrInUse），taskkill（Windows）与 pkill（macOS）双保险。
taskkill //F //IM aerodesk-signal.exe 2>/dev/null || true
taskkill //F //IM aerodesk-sfu.exe 2>/dev/null || true
pkill -f 'target/debug/aerodesk-signal' 2>/dev/null || true
pkill -f 'target/debug/aerodesk-sfu' 2>/dev/null || true
cd "$(dirname "$0")/.."

ROOM="eu-room-$(date +%s)"
REG="/tmp/multipop-e2e-$(date +%s).json"
export RUST_LOG="${RUST_LOG:-info}"

echo "== 构建"
cargo build -q -p aerodesk-sfu -p aerodesk-signal -p aerodesk-agent

# 预登记房间归属（镜像多 PoP 共享注册表形态：room → pop-b 静态钉住）。
NOW=$(date +%s)
printf '{"%s":{"pop":"pop-b","updated_at":%s}}\n' "$ROOM" "$NOW" > "$REG"

REC="$(mktemp -d)"
echo "== 启动 PoP A + PoP B"
RECORD_DIR="$REC/a" ./target/debug/aerodesk-sfu >/tmp/mpop-sfu-a.log 2>&1 &
SFU_A=$!
SFU_MEDIA_PORT=3479 SFU_SIGNAL_PORT=3005 SFU_INTERNAL_PORT=3007 RECORD_DIR="$REC/b" ./target/debug/aerodesk-sfu >/tmp/mpop-sfu-b.log 2>&1 &
SFU_B=$!
SIGNAL_OPS_PORT=3001 SIP_UDP_PORT=5060 SIP_TLS_PORT=5061 SIP_WSS_PORT=3061 POP_ID=pop-a \
  POP_REGISTRY_FILE="$REG" POP_SIP_URLS="pop-b=127.0.0.1:5070" SFU_URL=http://127.0.0.1:3002 \
  ./target/debug/aerodesk-signal >/tmp/mpop-sig-a.log 2>&1 &
SIG_A=$!
SIGNAL_OPS_PORT=3006 SIP_UDP_PORT=5070 SIP_TLS_PORT=5071 SIP_WSS_PORT=3071 POP_ID=pop-b \
  POP_REGISTRY_FILE="$REG" SFU_URL=http://127.0.0.1:3007 \
  ./target/debug/aerodesk-signal >/tmp/mpop-sig-b.log 2>&1 &
SIG_B=$!

for _ in $(seq 1 50); do
    grep -q "SIP 端点已就绪" /tmp/mpop-sig-a.log 2>/dev/null \
        && grep -q "SIP 端点已就绪" /tmp/mpop-sig-b.log 2>/dev/null && break
    sleep 0.2
done
sleep 0.3

echo "== viewer 经 signal-a 拨房间（应被 302 引导到 pop-b）"
AERO_SIP_PORT=5060 ./target/debug/aerodesk-agent --role viewer --reconnect \
    --signal ws://127.0.0.1:5060 --room "$ROOM" >/tmp/mpop-view.log 2>&1 &
VIEW=$!
sleep 10
kill "$VIEW" "$SFU_A" "$SFU_B" "$SIG_A" "$SIG_B" 2>/dev/null || true
wait 2>/dev/null || true

echo "== 断言"
fail=0
# 1) signal-a 302 决策（预登记命中 pop-b；日志消息体「room -> pop pop-b …」）
if grep -q "room -> pop pop-b (self=pop-a): 302 redirect" /tmp/mpop-sig-a.log; then
    echo "PASS signal-a redirect eu-* -> pop-b"
else
    echo "FAIL no redirect in signal-a"; tail -5 /tmp/mpop-sig-a.log; fail=1
fi
# 2) viewer 跟随换拨（客户端 302 跟随，#600）
if grep -q "SIP 302+Contact 跟随换拨" /tmp/mpop-view.log; then
    echo "PASS viewer followed 302 to pop-b"
else
    echo "FAIL viewer did not follow 302"; tail -8 /tmp/mpop-view.log; fail=1
fi
# 3) 目标 PoP 受理 REGISTER（viewer 换拨后注册 PoP-B）
if grep -q "SIP 注册" /tmp/mpop-sig-b.log; then
    echo "PASS client registered to signal-b (pop-b)"
else
    echo "FAIL no registration on signal-b"; tail -5 /tmp/mpop-sig-b.log; fail=1
fi
# 4) 换拨后会话在 B 会议桥建立（SDP 协商；无发布者 0 帧）
if grep -q "SDP negotiated" /tmp/mpop-view.log; then
    echo "PASS media session on sfu-b via pop-b (SDP negotiated)"
else
    echo "FAIL no session on sfu-b"; tail -8 /tmp/mpop-view.log; fail=1
fi
# 5) 无 panic
if grep -qiE "panic" /tmp/mpop-*.log; then
    echo "FAIL panic in logs"; fail=1
else
    echo "PASS no panics"
fi
rm -f "$REG"
exit $fail
