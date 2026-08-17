# Windows 主控侧一键互控冒烟(#487)：起 signal+SFU → CLI viewer 连 Mac 被控 ID
# → 收帧断言。Mac 侧部署见 docs/WIN_MAC_INTEROP_SMOKE_RUNBOOK.md。
# 用法：powershell -ExecutionPolicy Bypass -File scripts/winmac-interop-smoke.ps1 -MacId <ID>
param(
  [Parameter(Mandatory=$true)][string]$MacId,      # Mac 端左栏 ID（房间名）
  [string]$Token = "",                             # 信令 JWT（开启鉴权时必填）
  [string]$Bin = "target\debug\aerodesk-cli.exe",  # cli 路径
  [string]$Sfu  = "target\debug\aerodesk-sfu.exe",
  [string]$Sig  = "target\debug\aerodesk-signal.exe"
)
$ErrorActionPreference = 'Stop'
cd (Split-Path $PSScriptRoot -Parent)

$sfuJob = Start-Job { param($p) $env:SFU_BIND_ADDRESS='0.0.0.0'; & $p *> "$env:TEMP\interop-sfu.log" } -ArgumentList $Sfu
$sigJob = Start-Job { param($p) & $p *> "$env:TEMP\interop-sig.log" } -ArgumentList $Sig
try {
  Start-Sleep 3
  $args = @('--role','viewer','--signal','ws://127.0.0.1:3003','--room',$MacId)
  if ($Token) { $args += @('--token',$Token) }
  $out = & $Bin $args 2>&1 | Select-String 'RECEIVED' | Select-Object -Last 1
  if ($out) {
    $frames = [regex]::Match($out.ToString(), 'RECEIVED: (\d+) frames').Groups[1].Value
    if ([int]$frames -gt 0) {
      Write-Host "PASS: Mac 被控收流 $frames 帧 —— 矩阵 #1/#2 画面列可填 ✓"
    } else {
      Write-Host "FAIL: viewer 0 帧（Mac 端是否开被控/已授权/信号可达）"
    }
  } else {
    Write-Host "FAIL: viewer 无输出（检查 Mac 端状态与 $env:TEMP\interop-sfu.log）"
  }
} finally {
  Stop-Job $sfuJob,$sigJob; Remove-Job $sfuJob,$sigJob -Force
}
