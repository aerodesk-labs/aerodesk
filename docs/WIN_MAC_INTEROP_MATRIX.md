# Windows ↔ macOS 互控冒烟矩阵(2026-08-17 定调为最高优先级)

## 目标

双向互控全组合可用:Win 主控→mac 被控、mac 主控→Win 被控。先冒烟找出真实断点，再按缺口立项，不凭空补功能。

## 现状盘点(代码层,冒烟前的侦察结论)

| 面 | macOS 侧 | Windows 侧 | 共享层 |
|---|---|---|---|
| 采集 | SCK(capture.rs) | DXGI(capture.rs) | — |
| 编码 | VT(vt_encoder.rs) | FFmpeg h264_mf/x264(encode.rs) | aerodesk-codec |
| 注入 | CGEvent(inject.rs) | SendInput(inject.rs) | input 协议(protocol::input) |
| 音频 | SCK 系统音频(audio.rs/audio_capture.rs) | WASAPI(audio_capture.rs) | — |
| 剪贴板 | core::clipboard(平台 cfg) | 同左 | wire 共享 |
| 文件 | core file_transfer | 同左 | wire 共享 |
| 光标 | cursor.rs | cursor.rs | 归一化坐标协议 |
| 键映射 | keymap.rs(→ macos inject) | vk_for_code | "KeyA" 无关键码共享 |
| 权限流程 | PermissionCard UI(TCC 屏录/辅助功能)+ permissions.rs | 无需 | — |
| 桌面端 | publisher(macos_media)+viewer(generic_viewer+VT decode) | publisher(generic_publisher+DxgiCapturer)+viewer(generic_viewer) | generic_* |

## 冒烟矩阵(执行后填 ✓/✗/现象)

| # | 主控 | 被控 | 画面 | 输入 | 音频(被控→主控) | 剪贴板 | 文件 | 光标 | 备注 |
|---|---|---|---|---|---|---|---|---|---|
| 1 | Win CLI | mac CLI | ? | ? | ? | ? | ? | ? | |
| 2 | Win 桌面 UI | mac 桌面/CLI | ? | ? | ? | ? | ? | ? | |
| 3 | mac CLI | Win CLI | ? | ? | ? | ? | ? | ? | |
| 4 | mac 桌面 UI | Win 桌面/CLI | ? | ? | ? | ? | ? | ? | |

## 已知缺口/疑点(侦察出,待冒烟证实)

1. **macOS UI 观看端无 CI e2e**(CI 只有 Windows/Linux UI e2e viewer)——mac 主控体验无自动验证；
2. **音频跨端解码**:mac SCK 系统音频编码格式 → Win 观看端解码、Win WASAPI → mac 观看端解码，两条链路均未验证；
3. **keymap 修饰键**:mac 主控 Cmd/Win 键跨端映射未验证(keymap 覆盖字母/数字/F 键,修饰键组合存疑)；
4. **跨机组合无任何自动测试**:现有 e2e 全部单平台本机闭环；真·互控需要双端同时在线；
5. macOS 被控 TCC 授权(屏录+辅助功能)真实机器上的首次授权体验未走过。

## 执行方式(按现实条件)

- **Win 侧全链路**:本机即可(被控+主控自环,已多次验证)；
- **mac 侧**:Mac 真机人工 + CI macOS runner 单端闭环；
- **真·互控**:①两台真机(最直接);②CI 双 runner 经公网 signal/SFU 互联(需测试环境可用,后续做)。

## 预期产出

每格 ✓/✗+现象 → 缺口归类建批次 issue(checklist 制)→ 逐批修复 → 矩阵全 ✓ 即互控验收通过。
