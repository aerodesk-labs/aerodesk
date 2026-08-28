#!/usr/bin/env bash
# #58 画质选层端到端（#598 v0.4 会议形态恢复）：浏览器发布端（sip-publisher.html
# conference=1 直接入会）→ SFU 会议（view-<room>）→ 2×CLI viewer（--layer f/q）
# 入会同房间收流 + 选层请求转发断言。
#
# 恢复背景（#598 P2a 暂停 → v0.4 恢复）：会议桥（#606）+ 方向判定 + 浏览器发布
# 端入会（#608 conference=1）齐备——发布端 INVITE view-AoR（SFU role=publisher）、
# viewer INVITE 同 AoR（role=viewer）。simulcast 三层（q/h/f + f>q 码率）仍待
# 原生端会议发布落地（浏览器无 simulcast rid）——本脚本维持单层发布 + 选层请求
# 转发护栏。
# 依赖：cargo build、Playwright（npm i playwright-core）、Edge/Chrome（BROWSER env）、
# python3（静态服务 web/）。
# 用法: scripts/simulcast-e2e.sh [房间] [观察秒数]
set -euo pipefail
ROOT="$(cd "$(dirname "$0")/.." && pwd)"
cd "$ROOT"

ROOM="${1:-sim-$(date +%s)}"
OBS="${2:-12}"
WEB_SERVE_PORT="${WEB_SERVE_PORT:-38088}"
export FFMPEG_DIR="${FFMPEG_DIR:-/d/tools/ffmpeg81/ffmpeg-n8.1-latest-win64-gpl-shared-8.1}"
LAN_IP=$(python3 - <<'PY'
import socket
s = socket.socket(socket.AF_INET, socket.SOCK_DGRAM)
try:
    s.connect(('8.8.8.8', 80)); print(s.getsockname()[0])
except Exception: print('127.0.0.1')
PY
)
REC="$(mktemp -d)"

echo "== 构建"
cargo build -q -p aerodesk-sfu -p aerodesk-signal -p aerodesk-agent

E2E_DIR="${WEB_E2E_DIR:-/tmp/simulcast-e2e}"
mkdir -p "$E2E_DIR"; cd "$E2E_DIR"
[ -d node_modules/playwright-core ] || (npm init -y >/dev/null 2>&1; npm i playwright-core >/dev/null 2>&1)
cat > e2e-pub.js <<'JS'
const { chromium } = require('playwright-core');
const ROOM = process.argv[2];
(async () => {
  const browser = await chromium.launch({
    channel: process.env.BROWSER || 'msedge', headless: true,
    args: ['--use-fake-ui-for-media-stream', '--use-fake-device-for-media-stream',
           '--auto-accept-this-tab-capture', '--enable-usermedia-screen-capturing',
           '--ignore-certificate-errors'],
  });
  const page = await browser.newPage();
  // #598 v0.4：conference=1 → 直接会议发布（注册后 INVITE view-AoR）
  await page.goto(`http://127.0.0.1:${process.env.WEB_SERVE_PORT || 38088}/sip-publisher.html?device=${ROOM}&conference=1&signal=wss://127.0.0.1:3061`);
  await page.click('#connect');
  await page.waitForFunction(() => document.getElementById('status').innerText.includes('会议发布'), { timeout: 30000 });
  console.log('PASS publisher in conference');
  await new Promise(r => setTimeout(r, parseInt(process.env.OBS || '12') * 1000));
  await browser.close();
  console.log('E2E DONE');
})().catch(e => { console.error('E2E FAIL: ' + e.message); process.exit(1); });
JS
cd "$ROOT"

echo "== 启动 sfu/signal"
RECORD_DIR="$REC" SFU_HOST_ADDRESS=127.0.0.1 ./target/debug/aerodesk-sfu >/tmp/sim-sfu.log 2>&1 &
SFU_PID=$!
SIP_UDP_PORT=5060 SIP_WSS_PORT=3061 ./target/debug/aerodesk-signal >/tmp/sim-sig.log 2>&1 &
SIG_PID=$!
(cd "$ROOT/web" && python3 -m http.server "$WEB_SERVE_PORT" --bind 127.0.0.1 >/tmp/sim-http.log 2>&1) &
HTTP=$!
OK=0
for _ in $(seq 1 50); do
  if grep -q "SIP/UDP 监听已起" /tmp/sim-sig.log 2>/dev/null \
    && (exec 3<>/dev/tcp/127.0.0.1/3002) 2>/dev/null \
    && (exec 3<>/dev/tcp/127.0.0.1/$WEB_SERVE_PORT) 2>/dev/null; then OK=1; break; fi
  sleep 0.2
done
[ "$OK" = "1" ] || { echo "FAIL 服务未就绪"; tail -10 /tmp/sim-sig.log; exit 1; }

echo "== 启动 Web 发布端（conference=1 直接入会）"
OBS="$OBS" WEB_SERVE_PORT="$WEB_SERVE_PORT" node "$E2E_DIR/e2e-pub.js" "$ROOM" >/tmp/sim-pub.log 2>&1 &
PUB_PID=$!
OK=0
for _ in $(seq 1 60); do
  grep -q "PASS publisher in conference" /tmp/sim-pub.log 2>/dev/null && { OK=1; break; }
  kill -0 "$PUB_PID" 2>/dev/null || break
  sleep 0.5
done
[ "$OK" = "1" ] || { echo "FAIL 发布端未入会"; tail -8 /tmp/sim-pub.log; exit 1; }

echo "== 启动 viewer f/q（会议，先登记选层）"
./target/debug/aerodesk-agent --role viewer --layer f --reconnect \
  --signal "ws://$LAN_IP:3061" --room "view-$ROOM" >/tmp/sim-view-f.log 2>&1 &
F_PID=$!
./target/debug/aerodesk-agent --role viewer --layer q --reconnect \
  --signal "ws://$LAN_IP:3061" --room "view-$ROOM" >/tmp/sim-view-q.log 2>&1 &
Q_PID=$!
ready=0
for _ in $(seq 1 60); do
  if grep -q "layer request sent" /tmp/sim-view-f.log 2>/dev/null \
    && grep -q "layer request sent" /tmp/sim-view-q.log 2>/dev/null; then ready=1; break; fi
  sleep 0.5
done
[ "$ready" = "1" ] || { echo "FAIL viewer 未登记选层"; tail -5 /tmp/sim-view-f.log /tmp/sim-view-q.log; kill $F_PID $Q_PID $PUB_PID $SFU_PID $SIG_PID $HTTP 2>/dev/null; exit 1; }

echo "== 观察 ${OBS}s"
sleep "$OBS"

kill "$F_PID" "$Q_PID" "$PUB_PID" "$SFU_PID" "$SIG_PID" "$HTTP" 2>/dev/null || true
wait 2>/dev/null || true

echo "== 断言"
fail=0
# 1) 两个 viewer 都收到发布端媒体（SFU 多播转发成立）
for v in f q; do
  if grep -qE "RECEIVED: [1-9][0-9]* frames" /tmp/sim-view-$v.log; then
    echo "PASS viewer $v received media"
  else
    echo "FAIL viewer $v received 0 frames"; tail -8 /tmp/sim-view-$v.log; fail=1
  fi
done
# 2) SFU 收到两个显式选层请求（--layer f/q 经 control 通道）
for layer in High Low; do
  if grep -q "layer request: Some($layer)" /tmp/sim-sfu.log; then
    echo "PASS SFU layer request $layer"
  else
    echo "FAIL SFU layer request $layer missing"; fail=1
  fi
done
# 3) 无 panic
if grep -qiE "panic" /tmp/sim-view-f.log /tmp/sim-view-q.log /tmp/sim-sfu.log /tmp/sim-sig.log /tmp/sim-pub.log; then
  echo "FAIL panic in logs"; fail=1
else
  echo "PASS no panics"
fi

rm -rf "$REC"
exit $fail
