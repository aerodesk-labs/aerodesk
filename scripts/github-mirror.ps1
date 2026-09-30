#Requires -Version 5.1
<#
.SYNOPSIS
  AeroDesk 的 GitHub 镜像同步：walgit 主仓的 main + tags 单向镜像到 GitHub（发布驱动用 -Once；前台常驻循环仅供调试）。

.DESCRIPTION
  canonical 仓库是 walgit（默认 http://127.0.0.1:8081/gqf2008/aerodesk.git）：issue / PR / 看板
  都在 refs/collab/*，GitHub 只做镜像 + 发版流水线。本脚本维护一个裸镜像仓
  （默认 ~/.walgit/mirror/aerodesk.git），每轮做两件事：

    1. 从 walgit fetch main + tags（--prune），镜像仓的本地 refs = 唯一的发布集合；
    2. 把这个集合 push 到 GitHub（默认带 `+` 强制覆盖 = 纯镜像语义）。

  **发布范围 = main + tags**（Jev 判定 main_and_tags_only，p=0.970 / c=0.960）：walgit 上其它
  heads 一概不外流。项目流程要求实现分支先推 walgit 供独立审查，所以「在途分支被顺带发布到
  公开镜像」是常态风险；push refspec 只列 main，在途分支根本不进推送集合。（审查期实测旧
  refspec 会尝试推送在途的 fix/mirror-ops，靠 GitHub 500 才没发出去——纯属运气。）
  refs/collab/* 永不外流：两端 refspec 都只覆盖 refs/heads/main 与 refs/tags/*。

  上面两步的 refspec 都是 main + tags：镜像仓的本地 refs 就是发布集合本身，在途审查分支
  连本地副本都不会有，不可能外流。差集 = GitHub 真实 refs − 本地 refs，因此 `-Status` 的
  GitHub-only refs 清单**恰好**就是 `-Prune` 的删除集；反过来，walgit 上未发布的在途分支
  不会出现在这份清单里（它不在 GitHub 上，本来也无需删除）。

  推送默认**不删** GitHub 侧只存在的分支/标签（迁移前的历史 ref 不会被顺手删掉）。要让镜像与
  walgit 完全对齐（含删除），显式加 -Prune。**收窄 refspec 后 push --prune 只覆盖 refspec 命中
  的目的 ref（main 与 tags），再也删不掉 GitHub 上 walgit 没有的分支**；所以 -Prune 不再依赖它：
  先 `ls-remote` 取远端真实 ref 列表，与镜像仓本地 refs 求差集，再对差集里的每个 ref 执行显式
  删除（`push github --delete <ref>`）。远端不可达或查询为空时拒绝删除。

  代理：GitHub 走本机 Clash 代理，端口会漂移，所以默认自动探测（-ProxyPort >
  AERODESK_GITHUB_PROXY / HTTPS_PROXY 环境变量 > 注册表 ProxyServer > 常见端口探测）；
  walgit 在 127.0.0.1，显式设置 NO_PROXY 保证不经代理。GitHub 的 git 凭据沿用全局
  credential helper（本机是 `gh auth git-credential`）。

.PARAMETER Once
  只跑一轮后退出（发布驱动的常规入口：发 tag / 发版时显式跑一次）。默认是常驻循环（仅调试），Ctrl+C 结束。
  只推 main + tags，walgit 上的其它 heads 不会外流。

.PARAMETER NoForce
  推送不带 `+`：GitHub 侧一旦有 walgit 没有的提交（例如有人直推 GitHub），
  推送会被拒绝并在日志里报错，而不是被镜像覆盖。

.PARAMETER Prune
  删除 GitHub 上 walgit 已经没有的分支/标签，让镜像与 walgit 完全对齐。默认关闭。
  **实现不是 push --prune**：收窄后的 push refspec 只命中 main 与 tags，`push --prune`
  再也覆盖不到其它分支。改为先 `ls-remote github` 取远端真实 ref 列表，与镜像仓本地 refs
  （= main + tags 的发布集合）求差集，差集非空时对每个 ref 执行显式删除。
  远端不可达、查询为空、或没有任何 GitHub-only ref 时，都不会删除任何东西。
  注意 `-Prune` 删的是「不在发布集合里的 ref」= GitHub 真实 refs − 发布集合（main + tags），
  **不只是**「GitHub 有、walgit 没有」的那些：非 main 分支只要出现在 GitHub 上就会被删掉，
  哪怕它同时还在 walgit 里；要保住它只能先把它加进发布集合。
  哪些 ref 会被删见 `-Status`。

.PARAMETER InstallTask
  已停用：调用会直接拒绝（GitHub 镜像是发布驱动的，见 docs/WALGIT.md §4；清理历史任务用 -UninstallTask）。

.PARAMETER UninstallTask
  注销该计划任务。

.PARAMETER Branch
  被镜像的分支，默认 main（Jev 判定 main_and_tags_only）。改成别的分支等于改变发布范围，
  需要重新判定；保留参数只为在离线 harness 里复现同一套逻辑。

.PARAMETER Status
  打印计划任务状态、发布范围，以及最近的镜像日志与 ref 对齐情况。

.EXAMPLE
  pwsh -File scripts/github-mirror.ps1 -Once          # 手动同步一轮
  pwsh -File scripts/github-mirror.ps1                # 前台常驻循环
  pwsh -File scripts/github-mirror.ps1 -UninstallTask # 清理历史上装过的定时任务
  pwsh -File scripts/github-mirror.ps1 -Status
#>
[CmdletBinding()]
param(
    [string]$WalgitUrl = 'http://127.0.0.1:8081/gqf2008/aerodesk.git',
    [string]$GithubUrl = 'https://github.com/aerodesk-labs/aerodesk.git',
    [string]$MirrorDir = (Join-Path $env:USERPROFILE '.walgit\mirror\aerodesk.git'),
    [string]$LogFile   = (Join-Path $env:USERPROFILE '.walgit\sync-to-github-aerodesk.log'),
    [string]$Branch = 'main',
    [int]$ProxyPort = 0,
    [int]$IntervalSeconds = 60,
    [int]$MaxLogBytes = 5242880,
    [switch]$Once,
    [switch]$NoForce,
    [switch]$Prune,
    [switch]$InstallTask,
    [switch]$UninstallTask,
    [switch]$Status
)

Set-StrictMode -Version Latest
$ErrorActionPreference = 'Stop'

$TaskName = 'walgit-sync-github-aerodesk'
# 发布范围：只镜像这一个分支（+ 全部 tags）。改它等于改变对外发布范围，先重新判定。
$script:MirrorBranch = $Branch
$StateDir = Join-Path $env:USERPROFILE '.walgit'
$LockFile = Join-Path $StateDir 'sync-to-github-aerodesk.lock'
$script:ResolvedProxyPort = 0

# walgit 在回环上，任何时候都不该经代理；GitHub 由 -c http.proxy 显式指定。
$env:NO_PROXY = '127.0.0.1,localhost'
$env:no_proxy = $env:NO_PROXY

function Write-MirrorLog {
    param(
        [Parameter(Mandatory = $true)][string]$Message,
        [ValidateSet('INFO', 'WARN', 'ERROR')][string]$Level = 'INFO'
    )
    $line = '{0} [{1}] {2}' -f (Get-Date).ToString('yyyy-MM-dd HH:mm:ss'), $Level, $Message
    Write-Host $line
    try {
        $dir = Split-Path -Parent $LogFile
        if ($dir -and -not (Test-Path -LiteralPath $dir)) {
            New-Item -ItemType Directory -Force -Path $dir | Out-Null
        }
        if ((Test-Path -LiteralPath $LogFile) -and ((Get-Item -LiteralPath $LogFile).Length -gt $MaxLogBytes)) {
            $rotated = "$LogFile.1"
            if ([System.IO.File]::Exists($rotated)) { [System.IO.File]::Delete($rotated) }
            Move-Item -LiteralPath $LogFile -Destination $rotated -Force
        }
        Add-Content -LiteralPath $LogFile -Value $line -Encoding UTF8
    } catch {
        Write-Host "log write failed: $($_.Exception.Message)"
    }
}

function Invoke-Git {
    param(
        [Parameter(Mandatory = $true)][string[]]$Arguments,
        [switch]$ViaProxy,
        [switch]$AllowFailure
    )
    $cmd = @('git')
    if ($ViaProxy) {
        if ($script:ResolvedProxyPort -le 0) {
            throw "GitHub 需要代理，但没有探测到可用端口（用 -ProxyPort <端口> 指定，如 -ProxyPort 7890）"
        }
        $cmd += @(
            '-c', "http.proxy=http://127.0.0.1:$($script:ResolvedProxyPort)",
            '-c', "https.proxy=http://127.0.0.1:$($script:ResolvedProxyPort)",
            '-c', 'http.sslBackend=openssl',
            '-c', 'http.version=HTTP/1.1'
        )
    }
    $cmd += $Arguments
    $exe = $cmd[0]
    $rest = @()
    if ($cmd.Length -gt 1) { $rest = $cmd[1..($cmd.Length - 1)] }

    $output = & $exe @rest 2>&1
    $code = $LASTEXITCODE
    $lines = @($output | ForEach-Object { "$_" })
    if ($code -ne 0 -and -not $AllowFailure) {
        $joined = ($lines | Select-Object -Last 8) -join ' | '
        throw "git $($Arguments -join ' ') 失败（exit $code）：$joined"
    }
    return [pscustomobject]@{ ExitCode = $code; Output = $lines }
}

function Test-LoopbackPort {
    param([Parameter(Mandatory = $true)][int]$Port)
    $client = New-Object System.Net.Sockets.TcpClient
    try {
        $async = $client.BeginConnect('127.0.0.1', $Port, $null, $null)
        if (-not $async.AsyncWaitHandle.WaitOne(300)) { return $false }
        $client.EndConnect($async)
        return [bool]$client.Connected
    } catch {
        return $false
    } finally {
        $client.Close()
    }
}

function Resolve-ProxyPort {
    param([int]$Explicit = 0)
    if ($Explicit -gt 0) { return $Explicit }
    foreach ($name in @('AERODESK_GITHUB_PROXY', 'HTTPS_PROXY', 'https_proxy', 'HTTP_PROXY', 'http_proxy')) {
        $value = [Environment]::GetEnvironmentVariable($name)
        if ($value -and $value -match '(\d{2,5})\s*$') { return [int]$Matches[1] }
    }
    $settings = Get-ItemProperty -Path 'HKCU:\Software\Microsoft\Windows\CurrentVersion\Internet Settings' -ErrorAction SilentlyContinue
    if ($settings) {
        $property = $settings.PSObject.Properties['ProxyServer']
        if ($property -and "$($property.Value)" -match '(\d{2,5})\s*$') {
            $candidate = [int]$Matches[1]
            if (Test-LoopbackPort -Port $candidate) { return $candidate }
        }
    }
    foreach ($candidate in @(7897, 7890, 7891, 10809, 1080)) {
        if (Test-LoopbackPort -Port $candidate) { return $candidate }
    }
    return 0
}

function Set-MirrorRemote {
    param(
        [Parameter(Mandatory = $true)][string]$Name,
        [Parameter(Mandatory = $true)][string]$Url
    )
    $existing = (Invoke-Git -Arguments @('-C', $MirrorDir, 'remote')).Output
    if ($existing -contains $Name) {
        Invoke-Git -Arguments @('-C', $MirrorDir, 'remote', 'set-url', $Name, $Url) | Out-Null
    } else {
        Invoke-Git -Arguments @('-C', $MirrorDir, 'remote', 'add', $Name, $Url) | Out-Null
    }
}

function Initialize-Mirror {
    $parent = Split-Path -Parent $MirrorDir
    if ($parent -and -not (Test-Path -LiteralPath $parent)) {
        New-Item -ItemType Directory -Force -Path $parent | Out-Null
    }
    if (-not (Test-Path -LiteralPath (Join-Path $MirrorDir 'HEAD'))) {
        Invoke-Git -Arguments @('init', '--bare', $MirrorDir) | Out-Null
    }
    Set-MirrorRemote -Name 'origin' -Url $WalgitUrl
    Set-MirrorRemote -Name 'github' -Url $GithubUrl

    # refspec 显式写进 remote 配置：fetch --prune 才会同时裁剪分支与标签，
    # 同时保证 refs/collab/* 不会进入镜像仓（也就没有机会被推到 GitHub）。
    # **有意**清空 remote.origin.fetch 后只写下面两条：镜像仓里任何自定义 refspec 都会被丢弃，
    # 这是刻意的——镜像仓只接受 main + tags 的单向镜像，不接受别的 ref。
    # fetch 侧与 push 侧同窄（main + tags）：镜像仓的本地 refs 就是发布集合本身，
    # 在途审查分支连本地副本都不留。Get-GithubOnlyRefs = GitHub 真实 refs − 本地 refs，
    # 因此 `-Status` 的 GitHub-only refs 清单恰好就是 `-Prune` 的删除集；walgit 上未发布的
    # 在途分支不在 GitHub 上，本来就不需要（也不应该）出现在这份清单里。
    # git remote add 会先写一条默认 refspec +refs/heads/*:refs/remotes/origin/*；它不参与推送，
    # 却让镜像仓每轮多维护一份 refs/remotes/origin/*。所以先清空再只写两条
    #（--unset-all 在键不存在时 exit 5，用 -AllowFailure 吃掉），保证每次都是同一组 refspec。
    Invoke-Git -Arguments @('-C', $MirrorDir, 'config', '--unset-all', 'remote.origin.fetch') -AllowFailure | Out-Null
    foreach ($spec in @('+refs/heads/main:refs/heads/main', '+refs/tags/*:refs/tags/*')) {
        Invoke-Git -Arguments @('-C', $MirrorDir, 'config', '--add', 'remote.origin.fetch', $spec) | Out-Null
    }
    # github 只做 push 目标、从不 fetch；它那条默认 refspec 同样是死状态，一并清掉。
    Invoke-Git -Arguments @('-C', $MirrorDir, 'config', '--unset-all', 'remote.github.fetch') -AllowFailure | Out-Null
    # 旧默认 refspec 留下的 refs/remotes/*（origin/main、origin/HEAD、github/main 等）不再被任何
    # refspec 维护、也不参与推送，是纯死状态；删掉让镜像仓收敛到「只有 refs/heads + refs/tags」。
    # **--no-deref 是关键**：refs/remotes/origin/HEAD 是符号引用，若解引用删除，删掉的是它的目标
    # refs/remotes/origin/main，符号引用文件会以悬空态留在磁盘上——for-each-ref 看不见它（永远清不掉），
    # git fsck 也会由 exit 0 变 exit 2（invalid sha1 pointer 0000…）。
    # 只信任形如 refs/remotes/ 的行：for-each-ref 出错时会往 stderr 写 error:…，经 Invoke-Git 合并进 Output。
    $stale = Invoke-Git -Arguments @('-C', $MirrorDir, 'for-each-ref', '--format=%(refname)', 'refs/remotes') -AllowFailure
    foreach ($ref in @($stale.Output | Where-Object { $_ -match '^refs/remotes/' })) {
        Invoke-Git -Arguments @('-C', $MirrorDir, 'update-ref', '--no-deref', '-d', $ref) -AllowFailure | Out-Null
    }
    # 兜底：悬空符号引用（例如历史上被解引用删过一次的 origin/HEAD）不会出现在 for-each-ref 里，
    # 所以再按目录清一次。refs/remotes 是镜像仓的纯死状态缓存，递归删掉即可；packed-refs 已由上面的
    # --no-deref -d 清空，refs/heads 与 refs/tags 不受影响。不依赖 fsck，天然幂等。
    $remoteRefsDir = Join-Path (Join-Path $MirrorDir 'refs') 'remotes'
    if (Test-Path -LiteralPath $remoteRefsDir) {
        Remove-Item -LiteralPath $remoteRefsDir -Recurse -Force
    }
}

function Get-MirrorTip {
    $result = Invoke-Git -Arguments @('-C', $MirrorDir, 'rev-parse', '--verify', '--quiet', "refs/heads/$($script:MirrorBranch)") -AllowFailure
    if ($result.ExitCode -ne 0 -or $result.Output.Count -eq 0) { return '(none)' }
    return $result.Output[0]
}

function Invoke-MirrorCycle {
    $script:ResolvedProxyPort = Resolve-ProxyPort -Explicit $ProxyPort
    Initialize-Mirror

    $fetch = Invoke-Git -Arguments @('-C', $MirrorDir, 'fetch', 'origin', '--prune', '--no-tags')
    foreach ($line in $fetch.Output) {
        if ($line -match '^(From| \*)' -or $line -match '\[(new|deleted|updated)') {
            Write-MirrorLog "walgit fetch: $line"
        }
    }

    $force = '+'
    if ($NoForce) { $force = '' }
    # push refspec 只列 main 一个分支（+ 全部 tags）：walgit 上的其它 heads（在途审查分支等）
    # 根本不进推送集合，不可能被顺带发布。分支名硬编码为 main，没有可切分支的参数。
    $pushArgs = @('-C', $MirrorDir, 'push', '--porcelain', 'github',
                  "$($force)refs/heads/$($script:MirrorBranch):refs/heads/$($script:MirrorBranch)",
                  "$($force)refs/tags/*:refs/tags/*")
    $push = Invoke-Git -Arguments $pushArgs -ViaProxy -AllowFailure
    if ($push.ExitCode -ne 0) {
        foreach ($line in $push.Output) { Write-MirrorLog "github push: $line" 'ERROR' }
        Write-MirrorLog "镜像推送失败（GitHub 侧可能有 walgit 没有的提交，或 main 分叉；要强制覆盖去掉 -NoForce）" 'ERROR'
        return $false
    }
    foreach ($line in $push.Output) {
        if ($line -match 'up to date') { continue }
        if ($line.Trim().Length -eq 0) { continue }
        Write-MirrorLog "github push: $($line.Trim())"
    }
    if ($Prune) {
        # 收窄 refspec 后 push --prune 只能覆盖 main/tags 这些命中的目的 ref，删不掉 GitHub 上
        # 不在发布集合里的分支；所以 -Prune 改为显式删除：远端真实 ref 列表 − 镜像仓本地 refs。
        # 取列表失败/为空时 Get-GithubOnlyRefs 会抛错，这里就地变成一轮失败，绝不盲删。
        $only = @(Get-GithubOnlyRefs)
        if ($only.Count -eq 0) {
            Write-MirrorLog 'prune: 没有 GitHub-only ref，无需删除'
        } else {
            Write-MirrorLog "prune: 删除 $($only.Count) 个 GitHub-only ref"
            $deleteArgs = @('-C', $MirrorDir, 'push', '--porcelain', 'github', '--delete') + $only
            $delete = Invoke-Git -Arguments $deleteArgs -ViaProxy -AllowFailure
            foreach ($line in $delete.Output) {
                if ($line.Trim().Length -eq 0) { continue }
                Write-MirrorLog "github prune: $($line.Trim())"
            }
            if ($delete.ExitCode -ne 0) {
                Write-MirrorLog 'prune 删除失败（远端可能已变化，下一轮会重算差集）' 'ERROR'
                return $false
            }
        }
    }
    return $true
}

function Install-MirrorTask {
    # 用户 2026-09-20 明令：GitHub 镜像改为发布驱动，不接受每 60 秒轮询的计划任务——它在桌面
    # 会话里每 60 秒拉起一个控制台程序，-WindowStyle Hidden 挡不住窗口闪现（证据见看板卡
    # mirror-ops-quiet / win-sched-task-console）。这里直接拒绝而不是只打印告警：告警会被忽略，
    # 拒绝才能让「不要装回来」这条规则可被机器执行。真需要无人值守时，应改用 GUI 子系统
    # launcher（walgit-service-host.exe 形态），不要把控制台程序交给计划任务。
    $reason = '-InstallTask 已停用：GitHub 镜像是发布驱动的，请改为发 tag / 发版时显式跑一次 pwsh -File scripts/github-mirror.ps1 -Once（见 docs/WALGIT.md §4）；清理历史任务仍可用 -UninstallTask'
    Write-MirrorLog $reason 'WARN'
    throw $reason
}

function Uninstall-MirrorTask {
    $task = Get-ScheduledTask -TaskName $TaskName -ErrorAction SilentlyContinue
    if (-not $task) {
        Write-MirrorLog "计划任务不存在：$TaskName"
        return
    }
    Unregister-ScheduledTask -TaskName $TaskName -Confirm:$false
    Write-MirrorLog "计划任务已注销：$TaskName"
}

function Show-MirrorStatus {
    $task = Get-ScheduledTask -TaskName $TaskName -ErrorAction SilentlyContinue
    if ($task) {
        $info = Get-ScheduledTaskInfo -TaskName $TaskName
        Write-Host "task            : $TaskName ($($task.State))"
        Write-Host "last run        : $($info.LastRunTime) exit=$($info.LastTaskResult)"
        Write-Host "next run        : $($info.NextRunTime)"
    } else {
        Write-Host "task            : $TaskName 未安装（发布驱动，无需安装；历史注册用 -UninstallTask 清理）"
    }
    Write-Host "mirror dir      : $MirrorDir"
    Write-Host "walgit remote   : $WalgitUrl"
    Write-Host "github remote   : $GithubUrl"
    Write-Host "publish scope   : branch refs/heads/$($script:MirrorBranch) + all refs/tags（其余 walgit heads 不发布）"
    $script:ResolvedProxyPort = Resolve-ProxyPort -Explicit $ProxyPort
    Write-Host "proxy port      : $($script:ResolvedProxyPort)"
    Show-GithubOnlyRefs
    if (Test-Path -LiteralPath $LogFile) {
        Write-Host "--- last log lines ($LogFile) ---"
        Get-Content -LiteralPath $LogFile -Tail 10 | ForEach-Object { Write-Host $_ }
    }
}

function Get-RemoteRefNames {
    # 取 GitHub 端真实 ref 列表（heads + tags）。--branches/--tags 限定查询范围（本机 git 2.55 的
    # usage 只列 --branches，--heads 虽仍生效但已不文档化）；--exit-code 让「没有任何匹配 ref」以
    # exit 2 结束，从而把「查询结果为空」与「确实没有 GitHub-only ref（0 个）」区分开——这条清单
    # 既是 -Status 的对齐预览，也是 -Prune 的删除依据，假绿灯等于把安全网关掉。
    # 失败一律抛错：调用方（-Status 打印、-Prune 删除）都不允许在「远端不可达」时把空集当成对齐。
    $query = Invoke-Git -Arguments @('-C', $MirrorDir, 'ls-remote', '--branches', '--tags', '--exit-code', 'github') -ViaProxy -AllowFailure
    if ($query.ExitCode -eq 2) {
        throw 'git ls-remote github 查询结果为空——GitHub 侧没有任何 branch/tag 是可疑状态（远端未初始化或不可达），先确认远端再操作'
    }
    if ($query.ExitCode -ne 0) {
        $tail = ($query.Output | Select-Object -Last 3) -join ' | '
        throw "git ls-remote github 失败（exit $($query.ExitCode)）：$tail"
    }
    if (@($query.Output).Count -eq 0) {
        throw 'git ls-remote github 退出码 0 却没有任何 ref——结果不可信，拒绝继续'
    }
    $names = @{}
    foreach ($line in $query.Output) {
        $parts = $line -split '\s+', 2
        if ($parts.Count -eq 2 -and $parts[1] -notmatch '\^\{\}$') { $names[$parts[1]] = $true }
    }
    return $names
}

function Get-LocalRefNames {
    # 镜像仓本地 refs = 发布集合（fetch 侧同窄：main + tags），也是 -Prune 的保留集；
    # 差集 = 远端 refs − 本地 refs，即「不在发布集合里的 ref」。
    return @((Invoke-Git -Arguments @('-C', $MirrorDir, 'for-each-ref', '--format=%(refname)', 'refs/heads', 'refs/tags')).Output)
}

function Get-GithubOnlyRefs {
    # 「不在发布集合里」的 ref = 远端真实 ref 列表 − 镜像仓本地 refs。
    # 镜像仓本地 refs 就是发布集合（main + tags），所以这个差集正是 -Prune 要删的集合，
    # 也是 `-Status` 打印的那份 GitHub-only refs 清单——预览与删除用的是同一份数据。
    # 反向不成立：walgit 上的在途分支（未发布、从未去过 GitHub）不属于这个集合。
    $remoteNames = Get-RemoteRefNames
    if ($remoteNames.Count -eq 0) { return @() }
    $local = Get-LocalRefNames
    return @($remoteNames.Keys | Where-Object { $local -notcontains $_ })
}

function Show-GithubOnlyRefs {
    if (-not (Test-Path -LiteralPath (Join-Path $MirrorDir 'HEAD'))) {
        Write-Host 'github-only refs: 镜像仓还没建（先跑一次 -Once）'
        return
    }
    try {
        $onlyRemote = @(Get-GithubOnlyRefs | Sort-Object)
        Write-Host "github-only refs: $($onlyRemote.Count) 个（不在发布集合里的 ref；-Prune 会删除这些）"
        foreach ($name in $onlyRemote) { Write-Host "  - $name" }
    } catch {
        Write-Host "github-only refs: 查询失败（$($_.Exception.Message)）"
    }
}

if ($InstallTask -and $UninstallTask) { throw '-InstallTask 与 -UninstallTask 不能同时使用' }

if ($InstallTask) { Install-MirrorTask; exit 0 }
if ($UninstallTask) { Uninstall-MirrorTask; exit 0 }
if ($Status) { Show-MirrorStatus; exit 0 }

if (-not (Test-Path -LiteralPath $StateDir)) {
    New-Item -ItemType Directory -Force -Path $StateDir | Out-Null
}

$lock = $null
try {
    $lock = [System.IO.File]::Open($LockFile, [System.IO.FileMode]::OpenOrCreate, [System.IO.FileAccess]::ReadWrite, [System.IO.FileShare]::None)
} catch {
    Write-MirrorLog '已有镜像同步在跑（锁被占用），本轮跳过' 'WARN'
    exit 0
}

try {
    if ($Once) {
        $ok = Invoke-MirrorCycle
        if ($ok) { exit 0 } else { exit 1 }
    }
    Write-MirrorLog "镜像循环启动：$WalgitUrl → $GithubUrl（每 $IntervalSeconds 秒，mirror=$MirrorDir）"
    while ($true) {
        try {
            $ok = Invoke-MirrorCycle
        } catch {
            Write-MirrorLog "本轮镜像失败：$($_.Exception.Message)" 'ERROR'
            $ok = $false
        }
        if ($ok) {
            Write-MirrorLog ("同步完成 main={0}" -f (Get-MirrorTip))
        }
        Start-Sleep -Seconds $IntervalSeconds
    }
} catch {
    Write-MirrorLog "镜像失败：$($_.Exception.Message)" 'ERROR'
    exit 1
} finally {
    if ($lock) { $lock.Dispose() }
}
