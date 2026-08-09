#!/usr/bin/env bash
# web-reconnect-e2e.sh —— Web 端自动重连（#175）：
#   Playwright 打开观看页 → 初始收流 → 杀 SFU+signal → 重启 → 页面自动重连并恢复 video。
# 依赖：cargo build、playwright-core、Edge/Chrome（BROWSER 可选，默认 msedge）。
set -euo pipefail
ROOT="$(cd "$(dirname "$0")/.." && pwd)"
cd "$ROOT"
export RUST_LOG="${RUST_LOG:-info}"

ROOM="webrec-$(date +%s)"
REC="$(mktemp -d)"

E2E_DIR="${WEB_E2E_DIR:-/tmp/web-e2e}"
mkdir -p "$E2E_DIR"; cd "$E2E_DIR"
if [ ! -d node_modules/playwright-core ]; then npm init -y >/dev/null 2>&1; npm i playwright-core >/dev/null 2>&1; fi

cat > web-reconnect-run.js <<JS
const { chromium } = require('playwright-core');
const ROOM = process.argv[2];
(async () => {
  const browser = await chromium.launch({ channel: process.env.BROWSER || 'msedge', headless: true });
  const page = await browser.newPage();
  await page.goto('http://127.0.0.1:14502/?room=' + ROOM + '&role=viewer&signal=ws://127.0.0.1:14503/ws');
  await page.click('#connect');
  await page.waitForFunction(() => document.getElementById('video').readyState >= 2, { timeout: 40000 });
  console.log('INITIAL_OK');
  // 服务被 bash 重启后，页面应自动重连
  await page.waitForFunction(() => document.getElementById('log').innerText.includes('重连'), { timeout: 120000 });
  await page.waitForFunction(() => document.getElementById('status').innerText.includes('已连接'), { timeout: 90000 });
  await page.waitForFunction(() => document.getElementById('video').readyState >= 2, { timeout: 90000 });
  console.log('RECONNECT_OK');
  await browser.close();
  console.log('E2E_DONE');
})().catch(e => { console.error('E2E_FAIL: ' + e.message); process.exit(1); });
JS

cd "$ROOT"
echo "== 构建"
cargo build -q -p aerodesk-sfu -p aerodesk-signal -p aerodesk-cli

start_sfu() {
    RECORD_DIR="$REC" \
      SFU_MEDIA_PORT=14578 SFU_SIGNAL_PORT=14500 SFU_INTERNAL_PORT=14502 \
      ./target/debug/aerodesk-sfu >/tmp/webrec-sfu.log 2>&1 &
    echo $! > /tmp/webrec-sfu.pid
}
start_signal() {
    SIGNAL_PORT=14501 SIGNAL_PLAIN_PORT=14503 SFU_URL=http://127.0.0.1:14502 \
      ./target/debug/aerodesk-signal >/tmp/webrec-sig.log 2>&1 &
    echo $! > /tmp/webrec-sig.pid
}
stop_services() {
    if [ -f /tmp/webrec-sfu.pid ]; then kill "$(cat /tmp/webrec-sfu.pid)" 2>/dev/null || true; rm -f /tmp/webrec-sfu.pid; fi
    if [ -f /tmp/webrec-sig.pid ]; then kill "$(cat /tmp/webrec-sig.pid)" 2>/dev/null || true; rm -f /tmp/webrec-sig.pid; fi
    for p in 14578 14502 14503; do
        for _ in $(seq 1 50); do nc -z 127.0.0.1 "$p" 2>/dev/null || break; sleep 0.2; done
    done
}
wait_ports() {
    for _ in $(seq 1 50); do
        if nc -z 127.0.0.1 14502 2>/dev/null && nc -z 127.0.0.1 14503 2>/dev/null; then return 0; fi
        sleep 0.2
    done
    return 1
}

echo "== 启动服务 + publisher"
start_sfu; start_signal
wait_ports || { echo "FAIL: 服务未就绪"; exit 1; }
./target/debug/aerodesk-cli --role publisher --encoder x264 --noisy \
    --signal ws://127.0.0.1:14503 --room "$ROOM" >/tmp/webrec-pub.log 2>&1 &
echo $! > /tmp/webrec-pub.pid

echo "== 启动 Playwright（初始收流 → 观察重连）"
cd "$E2E_DIR"
BROWSER="${BROWSER:-msedge}" node web-reconnect-run.js "$ROOM" > /tmp/webrec-node.log 2>&1 &
NODE_PID=$!
cd "$ROOT"

# 等初始收流
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
kill "$(cat /tmp/webrec-pub.pid)" 2>/dev/null || true
./target/debug/aerodesk-cli --role publisher --encoder x264 --noisy \
    --signal ws://127.0.0.1:14503 --room "$ROOM" >/tmp/webrec-pub2.log 2>&1 &
echo $! > /tmp/webrec-pub.pid

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

kill $NODE_PID 2>/dev/null || true
stop_services
echo "E2E DONE"
