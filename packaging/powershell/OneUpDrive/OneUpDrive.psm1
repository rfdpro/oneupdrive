# OneUpDrive's background agent (oneupd) from PowerShell, through its control
# pipe: the JSON-RPC calls `oneup status`, `oneup pause` and the others use
# (docs/architecture/control-api.md). Only the Windows user who runs the agent
# can open the pipe.

Set-StrictMode -Version Latest

$script:PipePrefix = '\\.\pipe\oneupdrive-'

function Get-OneUpDataFolder {
    param([string]$DataDir)
    if ($DataDir) { return $DataDir }
    return (Join-Path $env:LOCALAPPDATA 'OneUpDrive')
}

# A property of a reply, or $null when the agent left it out (it leaves out
# what's empty).
function Get-Field {
    param($Object, [string]$Name)
    if ($null -eq $Object) { return $null }
    $p = $Object.PSObject.Properties[$Name]
    if ($p) { return $p.Value }
    return $null
}

function Invoke-OneUpCall {
    <#
    .SYNOPSIS
    Makes one JSON-RPC call to the agent and returns its result.
    #>
    [CmdletBinding()]
    param(
        [Parameter(Mandatory)][string]$Method,
        [hashtable]$Params,
        [string]$DataDir,
        [int]$TimeoutSeconds = 30
    )
    $dir = Get-OneUpDataFolder $DataDir
    $file = Join-Path $dir 'agent.pipe'
    if (-not (Test-Path -LiteralPath $file)) {
        throw "oneupd isn't running for $dir"
    }
    $full = (Get-Content -LiteralPath $file -Raw).Trim()
    if (-not $full.StartsWith($script:PipePrefix)) {
        throw "$file doesn't hold a OneUpDrive pipe name"
    }
    $name = $full.Substring('\\.\pipe\'.Length)
    # Identification only: the agent can tell who connects, never act as them.
    $pipe = New-Object System.IO.Pipes.NamedPipeClientStream(
        '.', $name, [System.IO.Pipes.PipeDirection]::InOut, [System.IO.Pipes.PipeOptions]::None,
        [System.Security.Principal.TokenImpersonationLevel]::Identification)
    try {
        try { $pipe.Connect(2000) } catch { throw "oneupd isn't running for $dir" }
        $utf8 = New-Object System.Text.UTF8Encoding($false)
        $writer = New-Object System.IO.StreamWriter($pipe, $utf8)
        $writer.NewLine = "`n"
        $writer.AutoFlush = $true
        $reader = New-Object System.IO.StreamReader($pipe, $utf8)
        $request = @{ jsonrpc = '2.0'; id = 1; method = $Method }
        if ($Params) { $request.params = $Params }
        $writer.WriteLine(($request | ConvertTo-Json -Compress -Depth 5))
        $read = $reader.ReadLineAsync()
        if (-not $read.Wait($TimeoutSeconds * 1000)) { throw 'no reply from oneupd' }
        $line = $read.Result
        if (-not $line) { throw 'oneupd closed the connection' }
        $reply = $line | ConvertFrom-Json
        $err = Get-Field $reply 'error'
        if ($err) { throw "oneupd: $($err.message)" }
        return (Get-Field $reply 'result')
    } finally {
        $pipe.Dispose()
    }
}

function Get-OneUpStatus {
    <#
    .SYNOPSIS
    The agent's status: what it's doing, pauses, safety stops, problems.
    .EXAMPLE
    (Get-OneUpStatus).activity
    #>
    [CmdletBinding()]
    param([string]$DataDir)
    Invoke-OneUpCall -Method status -DataDir $DataDir
}

function Suspend-OneUp {
    <#
    .SYNOPSIS
    Pauses sync: both directions, or only changes to this PC (Local) or to OneDrive (Remote).
    .EXAMPLE
    Suspend-OneUp -Minutes 30
    #>
    [CmdletBinding(SupportsShouldProcess)]
    param(
        [ValidateSet('Both', 'Local', 'Remote')][string]$Direction = 'Both',
        [ValidateRange(1, [int]::MaxValue)][int]$Minutes,
        [string]$DataDir
    )
    $params = @{ direction = $Direction.ToLowerInvariant() }
    if ($PSBoundParameters.ContainsKey('Minutes')) { $params.minutes = $Minutes }
    if ($PSCmdlet.ShouldProcess('OneUpDrive', "Pause ($Direction)")) {
        Invoke-OneUpCall -Method pause -Params $params -DataDir $DataDir
    }
}

function Resume-OneUp {
    <#
    .SYNOPSIS
    Resumes sync after Suspend-OneUp (or `oneup pause`).
    #>
    [CmdletBinding(SupportsShouldProcess)]
    param(
        [ValidateSet('Both', 'Local', 'Remote')][string]$Direction = 'Both',
        [string]$DataDir
    )
    if ($PSCmdlet.ShouldProcess('OneUpDrive', "Resume ($Direction)")) {
        Invoke-OneUpCall -Method resume -Params @{ direction = $Direction.ToLowerInvariant() } -DataDir $DataDir
    }
}

function Wait-OneUpSync {
    <#
    .SYNOPSIS
    Waits until the agent is up to date (idle), and returns its status.
    .DESCRIPTION
    Follows the agent with its `watch` call, which answers when the status
    changes, so it doesn't poll. With -SyncNow it first has the agent check
    both sides, and waits for that check to finish. Fails if sync is paused,
    a safety stop waits for you, or the time runs out.
    .EXAMPLE
    Wait-OneUpSync -SyncNow -TimeoutSeconds 600
    #>
    [CmdletBinding()]
    param(
        [int]$TimeoutSeconds = 600,
        [switch]$SyncNow,
        [string]$DataDir
    )
    $since = [DateTimeOffset]::UtcNow.ToUnixTimeMilliseconds()
    $deadline = (Get-Date).AddSeconds($TimeoutSeconds)
    $s = if ($SyncNow) { Invoke-OneUpCall -Method sync_now -DataDir $DataDir } else { Get-OneUpStatus -DataDir $DataDir }
    while ($true) {
        # A one-direction pause still reads activity 'idle', so check the
        # pause flags too: "up to date" must mean nothing is held back.
        $paused = Get-Field $s 'paused'
        if ($paused -and ($paused.local -or $paused.remote)) {
            throw 'Sync is paused (Resume-OneUp).'
        }
        # The agent can pause itself (metered connection, battery saver,
        # quiet hours): the activity still reads 'idle', so check for it.
        $auto = Get-Field $s 'auto_pause'
        if ($auto) {
            throw "Sync is paused by itself: $auto."
        }
        $checked = Get-Field $s 'checked_ms'
        $fresh = (-not $SyncNow) -or ($checked -and $checked -ge $since)
        if ($s.activity -eq 'idle' -and -not (Get-Field $s 'progress') -and $fresh) { return $s }
        switch ($s.activity) {
            'paused' { throw 'Sync is paused (Resume-OneUp).' }
            'waiting_for_you' { throw 'A safety stop waits for you (oneup status).' }
            'stopping' { throw 'oneupd is stopping.' }
        }
        $left = ($deadline - (Get-Date)).TotalSeconds
        if ($left -le 0) { throw "Not up to date after $TimeoutSeconds s (activity: $($s.activity))." }
        # Wait for the next status change, but never past the deadline: the
        # agent answers a watch within 50 s even if nothing changed, and the
        # reply itself may take a little longer.
        $wait = [Math]::Min(80, [int][Math]::Ceiling($left) + 5)
        $s = Invoke-OneUpCall -Method watch -Params @{ since = $s.rev } -DataDir $DataDir -TimeoutSeconds $wait
    }
}

Export-ModuleMember -Function Get-OneUpStatus, Suspend-OneUp, Resume-OneUp, Wait-OneUpSync
