#!/usr/bin/env bash
# #216 M3：桥接编排端到端——BRIDGE_CMD 桥优先 + 失败回退 Redirect + 延迟分布验收。
#
# 双模式：
# - local（默认，CI）：本地双 SFU 模拟双 PoP（148xx/149xx 独立端口），五场景全跑：
#   0 直连基线 → 1 桥优先 → 3 桥死亡重建 → 4 连接中 viewer 自动恢复 → 2 失败回退
# - remote（真实多 PoP 部署验收，#253）：设置 POP_A_SIGNAL / POP_B_SIGNAL（如
#   wss://pop-a.example.com/ws）即远程模式——不拉起本地 PoP，直接对真实端点执行：
#   0 直连基线（A）→ 1 桥优先（viewer@B 无 Redirect 解码）→ 延迟 ≥30 样本
#   p50/p90/p99 → （可选 BRIDGE_KILL_CMD）4 桥死亡自动恢复。
#   远程模式需 AUTH（信令 token）；BRIDGE_KILL_CMD 例：
#     'ssh pop-b "pkill -f aerodesk-bridge"'  或  'systemctl restart aerodesk-signal'
# - REMOTE_LOOPBACK=1（#257）：本地起双 PoP（同 local）但走 remote 断言流——
#   端到端验证远程验收工具（无本地日志断言、BRIDGE_KILL_CMD 恢复用
#   pkill -f aerodesk-bridge）。CI 双跑。
#
# 延迟 p99 断言（两种模式）：桥 p99 ≤ 直连 p99×4+500ms（SCTP 每跳 ~150ms，见 BRIDGE.md）。
set -uo pipefail
cd "$(dirname "$0")/.."
export RUST_LOG="${RUST_LOG:-info}"
TARGET_DIR="${CARGO_TARGET_DIR:-$PWD/target}/debug"

fail() { echo "FAIL: $*"; exit 1; }

REMOTE=0
# #257：REMOTE_LOOPBACK=1 → 本地起双 PoP 但走 remote 断言流（端到端验证远程工具），
# 优先级高于 POP_A_SIGNAL/POP_B_SIGNAL（后者被忽略）。
if [ "${REMOTE_LOOPBACK:-}" = "1" ]; then
  REMOTE=1
  if [ -n "${POP_A_SIGNAL:-}" ] || [ -n "${POP_B_SIGNAL:-}" ]; then
    echo "WARN: REMOTE_LOOPBACK 优先，忽略 POP_A_SIGNAL/POP_B_SIGNAL" >&2
  fi
  echo "== REMOTE_LOOPBACK 模式：本地双 PoP + remote 断言流（#257）"
elif [ -n "${POP_A_SIGNAL:-}" ] || [ -n "${POP_B_SIGNAL:-}" ]; then
  if [ -n "${POP_A_SIGNAL:-}" ] && [ -n "${POP_B_SIGNAL:-}" ]; then
    REMOTE=1
  else
    echo "WARN: 只设置了 POP_A_SIGNAL/POP_B_SIGNAL 之一，按 local 模式运行；远程模式需两者都设" >&2
  fi
fi

ROOM="bridge-fb-$(date +%s)"
# PoP-A
SIG_A=14800; INT_A=14802; PLAIN_A=14803; MEDIA_A=14878
# PoP-B
SIG_B=14900; INT_B=14902; PLAIN_B=14903; MEDIA_B=14978
if [ "$REMOTE" = "1" ] && [ "${REMOTE_LOOPBACK:-}" != "1" ]; then
  SIG_A_URL="$POP_A_SIGNAL"; SIG_B_URL="$POP_B_SIGNAL"
else
  SIG_A_URL="ws://127.0.0.1:${PLAIN_A}"; SIG_B_URL="ws://127.0.0.1:${PLAIN_B}"
fi
AUTH="${AUTH:-}"
BRIDGE_KILL_CMD="${BRIDGE_KILL_CMD:-}"
# 生产认证路径：信令 AUTH_TOKENS 校验；桥经 BRIDGE_AUTH_TOKEN 注入 --auth-token。
if [ "$REMOTE" = "1" ] && [ "${REMOTE_LOOPBACK:-}" != "1" ]; then
  [ -n "$AUTH" ] || fail "远程模式必须设置 AUTH（信令 token）"
  BRIDGE_CMD="${BRIDGE_CMD:-}"
  [ -n "$BRIDGE_CMD" ] || fail "远程模式必须设置 BRIDGE_CMD（与 PoP-B 信令同款命令模板，含 {room}；实际由 PoP-B 信令执行，本脚本仅校验非空）"
else
  AUTH="test-bridge-token"
  BRIDGE_CMD="$TARGET_DIR/aerodesk-bridge --remote-signal ${SIG_A_URL} --local-signal ${SIG_B_URL} --room {room} --auth-token \"\$BRIDGE_AUTH_TOKEN\" --codec h264"
fi

cleanup() {
  pkill -f 'aerodesk-bridge' 2>/dev/null || true
  pkill -f 'aerodesk-cli' 2>/dev/null || true
  # #257 review：kill 后 wait，确保端口释放再退出（CI 同 step 双跑防 bind 竞争）。
  [ -n "${SFU_A:-}" ] && { kill "$SFU_A" 2>/dev/null || true; wait "$SFU_A" 2>/dev/null || true; }
  [ -n "${SFU_B:-}" ] && { kill "$SFU_B" 2>/dev/null || true; wait "$SFU_B" 2>/dev/null || true; }
  [ -n "${SIG_A_PID:-}" ] && { kill "$SIG_A_PID" 2>/dev/null || true; wait "$SIG_A_PID" 2>/dev/null || true; }
  [ -n "${SIG_B_PID:-}" ] && { kill "$SIG_B_PID" 2>/dev/null || true; wait "$SIG_B_PID" 2>/dev/null || true; }
}
trap cleanup EXIT

# 输出 "p50 p90 p99"（无样本输出 "NONE NONE NONE"）。
latency_stats() {
  python3 - "$1" <<'PY'
import re, sys
s = open(sys.argv[1]).read()
vals = sorted(int(m) for m in re.findall(r'LATENCY: (\d+) ms', s))
if not vals:
    print("NONE NONE NONE"); raise SystemExit(0)
def pct(p):
    # 截断式索引（保守偏高；n=30 时 p50/p90 比 nearest-rank 高一档，p99=max）
    return vals[min(len(vals)-1, int(len(vals)*p))]
print(pct(0.50), pct(0.90), pct(0.99))
PY
}
latency_count() { local c; c=$(grep -c "LATENCY:" "$1" 2>/dev/null); echo "${c:-0}"; }

wait_decoded() { # $1=logfile
  for _ in $(seq 1 240); do
    grep -qE "DECODED: [1-9]" "$1" 2>/dev/null && return 0
    sleep 0.5
  done
  return 1
}
# 等某日志出现子串（$1=logfile $2=pattern $3=循环次数默认 240）
wait_log() {
  local n="${3:-240}"
  for _ in $(seq 1 "$n"); do
    grep -q "$2" "$1" 2>/dev/null && return 0
    sleep 0.5
  done
  return 1
}

echo "== 构建"
if [ "$REMOTE" = "1" ] && [ "${REMOTE_LOOPBACK:-}" != "1" ]; then
  cargo build -q -p aerodesk-cli   # 远程验收只需 CLI；SFU/signal/bridge 在 PoP 上
else
  cargo build -q -p aerodesk-sfu -p aerodesk-signal -p aerodesk-cli -p aerodesk-bridge
fi
REC_A="$(mktemp -d)"; REC_B="$(mktemp -d)"

if [ "$REMOTE" = "0" ] || [ "${REMOTE_LOOPBACK:-}" = "1" ]; then
  echo "== 启动 PoP-A（148xx）"
  RECORD_DIR="$REC_A" SFU_MEDIA_PORT="$MEDIA_A" SFU_SIGNAL_PORT="$SIG_A" SFU_INTERNAL_PORT="$INT_A" \
    "$TARGET_DIR/aerodesk-sfu" >/tmp/bfb-sfu-a.log 2>&1 &
  SFU_A=$!
  POP_ID=pop-a AUTH_TOKENS="$AUTH" SIGNAL_PORT=14801 SIGNAL_PLAIN_PORT="$PLAIN_A" SFU_URL="http://127.0.0.1:${INT_A}" \
    "$TARGET_DIR/aerodesk-signal" >/tmp/bfb-sig-a.log 2>&1 &
  SIG_A_PID=$!
  for _ in $(seq 1 80); do nc -z 127.0.0.1 "$PLAIN_A" 2>/dev/null && break; sleep 0.2; done
  sleep 0.3
elif [ "${REMOTE_LOOPBACK:-}" != "1" ]; then
  echo "== 远程模式：使用外部 PoP（A=${POP_A_SIGNAL} B=${POP_B_SIGNAL}），跳过本地启动"
fi

echo "== 场景 0：直连延迟基线（同 PoP-A publisher+viewer）"
"$TARGET_DIR/aerodesk-cli" --role publisher --signal "$SIG_A_URL" --room "$ROOM" --token "$AUTH" \
  --encoder vt --width 1280 --height 720 --fps 30 --bitrate 2000000 --noisy \
  >/tmp/bfb-direct-pub.log 2>&1 &
PUB0=$!
ok=0
for _ in $(seq 1 120); do grep -q "ICE connected" /tmp/bfb-direct-pub.log 2>/dev/null && ok=1 && break; sleep 0.5; done
[ "$ok" = "1" ] || fail "场景0：publisher 未连上"
"$TARGET_DIR/aerodesk-cli" --role viewer --signal "$SIG_A_URL" --room "$ROOM" --token "$AUTH" \
  >/tmp/bfb-direct-view.log 2>&1 &
VIEW0=$!
wait_decoded /tmp/bfb-direct-view.log || fail "场景0：直连 viewer 未解码"
# 样本不足时最多等两轮（数据通道偶发中断后恢复）。
for _attempt in 1 2; do
  for _ in $(seq 1 160); do
    [ "$(latency_count /tmp/bfb-direct-view.log)" -ge 30 ] && break
    sleep 0.5
  done
  [ "$(latency_count /tmp/bfb-direct-view.log)" -ge 30 ] && break
done
DIRECT_STATS=$(latency_stats /tmp/bfb-direct-view.log)
DIRECT_P99=$(echo "$DIRECT_STATS" | awk '{print $3}')
DIRECT_N=$(latency_count /tmp/bfb-direct-view.log)
echo "  直连基线：samples=${DIRECT_N} p50/p90/p99=${DIRECT_STATS}ms"
[ "${DIRECT_N:-0}" -ge 30 ] || fail "场景0：直连样本不足（N=${DIRECT_N}，需 ≥30）"
[ "$DIRECT_P99" != "NONE" ] || fail "场景0：无 LATENCY 样本"
kill "$VIEW0" "$PUB0" 2>/dev/null || true; sleep 1

if [ "$REMOTE" = "0" ] || [ "${REMOTE_LOOPBACK:-}" = "1" ]; then
  echo "== 启动 PoP-B（149xx，BRIDGE_CMD 桥优先）"
  RECORD_DIR="$REC_B" SFU_MEDIA_PORT="$MEDIA_B" SFU_SIGNAL_PORT="$SIG_B" SFU_INTERNAL_PORT="$INT_B" \
    "$TARGET_DIR/aerodesk-sfu" >/tmp/bfb-sfu-b.log 2>&1 &
  SFU_B=$!
  POP_ID=pop-b AUTH_TOKENS="$AUTH" ROOM_POP_MAP="bridge-=pop-a" POP_URLS="pop-a=${SIG_A_URL}" \
    BRIDGE_CMD="$BRIDGE_CMD" BRIDGE_READY_TIMEOUT_SECS=20 BRIDGE_AUTH_TOKEN="$AUTH" \
    BRIDGE_MONITOR_INTERVAL_SECS=2 \
    SIGNAL_PORT=14901 SIGNAL_PLAIN_PORT="$PLAIN_B" SFU_URL="http://127.0.0.1:${INT_B}" \
    "$TARGET_DIR/aerodesk-signal" >/tmp/bfb-sig-b.log 2>&1 &
  SIG_B_PID=$!
  for _ in $(seq 1 80); do nc -z 127.0.0.1 "$PLAIN_B" 2>/dev/null && break; sleep 0.2; done
  sleep 0.3
  grep -q "bridge orchestration enabled" /tmp/bfb-sig-b.log || fail "PoP-B 未启用桥编排（BRIDGE_CMD 未生效）"
  echo "  PoP-B bridge orchestration enabled"
else
  echo "== 场景 1（远程）：PoP-B viewer 经桥接入（需 PoP-B 信令已配 BRIDGE_CMD/ROOM_POP_MAP/POP_URLS）"
fi

echo "== 场景 1：PoP-A publisher + PoP-B viewer（桥优先，不 Redirect）"
# --audio（PCMU）：验证 #260 音频跨 PoP 桥转发。
"$TARGET_DIR/aerodesk-cli" --role publisher --signal "$SIG_A_URL" --room "$ROOM" --token "$AUTH" \
  --encoder vt --width 1280 --height 720 --fps 30 --bitrate 2000000 --noisy --audio \
  >/tmp/bfb-pub-a.log 2>&1 &
PUB_A=$!
ok=0
for _ in $(seq 1 120); do grep -q "ICE connected" /tmp/bfb-pub-a.log 2>/dev/null && ok=1 && break; sleep 0.5; done
[ "$ok" = "1" ] || fail "场景1：PoP-A publisher 未连上"; echo "  publisher connected"

# 远程模式场景 4（可选 BRIDGE_KILL_CMD）依赖 viewer --reconnect；本地模式场景 3 会
# 另起带 --reconnect 的 viewer，此处带上也无害。
RECONNECT_FLAG=""
[ "$REMOTE" = "1" ] && RECONNECT_FLAG="--reconnect"
# --display 1：验证 #260 显示器切换经 control 通道跨 PoP（到主 PoP publisher）。
"$TARGET_DIR/aerodesk-cli" --role viewer --signal "$SIG_B_URL" --room "$ROOM" --token "$AUTH" $RECONNECT_FLAG --display 1 \
  >/tmp/bfb-view-b.log 2>&1 &
VIEW_B=$!
wait_decoded /tmp/bfb-view-b.log || fail "场景1：PoP-B viewer 未解码跨 PoP 媒体（见 /tmp/bfb-view-b.log）"
grep -q "signal redirect" /tmp/bfb-view-b.log && fail "场景1：viewer 不应收到 Redirect（桥优先应本 PoP 接入）"
if [ "$REMOTE" = "0" ]; then
  grep -q "bridge ready" /tmp/bfb-sig-b.log || fail "场景1：PoP-B 信令未记录 bridge ready"
  grep -q "bridge: spawned" /tmp/bfb-sig-b.log || fail "场景1：PoP-B 信令未自动 spawn 桥"
fi
echo "  场景1 PASS：viewer 本 PoP 接入（无 Redirect）"
DECODED=$(grep -oE "DECODED: [0-9]+" /tmp/bfb-view-b.log | tail -1 | cut -d' ' -f2)
echo "  viewer DECODED=${DECODED}"

# #260：音频跨 PoP（viewer AUDIO 帧 >0）+ 显示器切换到达主 PoP publisher。
ok=0
for _ in $(seq 1 60); do
  grep -qE "AUDIO: [1-9]" /tmp/bfb-view-b.log 2>/dev/null && ok=1 && break
  sleep 0.5
done
[ "$ok" = "1" ] || fail "场景1：viewer 未收到跨 PoP 音频（AUDIO=0，见 /tmp/bfb-view-b.log）"
echo "  PASS 跨 PoP 音频：viewer AUDIO>0"
ok=0
for _ in $(seq 1 60); do
  grep -q "control: display switch request -> display 1" /tmp/bfb-pub-a.log 2>/dev/null && ok=1 && break
  sleep 0.5
done
[ "$ok" = "1" ] || fail "场景1：PoP-A publisher 未收到跨 PoP 显示器切换（见 /tmp/bfb-pub-a.log）"
echo "  PASS 跨 PoP 显示器切换：publisher 收到 display 1 请求"

echo "== 桥延迟分布（LATENCY ≥30 样本，与直连基线对比）"
# 数据通道在桥死亡/负载下可能偶发中断后恢复：样本不足时最多等两轮（共 ~160s）。
for _attempt in 1 2; do
  for _ in $(seq 1 160); do
    [ "$(latency_count /tmp/bfb-view-b.log)" -ge 30 ] && break
    sleep 0.5
  done
  [ "$(latency_count /tmp/bfb-view-b.log)" -ge 30 ] && break
done
BRIDGE_STATS=$(latency_stats /tmp/bfb-view-b.log)
BRIDGE_P99=$(echo "$BRIDGE_STATS" | awk '{print $3}')
BRIDGE_N=$(latency_count /tmp/bfb-view-b.log)
echo "  桥路径：samples=${BRIDGE_N} p50/p90/p99=${BRIDGE_STATS}ms（直连基线 ${DIRECT_STATS}ms）"
[ "${BRIDGE_N:-0}" -ge 30 ] || fail "桥路径样本不足（N=${BRIDGE_N}，需 ≥30）"
[ "$BRIDGE_P99" != "NONE" ] || fail "桥路径无 LATENCY 样本（cursor 链路未通）"
THRESHOLD=$((DIRECT_P99 * 4 + 500))
[ "$BRIDGE_P99" -lt "$THRESHOLD" ] || fail "桥延迟 p99=${BRIDGE_P99}ms ≥ 阈值 ${THRESHOLD}ms（直连 ${DIRECT_P99}ms）"

if [ "$REMOTE" = "1" ]; then
  # 远程模式：场景 3/2 需本地信令控制，跳过；场景 4 可选（需 BRIDGE_KILL_CMD）。
  # REMOTE_LOOPBACK 未显式给 BRIDGE_KILL_CMD 时默认本地 pkill（#257）。
  if [ "${REMOTE_LOOPBACK:-}" = "1" ] && [ -z "$BRIDGE_KILL_CMD" ]; then
    # [a] 技巧：避免 pkill -f 匹配到自身 sh -c 命令行（Linux 上会自杀）。
    BRIDGE_KILL_CMD="pkill -f '[a]erodesk-bridge'"
  fi
  if [ -n "$BRIDGE_KILL_CMD" ]; then
    echo "== 场景 4（远程）：桥死亡自动恢复（BRIDGE_KILL_CMD）"
    BEFORE=$(grep -c "DECODED:" /tmp/bfb-view-b.log 2>/dev/null || echo 0)
    sh -c "$BRIDGE_KILL_CMD" || fail "场景4：BRIDGE_KILL_CMD 执行失败"
    wait_log /tmp/bfb-view-b.log "reconnecting" 240 || fail "场景4：viewer 未重连（见 /tmp/bfb-view-b.log）"
    if [ "${REMOTE_LOOPBACK:-}" = "1" ]; then
      # #257 review：loopback 有本地日志，先等桥重建（spawn≥2）再等解码——避免
      # 用 kill 前的旧统计行假 PASS，并证明「桥恢复」而非回退 Redirect。
      ok=0
      for _ in $(seq 1 240); do
        N=$(grep -c "bridge: spawned" /tmp/bfb-sig-b.log 2>/dev/null || echo 0)
        [ "${N:-0}" -ge 2 ] && ok=1 && break
        sleep 0.5
      done
      [ "$ok" = "1" ] || fail "场景4(loopback)：桥未重建（见 /tmp/bfb-sig-b.log）"
      grep -q "signal redirect" /tmp/bfb-view-b.log && fail "场景4(loopback)：恢复不应走 Redirect 回退"
      grep -q "bridge died for room $ROOM" /tmp/bfb-sig-b.log || fail "场景4(loopback)：signal 未检测到桥死亡"
      grep -q "fail cooldown" /tmp/bfb-sig-b.log && fail "场景4(loopback)：恢复过程不应触发失败冷却"
    fi
    ok=0
    for _ in $(seq 1 240); do
      AFTER=$(grep -c "DECODED:" /tmp/bfb-view-b.log 2>/dev/null || echo 0)
      [ "${AFTER:-0}" -gt "${BEFORE:-0}" ] && ok=1 && break
      sleep 0.5
    done
    [ "$ok" = "1" ] || fail "场景4：viewer 重连后未恢复解码" 
    echo "  场景4 PASS（远程）：桥死亡 → viewer 自动重连并恢复解码"
  else
    echo "  （跳过场景 4：远程模式未设 BRIDGE_KILL_CMD）"
  fi
  kill "$VIEW_B" "$PUB_A" 2>/dev/null || true
  if [ "${REMOTE_LOOPBACK:-}" = "1" ]; then
    grep -qiE "panicked|fatal runtime error|overflowed its stack" /tmp/bfb-*.log && fail "发现 panic/abort"
  else
    grep -qiE "panicked|fatal runtime error|overflowed its stack" /tmp/bfb-view-b.log /tmp/bfb-pub-a.log && fail "发现 panic/abort"
  fi
  echo "== #216 M3 远程验收 PASS（直连 p50/p90/p99=${DIRECT_STATS}ms 桥=${BRIDGE_STATS}ms）=="
  exit 0
fi

echo "== 场景 3：桥死亡后新 viewer 加入自动重建桥（自然死亡不触发冷却）"
kill "$VIEW_B" 2>/dev/null || true
sleep 1
pkill -f 'aerodesk-bridge' 2>/dev/null || true
sleep 2
# --reconnect：场景 4 需要连接中 viewer 在 kick 后自动重连。
"$TARGET_DIR/aerodesk-cli" --role viewer --signal "$SIG_B_URL" --room "$ROOM" --token "$AUTH" --reconnect \
  >/tmp/bfb-view-b3.log 2>&1 &
VIEW_B=$!
wait_decoded /tmp/bfb-view-b3.log || fail "场景3：桥死亡后 viewer 未恢复解码（见 /tmp/bfb-view-b3.log）"
grep -q "signal redirect" /tmp/bfb-view-b3.log && fail "场景3：重建桥不应触发 Redirect"
SPAWNS=$(grep -c "bridge: spawned" /tmp/bfb-sig-b.log 2>/dev/null || echo 0)
[ "$SPAWNS" -ge 2 ] || fail "场景3：桥未重建（spawn 次数=${SPAWNS}，应 ≥2）"
grep -q "bridge: room $ROOM ready" /tmp/bfb-sig-b.log || fail "场景3：新桥未就绪"
grep -q "fail cooldown" /tmp/bfb-sig-b.log && fail "场景3：自然死亡不应触发失败冷却"
echo "  场景3 PASS：桥死亡后新 viewer 自动重建桥（spawn=${SPAWNS}）并恢复解码"

echo "== 场景 4：连接中 viewer 自动恢复（桥死亡 → SFU room-kick → --reconnect → 重建桥）"
BEFORE=$(grep -c "DECODED:" /tmp/bfb-view-b3.log 2>/dev/null || echo 0)
pkill -f 'aerodesk-bridge' 2>/dev/null || true
# 1) 信令 2s 轮询检测死亡并执行 room-kick（必须先于客户端 8s watchdog）。
wait_log /tmp/bfb-sig-b.log "kicking SFU room" 60 || fail "场景4：signal 未执行 SFU room-kick（见 /tmp/bfb-sig-b.log）"
# 2) SFU 侧确认 kick 投递到房间会话（媒体停止 → viewer 8s watchdog/ICE 断开 → 重连）。
grep -q "kick client" /tmp/bfb-sfu-b.log || fail "场景4：SFU 未执行 room-kick（见 /tmp/bfb-sfu-b.log）"
wait_log /tmp/bfb-view-b3.log "reconnecting" 120 || fail "场景4：viewer 未重连（见 /tmp/bfb-view-b3.log）"
# 3) 信令重建桥（spawn≥3）。
ok=0
for _ in $(seq 1 120); do
  N=$(grep -c "bridge: spawned" /tmp/bfb-sig-b.log 2>/dev/null || echo 0)
  [ "${N:-0}" -ge 3 ] && ok=1 && break
  sleep 0.5
done
[ "$ok" = "1" ] || fail "场景4：恢复后桥未重建（见 /tmp/bfb-sig-b.log）"
# 4) viewer 重连后出现新的解码统计行（会话计数重置）。
ok=0
for _ in $(seq 1 120); do
  AFTER=$(grep -c "DECODED:" /tmp/bfb-view-b3.log 2>/dev/null || echo 0)
  [ "${AFTER:-0}" -gt "${BEFORE:-0}" ] && ok=1 && break
  sleep 0.5
done
[ "$ok" = "1" ] || fail "场景4：viewer 重连后未恢复解码"
grep -q "bridge died for room $ROOM" /tmp/bfb-sig-b.log || fail "场景4：signal 未检测到桥死亡"
SPAWNS4=$(grep -c "bridge: spawned" /tmp/bfb-sig-b.log 2>/dev/null || echo 0)
grep -q "fail cooldown" /tmp/bfb-sig-b.log && fail "场景4：恢复过程不应触发失败冷却"
echo "  场景4 PASS：桥死亡 → kick → viewer 自动重连 → 重建桥（spawn=${SPAWNS4}）恢复解码"
kill "$VIEW_B" "$PUB_A" 2>/dev/null || true
sleep 1
pkill -f 'aerodesk-bridge' 2>/dev/null || true

echo "== 场景 2：桥失败回退 v1 Redirect"
kill "$VIEW_B" 2>/dev/null || true
sleep 1
pkill -f 'aerodesk-bridge' 2>/dev/null || true
kill "$SIG_B_PID" 2>/dev/null || true; wait "$SIG_B_PID" 2>/dev/null || true
sleep 1
# 重启 PoP-B 信令：BRIDGE_CMD 必失败（false）→ 桥失败 → 回退 Redirect。
POP_ID=pop-b AUTH_TOKENS="$AUTH" ROOM_POP_MAP="bridge-=pop-a" POP_URLS="pop-a=${SIG_A_URL}" \
  BRIDGE_CMD="false" BRIDGE_READY_TIMEOUT_SECS=5 BRIDGE_FAIL_COOLDOWN_SECS=5 \
  SIGNAL_PORT=14901 SIGNAL_PLAIN_PORT="$PLAIN_B" SFU_URL="http://127.0.0.1:${INT_B}" \
  "$TARGET_DIR/aerodesk-signal" >/tmp/bfb-sig-b2.log 2>&1 &
SIG_B_PID=$!
for _ in $(seq 1 50); do nc -z 127.0.0.1 "$PLAIN_B" 2>/dev/null && break; sleep 0.2; done
# PoP-A publisher 仍在 room（重新起）
"$TARGET_DIR/aerodesk-cli" --role publisher --signal "$SIG_A_URL" --room "$ROOM" --token "$AUTH" \
  --encoder vt --width 1280 --height 720 --fps 30 --bitrate 2000000 --noisy \
  >/tmp/bfb-pub-a2.log 2>&1 &
PUB_A=$!
ok=0
for _ in $(seq 1 120); do grep -q "ICE connected" /tmp/bfb-pub-a2.log 2>/dev/null && ok=1 && break; sleep 0.5; done
[ "$ok" = "1" ] || fail "场景2：PoP-A publisher 未连上"

"$TARGET_DIR/aerodesk-cli" --role viewer --signal "$SIG_B_URL" --room "$ROOM" --token "$AUTH" \
  >/tmp/bfb-view-b2.log 2>&1 &
VIEW_B=$!
wait_log /tmp/bfb-view-b2.log "signal redirect" 240 || fail "场景2：viewer 未收到 Redirect（桥失败应回退 v1）"
wait_decoded /tmp/bfb-view-b2.log || fail "场景2：viewer 跟随 Redirect 到 pop-a 后未解码"
grep -q "fallback redirect" /tmp/bfb-sig-b2.log || fail "场景2：PoP-B 信令未记录 fallback redirect"
echo "  场景2 PASS：桥失败 → v1 Redirect → viewer 自动跟随到 pop-a 解码"

grep -qiE "panicked|fatal runtime error|overflowed its stack" /tmp/bfb-*.log && fail "发现 panic/abort"

kill "$VIEW_B" "$PUB_A" 2>/dev/null || true
echo "== #216 M3 桥接编排 e2e PASS（直连 p50/p90/p99=${DIRECT_STATS}ms 桥=${BRIDGE_STATS}ms；桥优先+重建+恢复+回退）=="
