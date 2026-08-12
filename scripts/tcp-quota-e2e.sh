#!/usr/bin/env bash
# tcp-quota-e2e.sh —— SFU TCP 中继并发连接上限 e2e（#268）：
# SFU_TCP_MAX_CONNS=N 时，开 N+2 条 TCP 连接，断言 N 条保持打开、2 条被关闭。
set -euo pipefail
cd "$(dirname "$0")/.."

PORT=1478
MAX=4
ROOM="tcpq-$(date +%s)"

echo "== 构建"
cargo build -q -p aerodesk-sfu

echo "== 启动 sfu（SFU_TCP_MAX_CONNS=${MAX}，媒体端口 ${PORT}）"
SFU_MEDIA_PORT=$PORT SFU_TCP_MAX_CONNS=$MAX SFU_BIND_ADDRESS=127.0.0.1 \
  ./target/debug/aerodesk-sfu >/tmp/tcpq-sfu.log 2>&1 &
SFU=$!
trap 'kill $SFU 2>/dev/null || true' EXIT
# 前序脚本 SFU 收 SIGTERM 后可能仍在收尾；先等 1478（媒体端口）完全释放，
# 避免本 SFU 绑定失败（Address already in use，CI 顺序执行偶发）。
for _ in $(seq 1 50); do
    if ! nc -z 127.0.0.1 1478 2>/dev/null; then break; fi
    sleep 0.2
done
for _ in $(seq 1 50); do
  if nc -z 127.0.0.1 "$PORT" 2>/dev/null; then break; fi
  sleep 0.2
done
sleep 0.3

echo "== 打开 $((MAX+2)) 条 TCP 连接，断言 $MAX 条保持打开"
python3 - "$PORT" "$MAX" <<'PYEOF'
import socket, sys, time
port, max_conns = int(sys.argv[1]), int(sys.argv[2])
socks = []
for i in range(max_conns + 2):
    s = socket.create_connection(("127.0.0.1", port), timeout=3)
    s.setblocking(False)
    socks.append(s)
# 等服务端 accept + 处理超限（accept 循环 10ms 轮询 + 关闭）。
time.sleep(1.0)
open_count, closed = 0, 0
for s in socks:
    try:
        d = s.recv(1)
        if d == b"":
            closed += 1
        else:
            open_count += 1
    except BlockingIOError:
        open_count += 1
    except ConnectionResetError:
        closed += 1
    except OSError:
        closed += 1
print(f"  open={open_count} closed={closed}")
if open_count != max_conns or closed != 2:
    print(f"FAIL: 期望 open={max_conns} closed=2，实际 open={open_count} closed={closed}")
    sys.exit(1)
print(f"PASS: 并发上限生效（open={max_conns} closed=2）")
PYEOF
kill "$SFU" 2>/dev/null || true
echo "TCP-QUOTA E2E PASS"
