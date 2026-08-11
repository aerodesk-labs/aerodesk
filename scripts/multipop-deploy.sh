#!/usr/bin/env bash
# multipop-deploy.sh —— 真实双 PoP 一键部署 + 远程模式验收（#216 M8 / #264）。
#
# 把「构建 → 拷贝 → 装 systemd → 配 env → 启动 → 远程验收」收敛为一条命令，
# 让 #216 剩余里程碑「真实多 PoP 部署验收」在基建就绪后一步执行。
#
# 前提：
#   1) 本机可免密 ssh 到两台主机且有 sudo（systemd 安装）；
#   2) 本机可编译（默认 release）；
#   3) 端口：SFU 媒体 3478/公共 HTTPS 3000/内部 3002；signal WSS 3001/明文 3003；
#      TURN UDP+TCP 3479/TLS 5349（可经 --*-port 覆盖）；
#   4) 客户端连信令地址默认 wss://<host>:443/ws（生产反代）；直连用
#      --signal-url-a/-b 覆盖（如 ws://<host>:3001/ws）。
#
# 用法：
#   scripts/multipop-deploy.sh --pop-a root@pop-a.example.com --pop-b root@pop-b.example.com \
#     --auth <信令token> --room-prefix bridge- [--dry-run] [--deploy-only] [--cleanup] [--help]
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
while [ "$#" -gt 0 ]; do
  case "$1" in
    --pop-a) POP_A="$2"; shift 2 ;;
    --pop-b) POP_B="$2"; shift 2 ;;
    --auth) AUTH="$2"; shift 2 ;;
    --room-prefix) ROOM_PREFIX="$2"; shift 2 ;;
    --signal-url-a) SIGNAL_A_URL="$2"; shift 2 ;;
    --signal-url-b) SIGNAL_B_URL="$2"; shift 2 ;;
    --install-dir) INSTALL_DIR="$2"; shift 2 ;;
    --record-dir) RECORD_DIR="$2"; shift 2 ;;
    --jwt-secret) JWT_SECRET="$2"; shift 2 ;;
    --cert-file) CERT_FILE="$2"; shift 2 ;;
    --key-file) KEY_FILE="$2"; shift 2 ;;
    --sfu-port) SFU_PORT="$2"; shift 2 ;;
    --sfu-http-port) SFU_HTTP_PORT="$2"; shift 2 ;;
    --sfu-int-port) SFU_INT_PORT="$2"; shift 2 ;;
    --sig-wss-port) SIG_WSS_PORT="$2"; shift 2 ;;
    --sig-plain-port) SIG_PLAIN_PORT="$2"; shift 2 ;;
    --turn-port) TURN_PORT="$2"; shift 2 ;;
    --turn-tls-port) TURN_TLS_PORT="$2"; shift 2 ;;
    --profile) PROFILE="$2"; TARGET_DIR="$PWD/target/$PROFILE"; shift 2 ;;
    --skip-build) SKIP_BUILD=1; shift ;;
    --deploy-only) DEPLOY_ONLY=1; shift ;;
    --cleanup) CLEANUP=1; shift ;;
    --dry-run) DRY_RUN=1; shift ;;
    --help|-h)
      sed -n '2,28p' "$0" | sed 's/^# \{0,1\}//'; exit 0 ;;
    *) echo "未知参数: ${1}（--help 查看用法）" >&2; exit 2 ;;
  esac
done
[ -n "$POP_A" ] && [ -n "$POP_B" ] || { echo "需要 --pop-a 与 --pop-b（user@host）" >&2; exit 2; }
[ -n "$AUTH" ] || AUTH="$(openssl rand -hex 16)"
[ -n "$JWT_SECRET" ] || JWT_SECRET="$(openssl rand -hex 24)"

say() { echo "[deploy] $*"; }
run() { if [ "$DRY_RUN" = "1" ]; then echo "[dry-run] $*"; else "$@"; fi; }
ssh_run() { if [ "$DRY_RUN" = "1" ]; then echo "[dry-run] ssh $1: $2"; else ssh "$1" "$2"; fi; }
scp_to() { if [ "$DRY_RUN" = "1" ]; then echo "[dry-run] scp $2 $1:$3"; else scp -q "$2" "$1:$3"; fi; }

HOST_A="${POP_A##*@}"; HOST_B="${POP_B##*@}"
SIGNAL_A_URL="${SIGNAL_A_URL:-wss://${HOST_A}:443/ws}"
SIGNAL_B_URL="${SIGNAL_B_URL:-wss://${HOST_B}:443/ws}"

# 按 PoP 生成 systemd unit 的 Environment 行（$ 需在 unit 里保持字面，供 sh -c 展开）。
# 用函数返回字符串（全局 OUT 变量），避免命令替换吃掉换行。
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
  if [ -n "$CERT_FILE" ]; then printf -v OUT '%sEnvironment=CERT_FILE=%s\n' "$OUT" "$CERT_FILE"; fi
  if [ -n "$KEY_FILE" ]; then printf -v OUT '%sEnvironment=KEY_FILE=%s\n' "$OUT" "$KEY_FILE"; fi
}
gen_signal_env() { # $1=pop-a|pop-b
  OUT=""
  printf -v OUT '%sEnvironment=SIGNAL_PORT=%s\n' "$OUT" "$SIG_WSS_PORT"
  printf -v OUT '%sEnvironment=SIGNAL_PLAIN_PORT=%s\n' "$OUT" "$SIG_PLAIN_PORT"
  printf -v OUT '%sEnvironment=JWT_SECRET=%s\n' "$OUT" "$JWT_SECRET"
  printf -v OUT '%sEnvironment=AUTH_TOKENS=%s\n' "$OUT" "$AUTH"
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
    printf -v OUT '%sEnvironment=BRIDGE_CMD=/opt/aerodesk/bin/aerodesk-bridge --remote-signal %s --local-signal %s --room {room} --auth-token $BRIDGE_AUTH_TOKEN --codec h264\n' "$OUT" "$SIGNAL_A_URL" "$SIGNAL_B_URL"
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
${envs}

[Install]
WantedBy=multi-user.target
EOF
}

# ---- 1) 构建（release）----
if [ "$SKIP_BUILD" = "0" ]; then
  say "构建 $PROFILE 产物（sfu/signal/bridge/cli）"
  run cargo build --$PROFILE -p aerodesk-sfu -p aerodesk-signal -p aerodesk-bridge -p aerodesk-cli || { echo "构建失败" >&2; exit 1; }
fi
for b in aerodesk-sfu aerodesk-signal aerodesk-bridge; do
  [ -x "$TARGET_DIR/$b" ] || { echo "缺少 $TARGET_DIR/${b}（--skip-build 时需已构建）" >&2; exit 1; }
done

# ---- 2) 部署到两台主机 ----
deploy_host() { # $1=host(user@host) $2=pop $3=is_a(0/1)
  local host="$1" pop="$2"
  say "部署 $pop -> $host"
  ssh_run "$host" "mkdir -p ${INSTALL_DIR}/bin ${RECORD_DIR} ${INSTALL_DIR}/units"
  scp_to "$host" "$TARGET_DIR/aerodesk-sfu" "${INSTALL_DIR}/bin/"
  scp_to "$host" "$TARGET_DIR/aerodesk-signal" "${INSTALL_DIR}/bin/"
  scp_to "$host" "$TARGET_DIR/aerodesk-bridge" "${INSTALL_DIR}/bin/"
  TMPDIR_UNITS="$(mktemp -d)"
  gen_unit_file "aerodesk-sfu.service" "sfu" "$pop"
  gen_unit_file "aerodesk-signal.service" "signal" "$pop"
  scp_to "$host" "$TMPDIR_UNITS/aerodesk-sfu.service" "/tmp/aerodesk-sfu.service"
  scp_to "$host" "$TMPDIR_UNITS/aerodesk-signal.service" "/tmp/aerodesk-signal.service"
  rm -rf "$TMPDIR_UNITS"
  ssh_run "$host" "sudo install -m 0644 /tmp/aerodesk-sfu.service /tmp/aerodesk-signal.service /etc/systemd/system/ && sudo systemctl daemon-reload && sudo systemctl enable --now aerodesk-sfu aerodesk-signal"
}

if [ "$CLEANUP" = "1" ]; then
  say "停止并禁用服务（--cleanup）"
  for h in "$POP_A" "$POP_B"; do
    ssh_run "$h" "sudo systemctl disable --now aerodesk-sfu aerodesk-signal || true"
  done
  exit 0
fi

deploy_host "$POP_A" "pop-a"
deploy_host "$POP_B" "pop-b"

# ---- 3) 健康等待 ----
say "等待服务健康（$HOST_A:$SIG_WSS_PORT / $HOST_B:${SIG_WSS_PORT}）"
for i in $(seq 1 60); do
  if nc -z "$HOST_A" "$SIG_WSS_PORT" 2>/dev/null && nc -z "$HOST_B" "$SIG_WSS_PORT" 2>/dev/null; then
    say "双 PoP 信令端口就绪"; break
  fi
  [ "$i" = "60" ] && { echo "健康等待超时" >&2; exit 1; }
  sleep 2
done

# ---- 4) 远程模式验收 + 报告 ----
if [ "$DEPLOY_ONLY" = "1" ]; then
  say "部署完成（--deploy-only，跳过验收）"
  exit 0
fi
REPORT="/tmp/aerodesk-acceptance-$(date +%Y%m%d-%H%M%S).log"
say "运行远程模式验收（报告：${REPORT}）"
BRIDGE_CMD_ACCEPT="/opt/aerodesk/bin/aerodesk-bridge --remote-signal ${SIGNAL_A_URL} --local-signal ${SIGNAL_B_URL} --room {room} --auth-token \$BRIDGE_AUTH_TOKEN --codec h264"
if [ "$DRY_RUN" = "1" ]; then
  echo "[dry-run] POP_A_SIGNAL=$SIGNAL_A_URL POP_B_SIGNAL=$SIGNAL_B_URL AUTH=$AUTH \\"
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
