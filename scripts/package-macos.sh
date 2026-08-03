#!/usr/bin/env bash
# 打包 macOS App bundle（#7 Slint UI 壳）。
# 产物: dist/AeroDesk.app（拖拽即用；正式分发需签名/公证）。
set -euo pipefail
cd "$(dirname "$0")/.."

cargo build --release -p aerodesk-ui

APP=dist/AeroDesk.app
rm -rf "$APP" 2>/dev/null || true
mkdir -p "$APP/Contents/MacOS" "$APP/Contents/Resources"

cp target/release/aerodesk-ui "$APP/Contents/MacOS/AeroDesk"
cat > "$APP/Contents/Info.plist" <<'PLIST'
<?xml version="1.0" encoding="UTF-8"?>
<!DOCTYPE plist PUBLIC "-//Apple//DTD PLIST 1.0//EN" "http://www.apple.com/DTDs/PropertyList-1.0.dtd">
<plist version="1.0">
<dict>
    <key>CFBundleName</key><string>AeroDesk</string>
    <key>CFBundleDisplayName</key><string>AeroDesk</string>
    <key>CFBundleIdentifier</key><string>io.aerodesk.desktop</string>
    <key>CFBundleExecutable</key><string>AeroDesk</string>
    <key>CFBundlePackageType</key><string>APPL</string>
    <key>CFBundleShortVersionString</key><string>0.1.0</string>
    <key>CFBundleVersion</key><string>1</string>
    <key>LSMinimumSystemVersion</key><string>13.0</string>
    <key>NSHighResolutionCapable</key><true/>
    <key>NSAppTransportSecurity</key>
    <dict>
        <key>NSAllowsArbitraryLoads</key><true/>
    </dict>
</dict>
</plist>
PLIST
chmod +x "$APP/Contents/MacOS/AeroDesk"
echo "== 完成: $APP"
echo "签名/公证（正式分发）:"
echo "  codesign --force --deep --sign 'Developer ID Application: ...' $APP"
echo "  xcrun notarytool submit $APP --keychain-profile aerodesk --wait"
