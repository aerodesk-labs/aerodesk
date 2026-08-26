#!/usr/bin/env bash
# #72/#503-2 剪贴板双向同步端到端（macOS）：viewer 剪贴板 → file 通道 → publisher
# 落地；publisher 剪贴板变化 → file 通道 → viewer 落地。文本 + 图片（PNG）双向。
# 单机测试：两个进程共享系统剪贴板，用日志断言两个方向都真实走通。
# 图片方向（#503-2）：osascript 写 PNG 到剪贴板（无 AppleEvents 授权时 SKIP）；
# 两进程各自轮询发送一次、各自落地一次，日志同时出现 sent/apply 即双向通过。
# 用法: scripts/clipboard-e2e.sh [房间]
set -euo pipefail
cd "$(dirname "$0")/.."

ROOM="${1:-clip-$(date +%s)}"
export RUST_LOG="${RUST_LOG:-info}"

echo "== 构建"
cargo build -q -p aerodesk-sfu -p aerodesk-signal -p aerodesk-agent

REC="$(mktemp -d)"
echo "== 启动 sfu/signal"
RECORD_DIR="$REC" ./target/debug/aerodesk-sfu >/tmp/clip-sfu.log 2>&1 &
SFU_PID=$!
SIP_UDP_PORT=5060 ./target/debug/aerodesk-signal >/tmp/clip-sig.log 2>&1 &
SIG_PID=$!
for _ in $(seq 1 50); do
    if nc -z 127.0.0.1 3003 2>/dev/null; then break; fi
    if ! kill -0 "$SIG_PID" 2>/dev/null; then
        echo "signal 服务器启动失败"; cat /tmp/clip-sig.log; exit 1
    fi
    sleep 0.2
done
sleep 0.3

# #584 SIP 1:1：publisher 先注册被叫、viewer 后呼入（viewer 先起时 INVITE 无绑定
# 走会议桥，双向剪贴板链路建不起来）。
echo "== 预置剪贴板 AAA，启动 publisher + viewer"
printf 'AAA' | pbcopy
./target/debug/aerodesk-agent --role publisher \
    --signal ws://127.0.0.1:3003 --room "$ROOM" >/tmp/clip-pub.log 2>&1 &
PUB_PID=$!
sleep 2
./target/debug/aerodesk-agent --role viewer \
    --signal ws://127.0.0.1:3003 --room "$ROOM" >/tmp/clip-view.log 2>&1 &
VIEW_PID=$!

# 方向1：viewer 轮询到 AAA → file 通道 → publisher 落地
dir1=0
for _ in $(seq 1 50); do
    if grep -q "clipboard: apply 3 chars from remote" /tmp/clip-pub.log 2>/dev/null; then dir1=1; break; fi
    sleep 0.2
done

# 方向2：本机剪贴板改为 BBB → publisher 轮询 → file 通道 → viewer 落地
printf 'BBB' | pbcopy
dir2=0
for _ in $(seq 1 50); do
    if grep -q "clipboard: apply 3 chars from remote" /tmp/clip-view.log 2>/dev/null; then dir2=1; break; fi
    sleep 0.2
done

# 方向3/4（#503-2）：图片双向——osascript 写 1x1 红色 PNG 到剪贴板；两端各自
# 轮询发送一次、各自落地一次（sent 与 apply 日志配对即双向真实走通）。
# 无 AppleEvents 授权（非交互会话）时 SKIP，不影响文本方向结论。
dir3=0
dir4=0
IMG="$REC/clip-img.png"
printf '\x89\x50\x4E\x47\x0D\x0A\x1A\x0A\x00\x00\x00\x0D\x49\x48\x44\x52\x00\x00\x00\x01\x00\x00\x00\x01\x08\x02\x00\x00\x00\x90\x77\x53\xDE\x00\x00\x00\x0C\x49\x44\x41\x54\x78\x9C\x62\x00\x01\x00\x00\x05\x00\x01\x0D\x0A\x2D\xB4\x00\x00\x00\x00\x49\x45\x4E\x44\xAE\x42\x60\x82' > "$IMG"
if osascript -e "set the clipboard to (read (POSIX file \"$IMG\") as «class PNGf»)" >/dev/null 2>&1; then
    for _ in $(seq 1 50); do
        if grep -q "clipboard: sent image" /tmp/clip-view.log 2>/dev/null \
            && grep -q "clipboard: apply image" /tmp/clip-pub.log 2>/dev/null; then dir3=1; break; fi
        sleep 0.2
    done
    for _ in $(seq 1 50); do
        if grep -q "clipboard: sent image" /tmp/clip-pub.log 2>/dev/null \
            && grep -q "clipboard: apply image" /tmp/clip-view.log 2>/dev/null; then dir4=1; break; fi
        sleep 0.2
    done
else
    echo "SKIP 图片方向：osascript 无法写入图片剪贴板（无 AppleEvents 授权）"
fi

kill "$VIEW_PID" "$PUB_PID" "$SFU_PID" "$SIG_PID" 2>/dev/null || true
wait 2>/dev/null || true

# 清理剪贴板：测试图（1x1 PNG）残留会给同 job 后续 step（simulcast 等的
# viewer 剪贴板轮询）造成环境污染——viewer 捡到残留图上传并确认后提前退出
# （#595 回归：simulcast e2e 6 连败，RECEIVED 0 帧）。
printf '' | pbcopy || true

echo "== 断言"
fail=0
if [ "$dir1" = "1" ]; then
    echo "PASS viewer->publisher clipboard text (AAA applied)"
else
    echo "FAIL viewer->publisher text not received"; tail -5 /tmp/clip-pub.log; tail -5 /tmp/clip-view.log; fail=1
fi
if [ "$dir2" = "1" ]; then
    echo "PASS publisher->viewer clipboard text (BBB applied)"
else
    echo "FAIL publisher->viewer text not received"; tail -5 /tmp/clip-pub.log; tail -5 /tmp/clip-view.log; fail=1
fi
if [ "$dir3" = "1" ]; then
    echo "PASS viewer->publisher clipboard image (sent + applied)"
else
    echo "FAIL viewer->publisher image not received"; tail -5 /tmp/clip-pub.log; tail -5 /tmp/clip-view.log; fail=1
fi
if [ "$dir4" = "1" ]; then
    echo "PASS publisher->viewer clipboard image (sent + applied)"
else
    echo "FAIL publisher->viewer image not received"; tail -5 /tmp/clip-pub.log; tail -5 /tmp/clip-view.log; fail=1
fi
if grep -qiE "panic" /tmp/clip-pub.log /tmp/clip-view.log /tmp/clip-sfu.log; then
    echo "FAIL panic in logs"; fail=1
else
    echo "PASS no panics"
fi

exit $fail
