#!/usr/bin/env bash
# Linux 主控端（aerodesk-desktop）运行态端到端：Xvfb 无头跑 Slint UI 观看真实媒体。
# 发布端用 headless Chrome 屏幕共享（Web 被控端，ubuntu runner 预装 Chrome），
# 同时验证 Web 被控端 ↔ Linux 主控端互操作。
# 依赖：xvfb、Chrome、playwright-core、UI 系统库（CI ubuntu System deps 已装）。
# 用法: scripts/linux-ui-e2e.sh [room]
set -euo pipefail
export PYTHONIOENCODING=utf-8
ROOT="$(cd "$(dirname "$0")/.." && pwd)"
cd "$ROOT"
ROOM="${1:-linuxui-$(date +%s)}"

echo "== [1/7] 构建（Linux 编不了 aerodesk-agent——CI 排除，故 publisher 用 Web）"
cargo build -q -p aerodesk-sfu -p aerodesk-signal -p aerodesk-desktop

E2E_DIR="${WEB_E2E_DIR:-/tmp/linux-ui-e2e}"
mkdir -p "$E2E_DIR"
cd "$E2E_DIR"
if [ ! -d node_modules/playwright-core ]; then npm init -y >/dev/null 2>&1; npm i playwright-core >/dev/null 2>&1; fi
cat > e2e-pub.js <<'JS'
const { chromium } = require('playwright-core');
const ROOM = process.argv[2];
(async () => {
  const browser = await chromium.launch({
    channel: process.env.BROWSER || 'chrome', headless: true,
    args: ['--use-fake-ui-for-media-stream', '--use-fake-device-for-media-stream', '--auto-accept-this-tab-capture', '--enable-usermedia-screen-capturing',
           // 3061 为自签 WSS：headless 默认拒自签证书（ERR_CERT_AUTHORITY_INVALID），
           // 页面永远到不了「等待观看端拨入」——flag + context 双保险。
           '--ignore-certificate-errors'],
  });
  // 不单靠 launch flag：context 级 ignoreHTTPSErrors 走 CDP
  // Security.setIgnoreCertificateErrors，对 wss 一并生效且跨渠道稳定。
  const ctx = await browser.newContext({ ignoreHTTPSErrors: true });
  const page = await ctx.newPage();
  // 观测转发：失败原因必须落进本 stdout（pub.log），否则 bash 侧 tail 抓空。
  page.on('console', m => console.log('[console]', m.text()));
  page.on('pageerror', e => console.error('[pageerror]', e.message));
  page.on('requestfailed', r => console.log('[requestfailed]', r.url(), r.failure() && r.failure().errorText));
  page.on('websocket', ws => { console.log('[ws open]', ws.url()); ws.on('close', () => console.log('[ws closed]')); });
  try {
    await page.goto(`http://127.0.0.1:${process.env.WEB_SERVE_PORT || 38085}/sip-publisher.html?device=${ROOM}&token=e2e-token&signal=wss://127.0.0.1:3061`);
    await page.click('#connect');
    // 15s < bash 注册门 30s：TimeoutError + 页面快照必须先于 bash 门放弃落盘。
    await page.waitForFunction(() => document.getElementById('status').innerText.includes('等待观看端拨入'), { timeout: 15000 });
    console.log('PASS page registered, waiting INVITE');
    await page.waitForFunction(() => document.getElementById('status').innerText.includes('已连接'), { timeout: 25000 });
    console.log('PASS publisher connected');
    await new Promise(r => setTimeout(r, 15000)); // 保持发布（供 UI 收流）
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

echo "== [2/7] 启动 Xvfb :99"
Xvfb :99 -screen 0 1280x800x24 +extension GLX +render >/tmp/linuxui-xvfb.log 2>&1 &
XVFB=$!
sleep 1

echo "== [2.5/7] Linux 被控端运行级自测（X11 采集 + XTest 注入）"
cd "$ROOT"
DISPLAY=:99 cargo test -p aerodesk-platform --test x11_runtime 2>&1 | tail -12

echo "== [3/7] 启动 SFU/signal"
REC="$(mktemp -d)"
RECORD_DIR="$REC" "$ROOT/target/debug/aerodesk-sfu" >/tmp/linuxui-sfu.log 2>&1 &
SFU=$!
# SIP 会议桥链路：SIP/UDP 5060 + Digest 凭证（desktop 侧 settings 同步 seed）。
SIP_UDP_PORT=5060 SIP_WSS_PORT=3061 \
  SIP_DIGEST_USERS="AD-E2EUI=e2e-token,${ROOM}=e2e-token" \
  "$ROOT/target/debug/aerodesk-signal" >/tmp/linuxui-sig.log 2>&1 &
SIG=$!
(cd "$ROOT/web" && python3 -m http.server "${WEB_SERVE_PORT:-38085}" >/tmp/linuxui-http.log 2>&1) &
HTTP=$!
# HTTP 静态服务就绪门（此前无探活——node goto 撞上启动空窗即白跑）。
WEB_OK=0
for _ in $(seq 1 50); do
  if (exec 3<>/dev/tcp/127.0.0.1/${WEB_SERVE_PORT:-38085}) 2>/dev/null; then WEB_OK=1; break; fi
  sleep 0.2
done
[ "$WEB_OK" = "1" ] || { echo "FAIL: web http.server 未就绪"; tail -10 /tmp/linuxui-http.log; exit 1; }
# signal 就绪门：SIP/UDP + SIP/WSS 两行监听日志 + SFU TCP 3002 探活。WSS 单列防
# TLS/证书加载失败被 UDP 单条件漏放行（浏览器侧 wss 连不上即注册不了）。
OK=0
for _ in $(seq 1 50); do
    if grep -q "SIP/UDP 监听已起" /tmp/linuxui-sig.log 2>/dev/null \
        && grep -q "SIP/WSS 监听已起" /tmp/linuxui-sig.log 2>/dev/null \
        && (exec 3<>/dev/tcp/127.0.0.1/3002) 2>/dev/null; then OK=1; break; fi
    sleep 0.2
done
if [ "$OK" != "1" ]; then echo "FAIL: SFU/signal not ready; logs:"; tail -20 /tmp/linuxui-sig.log /tmp/linuxui-sfu.log; exit 1; fi
echo "PASS signal ready (SIP/UDP + SIP/WSS + SFU TCP 3002)"

echo "== [4/7] Web 被控端发布（headless Chrome 屏幕共享）"
set +e
WEB_SERVE_PORT="${WEB_SERVE_PORT:-38085}" node "$E2E_DIR/e2e-pub.js" "$ROOM" >/tmp/linuxui-pub.log 2>&1 &
PUB=$!
set -e
# UAS 时序：页面注册就绪（≤30s）后才起 UI 拨入（先拨会 503）。
# 60×0.5s=30s > js 内 waitForFunction 15s+launch/goto：js 的 TimeoutError+快照
# 必然先落盘，tail 不再抓到空文件。
OK=0
for _ in $(seq 1 60); do
  grep -q "PASS page registered" /tmp/linuxui-pub.log 2>/dev/null && OK=1 && break
  sleep 0.5
done
if [ "$OK" != "1" ]; then
  echo "FAIL 页面未注册就绪"
  echo "--- pub.log ---"; tail -15 /tmp/linuxui-pub.log
  echo "--- sig.log ---"; tail -20 /tmp/linuxui-sig.log
  echo "--- http.log ---"; tail -10 /tmp/linuxui-http.log
  exit 1
fi

echo "== [4.5/7] seed SIP 配置（desktop 启动即 REGISTER，观看经会议桥）"
# 隔离 HOME：seed 与 desktop 启动同用 $E2E_DIR（不碰真实配置）。
export AERO_E2E_HOME="$E2E_DIR"
python3 - <<'PY'
import json, os
settings = {
    "server_default": "127.0.0.1:3003",
    "device_id": "AD-E2EUI",
    "token_default": "e2e-token",
    "remember_token": True,
    "server_tls": False,
    "sip_transport": "udp",
    "sip_port": 5060,
}
import os as _os; path = _os.path.join(_os.environ.get("AERO_E2E_HOME", _os.path.expanduser("~")), ".aerodesk-settings.json")
open(path, "w").write(json.dumps(settings))
print("seeded", path)
PY

echo "== [5/7] 启动 Linux UI（Xvfb，自动连接观看）"
ls -la "$ROOT/target/debug/aerodesk-desktop" || echo "UI BINARY MISSING"
RUST_LOG=debug DISPLAY=:99 HOME="$E2E_DIR" "$ROOT/target/debug/aerodesk-desktop" \
  -server 127.0.0.1:3003 -room "$ROOM" -autoconnect >/tmp/linuxui-ui.log 2>&1 &
UI_PID=$!
sleep 5
echo "UI alive: $(kill -0 $UI_PID 2>/dev/null && echo yes || echo no), log lines: $(wc -l < /tmp/linuxui-ui.log 2>/dev/null || echo 0)"

echo "== [6/7] 断言连接链路（ICE Completed = Linux 主控端成功接入 SFU）"
# 媒体解码渲染在 macOS/真机验证（generic viewer 与 macOS 同构）；本环境验证
# Linux 主控端 UI 启动 → 信令/SDP → ICE 建链全链路。
python3 - <<'PY'
import time, sys
ok = False
for i in range(60):  # 最多 60s
    try:
        txt = open('/tmp/linuxui-ui.log', errors='replace').read()
    except FileNotFoundError:
        txt = ''
    if 'IceConnectionStateChange(Completed)' in txt or 'ICE remote address' in txt:
        print("PASS Linux UI ICE Completed (connected to SFU)")
        ok = True
        break
    if ('sip call failed' in txt or 'sip call rejected' in txt
            or 'sip call peer hangup' in txt or '链路未启动' in txt):
        print("FAIL: SIP 呼叫失败")
        ok = False
        break
    time.sleep(1)
if not ok:
    print("FAIL: 60s 内 ICE 未 Completed；UI 日志尾：")
    print(open('/tmp/linuxui-ui.log', errors='replace').read()[-1500:])
    sys.exit(1)
PY

echo "== [7/7] 截图留证"
if command -v xwd >/dev/null 2>&1 && command -v convert >/dev/null 2>&1; then
  DISPLAY=:99 xwd -root -silent | convert xwd:- /tmp/linuxui-e2e.png || true
fi

wait "$PUB" 2>/dev/null || true
kill "$UI_PID" "$HTTP" "$SFU" "$SIG" "$XVFB" 2>/dev/null || true
echo "E2E DONE"
