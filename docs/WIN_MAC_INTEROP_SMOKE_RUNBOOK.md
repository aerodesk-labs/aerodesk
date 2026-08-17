# Win ↔ macOS 互控真机冒烟手册(#487)

前置:PR #489(macOS 桌面被控)已合入 main;`mac-smoke-build` workflow 已产出
`aerodesk-mac-smoke.zip`(GitHub Actions → Mac smoke build → 最新 run → Artifacts)。

## 一、Mac 侧(笔记本,被控 + 主控两用)

1. 浏览器打开仓库 Actions → **Mac smoke build** → 最新绿 run → 下载
   `aerodesk-mac-smoke` artifact → 解压得到 `AeroDesk.app`
2. 拖入「应用程序」,双击打开(未签名 Developer ID 时:右键 → 打开 → 确认)
3. 首次启动即出现权限引导卡:
   - 「屏幕录制」授权 → 系统设置 > 隐私与安全性 > 屏幕录制 → 勾选 AeroDesk → **重启应用**
   - 「辅助功能」授权(被控时接收键鼠)→ 勾选 AeroDesk
4. 重启后记下左栏 **ID**(如 314159265)与一次性密码 → 打开「被控端」页开启「开启被控」开关
5. 主控测试时无需授权,直接连对方 ID

## 二、Windows 侧(本机,信令 + SFU + 主控/被控两用)

```powershell
# 1. 起服务(若 Mac 与本机同一局域网,Mac 经 ws://<本机LAN IP>:3003/ws 接入)
SFU_BIND_ADDRESS=0.0.0.0 .\target\debug\aerodesk-sfu.exe
.\target\debug\aerodesk-signal.exe
# 2. 记本机 LAN IP: ipconfig → 无线/以太网适配器 IPv4(如 192.168.x.x)
# 3. 防火墙放行(管理员,一次性):
netsh advfirewall firewall add rule name="AeroDesk" dir=in action=allow protocol=TCP localport=3001,3002,3003
netsh advfirewall firewall add rule name="AeroDesk-media" dir=in action=allow protocol=UDP localport=3478
# 4. 主控:桌面 UI 或 CLI:
.\target\debug\aerodesk-cli.exe --role viewer --signal ws://<本机IP>:3003 --room <Mac端ID>
```

## 三、冒烟矩阵(逐格记录 ✓/✗ + 现象)

| # | 主控 | 被控 | 画面 | 输入 | 音频 | 剪贴板 | 文件 | 光标 |
|---|---|---|---|---|---|---|---|---|
| 1 | Win CLI | mac 桌面 | | | | | | |
| 2 | Win 桌面 UI | mac 桌面 | | | | | | |
| 3 | mac 桌面 UI | Win CLI | | | | | | |
| 4 | mac 桌面 UI | Win 桌面 | | | | | | |

操作要点:输入=移动/点击/键入;音频=被控端放歌;剪贴板=双向粘贴文本;
文件=发送小文件核对校验;光标=开「显示远端光标」开关看叠加层。

## 四、结果回填

把表格(或截图/日志)贴到 issue #487 评论区;任何 ✗ 格附现象描述
(黑屏/无光标/无声音/权限弹窗等),即成为下一批修复输入。
