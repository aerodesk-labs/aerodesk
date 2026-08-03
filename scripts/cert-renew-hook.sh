#!/usr/bin/env bash
# certbot deploy-hook：续期后将新证书安装到 aerodesk 服务路径并重启。
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
set -euo pipefail

CERT_DEST="${CERT_DEST:-/etc/aerodesk/tls}"
SIGNAL_SVC="${SIGNAL_SVC:-aerodesk-signal}"
SFU_SVC="${SFU_SVC:-aerodesk-sfu}"

# certbot 注入的续期证书路径（LIVE 目录）
: "${RENEWED_LINEAGE:?RENEWED_LINEAGE not set (run from certbot deploy-hook)}"

mkdir -p "$CERT_DEST"
cp "$RENEWED_LINEAGE/fullchain.pem" "$CERT_DEST/cer.pem"
cp "$RENEWED_LINEAGE/privkey.pem"   "$CERT_DEST/key.pem"
chmod 640 "$CERT_DEST/key.pem"
chown -R root:root "$CERT_DEST"

echo "== 证书已安装到 $CERT_DEST，重启服务"
if command -v systemctl >/dev/null 2>&1; then
  systemctl restart "$SIGNAL_SVC" "$SFU_SVC"
else
  echo "（非 systemd 环境：请手动重启 signal/sfu，CERT_FILE/KEY_FILE 指向 $CERT_DEST）"
fi
