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
    # #14：单文件无法同时服务模拟器/真机，输出 XCFramework（project.yml 按此链接）。
    # 用绝对路径 + 临时目录输出，避免相对路径/残留旧目录导致的失败。
    ROOT="$(pwd)"
    OUT_XCFRAMEWORK="$ROOT/$DEST/AeroDesk.xcframework"
    TMP_XCFRAMEWORK="$(mktemp -d)/AeroDesk.xcframework"
    # 头文件无需内嵌（工程 HEADER_SEARCH_PATHS 已指向 AeroDeskBridge）。
    # 内存压力下 xcodebuild 偶发瞬时写失败，重试一次。
    for attempt in 1 2; do
      if xcodebuild -create-xcframework \
          -library "$ROOT/target/aarch64-apple-ios-sim/release/libaerodesk_ios.a" \
          -library "$ROOT/target/aarch64-apple-ios/release/libaerodesk_ios.a" \
          -output "$TMP_XCFRAMEWORK" >/tmp/xcframework-build.log 2>&1; then
        break
      fi
      echo "xcodebuild 失败（第 $attempt 次），重试…"
      sleep 2
    done
    grep -q "successfully written" /tmp/xcframework-build.log || { cat /tmp/xcframework-build.log; exit 1; }
    rm -rf "$OUT_XCFRAMEWORK"
    mv -f "$TMP_XCFRAMEWORK" "$OUT_XCFRAMEWORK"
    echo "== 完成: ${OUT_XCFRAMEWORK}（模拟器 + 真机）"
    ;;
esac
echo "== 完成: $DEST"
