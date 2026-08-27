# 非登录态 VM 实测编排（宿主侧）。用法：管理员 PowerShell 执行
#   powershell -ExecutionPolicy Bypass -File scripts/win-vm-matrix.ps1
# 前置：
#   1. E:\aerodesk-vm\WinDev2407Eval.vhdx 已解压（下载：aka.ms/windev_VM_hyperv）
#   2. E:\aerodesk-vm\bins\ 下已放 aerodesk-host/cli/signal/sfu 四 exe + FFmpeg DLL
#   3. 宿主 signal/sfu 由本脚本自启（防火墙放通 3001/TCP + 5060/UDP 与 3478/UDP+TCP）
#   注：P3 起 signal 为 SIP 单栈——WSS 时代工件，本脚本覆盖部分场景
#   （SIP/UDP 5060 直连；SIP/TLS 5061、SIP/WSS 3061 默认同证书开启）。
#   viewer 的 AERO_SIP_PORT 显式端口提示待 #601 客户端面合并。
# 评估镜像默认凭据（页面文档）：admin / Passw0rd!
$ErrorActionPreference = 'Stop'
$VHDPath = 'E:\aerodesk-vm\WinDev2407Eval.vhdx'
$BinDir  = 'E:\aerodesk-vm\bins'
$VMName  = 'aerodesk-prelogin'

# ---- 0. 宿主侧:对 VM 可达 IP + 防火墙 + signal/sfu 自启 ----
$hostIp = (Get-NetIPAddress -AddressFamily IPv4 | Where-Object { $_.InterfaceAlias -like '*Default Switch*' } | Select-Object -First 1).IPAddress
if (-not $hostIp) { $hostIp = (Get-NetIPAddress -AddressFamily IPv4 | Where-Object { $_.InterfaceAlias -like '*Ethernet*' } | Select-Object -First 1).IPAddress }
Write-Host "宿主对 VM 可达 IP: $hostIp"
foreach ($r in 'AeroDeskMatrix-Signal','AeroDeskMatrix-SFUudp','AeroDeskMatrix-SFUtcp') {
  if (Get-NetFirewallRule -Name $r -ErrorAction SilentlyContinue) { Remove-NetFirewallRule -Name $r }
}
New-NetFirewallRule -Name 'AeroDeskMatrix-Signal' -Direction Inbound -Action Allow -Protocol TCP -LocalPort 3001 | Out-Null
New-NetFirewallRule -Name 'AeroDeskMatrix-SipUdp' -Direction Inbound -Action Allow -Protocol UDP -LocalPort 5060 | Out-Null
New-NetFirewallRule -Name 'AeroDeskMatrix-SipTls' -Direction Inbound -Action Allow -Protocol TCP -LocalPort 5061 | Out-Null
New-NetFirewallRule -Name 'AeroDeskMatrix-SFUudp' -Direction Inbound -Action Allow -Protocol UDP -LocalPort 3478 | Out-Null
New-NetFirewallRule -Name 'AeroDeskMatrix-SFUtcp' -Direction Inbound -Action Allow -Protocol TCP -LocalPort 3478 | Out-Null
# SFU 候选必须通告宿主在 Default Switch 侧的 IP,否则 guest 够不到候选地址
$env:SFU_HOST_ADDRESS = $hostIp
if (-not (Get-NetTCPConnection -LocalPort 3001 -State Listen -ErrorAction SilentlyContinue)) {
  Start-Process -FilePath "$BinDir\aerodesk-signal.exe" -WindowStyle Hidden
  Write-Host '== signal 已自启(ops 3001 / SIP UDP 5060)'
}
if (-not (Get-NetUDPEndpoint -LocalPort 3478 -ErrorAction SilentlyContinue)) {
  Start-Process -FilePath "$BinDir\aerodesk-sfu.exe" -WindowStyle Hidden
  Write-Host '== sfu 已自启(3478, SFU_HOST_ADDRESS=$env:SFU_HOST_ADDRESS)'
}

# ---- 1. 建 VM ----
if (-not (Get-VM -Name $VMName -ErrorAction SilentlyContinue)) {
  New-VM -Name $VMName -Generation 2 -MemoryStartupBytes 4GB -VHDPath $VHDPath -SwitchName (Get-VMSwitch -SwitchType Internal,External,Private | Select-Object -First 1).Name
  Set-VMProcessor -VMName $VMName -Count 4
  Enable-VMIntegrationService -VMName $VMName -Name 'Guest Service Interface'
}
if ((Get-VM -Name $VMName).State -ne 'Running') { Start-VM -Name $VMName }
Write-Host '== 等 VM 启动(首次需数分钟) ...'

# ---- 2. 等 PowerShell Direct 就绪（Guest Service Interface 上线） ----
$cred = New-Object PSCredential('admin', (ConvertTo-SecureString 'Passw0rd!' -AsPlainText -Force))
$ok = $false
foreach ($i in 1..60) {
  Start-Sleep 5
  try {
    $r = Invoke-Command -VMName $VMName -Credential $cred -ScriptBlock { hostname } -ErrorAction Stop
    $ok = $true; Write-Host "guest 就绪: $r"; break
  } catch { Write-Host "  等 guest ($i/60)..." }
}
if (-not $ok) { throw 'guest 未在 5 分钟内就绪' }

# ---- 3. 拷二进制进 VM ----
Invoke-Command -VMName $VMName -Credential $cred -ScriptBlock {
  New-Item -ItemType Directory -Force -Path C:\aerodesk | Out-Null
}
Copy-VMFile -VMName $VMName -SourcePath "$BinDir\*" -DestinationPath 'C:\aerodesk' -CreateFullPath -FileSource Host

# ---- 4. guest 内安装服务 + 配置指向宿主 signal(hostIp 见第 0 节) ----
Invoke-Command -VMName $VMName -Credential $cred -ScriptBlock {
  param($sig)
  Set-ExecutionPolicy Bypass -Scope Process -Force
  Set-ItemProperty -Path 'HKLM:\SOFTWARE\Microsoft\Windows NT\CurrentVersion\Winlogon' -Name AutoAdminLogon -Value 0
  # 服务配置：frame_source=auto（矩阵 A 直抓 → 失败自回退 helper（矩阵 B））
  $cfg = @{ server = "ws://${sig}:5060"; device_id = 'vm-prelogin'; auto_publish = $true; frame_source = 'auto'; helper_port = 0 } | ConvertTo-Json
  New-Item -ItemType Directory -Force -Path C:\ProgramData\AeroDesk | Out-Null
  Set-Content -Path C:\ProgramData\AeroDesk\service-settings.json -Value $cfg -Encoding UTF8
  cd C:\aerodesk
  .\aerodesk-host.exe --install-service
} -ArgumentList $hostIp
Write-Host '== 服务已安装，准备重启到登录界面'
Restart-VM -VMName $VMName -Force

# ---- 5. 等 VM 回到登录界面，服务在线（P0 验收 + 矩阵 A/B 数据源） ----
Start-Sleep 20
Write-Host '== 重启完成：检查服务日志（登录界面阶段的矩阵 A/B 结论在这里）'
Start-Sleep 40
Invoke-Command -VMName $VMName -Credential $cred -ScriptBlock {
  if (Test-Path C:\ProgramData\AeroDesk\logs\service.log) {
    Get-Content C:\ProgramData\AeroDesk\logs\service.log -Tail 40
  } else {
    '无 service.log——服务未在登录界面启动，检查 sc query AeroDeskService'
    sc.exe query AeroDeskService
  }
}
Write-Host '== 宿主侧断言：signal 日志应见 vm-prelogin 设备 join（P0 在线）——请核对'
Write-Host '== 宿主侧 viewer 收帧断言（矩阵 A/B 画面结论）：'
Write-Host "   $BinDir\aerodesk-agent.exe --role viewer --signal ws://${hostIp}:5060 --room vm-prelogin"
