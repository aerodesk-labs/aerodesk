#!/usr/bin/env bash
# 无 OHOS NDK 时的交叉编译自检通道（cargo check 专用）。
#
# 用法：
#   bash crates/aerodesk-ohos/tools/cargo-check-ohos.sh
#
# 作用：
#   1) 用 Zig 的 aarch64-linux-musl libc 编译 ring/aws-lc-sys 的 C 源码，
#      让 `cargo check --target aarch64-unknown-linux-ohos` 能跑完；
#   2) 不生成最终 .so（cdylib 链接仍需要 OHOS NDK 的 clang/链接器）。
set -euo pipefail

HERE="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
ROOT="$(cd "$HERE/../../.." && pwd)"
TARGET="aarch64-unknown-linux-ohos"

command -v zig >/dev/null 2>&1 || {
  echo "missing zig; install zig >= 0.16" >&2
  exit 1
}

if ! rustup target list --installed 2>/dev/null | grep -q "$TARGET"; then
  echo "missing Rust target; run: rustup target add $TARGET" >&2
  exit 1
fi

export CC_aarch64_unknown_linux_ohos="$HERE/zig-cc-ohos"
# ring 的 cc-rs 需要 GNU 风格的 ar cq 创建 archive；Apple /usr/bin/ar 已实测可用。
# （Zig 的 ar 需要 archive 已存在，不能直接 cq 创建，故不用。）
export AR_aarch64_unknown_linux_ohos="/usr/bin/ar"
export RANLIB_aarch64_unknown_linux_ohos="/usr/bin/ranlib"

cd "$ROOT"
cargo check -p aerodesk-ohos --target "$TARGET" "$@"
