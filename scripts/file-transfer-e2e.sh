#!/usr/bin/env bash
# #72 文件传输端到端：publisher --send-file → SFU → viewer --recv-dir。
# 断言：接收文件与源文件 SHA-256 一致。
# 用法: scripts/file-transfer-e2e.sh [房间] [文件大小 KB]
set -euo pipefail
cd "$(dirname "$0")/.."

ROOM="${1:-ftx-$(date +%s)}"
SIZE_KB="${2:-2048}"  # CI 默认 2MB（共享 runner 上 8MB 补包轮次会超窗口）；大文件用手动 100MB 验收
export RUST_LOG="${RUST_LOG:-info}"

echo "== 构建"
cargo build -q -p aerodesk-sfu -p aerodesk-signal -p aerodesk-cli

REC="$(mktemp -d)"
SRC="$REC/send-${SIZE_KB}kb.bin"
OUT="$REC/out"
mkdir -p "$OUT"
# 随机内容（不是全零，压缩/传输差异可检出）
dd if=/dev/urandom of="$SRC" bs=1024 count="$SIZE_KB" 2>/dev/null
SRC_HASH="$(shasum -a 256 "$SRC" | awk '{print $1}')"
echo "src: $SRC ($SIZE_KB KB) sha256=$SRC_HASH"

echo "== 启动 sfu/signal"
RECORD_DIR="$REC" ./target/debug/aerodesk-sfu >/tmp/ftx-sfu.log 2>&1 &
SFU_PID=$!
./target/debug/aerodesk-signal >/tmp/ftx-sig.log 2>&1 &
SIG_PID=$!
for _ in $(seq 1 50); do
    if nc -z 127.0.0.1 3003 2>/dev/null; then break; fi
    if ! kill -0 "$SIG_PID" 2>/dev/null; then
        echo "signal 服务器启动失败"; cat /tmp/ftx-sig.log; exit 1
    fi
    sleep 0.2
done
sleep 0.3

echo "== 启动 publisher（--send-file）+ viewer（--recv-dir）"
./target/debug/aerodesk-cli --role publisher --send-file "$SRC" \
    --signal ws://127.0.0.1:3003 --room "$ROOM" >/tmp/ftx-pub.log 2>&1 &
PUB_PID=$!
./target/debug/aerodesk-cli --role viewer --recv-dir "$OUT" \
    --signal ws://127.0.0.1:3003 --room "$ROOM" >/tmp/ftx-view.log 2>&1 &
VIEW_PID=$!

# 等待接收完成：按尺寸放大窗口（100MB 单发流控 ~7min，窗口留足余量；
# 成功会提前退出，不拖慢小文件）
WAIT_TICKS=$(( (SIZE_KB / 32 + 300) ))
done=0
for _ in $(seq 1 "$WAIT_TICKS"); do
    if grep -q "file receive complete" /tmp/ftx-view.log 2>/dev/null; then done=1; break; fi
    if ! kill -0 "$PUB_PID" 2>/dev/null || ! kill -0 "$VIEW_PID" 2>/dev/null; then break; fi
    sleep 0.2
done
kill "$PUB_PID" "$VIEW_PID" "$SFU_PID" "$SIG_PID" 2>/dev/null || true
wait 2>/dev/null || true

echo "== 断言"
fail=0
if [ "$done" = "1" ]; then
    echo "PASS receive complete"
else
    echo "FAIL receive not completed"; tail -5 /tmp/ftx-view.log; tail -5 /tmp/ftx-pub.log; fail=1
fi
OUT_FILE="$OUT/$(basename "$SRC")"
if [ -f "$OUT_FILE" ]; then
    OUT_HASH="$(shasum -a 256 "$OUT_FILE" | awk '{print $1}')"
    if [ "$OUT_HASH" = "$SRC_HASH" ]; then
        echo "PASS sha256 match ($OUT_HASH)"
    else
        echo "FAIL sha256 mismatch: src=$SRC_HASH out=$OUT_HASH"; fail=1
    fi
else
    echo "FAIL output file missing"; fail=1
fi
if grep -qiE "panic" /tmp/ftx-pub.log /tmp/ftx-view.log /tmp/ftx-sfu.log; then
    echo "FAIL panic in logs"; fail=1
    echo "---- sfu log ----"; tail -30 /tmp/ftx-sfu.log
fi
# #102：SFU 崩溃（Abort）时输出日志（仅在断言失败时，且 SFU 已非正常退出）
if [ "$fail" != "0" ] && ! kill -0 "$SFU_PID" 2>/dev/null && [ -f /tmp/ftx-sfu.log ]; then
    echo "NOTE sfu exited; log tail:"; tail -20 /tmp/ftx-sfu.log
fi

exit $fail
