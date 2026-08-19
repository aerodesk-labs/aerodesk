# Windows Edge 观看端端到端（#3 Windows→Web 验收 + #75 输入回传回归）。
# 与 CI web-e2e.sh（macOS Chrome）等价：本机/CI Windows 用 Edge（预装）验证
# 浏览器观看端全链路——signaling → ICE/DTLS/SCTP → 视频轨 → 输入回传。
# 依赖：cargo 已构建 sfu/signal/cli（脚本内也 build -q）、FFMPEG_DIR（合成源编码）、
#       Edge 预装、Node + playwright-core（脚本自动 npm i）。
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

$sfu = $null; $sig = $null; $pub = $null
try {
    Stop-AeroDesk
    Start-Sleep -Milliseconds 500
    Write-Host "== build"
    cargo build -q -p aerodesk-sfu -p aerodesk-signal -p aerodesk-agent
    Write-Host "== start sfu/signal"
    # 本机/CI Windows runner 自动选网卡可能 bind 失败（AddrNotAvailable），强制回环。
    $env:SFU_BIND_ADDRESS = "127.0.0.1"
    $env:SFU_HOST_ADDRESS = "127.0.0.1"
    $sfu = Start-Process -FilePath ".\target\debug\aerodesk-sfu.exe" -WindowStyle Hidden `
        -RedirectStandardOutput "$logDir\sfu.log" -RedirectStandardError "$logDir\sfu.err" -PassThru
    $sig = Start-Process -FilePath ".\target\debug\aerodesk-signal.exe" -WindowStyle Hidden `
        -RedirectStandardOutput "$logDir\sig.log" -RedirectStandardError "$logDir\sig.err" -PassThru
    Start-Sleep -Seconds 3
    # 合成源发布端（静态桌面不影响出帧；Windows 无 x264 用 ffmpeg/h264）
    $pub = Start-Process -FilePath ".\target\debug\aerodesk-agent.exe" -WindowStyle Hidden `
        -ArgumentList @('--role','publisher','--encoder','ffmpeg','--codec','h264','--signal','ws://127.0.0.1:3003','--room',$Room) `
        -RedirectStandardOutput "$logDir\pub.log" -RedirectStandardError "$logDir\pub.err" -PassThru
    Start-Sleep -Seconds 4

    Write-Host "== playwright-core"
    $e2eDir = Join-Path $env:TEMP "web-edge-e2e"
    New-Item -ItemType Directory -Force -Path $e2eDir | Out-Null
    if (-not (Test-Path "$e2eDir\node_modules\playwright-core")) {
        Push-Location $e2eDir
        npm init -y | Out-Null
        npm i playwright-core | Out-Null
        Pop-Location
    }

    Write-Host "== Edge viewer"
    # node 按脚本所在目录解析 node_modules，用 NODE_PATH 指到 playwright-core。
    Push-Location $e2eDir
    $oldNodePath = $env:NODE_PATH
    $env:NODE_PATH = "$e2eDir\node_modules"
    & node "$Root\scripts\web-edge-e2e-run.js" $Room
    $nodeRc = $LASTEXITCODE
    $env:NODE_PATH = $oldNodePath
    Pop-Location

    # #523：输入回传计数改轮询（≤15s）——DataChannel 在视频就绪后可能仍在协商，
    # 固定 sleep 后单次数是竞态；真断链（#75 回归）事件永远不到，轮询不会掩盖。
    $inputHits = 0
    foreach ($i in 1..30) {
        $pubTxt = Get-Content "$logDir\pub.err" -Raw -ErrorAction SilentlyContinue
        if ($pubTxt) { $inputHits = ([regex]::Matches($pubTxt, "input: seq=")).Count }
        if ($inputHits -ge 1) { break }
        Start-Sleep -Milliseconds 500
    }

    Write-Host "== 断言"
    $fail = 0
    if ($nodeRc -eq 0) { Write-Host "PASS Edge video playing" } else { Write-Host "FAIL Edge video"; $fail = 1 }
    if ($inputHits -ge 1) { Write-Host "PASS Edge input events -> publisher ($inputHits)" } else {
        Write-Host "FAIL no input events"
        Get-Content "$logDir\pub.err" -Tail 5 -ErrorAction SilentlyContinue
        $fail = 1
    }
    if (Select-String -Path "$logDir\sfu.log","$logDir\sfu.err","$logDir\pub.err" -Pattern "panic" -Quiet) { Write-Host "FAIL panic in logs"; $fail = 1 }
    exit $fail
}
finally {
    if ($pub) { Stop-Process -Id $pub.Id -Force -ErrorAction SilentlyContinue }
    if ($sfu) { Stop-Process -Id $sfu.Id -Force -ErrorAction SilentlyContinue }
    if ($sig) { Stop-Process -Id $sig.Id -Force -ErrorAction SilentlyContinue }
    Stop-AeroDesk
    Write-Host "LOGDIR=$logDir"
}
