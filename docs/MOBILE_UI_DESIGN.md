# AeroDesk 移动端 UI 设计（v0.1）

## 1. 目标

为 iOS/iPad、Android、HarmonyOS 设计统一的移动端 UI/UX 骨架。UI 层优先使用
Slint，native 层只保留系统能力垫片（权限、采集、编解码、输入注入、生命周期）。

## 2. 平台约束

| 平台 | Slint 运行时 | 推荐渲染 | native 垫片 |
| --- | --- | --- | --- |
| iOS/iPad | `backend-winit` + software | software | Swift / AVFoundation / VideoToolbox |
| Android | `backend-android-activity-06` + software | software | Kotlin / MediaCodec / MediaProjection / Accessibility |
| HarmonyOS | 暂无官方 Slint backend | ArkTS 先手写，Slint 组件可迁移 | ArkTS / OH_VideoDecoder / AVScreenCapture |

> OHOS：当前没有可用的 Slint backend。设计上仍保持与 Slint 组件同构，先以
> ArkTS 实现，待 Slint 官方或社区 OHOS backend 成熟后切换。

## 3. 信息架构

统一采用「启动页 → 会话页 → 权限/设置页」三块。

### 3.1 启动页（Home）

- 显示设备名 / ID / 在线状态（信令连接状态）
- 服务器地址输入（开发/CI 用）
- 设备输入（当前“房间”的用户语义）
- 动作：
  - 连接观看
  - 开始发布（被控）
  - 权限入口（录屏/摄像头/无障碍）

### 3.2 观看页（Viewer）

- 主区域：视频画布
- 状态条：连接状态、帧率、分辨率、解码方式（Hardware/Software）
- 底部工具栏：
  - 鼠标/触摸模式
  - 键盘
  - 剪贴板
  - 显示器/摄像头切换
  - 断开
- 手势层：触摸 → 归一化坐标 → InputFrame

### 3.3 被控页（Publisher）

- 权限状态：
  - 屏幕采集
  - 摄像头
  - 输入注入
  - 麦克风/系统音频
- 发布状态：码率、帧率、编码方式
- 停止发布

### 3.4 设置页

- 服务器配置
- 编码偏好（H.264 / HEVC）
- 音频开关
- 强制中继（NAT 兜底）
- 关于 / 诊断

## 4. Slint 组件划分

建议抽出共享组件：

```text
mobile-common.slint
├── MobileWindow          # 统一窗口/页面容器
├── StatusPill            # 连接/在线状态胶囊
├── IconButton            # 工具栏按钮
├── SessionVideoSurface   # 视频画布（占位，后续接 Image）
├── ConnectCard           # server/device 输入 + 连接按钮
├── SessionToolbar        # 鼠标/键盘/剪贴板/切换/断开
└── PermissionRow         # 权限开关与状态
```

iOS 和 Android 共用同一份 `mobile-common.slint` 逻辑；平台差异通过 Slint
callback 与 native 垫片隔离。OHOS 当前用 ArkTS 复刻同结构，不强行依赖 Slint。

## 5. 页面流程

```text
Home
 ├─ 连接观看 → Viewer
 │     └─ 断开 → Home
 ├─ 开始发布 → Publisher
 │     └─ 权限缺失 → 系统权限页 → Publisher
 └─ 设置 → Settings → Home
```

## 6. 状态模型

建议在每个端侧 crate 维护一个轻量 `MobileUiState`：

- `screen`: Home | Viewer | Publisher | Settings
- `connection`: Disconnected | Connecting | Connected | Error
- `device_id`: string
- `server`: string
- `room_or_device`: string
- `decoder`: Hardware | Software | Unknown
- `fps / resolution / bitrate`
- `permissions`: screen/camera/input/audio booleans
- `publisher_active`: bool

## 7. Slint callback 接口

```slint
callback connect_viewer(string, string);
callback start_publisher(string, string);
callback disconnect();
callback send_input(string);           // InputFrame JSON
callback toggle_camera(bool);
callback switch_display(int);
callback request_permission(string);    // "screen" | "camera" | "input" | "audio"
callback open_settings();
```

## 8. 平台差异

- iOS/iPad：
  - iPad 使用更宽的工具栏布局；iPhone 使用底部紧凑 toolbar。
  - 默认 SwiftUI 过渡，`-slint` 启用 Slint UI；后续真机验证后切默认。
- Android：
  - `SlintActivity` 已是 launcher。
  - 权限通过 Android 系统 Intent 弹出，Slint 只展示状态与入口。
- HarmonyOS：
  - ArkTS 页面结构与 Slint 组件一一对应。
  - 权限、XComponent 视频渲染由 ArkTS 管理。

## 9. 验收建议

- [ ] 三端能启动进入 Home
- [ ] 观看端能连接并显示视频帧
- [ ] 触摸/鼠标输入可回传
- [ ] 发布端能发起屏幕采集/编码
- [ ] 权限缺失时有明确引导
- [ ] UI 在手机与平板尺寸下可读、可点击

## 10. 本阶段只做设计

本文件为 v0.1 设计稿，先不写 Slint 实现，待设计确认后再进入：
1. 抽 `mobile-common.slint`
2. 接入 iOS/Android 的 Slint 宿主
3. OHOS ArkTS 复刻