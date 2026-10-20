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
| `args` | Fixed arguments (switches, a script path). Templates (`{path}` and the others below) are allowed **only when `program` is a plain `.exe`** that isn't an interpreter; see below | none |
| `timeout_s` | It's stopped after this many seconds, with anything it started | 60 |
| `per_minute` | It starts at most this often | 60 |
| `coalesce_s` | It waits this long after an item's last change, so a file saved ten times in a row starts it once | 2 |
| `enabled` | `false` turns it off | `true` |

OneUpDrive reads the hooks again when `config.toml` changes.

### How the event reaches your program
Every hook gets the event two ways, so your program never has to take it from the command line:

* **Environment variables:** `ONEUPHOOK_EVENT` (`synced`, `conflict`, …), `ONEUPHOOK_PATH` (the item's full path on this PC), `ONEUPHOOK_RELPATH` (inside your synced folder), `ONEUPHOOK_ACTION` (`upload`, `download`, …), `ONEUPHOOK_DETAIL`.
* **Standard input:** one JSON line, `{"event":...,"path":...,"relpath":...,"action":...,"detail":...,"seq":...,"at_ms":...}`.

On top of that, if `program` is a plain `.exe` that isn't an interpreter, you may put **templates** in `args`: `{event}`, `{path}`, `{relpath}`, `{action}`, `{detail}` (and `{{` for a literal `{`). OneUpDrive fills them in before starting it.

For an interpreter or a script (`cmd.exe`, `powershell.exe`, `python.exe`, a `.bat`, a `.ps1`, …), templates in `args` are **refused**: a file name or message from the cloud could contain characters the interpreter runs as a command. Read the environment variables instead. For example, a PowerShell hook:

```toml
[[hook]]
name = "notify"
on = ["conflict"]
program = "powershell.exe"
args = ["-NoProfile", "-File", 'C:\Tools\notify.ps1']
```
```powershell
# notify.ps1 — the event is in the environment, never on the command line.
"$env:ONEUPHOOK_EVENT on $env:ONEUPHOOK_RELPATH" | Out-File -Append C:\Tools\hooks.log
```

A Python hook:

```toml
[[hook]]
name = "thumbnail"
on = ["synced"]
paths = ["/Photos/"]
program = "python.exe"
args = [r"C:\Tools\thumb.py"]
```
```python
# thumb.py
import os, json, sys
event = json.loads(sys.stdin.readline())   # or os.environ["ONEUPHOOK_PATH"]
make_thumbnail(os.environ["ONEUPHOOK_PATH"])
```

A plain `.exe` of your own may take the path as an argument, since it receives it as data, not a command:

```toml
program = 'C:\Tools\thumbnail.exe'
args = ["--out", 'C:\Thumbs', "{path}"]
```

A `.bat` or `.cmd` works as the program directly (read `%ONEUPHOOK_PATH%`); for a `.ps1`/`.vbs`/`.js`, name its interpreter (`powershell.exe -File …`, `wscript.exe …`).

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

* its program, or a script it runs (a `.bat`, or a `-File`/positional script given to an interpreter), is in a folder OneUpDrive syncs (any account's, any pair's or vault's), in a folder another sync app manages (the OneDrive app, Dropbox, …), or reached through a link into one — including a second hard link into such a folder;
* its program is given as a relative path (`tools\x.exe`), or a script path is drive-relative (`C:script.py`): give a full path;
* it puts a template (`{path}`, …) in `args` while `program` is an interpreter or a script (use the environment variables instead, as above);
* OneUpDrive can't list all the folders it syncs just then (it waits and tries again rather than risk missing one).

It checks when the hooks load and again right before each start, against the folders read fresh then. Move the program to a folder that isn't synced, like `C:\Tools`. OneUpDrive also starts a hook with synced folders removed from its `PATH` and in a safe working folder, so it can't pick up a cloud-controlled helper by accident.

Hooks get the event's words and paths, nothing else: never a file's contents, your sign-in, a safety stop's approval code, or OneUpDrive's own settings in the environment (`ONEUP_*` is removed).

**Hooks run as you.** A hook's program can do anything you can: open your files, read what only you can read. What it *can't* do is use OneUpDrive's own controls: the channel that `oneup approve`, pause and status use refuses hook programs (and anything they start), so a hook can't approve a safety stop or read one's code. That aside, OneUpDrive gives a hook nothing a program you double-click doesn't already have — including that files only you can read (Windows protects them per user, not per program) stay readable. Only add programs you trust, as you would for anything that starts by itself.

## PowerShell
The module in `packaging\powershell` has `Get-OneUpStatus`, `Suspend-OneUp`, `Resume-OneUp` and `Wait-OneUpSync`:

```powershell
Import-Module .\packaging\powershell\OneUpDrive
Suspend-OneUp -Minutes 30
Resume-OneUp
Wait-OneUpSync -SyncNow    # until up to date; fails if paused or a safety stop waits for you
```

Its [README](../../packaging/powershell/README.md) has the details.
