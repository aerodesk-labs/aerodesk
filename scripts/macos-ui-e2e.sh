#!/usr/bin/env bash
# macOS 主控端（aerodesk-desktop）运行态端到端（#487 矩阵缺口①）：
# macOS runner 自带真 WindowServer（无需 Xvfb），直接跑 Slint UI 观看 Web 被控端
# （headless Chrome 屏幕共享）发布的真实媒体，断言 ICE Completed。
# 同时是 #497/#498（mac 主控键码/修饰键修复）所在 mac 主控路径的 CI 回归护栏。
# mac 被控端运行态（SCK 采集/CGEvent 注入）需 TCC 授权，CI runner 无法自测，
# 仍属真机冒烟项（缺口⑤），本脚本不覆盖。
# 依赖：cargo、node/playwright-core、Chrome（macos runner 预装）、ffmpeg/x264（CI 已装）。
# 用法: scripts/macos-ui-e2e.sh [room]
set -euo pipefail
export PYTHONIOENCODING=utf-8
ROOT="$(cd "$(dirname "$0")/.." && pwd)"
cd "$ROOT"
ROOM="${1:-macosui-$(date +%s)}"

echo "== [1/6] 构建（macOS）"
cargo build -q -p aerodesk-sfu -p aerodesk-signal -p aerodesk-desktop

echo "== [2/6] 准备 Web 被控端（playwright-core + 预装 Chrome）"
E2E_DIR="${WEB_E2E_DIR:-/tmp/macos-ui-e2e}"
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

echo "== [3/6] 启动 SFU/signal"
cd "$ROOT"
REC="$(mktemp -d)"
RECORD_DIR="$REC" "$ROOT/target/debug/aerodesk-sfu" >/tmp/macosui-sfu.log 2>&1 &
SFU=$!
# SIP 会议桥链路（WSS 兜底已删 #576——desktop 观看必经 SIP）。
SIP_UDP_PORT=5060 "$ROOT/target/debug/aerodesk-signal" >/tmp/macosui-sig.log 2>&1 &
SIG=$!
OK=0
for _ in $(seq 1 80); do
    if nc -z 127.0.0.1 3003 2>/dev/null && nc -z 127.0.0.1 3002 2>/dev/null; then OK=1; break; fi
    sleep 0.2
done
if [ "$OK" != "1" ]; then echo "FAIL: SFU/signal not ready; logs:"; tail -20 /tmp/macosui-sig.log /tmp/macosui-sfu.log; exit 1; fi
# SIP/UDP 就绪门：signal 的 SIP 绑定失败是非致命（线程内 error!），TCP 探活会漏。
OK=0
for _ in $(seq 1 80); do
    if grep -q "SIP/UDP 监听已起" /tmp/macosui-sig.log 2>/dev/null; then OK=1; break; fi
    sleep 0.2
done
if [ "$OK" != "1" ]; then echo "FAIL: SIP/UDP 未就绪"; tail -10 /tmp/macosui-sig.log; exit 1; fi

echo "== [4/6] Web 被控端发布（headless Chrome 屏幕共享）"
set +e
node "$E2E_DIR/e2e-pub.js" "$ROOM" &
PUB=$!
set -e
sleep 3

echo "== [4.5/6] seed SIP 配置（desktop 启动即 REGISTER，观看经会议桥）"
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
path = os.path.join(os.environ.get("AERO_E2E_HOME", os.path.expanduser("~")), ".aerodesk-settings.json")
open(path, "w").write(json.dumps(settings))
print("seeded", path)
PY

echo "== [5/6] 启动 macOS UI（自动连接观看）并断言 ICE Completed"
ls -la "$ROOT/target/debug/aerodesk-desktop" || echo "UI BINARY MISSING"
RUST_LOG=debug HOME="$E2E_DIR" "$ROOT/target/debug/aerodesk-desktop" \
  -server 127.0.0.1:3003 -room "$ROOM" -autoconnect >/tmp/macosui-ui.log 2>&1 &
UI_PID=$!
sleep 8
if ! kill -0 $UI_PID 2>/dev/null; then
  echo "FAIL: UI 进程退出；日志："
  cat /tmp/macosui-ui.log 2>/dev/null || echo "(无日志)"
  exit 1
fi
echo "UI alive: yes, log lines: $(wc -l < /tmp/macosui-ui.log 2>/dev/null || echo 0)"
# macOS 全套媒体 e2e（web-e2e/web-pub-e2e 等）均在 macOS runner 完成 ICE 回环，
# 此处与 Linux UI e2e 同标准断言 Completed。
python3 - <<'PY'
import time, sys
ok = False
for i in range(150):  # 最多 60s
    try:
        txt = open('/tmp/macosui-ui.log', encoding='utf-8', errors='replace').read()
    except FileNotFoundError:
        txt = ''
    if 'IceConnectionStateChange(Completed)' in txt or 'ICE remote address' in txt:
        print("PASS macOS UI ICE Completed (connected to SFU)")
        ok = True
        break
    if ('sip call failed' in txt or 'sip call rejected' in txt
            or 'sip call peer hangup' in txt or '链路未启动' in txt):
        print("FAIL: connect 失败/超时")
        ok = False
        break
    time.sleep(1)
if not ok:
    print("FAIL: 150s 内 ICE 未 Completed；UI 日志尾：")
    print(open('/tmp/macosui-ui.log', encoding='utf-8', errors='replace').read()[-1500:])
    sys.exit(1)
PY

echo "== [6/6] 截图留证（best-effort，无 Screen Recording 授权时内容可能为空）+ 清理"
screencapture -x /tmp/macosui-e2e.png 2>/dev/null || true

wait "$PUB" 2>/dev/null || true
kill "$UI_PID" "$SFU" "$SIG" 2>/dev/null || true
echo "E2E DONE"
