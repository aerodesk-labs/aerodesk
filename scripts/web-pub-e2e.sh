#!/usr/bin/env bash
# Web 被控端端到端（#598 P2a：JSON WSS 面 → SIP-WSS）：headless 浏览器被控页
# （sip-publisher.html，UAS 静默接听）→ CLI viewer（SIP UDP 拨入）收流断言。
# 屏幕共享用 fake device；viewer 走 LAN-IP（浏览器通告 LAN/假 IP 候选，viewer
# 绑回环会 ICE 失败——LAN-IP 配方见 web-sip-wss-design.md §4.1）。
# 依赖：cargo build、Playwright（npm i playwright-core）、Chrome（BROWSER env）、
# python3（静态服务 web/）。
# 用法: scripts/web-pub-e2e.sh [room]
set -euo pipefail
ROOT="$(cd "$(dirname "$0")/.." && pwd)"
cd "$ROOT"

ROOM="${1:-webpub-$(date +%s)}"
E2E_DIR="${WEB_E2E_DIR:-/tmp/web-pub-e2e}"
WEB_SERVE_PORT="${WEB_SERVE_PORT:-38082}"
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
      '--ignore-certificate-errors',      // 3061 为自签 WSS（RFC 7118）
    ],
  });
  // 不单靠 launch flag：context 级 ignoreHTTPSErrors 走 CDP
  // Security.setIgnoreCertificateErrors，对 wss 一并生效且跨渠道稳定。
  const ctx = await browser.newContext({ ignoreHTTPSErrors: true });
  const page = await ctx.newPage();
  // 观测转发：失败原因必须落进本 stdout（node log），否则 bash 侧 tail 抓空。
  page.on('console', m => console.log('[console]', m.text()));
  page.on('pageerror', e => console.error('[pageerror]', e.message));
  page.on('requestfailed', r => console.log('[requestfailed]', r.url(), r.failure() && r.failure().errorText));
  page.on('websocket', ws => { console.log('[ws open]', ws.url()); ws.on('close', () => console.log('[ws closed]')); });
  try {
    await page.goto(`http://127.0.0.1:${process.env.WEB_SERVE_PORT || 38082}/sip-publisher.html?device=${ROOM}&signal=wss://127.0.0.1:3061`);
    await page.click('#connect');
    // 15s < bash 注册门 30s：TimeoutError + 页面快照必须先于 bash 门放弃落盘。
    await page.waitForFunction(() => document.getElementById('status').innerText.includes('等待观看端拨入'), { timeout: 15000 });
    console.log('PASS page registered, waiting INVITE');
    await page.waitForFunction(() => document.getElementById('status').innerText.includes('已接听'), { timeout: 30000 });
    console.log('PASS page answered call (200 OK)');
    // 保持发布 12s（让 CLI viewer 收到关键帧 + 若干帧）
    await new Promise(r => setTimeout(r, 12000));
    await browser.close();
    console.log('E2E DONE');
  } catch (e) {
    // 失败现场快照：页面 status/log 随错误一并输出（此前 tail 恒为空文件的盲区）。
    let extra = '';
    try {
      const s = await page.evaluate(() => ({
        status: document.getElementById('status').innerText,
        log: document.getElementById('log').innerText.slice(-800),
      }));
      extra = ' ' + JSON.stringify(s);
    } catch (_) { /* 页面未及创建/已关闭：无快照可抓 */ }
    console.error('E2E FAIL:', e.message + extra);
    process.exit(1);
  }
})();
JS

cd "$ROOT"
echo "== 构建"
cargo build -q -p aerodesk-sfu -p aerodesk-signal -p aerodesk-agent
echo "== 启动服务"
# 前置 e2e 可能残留 SFU/signal 占用端口 → 先清理，避免 bind 失败。
pkill -f "aerodesk-sfu|aerodesk-signal" 2>/dev/null || true
sleep 1
REC="$(mktemp -d)"
RECORD_DIR="$REC" "$ROOT/target/debug/aerodesk-sfu" >/tmp/webpub-sfu.log 2>&1 &
SFU=$!
# #598 P2a：SIP-WSS 面（3061）承载浏览器信令；静态服务 web/。
SIP_WSS_PORT=3061 SIP_UDP_PORT=5060 "$ROOT/target/debug/aerodesk-signal" >/tmp/webpub-sig.log 2>&1 &
SIG=$!
# 重试残留清理 + 就绪门：ci-retry 重跑时上轮 http.server 可能占口/半死；
# 未就绪即跑 node 会被 macOS 丢 SYN（ERR_CONNECTION_TIMED_OUT 实测）。
pkill -f "http.server $WEB_SERVE_PORT" 2>/dev/null || true
(cd "$ROOT/web" && python3 -m http.server "$WEB_SERVE_PORT" --bind 127.0.0.1 >/tmp/webpub-http.log 2>&1) &
HTTP=$!
HTTP_OK=0
for _ in $(seq 1 50); do
    if (exec 3<>/dev/tcp/127.0.0.1/$WEB_SERVE_PORT) 2>/dev/null; then HTTP_OK=1; break; fi
    if ! kill -0 "$HTTP" 2>/dev/null; then break; fi
    sleep 0.5
done
[ "$HTTP_OK" = "1" ] || { echo "FAIL: web 静态服务未就绪（$WEB_SERVE_PORT）"; tail -10 /tmp/webpub-http.log; exit 1; }
# HTTP 静态服务就绪门（此前无探活——node goto 撞上启动空窗即白跑）。
WEB_OK=0
for _ in $(seq 1 50); do
  if (exec 3<>/dev/tcp/127.0.0.1/"$WEB_SERVE_PORT") 2>/dev/null; then WEB_OK=1; break; fi
  sleep 0.2
done
[ "$WEB_OK" = "1" ] || { echo "FAIL: web http.server 未就绪"; tail -10 /tmp/webpub-http.log; exit 1; }
# signal 就绪门：SIP/UDP + SIP/WSS 两行监听日志 + SFU TCP 3002 探活。WSS 单列防
# TLS/证书加载失败被 UDP 单条件漏放行（浏览器侧 wss 连不上即注册不了）。
OK=0
for _ in $(seq 1 50); do
    if grep -q "SIP/UDP 监听已起" /tmp/webpub-sig.log 2>/dev/null \
        && grep -q "SIP/WSS 监听已起" /tmp/webpub-sig.log 2>/dev/null \
        && (exec 3<>/dev/tcp/127.0.0.1/3002) 2>/dev/null; then OK=1; break; fi
    if ! kill -0 "$SFU" 2>/dev/null; then break; fi
    sleep 0.2
done
if [ "$OK" != "1" ]; then
  echo "FAIL: SFU/signal 未就绪；sfu log:"; tail -20 /tmp/webpub-sfu.log; exit 1
fi
echo "PASS signal ready (SIP/UDP + SIP/WSS + SFU TCP 3002)"
# LAN-IP 配方：viewer 信令走本机出口 IP（浏览器通告 LAN 候选；绑回环 ICE 必败）。
LAN_IP=$(python3 - <<'PY'
import socket
s = socket.socket(socket.AF_INET, socket.SOCK_DGRAM)
try:
    s.connect(('8.8.8.8', 80))
    print(s.getsockname()[0])
except Exception:
    print('127.0.0.1')
PY
)
echo "  viewer 信令 LAN-IP=$LAN_IP"
# 时序（UAS 语义）：页面先注册（node 起、等 '等待观看端拨入'），
# 再起 CLI viewer 拨入——viewer 先拨会因无绑定走会议桥（503/空白会话）。
set +e
WEB_SERVE_PORT="$WEB_SERVE_PORT" node "$E2E_DIR/e2e-pub.js" "$ROOM" >/tmp/webpub-node.log 2>&1 &
NODE=$!
set -e
OK=0
# 60×0.5s=30s > js 内 waitForFunction 15s+launch/goto：js 的 TimeoutError+快照
# 必然先落盘，tail 不再抓到空文件。
for _ in $(seq 1 60); do
  grep -q "PASS page registered" /tmp/webpub-node.log 2>/dev/null && OK=1 && break
  sleep 0.5
done
if [ "$OK" != "1" ]; then
  echo "FAIL 页面未注册就绪"
  echo "--- node.log ---"; tail -15 /tmp/webpub-node.log
  echo "--- sig.log ---"; tail -20 /tmp/webpub-sig.log
  echo "--- http.log ---"; tail -10 /tmp/webpub-http.log
  exit 1
fi

# CLI viewer 作为观看端：断言能收到 Web 被控端发布的媒体帧。
"$ROOT/target/debug/aerodesk-agent" --role viewer --signal "ws://$LAN_IP:3061" --room "$ROOM" >/tmp/webpub-view.log 2>&1 &
VIEW=$!
wait "$NODE"
RES=$?
sleep 3

kill "$VIEW" "$HTTP" "$SFU" "$SIG" 2>/dev/null || true
# 断言观看端收到 Web 被控端的媒体（RECEIVED 行 + 帧数 > 0）
if grep -qE "RECEIVED: [1-9][0-9]* frames" /tmp/webpub-view.log; then
  echo "PASS CLI viewer received frames from web publisher"
else
  echo "FAIL viewer frames"; tail -8 /tmp/webpub-view.log; RES=1
fi
rm -rf "$REC"
exit $RES
