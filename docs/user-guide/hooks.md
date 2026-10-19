# Scripts and hooks

OneUpDrive can tell other programs what it does, and start a program of yours when something happens: make a thumbnail when a photo arrives, send yourself a message when a safety stop needs you, copy a report somewhere once it's synced. Three ways in:

* **`oneup watch`** shows what happens as it happens, for you or for a script.
* **Hooks** start a program of yours on chosen events, by themselves.
* **A PowerShell module** reads the status, pauses, resumes and waits for sync.

## Watching what happens
```
oneup watch
```
prints a line for each event until you press Ctrl+C:

```
2026-05-04 10:12:31  synced      /Photos/beach.jpg  (upload)
2026-05-04 10:12:33  conflict    /notes.txt  (keep both; -> notes (PC).txt; changed on both sides)
2026-05-04 10:15:02  paused      (both directions; by itself: on a metered connection)
```

`oneup watch --json` prints one JSON object per line instead, for scripts: `seq`, `at_ms` (Unix time in milliseconds), `kind`, `path` (inside your synced folder), and `action` and `detail` when there are any.

| Event | When |
|---|---|
| `synced` | A file or folder was uploaded, downloaded, created, moved, renamed or removed |
| `conflict` | It changed on both sides, so both versions were kept |
| `failure` | Something couldn't be done to an item (it's tried again later) |
| `safety_stop` | A safety stop needs you (see [Syncing in depth](syncing.md)) |
| `paused`, `resumed` | Sync paused or resumed, by you or by itself |
| `problem`, `resolved` | A problem began, or cleared (see [When something goes wrong](troubleshooting.md)) |

OneUpDrive keeps the latest few thousand events in memory. If a program reading them falls further behind (a first sync of a big folder can make tens of thousands at once), `oneup watch` says some were missed; `oneup activity` has everything.

## Hooks
A hook is a program OneUpDrive starts when an event happens. Add a `[[hook]]` section to `config.toml` in your data folder (`%LOCALAPPDATA%\OneUpDrive`), one per hook:

```toml
[[hook]]
name = "thumbnails"
on = ["synced"]
paths = ["/Photos/", "*.jpg"]
program = 'C:\Tools\thumbnail.exe'
args = ["--out", 'C:\Thumbs', "{path}"]
```

| Setting | Means | Default |
|---|---|---|
| `name` | What you call it (each hook needs its own) | |
| `on` | The events that start it (the table above) | |
| `paths` | Only items these rules match, written like [exclusion rules](excluding-files.md). Leave it out for every item, and for events that aren't about an item (pauses, problems, safety stops) | every item |
| `program` | The program's full path, or just its name if it's on your `PATH` (like `powershell.exe`) | |
| `args` | What it gets, with these filled in: `{event}`, `{path}` (the item's full path on this PC), `{relpath}` (inside your synced folder), `{action}` (`upload`, `download`, …), `{detail}`. `{{` is a plain `{` | none |
| `timeout_s` | It's stopped after this many seconds, with anything it started | 60 |
| `per_minute` | It starts at most this often | 60 |
| `coalesce_s` | It waits this long after an item's last change, so a file saved ten times in a row starts it once | 2 |
| `enabled` | `false` turns it off | `true` |

OneUpDrive reads the hooks again when `config.toml` changes. To run a PowerShell script, use `program = "powershell.exe"` and `args = ["-NoProfile", "-File", 'C:\Tools\photo.ps1', "{path}"]`.

### What a hook runs like
* In the background: below normal priority, power-saving, no window. It doesn't slow syncing down: OneUpDrive never waits for a hook.
* One at a time per hook, two at most in all. Events wait their turn; if more than 16,384 pile up, the newest are dropped, and the activity log says how many.
* What it prints goes to `hooks.log` in your data folder, with how each run ended. Failed runs, refused hooks and dropped events also show in the activity log (`oneup activity`, the settings window's Activity page).
* When it ends, so does anything it started. Start long-running programs another way.

### Turning hooks on and off
* `oneup hooks` lists them, with any reason one doesn't run.
* `oneup hooks pause` and `oneup hooks resume` stop and restart all of them. While paused, events don't start them later either.
* `oneup hooks off NAME` and `oneup hooks on NAME` turn one off and on.
* The settings window's **General** page has the same: **Run hooks**, and a switch for each hook.
* `oneup explain PATH` names the hooks a change to that item starts.

### What hooks can't do
A change in OneDrive must never become a program running on your PC. So OneUpDrive refuses a hook, and says why, when:

* its program is in a folder OneUpDrive syncs (any account's, any pair's or vault's), in a folder another sync app manages (the OneDrive app, Dropbox, …), or reached through a link into one;
* an argument names a file in such a folder (a script for `powershell.exe`, say);
* its program is given as a relative path (`tools\x.exe`): give the full path;
* it would hand a file's name to `cmd.exe`, a batch file, or PowerShell without `-File`. These read their arguments as commands, and a file someone shared with you could be named to run one.

It checks when the hooks load and again right before each start. Move the program to a folder that isn't synced, like `C:\Tools`.

Hooks get the event's words and paths as arguments, nothing else: never a file's contents, your sign-in, a safety stop's approval code, or OneUpDrive's own settings in the environment.

**Hooks run as you.** Anything you can do, a hook's program can do too: open your files, run `oneup approve`, change your settings. OneUpDrive gives it no more than that, and no way to answer a safety stop that any other program you run doesn't have. Only add programs you trust, as you would for anything that starts by itself.

## PowerShell
The module in `packaging\powershell` has `Get-OneUpStatus`, `Suspend-OneUp`, `Resume-OneUp` and `Wait-OneUpSync`:

```powershell
Import-Module .\packaging\powershell\OneUpDrive
Suspend-OneUp -Minutes 30
Resume-OneUp
Wait-OneUpSync -SyncNow    # until up to date; fails if paused or a safety stop waits for you
```

Its [README](../../packaging/powershell/README.md) has the details.
