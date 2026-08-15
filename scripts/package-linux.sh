#!/usr/bin/env bash
# Linux 打包：.deb（cargo-deb）+ 便携 tar.gz + rpm（rpmbuild）+ AppImage（linuxdeploy）。
# release workflow linux job 使用；依赖 ubuntu-latest（构建系统库见 ci.yml，
# rpmbuild 由本脚本自装或 release.yml System deps 提供）。
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
cargo deb -p aerodesk-desktop
cp target/debian/aerodesk_*.deb dist/

echo "== [3/5] 便携 tar.gz（二进制 + 图标 + desktop）"
STAGE="dist/aerodesk-$VERSION-linux-x86_64"
mkdir -p "$STAGE"
cp target/release/aerodesk-desktop "$STAGE/"
cp app-assets/icon-1024.png "$STAGE/aerodesk.png"
cp app-assets/aerodesk.desktop "$STAGE/aerodesk.desktop"
cat > "$STAGE/README.txt" <<'EOF'
AeroDesk Linux 便携包
- 直接运行：./aerodesk-desktop
- 可选安装 desktop/icon：
    mkdir -p ~/.local/share/applications ~/.local/share/icons/hicolor/512x512/apps
    cp aerodesk.desktop ~/.local/share/applications/
    cp aerodesk.png ~/.local/share/icons/hicolor/512x512/apps/aerodesk.png
EOF
tar -C dist -czf "dist/aerodesk-$VERSION-linux-x86_64.tar.gz" "$(basename "$STAGE")"
rm -rf "$STAGE"

echo "== [4/5] rpm（rpmbuild）"
if ! command -v rpmbuild >/dev/null 2>&1; then
  sudo apt-get install -y -qq rpm >/dev/null 2>&1 || true
fi
if command -v rpmbuild >/dev/null 2>&1; then
  RPM_TOPDIR="$ROOT/target/rpmbuild"
  mkdir -p "$RPM_TOPDIR/SPECS" "$RPM_TOPDIR/RPMS" "$RPM_TOPDIR/BUILD" "$RPM_TOPDIR/SOURCES" "$RPM_TOPDIR/BUILDROOT"
  SPEC="$RPM_TOPDIR/SPECS/aerodesk.spec"
  cat > "$SPEC" <<EOF
Name: aerodesk
Version: $VERSION
Release: 1
Summary: AeroDesk remote desktop client
License: MIT OR Apache-2.0
BuildArch: x86_64
Requires: libavcodec >= 60, libx264 >= 155, fontconfig, xkbcommon, libX11
%description
AeroDesk remote desktop client (viewer/controller): connect/room/settings across platforms.
%install
mkdir -p %{buildroot}/usr/bin %{buildroot}/usr/share/applications %{buildroot}/usr/share/icons/hicolor/512x512/apps
install -m755 $ROOT/target/release/aerodesk-desktop %{buildroot}/usr/bin/aerodesk-desktop
install -m644 $ROOT/app-assets/aerodesk.desktop %{buildroot}/usr/share/applications/aerodesk.desktop
install -m644 $ROOT/app-assets/icon-1024.png %{buildroot}/usr/share/icons/hicolor/512x512/apps/aerodesk.png
%files
/usr/bin/aerodesk-desktop
/usr/share/applications/aerodesk.desktop
/usr/share/icons/hicolor/512x512/apps/aerodesk.png
EOF
  rpmbuild -bb --define "_topdir $RPM_TOPDIR" "$SPEC" >/tmp/aerodesk-rpmbuild.log 2>&1 \
    || { echo "rpmbuild 失败："; tail -30 /tmp/aerodesk-rpmbuild.log; exit 1; }
  cp "$RPM_TOPDIR"/RPMS/x86_64/aerodesk-*.rpm dist/
else
  echo "WARN: rpmbuild 不可用，跳过 rpm"
fi

echo "== [5/5] AppImage（linuxdeploy）"
LINUXDEPLOY="$ROOT/target/linuxdeploy-x86_64.AppImage"
if [ ! -x "$LINUXDEPLOY" ]; then
  curl -L -sS -o "$LINUXDEPLOY" \
    "https://github.com/linuxdeploy/linuxdeploy/releases/download/continuous/linuxdeploy-x86_64.AppImage"
  chmod +x "$LINUXDEPLOY"
fi
export APPIMAGE_EXTRACT_AND_RUN=1
APPDIR="$ROOT/target/aerodesk-appdir"
rm -rf "$APPDIR"
mkdir -p "$APPDIR/usr/bin" "$APPDIR/usr/share/applications" "$APPDIR/usr/share/icons/hicolor/512x512/apps"
cp target/release/aerodesk-desktop "$APPDIR/usr/bin/"
cp app-assets/aerodesk.desktop "$APPDIR/usr/share/applications/"
cp app-assets/icon-1024.png "$APPDIR/usr/share/icons/hicolor/512x512/apps/aerodesk.png"
"$LINUXDEPLOY" --appdir "$APPDIR" --output appimage >/tmp/aerodesk-linuxdeploy.log 2>&1 \
  || { echo "linuxdeploy 失败："; tail -30 /tmp/aerodesk-linuxdeploy.log; exit 1; }
# linuxdeploy 按 desktop Name 命名：AeroDesk-x86_64.AppImage → 规范文件名。
mv *.AppImage "dist/aerodesk-$VERSION-x86_64.AppImage"

echo "== 产物 =="
ls -lh dist/
