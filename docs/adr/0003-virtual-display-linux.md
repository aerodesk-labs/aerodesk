# ADR-0003：Linux 虚拟显示器集成设计（VKMS + KDE krfb-virtualmonitor）

- 状态：已采纳（设计稿，2026-08-08；实现待 Linux 真机验收）
- 关联 Issue：#114（调研）、#4（Linux 适配器）、#109（AI 远控）、#75（鼠标控制）
- 上游决策：ADR-0001（Windows 先行）；本文档为 Linux 落地设计

## 背景

Linux 被控端目前直接捕获物理屏（X11 x11rb GetImage / Wayland portal）。远程会话需要独立输出面：
无头服务器没有显示器、捕获物理屏干扰本机操作、AI 远控（#109）需要稳定虚拟输出。

## 候选对比

| 方案 | 性质 | 输出层级 | 集成成本 | 结论 |
|---|---|---|---|---|
| **VKMS（内核自带）+ KWin 第二 DRM** | 内核模块 `modprobe vkms` | DRM/KMS 级虚拟输出 | 中：模块加载 + KWIN_DRM_DEVICES | **选定主线** |
| **krfb-virtualmonitor**（KDE Plasma 6） | KDE 自带工具 | **compositor 级**（非 DRM/KMS） | 低：子进程 `--name/--resolution/--port` | 与 VKMS 配合（KDE 场景直接可用） |
| EVDI（DisplayLink 开源内核模块） | 第三方内核模块 | DRM/KMS | 中高（dkms 编译/签名） | 备选 |
| Xvfb + xrandr | X 虚拟帧缓冲 | X 级 | 低 | 兜底（非 Wayland） |

关键事实（已核对 KDE Discuss/文档）：
- VKMS：`modprobe vkms`（需 root/模块可用）；作为**第二 DRM 设备**（如 `/dev/dri/card1`）提供虚拟输出
- KWin：`KWIN_DRM_DEVICES="/dev/dri/card1"` 让 KWin 使用指定设备；**列表第一个设备用于渲染**，
  一般把真实 iGPU（card0）放前面做渲染、VKMS（card1）做虚拟输出，避免虚拟输出拖慢渲染
- krfb-virtualmonitor：`krfb-virtualmonitor --name VD1 --resolution 1920x1080 --scale 1 --password <pw> --port 5900`
  （KDE Plasma 6 / Wayland）；输出在 **compositor 级**，DRM/KMS 采集看不到它——
  Sunshine 场景需 `capture=kwin`；我们走 PipeWire 采集虚拟输出同理需 compositor 通路
- kscreen-doctor 可配置虚拟输出模式：`kscreen-doctor output.<name>.mode.<id> output.<name>.scale.<s>`

## 集成设计（aerodesk-linux `vdd` 模块，未来实现）

```
VddManager (aerodesk-linux)
 ├─ new()       : 探测环境：
 │                ├─ KDE Plasma 6 / Wayland → krfb-virtualmonitor 可用（主路径）
 │                ├─ X11 → Xvfb 兜底
 │                └─ 其它 Wayland → VKMS + KWIN_DRM_DEVICES 说明（需要会话重启使 env 生效）
 ├─ add(w,h,hz) : 子进程 krfb-virtualmonitor --name aerodesk-<session>
 │                --resolution <W>x<H> --scale 1 --port <随机/固定>（VNC 端口仅内部用）
 │                → 轮询 kscreen-doctor / Wayland 输出列表确认虚拟输出出现
 ├─ remove()    : 终止子进程（SIGTERM → 等待 → SIGKILL 兜底），虚拟 monitor 自动消失
 └─ Drop        : 会话结束清理
```

- 采集联动：
  - KDE/compositor 路径：PipeWire（xdg-desktop-portal）从虚拟输出采集（类似物理屏，按输出名选择）
  - X11 兜底：Xvfb 虚拟屏 + x11rb GetImage（现有采集器直接可用，按 DISPLAY 选择）
- 会话集成：被控端会话建立时 `new() + add(3840,2160,60)`（默认 4K60，可配置），结束 `Drop`
- 错误处理：vkms 未加载/无 root → 明确提示 `modprobe vkms` 与 `sudoers`/systemd 方案；
  krfb-virtualmonitor 不在（非 KDE）→ 明确报错并建议回退；不静默降级（吸取 #11 教训）
- 权限：`modprobe vkms` 需要 root——部署文档给两种方式：systemd 服务预加载，或 `sudoers` 白名单
  （`aerodesk ALL=(root) NOPASSWD: /sbin/modprobe vkms`）

## 风险与缓解

| 风险 | 缓解 |
|---|---|
| VKMS 需 root 加载模块 | systemd 预加载 / sudoers 白名单；文档明示 |
| `KWIN_DRM_DEVICES` 需会话重启生效 | 文档要求重启会话后验收；实现时检测 env 并提示 |
| krfb-virtualmonitor 依赖 KDE Plasma 6 | 探测 DE；GNOME/其它回退 Xvfb 或 VKMS-only 说明 |
| 虚拟输出采集路径依赖 compositor | KDE 走 PipeWire（portal）；文档给出 Sunshine `capture=kwin` 类比 |
| 无头服务器无 GPU 渲染 | KWin 需加速设备渲染；VKMS 仅作第二输出，渲染走真实 GPU/llvmpipe（文档说明） |

## 与 #109 / #75 联动

- #109：虚拟屏是 AI"看得见"的稳定输出面；Linux 虚拟屏接入后，#109 会话内接线纳入里程碑
- #75：虚拟输出避免把物理屏光标钉住

## 验收（未来，需 Linux 真机：KDE Plasma 6 / Wayland）

- [ ] KDE 会话内创建虚拟 monitor → PipeWire 采集虚拟输出 → 远端可观看；会话结束虚拟 monitor 消失
- [ ] X11 回退：Xvfb 虚拟屏可被现有 x11rb 采集
- [ ] vkms 未加载/权限不足时给出明确错误，不 panic 不残留
- [ ] 4K60 虚拟屏采集端到端可用（配合 #8 验收）
