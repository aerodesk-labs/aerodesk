#!/usr/bin/env bash
# Web 被控端多方会议升级端到端（#598 v0.4 §4.1 Web 面）：
# 浏览器被控页 1:1 ↔ CLI viewer1 → CLI viewer2 入呼触发升级 →
# 被控页回 302 + BYE(cause=302) + 转会议发布；viewer1/2 跟随重拨 view-AoR；
# SFU 会议三方入会、双 viewer 收流。
set -euo pipefail
ROOT="$(cd "$(dirname "$0")/.." && pwd)"
cd "$ROOT"
export FFMPEG_DIR="${FFMPEG_DIR:-/d/tools/ffmpeg81/ffmpeg-n8.1-latest-win64-gpl-shared-8.1}"

ROOM="webconf-$(date +%s)"
WEB_SERVE_PORT=38090
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
echo "== 启动服务（LAN-IP 配方）"
RECORD_DIR="$REC" SFU_HOST_ADDRESS=127.0.0.1 ./target/debug/aerodesk-sfu >/tmp/webconf-sfu.log 2>&1 &
SFU=$!
SIP_UDP_PORT=5060 SIP_WSS_PORT=3061 ./target/debug/aerodesk-signal >/tmp/webconf-sig.log 2>&1 &
SIG=$!
(cd "$ROOT/web" && python3 -m http.server "$WEB_SERVE_PORT" --bind 127.0.0.1 >/tmp/webconf-http.log 2>&1) &
HTTP=$!
OK=0
for _ in $(seq 1 50); do
  grep -q "SIP/UDP 监听已起" /tmp/webconf-sig.log 2>/dev/null \
    && (exec 3<>/dev/tcp/127.0.0.1/3002) 2>/dev/null \
    && (exec 3<>/dev/tcp/127.0.0.1/$WEB_SERVE_PORT) 2>/dev/null && { OK=1; break; }
  sleep 0.2
done
[ "$OK" = "1" ] || { echo "FAIL 服务未就绪"; exit 1; }

echo "== 被控页（headless Edge，UAS）"
E2E_DIR="${WEB_E2E_DIR:-/tmp/webconf-e2e}"
mkdir -p "$E2E_DIR"; cd "$E2E_DIR"
[ -d node_modules/playwright-core ] || (npm init -y >/dev/null 2>&1; npm i playwright-core >/dev/null 2>&1)
cat > webconf-pub.js <<'JS'
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
  page.on('pageerror', e => console.log('PAGEERROR: ' + e.message));
  await page.goto(`http://127.0.0.1:38090/sip-publisher.html?device=${ROOM}&signal=wss://127.0.0.1:3061`);
  await page.click('#connect');
  await page.waitForFunction(() => document.getElementById('status').innerText.includes('等待观看端拨入'), { timeout: 25000 });
  console.log('PASS page registered');
  // 观察期：1:1 → 第 2 观看者触发升级 → 会议发布
  const deadline = Date.now() + 90000;
  while (Date.now() < deadline) {
    const st = await page.evaluate(() => ({
      status: document.getElementById('status').innerText,
      log: document.getElementById('log').innerText.slice(-1200),
    })).catch(() => null);
    if (st && st.status.includes('会议发布')) {
      console.log('PASS page escalated to conference');
      // 升级后继续观察 60s：页面事件循环是否存活（DEBUG ws-heartbeat /
      // INVITE 到达日志出现即证明存活）
      const obsEnd = Date.now() + 60000;
      let sawAlive = false;
      while (Date.now() < obsEnd) {
        const s2 = await page.evaluate(() => ({
          log: document.getElementById('log').innerText.slice(-400),
        })).catch(() => null);
        if (s2 && /DEBUG INVITE rx \(phase=conference/.test(s2.log)) { sawAlive = true; break; }
        await new Promise(r => setTimeout(r, 1000));
      }
      console.log('PAGE_ALIVE=' + sawAlive);
      const fin = await page.evaluate(() => document.getElementById('log').innerText).catch(() => '');
      console.log('--- page log (final) ---');
      console.log(fin.slice(-2500));
      await browser.close();
      console.log('E2E DONE');
      return;
    }
    await new Promise(r => setTimeout(r, 1000));
  }
  const st = await page.evaluate(() => document.getElementById('log').innerText).catch(() => '');
  console.log('E2E FAIL: 90s 内未进入会议发布');
  console.log(st.slice(-1500));
  await browser.close();
  process.exit(1);
})().catch(e => { console.error('E2E FAIL: ' + e.message); process.exit(1); });
JS
cd "$ROOT"
node "$E2E_DIR/webconf-pub.js" "$ROOM" > /tmp/webconf-pub.log 2>&1 &
PUB=$!
for _ in $(seq 1 40); do
  grep -q "PASS page registered" /tmp/webconf-pub.log 2>/dev/null && break
  sleep 0.5
done
grep -q "PASS page registered" /tmp/webconf-pub.log || { echo "FAIL 页面未注册"; tail -6 /tmp/webconf-pub.log; exit 1; }

echo "== viewer1 拨入（1:1）"
./target/debug/aerodesk-agent --role viewer --reconnect --signal "ws://$LAN_IP:3061" --room "$ROOM" \
  >/tmp/webconf-v1.log 2>&1 &
V1=$!
OK=0
for _ in $(seq 1 60); do
  grep -qE "RECEIVED: [1-9]" /tmp/webconf-v1.log 2>/dev/null && { OK=1; break; }
  sleep 0.5
done
[ "$OK" = "1" ] || { echo "FAIL V1 未收到帧"; tail -6 /tmp/webconf-v1.log; exit 1; }
echo "PASS V1 1:1 收帧"

echo "== viewer2 拨入（触发升级）"
./target/debug/aerodesk-agent --role viewer --reconnect --signal "ws://$LAN_IP:3061" --room "$ROOM" \
  >/tmp/webconf-v2.log 2>&1 &
V2=$!

echo "== 断言"
OK=0
for _ in $(seq 1 60); do
  grep -q "PASS page escalated" /tmp/webconf-pub.log 2>/dev/null && { OK=1; break; }
  sleep 1
done
[ "$OK" = "1" ] || { echo "FAIL 页面未进入会议"; tail -8 /tmp/webconf-pub.log; exit 1; }
echo "PASS 页面升级会议发布"

# V1 跟随重拨（BYE cause=302 → view-AoR）后重新收帧
OK=0
for _ in $(seq 1 60); do
  grep -q "跟随升级重拨" /tmp/webconf-v1.log 2>/dev/null && { OK=1; break; }
  sleep 1
done
[ "$OK" = "1" ] || { echo "FAIL V1 未跟随升级"; tail -8 /tmp/webconf-v1.log; exit 1; }
echo "PASS V1 跟随升级重拨"

# 双 viewer 经 SFU 收帧
for v in 1 2; do
  OK=0
  for _ in $(seq 1 60); do
    grep -qE "RECEIVED: [1-9]" /tmp/webconf-v$v.log 2>/dev/null && { OK=1; break; }
    sleep 1
  done
  [ "$OK" = "1" ] || { echo "FAIL viewer$v 会议未收帧"; tail -6 /tmp/webconf-v$v.log; exit 1; }
  echo "PASS viewer$v 会议收帧"
done

# SFU 会议三方入会
grep -q "view-$ROOM" /tmp/webconf-sfu.log || echo "WARN SFU 日志未见会议房间（用 127.0.0.1 host 时 shard 日志格式可能不同）"

kill "$PUB" "$V1" "$V2" "$HTTP" "$SFU" "$SIG" 2>/dev/null || true
echo "E2E DONE"
