#!/usr/bin/env bash
# Windows 打包：便携 ZIP（aerodesk-ui.exe + aerodesk-cli.exe + FFmpeg DLL + 图标 + README）。
# release workflow windows job 使用（#7 PACKAGING.md「Windows 待接入」）。
# 依赖：windows-latest runner 已装 Git Bash；FFMPEG_DIR 指向 BtbN FFmpeg 共享构建根目录
#（release.yml 的 System deps 步骤设置，与 test job 同款）。
# 用法: bash scripts/package-windows.sh
set -euo pipefail
ROOT="$(cd "$(dirname "$0")/.." && pwd)"
cd "$ROOT"

VERSION="$(sed -n 's/^version = "\(.*\)"/\1/p' Cargo.toml | head -1)"
[ -n "$VERSION" ] || { echo "cannot read version"; exit 1; }
[ -n "${FFMPEG_DIR:-}" ] || { echo "FFMPEG_DIR 未设置（需指向 FFmpeg 共享构建根目录）"; exit 1; }

echo "== [1/3] 校验产物"
for b in target/release/aerodesk-ui.exe target/release/aerodesk-cli.exe; do
  [ -f "$b" ] || { echo "缺少 $b（先 cargo build --release -p aerodesk-ui -p aerodesk-cli）"; exit 1; }
done
[ -d "$FFMPEG_DIR/bin" ] || { echo "FFMPEG_DIR/bin 不存在: $FFMPEG_DIR"; exit 1; }

echo "== [2/3] 组装便携目录"
STAGE="dist/aerodesk-$VERSION-win64"
rm -rf "$STAGE"
mkdir -p "$STAGE"
cp target/release/aerodesk-ui.exe "$STAGE/"
cp target/release/aerodesk-cli.exe "$STAGE/"
# FFmpeg 共享 DLL（avcodec/avformat/avutil/avfilter/avdevice/swscale/swresample）。
cp "$FFMPEG_DIR"/bin/*.dll "$STAGE/"
cp app-assets/icon-1024.png "$STAGE/aerodesk.png"
cat > "$STAGE/README.txt" <<EOF
AeroDesk Windows 便携包（$VERSION）
- 观看/主控端：双击 aerodesk-ui.exe（连接服务器/房间，支持 Windows 被控端双角色）
- 命令行：aerodesk-cli.exe --role publisher|viewer --signal ws://<host>:3003 --room <room>
- 被控端示例：aerodesk-cli.exe --role publisher --encoder screen --signal ws://<host>:3003 --room demo
- 依赖：本目录内 FFmpeg 共享 DLL（avcodec/avformat/avutil/avfilter/avdevice/swscale/swresample），
  请保持 exe 与 DLL 同目录；Windows 10/11 x64。
EOF

echo "== [3/4] 构建 MSI（WiX 4.0.5，dotnet tool wix；不可用时跳过仅出 ZIP）"
if ! command -v wix >/dev/null 2>&1; then
  # 固定 4.0.5：WiX v7 引入 OSMF 许可门禁，自动 CI 不可用；v4 为 MIT。
  dotnet tool install --global wix --version 4.0.5 >/dev/null 2>&1 || true
  export PATH="$HOME/.dotnet/tools:$PATH"
fi
if command -v wix >/dev/null 2>&1; then
  wix build packaging/windows/AeroDesk.wxs \
    -o "dist/aerodesk-$VERSION-win64.msi" \
    -d "Version=$VERSION" -d "BuildDir=$STAGE" \
    >/tmp/aerodesk-wix.log 2>&1 \
    || { echo "wix build 失败："; tail -30 /tmp/aerodesk-wix.log; exit 1; }
else
  echo "WARN: wix 不可用，跳过 MSI（仅产 ZIP）"
fi

echo "== [4/4] 压缩 ZIP"
if command -v python3 >/dev/null 2>&1; then
  python3 -m zipfile -c "dist/aerodesk-$VERSION-win64.zip" "$STAGE"
else
  powershell.exe -NoProfile -Command "Compress-Archive -Path '$STAGE' -DestinationPath 'dist/aerodesk-$VERSION-win64.zip' -Force"
fi
rm -rf "$STAGE"
echo "== 产物 =="
ls -lh dist/aerodesk-$VERSION-win64.*