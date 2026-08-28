#!/usr/bin/env bash
# popreg-e2e.sh —— 多 PoP v2 动态 room→PoP 注册表（P3 SIP 单栈 302 版，#154）。
#
# 双 PoP（无静态钉住，共享注册表文件，各自显式 SIP UDP 端口）：
#   publisher 经 signal-a 首个 INVITE dyn-* 房间 → 登记 pop-a（owner PoP）；
#   viewer 经 signal-b INVITE 同房间 → 注册表命中 pop-a → 302+Contact 引导回
#   PoP-A → 客户端跟随 → 会议桥入 sfu-a。
#
# 注意：服务端 INVITE 归属登记 + 302+Contact P3.1 已实现（断言 1/2/3 锚定现实
# 日志：302 决策消息体为「room -> pop <pop> …」，房间名在 room= 结构化字段）；
# 客户端 302 跟随（会话层换拨）**尚未实现**（#600 仅落地 core 层 RedirectedTo
# 事件透传）——断言 4（跟随后媒体入 PoP-A）在跟随落地前必失败，CI 维持 if:false。
set -euo pipefail
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
SIGNAL_OPS_PORT=3001 SIP_UDP_PORT=5060 POP_ID=pop-a POP_REGISTRY_FILE="$REG" \
  SFU_URL=http://127.0.0.1:3002 ./target/debug/aerodesk-signal >/tmp/popreg-sig-a.log 2>&1 &
SIG_A=$!
SIGNAL_OPS_PORT=3006 SIP_UDP_PORT=5070 POP_ID=pop-b POP_REGISTRY_FILE="$REG" \
  POP_SIP_URLS="pop-a=127.0.0.1:5060" SFU_URL=http://127.0.0.1:3007 \
  ./target/debug/aerodesk-signal >/tmp/popreg-sig-b.log 2>&1 &
SIG_B=$!

for _ in $(seq 1 50); do
    if nc -z 127.0.0.1 3002 2>/dev/null && nc -z 127.0.0.1 3007 2>/dev/null \
       && nc -z 127.0.0.1 3001 2>/dev/null && nc -z 127.0.0.1 3006 2>/dev/null; then
        break
    fi
    sleep 0.2
done
sleep 0.3

echo "== publisher 经 signal-a 首个 INVITE（登记 pop-a）"
AERO_SIP_PORT=5060 ./target/debug/aerodesk-agent --role publisher --encoder x264 --noisy \
    --signal ws://127.0.0.1:3001 --room "$ROOM" >/tmp/popreg-pub.log 2>&1 &
PUB=$!
sleep 2
echo "== viewer 经 signal-b INVITE 同房间（应 302 引导回 pop-a）"
AERO_SIP_PORT=5070 ./target/debug/aerodesk-agent --role viewer \
    --signal ws://127.0.0.1:3006 --room "$ROOM" >/tmp/popreg-view.log 2>&1 &
VIEW=$!
sleep 10
kill "$PUB" "$VIEW" "$SFU_A" "$SFU_B" "$SIG_A" "$SIG_B" 2>/dev/null || true
wait 2>/dev/null || true

echo "== 断言"
fail=0
# 1) 首个 INVITE 者在 signal-a 登记（P3 新锚点：INVITE 会议分支写入）
if grep -q "room $ROOM registered to pop pop-a (first inviter)" /tmp/popreg-sig-a.log; then
    echo "PASS signal-a registered room (first inviter)"
else
    echo "FAIL no first-inviter registration"; tail -5 /tmp/popreg-sig-a.log; fail=1
fi
# 2) signal-b 查注册表命中 pop-a → 302+Contact 引导（P3 日志：房间名在 room=
#    结构化字段；消息体「room -> pop pop-a …」不含房间名，勿用
#    「room $ROOM -> pop pop-a」旧 JSON 形态——对现实日志永不匹配）
if grep -q "room=$ROOM" /tmp/popreg-sig-b.log \
   && grep -q "room -> pop pop-a " /tmp/popreg-sig-b.log \
   && grep -q "302 redirect" /tmp/popreg-sig-b.log; then
    echo "PASS signal-b dynamic redirect (302) to pop-a"
else
    echo "FAIL no dynamic 302 redirect"; tail -5 /tmp/popreg-sig-b.log; fail=1
fi
# 3) 注册表文件持久化房间归属
if grep -q "\"$ROOM\"" "$REG" 2>/dev/null; then
    echo "PASS registry file contains room"
else
    echo "FAIL registry file missing room"; cat "$REG" 2>/dev/null | tail -3; fail=1
fi
# 4) viewer 跟随后媒体入 PoP-A（ICE 建链 + 解码帧；客户端 302 跟随未实现前必缺失）
if grep -q "ICE connected" /tmp/popreg-view.log \
   && grep -qE "DECODED: [1-9]" /tmp/popreg-view.log; then
    echo "PASS media on sfu-a via pop-a (ICE + decoded frames)"
else
    echo "FAIL no media on sfu-a"; tail -8 /tmp/popreg-view.log; fail=1
fi
# 5) 无 panic
if grep -qiE "panic" /tmp/popreg-*.log; then
    echo "FAIL panic in logs"; fail=1
else
    echo "PASS no panics"
fi
rm -f "$REG"
exit $fail
