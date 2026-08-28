# Windows 显示器切换端到端（#58/#408）：viewer `--display N` → control 通道 →
# publisher 重建采集并同步注入/光标坐标基准。
# 与 windows_runtime 的 switch_display 单测互补：覆盖 CLI 全链路接线。
# #487 回归：同一 loopback 追加「screen 发布端真出帧」断言——CI runner 桌面
# 静态，走 #477 机制 B 心跳（2s 无帧即缓存末帧重编码冲刷 MF 管线）；真机活屏
# 走变化帧。两条路都必须在 viewer 连接后数秒内产出 RECEIVED > 0。
# 采集不可用（headless/服务会话）时 SKIP（exit 0），避免 CI 假红。
# 用法: scripts/display-switch-e2e-win.ps1 [room]
$ErrorActionPreference = 'Stop'
$Root = Split-Path -Parent $PSScriptRoot
Set-Location $Root
$Room = if ($args.Count -gt 0) { $args[0] } else { "disp-switch-$([DateTime]::Now.ToString('HHmmss'))" }
$env:RUST_LOG = if ($env:RUST_LOG) { $env:RUST_LOG } else { "info" }
if (-not $env:FFMPEG_DIR) { throw "FFMPEG_DIR 未设置（CI 已配；本地需指向 FFmpeg 8.1 开发包根目录）" }

$logDir = Join-Path $env:TEMP ("display-switch-e2e-" + [DateTime]::Now.ToString('HHmmss'))
New-Item -ItemType Directory -Force -Path $logDir | Out-Null

function Stop-AeroDesk {
    Get-Process | Where-Object { $_.ProcessName -like 'aerodesk-*' } |
        Stop-Process -Force -ErrorAction SilentlyContinue
}

$sfu = $null; $sig = $null; $pub = $null; $view = $null
try {
    Stop-AeroDesk
    Start-Sleep -Milliseconds 500
    Write-Host "== build"
    cargo build -q -p aerodesk-sfu -p aerodesk-signal -p aerodesk-agent
    Write-Host "== start sfu/signal"
    $env:SFU_BIND_ADDRESS = "127.0.0.1"
    $env:SFU_HOST_ADDRESS = "127.0.0.1"
    $sfu = Start-Process -FilePath ".\target\debug\aerodesk-sfu.exe" -WindowStyle Hidden `
        -RedirectStandardOutput "$logDir\sfu.log" -RedirectStandardError "$logDir\sfu.err" -PassThru
    $sig = Start-Process -FilePath ".\target\debug\aerodesk-signal.exe" -WindowStyle Hidden `
        -RedirectStandardOutput "$logDir\sig.log" -RedirectStandardError "$logDir\sig.err" -PassThru
    Start-Sleep -Seconds 3

    Write-Host "== publisher (screen capture, display 0)"
    $pub = Start-Process -FilePath ".\target\debug\aerodesk-agent.exe" -WindowStyle Hidden `
        -ArgumentList @('--role','publisher','--encoder','screen','--signal','ws://127.0.0.1:3003','--room',$Room,'--display','0') `
        -RedirectStandardOutput "$logDir\pub.log" -RedirectStandardError "$logDir\pub.err" -PassThru
    Start-Sleep -Seconds 6

    $pubTxt = Get-Content "$logDir\pub.err" -Raw -ErrorAction SilentlyContinue
    if ($pubTxt -match "DXGI capture init failed" -or $pubTxt -notmatch "Windows screen capture started") {
        Write-Host "SKIP: DXGI 不可用（headless/服务会话），显示器切换 e2e 跳过（真机/交互会话覆盖）"
        exit 0
    }

    Write-Host "== viewer --display 0 (control channel)"
    $view = Start-Process -FilePath ".\target\debug\aerodesk-agent.exe" -WindowStyle Hidden `
        -ArgumentList @('--role','viewer','--signal','ws://127.0.0.1:3003','--room',$Room,'--display','0') `
        -RedirectStandardOutput "$logDir\view.log" -RedirectStandardError "$logDir\view.err" -PassThru
    Start-Sleep -Seconds 18

    $pubTxt2 = Get-Content "$logDir\pub.err" -Raw -ErrorAction SilentlyContinue
    Write-Host "== 断言"
    $fail = 0
    if ($pubTxt2 -match "display switch -> display 0") {
        Write-Host "PASS publisher display switch -> display 0"
    } else {
        Write-Host "FAIL no display switch log in publisher"
        Get-Content "$logDir\pub.err" -ErrorAction SilentlyContinue | Select-String -Pattern "display switch|error|panic" | Select-Object -Last 5
        $fail = 1
    }
    # #487：screen 发布端 loopback 必须真出帧（本断点曾因子串未断言而不可见）。
    # 轮询兜底信令/ICE 协商慢的情况（健康路径 ≤4s 出首帧；40×0.5s=20s 给
    # runner 负载抖动留足余量）；真零帧（#487 回归）永远等不到，不会掩盖。
    $frames = 0
    foreach ($i in 1..40) {
        $viewTxt = Get-Content "$logDir\view.err" -Raw -ErrorAction SilentlyContinue
        if ($viewTxt -match 'RECEIVED: ([1-9]\d*) frames') { $frames = [int]$Matches[1]; break }
        Start-Sleep -Milliseconds 500
    }
    if ($frames -ge 1) {
        Write-Host "PASS screen publisher loopback frames ($frames, #487)"
    } else {
        Write-Host "FAIL zero frames from screen publisher (#487 regression)"
        # 失败留痕：收发两端都打——viewer 侧此前从不打印（零帧定位盲区）。
        Write-Host "--- view.err tail ---"
        Get-Content "$logDir\view.err" -Tail 10 -ErrorAction SilentlyContinue
        Write-Host "--- pub.err (ICE/帧/编码/心跳相关) ---"
        Get-Content "$logDir\pub.err" -ErrorAction SilentlyContinue |
            Select-String -Pattern "ICE connected|next_frame|encode|heartbeat|GDI|screen capture" |
            Select-Object -Last 10
        $fail = 1
    }
    if (Select-String -Path "$logDir\sfu.log","$logDir\sfu.err","$logDir\pub.err" -Pattern "panic" -Quiet) {
        Write-Host "FAIL panic in logs"; $fail = 1
    }
    exit $fail
}
finally {
    if ($view) { Stop-Process -Id $view.Id -Force -ErrorAction SilentlyContinue }
    if ($pub) { Stop-Process -Id $pub.Id -Force -ErrorAction SilentlyContinue }
    if ($sfu) { Stop-Process -Id $sfu.Id -Force -ErrorAction SilentlyContinue }
    if ($sig) { Stop-Process -Id $sig.Id -Force -ErrorAction SilentlyContinue }
    Stop-AeroDesk
    Write-Host "LOGDIR=$logDir"
}
