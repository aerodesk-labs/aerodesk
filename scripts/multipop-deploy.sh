#!/usr/bin/env bash
# multipop-deploy.sh —— 真实双 PoP 一键部署 + 远程模式验收（#216 M8 / #264）。
#
# 把「构建 → 拷贝 → 装 systemd → 配 env → 启动 → 远程验收」收敛为一条命令。
#
# 前提：
#   1) 本机可免密 ssh 到两台主机且有 sudo（systemd 安装）；
#   2) 本机可编译（默认 release）；本机有 nc（健康探测）；
#   3) 端口：SFU 媒体 3478/公共 HTTPS 3000/内部 3002；signal WSS 3001/明文 3003；
#      TURN UDP+TCP 3479/TLS 5349（可经 --*-port 覆盖）；
#   4) 认证：静态 token（AUTH_TOKENS）——本脚本不签发 JWT（--jwt-secret 为可选
#      附加项；验收链路使用静态 token，见注意 1）；
#   5) 客户端默认用明文 ws://<host>:3003/ws 连信令（当前 tungstenite 构建无 TLS
#      feature，不支持 wss://）；生产请在前端放 TLS 反代或启用 wss 构建（见注意 2）；
#   6) --cert-file/--key-file 指向主机上已存在的证书路径（脚本不负责上传）。
#
# 注意 1：--jwt-secret 一旦设置，信令 auth_result 只认 JWT、不再回退 AUTH_TOKENS，
#         验收（静态 token）会全部 auth failed——生产用 JWT 请手动签发，勿与本
#         脚本验收混用。
# 注意 2：默认 SIGNAL_A_URL/B 为 ws://<host>:3003/ws；生产 wss 需 tungstenite TLS
#         feature + 证书，或反代 TLS 到 3003。
#
# 用法：
#   scripts/multipop-deploy.sh --pop-a root@pop-a.example.com --pop-b root@pop-b.example.com \
#     --auth <信令token> --room-prefix bridge- [--dry-run] [--deploy-only] [--cleanup] [--help]
#   [--sfu-host-address-a <IP> --sfu-host-address-b <IP>]  # SFU 对外通告地址覆盖（NAT/docker0；默认取 HOST 点分 IPv4）
set -uo pipefail
cd "$(dirname "$0")/.."

# ---- 参数 ----
POP_A=""; POP_B=""
AUTH=""; ROOM_PREFIX="bridge-"
SIGNAL_A_URL=""; SIGNAL_B_URL=""
INSTALL_DIR="/opt/aerodesk"; RECORD_DIR="/var/lib/aerodesk/records"
SFU_PORT=3478; SFU_HTTP_PORT=3000; SFU_INT_PORT=3002
SIG_WSS_PORT=3001; SIG_PLAIN_PORT=3003; TURN_PORT=3479; TURN_TLS_PORT=5349
PROFILE="release"; TARGET_DIR="$PWD/target/$PROFILE"
DRY_RUN=0; DEPLOY_ONLY=0; CLEANUP=0; SKIP_BUILD=0
JWT_SECRET=""; CERT_FILE=""; KEY_FILE=""
SFU_HOST_ADDRESS_A=""; SFU_HOST_ADDRESS_B=""
need_value() { [ "$#" -ge 2 ] || { echo "参数 $1 需要值" >&2; exit 2; }; }
while [ "$#" -gt 0 ]; do
  case "$1" in
    --pop-a) need_value "$@"; POP_A="$2"; shift 2 ;;
    --pop-b) need_value "$@"; POP_B="$2"; shift 2 ;;
    --auth) need_value "$@"; AUTH="$2"; shift 2 ;;
    --room-prefix) need_value "$@"; ROOM_PREFIX="$2"; shift 2 ;;
    --signal-url-a) need_value "$@"; SIGNAL_A_URL="$2"; shift 2 ;;
    --signal-url-b) need_value "$@"; SIGNAL_B_URL="$2"; shift 2 ;;
    --install-dir) need_value "$@"; INSTALL_DIR="$2"; shift 2 ;;
    --record-dir) need_value "$@"; RECORD_DIR="$2"; shift 2 ;;
    --jwt-secret) need_value "$@"; JWT_SECRET="$2"; shift 2 ;;
    --cert-file) need_value "$@"; CERT_FILE="$2"; shift 2 ;;
    --key-file) need_value "$@"; KEY_FILE="$2"; shift 2 ;;
    --sfu-host-address-a) need_value "$@"; SFU_HOST_ADDRESS_A="$2"; shift 2 ;;
    --sfu-host-address-b) need_value "$@"; SFU_HOST_ADDRESS_B="$2"; shift 2 ;;
    --sfu-port) need_value "$@"; SFU_PORT="$2"; shift 2 ;;
    --sfu-http-port) need_value "$@"; SFU_HTTP_PORT="$2"; shift 2 ;;
    --sfu-int-port) need_value "$@"; SFU_INT_PORT="$2"; shift 2 ;;
    --sig-wss-port) need_value "$@"; SIG_WSS_PORT="$2"; shift 2 ;;
    --sig-plain-port) need_value "$@"; SIG_PLAIN_PORT="$2"; shift 2 ;;
    --turn-port) need_value "$@"; TURN_PORT="$2"; shift 2 ;;
    --turn-tls-port) need_value "$@"; TURN_TLS_PORT="$2"; shift 2 ;;
    --profile) need_value "$@"; PROFILE="$2"; TARGET_DIR="$PWD/target/$PROFILE"; shift 2 ;;
    --skip-build) SKIP_BUILD=1; shift ;;
    --deploy-only) DEPLOY_ONLY=1; shift ;;
    --cleanup) CLEANUP=1; shift ;;
    --dry-run) DRY_RUN=1; shift ;;
    --help|-h)
      # 只输出文件头部连续注释块（用法/前提/注意），到首个非注释行即停，
      # 避免 sed 固定行号越界泄漏 set -uo pipefail 等源码行。
      awk 'NR == 1 { next } /^#/ { line = $0; sub(/^# ?/, "", line); print line; next } { exit }' "$0"
      exit 0 ;;
    *) echo "未知参数: ${1}（--help 查看用法）" >&2; exit 2 ;;
  esac
done
case "$PROFILE" in release|debug) ;; *) echo "--profile 仅支持 release|debug" >&2; exit 2 ;; esac
[ -n "$POP_A" ] && [ -n "$POP_B" ] || { echo "需要 --pop-a 与 --pop-b（user@host）" >&2; exit 2; }
command -v openssl >/dev/null || { echo "需要 openssl（生成默认 token/密钥）" >&2; exit 2; }
[ -n "$AUTH" ] || AUTH="$(openssl rand -hex 16)"
[ -n "$JWT_SECRET" ] && echo "WARN: --jwt-secret 会让信令只认 JWT，静态 token 验收将失败（见注意 1）" >&2

say() { echo "[deploy] $*"; }
run() { if [ "$DRY_RUN" = "1" ]; then echo "[dry-run] $*"; else "$@"; fi; }
ssh_run() { # $1=host $2=命令
  if [ "$DRY_RUN" = "1" ]; then echo "[dry-run] ssh $1: $2"; return 0; fi
  ssh "$1" "$2" || { echo "ssh 失败: $1" >&2; exit 1; }
}
scp_to() { # $1=host $2=本地 $3=远端
  if [ "$DRY_RUN" = "1" ]; then echo "[dry-run] scp $2 $1:$3"; return 0; fi
  scp -q "$2" "$1:$3" || { echo "scp 失败: $2 -> $1:$3" >&2; exit 1; }
}

HOST_A="${POP_A##*@}"; HOST_B="${POP_B##*@}"
# 当前 tungstenite 构建无 TLS feature：默认明文 ws://<host>:3003/ws（见注意 2）。
SIGNAL_A_URL="${SIGNAL_A_URL:-ws://${HOST_A}:${SIG_PLAIN_PORT}/ws}"
SIGNAL_B_URL="${SIGNAL_B_URL:-ws://${HOST_B}:${SIG_PLAIN_PORT}/ws}"

# ---- env 生成（printf 展开；$ 保留字面，供 sh -c 在子进程展开）----
gen_sfu_env() { # $1=pop-a|pop-b
  OUT=""
  printf -v OUT '%sEnvironment=SFU_MEDIA_PORT=%s\n' "$OUT" "$SFU_PORT"
  printf -v OUT '%sEnvironment=SFU_SIGNAL_PORT=%s\n' "$OUT" "$SFU_HTTP_PORT"
  printf -v OUT '%sEnvironment=SFU_INTERNAL_PORT=%s\n' "$OUT" "$SFU_INT_PORT"
  printf -v OUT '%sEnvironment=INTERNAL_TOKEN=%s\n' "$OUT" "$AUTH"
  printf -v OUT '%sEnvironment=RECORD_DIR=%s\n' "$OUT" "$RECORD_DIR"
  printf -v OUT '%sEnvironment=TURN_SECRET=%s\n' "$OUT" "$AUTH"
  printf -v OUT '%sEnvironment=SFU_TURN_PORT=%s\n' "$OUT" "$TURN_PORT"
  printf -v OUT '%sEnvironment=SFU_TURN_TLS_PORT=%s\n' "$OUT" "$TURN_TLS_PORT"
  # #216：SFU 对外通告地址与绑定地址分离（防 docker0/podman 等虚拟网卡排在 eth0 前
  # 被 select_host_address 选中，导致媒体/TURN 通告不可达地址）。HOST 为点分 IPv4
  # 时默认通告 HOST + 绑定 0.0.0.0；可用 --sfu-host-address-a/-b 显式覆盖。
  local h host_override
  if [ "$1" = "pop-a" ]; then h="$HOST_A"; host_override="$SFU_HOST_ADDRESS_A"; else h="$HOST_B"; host_override="$SFU_HOST_ADDRESS_B"; fi
  if [ -n "$host_override" ]; then
    printf -v OUT '%sEnvironment=SFU_HOST_ADDRESS=%s\n' "$OUT" "$host_override"
    printf -v OUT '%sEnvironment=SFU_BIND_ADDRESS=0.0.0.0\n' "$OUT"
  elif echo "$h" | grep -qE '^[0-9]+\.[0-9]+\.[0-9]+\.[0-9]+$'; then
    printf -v OUT '%sEnvironment=SFU_HOST_ADDRESS=%s\n' "$OUT" "$h"
    printf -v OUT '%sEnvironment=SFU_BIND_ADDRESS=0.0.0.0\n' "$OUT"
  fi
  if [ -n "$CERT_FILE" ]; then printf -v OUT '%sEnvironment=CERT_FILE=%s\n' "$OUT" "$CERT_FILE"; fi
  if [ -n "$KEY_FILE" ]; then printf -v OUT '%sEnvironment=KEY_FILE=%s\n' "$OUT" "$KEY_FILE"; fi
}
gen_signal_env() { # $1=pop-a|pop-b
  OUT=""
  printf -v OUT '%sEnvironment=SIGNAL_PORT=%s\n' "$OUT" "$SIG_WSS_PORT"
  printf -v OUT '%sEnvironment=SIGNAL_PLAIN_PORT=%s\n' "$OUT" "$SIG_PLAIN_PORT"
  printf -v OUT '%sEnvironment=AUTH_TOKENS=%s\n' "$OUT" "$AUTH"
  if [ -n "$JWT_SECRET" ]; then printf -v OUT '%sEnvironment=JWT_SECRET=%s\n' "$OUT" "$JWT_SECRET"; fi
  printf -v OUT '%sEnvironment=TURN_SECRET=%s\n' "$OUT" "$AUTH"
  printf -v OUT '%sEnvironment=SFU_URL=http://127.0.0.1:%s\n' "$OUT" "$SFU_INT_PORT"
  printf -v OUT '%sEnvironment=SFU_TOKEN=%s\n' "$OUT" "$AUTH"
  printf -v OUT '%sEnvironment=POP_ID=%s\n' "$OUT" "$1"
  if [ "$1" = "pop-a" ]; then
    printf -v OUT '%sEnvironment=TURN_URLS=turn:%s:%s?transport=udp,turn:%s:%s?transport=tcp,turns:%s:%s?transport=tcp\n' "$OUT" "$HOST_A" "$TURN_PORT" "$HOST_A" "$TURN_PORT" "$HOST_A" "$TURN_TLS_PORT"
  else
    printf -v OUT '%sEnvironment=TURN_URLS=turn:%s:%s?transport=udp,turn:%s:%s?transport=tcp,turns:%s:%s?transport=tcp\n' "$OUT" "$HOST_B" "$TURN_PORT" "$HOST_B" "$TURN_PORT" "$HOST_B" "$TURN_TLS_PORT"
    printf -v OUT '%sEnvironment=ROOM_POP_MAP=%s=pop-a\n' "$OUT" "$ROOM_PREFIX"
    printf -v OUT '%sEnvironment=POP_URLS=pop-a=%s\n' "$OUT" "$SIGNAL_A_URL"
    # systemd Environment= 值含空格必须整段加引号；$BRIDGE_AUTH_TOKEN 保持字面，
    # 由 signal 的 sh -c 在子进程展开（BRIDGE_AUTH_TOKEN 单独一行设置）。
    printf -v OUT '%sEnvironment="BRIDGE_CMD=%s/bin/aerodesk-bridge --remote-signal %s --local-signal %s --room {room} --auth-token $BRIDGE_AUTH_TOKEN --codec h264"\n' "$OUT" "$INSTALL_DIR" "$SIGNAL_A_URL" "$SIGNAL_B_URL"
    printf -v OUT '%sEnvironment=BRIDGE_AUTH_TOKEN=%s\n' "$OUT" "$AUTH"
    printf -v OUT '%sEnvironment=BRIDGE_READY_TIMEOUT_SECS=30\n' "$OUT"
  fi
  if [ -n "$CERT_FILE" ]; then printf -v OUT '%sEnvironment=CERT_FILE=%s\n' "$OUT" "$CERT_FILE"; fi
  if [ -n "$KEY_FILE" ]; then printf -v OUT '%sEnvironment=KEY_FILE=%s\n' "$OUT" "$KEY_FILE"; fi
}
gen_unit_file() { # $1=unit名 $2=kind $3=pop  -> $TMPDIR_UNITS/$1
  gen_${2}_env "$3"
  local envs="$OUT"
  cat > "$TMPDIR_UNITS/$1" <<EOF
[Unit]
Description=AeroDesk ${2} (${3})
After=network-online.target
Wants=network-online.target

[Service]
Type=simple
ExecStart=${INSTALL_DIR}/bin/aerodesk-${2}
Restart=on-failure
RestartSec=2
AmbientCapabilities=CAP_NET_BIND_SERVICE
${envs}

[Install]
WantedBy=multi-user.target
EOF
}

# ---- cleanup 优先于构建（无需本地产物）----
if [ "$CLEANUP" = "1" ]; then
  say "停止并禁用服务（--cleanup）"
  for h in "$POP_A" "$POP_B"; do
    ssh_run "$h" "sudo systemctl disable --now aerodesk-sfu aerodesk-signal || true; pkill -f '[a]erodesk-bridge' || true"
  done
  exit 0
fi

# ---- 1) 构建（release）----
if [ "$SKIP_BUILD" = "0" ]; then
  say "构建 $PROFILE 产物（sfu/signal/bridge/cli）"
  run cargo build --"$PROFILE" -p aerodesk-sfu -p aerodesk-signal -p aerodesk-cli || { echo "构建失败" >&2; exit 1; }
fi
for b in aerodesk-sfu aerodesk-signal aerodesk-bridge; do
  [ -x "$TARGET_DIR/$b" ] || { echo "缺少 $TARGET_DIR/${b}（--skip-build 时需已构建）" >&2; exit 1; }
done

# ---- 2) 部署到两台主机 ----
deploy_host() { # $1=host(user@host) $2=pop
  local host="$1" pop="$2"
  say "部署 $pop -> $host"
  ssh_run "$host" "mkdir -p ${INSTALL_DIR}/bin ${RECORD_DIR}"
  scp_to "$host" "$TARGET_DIR/aerodesk-sfu" "${INSTALL_DIR}/bin/"
  scp_to "$host" "$TARGET_DIR/aerodesk-signal" "${INSTALL_DIR}/bin/"
  scp_to "$host" "$TARGET_DIR/aerodesk-bridge" "${INSTALL_DIR}/bin/"
  TMPDIR_UNITS="$(mktemp -d)"
  gen_unit_file "aerodesk-sfu.service" "sfu" "$pop"
  gen_unit_file "aerodesk-signal.service" "signal" "$pop"
  scp_to "$host" "$TMPDIR_UNITS/aerodesk-sfu.service" "/tmp/aerodesk-sfu.service"
  scp_to "$host" "$TMPDIR_UNITS/aerodesk-signal.service" "/tmp/aerodesk-signal.service"
  rm -rf "$TMPDIR_UNITS"
  # unit 含密钥：0600；安装后删除远端临时副本。
  ssh_run "$host" "sudo install -m 0600 /tmp/aerodesk-sfu.service /tmp/aerodesk-signal.service /etc/systemd/system/ && sudo rm -f /tmp/aerodesk-sfu.service /tmp/aerodesk-signal.service && sudo systemctl daemon-reload && sudo systemctl enable --now aerodesk-sfu aerodesk-signal"
}

deploy_host "$POP_A" "pop-a"
deploy_host "$POP_B" "pop-b"

# ---- 3) 健康等待（验收用明文端口 3003；dry-run 只打印命令）----
say "等待服务健康（$HOST_A:$SIG_PLAIN_PORT / $HOST_B:${SIG_PLAIN_PORT}）"
if [ "$DRY_RUN" = "1" ]; then
  echo "[dry-run] nc -z $HOST_A $SIG_PLAIN_PORT && nc -z $HOST_B $SIG_PLAIN_PORT"
else
  for i in $(seq 1 60); do
    if nc -z "$HOST_A" "$SIG_PLAIN_PORT" 2>/dev/null && nc -z "$HOST_B" "$SIG_PLAIN_PORT" 2>/dev/null; then
      say "双 PoP 信令端口就绪"; break
    fi
    [ "$i" = "60" ] && { echo "健康等待超时" >&2; exit 1; }
    sleep 2
  done
fi

# ---- 4) 远程模式验收 + 报告 ----
if [ "$DEPLOY_ONLY" = "1" ]; then
  say "部署完成（--deploy-only，跳过验收）"
  exit 0
fi
REPORT="/tmp/aerodesk-acceptance-$(date +%Y%m%d-%H%M%S).log"
say "运行远程模式验收（报告：${REPORT}）"
BRIDGE_CMD_ACCEPT="${INSTALL_DIR}/bin/aerodesk-bridge --remote-signal ${SIGNAL_A_URL} --local-signal ${SIGNAL_B_URL} --room {room} --auth-token \$BRIDGE_AUTH_TOKEN --codec h264"
if [ "$DRY_RUN" = "1" ]; then
  echo "[dry-run] POP_A_SIGNAL=$SIGNAL_A_URL POP_B_SIGNAL=$SIGNAL_B_URL AUTH=<redacted> \\"
  echo "[dry-run]   BRIDGE_CMD='$BRIDGE_CMD_ACCEPT' BRIDGE_KILL_CMD='ssh $POP_B pkill -f aerodesk-bridge' \\"
  echo "[dry-run]   scripts/bridge-fallback-e2e.sh | tee $REPORT"
  exit 0
fi
POP_A_SIGNAL="$SIGNAL_A_URL" POP_B_SIGNAL="$SIGNAL_B_URL" AUTH="$AUTH" \
  BRIDGE_CMD="$BRIDGE_CMD_ACCEPT" \
  BRIDGE_KILL_CMD="ssh $POP_B pkill -f aerodesk-bridge" \
  scripts/bridge-fallback-e2e.sh 2>&1 | tee "$REPORT"
RC=${PIPESTATUS[0]}
echo
say "验收报告：$REPORT"
grep -E "PASS|FAIL|p50/p90/p99" "$REPORT" | tail -20 || true
if [ "$RC" = "0" ]; then
  say "真实多 PoP 部署验收 PASS"
else
  say "验收失败（RC=${RC}），详见 $REPORT"
fi
exit "$RC"
