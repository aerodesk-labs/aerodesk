#!/usr/bin/env bash
# Linux 原生被控端端到端（#4/#306）：Xvfb 上跑 aerodesk-agent --encoder screen
# （X11 采集 → VAAPI/x264 编码 → SFU）→ CLI viewer 收帧断言。
# 输入注入（XTest/uinput）由 x11_runtime/uinput_runtime 测试覆盖。
# 依赖：xvfb、x11-apps（与 linux-ui-e2e 同款 CI 系统依赖）。
# 用法: scripts/linux-native-e2e.sh [room]
set -euo pipefail
ROOT="$(cd "$(dirname "$0")/.." && pwd)"
cd "$ROOT"
ROOM="${1:-linux-native-$(date +%s)}"

echo "== [1/5] 构建 CLI + SFU + signal"
cargo build -q -p aerodesk-sfu -p aerodesk-signal -p aerodesk-agent

echo "== [2/5] 启动 Xvfb :98"
Xvfb :98 -screen 0 1024x768x24 >/tmp/linux-native-xvfb.log 2>&1 &
XVFB=$!
sleep 1

echo "== [3/5] 启动 SFU/signal"
REC="$(mktemp -d)"
# #535 排查：SFU debug 级日志（通道开/轨道增删/键帧请求转发路径）。
RECORD_DIR="$REC" "$ROOT/target/debug/aerodesk-sfu" >/tmp/linux-native-sfu.log 2>&1 &
SFU=$!
"$ROOT/target/debug/aerodesk-signal" >/tmp/linux-native-sig.log 2>&1 &
SIG=$!
OK=0
for _ in $(seq 1 50); do
    if nc -z 127.0.0.1 3003 2>/dev/null && nc -z 127.0.0.1 3002 2>/dev/null; then OK=1; break; fi
    sleep 0.2
done
if [ "$OK" != "1" ]; then echo "FAIL: SFU/signal 未就绪"; tail -10 /tmp/linux-native-sfu.log; exit 1; fi

echo "== [4/5] 原生 Linux 被控端发布（X11 采集 → 编码 → SFU）"
# #535 排查：publisher 亦开 debug——键帧请求（FIR）接收与编码器响应路径可见。
DISPLAY=:98 RUST_LOG=info "$ROOT/target/debug/aerodesk-agent" \
  --role publisher --encoder screen --signal ws://127.0.0.1:3003 --room "$ROOM" \
  >/tmp/linux-native-pub.log 2>&1 &
PUB=$!
sleep 3

echo "== [5/5] CLI viewer 收流断言"
# #535 排查：viewer debug 级日志（ICE/DTLS/SCTP/DCEP 通道开、FIR 发送、组装器）。
"$ROOT/target/debug/aerodesk-agent" --role viewer --signal ws://127.0.0.1:3003 --room "$ROOM" \
  >/tmp/linux-native-view.log 2>&1 &
VIEW=$!
python3 - <<'PY' || RC=$?
import time, sys
ok = False
for i in range(60):  # 最多 60s
    try:
        txt = open('/tmp/linux-native-view.log', errors='replace').read()
    except FileNotFoundError:
        txt = ''
    if 'RECEIVED:' in txt and any(c.isdigit() and c != '0' for c in txt.split('RECEIVED:')[-1][:20]):
        print("PASS native Linux controlled-side: viewer received frames")
        ok = True
        break
    if 'connect failed' in txt or 'TIMEOUT' in txt:
        print("FAIL: viewer connect failed")
        ok = False
        break
    time.sleep(1)
if not ok:
    print("E2E-FAIL: 60s 内未收到帧")
    sys.exit(1)
PY
if [ "${RC:-0}" != "0" ]; then
  set +e  # dump 块内 grep 无匹配（exit 1）不应中断
  echo "=== [dump/viewer] 启动段（ICE/DTLS/SCTP/DCEP） ==="
  head -80 /tmp/linux-native-view.log
  echo "=== [dump/viewer] 关键事件（去 PPS 噪声） ==="
  grep -avE 'non-existing PPS|decode_slice_header error|no frame' /tmp/linux-native-view.log | tail -200
  echo "=== [dump/viewer] PPS 噪声行数 ==="
  grep -ac 'non-existing PPS' /tmp/linux-native-view.log
  echo "=== [dump/pub] 启动段（编码器初始化） ==="
  head -60 /tmp/linux-native-pub.log
  echo "=== [dump/pub] 键帧/通道事件（去输入噪声） ==="
  grep -aE 'Keyframe|keyframe|ChannelOpen|channel|encode|frame' /tmp/linux-native-pub.log | grep -avE 'input: seq|inject: seq' | tail -120
  echo "=== [dump/sfu] 通道/轨道/键帧/背压事件 ==="
  grep -aE 'Client|channel|Channel|track|Track|negotiat|Keyframe|keyframe|signal_ready|背压' /tmp/linux-native-sfu.log | tail -150
  echo "=== [dump/signal] 日志尾 ==="
  tail -30 /tmp/linux-native-sig.log
  exit 1
fi

# #75 Linux 被控端真实光标（X11 QueryPointer → cursor 通道）：viewer 应打印 CURSOR。
echo "== [5b/5] 断言远程光标到达（CURSOR 日志）"
python3 - <<'PY'
import time, sys
ok = False
for i in range(25):  # 最多 25s（viewer CURSOR 日志节流 1s/条）
    try:
        txt = open('/tmp/linux-native-view.log', errors='replace').read()
    except FileNotFoundError:
        txt = ''
    if 'CURSOR:' in txt:
        print("PASS native Linux controlled-side: viewer received remote cursor")
        ok = True
        break
    time.sleep(1)
if not ok:
    print("FAIL: 25s 内未收到 CURSOR；viewer 日志尾：")
    print(open('/tmp/linux-native-view.log', errors='replace').read()[-1500:])
    sys.exit(1)
PY

# 发布端/SFU 无 panic
if grep -qiE "panic|auth failed" /tmp/linux-native-pub.log /tmp/linux-native-sfu.log; then
    echo "FAIL: publisher/SFU panic/error"
    tail -20 /tmp/linux-native-pub.log
    exit 1
fi

kill "$PUB" "$VIEW" "$SFU" "$SIG" "$XVFB" 2>/dev/null || true
echo "E2E DONE"
