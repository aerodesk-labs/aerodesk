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
# #109 权限/审计：e2e 用临时路径，避免污染 $HOME。
export AERODESK_CMD_ALLOWLIST="/tmp/aerodesk-cmd-allow-$ROOM.txt"
export AERODESK_CMD_AUDIT="/tmp/aerodesk-cmd-audit-$ROOM.jsonl"

echo "== 构建"
cargo build -q -p aerodesk-sfu -p aerodesk-signal -p aerodesk-agent

REC="$(mktemp -d)"
echo "== 启动 sfu/signal"
RECORD_DIR="$REC" ./target/debug/aerodesk-sfu >/tmp/cmd-sfu.log 2>&1 &
SFU_PID=$!
SIP_UDP_PORT=5060 ./target/debug/aerodesk-signal >/tmp/cmd-sig.log 2>&1 &
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
./target/debug/aerodesk-agent --role publisher --encoder x264 \
    --signal ws://127.0.0.1:3003 --room "$ROOM" >/tmp/cmd-pub.log 2>&1 &
PUB_PID=$!
sleep 2

fail=0

# 用例执行器：启动 viewer --run-command，等其自行退出（≤30s），断言日志。
run_case() {
  local name="$1" cmd="$2" log="$3"
  echo "== case: $name"
  ./target/debug/aerodesk-agent --role viewer --run-command "$cmd" \
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

DIR="/tmp/aerodesk-cmd-e2e-$ROOM"
rm -rf "$DIR"; mkdir -p "$DIR"
# 4) 写文件 + 读回
echo "== case: write-file"
./target/debug/aerodesk-agent --role viewer --write-file "$DIR/hello.txt" "hello-aerodesk-file" \
    --signal ws://127.0.0.1:3003 --room "$ROOM" >/tmp/cmd-view4.log 2>&1 &
VPID=$!
for _ in $(seq 1 60); do ! kill -0 "$VPID" 2>/dev/null && break; sleep 0.5; done
kill "$VPID" 2>/dev/null || true; wait "$VPID" 2>/dev/null || true
if grep -q "CMD_RESULT: ok=true type=file" /tmp/cmd-view4.log; then
    echo "PASS write-file"
else
    echo "FAIL write-file"; tail -5 /tmp/cmd-view4.log; fail=1
fi
echo "== case: read-file"
./target/debug/aerodesk-agent --role viewer --read-file "$DIR/hello.txt" \
    --signal ws://127.0.0.1:3003 --room "$ROOM" >/tmp/cmd-view5.log 2>&1 &
VPID=$!
for _ in $(seq 1 60); do ! kill -0 "$VPID" 2>/dev/null && break; sleep 0.5; done
kill "$VPID" 2>/dev/null || true; wait "$VPID" 2>/dev/null || true
if grep -q "CMD_RESULT: ok=true type=file" /tmp/cmd-view5.log && grep -q "hello-aerodesk-file" /tmp/cmd-view5.log; then
    echo "PASS read-file 内容一致"
else
    echo "FAIL read-file"; tail -5 /tmp/cmd-view5.log; fail=1
fi
# 5) 进程列表
echo "== case: list-processes"
./target/debug/aerodesk-agent --role viewer --list-processes \
    --signal ws://127.0.0.1:3003 --room "$ROOM" >/tmp/cmd-view6.log 2>&1 &
VPID=$!
for _ in $(seq 1 60); do ! kill -0 "$VPID" 2>/dev/null && break; sleep 0.5; done
kill "$VPID" 2>/dev/null || true; wait "$VPID" 2>/dev/null || true
if grep -q "CMD_RESULT: ok=true type=ps count=[1-9]" /tmp/cmd-view6.log; then
    echo "PASS list-processes"
else
    echo "FAIL list-processes"; tail -5 /tmp/cmd-view6.log; fail=1
fi
# 6) kill：后台 sleep → 记 pid → kill → 进程消失
echo "== case: kill-process"
run_case "kill-spawn" "sleep 100 & echo \$! > $DIR/pid" /tmp/cmd-view7.log
if ! grep -q "CMD_RESULT: ok=true" /tmp/cmd-view7.log; then
    echo "FAIL kill-spawn"; tail -5 /tmp/cmd-view7.log; fail=1
fi
PID=$(cat "$DIR/pid" 2>/dev/null || true)
if [ -z "$PID" ]; then echo "FAIL kill-spawn: 未拿到 pid"; fail=1; fi
if [ -n "$PID" ]; then
    # 用 --kill-pid 走协议层结束后台 sleep
    ./target/debug/aerodesk-agent --role viewer --kill-pid "$PID" \
        --signal ws://127.0.0.1:3003 --room "$ROOM" >/tmp/cmd-view9.log 2>&1 &
    VPID=$!
    for _ in $(seq 1 60); do ! kill -0 "$VPID" 2>/dev/null && break; sleep 0.5; done
    kill "$VPID" 2>/dev/null || true; wait "$VPID" 2>/dev/null || true
    if grep -q "CMD_RESULT: ok=true type=kill pid=$PID" /tmp/cmd-view9.log; then
        echo "PASS kill-process"
    else
        echo "FAIL kill-process"; tail -5 /tmp/cmd-view9.log; fail=1
    fi
    run_case "kill-verify-gone" "ps -p $PID" /tmp/cmd-view10.log || true
    if ! grep -qE "CMD_STDOUT:.*$PID" /tmp/cmd-view10.log; then
        echo "PASS kill 后进程不存在"
    else
        echo "FAIL kill 后进程仍存在"; tail -5 /tmp/cmd-view10.log; fail=1
    fi
fi

# 7) 审计查询：此前已执行多条命令，--cmd-audit 应能查到。
echo "== case: audit"
./target/debug/aerodesk-agent --cmd-audit 50 >/tmp/cmd-audit.log 2>&1 || true
if grep -q "echo hello-aerodesk-cmd" /tmp/cmd-audit.log && grep -q "rm -rf /" /tmp/cmd-audit.log; then
    echo "PASS audit tail 含命令记录"
else
    echo "FAIL audit"; tail -5 /tmp/cmd-audit.log; fail=1
fi
# 8) 白名单管理：add → list 可见 → remove → list 消失。
echo "== case: allowlist"
AERODESK_CMD_ALLOWLIST="$AERODESK_CMD_ALLOWLIST" ./target/debug/aerodesk-agent --cmd-allowlist add "/tmp/aerodesk-cmd-e2e-$ROOM" >/tmp/cmd-allow1.log 2>&1
AERODESK_CMD_ALLOWLIST="$AERODESK_CMD_ALLOWLIST" ./target/debug/aerodesk-agent --cmd-allowlist list >/tmp/cmd-allow2.log 2>&1
if grep -q "/tmp/aerodesk-cmd-e2e-$ROOM" /tmp/cmd-allow2.log; then
    echo "PASS allowlist add+list"
else
    echo "FAIL allowlist add"; tail -3 /tmp/cmd-allow2.log; fail=1
fi
AERODESK_CMD_ALLOWLIST="$AERODESK_CMD_ALLOWLIST" ./target/debug/aerodesk-agent --cmd-allowlist remove "/tmp/aerodesk-cmd-e2e-$ROOM" >/tmp/cmd-allow3.log 2>&1
AERODESK_CMD_ALLOWLIST="$AERODESK_CMD_ALLOWLIST" ./target/debug/aerodesk-agent --cmd-allowlist list >/tmp/cmd-allow4.log 2>&1
if ! grep -q "/tmp/aerodesk-cmd-e2e-$ROOM" /tmp/cmd-allow4.log; then
    echo "PASS allowlist remove"
else
    echo "FAIL allowlist remove"; tail -3 /tmp/cmd-allow4.log; fail=1
fi

kill "$PUB_PID" "$SFU_PID" "$SIG_PID" 2>/dev/null || true
wait 2>/dev/null || true
python3 - <<PYEOF
import os
for p in ["$AERODESK_CMD_ALLOWLIST", "$AERODESK_CMD_AUDIT"]:
    if os.path.exists(p):
        os.remove(p)
PYEOF

if grep -qiE "panic" /tmp/cmd-pub.log /tmp/cmd-view[0-9]*.log /tmp/cmd-sfu.log; then
    echo "FAIL panic in logs"; fail=1
else
    echo "PASS no panics"
fi

exit $fail
