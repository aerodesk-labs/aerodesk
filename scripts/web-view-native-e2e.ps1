# Web 观看页（sip-viewer.html）→ 原生 CLI publisher 方向端到端（#583 剩余工作）。
#
# 方向与既有 e2e 相反：既有脚本覆盖「Web 被控 → 原生/Web 观看」；本脚本覆盖
# 「headless 浏览器观看端（UAC，REGISTER + Digest）→ INVITE 原生 CLI publisher
# （SIP UAS）→ 1:1 P2P 媒体 + 键鼠经 data channel 回传」。该方向此前无覆盖。
#
# 参照：
#   scripts/web-pub-e2e.sh        —— 本方向的镜像（浏览器被控、CLI 观看）；本脚本方向对调。
#   scripts/web-edge-e2e-win.ps1  —— Windows 原生 web e2e 样板（进程/清理/日志组织沿用）。
#   docs/web-sip-wss-design.md §4.1 —— 候选面实测坑与配方。
#
# 【CI 接入】本脚本暂未加入 .github/workflows/ci.yml。主路径 --encoder screen 需要交互桌面
# 会话，外加预装 Edge + npm 拉 playwright-core——与既有 Windows job「e2e web viewer
# (Windows Edge)」（scripts/web-edge-e2e-win.ps1）同环境。接入即在 ci.yml 的 Windows 段追加：
#     - name: e2e web viewer -> native publisher (Windows)
#       if: runner.os == 'Windows'
#       shell: pwsh
#       run: ./scripts/web-view-native-e2e.ps1
# 本批未提交该 job 的理由：walgit 侧无法触发 GitHub Actions，提交一个未经 CI 实跑的 job 会
# 绕过「发版节点全绿」口径；是否纳入 Windows job 预算、是否套 ci-retry 属协调者决策。
#
# 【候选面：§4.1 记载的是**偶发场景**，不是本 harness 的普遍关键坑】
#   design §4.1 原文（docs/web-sip-wss-design.md:141-145）：浏览器**偶发**只通告局域网 IP
#   （172.19.44.184，无回环）；此时若对端绑 127.0.0.1 且只通告回环候选，候选对无法形成 →
#   ICE 超时。§4.1 的规避配方是被叫侧用**非回环**信令 URL（绑 0.0.0.0 + 出接口 IP 候选）。
#   原文限定词是「偶发」+「浏览器没有回环候选」这一特指场景，并非普遍规律。
#   **本 harness（浏览器与 publisher 同机）2026-10-01 实测：回环 URL 也能通过**——只把
#   $pubSignal 改成 'ws://127.0.0.1:3061' 重跑 → exit 0、RUNNER_PASS、PEER_CANDIDATE=host|127.0.0.1|60847。
#   故本脚本沿用非回环配方属**保险措施**（与 §4.1 及跨机/浏览器无回环候选场景一致），
#   不是本机必需前提；跨机场景仍按 §4.1 处理。
#   机制（供跨机排查）：agent 的 connect_inner 以 signal_url 是否含 127.0.0.1/localhost 决定
#   bind 127.0.0.1 还是 0.0.0.0，绑 0.0.0.0 时用出接口 IP（egress_ip）通告 host 候选
#   （crates/aerodesk-agent/src/main.rs:1080、crates/aerodesk-core/src/connect.rs:201）。
#   【与 §4.1 写法的差异】§4.1 写的是 --signal ws://<LAN-IP>:3003；那是 SIP 迁移前的 JSON
#   WSS 面遗留写法。现在 agent 仍接受 ws://host:port，但 **URL 端口被剥离**：
#   sip_link::from_parts 只用 host，SIP 端口来自 AERO_SIP_PORT（默认 5060 UDP），
#   所以这里显式设 AERO_SIP_PORT=5060 并保证 host 非回环即可；:3061 只是为了让人一眼看出
#   信号面，实际不进 SIP 端口。浏览器信令本身走 wss://127.0.0.1:3061。
#
# 【Digest】signal 设 AUTH_TOKENS=secret ⇒ open_register=false、token_password=secret：
#   REGISTER 走真实 401 + Digest（回退口令），INVITE 走 407 Proxy-Authentication；
#   浏览器页面与 publisher（--token secret）都带同一口令。
#
# 用法:
#   pwsh -File scripts/web-view-native-e2e.ps1 [room]
# worktree（无本地 target）用法:
#   $env:AERODESK_BIN_DIR = 'E:\...\aerodesk\target\debug'
#   pwsh -File scripts/web-view-native-e2e.ps1 -Room myroom -SkipBuild
param(
    [string]$Room = "",
    [switch]$SkipBuild
)
$ErrorActionPreference = 'Stop'
$Root = Split-Path -Parent $PSScriptRoot
$ScriptDir = $PSScriptRoot
Set-Location $Root

if (-not $Room) { $Room = "webview-$([DateTime]::Now.ToString('HHmmss'))" }
# e2e 口令（signal AUTH_TOKENS）；浏览器页面与 publisher 共用。
$Token = 'secret'
# 被控端采集源（默认 screen）：
#   screen = 真实屏幕采集（连续 H264 流 + 响应 KeyframeRequest，design §4.1 实测 readyState=4）。
#            需要交互桌面会话（Windows 屏幕采集在 headless/服务会话失败）。
#   pcap   = 内置合成 VP8 单次流（48 帧）。**实测症状**：本方向下浏览器收得到 RTP 但解不出帧
#            （INBOUND_RTP framesReceived=47 framesDecoded=0、readyState=0；pub.err 有
#            loaded 48 VP8 frames / starting stream / stream finished (48 frames)）。
#            **代码事实（已逐条核对）**：agent 1:1 pcap 媒体循环在 ClientEvent::IceConnected
#            就置 connected（main.rs:2150-2152）并从 :2179 无条件开送；对照屏幕/通用路径在
#            ChannelOpen 才置 connected（aerodesk-session/src/generic_publisher.rs:400-403）。
#            该循环的 match（main.rs:2149-2160）无 KeyframeRequest 分支，落到
#            handle_publisher_input（main.rs:1832-1890；其 match 以 _ => {} 收尾，全函数 0 处
#            KeyframeRequest）；对照路径确实响应（generic_publisher.rs:412、main.rs:4469-4479）。
#            **因果推断（未红检，仅为候选解释）**：IceConnected 早于 DTLS/SRTP 就绪导致首帧
#            关键帧被丢，且该循环无法补关键帧 ⇒ framesDecoded=0。待验方案：给 pcap 路径补
#            KeyframeRequest 处理（或改到 ChannelOpen 后再开送 / 循环重播）后再看 framesDecoded
#            是否 >0。AERODESK_PUB_ENCODER=pcap 目前**必然 FAIL 断言①**（runner 判
#            readyState>=2 且 framesDecoded>0），只用于复现/诊断该缺口；要单看「RTP 到达」
#            需自行改判据，本脚本没有它的通过路径。
$PubEncoder = if ($env:AERODESK_PUB_ENCODER) { $env:AERODESK_PUB_ENCODER } else { 'screen' }
$WebPort = if ($env:WEB_SERVE_PORT) { [int]$env:WEB_SERVE_PORT } else { 38084 }

# FFMPEG_DIR 是运行时加载 FFmpeg DLL 的前提；用户级变量对新进程可见，但当前进程可能读不到。
if (-not $env:FFMPEG_DIR) { $env:FFMPEG_DIR = [Environment]::GetEnvironmentVariable('FFMPEG_DIR', 'User') }
if (-not $env:FFMPEG_DIR -or -not (Test-Path $env:FFMPEG_DIR)) {
    throw "FFMPEG_DIR 未设置或不存在（用户级环境变量 / 显式传入）"
}
$env:PATH = "$env:FFMPEG_DIR\bin;$env:PATH"

$TargetDir = if ($env:CARGO_TARGET_DIR) { $env:CARGO_TARGET_DIR } else { Join-Path $Root 'target' }
$BinDir = if ($env:AERODESK_BIN_DIR) { $env:AERODESK_BIN_DIR } else { Join-Path $TargetDir 'debug' }

$logDir = Join-Path $env:TEMP ("web-view-native-e2e-" + [DateTime]::Now.ToString('HHmmss'))
New-Item -ItemType Directory -Force -Path $logDir | Out-Null

function Stop-AeroDesk {
    # 只清本 e2e 会用到的三个二进制；headless Edge 只杀带 --headless 的，用户窗口实例不动
    # （沿用 web-edge-e2e-win.ps1 的安全前提）。
    foreach ($n in 'aerodesk-sfu', 'aerodesk-signal', 'aerodesk-agent') {
        Get-Process -Name $n -ErrorAction SilentlyContinue | Stop-Process -Force -ErrorAction SilentlyContinue
    }
    Get-CimInstance Win32_Process -Filter "Name='msedge.exe'" -ErrorAction SilentlyContinue |
        Where-Object { $_.CommandLine -like '*--headless*' } |
        ForEach-Object { Stop-Process -Id $_.ProcessId -Force -ErrorAction SilentlyContinue }
}

function Get-LogText([string]$name) {
    $t = ''
    foreach ($suffix in @('.log', '.err')) {
        $p = Join-Path $logDir ($name + $suffix)
        if (Test-Path $p) {
            try { $t += (Get-Content -Path $p -Raw -ErrorAction SilentlyContinue) } catch { }
        }
    }
    if ($null -eq $t) { $t = '' }
    return $t
}

function Test-Port([int]$port, [string]$hostName = '127.0.0.1') {
    try {
        $c = New-Object System.Net.Sockets.TcpClient
        $c.Connect($hostName, $port); $c.Close(); return $true
    } catch { return $false }
}

function Wait-Log([string[]]$names, [string]$pattern, [int]$timeoutSec) {
    # 就绪门：等日志出现目标行，避免 node/浏览器撞启动空窗白跑。
    $deadline = (Get-Date).AddSeconds($timeoutSec)
    while ((Get-Date) -lt $deadline) {
        $txt = ($names | ForEach-Object { Get-LogText $_ }) -join [Environment]::NewLine
        if ($txt -match $pattern) { return $true }
        Start-Sleep -Milliseconds 300
    }
    return $false
}

function Get-LanIp {
    # 出接口 IP（§4.1 配方要求非回环）。优先 UDP connect 探测，回退网卡枚举。
    try {
        $u = New-Object System.Net.Sockets.UdpClient
        $u.Connect('8.8.8.8', 80)
        $ip = $u.Client.LocalEndPoint.Address.ToString()
        $u.Close()
        if ($ip -and $ip -ne '0.0.0.0' -and $ip -ne '127.0.0.1') { return $ip }
    } catch { }
    $cand = Get-NetIPAddress -AddressFamily IPv4 -ErrorAction SilentlyContinue |
        Where-Object { $_.IPAddress -ne '127.0.0.1' -and $_.IPAddress -notlike '169.254.*' } |
        Select-Object -First 1
    if ($cand) { return $cand.IPAddress }
    throw "找不到非回环 IPv4 接口（§4.1 配方需要出接口 IP 候选）"
}

$sfu = $null; $sig = $null; $http = $null; $pub = $null
try {
    Stop-AeroDesk
    Start-Sleep -Milliseconds 600

    Write-Host "== build"
    if ($SkipBuild) {
        foreach ($b in 'aerodesk-sfu.exe', 'aerodesk-signal.exe', 'aerodesk-agent.exe') {
            if (-not (Test-Path (Join-Path $BinDir $b))) { throw "-SkipBuild 但 $BinDir\$b 不存在" }
        }
        Write-Host "  skip（AERODESK_BIN_DIR=$BinDir）"
    } else {
        cargo build -q -p aerodesk-sfu -p aerodesk-signal -p aerodesk-agent
    }

    Write-Host "== start sfu/signal/web"
    $env:SFU_BIND_ADDRESS = '0.0.0.0'
    $env:SFU_HOST_ADDRESS = '127.0.0.1'
    $env:SIP_UDP_PORT = '5060'
    $env:SIP_WSS_PORT = '3061'
    # AUTH_TOKENS 非空 ⇒ open_register=false：REGISTER 真实 401+Digest，INVITE 407 质询。
    $env:AUTH_TOKENS = $Token
    $sfu = Start-Process -FilePath (Join-Path $BinDir 'aerodesk-sfu.exe') -WindowStyle Hidden -RedirectStandardOutput "$logDir\sfu.log" -RedirectStandardError "$logDir\sfu.err" -PassThru
    $sig = Start-Process -FilePath (Join-Path $BinDir 'aerodesk-signal.exe') -WindowStyle Hidden -RedirectStandardOutput "$logDir\sig.log" -RedirectStandardError "$logDir\sig.err" -PassThru
    $http = Start-Process -FilePath 'python' -ArgumentList '-m', 'http.server', "$WebPort", '--bind', '127.0.0.1' -WorkingDirectory (Join-Path $Root 'web') -WindowStyle Hidden -RedirectStandardOutput "$logDir\http.log" -RedirectStandardError "$logDir\http.err" -PassThru

    if (-not (Wait-Log @('sig') 'SIP/UDP 监听已起' 20)) { throw "signal SIP/UDP 未就绪" }
    if (-not (Wait-Log @('sig') 'SIP/WSS 监听已起' 20)) { throw "signal SIP/WSS 未就绪" }
    $webOk = $false
    for ($i = 0; $i -lt 60; $i++) { if (Test-Port $WebPort) { $webOk = $true; break }; Start-Sleep -Milliseconds 250 }
    if (-not $webOk) { throw "web 静态服务未就绪（$WebPort）" }
    Write-Host "PASS signal (SIP/UDP + SIP/WSS) + web ready"

    Write-Host "== playwright-core"
    $e2eDir = Join-Path $env:TEMP 'web-view-native-e2e'
    New-Item -ItemType Directory -Force -Path $e2eDir | Out-Null
    if (-not (Test-Path "$e2eDir\node_modules\playwright-core")) {
        Push-Location $e2eDir
        npm init -y | Out-Null
        npm i playwright-core | Out-Null
        Pop-Location
    }

    $lanIp = Get-LanIp
    Write-Host "  publisher 信令 host（非回环，触发 0.0.0.0 绑定 + 出接口候选）=$lanIp"

    Write-Host "== start native publisher (SIP UAS, device AoR = room)"
    # publisher 的 device_id = --room（agent main.rs:1017）：浏览器 INVITE 同值即 1:1 接通。
    $env:AERO_SIP_PORT = '5060'
    $env:RUST_LOG = 'aerodesk_agent=info'
    $pubSignal = 'ws://' + $lanIp + ':3061'
    $pub = Start-Process -FilePath (Join-Path $BinDir 'aerodesk-agent.exe') -WindowStyle Hidden -ArgumentList '--role', 'publisher', '--encoder', $PubEncoder, '--signal', $pubSignal, '--room', $Room, '--token', $Token -RedirectStandardOutput "$logDir\pub.log" -RedirectStandardError "$logDir\pub.err" -PassThru
    if (-not (Wait-Log @('pub') ("SIP registered: " + [regex]::Escape($Room)) 25)) {
        Write-Host "--- pub.err ---"; Get-LogText 'pub' | Select-Object -Last 30
        throw "publisher 未在 25s 内注册（device=$Room）"
    }
    Write-Host "PASS publisher registered as $Room"

    Write-Host "== run browser viewer (UAC) -> publisher"
    Push-Location $e2eDir
    $oldNodePath = $env:NODE_PATH
    $env:NODE_PATH = "$e2eDir\node_modules"
    $env:WEB_SERVE_PORT = "$WebPort"
    $env:ROOM = $Room
    $env:SIGNAL_URL = 'wss://127.0.0.1:3061'
    $env:TOKEN = $Token
    $nodeOut = & node (Join-Path $ScriptDir 'web-view-native-e2e-run.js') 2>&1 | Tee-Object -FilePath "$logDir\node.out"
    $nodeRc = $LASTEXITCODE
    $env:NODE_PATH = $oldNodePath
    Pop-Location
    $nodeOut | ForEach-Object { Write-Host $_ }
    $nodeText = ($nodeOut -join [Environment]::NewLine)
    Write-Host "  node exit=$nodeRc"

    Start-Sleep -Milliseconds 800
    $pubText = Get-LogText 'pub'
    $sfuText = Get-LogText 'sfu'

    Write-Host "== 断言"
    $fail = 0
    if ($nodeRc -eq 0) { Write-Host "PASS ① 浏览器收到可解码视频轨（video.readyState>=2 且 inbound-rtp framesDecoded>0）且候选对非 relay" }
    else { Write-Host "FAIL ① 浏览器侧未通过（见 runner 输出）"; $fail = 1 }

    if ($pubText -match 'input: seq=') { Write-Host "PASS ② 键鼠输入帧抵达 publisher（pub 日志 input: seq=）" }
    else { Write-Host "FAIL ② publisher 未收到 input 帧"; $fail = 1 }

    if ($pubText -match 'ICE connected') { Write-Host "PASS publisher ICE connected" }
    else { Write-Host "FAIL publisher 未 ICE connected"; $fail = 1 }

    # ③ 1:1 不经 SFU：媒体对端必须是 publisher 自己的媒体 socket（端口一致），
    #    且 publisher 未升级为 SFU 会议、SFU 侧未出现本房间。
    $pubPort = $null
    if ($pubText -match 'local UDP addr: [^:\r\n]+:(\d+)') { $pubPort = $Matches[1] }
    $peerPort = $null
    if ($nodeText -match 'PEER_PORT=(\d+)') { $peerPort = $Matches[1] }
    if ($pubPort -and $peerPort -and $pubPort -eq $peerPort) {
        Write-Host "PASS ③ 承重证据：浏览器 ICE 选中候选端口 = publisher 媒体 socket 端口（$pubPort）⇒ 1:1 直连，未经 SFU"
    } else {
        Write-Host "FAIL ③ 端口不一致（pub=$pubPort peer=$peerPort）⇒ 无法证明直连"
        $fail = 1
    }
    if ($pubText -match '升级为 SFU 会议') { Write-Host "FAIL publisher 升级为 SFU 会议"; $fail = 1 }
    # 否定式断言必须带前置：日志缺失/读不到时「日志里没有」会空过。故先要求日志存在且非空。
    if ($sfuText.Length -eq 0) {
        Write-Host "FAIL ③-b SFU 日志为空——否定式断言空过（无法排除媒体经 SFU）"; $fail = 1
    } elseif ($sfuText -match [regex]::Escape($Room)) {
        Write-Host "FAIL ③-b SFU 日志出现本房间 $Room（媒体/信令经 SFU）"; $fail = 1
    } else {
        Write-Host "PASS ③-b SFU 日志非空且无本房间记录（辅助证据；承重证据是上面的端口相等）"
    }

    if ($fail -ne 0) {
        Write-Host "--- node.out (tail) ---"; if (Test-Path "$logDir\node.out") { Get-Content "$logDir\node.out" | Select-Object -Last 40 }
        Write-Host "--- pub.err (tail) ---"; Get-LogText 'pub' | Select-Object -Last 30
        Write-Host "--- sig.err (tail) ---"; Get-LogText 'sig' | Select-Object -Last 30
        Write-Host "--- sfu.err (tail) ---"; Get-LogText 'sfu' | Select-Object -Last 20
    }
    Write-Host "LOGDIR=$logDir"
    exit $fail
}
finally {
    foreach ($p in @($pub, $http, $sfu, $sig)) { if ($p) { Stop-Process -Id $p.Id -Force -ErrorAction SilentlyContinue } }
    Stop-AeroDesk
}
