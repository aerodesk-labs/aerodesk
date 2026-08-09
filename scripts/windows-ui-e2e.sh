#!/usr/bin/env bash
# Windows 主控端（aerodesk-ui）运行态端到端：构建 → 本地 SFU → Web 被控端发布
# （headless Edge 屏幕共享）→ Windows UI 自动连接观看 → 断言 ICE Completed。
# 依赖：cargo、node/playwright-core、Edge（windows runner 预装）、UI 编译通过（#177）。
# 用法: scripts/windows-ui-e2e.sh [room]  （Git Bash）
set -euo pipefail
ROOT="$(cd "$(dirname "$0")/.." && pwd)"
cd "$ROOT"
ROOM="${1:-winui-$(date +%s)}"

echo "== [1/6] 构建（Windows）"
cargo build -q -p aerodesk-sfu -p aerodesk-signal -p aerodesk-ui

E2E_DIR="${WEB_E2E_DIR:-/tmp/win-ui-e2e}"
mkdir -p "$E2E_DIR"
cd "$E2E_DIR"
if [ ! -d node_modules/playwright-core ]; then npm init -y >/dev/null 2>&1; npm i playwright-core >/dev/null 2>&1; fi
cat > e2e-pub.js <<'JS'
const { chromium } = require('playwright-core');
const ROOM = process.argv[2];
(async () => {
  const browser = await chromium.launch({
    channel: 'msedge', headless: true,
    args: ['--use-fake-ui-for-media-stream', '--use-fake-device-for-media-stream', '--auto-accept-this-tab-capture', '--enable-usermedia-screen-capturing'],
  });
  const page = await browser.newPage();
  await page.goto(`http://127.0.0.1:3002/?room=${ROOM}&role=publisher&signal=ws://127.0.0.1:3003/ws`);
  await page.click('#connect');
  await page.waitForFunction(() => document.getElementById('log').innerText.includes('屏幕共享已授权'), { timeout: 20000 });
  console.log('PASS screen shared');
  await page.waitForFunction(() => document.getElementById('status').innerText.includes('已连接'), { timeout: 25000 });
  console.log('PASS publisher connected');
  await new Promise(r => setTimeout(r, 15000));
  await browser.close();
  console.log('E2E DONE');
})().catch(e => { console.error('E2E FAIL:', e.message); process.exit(1); });
JS

echo "== [2/6] 启动 SFU/signal（Windows）"
REC="$(mktemp -d)"
RECORD_DIR="$REC" "$ROOT/target/debug/aerodesk-sfu.exe" >/tmp/winui-sfu.log 2>&1 &
SFU=$!
"$ROOT/target/debug/aerodesk-signal.exe" >/tmp/winui-sig.log 2>&1 &
SIG=$!
python3 - <<'PY'
import socket, time, sys
ok = False
for _ in range(50):
    try:
        a = socket.create_connection(("127.0.0.1", 3003), 0.3); a.close()
        b = socket.create_connection(("127.0.0.1", 3002), 0.3); b.close()
        ok = True; break
    except OSError:
        time.sleep(0.2)
if not ok:
    print("FAIL: SFU/signal 未就绪"); sys.exit(1)
PY

echo "== [3/6] Web 被控端发布（headless Edge 屏幕共享）"
node "$E2E_DIR/e2e-pub.js" "$ROOM" &
PUB=$!
sleep 3

echo "== [4/6] 启动 Windows UI（自动连接观看）"
RUST_LOG=debug "$ROOT/target/debug/aerodesk-ui.exe" \
  -server 127.0.0.1:3003 -room "$ROOM" -autoconnect >/tmp/winui-ui.log 2>&1 &
UI_PID=$!
sleep 8
echo "UI alive: $(kill -0 $UI_PID 2>/dev/null && echo yes || echo no), log lines: $(wc -l < /tmp/winui-ui.log 2>/dev/null || echo 0)"

echo "== [5/6] 断言连接链路（ICE Completed = Windows 主控端成功接入 SFU）"
python3 - <<'PY'
import time, sys
ok = False
for i in range(60):
    try:
        txt = open('/tmp/winui-ui.log', errors='replace').read()
    except FileNotFoundError:
        txt = ''
    if 'IceConnectionStateChange(Completed)' in txt or 'ICE remote address' in txt:
        print("PASS Windows UI ICE Completed (connected to SFU)")
        ok = True
        break
    if 'generic viewer connect failed' in txt or 'connect TIMEOUT' in txt:
        print("FAIL: connect 失败/超时"); ok = False; break
    time.sleep(1)
if not ok:
    print("FAIL: 60s 内 ICE 未 Completed；UI 日志尾：")
    print(open('/tmp/winui-ui.log', errors='replace').read()[-1500:])
    sys.exit(1)
PY

echo "== [6/6] 清理"
taskkill //F //PID "$UI_PID" 2>/dev/null || true
taskkill //F //PID "$PUB" 2>/dev/null || true
taskkill //F //PID "$SFU" 2>/dev/null || true
taskkill //F //PID "$SIG" 2>/dev/null || true
echo "E2E DONE"
