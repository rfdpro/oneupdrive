# OneUpDrive PowerShell module

Controls OneUpDrive's background agent (`oneupd`) from PowerShell, through its control pipe. Windows PowerShell 5.1 and PowerShell 7 both work.

| Command | What it does |
|---|---|
| `Get-OneUpStatus` | The agent's status: what it's doing, pauses, safety stops, problems |
| `Suspend-OneUp [-Direction Both\|Local\|Remote] [-Minutes N]` | Pauses sync, like `oneup pause` |
| `Resume-OneUp [-Direction Both\|Local\|Remote]` | Resumes it, like `oneup resume` |
| `Wait-OneUpSync [-SyncNow] [-TimeoutSeconds N]` | Waits until the agent is up to date; fails if sync is paused or a safety stop waits for you |

Each takes `-DataDir` for a data folder other than the default (`%LOCALAPPDATA%\OneUpDrive`).

```powershell
Import-Module .\packaging\powershell\OneUpDrive
Suspend-OneUp -Minutes 30
Resume-OneUp
Wait-OneUpSync -SyncNow -TimeoutSeconds 600
(Get-OneUpStatus).activity
```

There is no command to approve a safety stop: that stays with `oneup approve`, the tray icon and the settings window.

For events as they happen, use `oneup watch --json` (one JSON object per line):

```powershell
oneup watch --json | ForEach-Object { $e = $_ | ConvertFrom-Json; if ($e.kind -eq 'conflict') { Write-Host $e.path } }
```

## Test

`smoke-test.ps1` runs every command against a fake agent on a pipe of its own, in a temporary folder. It needs no running agent and no Pester:

```powershell
powershell -NoProfile -File packaging\powershell\smoke-test.ps1
```
