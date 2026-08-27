#!/usr/bin/env bash
# #58 画质选层端到端（#598 P2a 起暂停）：Web 发布端（JSON WSS join 入 SFU 会议）
# + 2×CLI viewer（--layer f/q 会议桥选层登记）多播收流 + 选层请求转发断言。
#
# 暂停原因（#598 P2a）：JSON WSS 房间面退役——浏览器发布端入 SFU 会议的路径
# 依赖旧 join 语义；SIP 世界浏览器为 P2P-UAS（1:1 直连，无 SFU 会议参与）。
# 恢复条件：P3 会议桥落地后「浏览器/原生发布端入会」路径成形，本脚本按
# SIP 形态重写（发布端 INVITE 会议 AoR → SFU 会话 → 选层断言），恢复 CI 前
# 先本地验证。与 bridge 三脚本同暂停机制（ci.yml if: false）。
# 用法: scripts/simulcast-e2e.sh [房间] [观察秒数]
set -euo pipefail
cd "$(dirname "$0")/.."
ROOT="$(pwd)"

ROOM="${1:-sim-$(date +%s)}"
OBS="${2:-12}"
export RUST_LOG="${RUST_LOG:-info}"

echo "== 构建（debug）"
cargo build -q -p aerodesk-sfu -p aerodesk-signal -p aerodesk-agent

# Web 发布端（headless Chrome 屏幕共享，fake device；同 web-pub-e2e.sh）。
E2E_DIR="${WEB_E2E_DIR:-/tmp/simulcast-e2e}"
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
      '--use-fake-ui-for-media-stream',
      '--use-fake-device-for-media-stream',
      '--auto-accept-this-tab-capture',
      '--enable-usermedia-screen-capturing',
    ],
  });
  const page = await browser.newPage();
  await page.goto(`http://127.0.0.1:3002/?room=${ROOM}&role=publisher&signal=ws://127.0.0.1:3003/ws`);
  await page.click('#connect');
  await page.waitForFunction(() => document.getElementById('log').innerText.includes('屏幕共享已授权'), { timeout: 20000 });
  console.log('PASS screen shared');
  await page.waitForFunction(() => document.getElementById('status').innerText.includes('已连接'), { timeout: 25000 });
  console.log('PASS publisher connected');
  await new Promise(r => setTimeout(r, 15000)); // 保持发布（供 viewer 收流）
  await browser.close();
  console.log('E2E DONE');
})().catch(e => { console.error('E2E FAIL:', e.message); process.exit(1); });
JS
cd "$ROOT"

echo "== 启动 sfu/signal"
REC="$(mktemp -d)"
pkill -f "aerodesk-sfu|aerodesk-signal" 2>/dev/null || true
sleep 1
RECORD_DIR="$REC" ./target/debug/aerodesk-sfu >/tmp/sim-sfu.log 2>&1 &
SFU_PID=$!
SIP_UDP_PORT=5060 ./target/debug/aerodesk-signal >/tmp/sim-sig.log 2>&1 &
SIG_PID=$!
for _ in $(seq 1 50); do
    if nc -z 127.0.0.1 3003 2>/dev/null && nc -z 127.0.0.1 3002 2>/dev/null; then break; fi
    if ! kill -0 "$SIG_PID" 2>/dev/null; then
        echo "signal 服务器启动失败"; cat /tmp/sim-sig.log; exit 1
    fi
    sleep 0.2
done
sleep 0.3

# 先起两个 viewer 并等它们登记选层（High/Low），再启动发布端：
# viewer 的会议桥腿先建（SFU 房间按需创建，control 通道就绪即选层登记），
# 发布端后加入同房间 → SFU 即开始转发（无"迟到 viewer 等关键帧"问题）。
echo "== 启动 viewer f/q（SIP 会议桥，先登记选层）"
# 清空系统剪贴板：同 job 前置 step（clipboard sync 图片测试）残留的测试图会被
# viewer 剪贴板轮询捡到 → 上传确认 → send_complete 误判 → 媒体腿建立前退出
# （#595 回归：6 连败 RECEIVED 0 帧；产品侧 send_complete 已排除剪贴板，双保险）。
printf '' | pbcopy || true
./target/debug/aerodesk-agent --role viewer --layer f \
    --signal ws://127.0.0.1:3003 --room "$ROOM" >/tmp/sim-view-f.log 2>&1 &
F_PID=$!
./target/debug/aerodesk-agent --role viewer --layer q \
    --signal ws://127.0.0.1:3003 --room "$ROOM" >/tmp/sim-view-q.log 2>&1 &
Q_PID=$!
ready=0
for _ in $(seq 1 50); do
    if grep -q "layer request sent" /tmp/sim-view-f.log 2>/dev/null \
        && grep -q "layer request sent" /tmp/sim-view-q.log 2>/dev/null; then
        ready=1; break
    fi
    sleep 0.2
done
if [ "$ready" != "1" ]; then
    echo "FAIL viewer 未能在 10s 内登记选层"
    echo "--- f log:"; tail -5 /tmp/sim-view-f.log 2>/dev/null
    echo "--- q log:"; tail -5 /tmp/sim-view-q.log 2>/dev/null
    echo "--- sig log:"; tail -5 /tmp/sim-sig.log 2>/dev/null
    kill "$F_PID" "$Q_PID" "$SFU_PID" "$SIG_PID" 2>/dev/null || true
    exit 1
fi

echo "== 启动 Web 发布端（headless Chrome 屏幕共享）"
node "$E2E_DIR/e2e-pub.js" "$ROOM" &
PUB_PID=$!
echo "== 观察 f/q 层 ${OBS}s"
sleep "$OBS"

kill "$F_PID" "$Q_PID" "$PUB_PID" "$SFU_PID" "$SIG_PID" 2>/dev/null || true
wait 2>/dev/null || true

echo "== 断言"
fail=0
# 1) 两个 viewer 都收到发布端媒体（SFU 多播转发成立）
for v in f q; do
    if grep -qE "RECEIVED: [1-9][0-9]* frames" /tmp/sim-view-$v.log; then
        echo "PASS viewer $v received media"
    else
        echo "FAIL viewer $v received 0 frames"; tail -8 /tmp/sim-view-$v.log; fail=1
    fi
done
# 2) SFU 收到两个显式选层请求（--layer f/q 经 control 通道）
for layer in High Low; do
    if grep -q "layer request: Some($layer)" /tmp/sim-sfu.log; then
        echo "PASS SFU layer request $layer"
    else
        echo "FAIL SFU layer request $layer missing"; fail=1
    fi
done
# 3) 无 panic
if grep -qiE "panic" /tmp/sim-view-f.log /tmp/sim-view-q.log /tmp/sim-sfu.log /tmp/sim-sig.log; then
    echo "FAIL panic in logs"; fail=1
else
    echo "PASS no panics"
fi

exit $fail
