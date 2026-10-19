<#
.SYNOPSIS
Runs one simulation test over a range of seeds, split across processes.

.DESCRIPTION
Builds the test binary once (release), then runs it in -Processes processes
at the same time, each over its own slice of [From, From + Count), using the
simulations' SIM_SEED_FROM / SIM_SEEDS variables. Other SIM_* variables
(SIM_STORE) pass through from the environment.

A failing seed is printed by the test itself ("replay with SIM_SEED=n");
those lines are collected into <LogDir>/failures.txt, with the command that
replays each one, and the script exits 1.

.EXAMPLE
pwsh .github/scripts/sim.ps1 -Package oneup-sync -Test sim `
  -Name random_histories_converge_without_losing_edits -From 1 -Count 100000
#>
param(
    [Parameter(Mandatory)][string]$Package,
    [Parameter(Mandatory)][string]$Test,
    [Parameter(Mandatory)][string]$Name,
    [Parameter(Mandatory)][UInt64]$From,
    [Parameter(Mandatory)][UInt64]$Count,
    [int]$Processes = [Environment]::ProcessorCount,
    [string]$LogDir = "sim-logs"
)
$ErrorActionPreference = 'Stop'

$exe = cargo test --release -p $Package --test $Test --no-run --message-format=json |
    ForEach-Object { $_ | ConvertFrom-Json } |
    Where-Object { $_.reason -eq 'compiler-artifact' -and $_.executable -and $_.target.name -eq $Test } |
    Select-Object -Last 1 -ExpandProperty executable
if ($LASTEXITCODE -ne 0 -or -not $exe) { throw "building $Package's $Test test failed" }

New-Item -ItemType Directory -Force $LogDir | Out-Null
$per = [UInt64][Math]::Ceiling($Count / $Processes)
$runs = @()
$start = Get-Date
for ($i = 0; $i -lt $Processes; $i++) {
    $first = $From + [UInt64]$i * $per
    $n = [Math]::Min($per, $From + $Count - $first)
    if ($first -ge $From + $Count -or $n -le 0) { break }
    # Children inherit the environment as it is when they start.
    $env:SIM_SEED_FROM = "$first"
    $env:SIM_SEEDS = "$n"
    $log = Join-Path $LogDir "$Name-$i"
    $p = Start-Process -FilePath $exe -ArgumentList '--exact', $Name -NoNewWindow -PassThru `
        -RedirectStandardOutput "$log.out" -RedirectStandardError "$log.err"
    $null = $p.Handle # keeps the exit code readable after the process ends
    $runs += [pscustomobject]@{ Process = $p; Log = $log; First = $first; Count = $n }
}
Remove-Item Env:SIM_SEED_FROM, Env:SIM_SEEDS
$runs.Process | Wait-Process
$secs = ((Get-Date) - $start).TotalSeconds

$failed = $runs | Where-Object { $_.Process.ExitCode -ne 0 }
$store = if ($env:SIM_STORE) { "`$env:SIM_STORE='$($env:SIM_STORE)'; " } else { '' }
$summary = "{0} {1}: seeds {2}..{3}, {4} processes, {5:N1} s ({6:N0} seeds/s)" -f `
    $Package, $Name, $From, ($From + $Count - 1), $runs.Count, $secs, ($Count / [Math]::Max($secs, 0.001))
Write-Host $summary
if ($env:GITHUB_STEP_SUMMARY) { "* $summary" | Add-Content $env:GITHUB_STEP_SUMMARY }
if (-not $failed) { exit 0 }

$lines = foreach ($r in $failed) {
    Write-Host "::group::seeds $($r.First) to $($r.First + $r.Count - 1) failed"
    Get-Content "$($r.Log).out", "$($r.Log).err" | Select-Object -Last 80 | Write-Host
    Write-Host '::endgroup::'
    $seeds = Select-String -Path "$($r.Log).out", "$($r.Log).err" -Pattern 'replay with SIM_SEED=(\d+)' |
        ForEach-Object { $_.Matches[0].Groups[1].Value } | Sort-Object -Unique
    if ($seeds) {
        foreach ($s in $seeds) {
            "$Package ``$Name``, seed $s. Replay (PowerShell): ``$($store)`$env:SIM_SEED='$s'; `$env:SIM_DEBUG='1'; cargo test --release -p $Package --test $Test $Name``"
        }
    } else {
        "$Package ``$Name`` failed in seeds $($r.First) to $($r.First + $r.Count - 1) without naming a seed (exit code $($r.Process.ExitCode)); see the job log."
    }
}
$lines | Add-Content (Join-Path $LogDir 'failures.txt')
$lines | Write-Host
exit 1
