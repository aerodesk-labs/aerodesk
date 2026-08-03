#!/usr/bin/env bash
# 浏览器端到端：sfu + signal + x264 发布端 + Edge/Chromium 观看端。
# 依赖：cargo build、Playwright（npm i playwright-core）、Edge 或 Chrome。
# 用法: scripts/web-e2e.sh [room]
set -euo pipefail
cd "$(dirname "$0")/.."

ROOM="${1:-webe2e-$(date +%s)}"
E2E_DIR="${WEB_E2E_DIR:-/tmp/web-e2e}"
mkdir -p "$E2E_DIR"
cd "$E2E_DIR"
if [ ! -d node_modules/playwright-core ]; then npm init -y >/dev/null 2>&1; npm i playwright-core >/dev/null 2>&1; fi

cat > e2e-run.js <<'JS'
const { chromium } = require('playwright-core');
const ROOM = process.argv[2];
(async () => {
  const browser = await chromium.launch({ channel: process.env.BROWSER || 'msedge', headless: true });
  const page = await browser.newPage();
  await page.goto(`http://127.0.0.1:3002/?room=${ROOM}&role=viewer&signal=ws://127.0.0.1:3003/ws`);
  await page.click('#connect');
  await page.waitForFunction(() => document.getElementById('video').readyState >= 2, { timeout: 25000 });
  console.log('PASS video playing');
  await page.waitForFunction(() => document.getElementById('log').innerText.includes('input channel open'), { timeout: 15000 });
  console.log('PASS input channel open');
  for (let i = 0; i < 10; i++) {
    await page.$eval('#video', v => {
      const r = v.getBoundingClientRect();
      v.dispatchEvent(new MouseEvent('mousemove', { clientX: r.left + r.width / 2, clientY: r.top + r.height / 2, bubbles: true }));
    });
    await new Promise(r => setTimeout(r, 100));
  }
  await new Promise(r => setTimeout(r, 1500));
  await browser.close();
  console.log('E2E DONE');
})().catch(e => { console.error('E2E FAIL:', e.message); process.exit(1); });
JS

cd "$(dirname "$0")/../.."
echo "== 构建"
cargo build -q -p aerodesk-sfu -p aerodesk-signal -p aerodesk-cli
echo "== 启动服务"
REC="$(mktemp -d)"
RECORD_DIR="$REC" ./target/debug/aerodesk-sfu >/tmp/webe2e-sfu.log 2>&1 &
SFU=$!
./target/debug/aerodesk-signal >/tmp/webe2e-sig.log 2>&1 &
SIG=$!
sleep 1.5
./target/debug/aerodesk-cli --role publisher --signal ws://127.0.0.1:3003 --room "$ROOM" --encoder x264 >/tmp/webe2e-pub.log 2>&1 &
PUB=$!
sleep 2

set +e
node "$E2E_DIR/e2e-run.js" "$ROOM"
RES=$?
set -e

kill "$PUB" "$SFU" "$SIG" 2>/dev/null || true
# 断言发布端收到输入
if grep -q "input: seq=" /tmp/webe2e-pub.log; then
  echo "PASS publisher received input events"
else
  echo "FAIL publisher input"; RES=1
fi
rm -rf "$REC"
exit $RES
