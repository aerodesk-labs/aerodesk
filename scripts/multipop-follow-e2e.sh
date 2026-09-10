#!/usr/bin/env bash
# #600 多 PoP 302 跟随本地验证：signal A（含 POP_SIP_URLS 指向 B）+
# signal B（房间在 B）→ viewer 在 A 拨 B 房间 → A 回 302+Contact(B) →
# viewer 跟随换拨 B 并成功入会（SFU 在 B 侧）。
set -euo pipefail
# 重跑残留清理：双 PoP 各端口占用会致 ops bind 失败（3001 默认）。
taskkill //F //IM aerodesk-signal.exe 2>/dev/null || true
taskkill //F //IM aerodesk-sfu.exe 2>/dev/null || true
ROOT="$(cd "$(dirname "$0")/.." && pwd)"
cd "$ROOT"
# ffmpeg-sys-next 9 行为：FFMPEG_DIR 一旦「已设置」（含空串/不存在路径）即放弃
# pkg-config 直查该路径——历史默认值是提交者本机 Windows 路径，曾在 macOS 构建
# 失败。Windows CI 由 ci.yml 注入 BtbN 路径；本地开发用 AERO_FFMPEG_DIR 回退；
# 其余情况不导出（走 pkg-config/brew/apt 发现）。
FFMPEG_DIR="${FFMPEG_DIR:-${AERO_FFMPEG_DIR:-}}"
if [ -n "$FFMPEG_DIR" ] && [ ! -d "$FFMPEG_DIR" ]; then
    echo "WARN: FFMPEG_DIR=$FFMPEG_DIR 不存在，忽略（回退 pkg-config 发现）"
    FFMPEG_DIR=""
fi
if [ -n "$FFMPEG_DIR" ]; then export FFMPEG_DIR; else unset FFMPEG_DIR; fi

ROOM="multipop-$(date +%s)"
REC="$(mktemp -d)"
LAN_IP=$(python3 - <<'PY'
import socket
s = socket.socket(socket.AF_INET, socket.SOCK_DGRAM)
try:
    s.connect(('8.8.8.8', 80)); print(s.getsockname()[0])
except Exception: print('127.0.0.1')
PY
)

echo "== 构建"
cargo build -q -p aerodesk-sfu -p aerodesk-signal -p aerodesk-agent

echo "== 启动 PoP-A（SIP 15060，POP_SIP_URLS=127.0.0.1:15061 指向 B）"
RECORD_DIR="$REC" SFU_HOST_ADDRESS=127.0.0.1 SFU_SIGNAL_PORT=15000 SFU_INTERNAL_PORT=15002 \
  ./target/debug/aerodesk-sfu >/tmp/mpop-sfu-a.log 2>&1 &
SFU_A=$!
RUST_LOG=debug POP_ID=pop-a POP_REGISTRY_FILE=/tmp/mpop-registry.json ROOM_POP_MAP="multipop-=pop-b" POP_SIP_URLS="pop-b=127.0.0.1:15061" \
  SIP_UDP_PORT=15060 SFU_URL=http://127.0.0.1:15002 \
  ./target/debug/aerodesk-signal >/tmp/mpop-sig-a.log 2>&1 &
SIG_A=$!
echo "== 启动 PoP-B（SIP 15061 + 其 SFU 会议桥）"
RECORD_DIR="$REC" SFU_HOST_ADDRESS=127.0.0.1 SFU_SIGNAL_PORT=15100 SFU_INTERNAL_PORT=15102 \
  ./target/debug/aerodesk-sfu >/tmp/mpop-sfu-b.log 2>&1 &
SFU_B=$!
POP_ID=pop-b POP_REGISTRY_FILE=/tmp/mpop-registry.json POP_SIP_URLS=pop-a=127.0.0.1:15060 SIGNAL_OPS_PORT=15062 SIP_UDP_PORT=15061 SFU_URL=http://127.0.0.1:15102 \
  ./target/debug/aerodesk-signal >/tmp/mpop-sig-b.log 2>&1 &
SIG_B=$!
OK=0
for _ in $(seq 1 50); do
  grep -q "SIP/UDP 监听已起" /tmp/mpop-sig-a.log 2>/dev/null \
    && grep -q "SIP/UDP 监听已起" /tmp/mpop-sig-b.log 2>/dev/null && { OK=1; break; }
  sleep 0.2
done
[ "$OK" = "1" ] || { echo "FAIL 双 PoP 未就绪"; exit 1; }

echo "== 种子：viewer 在 B 拨房间（登记 owner=pop-b）"
AERO_SIP_PORT=15061 ./target/debug/aerodesk-agent --role viewer   --signal "ws://$LAN_IP:15061" --room "$ROOM" >/tmp/mpop-seed.log 2>&1 &
SEED=$!
sleep 3
kill "$SEED" 2>/dev/null || true
grep -q "registered to pop pop-b" /tmp/mpop-sig-b.log && echo "PASS B 登记房间" || echo "WARN B 未登记"

echo "== viewer 在 A 拨 B 的房间（multipop-* → pop-b）"
AERO_SIP_PORT=15060 ./target/debug/aerodesk-agent --role viewer --reconnect \
  --signal "ws://$LAN_IP:15060" --room "$ROOM" >/tmp/mpop-view.log 2>&1 &
VIEW=$!
OK=0
for _ in $(seq 1 40); do
  grep -q "跟随 302 换拨" /tmp/mpop-view.log 2>/dev/null && { OK=1; break; }
  sleep 0.5
done
[ "$OK" = "1" ] || { echo "FAIL viewer 未跟随 302"; tail -8 /tmp/mpop-view.log; kill $VIEW $SFU_A $SIG_A $SFU_B $SIG_B 2>/dev/null; exit 1; }
echo "PASS viewer 跟随 302 换拨 PoP-B"

# 换拨后 viewer 重拨到 B：房间 multipop-* 在 B 是未注册名 → B 会议桥 → SFU-B
# （无 publisher，viewer 入会但 0 帧——换拨语义验证目标：成功连到 B 的会议桥）
OK=0
for _ in $(seq 1 40); do
  grep -qE "SDP negotiated|会议|INVITE.*view-" /tmp/mpop-view.log 2>/dev/null && { OK=1; break; }
  sleep 0.5
done
[ "$OK" = "1" ] || { echo "FAIL viewer 未在 B 建立会话"; tail -8 /tmp/mpop-view.log; kill $VIEW $SFU_A $SIG_A $SFU_B $SIG_B 2>/dev/null; exit 1; }
echo "PASS viewer 在 PoP-B 建立会话（会议桥）"

kill "$VIEW" "$SFU_A" "$SIG_A" "$SFU_B" "$SIG_B" 2>/dev/null || true
echo "E2E DONE"
