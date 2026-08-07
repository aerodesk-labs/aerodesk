#!/usr/bin/env bash
# #72 文件传输取消回归：publisher --send-file + --cancel-send-after 中途取消
# → viewer 收到 FileCancel：接收器移除、不落盘、无残留临时文件、无 panic。
# 用法: scripts/file-cancel-e2e.sh [房间]
set -euo pipefail
cd "$(dirname "$0")/.."

ROOM="${1:-ftc-$(date +%s)}"
export RUST_LOG="${RUST_LOG:-info}"

echo "== 构建"
cargo build -q -p aerodesk-sfu -p aerodesk-signal -p aerodesk-cli

REC="$(mktemp -d)"
SRC="$REC/send-cancel.bin"
OUT="$REC/out"
mkdir -p "$OUT"
# 16MB：发送端 3s 启动延迟后单发节拍，6s 取消时仍在中途（覆盖 Meta 已到、
# 分片部分到达后取消的真实路径）。
dd if=/dev/urandom of="$SRC" bs=1M count=16 2>/dev/null

echo "== 启动 sfu/signal"
RECORD_DIR="$REC" ./target/debug/aerodesk-sfu >/tmp/ftc-sfu.log 2>&1 &
SFU_PID=$!
./target/debug/aerodesk-signal >/tmp/ftc-sig.log 2>&1 &
SIG_PID=$!
for _ in $(seq 1 50); do
    if nc -z 127.0.0.1 3003 2>/dev/null; then break; fi
    if ! kill -0 "$SIG_PID" 2>/dev/null; then
        echo "signal 服务器启动失败"; cat /tmp/ftc-sig.log; exit 1
    fi
    sleep 0.2
done
sleep 0.3

echo "== 启动 viewer（--recv-dir）+ publisher（--send-file --cancel-send-after 6）"
./target/debug/aerodesk-cli --role viewer --recv-dir "$OUT" \
    --signal ws://127.0.0.1:3003 --room "$ROOM" >/tmp/ftc-view.log 2>&1 &
VIEW_PID=$!
./target/debug/aerodesk-cli --role publisher --send-file "$SRC" --cancel-send-after 6 \
    --signal ws://127.0.0.1:3003 --room "$ROOM" >/tmp/ftc-pub.log 2>&1 &
PUB_PID=$!

# 轮询等取消传播（3s 启动延迟 + 发送 + 取消 + 收尾；CI 慢启动时固定 sleep
# 会误判，最多 ~30s）
PUB_CANCEL=0
VIEW_CANCEL=0
for _ in $(seq 1 60); do
    grep -q "file send cancelled" /tmp/ftc-pub.log 2>/dev/null && PUB_CANCEL=1
    grep -q "file .* cancelled" /tmp/ftc-view.log 2>/dev/null && VIEW_CANCEL=1
    if [ "$PUB_CANCEL" = "1" ] && [ "$VIEW_CANCEL" = "1" ]; then break; fi
    if ! kill -0 "$PUB_PID" 2>/dev/null || ! kill -0 "$VIEW_PID" 2>/dev/null; then break; fi
    sleep 0.5
done
kill "$PUB_PID" "$VIEW_PID" "$SFU_PID" "$SIG_PID" 2>/dev/null || true
wait 2>/dev/null || true

echo "== 断言"
fail=0
# 发送端确实触发了取消
if [ "$PUB_CANCEL" = "1" ]; then
    echo "PASS publisher cancelled send"
else
    echo "FAIL publisher cancel not triggered"; tail -5 /tmp/ftc-pub.log; fail=1
fi
# 接收端处理了 FileCancel（on_cancel 日志）
if [ "$VIEW_CANCEL" = "1" ]; then
    echo "PASS viewer handled FileCancel"
else
    echo "FAIL viewer cancel handler"; tail -5 /tmp/ftc-view.log; fail=1
fi
# 接收目录无残留文件（接收端只在 Done 校验通过后落盘，取消不应有文件）
REMAIN="$(find "$OUT" -type f | wc -l | tr -d ' ')"
if [ "$REMAIN" = "0" ]; then
    echo "PASS no residual file in recv dir"
else
    echo "FAIL residual file(s) in $OUT:"; find "$OUT" -type f; fail=1
fi
# 无 panic
if grep -qiE "panic" /tmp/ftc-pub.log /tmp/ftc-view.log /tmp/ftc-sfu.log; then
    echo "FAIL panic in logs"; fail=1
else
    echo "PASS no panics"
fi

exit $fail
