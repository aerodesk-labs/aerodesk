#!/usr/bin/env bash
# #58 画质选层端到端（SIP 语义版，#552 迁移后重写 2026-08-24）：
#   Web 端（headless Chrome 屏幕共享）发布 → SFU → 2 个 CLI viewer（SIP 会议桥）选层收流。
#
# 重写背景：#552 把 agent CLI 信令改为 SIP——publisher 是 1:1 P2P 被叫（无 SFU
# 会议发布/302 升级，属 P2 项），WSS 客户端面已删（#580）。原生端 simulcast 三层
# 端到端（rid q/h/f + f>q 码率切换）需原生端会议发布落地后恢复（P2，#553 交接项）。
# 本脚本维持 #58 的核心护栏：1 发布 → 2 观看的多播 + SFU 选层请求转发 + 无 panic。
# 发布端用 Web（走 signal 保留的 WSS /ws JSON 面），观看端用 CLI（SIP 会议桥进
# 同一房间——服务端按房间 FNV 哈希选同一 SFU shard，与 WSS 侧同池）。
#
# 断言：
#   1. 两个 viewer 都收到发布端媒体（RECEIVED > 0）——SFU 多播转发成立
#   2. SFU 收到 High/Low 两个显式选层请求（--layer f/q 经 control 通道）
#   3. 无 panic
# 用法: scripts/simulcast-e2e.sh [房间] [观察秒数]
# 依赖：cargo、node/playwright-core、Chrome（macOS runner 预装）。
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
