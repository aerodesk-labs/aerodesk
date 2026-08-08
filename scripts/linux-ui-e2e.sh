#!/usr/bin/env bash
# Linux 主控端（aerodesk-ui）运行态端到端：Xvfb 无头跑 Slint UI 观看真实媒体。
# 发布端用 headless Chrome 屏幕共享（Web 被控端，ubuntu runner 预装 Chrome），
# 同时验证 Web 被控端 ↔ Linux 主控端互操作。
# 依赖：xvfb、Chrome、playwright-core、UI 系统库（CI ubuntu System deps 已装）。
# 用法: scripts/linux-ui-e2e.sh [room]
set -euo pipefail
ROOT="$(cd "$(dirname "$0")/.." && pwd)"
cd "$ROOT"
ROOM="${1:-linuxui-$(date +%s)}"

echo "== [1/7] 构建（Linux 编不了 aerodesk-cli——CI 排除，故 publisher 用 Web）"
cargo build -q -p aerodesk-sfu -p aerodesk-signal -p aerodesk-ui

E2E_DIR="${WEB_E2E_DIR:-/tmp/linux-ui-e2e}"
mkdir -p "$E2E_DIR"
cd "$E2E_DIR"
if [ ! -d node_modules/playwright-core ]; then npm init -y >/dev/null 2>&1; npm i playwright-core >/dev/null 2>&1; fi
cat > e2e-pub.js <<'JS'
const { chromium } = require('playwright-core');
const ROOM = process.argv[2];
(async () => {
  const browser = await chromium.launch({
    channel: 'chrome', headless: true,
    args: ['--use-fake-ui-for-media-stream', '--use-fake-device-for-media-stream', '--auto-accept-this-tab-capture', '--enable-usermedia-screen-capturing'],
  });
  const page = await browser.newPage();
  await page.goto(`http://127.0.0.1:3002/?room=${ROOM}&role=publisher&signal=ws://127.0.0.1:3003/ws`);
  await page.click('#connect');
  await page.waitForFunction(() => document.getElementById('log').innerText.includes('屏幕共享已授权'), { timeout: 20000 });
  console.log('PASS screen shared');
  await page.waitForFunction(() => document.getElementById('status').innerText.includes('已连接'), { timeout: 25000 });
  console.log('PASS publisher connected');
  await new Promise(r => setTimeout(r, 15000)); // 保持发布（供 UI 收流）
  await browser.close();
  console.log('E2E DONE');
})().catch(e => { console.error('E2E FAIL:', e.message); process.exit(1); });
JS

echo "== [2/7] 启动 Xvfb :99"
Xvfb :99 -screen 0 1280x800x24 +extension GLX +render >/tmp/linuxui-xvfb.log 2>&1 &
XVFB=$!
sleep 1

echo "== [3/7] 启动 SFU/signal"
REC="$(mktemp -d)"
RECORD_DIR="$REC" "$ROOT/target/debug/aerodesk-sfu" >/tmp/linuxui-sfu.log 2>&1 &
SFU=$!
"$ROOT/target/debug/aerodesk-signal" >/tmp/linuxui-sig.log 2>&1 &
SIG=$!
OK=0
for _ in $(seq 1 50); do
    if nc -z 127.0.0.1 3003 2>/dev/null && nc -z 127.0.0.1 3002 2>/dev/null; then OK=1; break; fi
    sleep 0.2
done
if [ "$OK" != "1" ]; then echo "FAIL: SFU/signal 未就绪"; tail -10 /tmp/linuxui-sfu.log; exit 1; fi

echo "== [4/7] Web 被控端发布（headless Chrome 屏幕共享）"
set +e
node "$E2E_DIR/e2e-pub.js" "$ROOM" &
PUB=$!
set -e
sleep 3

echo "== [5/7] 启动 Linux UI（Xvfb，自动连接观看）"
ls -la "$ROOT/target/debug/aerodesk-ui" || echo "UI BINARY MISSING"
DISPLAY=:99 "$ROOT/target/debug/aerodesk-ui" \
  -server 127.0.0.1:3003 -room "$ROOM" -autoconnect >/tmp/linuxui-ui.log 2>&1 &
UI_PID=$!
sleep 5
echo "UI alive: $(kill -0 $UI_PID 2>/dev/null && echo yes || echo no), log lines: $(wc -l < /tmp/linuxui-ui.log 2>/dev/null || echo 0)

echo "== [6/7] 断言解码帧"
python3 - <<'PY'
import time, re, sys
ok = False
for i in range(75):  # 最多 75s（Chrome 发布 + UI 收流）
    try:
        txt = open('/tmp/linuxui-ui.log', errors='replace').read()
    except FileNotFoundError:
        txt = ''
    m = re.findall(r'generic viewer: decoded (\d+) frames', txt)
    if m and int(m[-1]) >= 10:
        print(f"PASS generic viewer decoded >= 10 frames (last {m[-1]})")
        ok = True
        break
    time.sleep(1)
if not ok:
    print("FAIL: 75s 内未解码 10 帧；UI 日志尾：")
    print(open('/tmp/linuxui-ui.log', errors='replace').read()[-1500:])
    print("--- xvfb ---")
    print(open('/tmp/linuxui-xvfb.log', errors='replace').read()[-500:])
    sys.exit(1)
PY

echo "== [7/7] 截图留证"
if command -v xwd >/dev/null 2>&1 && command -v convert >/dev/null 2>&1; then
  DISPLAY=:99 xwd -root -silent | convert xwd:- /tmp/linuxui-e2e.png || true
fi

wait "$PUB" 2>/dev/null || true
kill "$UI_PID" "$SFU" "$SIG" "$XVFB" 2>/dev/null || true
echo "E2E DONE"
