# Windows 主控侧一键互控冒烟(#487)：起 signal+SFU（已在运行则复用）→ CLI viewer
# 连 Mac 被控 ID → 轮询收帧断言（60s 窗口）。Mac 侧部署见
# docs/WIN_MAC_INTEROP_SMOKE_RUNBOOK.md。
# 用法：powershell -ExecutionPolicy Bypass -File scripts/winmac-interop-smoke.ps1 -MacId <ID>
param(
  [Parameter(Mandatory=$true)][string]$MacId,      # Mac 端左栏 ID（房间名）
  [string]$Token = "",                             # 信令 JWT（开启鉴权时必填）
  [string]$Bin = "target\debug\aerodesk-agent.exe",  # cli 路径
  [string]$Sfu  = "target\debug\aerodesk-sfu.exe",
  [string]$Sig  = "target\debug\aerodesk-signal.exe",
  [int]$TimeoutSec = 60                            # 收帧等待窗口
)
$ErrorActionPreference = 'Stop'
cd (Split-Path $PSScriptRoot -Parent)

function Test-TcpPort([int]$Port) {
  # 回环连接：被占用即视为服务在运行；拒绝连接立即返回 false。
  $c = [System.Net.Sockets.TcpClient]::new()
  try { $c.Connect('127.0.0.1', $Port); $true } catch { $false } finally { $c.Close() }
}

# 服务复用：本机 signal(TCP 3003)+SFU(TCP 3478) 已在运行时不再重复起
# （重复起只会 bind 失败静默退出，且 finally 误停用户常驻服务）。
$sfuJob = $null; $sigJob = $null
if ((Test-TcpPort 3003) -and (Test-TcpPort 3478)) {
  Write-Host "signal/SFU 已在运行，复用（不起新实例）"
} else {
  $sfuJob = Start-Job { param($p) $env:SFU_BIND_ADDRESS='0.0.0.0'; & $p *> "$env:TEMP\interop-sfu.log" } -ArgumentList $Sfu
  $sigJob = Start-Job { param($p) & $p *> "$env:TEMP\interop-sig.log" } -ArgumentList $Sig
  Start-Sleep 3
  if (-not (Test-TcpPort 3003)) {
    Write-Host "FAIL: signal 未就绪（3003 不可连），日志：$env:TEMP\interop-sig.log"
    if ($sfuJob) { Stop-Job $sfuJob; Remove-Job $sfuJob -Force }
    if ($sigJob) { Stop-Job $sigJob; Remove-Job $sigJob -Force }
    exit 1
  }
}

# viewer 长跑不退出（每 2s 打一行 RECEIVED 统计）——起后台进程 + 轮询日志，
# 窗口内见到非零帧即 PASS（与 scripts/audio-e2e.sh 同一模式）。
$vlog = "$env:TEMP\interop-viewer.log"
$vout = "$env:TEMP\interop-viewer.out.log"
Remove-Item $vlog, $vout -ErrorAction SilentlyContinue
$vargs = @('--role','viewer','--signal','ws://127.0.0.1:3003','--room',$MacId)
if ($Token) { $vargs += @('--token',$Token) }
$viewer = Start-Process -FilePath $Bin -ArgumentList $vargs -NoNewWindow -PassThru `
  -RedirectStandardError $vlog -RedirectStandardOutput $vout
try {
  $frames = 0; $connected = $false; $fatal = $false
  foreach ($i in 1..$TimeoutSec) {
    Start-Sleep 1
    if ($viewer.HasExited -and -not (Test-Path $vlog)) { $fatal = $true; break }
    if (Test-Path $vlog) {
      $txt = Get-Content $vlog -Raw -ErrorAction SilentlyContinue
      if ($txt -match 'connect failed|connect TIMEOUT|session error') { $fatal = $true; break }
      if ($txt -match 'RECEIVED: (\d+) frames') { $connected = $true; $frames = [int]$Matches[1] }
      if ($frames -gt 0) { break }
    }
  }
  if ($frames -gt 0) {
    Write-Host "PASS: Mac 被控收流 $frames 帧 —— 矩阵 #1/#2 画面列可填 ✓"
  } elseif ($fatal) {
    Write-Host "FAIL: viewer 建链失败（检查 signal 地址/令牌；日志 $vlog）"
  } elseif ($connected) {
    Write-Host "FAIL: 已连接但 ${TimeoutSec}s 内 0 帧（Mac 端是否开被控/已授权/房间 ID 是否一致）"
  } else {
    Write-Host "FAIL: viewer 无任何输出（检查 $Bin 与 $env:TEMP\interop-sfu.log）"
  }
} finally {
  if (-not $viewer.HasExited) { Stop-Process -Id $viewer.Id -Force -ErrorAction SilentlyContinue }
  if ($sfuJob) { Stop-Job $sfuJob; Remove-Job $sfuJob -Force -ErrorAction SilentlyContinue }
  if ($sigJob) { Stop-Job $sigJob; Remove-Job $sigJob -Force -ErrorAction SilentlyContinue }
}
