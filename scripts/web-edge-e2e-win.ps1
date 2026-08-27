# Windows Edge 观看端端到端（#3 Windows→Web 验收 + #75 输入回传回归）。
# #552 迁移后改双浏览器拓扑（2026-08-24）：CLI publisher 已是 SIP 1:1 被叫，
# WSS JSON 面无法对其呼叫（互通缺口待 Web SIP-WSS）——Edge 发布页 + Edge
# 观看页同 WSS 房间闭环：signaling → ICE/DTLS/SCTP → 视频轨 → 输入回传（SFU
# 转发到发布页 log）。浏览器被控端无系统注入，CLI 注入链路由 input-e2e 覆盖。
# 依赖：cargo 已构建 sfu/signal（脚本内也 build -q）、FFMPEG_DIR、Edge 预装、
#       Node + playwright-core（脚本自动 npm i）。
# 用法: scripts/web-edge-e2e-win.ps1 [room]
$ErrorActionPreference = 'Stop'
$Root = Split-Path -Parent $PSScriptRoot
Set-Location $Root
$Room = if ($args.Count -gt 0) { $args[0] } else { "webedge-win-$([DateTime]::Now.ToString('HHmmss'))" }
$env:RUST_LOG = if ($env:RUST_LOG) { $env:RUST_LOG } else { "info" }
if (-not $env:FFMPEG_DIR) { throw "FFMPEG_DIR 未设置（CI 已配；本地需指向 FFmpeg 8.1 开发包根目录）" }

$logDir = Join-Path $env:TEMP ("web-edge-e2e-" + [DateTime]::Now.ToString('HHmmss'))
New-Item -ItemType Directory -Force -Path $logDir | Out-Null

function Stop-AeroDesk {
    Get-Process | Where-Object { $_.ProcessName -like 'aerodesk-*' } |
        Stop-Process -Force -ErrorAction SilentlyContinue
    # 只清理由本脚本/历史崩溃遗留的 headless e2e Edge；用户正在用的窗口实例
    # 命令行不含 --headless，绝不被误杀（本机运行安全前提）。
    Get-CimInstance Win32_Process -Filter "Name='msedge.exe'" -ErrorAction SilentlyContinue |
        Where-Object { $_.CommandLine -like '*--headless*' } |
        ForEach-Object { Stop-Process -Id $_.ProcessId -Force -ErrorAction SilentlyContinue }
}

$sfu = $null; $sig = $null; $http = $null
try {
    Stop-AeroDesk
    Start-Sleep -Milliseconds 500
    Write-Host "== build"
    cargo build -q -p aerodesk-sfu -p aerodesk-signal
    Write-Host "== start sfu/signal"
    # 双浏览器拓扑下 Edge 的 ICE 候选含网卡 IP：SFU 绑 127.0.0.1 时发往网卡 IP
    # 报 WSAENETUNREACH（10051）→ 媒体不通。绑 0.0.0.0 覆盖全接口（Windows
    # runner 有网卡；本机无网卡时 SFU 默认通配绑定同样可回退）。
    $env:SFU_BIND_ADDRESS = "0.0.0.0"
    $env:SFU_HOST_ADDRESS = "127.0.0.1"
    # #552：CLI 客户端走 SIP UDP 面；#598 P2a：浏览器信令走 SIP-WSS 面（3061）。
    $env:SIP_UDP_PORT = "5060"
    $env:SIP_WSS_PORT = "3061"
    $sfu = Start-Process -FilePath ".\target\debug\aerodesk-sfu.exe" -WindowStyle Hidden `
        -RedirectStandardOutput "$logDir\sfu.log" -RedirectStandardError "$logDir\sfu.err" -PassThru
    $sig = Start-Process -FilePath ".\target\debug\aerodesk-signal.exe" -WindowStyle Hidden `
        -RedirectStandardOutput "$logDir\sig.log" -RedirectStandardError "$logDir\sig.err" -PassThru
    # #598 P2a：静态服务 web/（sip-*.html；SFU 内嵌换页在 P4）。
    $env:WEB_SERVE_PORT = "38083"
    $http = Start-Process -FilePath "python" -ArgumentList "-m","http.server","38083" `
        -WorkingDirectory "$Root\web" -WindowStyle Hidden `
        -RedirectStandardOutput "$logDir\http.log" -RedirectStandardError "$logDir\http.err" -PassThru
    Start-Sleep -Seconds 3

    Write-Host "== playwright-core"
    $e2eDir = Join-Path $env:TEMP "web-edge-e2e"
    New-Item -ItemType Directory -Force -Path $e2eDir | Out-Null
    if (-not (Test-Path "$e2eDir\node_modules\playwright-core")) {
        Push-Location $e2eDir
        npm init -y | Out-Null
        npm i playwright-core | Out-Null
        Pop-Location
    }

    Write-Host "== Edge 双浏览器（发布页 + 观看页）"
    # node 按脚本所在目录解析 node_modules，用 NODE_PATH 指到 playwright-core。
    Push-Location $e2eDir
    $oldNodePath = $env:NODE_PATH
    $env:NODE_PATH = "$e2eDir\node_modules"
    & node "$Root\scripts\web-edge-e2e-run.js" $Room
    $nodeRc = $LASTEXITCODE
    $env:NODE_PATH = $oldNodePath
    Pop-Location

    Write-Host "== 断言"
    $fail = 0
    if ($nodeRc -eq 0) { Write-Host "PASS Edge video playing + input relayed" } else { Write-Host "FAIL Edge video/input"; $fail = 1 }
    if (Select-String -Path "$logDir\sfu.log","$logDir\sfu.err","$logDir\sig.err" -Pattern "panic" -Quiet) { Write-Host "FAIL panic in logs"; $fail = 1 }
    exit $fail
}
finally {
    if ($http) { Stop-Process -Id $http.Id -Force -ErrorAction SilentlyContinue }
    if ($sfu) { Stop-Process -Id $sfu.Id -Force -ErrorAction SilentlyContinue }
    if ($sig) { Stop-Process -Id $sig.Id -Force -ErrorAction SilentlyContinue }
    Stop-AeroDesk
    Write-Host "LOGDIR=$logDir"
}
