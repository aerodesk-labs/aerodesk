#!/usr/bin/env bash
# 组装 AeroDesk.app bundle（编译产物已就绪时调用；CI release 与本地打包共用）。
# 用法: scripts/assemble-macos-app.sh <版本> <二进制路径> <输出.app路径>
#   例: scripts/assemble-macos-app.sh 0.1.0 target/release/aerodesk-ui dist/AeroDesk.app
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
# 本地开发/CI 统一 ad-hoc 签名：绑定 Info.plist + 密封资源。
# 未签名/仅 linker-signed 的 bundle 无法被 macOS TCC 识别为独立应用，
# 屏幕录制/辅助功能授权列表里看不到 AeroDesk.app。
codesign --force --sign - "$APP" >/dev/null 2>&1
if codesign --verify --deep --strict "$APP" >/dev/null 2>&1; then
  echo "== 完成: $APP (v$VERSION, ad-hoc signed + verified)"
else
  echo "!! 警告: $APP codesign 验证失败" >&2
  exit 1
fi
