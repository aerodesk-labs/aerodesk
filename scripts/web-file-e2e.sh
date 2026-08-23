#!/usr/bin/env bash
# Web 主控端文件上传端到端：Playwright 打开 SFU 内嵌页面（viewer）→ 选择本地文件
# → file data channel（Meta/8KB 分片/Done）→ CLI publisher（被控端）落盘 → sha256 一致。
# 依赖：cargo build、Playwright、Chrome（BROWSER env，CI 用 chrome）。
# 用法: scripts/web-file-e2e.sh [room]
set -euo pipefail
ROOT="$(cd "$(dirname "$0")/.." && pwd)"
cd "$ROOT"
ROOM="${1:-webfile-$(date +%s)}"
E2E_DIR="${WEB_E2E_DIR:-/tmp/web-file-e2e}"
mkdir -p "$E2E_DIR"
cd "$E2E_DIR"
if [ ! -d node_modules/playwright-core ]; then npm init -y >/dev/null 2>&1; npm i playwright-core >/dev/null 2>&1; fi

# 测试文件（1MB 随机）
dd if=/dev/urandom of="$E2E_DIR/upload.bin" bs=1M count=1 2>/dev/null
EXPECTED=$(shasum -a 256 "$E2E_DIR/upload.bin" | awk '{print $1}')

cat > e2e-file.js <<'JS'
const { chromium } = require('playwright-core');
const ROOM = process.argv[2];
const FILE = process.argv[3];
(async () => {
  const browser = await chromium.launch({ channel: process.env.BROWSER || 'msedge', headless: true });
  const page = await browser.newPage();
  await page.goto(`http://127.0.0.1:3002/?room=${ROOM}&role=viewer&signal=ws://127.0.0.1:3003/ws`);
  await page.click('#connect');
  await page.waitForFunction(() => document.getElementById('file').disabled === false, { timeout: 25000 });
  console.log('PASS connected, file input enabled');
  await page.setInputFiles('#file', FILE);
  await page.click('#sendfile');
  await page.waitForFunction(() => document.getElementById('fileinfo').innerText.includes('已发送完成'), { timeout: 30000 });
  console.log('PASS file send done');
  // 等接收端确认
  await page.waitForFunction(() => document.getElementById('log').innerText.includes('file confirmed'), { timeout: 15000 });
  console.log('PASS file confirmed by receiver');
  await browser.close();
  console.log('E2E DONE');
})().catch(e => { console.error('E2E FAIL:', e.message); process.exit(1); });
JS

cd "$ROOT"
echo "== 构建"
cargo build -q -p aerodesk-sfu -p aerodesk-signal -p aerodesk-agent
echo "== 启动服务"
# 前置 e2e 可能残留 SFU/signal 占用 3002/3003 → 先清理，并等端口释放。
pkill -f "aerodesk-sfu|aerodesk-signal" 2>/dev/null || true
for _ in $(seq 1 50); do
    if ! nc -z 127.0.0.1 3002 2>/dev/null && ! nc -z 127.0.0.1 3003 2>/dev/null; then break; fi
    sleep 0.2
done
sleep 0.5
REC="$(mktemp -d)"
RECORD_DIR="$REC" "$ROOT/target/debug/aerodesk-sfu" >/tmp/webfile-sfu.log 2>&1 &
SFU=$!
SIP_UDP_PORT=5060 "$ROOT/target/debug/aerodesk-signal" >/tmp/webfile-sig.log 2>&1 &
SIG=$!
for _ in $(seq 1 50); do
    if nc -z 127.0.0.1 3003 2>/dev/null && nc -z 127.0.0.1 3002 2>/dev/null; then break; fi
    sleep 0.2
done
RECV="$E2E_DIR/recv"
rm -rf "$RECV"
mkdir -p "$RECV"
# CLI publisher 作为被控端：--recv-dir 接收文件落盘
"$ROOT/target/debug/aerodesk-agent" --role publisher --signal ws://127.0.0.1:3003 --room "$ROOM" --encoder pcap --recv-dir "$RECV" >/tmp/webfile-pub.log 2>&1 &
PUB=$!
sleep 2

set +e
node "$E2E_DIR/e2e-file.js" "$ROOM" "$E2E_DIR/upload.bin"
RES=$?
set -e
sleep 3

kill "$PUB" "$SFU" "$SIG" 2>/dev/null || true
# 断言落盘 + sha256
if [ -f "$RECV/upload.bin" ]; then
  GOT=$(shasum -a 256 "$RECV/upload.bin" | awk '{print $1}')
  if [ "$GOT" = "$EXPECTED" ]; then
    echo "PASS receiver file sha256 match"
  else
    echo "FAIL sha256 mismatch: got $GOT expected $EXPECTED"; RES=1
  fi
else
  echo "FAIL: receiver file missing; pub log:"; tail -12 /tmp/webfile-pub.log; RES=1
fi
exit $RES
