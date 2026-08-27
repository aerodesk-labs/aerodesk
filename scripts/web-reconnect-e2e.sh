#!/usr/bin/env bash
# web-reconnect-e2e.sh —— Web 端自动重连（#175，SIP 版 #598 P2a）：
#   Playwright 双浏览器（sip-publisher.html 被控页 + sip-viewer.html 观看页，
#   SIP-WSS 1:1）→ 初始收流 → 杀 SFU+signal → 重启 → 两页自动退避重连
#   （WS close → 1s/2s/4s/8s 退避 → 重新 REGISTER；viewer 重建会话再收流）→
#   viewer 恢复 video。媒体为 1:1 直连（不经 SFU），服务重启只断信令面——
#   #175 的信令韧性断言语义保留。
# 依赖：cargo build、playwright-core、Edge/Chrome（BROWSER 可选，默认 msedge）、
# python3（静态服务 web/）。
set -euo pipefail
ROOT="$(cd "$(dirname "$0")/.." && pwd)"
cd "$ROOT"
export RUST_LOG="${RUST_LOG:-info}"

ROOM="webrec-$(date +%s)"
REC="$(mktemp -d)"
WEB_SERVE_PORT="${WEB_SERVE_PORT:-38087}"

E2E_DIR="${WEB_E2E_DIR:-/tmp/web-e2e}"
mkdir -p "$E2E_DIR"; cd "$E2E_DIR"
if [ ! -d node_modules/playwright-core ]; then npm init -y >/dev/null 2>&1; npm i playwright-core >/dev/null 2>&1; fi

cat > web-reconnect-run.js <<JS
const { chromium } = require('playwright-core');
const ROOM = process.argv[2];
(async () => {
  const browser = await chromium.launch({
    channel: process.env.BROWSER || 'msedge', headless: true,
    args: [
      '--use-fake-ui-for-media-stream',
      '--use-fake-device-for-media-stream',
      '--auto-accept-this-tab-capture',
      '--enable-usermedia-screen-capturing',
      '--ignore-certificate-errors',      // 3061 为自签 WSS（RFC 7118）
    ],
  });
  const SP = process.env.WEB_SERVE_PORT || 38087;
  // 被控页（UAS；服务重启后同样自动重连）
  const pub = await browser.newPage();
  pub.on('pageerror', e => console.log('PUB_PAGEERROR: ' + e.message));
  await pub.goto('http://127.0.0.1:' + SP + '/sip-publisher.html?device=' + ROOM + '&signal=wss://127.0.0.1:3061');
  await pub.click('#connect');
  await pub.waitForFunction(() => document.getElementById('status').innerText.includes('等待观看端拨入'), { timeout: 40000 });
  console.log('PUBLISHER_OK');
  // 观看页（初始收流 → 服务重启后自动重连恢复）
  const view = await browser.newPage();
  view.on('pageerror', e => console.log('VIEW_PAGEERROR: ' + e.message));
  await view.goto('http://127.0.0.1:' + SP + '/sip-viewer.html?target=' + ROOM + '&signal=wss://127.0.0.1:3061');
  await view.click('#connect');
  await view.waitForFunction(() => document.getElementById('video').readyState >= 2, { timeout: 40000 });
  console.log('INITIAL_OK');
  // 服务被 bash 重启后，页面应自动重连——显式轮询打点（每一步可见，替代
  // waitForFunction 黑盒；页面内部 30s INVITE 超时也在此暴露）。
  const t0 = Date.now();
  let sawReconnect = false, sawConnected = false, sawVideo = false, gaveUp = false;
  while (Date.now() - t0 < 150000 && !sawVideo && !gaveUp) {
    const st = await view.evaluate(() => ({
      status: document.getElementById('status').innerText,
      log: document.getElementById('log').innerText.slice(-600),
      rs: document.getElementById('video').readyState,
    })).catch(() => null);
    if (st) {
      if (!sawReconnect && st.log.includes('自动重连')) { sawReconnect = true; console.log('RECONNECT_LOG_SEEN'); }
      if (!sawConnected && st.status.includes('已连接')) { sawConnected = true; console.log('CONNECTED_AGAIN'); }
      if (!sawVideo && st.rs >= 2) { sawVideo = true; console.log('VIDEO_BACK'); }
      if (st.status.includes('连接失败') || st.status.includes('启动失败')) {
        console.log('RECONNECT_FAILED status=' + st.status + ' | log=' + st.log);
        gaveUp = true;
      }
    }
    await new Promise(r => setTimeout(r, 1000));
  }
  console.log('RECONNECT_OK=' + (sawVideo ? 'true' : 'false'));
  console.log('FINAL_STATUS=' + await view.evaluate(() => document.getElementById('status').innerText).catch(() => '?'));
  console.log('FINAL_LOG=' + await view.evaluate(() => document.getElementById('log').innerText.slice(-800)).catch(() => '?'));
  await browser.close();
  console.log('E2E_DONE');
})().catch(e => { console.error('E2E_FAIL: ' + e.message); process.exit(1); });
JS

cd "$ROOT"
echo "== 构建"
cargo build -q -p aerodesk-sfu -p aerodesk-signal -p aerodesk-agent

start_sfu() {
    RECORD_DIR="$REC" \
      SFU_MEDIA_PORT=14578 SFU_SIGNAL_PORT=14500 SFU_INTERNAL_PORT=14502 \
      ./target/debug/aerodesk-sfu >/tmp/webrec-sfu.log 2>&1 &
    echo $! > /tmp/webrec-sfu.pid
}
start_signal() {
    # #598 P2a：浏览器信令走 SIP-WSS（3061）；UDP 5060 供 CLI（本脚本不用）。
    SIGNAL_PORT=14501 SFU_URL=http://127.0.0.1:14502 \
      SIP_UDP_PORT=5060 SIP_WSS_PORT=3061 ./target/debug/aerodesk-signal >/tmp/webrec-sig.log 2>&1 &
    echo $! > /tmp/webrec-sig.pid
}
stop_services() {
    if [ -f /tmp/webrec-sfu.pid ]; then kill "$(cat /tmp/webrec-sfu.pid)" 2>/dev/null || true; rm -f /tmp/webrec-sfu.pid; fi
    if [ -f /tmp/webrec-sig.pid ]; then kill "$(cat /tmp/webrec-sig.pid)" 2>/dev/null || true; rm -f /tmp/webrec-sig.pid; fi
    for _ in $(seq 1 50); do
        (exec 3<>/dev/tcp/127.0.0.1/14502) 2>/dev/null || break
        sleep 0.2
    done
}
wait_ports() {
    for _ in $(seq 1 50); do
        if grep -q "SIP/UDP 监听已起" /tmp/webrec-sig.log 2>/dev/null \
            && (exec 3<>/dev/tcp/127.0.0.1/14502) 2>/dev/null; then return 0; fi
        sleep 0.2
    done
    return 1
}

echo "== 启动服务"
start_sfu; start_signal
(cd "$ROOT/web" && python3 -m http.server "$WEB_SERVE_PORT" >/tmp/webrec-http.log 2>&1) &
HTTP=$!
wait_ports || { echo "FAIL: 服务未就绪"; exit 1; }

echo "== 启动 Playwright（被控页 + 观看页：初始收流 → 观察重连）"
cd "$E2E_DIR"
BROWSER="${BROWSER:-msedge}" WEB_SERVE_PORT="$WEB_SERVE_PORT" node web-reconnect-run.js "$ROOM" > /tmp/webrec-node.log 2>&1 &
NODE_PID=$!
cd "$ROOT"

# 等初始收流（两页均已连接）
INIT=0
for _ in $(seq 1 120); do
    grep -q INITIAL_OK /tmp/webrec-node.log 2>/dev/null && { INIT=1; break; }
    kill -0 $NODE_PID 2>/dev/null || break
    sleep 0.5
done
if [ "$INIT" != "1" ]; then
    echo "FAIL: 页面初始未收流"; tail -8 /tmp/webrec-node.log; kill $NODE_PID 2>/dev/null || true
    stop_services; exit 1
fi
echo "PASS 初始收流"

echo "== 杀服务 → 重启（模拟服务重启）"
stop_services
sleep 3
start_sfu; start_signal
wait_ports || echo "WARN: 重启后服务未就绪"

echo "== 等页面自动重连"
OK=0
for _ in $(seq 1 240); do
    if grep -q RECONNECT_OK /tmp/webrec-node.log 2>/dev/null; then OK=1; break; fi
    if ! kill -0 $NODE_PID 2>/dev/null; then break; fi
    sleep 0.5
done
if grep -q E2E_FAIL /tmp/webrec-node.log 2>/dev/null; then
    echo "FAIL: 页面重连失败"; tail -8 /tmp/webrec-node.log; exit 1
fi
[ "$OK" = "1" ] && echo "PASS 页面自动重连并恢复 video" || { echo "FAIL: 未重连"; tail -8 /tmp/webrec-node.log; exit 1; }

kill $NODE_PID $HTTP 2>/dev/null || true
stop_services
echo "E2E DONE"
