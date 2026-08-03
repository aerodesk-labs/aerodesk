#!/usr/bin/env bash
# certbot deploy-hook：续期后将新证书原子安装到 aerodesk 服务路径并重启。
#
# 用法（certbot 配置）：
#   certbot certonly --standalone -d signal.aerodesk.io \
#     --deploy-hook /path/to/scripts/cert-renew-hook.sh
# 或：
#   certbot renew --deploy-hook /path/to/scripts/cert-renew-hook.sh
#
# 环境变量：
#   CERT_DEST  安装目录（默认 /etc/aerodesk/tls）
#   SIGNAL_SVC / SFU_SVC  systemd 服务名（默认 aerodesk-signal / aerodesk-sfu）
#
# 原子性与多 lineage（#13）：
# - 每个 lineage 安装到独立子目录 $CERT_DEST/<lineage>/，文件先写临时文件再
#   mv（同目录 rename 原子），服务不可能读到半截文件；
# - 顶层 cer.pem / key.pem 是指向最新 lineage 的符号链接，用 ln -sfn 单步原子切换，
#   证书与私钥永远来自同一个 lineage（不会出现证书/私钥错配或跨域名混合）；
# - 多 lineage（如 signal 与 sfu 各一张证书）共享 CERT_DEST 时互不覆盖；
#   如需服务各用各的，直接让服务读取 $CERT_DEST/<lineage>/fullchain.pem|privkey.pem。
set -euo pipefail

CERT_DEST="${CERT_DEST:-/etc/aerodesk/tls}"
SIGNAL_SVC="${SIGNAL_SVC:-aerodesk-signal}"
SFU_SVC="${SFU_SVC:-aerodesk-sfu}"

# certbot 注入的续期证书路径（LIVE 目录）
: "${RENEWED_LINEAGE:?RENEWED_LINEAGE not set (run from certbot deploy-hook)}"

LINEAGE_NAME="$(basename "$RENEWED_LINEAGE")"
LINEAGE_DIR="$CERT_DEST/$LINEAGE_NAME"
TMP_FULL="$LINEAGE_DIR/.fullchain.pem.tmp.$$"
TMP_KEY="$LINEAGE_DIR/.privkey.pem.tmp.$$"
trap 'rm -f "$TMP_FULL" "$TMP_KEY"' EXIT

mkdir -p "$LINEAGE_DIR"

# 先写临时文件并设好权限，最后 mv（同目录 rename，原子）
cp "$RENEWED_LINEAGE/fullchain.pem" "$TMP_FULL"
cp "$RENEWED_LINEAGE/privkey.pem"   "$TMP_KEY"
chmod 640 "$TMP_KEY"
chown root:root "$TMP_FULL" "$TMP_KEY" 2>/dev/null || true
mv -f "$TMP_FULL" "$LINEAGE_DIR/fullchain.pem"
mv -f "$TMP_KEY"  "$LINEAGE_DIR/privkey.pem"
trap - EXIT

# 顶层符号链接原子切换到本 lineage（证书/私钥成对出现）
ln -sfn "$LINEAGE_NAME/fullchain.pem" "$CERT_DEST/cer.pem"
ln -sfn "$LINEAGE_NAME/privkey.pem"  "$CERT_DEST/key.pem"
chmod 640 "$CERT_DEST/cer.pem" "$CERT_DEST/key.pem" 2>/dev/null || true
chown root:root "$CERT_DEST" 2>/dev/null || true

echo "== 证书已原子安装到 ${CERT_DEST}（lineage=${LINEAGE_NAME}），重启服务"
if command -v systemctl >/dev/null 2>&1; then
  systemctl restart "$SIGNAL_SVC" "$SFU_SVC"
else
  echo "（非 systemd 环境：请手动重启 signal/sfu，CERT_FILE/KEY_FILE 指向 ${CERT_DEST}）"
fi
