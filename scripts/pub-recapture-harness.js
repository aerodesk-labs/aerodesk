// pub-recapture-harness.js —— publisher 重连复用 capture 的窄 harness（#publisher-reconnect-recapture）。
//
// 验证 sip-publisher.html 在多次重连时**复用**已有 screen capture、不重复弹
// getDisplayMedia（真实浏览器每弹一次 = 一次系统采集选择器）。不依赖真实 signal：
// 页面内直接驱动 startCapture / cleanupAll，断言 navigator.mediaDevices.getDisplayMedia
// 的调用次数。
//
// 场景与期望（修复后）：
//   1) 首次采集          → getDisplayMedia 调用 1 次
//   2) 重连（保留媒体轨） → 复用，仍 1 次
//   3) 再一轮重连         → 复用，仍 1 次
//   4) 主动断开（stop 轨）→ 重新采集，2 次
// 修复前：cleanupAll 无条件 stop capture + startCapture 无条件重采 ⇒ 每轮重连都 +1（1,2,3,4）。
//
// 用法（需 playwright-core；脚本内置静态服务，服务仓库 web/ 目录）：
//   cd /tmp/web-e2e && npm i playwright-core 2>/dev/null; \
//   NODE_PATH=/tmp/web-e2e/node_modules node scripts/pub-recapture-harness.js
//   （端口默认 38099，可用 WEB_SERVE_PORT 覆盖；NODE_PATH 指向含 playwright-core 的 node_modules）
// 输出：AFTER_FIRST_CAPTURE / AFTER_RECONNECT_REUSE / AFTER_RECONNECT_REUSE_2 /
//   AFTER_DISCONNECT_RECAPTURE（各步 gdm 计数）+ HARNESS_PASS=<true|false>。
const http = require('http');
const fs = require('fs');
const path = require('path');
const { chromium } = require('playwright-core');

const PORT = parseInt(process.env.WEB_SERVE_PORT || '38099', 10);
const WEB_ROOT = path.resolve(__dirname, '..', 'web');

const MIME = { '.html': 'text/html', '.js': 'text/javascript', '.css': 'text/css' };
const server = http.createServer((req, res) => {
  const urlPath = decodeURIComponent((req.url || '/').split('?')[0]);
  const rel = urlPath === '/' ? '/sip-publisher.html' : urlPath;
  const file = path.join(WEB_ROOT, rel);
  if (!file.startsWith(WEB_ROOT) || !fs.existsSync(file)) {
    res.writeHead(404); res.end('404'); return;
  }
  res.writeHead(200, { 'Content-Type': MIME[path.extname(file)] || 'application/octet-stream' });
  res.end(fs.readFileSync(file));
});

(async () => {
  await new Promise(r => server.listen(PORT, '127.0.0.1', r));
  const browser = await chromium.launch({
    channel: 'msedge', headless: true,
    args: [
      '--no-sandbox',
      '--use-fake-ui-for-media-stream',
      '--use-fake-device-for-media-stream',
      '--auto-accept-this-tab-capture',
      '--enable-usermedia-screen-capturing',
    ],
  });
  const page = await browser.newPage();
  page.on('pageerror', e => console.log('PAGEERROR: ' + e.message));
  await page.addInitScript(() => {
    window.__gdmCalls = 0;
    const orig = navigator.mediaDevices.getDisplayMedia.bind(navigator.mediaDevices);
    navigator.mediaDevices.getDisplayMedia = async (...args) => {
      window.__gdmCalls++;
      return orig(...args);
    };
  });
  await page.goto(`http://127.0.0.1:${PORT}/sip-publisher.html`);
  const gdm = () => page.evaluate(() => window.__gdmCalls);

  await page.evaluate(() => startCapture());
  const afterFirst = await gdm();
  console.log('AFTER_FIRST_CAPTURE gdm=' + afterFirst);

  await page.evaluate(() => cleanupAll(false));   // 重连：保留媒体轨
  await page.evaluate(() => startCapture());
  const afterReconnect = await gdm();
  console.log('AFTER_RECONNECT_REUSE gdm=' + afterReconnect);

  await page.evaluate(() => cleanupAll(false));   // 再一轮重连
  await page.evaluate(() => startCapture());
  const afterReconnect2 = await gdm();
  console.log('AFTER_RECONNECT_REUSE_2 gdm=' + afterReconnect2);

  await page.evaluate(() => cleanupAll(true));    // 主动断开：stop 媒体轨
  await page.evaluate(() => startCapture());
  const afterDisconnect = await gdm();
  console.log('AFTER_DISCONNECT_RECAPTURE gdm=' + afterDisconnect);

  const pass = afterFirst === 1 && afterReconnect === 1 && afterReconnect2 === 1 && afterDisconnect === 2;
  console.log('HARNESS_PASS=' + pass);
  await browser.close();
  server.close();
  process.exit(pass ? 0 : 2);
})().catch(e => { console.error('E2E FAIL:', e.message); try { server.close(); } catch (_) {} process.exit(1); });
