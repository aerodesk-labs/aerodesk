#!/usr/bin/env bash
# #6 HarmonyOS cross-compile toolchain check.
# Root cause (2026-08): str0m's dimpl dep unconditionally pulls aws-lc-rs ->
# aws-lc-sys (C build); cross-compiling aarch64-unknown-linux-ohos needs the
# OHOS NDK clang/linker (aws-lc-sys has OHOS support; only the toolchain is
# missing). ring likewise needs a C toolchain.
# This script checks the Rust target / NDK env vars and smoke-compiles a tiny C
# program to verify the toolchain.
# Usage: scripts/check-ohos-toolchain.sh
set -u
TARGET="aarch64-unknown-linux-ohos"
CC_VAR="CC_aarch64_unknown_linux_ohos"
AR_VAR="AR_aarch64_unknown_linux_ohos"
RANLIB_VAR="RANLIB_aarch64_unknown_linux_ohos"
fail=0

echo "== 1) Rust target"
if rustup target list --installed 2>/dev/null | grep -q "$TARGET"; then
  echo "  OK: $TARGET installed"
else
  echo "  MISSING: rustup target add $TARGET"; fail=1
fi

echo "== 2) NDK env vars"
cc="$(printenv "$CC_VAR" 2>/dev/null || true)"
ar="$(printenv "$AR_VAR" 2>/dev/null || true)"
ranlib="$(printenv "$RANLIB_VAR" 2>/dev/null || true)"
[ -n "$cc" ] && echo "  OK: $CC_VAR=$cc" || { echo "  MISSING: $CC_VAR (point to OHOS NDK clang)"; fail=1; }
[ -n "$ar" ] && echo "  OK: $AR_VAR=$ar" || { echo "  MISSING: $AR_VAR (point to NDK llvm-ar)"; fail=1; }
[ -n "$ranlib" ] && echo "  OK: $RANLIB_VAR=$ranlib" || { echo "  MISSING: $RANLIB_VAR (point to NDK llvm-ranlib)"; fail=1; }

echo "== 3) C toolchain smoke"
if [ -n "$cc" ]; then
  tmp="$(mktemp -d)"
  printf 'int main(void){return 0;}\n' > "$tmp/t.c"
  if "$cc" --target="$TARGET" -o "$tmp/t" "$tmp/t.c" 2>"$tmp/err"; then
    echo "  OK: tiny C program cross-compiles"
  else
    echo "  FAIL: cross compile failed (missing sysroot or linker)"; head -5 "$tmp/err"; fail=1
  fi
else
  echo "  SKIP: CC unset, no smoke"
fi

echo "== 4) Result"
if [ "$fail" = "0" ]; then
  echo "  Toolchain ready. Run: cargo check -p aerodesk-ohos --target $TARGET"
else
  echo "  Toolchain missing. Install DevEco/OHOS NDK then set:"
  echo "    export PATH=\"<ohos-ndk>/llvm/bin\":\$PATH"
  echo "    export $CC_VAR=\"<ohos-ndk>/llvm/bin/clang\""
  echo "    export $AR_VAR=\"<ohos-ndk>/llvm/bin/llvm-ar\""
  echo "    export $RANLIB_VAR=\"<ohos-ndk>/llvm/bin/llvm-ranlib\""
  echo "    # if needed: export CFLAGS_aarch64_unknown_linux_ohos=\"--sysroot=<ohos-ndk>/sysroot\""
fi
exit "$fail"
