# windows-vdd-smoke.ps1 —— Parsec VDD 真机冒烟（ADR-0001 / #140）
#
# 用法（管理员 PowerShell）：
#   .\scripts\windows-vdd-smoke.ps1 -Install          # 安装驱动（需先放好 nefconw）并冒烟
#   .\scripts\windows-vdd-smoke.ps1 1920 1080 60      # 自定义分辨率
#
# 前置：
#   - 下载 parsec-vdd 驱动包（nomi-san/parsec-vdd，含 nefconw.exe）放到 vendor/parsec-vdd/
#   - 先构建示例：cargo build -p aerodesk-platform --release --examples
param(
    [switch]$Install,
    [int]$Width = 3840,
    [int]$Height = 2160,
    [int]$Hz = 60
)
$ErrorActionPreference = "Stop"

function Test-ParsecVdd {
    $dev = Get-PnpDevice -Class Display -ErrorAction SilentlyContinue |
        Where-Object { $_.FriendlyName -like "*Parsec*" }
    return ($null -ne $dev -and $dev.Status -eq "OK")
}

if (-not (Test-ParsecVdd)) {
    if (-not $Install) {
        Write-Host "Parsec VDD 驱动未安装/未就绪；加 -Install 自动安装（需管理员）。" -ForegroundColor Yellow
        exit 2
    }
    $nefconw = Join-Path $PSScriptRoot "..\vendor\parsec-vdd\nefconw.exe"
    if (-not (Test-Path $nefconw)) {
        Write-Host "未找到 $nefconw；请先把 parsec-vdd 驱动包（含 nefconw.exe）放到 vendor/parsec-vdd/" -ForegroundColor Red
        exit 1
    }
    Write-Host "安装 Parsec VDD 驱动（nefconw -i）..."
    Start-Process -FilePath $nefconw -ArgumentList "-i" -Wait -Verb RunAs
    Start-Sleep -Seconds 3
    if (-not (Test-ParsecVdd)) {
        Write-Host "安装后驱动仍不可用：请检查设备管理器/签名，必要时重启。" -ForegroundColor Red
        exit 1
    }
    Write-Host "Parsec VDD 驱动已就绪。" -ForegroundColor Green
} else {
    Write-Host "Parsec VDD 驱动已就绪。" -ForegroundColor Green
}

$exe = Join-Path $PSScriptRoot "..\target\release\examples\vdd_smoke.exe"
if (-not (Test-Path $exe)) {
    Write-Host "未找到示例二进制；请先执行：cargo build -p aerodesk-platform --release --examples" -ForegroundColor Yellow
    exit 3
}
Write-Host "运行 vdd_smoke ${Width}x${Height}@${Hz} ..."
& $exe $Width $Height $Hz
if ($LASTEXITCODE -ne 0) {
    Write-Host "FAIL: vdd_smoke 退出码 $LASTEXITCODE" -ForegroundColor Red
    exit $LASTEXITCODE
}
Write-Host "PASS: 虚拟屏 add/remove 冒烟完成（可再用 aerodesk-agent 会话验证采集）" -ForegroundColor Green
