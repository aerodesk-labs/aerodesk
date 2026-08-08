#!/usr/bin/env bash
# popreg-e2e.sh —— 多 PoP v2 动态 room→PoP 注册表（#154）。
#
# 双 PoP（无静态 ROOM_POP_MAP，共享注册表文件）：
#   publisher 经 signal-a 首个加入 dyn-* 房间 → 登记 pop-a；
#   viewer 经 signal-b 加入同房间 → 查注册表命中 pop-a → 重定向 → 跟随加入 PoP A。
set -euo pipefail
cd "$(dirname "$0")/.."

ROOM="dyn-room-$(date +%s)"
REG="/tmp/popreg-e2e-$(date +%s).json"
export RUST_LOG="${RUST_LOG:-info}"

echo "== 构建"
cargo build -q -p aerodesk-sfu -p aerodesk-signal -p aerodesk-cli

REC="$(mktemp -d)"
echo "== 启动 PoP A + PoP B，共享注册表 = $REG"
RECORD_DIR="$REC/a" ./target/debug/aerodesk-sfu >/tmp/popreg-sfu-a.log 2>&1 &
SFU_A=$!
SFU_MEDIA_PORT=3479 SFU_SIGNAL_PORT=3005 SFU_INTERNAL_PORT=3007 RECORD_DIR="$REC/b" ./target/debug/aerodesk-sfu >/tmp/popreg-sfu-b.log 2>&1 &
SFU_B=$!
POP_ID=pop-a POP_URLS="pop-b=ws://127.0.0.1:3006/ws" POP_REGISTRY_FILE="$REG" ./target/debug/aerodesk-signal >/tmp/popreg-sig-a.log 2>&1 &
SIG_A=$!
POP_ID=pop-b POP_URLS="pop-a=ws://127.0.0.1:3003/ws" SIGNAL_PORT=3004 SIGNAL_PLAIN_PORT=3006 SFU_URL=http://127.0.0.1:3007 POP_REGISTRY_FILE="$REG" ./target/debug/aerodesk-signal >/tmp/popreg-sig-b.log 2>&1 &
SIG_B=$!

for _ in $(seq 1 50); do
    if nc -z 127.0.0.1 3002 2>/dev/null && nc -z 127.0.0.1 3007 2>/dev/null \
       && nc -z 127.0.0.1 3003 2>/dev/null && nc -z 127.0.0.1 3006 2>/dev/null; then
        break
    fi
    sleep 0.2
done
sleep 0.3

echo "== publisher 经 signal-a 首个加入（登记 pop-a）"
./target/debug/aerodesk-cli --role publisher --encoder x264 --noisy \
    --signal ws://127.0.0.1:3003 --room "$ROOM" >/tmp/popreg-pub.log 2>&1 &
PUB=$!
sleep 2
echo "== viewer 经 signal-b 加入同房间（应动态重定向到 pop-a）"
./target/debug/aerodesk-cli --role viewer \
    --signal ws://127.0.0.1:3006 --room "$ROOM" >/tmp/popreg-view.log 2>&1 &
VIEW=$!
sleep 10
kill "$PUB" "$VIEW" "$SFU_A" "$SFU_B" "$SIG_A" "$SIG_B" 2>/dev/null || true
wait 2>/dev/null || true

echo "== 断言"
fail=0
# 1) 首个加入者在 signal-a 登记
if grep -q "registered to pop pop-a (first joiner)" /tmp/popreg-sig-a.log; then
    echo "PASS signal-a registered room (first joiner)"
else
    echo "FAIL no first-joiner registration"; tail -5 /tmp/popreg-sig-a.log; fail=1
fi
# 2) signal-b 查注册表命中 pop-a → 重定向
if grep -q "room $ROOM -> pop pop-a" /tmp/popreg-sig-b.log; then
    echo "PASS signal-b dynamic redirect to pop-a"
else
    echo "FAIL no dynamic redirect"; tail -5 /tmp/popreg-sig-b.log; fail=1
fi
# 3) 注册表文件持久化房间归属
if grep -q "\"$ROOM\"" "$REG" 2>/dev/null; then
    echo "PASS registry file contains room"
else
    echo "FAIL registry file missing room"; cat "$REG" 2>/dev/null | tail -3; fail=1
fi
# 4) viewer 重定向后加入 PoP A（sfu-a 有两个成员加入）
if [ "$(grep -c "joined room $ROOM" /tmp/popreg-sfu-a.log)" -ge 2 ]; then
    echo "PASS both peers joined on sfu-a (publisher + redirected viewer)"
else
    echo "FAIL expected 2 joins on sfu-a"; grep "joined room" /tmp/popreg-sfu-a.log | tail -5; fail=1
fi
# 5) 无 panic
if grep -qiE "panic" /tmp/popreg-*.log; then
    echo "FAIL panic in logs"; fail=1
else
    echo "PASS no panics"
fi
rm -f "$REG"
exit $fail
