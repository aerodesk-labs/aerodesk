#!/usr/bin/env bash
# Web 被控端端到端：headless Chrome 屏幕共享（fake device）→ SFU → CLI viewer 观看收流。
# 验证 web/index.html publisher 角色（屏幕共享采集 → WebRTC sendonly → SFU → 观看端解码）。
# 依赖：cargo build、Playwright（npm i playwright-core）、Chrome（BROWSER env）。
# 用法: scripts/web-pub-e2e.sh [room]
set -euo pipefail
ROOT="$(cd "$(dirname "$0")/.." && pwd)"
cd "$ROOT"

ROOM="${1:-webpub-$(date +%s)}"
E2E_DIR="${WEB_E2E_DIR:-/tmp/web-pub-e2e}"
mkdir -p "$E2E_DIR"
cd "$E2E_DIR"
if [ ! -d node_modules/playwright-core ]; then npm init -y >/dev/null 2>&1; npm i playwright-core >/dev/null 2>&1; fi

cat > e2e-pub.js <<'JS'
const { chromium } = require('playwright-core');
const ROOM = process.argv[2];
(async () => {
  const browser = await chromium.launch({
    channel: process.env.BROWSER || 'msedge', headless: true,
    args: [
      '--use-fake-ui-for-media-stream',   // getDisplayMedia 免交互授权
      '--use-fake-device-for-media-stream', // fake 摄像头/屏幕源
      '--auto-accept-this-tab-capture',   // headless 屏幕共享
      '--enable-usermedia-screen-capturing',
    ],
  });
  const page = await browser.newPage();
  await page.goto(`http://127.0.0.1:3002/?room=${ROOM}&role=publisher&signal=ws://127.0.0.1:3003/ws`);
  await page.click('#connect');
  await page.waitForFunction(() => document.getElementById('log').innerText.includes('屏幕共享已授权'), { timeout: 20000 });
  console.log('PASS screen shared');
  await page.waitForFunction(() => document.getElementById('status').innerText.includes('已连接'), { timeout: 25000 });
  console.log('PASS publisher connected');
  // 保持发布 12s（让 CLI viewer 收到关键帧 + 若干帧）
  await new Promise(r => setTimeout(r, 12000));
  await browser.close();
  console.log('E2E DONE');
})().catch(e => { console.error('E2E FAIL:', e.message); process.exit(1); });
JS

cd "$ROOT"
echo "== 构建"
cargo build -q -p aerodesk-sfu -p aerodesk-signal -p aerodesk-agent
echo "== 启动服务"
# 前置 e2e 可能残留 SFU/signal 占用 3002/3003 → 先清理，避免 bind 失败。
pkill -f "aerodesk-sfu|aerodesk-signal" 2>/dev/null || true
sleep 1
REC="$(mktemp -d)"
RECORD_DIR="$REC" "$ROOT/target/debug/aerodesk-sfu" >/tmp/webpub-sfu.log 2>&1 &
SFU=$!
SIP_UDP_PORT=5060 "$ROOT/target/debug/aerodesk-signal" >/tmp/webpub-sig.log 2>&1 &
SIG=$!
OK=0
for _ in $(seq 1 50); do
    if grep -q "SIP/UDP 监听已起" /tmp/webpub-sig.log 2>/dev/null && nc -z 127.0.0.1 3002 2>/dev/null; then OK=1; break; fi
    if ! kill -0 "$SFU" 2>/dev/null; then break; fi
    sleep 0.2
done
if [ "$OK" != "1" ]; then
  echo "FAIL: SFU/signal 未就绪；sfu log:"; tail -20 /tmp/webpub-sfu.log; exit 1
fi
# CLI viewer 作为观看端：断言能收到 Web 被控端发布的媒体帧。
"$ROOT/target/debug/aerodesk-agent" --role viewer --signal ws://127.0.0.1:3003 --room "$ROOM" >/tmp/webpub-view.log 2>&1 &
VIEW=$!
sleep 2

set +e
node "$E2E_DIR/e2e-pub.js" "$ROOM"
RES=$?
set -e
sleep 3

kill "$VIEW" "$SFU" "$SIG" 2>/dev/null || true
# 断言观看端收到 Web 被控端的媒体（RECEIVED 行 + 帧数 > 0）
if grep -qE "RECEIVED: [1-9][0-9]* frames" /tmp/webpub-view.log; then
  echo "PASS CLI viewer received frames from web publisher"
else
  echo "FAIL viewer frames"; tail -8 /tmp/webpub-view.log; RES=1
fi
rm -rf "$REC"
exit $RES
