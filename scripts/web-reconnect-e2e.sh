#!/usr/bin/env bash
# web-reconnect-e2e.sh —— Web 端自动重连（#175，SIP 版 #598 P2a）：
#   Playwright 双浏览器（sip-publisher.html 被控页 + sip-viewer.html 观看页，
#   SIP-WSS 1:1）→ 初始收流 + 键鼠输入基线 → 杀 SFU+signal → 停机 >1s → 重启
#   → 两页自动退避重连（WS close → 1s/2s/4s/8s 退避 → 重新 REGISTER；
#   viewer 重建会话再收流）。
# 【判据（本批加强）】不再只看 video.readyState 未跌——必须同时满足：
#   DROP_SEEN（观察到信令断开）→ RESIGNALED + REGISTERED_AGAIN（重连并重新注册）
#   → VIDEO_TRACK_AGAIN + VIDEO_FRAMES_DELTA>0（新会话**再次解码出帧**）
#   → INPUT_BACK（观看页键鼠帧**再次抵达被控页** log）。
#   浏览器侧汇总为 RECONNECT_OK=<true|false>；**bash 侧只认整行 RECONNECT_OK=true**
#   （旧写法 `grep -q RECONNECT_OK` 把 RECONNECT_OK=false 也当命中 ⇒ 放弃重连照样
#   PASS，是为假通过，已一并修掉；真值表见卡片回报）。
# 媒体为 1:1 直连（不经 SFU），服务重启只断信令面——#175 的信令韧性断言语义保留。
# 依赖：cargo build、playwright-core、Edge/Chrome（BROWSER 可选，默认 msedge）、
# python3（静态服务 web/）。
# 本机：Git Bash 起子进程会挂（规则库 LESSON_本机GitBash起子进程会挂起bash脚本本地无法执行
# 需pwsh等价验证），本脚本在本机不可直接跑；本批用 pwsh/Playwright 窄 harness（自有端口、
# 只杀自有 PID）抽取本文件的**同一份 runner JS** 验证。端口可用 WEB_SERVE_PORT / SIGNAL_URL 覆盖。
set -euo pipefail
ROOT="$(cd "$(dirname "$0")/.." && pwd)"
source "$ROOT/scripts/lib/e2e-ports.sh"   # e2e 端口统一（SIP_PORT / SIGNAL_OPS_PORT 可覆盖）
cd "$ROOT"
export RUST_LOG="${RUST_LOG:-info}"

ROOM="webrec-$(date +%s)"
REC="$(mktemp -d)"
WEB_SERVE_PORT="${WEB_SERVE_PORT:-38087}"
# 浏览器信令 URL（默认与 web/ 输入框默认一致；窄 harness 用自有端口时覆盖）。
SIGNAL_URL="${SIGNAL_URL:-wss://127.0.0.1:3061}"

E2E_DIR="${WEB_E2E_DIR:-/tmp/web-e2e}"
mkdir -p "$E2E_DIR"; cd "$E2E_DIR"
if [ ! -d node_modules/playwright-core ]; then npm init -y >/dev/null 2>&1; npm i playwright-core >/dev/null 2>&1; fi

cat > web-reconnect-run.js <<'JS'
// web-reconnect-run.js —— web-reconnect-e2e.sh 的浏览器侧驱动（由该脚本 heredoc 生成）。
// 场景：发布页（被控）+ 观看页 → 初始收流/输入基线 → 外部杀信令 → 重启 → 两页自动退避
//       重连 → 观看页**重新收流**且**输入仍回传**（旧判据只等 readyState>=2 未跌，会漏）。
// 输出约定（bash 侧捕获）：PUBLISHER_OK / INITIAL_VIDEO_OK / INITIAL_INPUT_OK / INITIAL_OK /
//   DROP_SEEN / RESIGNALED / REGISTERED_AGAIN / VIDEO_TRACK_AGAIN / VIDEO_FRAMES_DELTA /
//   VIDEO_BACK / INPUT_BACK / FINAL_STATUS / RECONNECT_OK=<true|false> / E2E_DONE / E2E_FAIL:
// 环境变量：WEB_SERVE_PORT / SIGNAL_URL / BROWSER / ROOM / RECONNECT_WAIT_MS。
const { chromium } = require('playwright-core');
const ROOM = process.argv[2];
const SP = process.env.WEB_SERVE_PORT || 38087;
const SIGNAL = process.env.SIGNAL_URL || 'wss://127.0.0.1:3061';
const WAIT_MS = parseInt(process.env.RECONNECT_WAIT_MS || '150000', 10);

(async () => {
  const browser = await chromium.launch({
    channel: process.env.BROWSER || 'msedge', headless: true,
    args: [
      '--no-sandbox',
      '--use-fake-ui-for-media-stream',
      '--use-fake-device-for-media-stream',
      '--auto-accept-this-tab-capture',
      '--enable-usermedia-screen-capturing',
      '--ignore-certificate-errors',
    ],
  });
  const sig = encodeURIComponent(SIGNAL);
  const pub = await browser.newPage();
  pub.on('pageerror', e => console.log('PUB_PAGEERROR: ' + e.message));
  await pub.goto('http://127.0.0.1:' + SP + '/sip-publisher.html?device=' + ROOM + '&signal=' + sig);
  await pub.click('#connect');
  await pub.waitForFunction(() => document.getElementById('status').innerText.indexOf('等待观看端拨入') >= 0, { timeout: 40000 });
  console.log('PUBLISHER_OK');

  const view = await browser.newPage();
  view.on('pageerror', e => console.log('VIEW_PAGEERROR: ' + e.message));
  await view.goto('http://127.0.0.1:' + SP + '/sip-viewer.html?target=' + ROOM + '&signal=' + sig);
  await view.click('#connect');

  const logOf = (p) => p.evaluate(() => document.getElementById('log').innerText).catch(() => '');
  const statusOf = (p) => p.evaluate(() => document.getElementById('status').innerText).catch(() => '?');
  const countIn = (t, n) => t.split(n).length - 1;

  // 观看页当前 RTCPeerConnection 的 inbound-rtp 计数：framesDecoded 是「解码出帧」的直接
  // 证据；readyState>=2 只是 HAVE_CURRENT_DATA，一帧冻住的画面同样满足。
  const mediaStats = () => view.evaluate(async () => {
    let pc = null;
    try { pc = (typeof rtc === 'undefined') ? null : rtc; } catch (_) { pc = null; }
    if (!pc) return { error: 'rtc 不可达' };
    let frames = 0, packets = 0;
    try {
      const stats = await pc.getStats();
      stats.forEach(r => {
        if (r.type === 'inbound-rtp' && r.kind === 'video') {
          frames = Math.max(frames, r.framesDecoded || 0);
          packets = Math.max(packets, r.packetsReceived || 0);
        }
      });
    } catch (e) { return { error: 'getStats: ' + e.message }; }
    const v = document.getElementById('video');
    return { frames: frames, packets: packets, readyState: v ? v.readyState : 0,
             currentTime: v ? v.currentTime : 0, hasSrc: !!(v && v.srcObject) };
  }).catch(() => null);

  // 观看页 → 发布页的输入回传落地计数（发布页 ondatachannel 打 'input: {...}'）。
  const pubInputCount = () => pub.evaluate(() =>
    (document.getElementById('log').innerText.match(/input: /g) || []).length).catch(() => -1);

  const sendMoves = async (n) => {
    for (let i = 0; i < n; i++) {
      await view.evaluate((k) => {
        const v = document.getElementById('video');
        if (!v) return;
        const r = v.getBoundingClientRect();
        if (r.width === 0 || r.height === 0) return;
        v.dispatchEvent(new MouseEvent('mousemove', {
          clientX: r.left + r.width * (0.35 + 0.03 * (k % 5)),
          clientY: r.top + r.height * (0.35 + 0.03 * (k % 3)),
          bubbles: true,
        }));
      }, i).catch(() => {});
      await new Promise(r => setTimeout(r, 120));
    }
  };

  // ---- 阶段 1：初始收流（解码帧 > 0）+ 输入基线 ----
  let initialVideo = false;
  const tInit = Date.now();
  while (Date.now() - tInit < 45000) {
    const st = await mediaStats();
    if (st && !st.error && st.readyState >= 2 && st.frames > 0) { initialVideo = true; break; }
    const s = await statusOf(view);
    if (s.indexOf('连接失败') >= 0 || s.indexOf('启动失败') >= 0) break;
    await new Promise(r => setTimeout(r, 500));
  }
  console.log('INITIAL_VIDEO_OK=' + initialVideo);
  if (!initialVideo) {
    console.log('---VIEW_STATUS--- ' + (await statusOf(view)));
    console.log('---VIEW_LOG--- ' + (await logOf(view)).slice(-1500));
    console.log('RECONNECT_OK=false');
    await browser.close();
    console.log('E2E_DONE');
    return;
  }
  const inputBase = await pubInputCount();
  await sendMoves(10);
  let initialInput = false;
  for (let i = 0; i < 20; i++) {
    if ((await pubInputCount()) > inputBase) { initialInput = true; break; }
    await new Promise(r => setTimeout(r, 250));
  }
  console.log('INITIAL_INPUT_OK=' + initialInput);
  // bash 侧以 INITIAL_OK 为「可以杀服务」的信号。
  console.log('INITIAL_OK');
  if (!initialInput) {
    console.log('RECONNECT_OK=false');
    await browser.close();
    console.log('E2E_DONE');
    return;
  }

  // ---- 阶段 2：等服务被外部杀掉 → 观察到断开 → 等重连成功 → 再次收流 + 输入仍回传 ----
  let dropSeen = false, resignaled = false, registeredAgain = false, trackAgain = false;
  let videoBack = false, inputBack = false, framesDelta = 0, prevFrames = null;
  const t0 = Date.now();
  while (Date.now() - t0 < WAIT_MS) {
    const lg = await logOf(view);
    const connects = countIn(lg, 'signaling connected:');
    const registers = countIn(lg, 'REGISTER \u2192 200 OK');
    const tracks = countIn(lg, 'received track: video');
    if (!dropSeen && lg.indexOf('自动重连') >= 0) { dropSeen = true; console.log('DROP_SEEN=true'); }
    if (!resignaled && connects >= 2) { resignaled = true; console.log('RESIGNALED=true'); }
    if (!registeredAgain && registers >= 2) { registeredAgain = true; console.log('REGISTERED_AGAIN=true'); }
    if (!trackAgain && tracks >= 2) { trackAgain = true; console.log('VIDEO_TRACK_AGAIN=true'); }
    if (resignaled && registeredAgain && trackAgain && !videoBack) {
      const st = await mediaStats();
      if (st && !st.error && st.readyState >= 2 && typeof st.frames === 'number') {
        if (prevFrames !== null && st.frames - prevFrames > 0) {
          framesDelta = st.frames - prevFrames;
          videoBack = true;
          console.log('VIDEO_FRAMES_DELTA=' + framesDelta);
          console.log('VIDEO_BACK=true');
        }
        prevFrames = st.frames;
      }
    }
    if (videoBack && !inputBack) {
      const before = await pubInputCount();
      await sendMoves(10);
      for (let i = 0; i < 12; i++) {
        if ((await pubInputCount()) > before) { inputBack = true; break; }
        await new Promise(r => setTimeout(r, 250));
      }
      console.log('INPUT_BACK=' + inputBack);
    }
    if (videoBack && inputBack) break;
    await new Promise(r => setTimeout(r, 500));
  }

  console.log('FINAL_STATUS=' + (await statusOf(view)));
  const ok = dropSeen && resignaled && registeredAgain && trackAgain && videoBack && inputBack;
  console.log('RECONNECT_OK=' + ok);
  if (!ok) {
    console.log('---VIEW_LOG--- ' + (await logOf(view)).slice(-2500));
    console.log('---PUB_LOG--- ' + (await logOf(pub)).slice(-1500));
  }
  await browser.close();
  console.log('E2E_DONE');
})().catch(e => { console.error('E2E_FAIL: ' + e.message); process.exit(1); });
JS

cd "$ROOT"
echo "== 构建"
cargo build -q -p aerodesk-sfu -p aerodesk-signal -p aerodesk-agent

start_sfu() {
    RECORD_DIR="$REC" \
      SFU_MEDIA_PORT=14578 SFU_SIGNAL_PORT=14500 SFU_INTERNAL_PORT=14502 \
      ./target/debug/aerodesk-sfu >/tmp/webrec-sfu.log 2>&1 &
    echo $! > /tmp/webrec-sfu.pid
}
start_signal() {
    # #598 P2a：浏览器信令走 SIP-WSS（3061）；UDP 5060 供 CLI（本脚本不用）。
    SIGNAL_PORT=14501 SFU_URL=http://127.0.0.1:14502 \
      SIP_UDP_PORT="$SIP_PORT" SIP_WSS_PORT=3061 ./target/debug/aerodesk-signal >/tmp/webrec-sig.log 2>&1 &
    echo $! > /tmp/webrec-sig.pid
}
stop_services() {
    if [ -f /tmp/webrec-sfu.pid ]; then kill "$(cat /tmp/webrec-sfu.pid)" 2>/dev/null || true; rm -f /tmp/webrec-sfu.pid; fi
    if [ -f /tmp/webrec-sig.pid ]; then kill "$(cat /tmp/webrec-sig.pid)" 2>/dev/null || true; rm -f /tmp/webrec-sig.pid; fi
    for _ in $(seq 1 50); do
        (exec 3<>/dev/tcp/127.0.0.1/14502) 2>/dev/null || break
        sleep 0.2
    done
}
wait_ports() {
    for _ in $(seq 1 50); do
        if grep -q "SIP/UDP 监听已起" /tmp/webrec-sig.log 2>/dev/null \
            && (exec 3<>/dev/tcp/127.0.0.1/14502) 2>/dev/null; then return 0; fi
        sleep 0.2
    done
    return 1
}

echo "== 启动服务"
# 重试残留清理：ci-retry 3× 重跑时上一轮的 http.server 可能仍占 38087
# （失败路径不回收后台进程），先按端口特征清掉再起，防 bind 竞争/半死监听。
pkill -f "http.server $WEB_SERVE_PORT" 2>/dev/null || true
start_sfu; start_signal
(cd "$ROOT/web" && python3 -m http.server "$WEB_SERVE_PORT" --bind 127.0.0.1 >/tmp/webrec-http.log 2>&1) &
HTTP=$!
# 静态服务就绪门（此前 macOS 出现 goto ERR_CONNECTION_TIMED_OUT：服务未起
# 即跑 node，连接被丢 SYN）——TCP 探活 25s，超时 dump http.log。
HTTP_OK=0
for _ in $(seq 1 50); do
    if (exec 3<>/dev/tcp/127.0.0.1/$WEB_SERVE_PORT) 2>/dev/null; then HTTP_OK=1; break; fi
    if ! kill -0 "$HTTP" 2>/dev/null; then break; fi
    sleep 0.5
done
if [ "$HTTP_OK" != "1" ]; then
    echo "FAIL: web 静态服务未就绪（${WEB_SERVE_PORT}）"; tail -10 /tmp/webrec-http.log; exit 1
fi
wait_ports || { echo "FAIL: 服务未就绪"; exit 1; }

echo "== 启动 Playwright（被控页 + 观看页：初始收流 → 观察重连）"
cd "$E2E_DIR"
BROWSER="${BROWSER:-msedge}" WEB_SERVE_PORT="$WEB_SERVE_PORT" SIGNAL_URL="$SIGNAL_URL" \
    node web-reconnect-run.js "$ROOM" > /tmp/webrec-node.log 2>&1 &
NODE_PID=$!
cd "$ROOT"

# 等初始收流（两页均已连接）
INIT=0
for _ in $(seq 1 120); do
    grep -q '^INITIAL_OK$' /tmp/webrec-node.log 2>/dev/null && { INIT=1; break; }
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

echo "== 等页面自动重连（断开 → 重连成功 → 再次收流 + 输入仍回传）"
# 只认**整行** RECONNECT_OK=(true|false) 的最后一个值，要求 = true。
# 旧写法 `grep -q RECONNECT_OK` 是子串匹配，会把 RECONNECT_OK=false（放弃重连）也当
# 命中，于是「重连失败」照样打印 PASS——本卡一并修掉这个假通过。
# set -euo pipefail 下 grep 无匹配会退出 1，故用 { ... || true; } 包住。
RC_OK=""
for _ in $(seq 1 300); do
    RC_OK=$( { grep -E '^RECONNECT_OK=(true|false)$' /tmp/webrec-node.log 2>/dev/null || true; } | tail -n 1 | cut -d= -f2 )
    if [ -n "$RC_OK" ]; then break; fi
    if ! kill -0 $NODE_PID 2>/dev/null; then break; fi
    sleep 0.5
done
if grep -q 'E2E_FAIL' /tmp/webrec-node.log 2>/dev/null; then
    echo "FAIL: 页面重连流程抛异常"; tail -12 /tmp/webrec-node.log; exit 1
fi
if [ "$RC_OK" = "true" ]; then
    echo "PASS 页面自动重连：再次解码出帧 + 键鼠输入再次抵达被控页（RECONNECT_OK=true）"
else
    echo "FAIL: 未重连恢复（RECONNECT_OK=${RC_OK:-<缺失>}）"; tail -12 /tmp/webrec-node.log; exit 1
fi

kill $NODE_PID $HTTP 2>/dev/null || true
stop_services
echo "E2E DONE"
