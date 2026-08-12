#!/usr/bin/env bash
# Linux 打包：cargo-deb .deb + 便携 tar.gz（release workflow linux job 使用）。
# 依赖：ubuntu-latest（cargo-deb 自动经 dpkg-shlibdeps 探测依赖；构建系统库见 ci.yml）。
# 用法: bash scripts/package-linux.sh
set -euo pipefail
ROOT="$(cd "$(dirname "$0")/.." && pwd)"
cd "$ROOT"

VERSION="$(sed -n 's/^version = "\(.*\)"/\1/p' Cargo.toml | head -1)"
[ -n "$VERSION" ] || { echo "cannot read version"; exit 1; }
mkdir -p dist

echo "== [1/3] cargo-deb 安装（不存在时）"
if ! command -v cargo-deb >/dev/null 2>&1; then
  cargo install cargo-deb --locked
fi

echo "== [2/3] 构建 .deb（depends=\$auto 自动探测）"
cargo deb -p aerodesk-ui
cp target/debian/aerodesk_*.deb dist/

echo "== [3/3] 便携 tar.gz（二进制 + 图标 + desktop）"
STAGE="dist/aerodesk-$VERSION-linux-x86_64"
mkdir -p "$STAGE"
cp target/release/aerodesk-ui "$STAGE/"
cp app-assets/icon-1024.png "$STAGE/aerodesk.png"
cp app-assets/aerodesk.desktop "$STAGE/aerodesk.desktop"
cat > "$STAGE/README.txt" <<'EOF'
AeroDesk Linux 便携包
- 直接运行：./aerodesk-ui
- 可选安装 desktop/icon：
    mkdir -p ~/.local/share/applications ~/.local/share/icons/hicolor/512x512/apps
    cp aerodesk.desktop ~/.local/share/applications/
    cp aerodesk.png ~/.local/share/icons/hicolor/512x512/apps/aerodesk.png
EOF
tar -C dist -czf "dist/aerodesk-$VERSION-linux-x86_64.tar.gz" "$(basename "$STAGE")"
rm -rf "$STAGE"

echo "== 产物 =="
ls -lh dist/
