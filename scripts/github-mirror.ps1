#Requires -Version 5.1
<#
.SYNOPSIS
  AeroDesk 的 GitHub 镜像循环：walgit 主仓的 heads + tags 单向镜像到 GitHub。

.DESCRIPTION
  canonical 仓库是 walgit（默认 http://127.0.0.1:8081/gqf2008/aerodesk.git）：issue / PR / 看板
  都在 refs/collab/*，GitHub 只做镜像 + 发版流水线。本脚本维护一个裸镜像仓
  （默认 ~/.walgit/mirror/aerodesk.git），每轮做两件事：

    1. 从 walgit fetch heads + tags（--prune），镜像仓的引用 = walgit 的现状；
    2. 把同样的 heads + tags push 到 GitHub（默认带 `+` 强制覆盖 = 纯镜像语义）。

  refs/collab/* 永不外流：两端的 refspec 只覆盖 refs/heads/* 与 refs/tags/*。
  推送默认**不删** GitHub 侧只存在的分支/标签（迁移前的历史 ref 不会被顺手删掉）；
  要让镜像与 walgit 完全对齐（含删除），显式加 -Prune。

  代理：GitHub 走本机 Clash 代理，端口会漂移，所以默认自动探测（-ProxyPort >
  AERODESK_GITHUB_PROXY / HTTPS_PROXY 环境变量 > 注册表 ProxyServer > 常见端口探测）；
  walgit 在 127.0.0.1，显式设置 NO_PROXY 保证不经代理。GitHub 的 git 凭据沿用全局
  credential helper（本机是 `gh auth git-credential`）。

.PARAMETER Once
  只跑一轮后退出（计划任务用这个）。默认是常驻循环，Ctrl+C 结束。

.PARAMETER NoForce
  推送不带 `+`：GitHub 侧一旦有 walgit 没有的提交（例如有人直推 GitHub），
  推送会被拒绝并在日志里报错，而不是被镜像覆盖。

.PARAMETER Prune
  推送带 `--prune`：删除 GitHub 上 walgit 已经没有的分支/标签，让镜像完全对齐 walgit。
  默认关闭（迁移前 GitHub 上的历史分支不会被自动删除）；哪些 ref 会被删见 `-Status`。

.PARAMETER InstallTask
  注册计划任务 walgit-sync-github-aerodesk：每 60 秒跑一轮 `-Once`。

.PARAMETER UninstallTask
  注销该计划任务。

.PARAMETER Status
  打印计划任务状态与最近的镜像日志。

.EXAMPLE
  pwsh -File scripts/github-mirror.ps1 -Once          # 手动同步一轮
  pwsh -File scripts/github-mirror.ps1                # 前台常驻循环
  pwsh -File scripts/github-mirror.ps1 -InstallTask   # 装成每分钟一次的定时任务
  pwsh -File scripts/github-mirror.ps1 -Status
#>
[CmdletBinding()]
param(
    [string]$WalgitUrl = 'http://127.0.0.1:8081/gqf2008/aerodesk.git',
    [string]$GithubUrl = 'https://github.com/aerodesk-labs/aerodesk.git',
    [string]$MirrorDir = (Join-Path $env:USERPROFILE '.walgit\mirror\aerodesk.git'),
    [string]$LogFile   = (Join-Path $env:USERPROFILE '.walgit\sync-to-github-aerodesk.log'),
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
    $current = (Invoke-Git -Arguments @('-C', $MirrorDir, 'config', '--get-all', 'remote.origin.fetch') -AllowFailure).Output
    foreach ($spec in @('+refs/heads/*:refs/heads/*', '+refs/tags/*:refs/tags/*')) {
        if ($current -notcontains $spec) {
            Invoke-Git -Arguments @('-C', $MirrorDir, 'config', '--add', 'remote.origin.fetch', $spec) | Out-Null
        }
    }
}

function Get-MirrorTip {
    $result = Invoke-Git -Arguments @('-C', $MirrorDir, 'rev-parse', '--verify', '--quiet', 'refs/heads/main') -AllowFailure
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
    $pushArgs = @('-C', $MirrorDir, 'push', '--porcelain')
    if ($Prune) { $pushArgs += '--prune' }
    $pushArgs += @('github', "$($force)refs/heads/*:refs/heads/*", "$($force)refs/tags/*:refs/tags/*")
    $push = Invoke-Git -Arguments $pushArgs -ViaProxy -AllowFailure
    if ($push.ExitCode -ne 0) {
        foreach ($line in $push.Output) { Write-MirrorLog "github push: $line" 'ERROR' }
        Write-MirrorLog "镜像推送失败（GitHub 侧可能有 walgit 没有的提交；要强制覆盖去掉 -NoForce）" 'ERROR'
        return $false
    }
    foreach ($line in $push.Output) {
        if ($line -match 'up to date') { continue }
        if ($line.Trim().Length -eq 0) { continue }
        Write-MirrorLog "github push: $($line.Trim())"
    }
    return $true
}

function Get-TaskInstallPath {
    if ($PSCommandPath) { return (Resolve-Path -LiteralPath $PSCommandPath).Path }
    throw '无法确定脚本自身路径（请用 -File 调用本脚本）'
}

function Install-MirrorTask {
    $scriptPath = Get-TaskInstallPath
    $action = New-ScheduledTaskAction -Execute 'powershell.exe' `
        -Argument ('-NoProfile -WindowStyle Hidden -ExecutionPolicy Bypass -File "{0}" -Once' -f $scriptPath)
    $trigger = New-ScheduledTaskTrigger -Once -At ((Get-Date).AddMinutes(1)) `
        -RepetitionInterval (New-TimeSpan -Seconds $IntervalSeconds) `
        -RepetitionDuration (New-TimeSpan -Days 3650)
    $settings = New-ScheduledTaskSettingsSet -AllowStartIfOnBatteries -DontStopIfGoingOnBatteries `
        -StartWhenAvailable -MultipleInstances IgnoreNew -ExecutionTimeLimit (New-TimeSpan -Minutes 10)
    Register-ScheduledTask -TaskName $TaskName -Action $action -Trigger $trigger -Settings $settings `
        -Description 'AeroDesk: mirror walgit heads/tags to the GitHub mirror (see docs/WALGIT.md)' -Force | Out-Null
    Write-MirrorLog "计划任务已注册：$TaskName（每 $IntervalSeconds 秒一轮，脚本 $scriptPath）"
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
        Write-Host "task            : $TaskName 未安装（pwsh -File scripts/github-mirror.ps1 -InstallTask）"
    }
    Write-Host "mirror dir      : $MirrorDir"
    Write-Host "walgit remote   : $WalgitUrl"
    Write-Host "github remote   : $GithubUrl"
    $script:ResolvedProxyPort = Resolve-ProxyPort -Explicit $ProxyPort
    Write-Host "proxy port      : $($script:ResolvedProxyPort)"
    Show-GithubOnlyRefs
    if (Test-Path -LiteralPath $LogFile) {
        Write-Host "--- last log lines ($LogFile) ---"
        Get-Content -LiteralPath $LogFile -Tail 10 | ForEach-Object { Write-Host $_ }
    }
}

function Show-GithubOnlyRefs {
    if (-not (Test-Path -LiteralPath (Join-Path $MirrorDir 'HEAD'))) {
        Write-Host 'github-only refs: 镜像仓还没建（先跑一次 -Once）'
        return
    }
    try {
        $remote = (Invoke-Git -Arguments @('-C', $MirrorDir, 'ls-remote', '--heads', '--tags', 'github') -ViaProxy).Output
        $remoteNames = @{}
        foreach ($line in $remote) {
            $parts = $line -split '\s+', 2
            if ($parts.Count -eq 2 -and $parts[1] -notmatch '\^\{\}$') { $remoteNames[$parts[1]] = $true }
        }
        $local = (Invoke-Git -Arguments @('-C', $MirrorDir, 'for-each-ref', '--format=%(refname)', 'refs/heads', 'refs/tags')).Output
        $onlyRemote = @($remoteNames.Keys | Where-Object { $local -notcontains $_ })
        Write-Host "github-only refs: $($onlyRemote.Count) 个（-Prune 会删除这些）"
        foreach ($name in ($onlyRemote | Sort-Object)) { Write-Host "  - $name" }
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
    Write-MirrorLog '已有镜像循环在跑（锁被占用），本轮跳过' 'WARN'
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
