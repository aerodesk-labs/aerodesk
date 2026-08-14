# 打包签名矩阵（P5 #7）

一套 Slint UI（桌面/移动/Web）+ 平台壳，产出各渠道安装包。

## 平台 → 产物 → 打包方式

| 平台 | 产物 | 工具 | 签名 |
|---|---|---|---|
| macOS | `.app` / `.dmg` | `scripts/package-macos.sh` | Developer ID + 公证（notarytool） |
| Windows | 便携 ZIP（已接入）/ `.exe` 安装器 / MSIX（待补） | `scripts/package-windows.sh`（exe + FFmpeg DLL）/ WiX / MSIX | 代码签名证书（EV 可选） |
| Linux | `.deb` / `.rpm` / AppImage / 便携 `tar.gz` | cargo-deb / rpmbuild / linuxdeploy | GPG 签名仓库（可选） |
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
bash scripts/package-linux.sh      # dist/aerodesk_<版本>_amd64.deb + tar.gz + rpm + AppImage
```

## Android / iOS（在各自构建机）

- Android：`cd android && ./gradlew assembleDebug`（或 `bundleRelease` 出 aab）
- iOS：`xcodebuild -project ios/AeroDesk.xcodeproj -scheme AeroDesk archive`

## 发布流水线（2026-08 接入）

`.github/workflows/release.yml`（参考 ../abb 项目的 Build & Release 流水线）：

- **触发**：打 `v*` tag 自动构建并发布 GitHub Release；`workflow_dispatch` 手动触发只出产物
- **macOS**：`cargo build --release --target aarch64-apple-darwin -p aerodesk-ui`
  → `scripts/assemble-macos-app.sh` 组装 `.app`（版本号取自 Cargo.toml、图标 `app-assets/AppIcon.icns`）
  → Developer ID 签名（临时 keychain，secrets：`APPLE_CERT_P12`/`APPLE_CERT_PASSWORD`/`APPLE_TEAM_ID`）
  → notarytool 公证 + stapler 装订 → DMG（拖拽安装）→ DMG 签名/公证/装订 → 上传 Release
- **Linux**：`cargo build --release -p aerodesk-ui` → `scripts/package-linux.sh`
  → `cargo-deb` `.deb`（`depends=$auto` 自动探测）+ 便携 `tar.gz` + `rpmbuild` `.rpm` + `linuxdeploy` AppImage
  → 上传 Release（无签名，GPG 签名仓库可选）
- **Windows**：`cargo build --release -p aerodesk-ui -p aerodesk-cli` → `scripts/package-windows.sh`
  → 便携 ZIP（aerodesk-ui.exe + aerodesk-cli.exe + FFmpeg 共享 DLL + 图标 + README）
  → 上传 Release（无签名；WiX/MSIX 安装器待补）
- **所需 secrets**：`APPLE_CERT_P12`（Developer ID Application 证书 base64）、`APPLE_CERT_PASSWORD`、
  `APPLE_TEAM_ID`、`APPLE_ID`、`APPLE_APP_PASSWORD`
- **Windows**：aerodesk-ui 非 macOS 分支已可观看（generic viewer），WiX/MSIX 打包 job 待补

本地打包（不签名/公证，自测用）：

```sh
scripts/package-macos.sh           # dist/AeroDesk.app
scripts/package-macos.sh --dmg    # 额外产出 dist/AeroDesk-<版本>.dmg
scripts/package-windows.sh       # dist/aerodesk-<版本>-win64.zip（需 FFMPEG_DIR + release 构建）
```

## 待接入

- [ ] Web：完善 `web/index.html`（浏览器原生 WebRTC；观看/发布/输入与原生端互通）
- [x] Windows：便携 ZIP 打包脚本 + release job（#7/PACKAGING 补齐）；WiX/MSIX 安装器待补
- [x] Linux：deb/tar.gz/rpm/AppImage 已接入 release job（#293/#312）
- [ ] HarmonyOS：DevEco 打包（SDK 到位后）

## 本地构建验证（2026-08-04）
### iOS（模拟器 + 真机 slice）
```sh
bash scripts/build-ios-lib.sh all   # 构建 sim/device 两个 Rust 静态库并合成 XCFramework
cd ios && xcodegen generate          # 生成 AeroDesk.xcodeproj（gitignore 生成物，pull 后必须重新生成）
xcodebuild -project AeroDesk.xcodeproj -scheme AeroDesk -destination 'generic/platform=iOS Simulator' -configuration Debug ARCHS=arm64 ONLY_ACTIVE_ARCH=YES build CODE_SIGNING_ALLOWED=NO
xcodebuild -project AeroDesk.xcodeproj -scheme AeroDesk -destination 'generic/platform=iOS' -configuration Debug ARCHS=arm64 ONLY_ACTIVE_ARCH=YES build CODE_SIGNING_ALLOWED=NO
```
> 注意：`ios/AeroDesk.xcodeproj` 与 `ios/AeroDeskBridge/lib/` 均为构建产物（gitignore）。
> 拉取新代码后务必先重新执行 `build-ios-lib.sh all` + `xcodegen generate`，否则会链接到过期的扁平 `.a`
> （报错 `built for 'iOS-simulator'` 之类，属 #14 同源回归）。两条 slice 构建已纳入 CI（iOS app build job）。
### Android（APK）
```sh
export JAVA_HOME=/opt/homebrew/opt/openjdk@17/libexec/openjdk.jdk/Contents/Home  # JDK 26 会触发 Kotlin 插件解析失败
export ANDROID_HOME=~/Library/Android/sdk
bash scripts/build-android-lib.sh      # 先构建 Rust .so（需 ANDROID_NDK_HOME 或 ~/Library/Android/sdk/ndk）
cd android && bash gradlew assembleDebug
```
> 注意：Gradle 在 **exFAT 卷**上会因 macOS 生成的 `._*` AppleDouble 文件导致资源合并/清理失败，
> 请在 APFS 卷（如 `~/tmp`）构建。Rust .so + APK 两条构建已纳入 CI（Android APK build job）。
