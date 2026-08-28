#!/usr/bin/env bash
# multipop-deploy.sh —— 真实双 PoP 一键部署 + 远程模式验收（#216 M8 / #264）。
#
# 把「构建 → 拷贝 → 装 systemd → 配 env → 启动 → 远程验收」收敛为一条命令。
#
# 前提：
#   1) 本机可免密 ssh 到两台主机且有 sudo（systemd 安装）；
#   2) 本机可编译（默认 release）；本机有 curl（P3 探活走 ops HTTPS /healthz）；
#   3) 端口：SFU 媒体 3478/公共 HTTPS 3000/内部 3002；signal ops HTTPS 3001/
#      SIP/UDP 5060（P3 SIP 单栈，TLS 5061/WSS 3061 默认同证书开启）；
#      TURN UDP+TCP 3479/TLS 5349（可经 --*-port 覆盖）；
#   4) 认证：静态 token（AUTH_TOKENS）——即 SIP Digest 口令（规范 §8 迁移期
#      同一凭据），客户端 --token 直传；
#   5) 客户端信令地址为 SIP 形态 ws://<host>:<sip-udp-port>（agent 的 ws://
#      URL 即 SIP 寻址载体，AERO_SIP_PORT 可显式覆盖端口）；跨 PoP 房间由
#      signal 302+Contact（POP_SIP_URLS）引导；
#   6) --cert-file/--key-file 指向主机上已存在的证书路径（脚本不负责上传）。
#
# 注意：远程桥接模式验收（BRIDGE 编排）随 P3 服务端拆栈退役，待 #601 桥双腿
#       SIP 化重建后恢复；当前 --deploy-only 部署链路可用。
#
# 用法：
#   scripts/multipop-deploy.sh --pop-a root@pop-a.example.com --pop-b root@pop-b.example.com \
#     --auth <信令token> [--dry-run] [--deploy-only] [--cleanup] [--help]
#   [--sfu-host-address-a <IP> --sfu-host-address-b <IP>]  # SFU 对外通告地址覆盖（NAT/docker0；默认取 HOST 点分 IPv4）
set -uo pipefail
cd "$(dirname "$0")/.."

# ---- 参数 ----
POP_A=""; POP_B=""
AUTH=""; ROOM_PREFIX="bridge-"
SIGNAL_A_URL=""; SIGNAL_B_URL=""
INSTALL_DIR="/opt/aerodesk"; RECORD_DIR="/var/lib/aerodesk/records"
SFU_PORT=3478; SFU_HTTP_PORT=3000; SFU_INT_PORT=3002
SIG_OPS_PORT=3001; SIG_SIP_PORT=5060; TURN_PORT=3479; TURN_TLS_PORT=5349
PROFILE="release"; TARGET_DIR="$PWD/target/$PROFILE"
DRY_RUN=0; DEPLOY_ONLY=0; CLEANUP=0; SKIP_BUILD=0
CERT_FILE=""; KEY_FILE=""
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
    --cert-file) need_value "$@"; CERT_FILE="$2"; shift 2 ;;
    --key-file) need_value "$@"; KEY_FILE="$2"; shift 2 ;;
    --sfu-host-address-a) need_value "$@"; SFU_HOST_ADDRESS_A="$2"; shift 2 ;;
    --sfu-host-address-b) need_value "$@"; SFU_HOST_ADDRESS_B="$2"; shift 2 ;;
    --sfu-port) need_value "$@"; SFU_PORT="$2"; shift 2 ;;
    --sfu-http-port) need_value "$@"; SFU_HTTP_PORT="$2"; shift 2 ;;
    --sfu-int-port) need_value "$@"; SFU_INT_PORT="$2"; shift 2 ;;
    --sig-ops-port) need_value "$@"; SIG_OPS_PORT="$2"; shift 2 ;;
    --sig-sip-port) need_value "$@"; SIG_SIP_PORT="$2"; shift 2 ;;
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
# P3 SIP 单栈：客户端信令地址 = SIP 形态 ws://<host>:<SIG_SIP_PORT>（agent 解析
# 为 SIP/UDP 到该 host:port，AERO_SIP_PORT 可显式覆盖；见前提 5）。
SIGNAL_A_URL="${SIGNAL_A_URL:-ws://${HOST_A}:${SIG_SIP_PORT}}"
SIGNAL_B_URL="${SIGNAL_B_URL:-ws://${HOST_B}:${SIG_SIP_PORT}}"

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
  printf -v OUT '%sEnvironment=SIGNAL_OPS_PORT=%s\n' "$OUT" "$SIG_OPS_PORT"
  # P3 SIP 单栈：SIP/UDP 显式；SIP/TLS 5061 与 SIP/WSS 3061 复用同证书默认开。
  printf -v OUT '%sEnvironment=SIP_UDP_PORT=%s\n' "$OUT" "$SIG_SIP_PORT"
  printf -v OUT '%sEnvironment=AUTH_TOKENS=%s\n' "$OUT" "$AUTH"
  printf -v OUT '%sEnvironment=SFU_URL=http://127.0.0.1:%s\n' "$OUT" "$SFU_INT_PORT"
  printf -v OUT '%sEnvironment=SFU_TOKEN=%s\n' "$OUT" "$AUTH"
  printf -v OUT '%sEnvironment=POP_ID=%s\n' "$OUT" "$1"
  # 跨 PoP：本 PoP 房间归属他 PoP 时 302+Contact 引导（host:port 载体）。
  # 双向对称——任一 PoP 都可能收到归属对端的 INVITE，缺一侧会回 486 而非 302。
  if [ "$1" = "pop-a" ]; then
    printf -v OUT '%sEnvironment=POP_SIP_URLS=pop-b=%s:%s\n' "$OUT" "$HOST_B" "$SIG_SIP_PORT"
  else
    printf -v OUT '%sEnvironment=POP_SIP_URLS=pop-a=%s:%s\n' "$OUT" "$HOST_A" "$SIG_SIP_PORT"
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
  run cargo build --"$PROFILE" -p aerodesk-sfu -p aerodesk-signal -p aerodesk-agent || { echo "构建失败" >&2; exit 1; }
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

# ---- 3) 健康等待（P3：探 ops HTTPS /healthz；dry-run 只打印命令）----
say "等待服务健康（$HOST_A:$SIG_OPS_PORT / $HOST_B:${SIG_OPS_PORT}）"
if [ "$DRY_RUN" = "1" ]; then
  echo "[dry-run] curl -sk https://$HOST_A:$SIG_OPS_PORT/healthz && curl -sk https://$HOST_B:$SIG_OPS_PORT/healthz"
else
  for i in $(seq 1 60); do
    if curl -sk "https://$HOST_A:$SIG_OPS_PORT/healthz" 2>/dev/null | grep -q status \
       && curl -sk "https://$HOST_B:$SIG_OPS_PORT/healthz" 2>/dev/null | grep -q status; then
      say "双 PoP ops 面就绪"; break
    fi
    [ "$i" = "60" ] && { echo "健康等待超时" >&2; exit 1; }
    sleep 2
  done
fi

# ---- 4) 远程验收（P3 退役注记）----

if [ "$DEPLOY_ONLY" = "1" ]; then
  say "部署完成（--deploy-only，跳过验收）"
  exit 0
fi
# P3：BRIDGE 编排（远程桥验收）随服务端拆栈退役——待 #601 桥双腿 SIP 化重建。
# 当前 --deploy-only 部署链路可用；不带 --deploy-only 时提示并退出。
say "远程桥验收已退役（BRIDGE 编排待 #601 重建）；仅完成部署与健康检查"
exit 0

