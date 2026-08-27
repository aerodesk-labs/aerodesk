#!/usr/bin/env bash
# 浏览器端到端（#552 迁移后双浏览器版 2026-08-24）：headless Chrome 屏幕共享
# 发布（WSS 房间）+ headless Chrome 观看（同房间收流 + 输入事件经 SFU 转发）。
# 迁移背景：CLI publisher 已是 SIP 1:1 被叫，WSS JSON 面无法对其呼叫（互通
# 缺口待 Web SIP-WSS）；Web 端自身能力（发布/观看/输入回传）在 WSS 房间内
# 闭环验证不受影响。浏览器被控端无系统注入能力——输入断言改验证 SFU 收到
# 输入事件（CLI 注入链路由 input-e2e 覆盖）。
# 依赖：cargo build、Playwright（npm i playwright-core）、Edge 或 Chrome。
# 用法: scripts/web-e2e.sh [room]
set -euo pipefail
ROOT="$(cd "$(dirname "$0")/.." && pwd)"
cd "$ROOT"

ROOM="${1:-webe2e-$(date +%s)}"
E2E_DIR="${WEB_E2E_DIR:-/tmp/web-e2e}"
mkdir -p "$E2E_DIR"
cd "$E2E_DIR"
if [ ! -d node_modules/playwright-core ]; then npm init -y >/dev/null 2>&1; npm i playwright-core >/dev/null 2>&1; fi

cat > e2e-web.js <<'JS'
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
  // 发布页（WSS 房间发布，屏幕共享）
  const pub = await browser.newPage();
  await pub.goto(`http://127.0.0.1:3002/?room=${ROOM}&role=publisher&signal=ws://127.0.0.1:3003/ws`);
  await pub.click('#connect');
  await pub.waitForFunction(() => document.getElementById('log').innerText.includes('屏幕共享已授权'), { timeout: 20000 });
  await pub.waitForFunction(() => document.getElementById('status').innerText.includes('已连接'), { timeout: 25000 });
  console.log('PASS publisher connected');
  // 观看页（同房间收流 + 输入事件）
  const view = await browser.newPage();
  await view.goto(`http://127.0.0.1:3002/?room=${ROOM}&role=viewer&signal=ws://127.0.0.1:3003/ws`);
  await view.click('#connect');
  await view.waitForFunction(() => document.getElementById('video').readyState >= 2, { timeout: 25000 });
  console.log('PASS video playing');
  await view.waitForFunction(() => document.getElementById('log').innerText.includes('input channel open'), { timeout: 15000 });
  console.log('PASS input channel open');
  for (let i = 0; i < 10; i++) {
    await view.$eval('#video', v => {
      const r = v.getBoundingClientRect();
      v.dispatchEvent(new MouseEvent('mousemove', { clientX: r.left + r.width / 2, clientY: r.top + r.height / 2, bubbles: true }));
    });
    await new Promise(r => setTimeout(r, 100));
  }
  // 发布页收到输入事件（经 SFU 转发；web/index.html 接收端 log 记 input event）
  await pub.waitForFunction(() => document.getElementById('log').innerText.includes('input event'), { timeout: 15000 });
  console.log('PASS input relayed to publisher page');
  await new Promise(r => setTimeout(r, 1500));
  await browser.close();
  console.log('E2E DONE');
})().catch(e => { console.error('E2E FAIL:', e.message); process.exit(1); });
JS

cd "$ROOT"
echo "== 构建"
cargo build -q -p aerodesk-sfu -p aerodesk-signal -p aerodesk-agent
echo "== 启动服务"
pkill -f "aerodesk-sfu|aerodesk-signal" 2>/dev/null || true
sleep 1
REC="$(mktemp -d)"
RECORD_DIR="$REC" "$ROOT/target/debug/aerodesk-sfu" >/tmp/webe2e-sfu.log 2>&1 &
SFU=$!
SIP_UDP_PORT=5060 "$ROOT/target/debug/aerodesk-signal" >/tmp/webe2e-sig.log 2>&1 &
SIG=$!
for _ in $(seq 1 50); do
    if grep -q "SIP/UDP 监听已起" /tmp/webe2e-sig.log 2>/dev/null && nc -z 127.0.0.1 3002 2>/dev/null; then break; fi
    if ! kill -0 "$SFU" 2>/dev/null; then echo "FAIL sfu died"; tail -20 /tmp/webe2e-sfu.log; exit 1; fi
    sleep 0.2
done
sleep 0.3

set +e
node "$E2E_DIR/e2e-web.js" "$ROOM"
RES=$?
set -e

kill "$SFU" "$SIG" 2>/dev/null || true
if grep -qiE "panic" /tmp/webe2e-sfu.log /tmp/webe2e-sig.log; then
  echo "FAIL panic in logs"; RES=1
fi
rm -rf "$REC"
exit $RES
