#!/usr/bin/env bash
# 构建 aerodesk-android 静态/动态库并复制到 jniLibs。
# 依赖：NDK（ANDROID_NDK_HOME 或 ~/Library/Android/sdk/ndk）、cargo-ndk。
# 注意：APK 构建（./gradlew assembleDebug）在 exFAT 卷上会因 macOS 生成的
# `._*` AppleDouble 文件导致资源合并失败，建议在 APFS 卷（如 ~/tmp）构建。
set -euo pipefail
cd "$(dirname "$0")/.."

NDK_HOME="${ANDROID_NDK_HOME:-$HOME/Library/Android/sdk/ndk/$(ls -t "$HOME/Library/Android/sdk/ndk" | head -1)}"
SDK_HOME="${ANDROID_HOME:-${ANDROID_SDK_ROOT:-$HOME/Library/Android/sdk}}"
ABI="${ABI:-arm64-v8a}"
TARGET="${TARGET:-aarch64-linux-android}"

# Slint Android 后端的 build.rs 需要 android.jar；优先 ANDROID_JAR，否则从 SDK 推导。
if [ -z "${ANDROID_JAR:-}" ] && [ -d "$SDK_HOME/platforms" ]; then
  PLATFORM="$(ls "$SDK_HOME/platforms" | sort -V | tail -1)"
  ANDROID_JAR="$SDK_HOME/platforms/$PLATFORM/android.jar"
  export ANDROID_JAR
fi
export ANDROID_HOME="$SDK_HOME"

echo "== cargo ndk -t $ABI build -p aerodesk-android --release"
ANDROID_NDK_HOME="$NDK_HOME" cargo ndk -t "$ABI" build --release -p aerodesk-android
DST="android/app/src/main/jniLibs/$ABI"
mkdir -p "$DST"
cp "target/$TARGET/release/libaerodesk_android.so" "$DST/"
echo "== 完成: $DST/libaerodesk_android.so"
echo "构建 APK: cd android && JAVA_HOME=… ANDROID_HOME=… ./gradlew assembleDebug"
