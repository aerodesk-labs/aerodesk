#!/usr/bin/env bash
# #109 远程命令端到端：viewer --run-command → SFU → publisher（macOS 执行器）→ 回传 stdout/stderr/exit。
# 断言：
#   1. 正常命令：ok=true、exit=0、stdout 含输出
#   2. 非零退出：exit=3、ok=false、stderr 含内容
#   3. 危险命令默认拦截：error 含 "blocked by policy"（rm -rf / 不执行）
#   4. 无 panic
# 用法: scripts/cmd-e2e.sh [房间]
set -euo pipefail
cd "$(dirname "$0")/.."

ROOM="${1:-cmd-$(date +%s)}"
export RUST_LOG="${RUST_LOG:-info}"

echo "== 构建"
cargo build -q -p aerodesk-sfu -p aerodesk-signal -p aerodesk-cli

REC="$(mktemp -d)"
echo "== 启动 sfu/signal"
RECORD_DIR="$REC" ./target/debug/aerodesk-sfu >/tmp/cmd-sfu.log 2>&1 &
SFU_PID=$!
./target/debug/aerodesk-signal >/tmp/cmd-sig.log 2>&1 &
SIG_PID=$!
for _ in $(seq 1 50); do
    if nc -z 127.0.0.1 3003 2>/dev/null && nc -z 127.0.0.1 3002 2>/dev/null; then break; fi
    if ! kill -0 "$SFU_PID" 2>/dev/null || ! kill -0 "$SIG_PID" 2>/dev/null; then
        echo "sfu/signal 启动失败"; tail -5 /tmp/cmd-sfu.log; tail -5 /tmp/cmd-sig.log; exit 1
    fi
    sleep 0.2
done
sleep 0.3

echo "== 启动 publisher（被控端，x264）"
./target/debug/aerodesk-cli --role publisher --encoder x264 \
    --signal ws://127.0.0.1:3003 --room "$ROOM" >/tmp/cmd-pub.log 2>&1 &
PUB_PID=$!
sleep 2

fail=0

# 用例执行器：启动 viewer --run-command，等其自行退出（≤30s），断言日志。
run_case() {
  local name="$1" cmd="$2" log="$3"
  echo "== case: $name"
  ./target/debug/aerodesk-cli --role viewer --run-command "$cmd" \
      --signal ws://127.0.0.1:3003 --room "$ROOM" >"$log" 2>&1 &
  local vpid=$!
  for _ in $(seq 1 60); do
    if ! kill -0 "$vpid" 2>/dev/null; then break; fi
    sleep 0.5
  done
  if kill -0 "$vpid" 2>/dev/null; then
    echo "FAIL case $name: viewer 未在 30s 内退出"; kill "$vpid" 2>/dev/null || true; wait "$vpid" 2>/dev/null || true; fail=1
  else
    wait "$vpid" 2>/dev/null || true
  fi
}

run_case "normal-echo" 'echo hello-aerodesk-cmd' /tmp/cmd-view1.log
if grep -q "CMD_RESULT: ok=true exit=Some(0)" /tmp/cmd-view1.log && grep -q "hello-aerodesk-cmd" /tmp/cmd-view1.log; then
    echo "PASS normal: ok=true exit=0 stdout 含输出"
else
    echo "FAIL normal"; tail -5 /tmp/cmd-view1.log; fail=1
fi

run_case "nonzero-exit" 'echo oops >&2; exit 3' /tmp/cmd-view2.log
if grep -q "CMD_RESULT: ok=false exit=Some(3)" /tmp/cmd-view2.log && grep -q "oops" /tmp/cmd-view2.log; then
    echo "PASS nonzero: exit=3 stderr 含内容"
else
    echo "FAIL nonzero"; tail -5 /tmp/cmd-view2.log; fail=1
fi

run_case "dangerous-blocked" 'rm -rf /' /tmp/cmd-view3.log
if grep -q "blocked by policy" /tmp/cmd-view3.log && grep -q "CMD_RESULT: ok=false" /tmp/cmd-view3.log; then
    echo "PASS dangerous: 默认拦截（rm -rf / 未执行）"
else
    echo "FAIL dangerous"; tail -5 /tmp/cmd-view3.log; fail=1
fi

kill "$PUB_PID" "$SFU_PID" "$SIG_PID" 2>/dev/null || true
wait 2>/dev/null || true

if grep -qiE "panic" /tmp/cmd-pub.log /tmp/cmd-view1.log /tmp/cmd-view2.log /tmp/cmd-view3.log /tmp/cmd-sfu.log; then
    echo "FAIL panic in logs"; fail=1
else
    echo "PASS no panics"
fi

exit $fail
