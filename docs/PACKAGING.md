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
| Web | WASM 静态站 | `wasm-pack` + slint 后端 | HTTPS（Caddy/Let's Encrypt） |

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

- [ ] Web：str0m 需要 wasm 支持（浏览器 WebSocket + 兼容 DTLS）后接 `wasm-pack`
- [ ] Windows：MF/NVENC 编码 + WiX/MSIX 打包脚本
- [ ] Linux：cargo-deb/rpm 脚本 + VAAPI 编码
- [ ] HarmonyOS：DevEco 打包（SDK 到位后）
- [ ] CI：发布流水线（tag 触发 + 各平台签名上传）
