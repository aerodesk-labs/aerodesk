#!/usr/bin/env bash
# 组装 AeroDesk.app bundle（编译产物已就绪时调用；CI release 与本地打包共用）。
# 用法: scripts/assemble-macos-app.sh <版本> <二进制路径> <输出.app路径>
#   例: scripts/assemble-macos-app.sh 0.1.0 target/release/aerodesk-desktop dist/AeroDesk.app
set -euo pipefail
cd "$(dirname "$0")/.."

VERSION="$1"; BIN="$2"; APP="$3"
[ -n "$VERSION" ] && [ -f "$BIN" ] || { echo "用法: $0 <版本> <二进制> <输出.app>" >&2; exit 1; }

[ -f app-assets/AppIcon.icns ] || bash scripts/make-app-icon.sh

rm -rf "$APP"
mkdir -p "$APP/Contents/MacOS" "$APP/Contents/Resources"
cp "$BIN" "$APP/Contents/MacOS/AeroDesk"
sed "s/__VERSION__/$VERSION/g" app-assets/Info.plist > "$APP/Contents/Info.plist"
cp app-assets/AppIcon.icns "$APP/Contents/Resources/AppIcon.icns"
chmod +x "$APP/Contents/MacOS/AeroDesk"
# 签名：优先使用稳定身份（Developer ID / 自签证书），保证 TCC 授权跨重建有效。
# ad-hoc（-）每次重建 cdhash 都变，TCC 授权会失配（设置里已授权、程序内未授权）。
# 未签名/仅 linker-signed 的 bundle 也无法被 macOS TCC 识别为独立应用。
BUNDLE_ID="$(/usr/libexec/PlistBuddy -c 'Print :CFBundleIdentifier' "$APP/Contents/Info.plist" 2>/dev/null || echo io.aerodesk.desktop)"
IDENTITY="$(security find-identity -v -p codesigning 2>/dev/null | sed -n 's/.*"\([^"]*\)".*/\1/p' | grep 'Developer ID Application' | head -1)"
if [ -z "$IDENTITY" ]; then
  IDENTITY="$(security find-identity -v -p codesigning 2>/dev/null | sed -n 's/.*"\([^"]*\)".*/\1/p' | head -1)"
fi
if [ -n "$IDENTITY" ]; then
  codesign --force --sign "$IDENTITY" --identifier "$BUNDLE_ID" "$APP" >/dev/null 2>&1
  SIGN_TAG="stable identity: $IDENTITY"
else
  codesign --force --sign - "$APP" >/dev/null 2>&1
  SIGN_TAG="ad-hoc（无稳定身份，TCC 授权可能失配）"
fi
if codesign --verify --deep --strict "$APP" >/dev/null 2>&1; then
  echo "== 完成: $APP (v$VERSION, $SIGN_TAG, verified)"
else
  echo "!! 警告: $APP codesign 验证失败" >&2
  exit 1
fi
