#!/usr/bin/env bash
# popreg-e2e.sh —— 多 PoP v2 动态 room→PoP 注册表（SIP 302+Contact 版，#154/#600）。
#
# 双 PoP（无静态钉住，共享注册表文件，各自显式 SIP 端口）：
#   PoP A: sfu-a(media 3478/internal 3002) + signal-a(SIP UDP 5060 / TLS 5061 /
#          WSS 3061 / ops 3001)
#   PoP B: sfu-b(media 3479/internal 3007) + signal-b(SIP UDP 5070 / TLS 5071 /
#          WSS 3071 / ops 3006)
#
# 流程（#600 客户端 302 跟随已落地，全链 SIP）：
#   seed viewer 在 A 首个 INVITE dyn-* 房间 → 登记 owner=pop-a（写共享注册表）；
#   viewer 在 B INVITE 同房间 → B 查注册表命中 pop-a → 302+Contact(<sip:room@
#   127.0.0.1:5060>) → 客户端跟随换拨 A → A 会议桥 → sfu-a 建会话。
#
# 注意：发布者角色不参与本脚本——SIP 形态 publisher 是 UAS 被叫（注册房间名
#   为设备 AoR），INVITE 会被 registrar 命中走 1:1 转发，**绕过** 302 决策
#   （旧 JSON「publisher join 触发登记」语义在 SIP 下不存在）。
set -euo pipefail
# 残留清理（同 multipop-e2e.sh：CI 实测双 PoP 默认 TLS/WSS 互撞 + 前序 e2e
# 残留进程致 AddrInUse）。
taskkill //F //IM aerodesk-signal.exe 2>/dev/null || true
taskkill //F //IM aerodesk-sfu.exe 2>/dev/null || true
pkill -f 'target/debug/aerodesk-signal' 2>/dev/null || true
pkill -f 'target/debug/aerodesk-sfu' 2>/dev/null || true
cd "$(dirname "$0")/.."

ROOM="dyn-room-$(date +%s)"
REG="/tmp/popreg-e2e-$(date +%s).json"
export RUST_LOG="${RUST_LOG:-info}"

echo "== 构建"
cargo build -q -p aerodesk-sfu -p aerodesk-signal -p aerodesk-agent

REC="$(mktemp -d)"
echo "== 启动 PoP A + PoP B，共享注册表 = $REG"
RECORD_DIR="$REC/a" ./target/debug/aerodesk-sfu >/tmp/popreg-sfu-a.log 2>&1 &
SFU_A=$!
SFU_MEDIA_PORT=3479 SFU_SIGNAL_PORT=3005 SFU_INTERNAL_PORT=3007 RECORD_DIR="$REC/b" ./target/debug/aerodesk-sfu >/tmp/popreg-sfu-b.log 2>&1 &
SFU_B=$!
SIGNAL_OPS_PORT=3001 SIP_UDP_PORT=5060 SIP_TLS_PORT=5061 SIP_WSS_PORT=3061 POP_ID=pop-a \
  POP_REGISTRY_FILE="$REG" SFU_URL=http://127.0.0.1:3002 \
  ./target/debug/aerodesk-signal >/tmp/popreg-sig-a.log 2>&1 &
SIG_A=$!
SIGNAL_OPS_PORT=3006 SIP_UDP_PORT=5070 SIP_TLS_PORT=5071 SIP_WSS_PORT=3071 POP_ID=pop-b \
  POP_REGISTRY_FILE="$REG" POP_SIP_URLS="pop-a=127.0.0.1:5060" SFU_URL=http://127.0.0.1:3007 \
  ./target/debug/aerodesk-signal >/tmp/popreg-sig-b.log 2>&1 &
SIG_B=$!

for _ in $(seq 1 50); do
    grep -q "SIP 端点已就绪" /tmp/popreg-sig-a.log 2>/dev/null \
        && grep -q "SIP 端点已就绪" /tmp/popreg-sig-b.log 2>/dev/null && break
    sleep 0.2
done
sleep 0.3

echo "== seed viewer 经 signal-a 首个 INVITE（登记 pop-a）"
AERO_SIP_PORT=5060 ./target/debug/aerodesk-agent --role viewer --reconnect \
    --signal ws://127.0.0.1:5060 --room "$ROOM" >/tmp/popreg-seed.log 2>&1 &
SEED=$!
sleep 2
echo "== viewer 经 signal-b INVITE 同房间（应 302 引导回 pop-a）"
AERO_SIP_PORT=5070 ./target/debug/aerodesk-agent --role viewer --reconnect \
    --signal ws://127.0.0.1:5070 --room "$ROOM" >/tmp/popreg-view.log 2>&1 &
VIEW=$!
sleep 10
kill "$SEED" "$VIEW" "$SFU_A" "$SFU_B" "$SIG_A" "$SIG_B" 2>/dev/null || true
wait 2>/dev/null || true

echo "== 断言"
fail=0
# 1) 首个 INVITE 者在 signal-a 登记 owner=pop-a（写注册表）
if grep -q "registered to pop pop-a (first inviter)" /tmp/popreg-sig-a.log; then
    echo "PASS signal-a registered room (first inviter)"
else
    echo "FAIL no first-inviter registration"; tail -5 /tmp/popreg-sig-a.log; fail=1
fi
# 2) signal-b 查注册表命中 pop-a → 302+Contact 引导
if grep -q "room -> pop pop-a (self=pop-b): 302 redirect" /tmp/popreg-sig-b.log; then
    echo "PASS signal-b dynamic redirect (302) to pop-a"
else
    echo "FAIL no dynamic 302 redirect"; tail -5 /tmp/popreg-sig-b.log; fail=1
fi
# 3) 注册表文件持久化房间归属（A 登记落盘，B 能读到）
if grep -q "\"$ROOM\"" "$REG" 2>/dev/null; then
    echo "PASS registry file contains room"
else
    echo "FAIL registry file missing room"; cat "$REG" 2>/dev/null | tail -3; fail=1
fi
# 4) viewer 跟随 302 换拨 A 并在 A 会议桥建会话
if grep -q "SIP 302+Contact 跟随换拨" /tmp/popreg-view.log \
   && grep -q "SDP negotiated" /tmp/popreg-view.log; then
    echo "PASS viewer followed 302 to pop-a and negotiated session"
else
    echo "FAIL viewer did not follow 302"; tail -8 /tmp/popreg-view.log; fail=1
fi
# 5) 无 panic
if grep -qiE "panic" /tmp/popreg-*.log; then
    echo "FAIL panic in logs"; fail=1
else
    echo "PASS no panics"
fi
rm -f "$REG"
exit $fail
