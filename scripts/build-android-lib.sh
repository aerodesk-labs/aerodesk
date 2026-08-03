#!/usr/bin/env bash
# 构建 aerodesk-android 静态/动态库并复制到 jniLibs。
# 依赖：NDK（ANDROID_NDK_HOME 或 ~/Library/Android/sdk/ndk）、cargo-ndk。
set -euo pipefail
cd "$(dirname "$0")/.."

NDK_HOME="${ANDROID_NDK_HOME:-$HOME/Library/Android/sdk/ndk/$(ls -t "$HOME/Library/Android/sdk/ndk" | head -1)}"
ABI="${ABI:-arm64-v8a}"
TARGET="${TARGET:-aarch64-linux-android}"

echo "== cargo ndk -t $ABI build -p aerodesk-android --release"
ANDROID_NDK_HOME="$NDK_HOME" cargo ndk -t "$ABI" -p aerodesk-android build --release

DST="android/app/src/main/jniLibs/$ABI"
mkdir -p "$DST"
cp "target/$TARGET/release/libaerodesk_android.so" "$DST/"
echo "== 完成: $DST/libaerodesk_android.so"
echo "构建 APK: cd android && JAVA_HOME=… ANDROID_HOME=… ./gradlew assembleDebug"
