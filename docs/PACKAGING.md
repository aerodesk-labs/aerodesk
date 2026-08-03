# 打包签名矩阵（P5 #7）

一套 Slint UI（桌面/移动/Web）+ 平台壳，产出各渠道安装包。

## 平台 → 产物 → 打包方式

| 平台 | 产物 | 工具 | 签名 |
|---|---|---|---|
| macOS | `.app` / `.dmg` | `scripts/package-macos.sh` | Developer ID + 公证（notarytool） |
| Windows | `.exe` 安装器 / MSIX | cargo-bundle / WiX / MSIX 打包 | 代码签名证书（EV 可选） |
| Linux | `.deb` / `.rpm` / AppImage | cargo-deb / fpm / appimage-builder | GPG 签名仓库（可选） |
| Android | `.apk` / `.aab` | Gradle（`android/`） | Play App Signing（上传密钥） |
| iOS | `.ipa` | Xcode Archive（`ios/`） | App Store / TestFlight（开发者证书） |
| HarmonyOS | `.hap` | DevEco Studio | 鸿蒙开发者证书 + Profile |
| Web | HTML/JS 静态站（浏览器原生 WebRTC） | `web/index.html` 路线，无需 Rust/WASM | HTTPS（Caddy/Let's Encrypt） |

## 一键流程（桌面）

```sh
# macOS
scripts/package-macos.sh                       # dist/AeroDesk.app
codesign --force --deep --sign "Developer ID Application: …" dist/AeroDesk.app
xcrun notarytool submit dist/AeroDesk.app --keychain-profile aerodesk --wait

# Linux（在 CI ubuntu 或目标机）
cargo install cargo-deb
cargo deb -p aerodesk-ui --target x86_64-unknown-linux-gnu
```

## Android / iOS（在各自构建机）

- Android：`cd android && ./gradlew assembleDebug`（或 `bundleRelease` 出 aab）
- iOS：`xcodebuild -project ios/AeroDesk.xcodeproj -scheme AeroDesk archive`

## 待接入

- [ ] Web：完善 `web/index.html`（浏览器原生 WebRTC；观看/发布/输入与原生端互通）
- [ ] Windows：MF/NVENC 编码 + WiX/MSIX 打包脚本
- [ ] Linux：cargo-deb/rpm 脚本 + VAAPI 编码
- [ ] HarmonyOS：DevEco 打包（SDK 到位后）
- [ ] CI：发布流水线（tag 触发 + 各平台签名上传）

## 本地构建验证（2026-08-04）
### iOS（模拟器）
```sh
bash scripts/build-ios-lib.sh all
cd ios && xcodegen generate
xcodebuild -project AeroDesk.xcodeproj -scheme AeroDesk -destination 'generic/platform=iOS Simulator' -configuration Debug ARCHS=arm64 ONLY_ACTIVE_ARCH=YES build CODE_SIGNING_ALLOWED=NO
```
### Android（APK）
```sh
export JAVA_HOME=/opt/homebrew/opt/openjdk@17/libexec/openjdk.jdk/Contents/Home  # JDK 26 会触发 Kotlin 插件解析失败
export ANDROID_HOME=~/Library/Android/sdk
cd android && bash gradlew assembleDebug
```
