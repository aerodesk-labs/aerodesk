# Web 观看页（sip-viewer.html）→ 原生 CLI publisher 方向端到端（#583 剩余工作）。
#
# 方向与既有 e2e 相反：既有脚本覆盖「Web 被控 → 原生/Web 观看」；本脚本覆盖
# 「headless 浏览器观看端（UAC，REGISTER + Digest）→ INVITE 原生 CLI publisher
# （SIP UAS）→ 1:1 P2P 媒体（视频 + 音频）+ 键鼠经 data channel 回传」。该方向此前无覆盖。
# 2026-10-01 #583 音频缺口补齐：观看页建 audio recvonly transceiver，publisher 加 --audio
# （Windows = WASAPI loopback，失败回退合成音），断言①-b 检查 answer 含 m=audio 且音频轨
# 有 inbound-rtp packetsReceived>0。
#
# 【#583 边界与已知限制（2026-10-01 复审后补录）】
# (1) 编解码白名单来源与覆盖（web/sip-viewer.html preferAeroCodecs；全局默认生效）：
#     白名单 = Chromium 能解码 ∩ AeroDesk 发布端会产出。
#       视频 H264 —— 可产出：--codec h264（默认；Windows screen / desktop 默认 H264）
#                    ＋ h264_videotoolbox / h264_mf / libx264 / OpenH264
#                    （agent main.rs:352-371；aerodesk-session/src/generic_publisher.rs:320-344）；白名单 ✅
#       视频 VP8  —— 可产出：默认合成源 pcap（publisher_media_loop 直接送 VP8 帧）；白名单 ✅
#       视频 VP9  —— 可产出：--codec vp9（libvpx-vp9，aerodesk-codec/src/encode.rs:38）；白名单 ✅
#       视频 AV1  —— 可产出：--codec av1（libsvtav1，encode.rs:39）；白名单 ✅
#       视频 rtx  —— 重传；白名单 ✅
#       音频 PCMU —— 可产出：Endpoint enable_pcmu（合成 AudioTicker / 真实系统音频）；白名单 ✅
#       音频 Opus —— 可产出：enable_opus（--audio-opus / RealAudioSender）；白名单 ✅
#     **已知限制 H265/HEVC**：发布端会产（--codec h265；**macOS 在 VT HEVC 可用时默认优先
#     h265**，agent main.rs:349-365；hevc_videotoolbox / hevc_mf / libx265），但 Chromium
#     WebRTC 不收 H265（本机 headless Edge 实测 getCapabilities('video') 无 video/H265）
#     ⇒ **macOS 原生发布端默认 HEVC ⇒ Web 观看页协商不出视频**（Windows 默认 H264 正常）。
#     该限制先于本批存在（裁剪前浏览器同样收不了 H265），非本批引入；卡
#     web-viewer-macos-hevc-no-video 跟踪。未在 macOS 红检。
#     **有意收窄（本仓无消费者）**：video/red、video/ulpfec、audio/red、audio/G722、
#     audio/PCMA、audio/CN、audio/telephone-event(DTMF)——全仓 grep 无引用（复审复核）。
#     白名单 ≠ 页面全能力；将来若有消费者需要这些，须先扩白名单并重核 8192B 预算。
# (2) 8192B 悬崖**已修**（卡 sip-udp-8192-sdp-cliff）：vendored rsipstack UDP 收包缓冲由
#     8192B 提到最大 UDP 载荷 65535B（>8 KiB 的 INVITE 现可完整收下）；收包侧截断/超限记录
#     warn（含缓冲容量与实际读到的字节数）。页面侧编解码裁剪仍保留，但**不再是 8 KiB 的必需品**，
#     降为协商面收窄的优化。
#     直证（非推断，窄 harness 复用 vendored UdpConnection::serve_loop）：body 8198 → 总 8519B
#     datagram 在 Windows 上触发 WSAEMSGSIZE(os error 10040) 被丢弃；body 6943（总 7264B）与
#     body 5883（总 6204B）收到。且原 vendored warn 因 agent 的 RUST_LOG=aerodesk_agent=info
#     过滤（EnvFilter 未匹配 target 默认关闭）**不可见**——这才是「零日志」的直因；过滤器已放开
#     rsipstack=warn。
# (3) 本批只证明「音频轨已连通（协商出 m=audio + 有 inbound-rtp RTP）」，**不证明音频
#     可用/音质**：实测 concealedSamples 偏高（约 57-62% PLC），成因未定性，另开卡。
# (4) 重连语义：本机用 pwsh/Playwright 窄 harness（自用端口、只杀自有进程）验「断信令 →
#     重连 → 再收流」——head 页与原版页**同样失败**（单次 1s 重试后停在「连接失败」、无
#     第二次 REGISTER）⇒ 该缺口先于本批存在、非本批回归；现有 web-reconnect-e2e.sh 的判据
#     只看 video.readyState 未跌，可能观察不到重连失败。scripts/web-e2e.sh:43 与
#     scripts/web-reconnect-e2e.sh:48 都加载本页——除上述窄 harness 外，这两个 bash 脚本
#     本批未在本机跑（Git Bash 起子进程会挂）。
#
# 参照：
#   scripts/web-pub-e2e.sh        —— 本方向的镜像（浏览器被控、CLI 观看）；本脚本方向对调。
#   scripts/web-edge-e2e-win.ps1  —— Windows 原生 web e2e 样板（进程/清理/日志组织沿用）。
#   docs/web-sip-wss-design.md §4.1 —— 候选面实测坑与配方。
#
# 【CI 接入】已加入 .github/workflows/ci.yml 的 Windows 段（test job，
# 「e2e web viewer -> native publisher pcap (Windows)」，固定
# AERODESK_PUB_ENCODER=pcap）——合成源不依赖交互桌面，与既有 Windows job
# 「e2e web viewer (Windows Edge)」（scripts/web-edge-e2e-win.ps1）同环境
# （Edge 由 runner 预装，playwright-core 由脚本 npm i）。主路径 --encoder screen
# 仍需交互桌面会话，只用于本机验收。
# 能接 CI 的前提是 pcap 路径存在可绿路径：断言①（readyState>=2 且
# framesDecoded>0）此前恒 FAIL，根因与红检证据见下方 pcap 说明。
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
#   pcap   = 内置合成 VP8 流（vp8.pcap，修复后解析为 101 帧 / 3 关键帧）。
#            **根因已定性并修复（2026-10-01）**：本方向 framesDecoded=0 的原因不在发布
#            时序，而是 aerodesk-core 的 pcap 解析读错 RTP 头扩展长度。旧代码
#            `12 + cc*4 + rtp[12]`：该 pcap 每个 RTP 包都带 0xBEDE 头扩展，rtp[12] 正是
#            profile 高字节 0xBE(=190)，被当成扩展字节数，于是负载从真实 payload 中间
#            切开，产出 48 个无法解码的帧（首帧也不是关键帧）。
#            红检（真实 Rust 解析器 parse_vp8_pcap 导出 IVF → ffmpeg 解码，不依赖浏览器）：
#              修复前 frame=0（RC=69，Invalid sync code / Decoding error），
#                     解析得 48 帧、首帧 keyframe=false；
#              修复后 frame=101（RC=0），解析得 101 帧、首帧 keyframe=true。
#            卡片里原先「IceConnected 早于 DTLS/SRTP 就绪导致首帧关键帧被丢、且该循环
#            无法补关键帧 ⇒ framesDecoded=0」的因果推断据此**证伪**：帧本身不可解，
#            补 KeyframeRequest / 改开送时机 / 循环重播都救不回来。
#            附（这些代码事实仍成立，修复后不再是瓶颈）：pcap 媒体循环在
#            ClientEvent::IceConnected 就置 connected（main.rs:2150-2152）并从 :2179
#            无条件开送；其 match（main.rs:2149-2160）无 KeyframeRequest 分支，落到
#            handle_publisher_input（main.rs:1832-1890，match 以 _ => {} 收尾，全函数 0 处
#            KeyframeRequest）。修复后流内关键帧在第 0/2/5 帧（相邻约 33ms），即便首帧
#            在 DTLS 就绪前被丢，紧随的关键帧也能让解码器起步。
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
    $env:RUST_LOG = 'aerodesk_agent=info,rsipstack=warn'
    $pubSignal = 'ws://' + $lanIp + ':3061'
    $pub = Start-Process -FilePath (Join-Path $BinDir 'aerodesk-agent.exe') -WindowStyle Hidden -ArgumentList '--role', 'publisher', '--encoder', $PubEncoder, '--audio', '--signal', $pubSignal, '--room', $Room, '--token', $Token -RedirectStandardOutput "$logDir\pub.log" -RedirectStandardError "$logDir\pub.err" -PassThru
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

    # ①-b #583 音频：观看页协商出 m=audio 且音频轨有 inbound-rtp 计数。
    #     与 ① 同看 $nodeRc，但独立复核——避免 runner 判据被改窄后无人发现。
    $audioPacketMatch = [regex]::Match($nodeText, 'AUDIO_PACKETS=(\d+)')
    $audioHasAudio = $nodeText -match 'ANSWER_HAS_AUDIO=true'
    $audioPresent = $nodeText -match 'AUDIO_TRACK=present'
    $audioCounted = $audioPacketMatch.Success -and ([int]$audioPacketMatch.Groups[1].Value -gt 0)
    if ($audioHasAudio -and $audioPresent -and $audioCounted) {
        Write-Host "PASS ①-b 观看页协商出 m=audio 且音频轨有 inbound-rtp：ANSWER_HAS_AUDIO=true AUDIO_TRACK=present AUDIO_PACKETS=$($audioPacketMatch.Groups[1].Value)>0"
    } else {
        Write-Host "FAIL ①-b 音频缺口未补：ANSWER_HAS_AUDIO=$audioHasAudio AUDIO_TRACK=$audioPresent AUDIO_PACKETS=$($audioPacketMatch.Groups[1].Value)"
        $fail = 1
    }

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
