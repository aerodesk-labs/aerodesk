#!/usr/bin/env bash
# 浏览器端到端（#598 P2a：JSON WSS 面 → SIP-WSS 双页闭环）：headless 浏览器
# 被控页（sip-publisher.html，UAS 等被拨）+ headless 浏览器观看页（sip-viewer.html）
# 收流 + 输入事件经 data channel 直达被控页。
# 迁移背景：CLI publisher 已是 SIP 1:1 被叫；本脚本验证 Web↔Web 全 SIP 链路。
# 浏览器被控端无系统注入能力——输入断言改验证被控页收到输入事件。
# 依赖：cargo build、Playwright（npm i playwright-core）、Edge 或 Chrome、
# python3（静态服务 web/）。
# 用法: scripts/web-e2e.sh [room]
set -euo pipefail
ROOT="$(cd "$(dirname "$0")/.." && pwd)"
cd "$ROOT"

ROOM="${1:-webe2e-$(date +%s)}"
E2E_DIR="${WEB_E2E_DIR:-/tmp/web-e2e}"
WEB_SERVE_PORT="${WEB_SERVE_PORT:-38081}"
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
      '--ignore-certificate-errors',      // 3061 为自签 WSS（RFC 7118）
    ],
  });
  // 被控页（sip-publisher.html：UAS，等观看端拨入）
  const pub = await browser.newPage();
  await pub.goto(`http://127.0.0.1:${process.env.WEB_SERVE_PORT || 38081}/sip-publisher.html?device=${ROOM}&signal=wss://127.0.0.1:3061`);
  await pub.click('#connect');
  await pub.waitForFunction(() => document.getElementById('status').innerText.includes('等待观看端拨入'), { timeout: 20000 });
  console.log('PASS publisher registered, waiting INVITE');
  // 观看页（sip-viewer.html：呼入同房间收流 + 输入事件）
  const view = await browser.newPage();
  await view.goto(`http://127.0.0.1:${process.env.WEB_SERVE_PORT || 38081}/sip-viewer.html?target=${ROOM}&signal=wss://127.0.0.1:3061`);
  await view.click('#connect');
  await view.waitForFunction(() => document.getElementById('status').innerText.includes('已连接'), { timeout: 30000 });
  console.log('PASS viewer connected');
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
  // 被控页收到输入事件（sip-publisher.html ondatachannel 记 "input: {...}"）
  await pub.waitForFunction(() => document.getElementById('log').innerText.includes('input: '), { timeout: 15000 });
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
# #598 P2a：SIP-WSS 面（3061）承载浏览器信令；静态服务 web/（sip-*.html）。
SIP_WSS_PORT=3061 SIP_UDP_PORT=5060 "$ROOT/target/debug/aerodesk-signal" >/tmp/webe2e-sig.log 2>&1 &
SIG=$!
# 重试残留清理 + 就绪门：ci-retry 重跑时上轮 http.server 可能占口/半死；
# 未就绪即跑 node 会被 macOS 丢 SYN（ERR_CONNECTION_TIMED_OUT 实测）。
pkill -f "http.server $WEB_SERVE_PORT" 2>/dev/null || true
(cd "$ROOT/web" && python3 -m http.server "$WEB_SERVE_PORT" --bind 127.0.0.1 >/tmp/webe2e-http.log 2>&1) &
HTTP=$!
HTTP_OK=0
for _ in $(seq 1 50); do
    if (exec 3<>/dev/tcp/127.0.0.1/$WEB_SERVE_PORT) 2>/dev/null; then HTTP_OK=1; break; fi
    if ! kill -0 "$HTTP" 2>/dev/null; then break; fi
    sleep 0.5
done
[ "$HTTP_OK" = "1" ] || { echo "FAIL: web 静态服务未就绪（$WEB_SERVE_PORT）"; tail -10 /tmp/webe2e-http.log; exit 1; }
for _ in $(seq 1 50); do
    if grep -q "SIP/UDP 监听已起" /tmp/webe2e-sig.log 2>/dev/null && (exec 3<>/dev/tcp/127.0.0.1/3002) 2>/dev/null; then break; fi
    if ! kill -0 "$SFU" 2>/dev/null; then echo "FAIL sfu died"; tail -20 /tmp/webe2e-sfu.log; exit 1; fi
    sleep 0.2
done
sleep 0.3

set +e
WEB_SERVE_PORT="$WEB_SERVE_PORT" node "$E2E_DIR/e2e-web.js" "$ROOM"
RES=$?
set -e

kill "$HTTP" "$SFU" "$SIG" 2>/dev/null || true
if grep -qiE "panic" /tmp/webe2e-sfu.log /tmp/webe2e-sig.log; then
  echo "FAIL panic in logs"; RES=1
fi
rm -rf "$REC"
exit $RES
