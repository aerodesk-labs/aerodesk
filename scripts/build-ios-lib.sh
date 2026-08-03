#!/usr/bin/env bash
# 构建 aerodesk-ios 静态库并复制到 ios/AeroDeskBridge/lib。
# 用法: scripts/build-ios-lib.sh [sim|device|all]
set -euo pipefail
cd "$(dirname "$0")/.."
DEST=ios/AeroDeskBridge/lib
mkdir -p "$DEST"
MODE="${1:-sim}"

build() {
  local target="$1"
  echo "== cargo build -p aerodesk-ios --target $target --release"
  cargo build -p aerodesk-ios --target "$target" --release
}

case "$MODE" in
  sim)
    build aarch64-apple-ios-sim
    cp target/aarch64-apple-ios-sim/release/libaerodesk_ios.a "$DEST/libaerodesk_ios.a"
    ;;
  device)
    build aarch64-apple-ios
    cp target/aarch64-apple-ios/release/libaerodesk_ios.a "$DEST/libaerodesk_ios.a"
    ;;
  all)
    build aarch64-apple-ios-sim
    build aarch64-apple-ios
    cp target/aarch64-apple-ios-sim/release/libaerodesk_ios.a "$DEST/libaerodesk_ios_sim.a"
    cp target/aarch64-apple-ios/release/libaerodesk_ios.a "$DEST/libaerodesk_ios_device.a"
    echo "两个架构已输出（xcframework 后续再合并）"
    ;;
esac
echo "== 完成: $DEST/libaerodesk_ios.a"
