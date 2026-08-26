#!/usr/bin/env bash
# nat-e2e.sh —— NAT srflx/relay 公网路径实测（#582，P1 交接项自动化版）。
#
# 场景（对照 docs/NAT_ACCEPTANCE.md §3/§6.1）：
#   S0  直连基线（netns 双私网路由可达 / host 本机）：ICE 直连 + 媒体到达 + TURN 闲置
#   S2a 双 NAT 打洞失败（无 TURN）：ICE 超时干净失败、无假通、无媒体
#   S2b 双 NAT 打洞失败（有 TURN）：直连失败 → TURN 兜底 → 媒体恢复
#   S3  relay 强制（AERODESK_FORCE_RELAY=1）：只通告 relayed、媒体经 TURN
#   S4  回退黑屏时长上限：直连会话中切断直连 → 恢复间隙 ≤ NAT_BLACKSCREEN_BOUND
#
# 模式（NAT_MODE=auto|netns|host|skip，默认 auto 自动探测）：
#   netns  Linux root：网络命名空间 natA(10.200.0.0/24)/natB(10.201.0.0/24) +
#          iptables FORWARD 阻断模拟双端 NAT（两私网互不可达，直连打洞失败；
#          撤阻断近似"打洞成功"直连）。本机无公网 NAT 时的替代验证面。
#   host   本机直连冒烟（S0+S3；无 NAT 语义，验证工具链/脚本自洽）。
#   skip   打印 SKIP 说明 + 公网实测步骤指引（docs/NAT_ACCEPTANCE.md §4/§5）。
#
# 用法：
#   sudo NAT_MODE=netns ./scripts/nat-e2e.sh     # Linux root 全量断言
#   NAT_MODE=host ./scripts/nat-e2e.sh           # 本机冒烟（需已构建三件套）
#   NAT_MODE=skip ./scripts/nat-e2e.sh           # 强制 SKIP 模式
#
# 环境变量：
#   NAT_PUBLIC_IP             netns 模式通告/中继地址（默认宿主主 IP，回退 10.200.0.1）
#   NAT_BLACKSCREEN_BOUND     回退黑屏时长上限 ms（默认 15000 = TURN ICE 15s 超时上限）
#   NAT_DEBUG=1               RUST_LOG=debug（输出媒体收包源地址/路径锁定证据行）
#   NAT_SKIP_BUILD=1          跳过 cargo build，直接使用已构建产物（快速迭代/二进制被占用时）
#   AERO_TURN_URLS/USERNAME/CREDENTIAL 覆盖（默认自动按 TURN_SECRET 生成）
#   RUST_LOG / CARGO_TARGET_DIR 透传
#
# 无 NAT 环境（本机/CI）行为：NAT_MODE=auto 探测失败 → host 模式跑 S0+S3；
# 连 host 模式也不可用（未构建）→ 打印 SKIP 说明与公网实测步骤。

set -uo pipefail
cd "$(dirname "$0")/.."
# 全量 info（SFU/signal/agent 三个 crate 都要有日志供就绪/证据断言；
# 需媒体源地址/路径锁定证据时 RUST_LOG=debug 或 NAT_DEBUG 场景单独覆盖）。
export RUST_LOG="${RUST_LOG:-info}"

# ---- 配置 ----
MODE="${NAT_MODE:-auto}"
BOUND_MS="${NAT_BLACKSCREEN_BOUND:-15000}"
TURN_SECRET="${TURN_SECRET:-nat-e2e-secret}"
# 独立端口段（167xx）避免与本机其它 e2e（15xxx/16xxx）与 CI runner 冲突。
SIP_UDP=16703      # signal SIP UDP 监听 + 客户端 AERO_SIP_PORT
SIG_WSS=16701      # signal WSS（未用，占位防默认端口冲突）
SIG_PLAIN=16704    # signal 明文 WS（未用）
SFU_MEDIA=16778
SFU_SIG=16700      # SFU 信令端口（内部连通）
SFU_INT=16702      # SFU 内部端口（/metrics/prometheus）
TURN_PORT=16779
TURN_TLS_PORT=16734

LOG_DIR="$(mktemp -d /tmp/nat-e2e.XXXXXX 2>/dev/null || echo /tmp/nat-e2e)"
echo "== nat-e2e（#582）日志目录：$LOG_DIR"
TARGET_DIR="${CARGO_TARGET_DIR:-$PWD/target}/debug"

PASS=0; FAILS=0
ok()   { echo "PASS $*"; PASS=$((PASS+1)); }
fail() { echo "FAIL $*"; FAILS=$((FAILS+1)); }

wait_log() { # <file> <pattern> <timeout_s>
  local f=$1 pat=$2 t=$3
  for _ in $(seq $((t*5))); do
    grep -q "$pat" "$f" 2>/dev/null && return 0
    sleep 0.2
  done
  return 1
}

# 提取 RECEIVED 行时间戳（tracing 默认 ISO8601 前缀）→ epoch ms；失败回 0。
recv_ts() { # <file> <last|first> [行号下限]
  local f=$1 pos=$2 min=$3
  local line
  if [ "$pos" = last ]; then
    line=$(grep -E 'RECEIVED:' "$f" | tail -1)
  else
    line=$(grep -E 'RECEIVED:' "$f" | awk -v m="$min" 'NR>m{print; exit}')
  fi
  [ -n "$line" ] || return 1
  local ts
  ts=$(printf '%s' "$line" | sed -E 's/^([0-9]{4}-[0-9T:.+-]+Z?).*/\1/' | tr -d ' ')
  date -d "$ts" +%s%3N 2>/dev/null || echo 0
}
recv_count() { grep -cE 'RECEIVED:' "$1" 2>/dev/null || echo 0; }

# ---- 工具链探测 ----
have_netns() {
  [ "$(id -u)" = 0 ] || return 1
  command -v ip >/dev/null || return 1
  command -v iptables >/dev/null || return 1
  ip netns add "__nat_e2e_probe_$$" 2>/dev/null || return 1
  ip netns del "__nat_e2e_probe_$$" 2>/dev/null || true
  return 0
}

if [ "$MODE" = "auto" ]; then
  if have_netns; then MODE=netns; else MODE=host; fi
fi
echo "== 模式：$MODE"
# 双端 agent 共用的固定环境：SIP UDP 端口（signal 的 SIP_UDP_PORT 与之一致）与日志级别。
export AERO_SIP_PORT=$SIP_UDP RUST_LOG="$RUST_LOG"

# Windows（Git Bash/MSYS）：x264 编码器不可用（agent 编译门控），host 冒烟用
# --encoder screen（DXGI 采集）。netns 模式仅 Linux。
case "$(uname -s 2>/dev/null)" in
  MINGW*|MSYS*) IS_WINDOWS=1 ;;
  *) IS_WINDOWS=0 ;;
esac
if [ "$MODE" = "host" ]; then
  PUBLIC_IP="127.0.0.1"
else
  PUBLIC_IP="${NAT_PUBLIC_IP:-}"
  if [ -z "$PUBLIC_IP" ]; then
    PUBLIC_IP=$(ip -4 -o addr show scope global 2>/dev/null | awk '$2 !~ /^(veth|docker|br-|lo)/ {split($4,a,"/"); print a[1]; exit}')
  fi
  [ -n "$PUBLIC_IP" ] || PUBLIC_IP="10.200.0.1"
fi

if [ "$MODE" = "skip" ]; then
  echo "SKIP: 本机无 netns 能力（非 Linux/无 root）且未要求 host 模式。"
  echo "      公网实测步骤见 docs/NAT_ACCEPTANCE.md §4/§5："
  echo "        1) 公网 VPS 部署 aerodesk-sfu + aerodesk-signal（SFU_HOST_ADDRESS=公网 IP,"
  echo "           TURN_SECRET, 开放 5060/3061/3478/3479/5349/49152-49200）"
  echo "        2) 双客户端设 AERO_TURN_URLS/USERNAME/CREDENTIAL（REST 凭证生成见文档 §4.2）"
  echo "        3) S1 Cone NAT 直连：RUST_LOG=debug 采集 'recv ... from <公网IP:端口>'"
  echo "        4) S3 relay：AERODESK_FORCE_RELAY=1 + AERO_TURN_*，SFU /metrics 看 allocation"
  echo "        5) S4 黑屏时长：会话中 iptables 断直连，量 RECEIVED 时间戳间隙（文档 §7）"
  exit 0
fi

# ---- 构建 ----
if [ "${NAT_SKIP_BUILD:-0}" = "1" ]; then
  echo "== 跳过构建（NAT_SKIP_BUILD=1，直接使用 $TARGET_DIR 产物）"
  [ -x "$TARGET_DIR/aerodesk-agent" ] || { echo "FAIL: $TARGET_DIR/aerodesk-agent 不存在"; exit 1; }
else
  echo "== 构建（aerodesk-agent/sfu/signal）"
  if ! cargo build -q -p aerodesk-agent -p aerodesk-sfu -p aerodesk-signal 2>/tmp/nat-e2e-build.log; then
    echo "FAIL: 构建失败（详见 /tmp/nat-e2e-build.log）——NAT_MODE=host/skip 模式需先构建"
    exit 1
  fi
fi
BIN="$TARGET_DIR/aerodesk-agent"

# ---- 服务器 ----
SFU_PID=""; SIG_PID=""

start_servers() {
  local turn_urls="turn:$PUBLIC_IP:$TURN_PORT?transport=udp,turn:$PUBLIC_IP:$TURN_PORT?transport=tcp"
  # Windows（Git Bash）：必须直接后台启动——`env VAR=x cmd &` 或 `(export; exec cmd) &`
  # 的 $! 都不是真进程 PID（msys exec 走 CreateProcess，PID 会变），kill/taskkill
  # 打不到 → 进程残留。环境用 export 传递（子进程继承），agent 侧同名 AERO_* 变量
  # 在 launch_agent 内 unset 隔离，互不污染。
  export RECORD_DIR="$LOG_DIR/rec" TURN_SECRET="$TURN_SECRET" \
    SFU_HOST_ADDRESS="$PUBLIC_IP" \
    SFU_MEDIA_PORT=$SFU_MEDIA SFU_SIGNAL_PORT=$SFU_SIG SFU_INTERNAL_PORT=$SFU_INT \
    SFU_TURN_PORT=$TURN_PORT SFU_TURN_TLS_PORT=$TURN_TLS_PORT
  "$TARGET_DIR/aerodesk-sfu" >"$LOG_DIR/sfu.log" 2>&1 &
  SFU_PID=$!
  export SIP_UDP_PORT=$SIP_UDP SIGNAL_PORT=$SIG_WSS SIGNAL_PLAIN_PORT=$SIG_PLAIN \
    TURN_SECRET="$TURN_SECRET" TURN_URLS="$turn_urls"
  "$TARGET_DIR/aerodesk-signal" >"$LOG_DIR/signal.log" 2>&1 &
  SIG_PID=$!
  wait_log "$LOG_DIR/sfu.log" 'embedded TURN+STUN server UDP on' 30 \
    || { echo "FAIL: SFU 内嵌 TURN 未启动"; tail -5 "$LOG_DIR/sfu.log"; exit 1; }
  wait_log "$LOG_DIR/sfu.log" 'Bound UDP media port' 30 \
    || { echo "FAIL: SFU 媒体端口未启动"; tail -5 "$LOG_DIR/sfu.log"; exit 1; }
  wait_log "$LOG_DIR/signal.log" 'SIP 信令端点已启动' 30 \
    || { echo "FAIL: signal SIP 端点未启动"; tail -5 "$LOG_DIR/signal.log"; exit 1; }
  echo "PASS 服务器就绪（SFU+TURN :$TURN_PORT, signal SIP/UDP :$SIP_UDP）"
}

stop_servers() {
  kill_proc "${SFU_PID:-}"
  kill_proc "${SIG_PID:-}"
  [ -n "$SFU_PID" ] && wait "$SFU_PID" 2>/dev/null || true
  [ -n "$SIG_PID" ] && wait "$SIG_PID" 2>/dev/null || true
}

turn_alloc() { # 活跃 allocation 数（SFU 内部端口 /metrics/prometheus）
  local v
  v=$(curl -s --max-time 2 "http://127.0.0.1:$SFU_INT/metrics/prometheus" 2>/dev/null \
      | awk '/^aerodesk_sfu_turn_allocations [0-9]+$/{print $2; exit}')
  echo "${v:-0}"
}

# ---- netns 双 NAT 拓扑 ----
NS_A="natA-$$"; NS_B="natB-$$"
VETH_A="vethA-$$"; VETH_B="vethB-$$"
# 宿主 FORWARD 规则管理：block_direct 阻双私网互通（=双 NAT 打洞失败）；
# allow_direct 撤阻断（=私网可路由直连）。
NET_A="10.200.0"; NET_B="10.201.0"
IPS_SAVED=""

setup_netns() {
  local ip_forward
  ip_forward=$(sysctl -n net.ipv4.ip_forward 2>/dev/null || echo 0)
  IPS_SAVED="$ip_forward"
  sysctl -w net.ipv4.ip_forward=1 >/dev/null 2>&1 || true
  local failed=0
  ip netns add "$NS_A" 2>/dev/null || failed=1
  ip netns add "$NS_B" 2>/dev/null || failed=1
  ip link add "$VETH_A" type veth peer name "${VETH_A}p" 2>/dev/null || failed=1
  ip link add "$VETH_B" type veth peer name "${VETH_B}p" 2>/dev/null || failed=1
  if [ "$failed" -ne 0 ]; then
    echo "SKIP: netns/veth 创建失败（需 root + iproute2）——改用 NAT_MODE=host"
    return 1
  fi
  ip link set "${VETH_A}p" netns "$NS_A"
  ip link set "${VETH_B}p" netns "$NS_B"
  # natA：被控端侧私网
  ip netns exec "$NS_A" ip addr add "$NET_A.2/24" dev "${VETH_A}p"
  ip netns exec "$NS_A" ip link set "${VETH_A}p" up
  ip netns exec "$NS_A" ip link set lo up
  ip netns exec "$NS_A" ip route add default via "$NET_A.1"
  ip addr add "$NET_A.1/24" dev "$VETH_A"
  ip link set "$VETH_A" up
  # natB：观看端侧私网
  ip netns exec "$NS_B" ip addr add "$NET_B.2/24" dev "${VETH_B}p"
  ip netns exec "$NS_B" ip link set "${VETH_B}p" up
  ip netns exec "$NS_B" ip link set lo up
  ip netns exec "$NS_B" ip route add default via "$NET_B.1"
  ip addr add "$NET_B.1/24" dev "$VETH_B"
  ip link set "$VETH_B" up
  echo "PASS netns 就绪：$NS_A($NET_A.2) / $NS_B($NET_B.2)，宿主路由 10.200.0.1/10.201.0.1"
}

block_direct() {
  iptables -I FORWARD 1 -s "$NET_A.0/24" -d "$NET_B.0/24" -j DROP 2>/dev/null || true
  iptables -I FORWARD 1 -s "$NET_B.0/24" -d "$NET_A.0/24" -j DROP 2>/dev/null || true
}
allow_direct() {
  iptables -D FORWARD -s "$NET_A.0/24" -d "$NET_B.0/24" -j DROP 2>/dev/null || true
  iptables -D FORWARD -s "$NET_B.0/24" -d "$NET_A.0/24" -j DROP 2>/dev/null || true
  iptables -D FORWARD -s "$NET_A.0/24" -d "$NET_B.0/24" -j ACCEPT 2>/dev/null || true
  iptables -D FORWARD -s "$NET_B.0/24" -d "$NET_A.0/24" -j ACCEPT 2>/dev/null || true
  iptables -I FORWARD 1 -s "$NET_A.0/24" -d "$NET_B.0/24" -j ACCEPT 2>/dev/null || true
  iptables -I FORWARD 1 -s "$NET_B.0/24" -d "$NET_A.0/24" -j ACCEPT 2>/dev/null || true
}
cleanup_nat() {
  allow_direct
  [ -n "$IPS_SAVED" ] && sysctl -w net.ipv4.ip_forward="$IPS_SAVED" >/dev/null 2>&1 || true
  ip link del "$VETH_A" 2>/dev/null || true
  ip link del "$VETH_B" 2>/dev/null || true
  ip netns del "$NS_A" 2>/dev/null || true
  ip netns del "$NS_B" 2>/dev/null || true
}

# 启动一个 agent 并输出真进程 PID。
# Windows/Git Bash 下 `env VAR=x cmd &` / `(export; exec cmd) &` 的 $! 都不是
# 真进程 PID（msys exec 是 CreateProcess，PID 会变）→ 直接后台启动 +
# export 传环境、launch 前后 unset AERO_* 隔离场景配置。
# netns 下 `ip netns exec` 是 fork 包装（ip 为父、agent 为子），PID 追踪失效，
# 由 kill_pair 按房间名 pkill 兜底清理。
launch_agent() { # <assigns> <role> <signal_url> <room> <extra_args> <logfile> [ns]
  local assigns=$1 role=$2 sig=$3 room=$4 extra=$5 log=$6 ns=${7:-} pid
  unset AERO_TURN_URLS AERO_TURN_USERNAME AERO_TURN_CREDENTIAL AERODESK_FORCE_RELAY 2>/dev/null || true
  # shellcheck disable=SC2086
  [ -n "$assigns" ] && export $assigns
  if [ -n "$ns" ]; then
    ip netns exec "$ns" "$BIN" --role "$role" --signal "$sig" --room "$room" $extra >"$log" 2>&1 &
  else
    "$BIN" --role "$role" --signal "$sig" --room "$room" $extra >"$log" 2>&1 &
  fi
  pid=$!
  unset AERO_TURN_URLS AERO_TURN_USERNAME AERO_TURN_CREDENTIAL AERODESK_FORCE_RELAY 2>/dev/null || true
  echo "$pid"
}

# ---- 客户端对 ----
# run_pair <view_env> <view_args> [<pub_env>] <scenario_tag>
# 返回后 PUB_PID/VIEW_PID 指向进程；pub 在 natA（或本机）、view 在 natB（或本机）。
# publisher 编码器：Windows 无 x264（agent 编译门控）→ screen（DXGI）；其余 x264+合成源。
run_pair() {
  local view_env="$1" view_args="$2" pub_env="${3:-}" tag="${4:-}"
  local room="nat-e2e-$$-$tag-$(date +%s)"
  ROOM="$room"
  local signal_a signal_b pub_enc
  if [ "$MODE" = "netns" ]; then
    signal_a="ws://$NET_A.1:$SIP_UDP/ws"
    signal_b="ws://$NET_B.1:$SIP_UDP/ws"
  else
    signal_a="ws://127.0.0.1:$SIP_UDP/ws"
    signal_b="ws://127.0.0.1:$SIP_UDP/ws"
  fi
  if [ "$IS_WINDOWS" = 1 ]; then
    pub_enc="--encoder screen"
  else
    pub_enc="--encoder x264 --noisy"
  fi
  PUB_LOG="$LOG_DIR/pub-$tag.log"; VIEW_LOG="$LOG_DIR/view-$tag.log"
  # 先起被控端并等注册完成，再起观看端——否则 viewer 的 INVITE 可能抢跑在
  # publisher REGISTER 之前，signal 会把它当会议 INVITE 转 SFU 桥（无 SFU_URL → 503）。
  if [ "$MODE" = "netns" ]; then
    PUB_PID=$(launch_agent "$pub_env" publisher "$signal_a" "$room" "$pub_enc --reconnect --reconnect-max 10" "$PUB_LOG" "$NS_A")
    wait_log "$PUB_LOG" 'SIP registered' 30 || fail "publisher SIP 注册超时（$tag）"
    VIEW_PID=$(launch_agent "$view_env" viewer "$signal_b" "$room" "$view_args" "$VIEW_LOG" "$NS_B")
  else
    PUB_PID=$(launch_agent "$pub_env" publisher "$signal_a" "$room" "$pub_enc --reconnect --reconnect-max 10" "$PUB_LOG")
    wait_log "$PUB_LOG" 'SIP registered' 30 || fail "publisher SIP 注册超时（$tag）"
    VIEW_PID=$(launch_agent "$view_env" viewer "$signal_b" "$room" "$view_args" "$VIEW_LOG")
  fi
  wait_log "$VIEW_LOG" 'SIP registered' 30 || fail "viewer SIP 注册超时（$tag）"
}

# 杀进程：SIGTERM 后等退出（最多 1s），未退 SIGKILL 兜底。Windows（Git Bash/msys）
# 下 kill 走 msys 运行时，对原生进程可靠；taskkill 作为双保险。
kill_proc() { # <pid>
  local p=$1
  [ -n "$p" ] || return 0
  kill "$p" 2>/dev/null || true
  if [ "$IS_WINDOWS" = 1 ]; then
    taskkill //F //PID "$p" >/dev/null 2>&1 || true
  else
    for _ in 1 2 3 4 5; do kill -0 "$p" 2>/dev/null || break; sleep 0.2; done
    kill -9 "$p" 2>/dev/null || true
  fi
}

kill_pair() {
  kill_proc "${VIEW_PID:-}"
  kill_proc "${PUB_PID:-}"
  # netns 下 `ip netns exec` 是 fork 包装（ip 为父、agent 为子）：仅杀父会留孤儿，
  # 按本场景唯一房间名兜底清理（模式仅 Linux）。
  if [ "$MODE" = "netns" ] && [ -n "${ROOM:-}" ]; then
    pkill -f "aerodesk-agent --role .* --room $ROOM" 2>/dev/null || true
  fi
  [ -n "${VIEW_PID:-}" ] && wait "$VIEW_PID" 2>/dev/null || true
  [ -n "${PUB_PID:-}" ] && wait "$PUB_PID" 2>/dev/null || true
  VIEW_PID=""; PUB_PID=""
}

# TURN 凭证（coturn REST 规范，与 TURN_SECRET 一致；SIP 无 join 下发一环 → 本地配置 #570）。
# 输出为未加引号的 `VAR=value` 空格串，供 launch_agent 的 export 展开（值不含空白字符）。
# 用户覆盖（AERO_TURN_* 环境变量）在启动期快照，因为 launch_agent 会 unset 这些
# 环境变量来隔离各场景配置。
TURN_URLS_OVERRIDE="${AERO_TURN_URLS:-}"
TURN_USER_OVERRIDE="${AERO_TURN_USERNAME:-}"
TURN_CRED_OVERRIDE="${AERO_TURN_CREDENTIAL:-}"
turn_env() {
  local u c
  u="$(($(date +%s) + 3600)):nat-e2e"
  c="$(python3 -c "import hmac,hashlib,base64; print(base64.b64encode(hmac.new(b'$TURN_SECRET', b'$u', hashlib.sha1).digest()).decode())" 2>/dev/null)"
  if [ -z "$TURN_URLS_OVERRIDE" ]; then
    printf 'AERO_TURN_URLS=turn:%s:%s?transport=udp,turn:%s:%s?transport=tcp AERO_TURN_USERNAME=%s AERO_TURN_CREDENTIAL=%s' \
      "$PUBLIC_IP" "$TURN_PORT" "$PUBLIC_IP" "$TURN_PORT" "$u" "$c"
  else
    printf 'AERO_TURN_URLS=%s AERO_TURN_USERNAME=%s AERO_TURN_CREDENTIAL=%s' \
      "$TURN_URLS_OVERRIDE" "$TURN_USER_OVERRIDE" "$TURN_CRED_OVERRIDE"
  fi
}

# ---- 场景 ----
scenario_s0() { # 直连基线：ICE 直连 + 媒体 + TURN 闲置（A1）
  echo "== S0 直连基线（$MODE）"
  allow_direct
  run_pair "" "" "" s0
  local al
  if wait_log "$VIEW_LOG" 'ICE connected' 25 && wait_log "$VIEW_LOG" 'RECEIVED: [1-9]' 25; then
    al=$(turn_alloc)
    if [ "$al" -eq 0 ]; then
      ok "S0 直连媒体到达 + TURN 闲置（allocations=$al）——媒体不经服务器"
    else
      fail "S0 直连成立但 TURN allocation=$al ≠ 0（不应有 relay）"
    fi
  else
    fail "S0 ICE/媒体未到达"; tail -6 "$VIEW_LOG"
  fi
  kill_pair
}

scenario_s2a() { # 双 NAT 打洞失败（无 TURN）：ICE 超时干净失败（A2）
  echo "== S2a 双 NAT 打洞失败（无 TURN，直连应干净失败）"
  block_direct
  # 无 --reconnect：首个 ICE 失败即退出，便于断言
  run_pair "" "" "" s2a
  if wait_log "$VIEW_LOG" 'ICE 连接超时' 25; then
    if grep -qE 'RECEIVED:|ICE connected' "$VIEW_LOG"; then
      fail "S2a 出现假通（不应有 RECEIVED/ICE connected）"
    else
      ok "S2a 无 TURN 时双 NAT 直连干净失败（ICE 连接超时，无假通）"
    fi
  else
    fail "S2a 预期 ICE 超时未出现"; tail -6 "$VIEW_LOG"
  fi
  kill_pair
  allow_direct
}

scenario_s2b() { # 双 NAT + TURN：直连失败 → TURN 兜底 → 媒体恢复（A3）
  echo "== S2b 双 NAT + TURN 兜底"
  block_direct
  local tenv
  tenv=$(turn_env)
  # 双端都要 TURN：直连被阻断后，任一端的 relayed 候选缺失都会让 ICE 无可用对
  run_pair "$tenv" "--reconnect --reconnect-max 10" "$tenv" s2b
  if wait_log "$VIEW_LOG" 'ICE connected' 30 && wait_log "$VIEW_LOG" 'RECEIVED: [1-9]' 30; then
    local al
    al=$(turn_alloc)
    if [ "$al" -ge 2 ]; then
      ok "S2b 打洞失败后 TURN 兜底成功（allocations=$al，媒体恢复）"
    else
      fail "S2b 媒体到达但 allocations=$al < 2（未走 relay）"
    fi
  else
    fail "S2b TURN 兜底未完成"; tail -6 "$VIEW_LOG"
  fi
  kill_pair
  allow_direct
}

scenario_s3() { # relay 强制路径（A4）
  echo "== S3 relay 强制（AERODESK_FORCE_RELAY=1）"
  block_direct
  local tenv
  tenv=$(turn_env)
  run_pair "AERODESK_FORCE_RELAY=1 $tenv" "--reconnect --reconnect-max 10" "$tenv" s3
  if grep -q 'force-relay: skip host candidate' "$VIEW_LOG" \
     && grep -q 'relayed candidate' "$VIEW_LOG" \
     && wait_log "$VIEW_LOG" 'RECEIVED: [1-9]' 30; then
    local al
    al=$(turn_alloc)
    if [ "$al" -ge 2 ]; then
      ok "S3 relay 强制生效（skip host + relayed 候选 + 媒体经 TURN，allocations=$al）"
    else
      fail "S3 媒体到达但 allocations=$al < 2"
    fi
  else
    fail "S3 force-relay 断言未满足"; tail -6 "$VIEW_LOG"
  fi
  kill_pair
  allow_direct
}

scenario_s4() { # 回退黑屏时长上限（A5）：直连会话中切断 → TURN 恢复
  echo "== S4 回退黑屏时长上限（切断直连 → 恢复间隙 ≤ ${BOUND_MS}ms）"
  allow_direct
  local tenv
  tenv=$(turn_env)
  run_pair "$tenv" "--reconnect --reconnect-max 10" "$tenv" s4
  if ! wait_log "$VIEW_LOG" 'RECEIVED: [1-9]' 30; then
    fail "S4 基线直连未建立"; tail -6 "$VIEW_LOG"
    kill_pair; return
  fi
  # 直连会话确认（含 TURN 配置下仍应直连优先）
  if [ "$(turn_alloc)" -ge 2 ]; then
    echo "  note: 基线已含 TURN allocation（直连锁定前双路）；S4 以会话级恢复计时"
  fi
  local t0 n0
  n0=$(recv_count "$VIEW_LOG")
  t0=$(recv_ts "$VIEW_LOG" last)
  echo "  T0=$t0 基线最后 RECEIVED（行 #$n0），2s 后切断直连"
  sleep 2
  block_direct
  local deadline=$((BOUND_MS + 15000)) waited=0
  local n1
  while [ "$waited" -lt $((deadline / 200)) ]; do
    n1=$(recv_count "$VIEW_LOG")
    if [ "$n1" -gt "$n0" ]; then break; fi
    sleep 0.2; waited=$((waited + 1))
  done
  allow_direct
  local t1 gap
  if [ "$(recv_count "$VIEW_LOG")" -gt "$n0" ]; then
    t1=$(recv_ts "$VIEW_LOG" first "$n0")
    gap=$((t1 - t0))
    if [ "$gap" -le "$BOUND_MS" ] && [ "$gap" -gt 0 ]; then
      ok "S4 回退黑屏时长上界=${gap}ms ≤ ${BOUND_MS}ms（RECEIVED 时间戳间隙法）"
    else
      fail "S4 回退间隙=${gap}ms > ${BOUND_MS}ms"
    fi
  else
    fail "S4 切断后未恢复（${deadline}ms 内无新 RECEIVED）"; tail -6 "$VIEW_LOG"
  fi
  kill_pair
}

# ---- 主流程 ----
cleanup() {
  kill_pair 2>/dev/null || true
  stop_servers
  if [ "$MODE" = "netns" ]; then cleanup_nat; fi
  echo "== 汇总：PASS=$PASS FAIL=$FAILS；日志：$LOG_DIR"
}
trap cleanup EXIT

start_servers

if [ "$MODE" = "netns" ]; then
  if ! setup_netns; then
    echo "SKIP: netns 不可用——公网实测步骤见 docs/NAT_ACCEPTANCE.md §4/§5"
    exit 0
  fi
fi

scenario_s0
if [ "$MODE" = "netns" ]; then
  scenario_s2a
  scenario_s2b
  scenario_s3
  scenario_s4
else
  # host 模式：无 NAT 语义，只跑 S0 + S3（relay 路径仍可验证）
  scenario_s3
  echo "== host 模式完成：S0 直连 + S3 relay 冒烟。双 NAT 断言需 NAT_MODE=netns（Linux root）"
  echo "   或公网实测（docs/NAT_ACCEPTANCE.md §4/§5，S1 需真 NAT 采媒体源=公网映射地址）"
fi

[ "$FAILS" -eq 0 ] && echo "== NAT E2E PASS（#582）" || echo "== NAT E2E FAIL（$FAILS 项）"
exit $((FAILS > 0 ? 1 : 0))
