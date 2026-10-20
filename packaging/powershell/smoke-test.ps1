# Smoke test for the OneUpDrive PowerShell module, without Pester and without
# a running agent: a fake agent answers on a pipe of its own, in a temporary
# data folder. Run: powershell -NoProfile -File packaging\powershell\smoke-test.ps1
# (or pwsh). Exits 0 when everything passed.

Set-StrictMode -Version Latest
$ErrorActionPreference = 'Stop'

$dir = Join-Path ([IO.Path]::GetTempPath()) ("oneup-ps-smoke-" + [Guid]::NewGuid().ToString('N'))
New-Item -ItemType Directory -Path $dir | Out-Null
$name = 'oneupdrive-' + [Guid]::NewGuid().ToString('N')
Set-Content -LiteralPath (Join-Path $dir 'agent.pipe') -Value "\\.\pipe\$name" -NoNewline -Encoding ascii

# The fake agent: one connection at a time, one call per connection, as the
# module makes them. It pauses, resumes, and finishes a check a moment after
# `sync_now`, which a `watch` then reports.
$server = [PowerShell]::Create()
[void]$server.AddScript({
    param($name)
    $state = @{ rev = 1; activity = 'idle'; local = $false; remote = $false; until = $null; checked = $null; pending = $false; auto = $null }
    $status = {
        $s = [ordered]@{
            version = 'fake'; rev = $state.rev; local = 'C:\Sync'; remote = '/'; activity = $state.activity
            paused = [ordered]@{ local = $state.local; remote = $state.remote }; push = $false
        }
        if ($state.until) { $s.paused.until_ms = $state.until }
        if ($state.checked) { $s.checked_ms = $state.checked }
        if ($state.auto) { $s.auto_pause = $state.auto }
        $s
    }
    $utf8 = New-Object System.Text.UTF8Encoding($false)
    $done = $false
    while (-not $done) {
        $pipe = New-Object System.IO.Pipes.NamedPipeServerStream($name, [System.IO.Pipes.PipeDirection]::InOut, 1)
        $pipe.WaitForConnection()
        $reader = New-Object System.IO.StreamReader($pipe, $utf8)
        $writer = New-Object System.IO.StreamWriter($pipe, $utf8)
        $writer.NewLine = "`n"
        $req = $reader.ReadLine() | ConvertFrom-Json
        switch ($req.method) {
            'pause' {
                if ($req.params.direction -ne 'remote') { $state.local = $true }
                if ($req.params.direction -ne 'local') { $state.remote = $true }
                if ($req.params.PSObject.Properties['minutes']) { $state.until = 1000 + 60000 * $req.params.minutes }
                $state.activity = if ($state.local -and $state.remote) { 'paused' } else { 'idle' }
                $state.rev++
            }
            'resume' {
                if ($req.params.direction -ne 'remote') { $state.local = $false }
                if ($req.params.direction -ne 'local') { $state.remote = $false }
                if (-not ($state.local -or $state.remote)) { $state.until = $null }
                $state.activity = 'idle'
                $state.rev++
            }
            'sync_now' { $state.activity = 'syncing'; $state.pending = $true; $state.rev++ }
            # Test hooks: the agent pauses itself / undoes it.
            'autopause' { $state.auto = $req.params.why; $state.activity = 'idle'; $state.rev++ }
            # The test is over.
            'quit' { $done = $true }
            'watch' {
                if ($state.pending -and $req.params.since -eq $state.rev) {
                    Start-Sleep -Milliseconds 300
                    $state.activity = 'idle'
                    $state.checked = [DateTimeOffset]::UtcNow.ToUnixTimeMilliseconds()
                    $state.pending = $false
                    $state.rev++
                }
            }
        }
        $reply = @{ jsonrpc = '2.0'; id = $req.id; result = (& $status) }
        $writer.WriteLine(($reply | ConvertTo-Json -Compress -Depth 5))
        $writer.Flush()
        $pipe.WaitForPipeDrain()
        $pipe.Dispose()
    }
}).AddArgument($name)
$running = $server.BeginInvoke()

$failed = 0
function Check([string]$what, [bool]$ok) {
    if ($ok) { Write-Host "ok    $what" } else { Write-Host "FAIL  $what"; $script:failed++ }
}

try {
    Import-Module (Join-Path $PSScriptRoot 'OneUpDrive') -Force

    $s = Get-OneUpStatus -DataDir $dir
    Check 'Get-OneUpStatus reads the status' ($s.activity -eq 'idle' -and $s.version -eq 'fake')

    $s = Suspend-OneUp -Minutes 5 -DataDir $dir
    Check 'Suspend-OneUp pauses both directions, timed' ($s.paused.local -and $s.paused.remote -and $s.paused.until_ms -eq 301000)

    $s = Resume-OneUp -Direction Remote -DataDir $dir
    Check 'Resume-OneUp resumes one direction' ($s.paused.local -and -not $s.paused.remote)

    $s = Resume-OneUp -DataDir $dir
    Check 'Resume-OneUp resumes the rest' (-not $s.paused.local -and -not $s.paused.remote)

    $start = [DateTimeOffset]::UtcNow.ToUnixTimeMilliseconds()
    $s = Wait-OneUpSync -SyncNow -TimeoutSeconds 30 -DataDir $dir
    Check 'Wait-OneUpSync -SyncNow waits for the check to finish' ($s.activity -eq 'idle' -and $s.checked_ms -ge $start)

    [void](Suspend-OneUp -DataDir $dir)
    $threw = $false
    try { Wait-OneUpSync -TimeoutSeconds 5 -DataDir $dir | Out-Null } catch { $threw = $_.Exception.Message -like '*paused*' }
    Check 'Wait-OneUpSync says when sync is paused' $threw
    [void](Resume-OneUp -DataDir $dir)

    # A one-direction pause leaves activity 'idle', but Wait-OneUpSync must
    # still report it (nothing on that side can finish).
    foreach ($dir2 in 'Local', 'Remote') {
        [void](Suspend-OneUp -Direction $dir2 -DataDir $dir)
        $threw = $false
        try { Wait-OneUpSync -TimeoutSeconds 5 -DataDir $dir | Out-Null } catch { $threw = $_.Exception.Message -like '*paused*' }
        Check "Wait-OneUpSync reports a $dir2-only pause" $threw
        [void](Resume-OneUp -DataDir $dir)
    }

    # Paused by itself (metered, battery, quiet hours): activity is 'idle',
    # but Wait-OneUpSync must still report it.
    & (Get-Module OneUpDrive) { param($d) Invoke-OneUpCall -Method autopause -Params @{ why = 'on a metered connection' } -DataDir $d } $dir | Out-Null
    $threw = $false
    try { Wait-OneUpSync -TimeoutSeconds 5 -DataDir $dir | Out-Null } catch { $threw = $_.Exception.Message -like '*by itself*metered*' }
    Check 'Wait-OneUpSync reports an automatic pause' $threw
    & (Get-Module OneUpDrive) { param($d) Invoke-OneUpCall -Method autopause -Params @{ why = $null } -DataDir $d } $dir | Out-Null

    $none = Join-Path ([IO.Path]::GetTempPath()) ("oneup-ps-none-" + [Guid]::NewGuid().ToString('N'))
    $threw = $false
    try { Get-OneUpStatus -DataDir $none | Out-Null } catch { $threw = $_.Exception.Message -like "*isn't running*" }
    Check 'an agent that isn''t running is reported' $threw
} finally {
    # Ends the fake agent's loop (it waits for a connection, which nothing
    # else interrupts).
    try { & (Get-Module OneUpDrive) { param($d) Invoke-OneUpCall -Method quit -DataDir $d } $dir | Out-Null } catch { }
    try { $server.EndInvoke($running) | Out-Null } catch { }
    $server.Dispose()
    Remove-Item -LiteralPath $dir -Recurse -Force -ErrorAction SilentlyContinue
}

if ($failed) {
    Write-Host "$failed failed."
    exit 1
}
Write-Host 'All passed.'
exit 0
