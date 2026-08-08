#!/usr/bin/env bash
# 打包 macOS App bundle（#7 Slint UI 壳）。
# 产物: dist/AeroDesk.app（默认）；--dmg 额外产出 dist/AeroDesk-<版本>.dmg（拖拽安装）。
# 版本号取自 Cargo.toml；图标/Info.plist 见 app-assets/。
# 正式分发：codesign + notarytool 在 CI release.yml 或本地手动执行。
set -euo pipefail
cd "$(dirname "$0")/.."

MAKE_DMG=0
for a in "$@"; do
  case "$a" in
    --dmg) MAKE_DMG=1 ;;
    *) echo "未知参数: $a（支持 --dmg）" >&2; exit 1 ;;
  esac
done

VERSION=$(sed -n 's/^version = "\(.*\)"/\1/p' Cargo.toml | head -1)
[ -n "$VERSION" ] || { echo "无法从 Cargo.toml 读取版本" >&2; exit 1; }
echo "== 版本: $VERSION"

echo "[1/3] cargo release 编译 -p aerodesk-ui …"
cargo build --release -p aerodesk-ui

echo "[2/3] 组装 dist/AeroDesk.app …"
bash scripts/assemble-macos-app.sh "$VERSION" target/release/aerodesk-ui dist/AeroDesk.app

if [ "$MAKE_DMG" = "1" ]; then
  echo "[3/3] 打包 DMG（拖拽安装）…"
  STAGING="$(mktemp -d)"
  trap 'rm -rf "$STAGING"' EXIT
  cp -R dist/AeroDesk.app "$STAGING/"
  ln -s /Applications "$STAGING/Applications"
  DMG="dist/AeroDesk-$VERSION.dmg"
  rm -f "$DMG" 2>/dev/null || true
  hdiutil create -volname "AeroDesk" -srcfolder "$STAGING" -ov -format UDZO "$DMG"
  echo "== 完成: $DMG"
  echo "签名/公证（正式分发）:"
  echo "  codesign --force --options runtime --timestamp --sign 'Developer ID Application: …' 'dist/AeroDesk.app'"
  echo "  xcrun notarytool submit '$DMG' --keychain-profile aerodesk --wait"
fi
