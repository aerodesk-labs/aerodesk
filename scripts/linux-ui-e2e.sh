#!/usr/bin/env bash
# Linux 主控端（aerodesk-ui）运行态端到端：Xvfb 无头跑 Slint UI → 本地 SFU+x264 发布端
# → UI 自动连接观看 → 断言 generic_media 解码帧日志增长（OpenH264 软解）。
# 依赖：xvfb、cargo 构建、libfontconfig/libxkbcommon 等 UI 系统库（CI ubuntu System deps 已装）。
# 用法: scripts/linux-ui-e2e.sh [room]
set -euo pipefail
ROOT="$(cd "$(dirname "$0")/.." && pwd)"
cd "$ROOT"
ROOM="${1:-linuxui-$(date +%s)}"

echo "== [1/6] 构建（debug，含 UI）"
cargo build -q -p aerodesk-sfu -p aerodesk-signal -p aerodesk-cli -p aerodesk-ui

echo "== [2/6] 启动 Xvfb :99"
Xvfb :99 -screen 0 1280x800x24 >/tmp/linuxui-xvfb.log 2>&1 &
XVFB=$!
sleep 1

echo "== [3/6] 启动 SFU/signal/publisher"
REC="$(mktemp -d)"
RECORD_DIR="$REC" "$ROOT/target/debug/aerodesk-sfu" >/tmp/linuxui-sfu.log 2>&1 &
SFU=$!
"$ROOT/target/debug/aerodesk-signal" >/tmp/linuxui-sig.log 2>&1 &
SIG=$!
for _ in $(seq 1 50); do
    if nc -z 127.0.0.1 3003 2>/dev/null && nc -z 127.0.0.1 3002 2>/dev/null; then break; fi
    sleep 0.2
done
"$ROOT/target/debug/aerodesk-cli" --role publisher --signal ws://127.0.0.1:3003 --room "$ROOM" --encoder x264 >/tmp/linuxui-pub.log 2>&1 &
PUB=$!
sleep 2

echo "== [4/6] 启动 UI（Xvfb，自动连接观看）"
DISPLAY=:99 "$ROOT/target/debug/aerodesk-ui" \
  -server 127.0.0.1:3003 -room "$ROOM" -autoconnect >/tmp/linuxui-ui.log 2>&1 &
UI_PID=$!
sleep 3

echo "== [5/6] 断言解码帧"
python3 - <<'PY'
import time, re, sys
ok = False
for i in range(60):  # 最多 60s
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
    print("FAIL: 60s 内未解码 10 帧；UI 日志尾：")
    print(open('/tmp/linuxui-ui.log', errors='replace').read()[-1200:])
    print("--- pub ---")
    print(open('/tmp/linuxui-pub.log', errors='replace').read()[-400:])
    sys.exit(1)
PY

echo "== [6/6] 截图留证（xwd → png）"
if command -v xwd >/dev/null 2>&1 && command -v convert >/dev/null 2>&1; then
  DISPLAY=:99 xwd -root -silent | convert xwd:- /tmp/linuxui-e2e.png || true
fi

kill "$UI_PID" "$PUB" "$SFU" "$SIG" "$XVFB" 2>/dev/null || true
echo "E2E DONE"
