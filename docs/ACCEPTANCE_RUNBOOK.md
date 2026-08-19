# 真机验收 Runbook（#4 Linux 适配器 / #75 鼠标控制）

> 目标：在真实桌面/多显示器环境下，用可复现的命令完成 #4（Linux 被控端+观看端）
> 与 #75（远程光标/输入注入/坐标映射/剪贴板）验收，并输出可核对的证据。
> 服务器部署/打包已就绪（见 docs/DEPLOYMENT.md、PACKAGING.md）；本文件只做真机验收。

## 0. 准备

| 项 | 要求 |
|---|---|
| Linux 被控端 | 桌面真机（X11 会话；Wayland 会话另见 1.2）；可选 USB/V4L2 摄像头 |
| 观看端 | Windows/macOS/Linux 任意（CLI 或 UI）；推荐与服务器异地网络 |
| 服务器 | SFU + signal 已部署（含 TURN），端口/认证见部署实例 |
| 网络 | 本机关闭代理/TUN（Clash 等）或加 DIRECT 规则；安全组放行 signal/媒体端口 |
| 构建 | `cargo build --release -p aerodesk-agent -p aerodesk-desktop`（依赖见 CI System deps），或用 pre-release 包 |

常用连接参数（实测示例，按部署替换）：

```text
SIGNAL=ws://<host>:<port>/ws          # 例：ws://129.226.150.174:14703/ws
TOKEN=<token>                          # AUTH_TOKENS 之一；无认证可省略
ROOM=accept-<你的标记>
# 媒体：UDP/TCP <media_port>（例 14778）；TURN <turn_port>（例 14779）
```

## 1. Linux 被控端验收（#4）

### 1.1 X11 会话（必做）

```bash
# 1) 服务器（若未部署，本地起一套）
RECORD_DIR=/tmp/rec ./target/release/aerodesk-sfu &
./target/release/aerodesk-signal &

# 2) 被控端（X11 桌面会话；DISPLAY 必须指向真实桌面）
DISPLAY=:0 ./target/release/aerodesk-agent \
  --role publisher --encoder screen \
  --signal "$SIGNAL" --room "$ROOM" --token "$TOKEN" --audio \
  [--camera --camera-device /dev/video0]

# 3) 观看端（另一台机器或本机第二个进程）
./target/release/aerodesk-agent \
  --role viewer --signal "$SIGNAL" --room "$ROOM" --token "$TOKEN" --audio
```

**断言（看 viewer/publisher 日志）**：

| # | 项 | 通过条件 | 证据 |
|---|---|---|---|
| 1.1a | 视频 | viewer `RECEIVED:` 帧数持续增长、`DECODED:` >0 | 日志 |
| 1.1b | 音频 | viewer `AUDIO: ... played=N` 增长 | 日志 |
| 1.1c | 远程光标 | viewer `CURSOR: x=.. y=..` 出现且随被控端鼠标移动变化 | 日志 |
| 1.1d | 输入注入 | 观看端移动/点击/滚轮/按键 → 被控端真实桌面响应；publisher `inject: seq=N` | 日志+桌面观察 |
| 1.1e | 剪贴板文本/图片 | 观看端复制 → 被控端粘贴成功；反向同理 | 实测 |
| 1.1f | 保持唤醒 | 长会话（>15min）不熄屏/不休眠 | 实测 |
| 1.1g | 远程命令 | 观看端 `--run-command 'ls /tmp'` 返回被控端结果 | 日志 |
| 1.1h | 摄像头（可选） | `--list-cameras` 列出设备；`--camera` 后 viewer `CAMERA:` 计数增长 | 日志 |

### 1.2 Wayland 会话（可选，若真机有 Wayland 桌面）

```bash
# 无 DISPLAY 时 --encoder screen 走 xdg-desktop-portal ScreenCast
./target/release/aerodesk-agent --role publisher --encoder screen \
  --signal "$SIGNAL" --room "$ROOM" --token "$TOKEN"
```
- 首次会弹 portal 授权（屏幕/输入）→ 允许
- 注入走 portal RemoteDesktop；若不可用回退 uinput（需 root/udev 规则）
- 断言同 1.1a–1.1d

### 1.3 VAAPI 硬编/硬解（可选）

- publisher 日志应出现 VAAPI 编码器选择（`vaapi`）；viewer 日志出现 VAAPI 解码器
- 无 `/dev/dri` 或驱动缺失时回退软编/软解 **不算失败**，但需在证据里注明

### 1.4 #4 关闭条件（全部勾选）

- [ ] 1.1a 视频端到端（X11 发布 → 观看）
- [ ] 1.1d 键鼠/滚轮/修饰键/拖拽注入生效
- [ ] 1.1e 剪贴板双向（文本；图片可选）
- [ ] 1.1g 远程命令执行
- [ ] Wayland portal 采集+注入（可选）
- [ ] VAAPI 硬编/硬解（可选）
- [ ] 摄像头第二路轨（可选）
- 证据：日志/截图 → 关闭 #4

## 2. 鼠标控制验收（#75）

### 2.1 远程光标跟随

- 被控端移动鼠标 → 观看端叠加光标实时跟随
- 数值对照：被控端 `xdotool getmouselocation`（Linux）/`GetCursorPos`（Windows）与 viewer `CURSOR:` 归一化坐标换算一致（允许 ±0.02）

### 2.2 输入回传 e2e（自动）

```bash
bash scripts/input-e2e.sh   # MouseMove(0.3,0.4)/Button/Wheel/Key/修饰键坐标值断言
```

### 2.3 坐标映射（多显示器/高 DPI）

- 被控端双显示器（不同分辨率/DPI，一横一竖最佳）
- 观看端点击/拖动跨屏位置 → 被控端光标落在对应显示器同一点（验证 #403 归一化 + 各平台 display_rect 映射）
- 参考实现：Windows `WindowsCursor`（virtual screen + active display）、Linux `LinuxCursor`（root window）、macOS `MacCursor`（主屏）

### 2.4 平台注入矩阵（真机）

| 平台 | 注入 | 光标源 | 验收证据 |
|---|---|---|---|
| macOS | CGEvent | MacCursor | 已通（#79/#86） |
| Windows | SendInput | WindowsCursor（#406） | 真机对照 GetCursorPos + `inject:` 日志 |
| Linux | XTest/uinput/portal | LinuxCursor（#392） | 真机对照 xdotool + `inject:` 日志 |
| Android | AccessibilityService | —（待真机） | 真机 |

### 2.5 剪贴板双向（#75 关联）

观看端复制 → 被控端粘贴；被控端复制 → 观看端粘贴（文本；图片按平台支持）

### 2.6 #75 关闭条件（全部勾选）

- [ ] 远程光标渲染 + 跟随（至少一个被控端平台真机）
- [ ] 高 DPI/多分辨率坐标映射（至少一个多显示器真机）
- [ ] 修饰键组合/拖拽/滚轮真机生效（macOS 已通；Windows/Linux 真机补齐）
- [ ] Windows/Linux/Android 注入真机可用
- 证据：日志/截图/input-e2e PASS → 关闭 #75

## 3. 常见问题

| 现象 | 原因 | 处理 |
|---|---|---|
| 所有端口 TCP 通但 0 字节 | 本机代理/TUN（Clash 等）接管流量 | 关闭 TUN 或加 `IP-CIDR,<服务器IP>/32,DIRECT` |
| signal 连不上（握手失败） | 端口不对/安全组未放行 | 用部署实际端口（默认 3003，本部署 14703）；安全组放行 TCP |
| ICE connected 但 0 帧 | UDP 媒体端口未放行，或 SFU 未设公网通告地址 | 安全组放行 **UDP** <media_port>；SFU 设 `SFU_HOST_ADDRESS=<公网IP>` + `SFU_BIND_ADDRESS=0.0.0.0` |
| 被控端无画面 | 无头/服务会话无桌面输出 | 真实桌面会话；X11 需 DISPLAY |
| 摄像头枚举为空 | 无 /dev/video* 或权限 | 插摄像头 + 用户加入 video 组 |

## 4. 关联

- 代码现状：#4/#75 功能已全部合入（#282–#414 批次）
- 本 runbook 只差真机执行；完成后按 1.4 / 2.6 勾选并在对应 issue 附证据关闭
