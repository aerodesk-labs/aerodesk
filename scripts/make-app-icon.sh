#!/usr/bin/env bash
# 生成 AeroDesk.app 图标（默认 app-assets/AppIcon.icns）。
# 1) scripts/make-app-icon.py 画 1024x1024 占位图标（标准库，零依赖）
# 2) sips 缩放到 iconset 各尺寸
# 3) iconutil 合成 .icns
# 依赖: python3、sips、iconutil（macOS 自带）。
# 用法: scripts/make-app-icon.sh [输出目录]   # 默认 app-assets/AppIcon.icns
set -euo pipefail
cd "$(dirname "$0")/.."

OUT="${1:-app-assets/AppIcon.icns}"
TMP="$(mktemp -d)"
trap 'rm -rf "$TMP"' EXIT

python3 scripts/make-app-icon.py "$TMP/icon-1024.png"

ICONSET="$TMP/AppIcon.iconset"
mkdir -p "$ICONSET"
for size in 16 32 128 256 512; do
  sips -z "$size" "$size" "$TMP/icon-1024.png" --out "$ICONSET/icon_${size}x${size}.png" >/dev/null
  d=$((size * 2))
  sips -z "$d" "$d" "$TMP/icon-1024.png" --out "$ICONSET/icon_${size}x${size}@2x.png" >/dev/null
done

mkdir -p "$(dirname "$OUT")"
iconutil -c icns "$ICONSET" -o "$OUT"
echo "== 完成: $OUT"
