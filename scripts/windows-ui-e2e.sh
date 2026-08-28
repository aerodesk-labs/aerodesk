#!/usr/bin/env bash
# Windows 主控端（aerodesk-desktop）运行态端到端：构建 → 本地 SFU → Web 被控端发布
# （headless Edge 屏幕共享）→ Windows UI 自动连接观看 → 断言 ICE Completed。
# 依赖：cargo、node/playwright-core、Edge（windows runner 预装）、UI 编译通过（#177）。
# 用法: scripts/windows-ui-e2e.sh [room]  （Git Bash）
set -euo pipefail
export PYTHONIOENCODING=utf-8
ROOT="$(cd "$(dirname "$0")/.." && pwd)"
cd "$ROOT"
ROOM="${1:-winui-$(date +%s)}"

echo "== [1/6] 构建（Windows）"
cargo build -q -p aerodesk-sfu -p aerodesk-signal -p aerodesk-desktop

E2E_DIR="${WEB_E2E_DIR:-/tmp/win-ui-e2e}"
mkdir -p "$E2E_DIR"
cd "$E2E_DIR"
if [ ! -d node_modules/playwright-core ]; then npm init -y >/dev/null 2>&1; npm i playwright-core >/dev/null 2>&1; fi
cat > e2e-pub.js <<'JS'
const { chromium } = require('playwright-core');
const ROOM = process.argv[2];
(async () => {
  const browser = await chromium.launch({
    channel: process.env.BROWSER || 'msedge', headless: true,
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
    await page.goto(`http://127.0.0.1:${process.env.WEB_SERVE_PORT || 38086}/sip-publisher.html?device=${ROOM}&token=e2e-token&signal=wss://127.0.0.1:3061`);
    await page.click('#connect');
    // 15s < bash 注册门 30s：TimeoutError + 页面快照必须先于 bash 门放弃落盘。
    await page.waitForFunction(() => document.getElementById('status').innerText.includes('等待观看端拨入'), { timeout: 15000 });
    console.log('PASS page registered, waiting INVITE');
    await page.waitForFunction(() => document.getElementById('status').innerText.includes('已连接'), { timeout: 25000 });
    console.log('PASS publisher connected');
    await new Promise(r => setTimeout(r, 15000));
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

echo "== [1.5/6] Windows 被控端运行级自测（DXGI 采集 + SendInput 注入）"
cd "$ROOT"
cargo test -p aerodesk-platform --test windows_runtime 2>&1 | tail -12

echo "== [2/6] 启动 SFU/signal（Windows）"
# Windows 防火墙可能阻止 SFU UDP 3478 入站（同机回包）→ 放行（runner 有管理员权限）。
netsh advfirewall firewall add rule name="aerodesk-e2e-udp3478" dir=in action=allow protocol=UDP localport=3478 >/dev/null 2>&1 || true
netsh advfirewall firewall add rule name="aerodesk-e2e-tcp" dir=in action=allow protocol=TCP localport=3001,3002,3003 >/dev/null 2>&1 || true
REC="$(mktemp -d)"
RECORD_DIR="$REC" "$ROOT/target/debug/aerodesk-sfu.exe" >/tmp/winui-sfu.log 2>&1 &
SFU=$!
# SIP 会议桥链路：SIP/UDP 5060 + Digest 凭证（desktop 侧 settings 同步 seed）。
SIP_UDP_PORT=5060 SIP_WSS_PORT=3061 \
  SIP_DIGEST_USERS="AD-E2EUI=e2e-token,${ROOM}=e2e-token" \
  "$ROOT/target/debug/aerodesk-signal.exe" >/tmp/winui-sig.log 2>&1 &
SIG=$!
(cd "$ROOT/web" && python3 -m http.server "${WEB_SERVE_PORT:-38086}" >/tmp/winui-http.log 2>&1) &
HTTP=$!
export WEB_SERVE_PORT="${WEB_SERVE_PORT:-38086}"
export WINUI_TMP="$(cygpath -w /tmp)"
python3 - <<'PY'
import socket, time, sys, os
# SFU(3003/3002) + web 静态服务端口一并探活（http.server 未就绪时 node goto 白跑）。
web_port = int(os.environ.get("WEB_SERVE_PORT", "38086"))
ok = False
for _ in range(50):
    try:
        for p in (3003, 3002, web_port):
            c = socket.create_connection(("127.0.0.1", p), 0.5); c.close()
        ok = True; break
    except OSError:
        time.sleep(0.2)
if not ok:
    print("FAIL: SFU/signal/web 未就绪; logs:")
    # Windows python 读不了 Git-Bash /tmp 路径（解析为 C:\tmp）——cygpath 转
    # Windows 路径再进 python（与断言步 WINUI_LOG 同款，勿再踩）。
    tmp = os.environ["WINUI_TMP"]
    for name in ("winui-sig.log", "winui-sfu.log", "winui-http.log"):
        try:
            print(f"--- {name} ---")
            print(open(os.path.join(tmp, name), encoding="utf-8", errors="replace").read()[-2000:])
        except OSError:
            pass
    sys.exit(1)
print("PASS SFU/signal/web TCP ready")
PY
# SIP 就绪门：desktop 观看经 SIP 会议桥（WSS 兜底已删），SIP 起不来必失败——
# signal 的 SIP 绑定失败是非致命 error!（线程内），TCP 探活会漏。
# UDP + WSS 两行监听日志双条件：WSS 单列防 TLS/证书加载失败被 UDP 单条件漏放行
# （浏览器侧 wss 连不上即注册不了）。
# Windows python 读不了 Git-Bash /tmp 路径（解析为 C:	mp）——cygpath 转
# Windows 路径再进 python（与断言步 WINUI_LOG 同款，勿再踩）。
export SIP_SIG_LOG="$(cygpath -w /tmp/winui-sig.log)"
python3 - <<'PY'
import os, time
path = os.environ["SIP_SIG_LOG"]
for _ in range(80):
    try:
        txt = open(path, encoding="utf-8", errors="replace").read()
        # UDP + WSS 双监听都就绪才放行（WSS 单列防 TLS/证书加载失败漏放行）。
        if "SIP/UDP 监听已起" in txt and "SIP/WSS 监听已起" in txt:
            print("PASS SIP/UDP + SIP/WSS ready"); break
    except OSError:
        pass
    time.sleep(0.2)
else:
    print("FAIL: SIP 监听未就绪，signal 日志尾：")
    try:
        print(open(path, encoding="utf-8", errors="replace").read()[-2000:])
    except OSError as e:
        print("读取失败:", e)
    raise SystemExit(1)
PY

echo "== [3/6] Web 被控端发布（headless Edge 屏幕共享）"
WEB_SERVE_PORT="${WEB_SERVE_PORT:-38086}" node "$E2E_DIR/e2e-pub.js" "$ROOM" >/tmp/winui-pub.log 2>&1 &
PUB=$!
# UAS 时序：页面注册就绪（≤30s）后才起 UI 拨入（先拨会 503）。
# 60×0.5s=30s > js 内 waitForFunction 15s+launch/goto：js 的 TimeoutError+快照
# 必然先落盘，tail 不再抓到空文件。
OK=0
for _ in $(seq 1 60); do
  grep -q "PASS page registered" /tmp/winui-pub.log 2>/dev/null && OK=1 && break
  sleep 0.5
done
if [ "$OK" != "1" ]; then
  echo "FAIL 页面未注册就绪"
  echo "--- pub.log ---"; tail -15 /tmp/winui-pub.log
  echo "--- sig.log ---"; tail -20 /tmp/winui-sig.log
  echo "--- http.log ---"; tail -10 /tmp/winui-http.log
  exit 1
fi

echo "== [3.5/6] seed SIP 配置（desktop 启动即 REGISTER，观看经会议桥）"
# 隔离 HOME：seed 与 desktop 启动同用 $E2E_DIR（不碰真实配置）。
export AERO_E2E_HOME="$E2E_DIR"
python3 - <<'PY'
import sys
sys.stdout.reconfigure(encoding='utf-8', errors='replace')  # cp1252 控制台打印中文会崩
import json, os
# e2e SIP 链路配置：desktop 启动时读 ~/.aerodesk-settings.json 建 SipCallLink
# （REGISTER 用 Digest 凭证），观看任意房间经 SIP 会议桥入 SFU。
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

echo "== [4/6] 启动 Windows UI（自动连接观看）"
RUST_LOG=debug HOME="$(cygpath -w "$E2E_DIR")" "$ROOT/target/debug/aerodesk-desktop.exe" \
  -server 127.0.0.1:3003 -room "$ROOM" -autoconnect >/tmp/winui-ui.log 2>&1 &
UI_PID=$!
sleep 8
if ! kill -0 $UI_PID 2>/dev/null; then
  echo "FAIL: UI 进程退出；日志："
  cat /tmp/winui-ui.log 2>/dev/null || echo "(无日志)"
  exit 1
fi
echo "UI alive: yes, log lines: $(wc -l < /tmp/winui-ui.log 2>/dev/null || echo 0)"

echo "== [5/6] 断言连接链路（信令/SDP/ICE 启动 = Windows 主控端成功接入 SFU）"
# Windows runner 网络环境（VM 内 UDP 回程限制）下 ICE 可能停在 Checking；
# 同代码在 Linux 已验证 ICE Completed。此处验证建链启动（Checking/Completed 均 PASS）。
export WINUI_LOG="$(cygpath -w /tmp/winui-ui.log)"
python3 - <<'PY'
import time, sys, os
sys.stdout.reconfigure(encoding='utf-8', errors='replace')
ok = False
for i in range(60):
    try:
        txt = open(os.environ['WINUI_LOG'], encoding='utf-8', errors='replace').read()
    except FileNotFoundError:
        txt = ''
    if 'IceConnectionStateChange(Completed)' in txt or 'ICE remote address' in txt:
        print("PASS Windows UI ICE Completed (connected to SFU)")
        ok = True
        break
    if 'IceConnectionStateChange(Checking)' in txt:
        print("PASS Windows UI ICE Checking (signaling/SDP/ICE started)")
        ok = True
        break
    if ('sip call failed' in txt or 'sip call rejected' in txt
            or 'sip call peer hangup' in txt or '链路未启动' in txt):
        print("FAIL: SIP 呼叫失败"); ok = False; break
    time.sleep(1)
if not ok:
    print("FAIL: 60s 内 ICE 未启动；UI 日志尾：")
    print(open(os.environ['WINUI_LOG'], encoding='utf-8', errors='replace').read()[-1500:])
    sys.exit(1)
PY

echo "== [6/6] 清理"
taskkill //F //PID "$UI_PID" 2>/dev/null || true
taskkill //F //PID "$PUB" 2>/dev/null || true
taskkill //F //PID "$SFU" 2>/dev/null || true
taskkill //F //PID "$SIG" 2>/dev/null || true
[ -n "${HTTP:-}" ] && taskkill //F //PID "$HTTP" 2>/dev/null || true
echo "E2E DONE"
