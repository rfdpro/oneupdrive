# Events, hooks and scripting

The agent tells what it does as **events**, which `oneup watch` and scripts read over the control pipe, and which start **hooks**: programs the user names in `config.toml`. Code: `bins/oneup/src/events.rs` (the feed), `crates/oneup-hooks` (hook settings, where programs may be, the runner), `oneup_sys::contained` (starting a program contained), `bins/oneup/src/hooks.rs` (the agent's hooks: loading, notes, `oneup explain`, the settings window), `bins/oneup/src/hooks_cmd.rs` (`oneup watch`, `oneup hooks`), `packaging/powershell` (the PowerShell module). User guide: [Scripts and hooks](../user-guide/hooks.md).

## The event feed
`oneup_ipc::Event`: `seq` (from 1, per agent run), `at_ms`, `kind`, `path` (relative to the synced folder, `/a/b.txt`; empty for events about sync itself), `action`, `detail`.

| Kind | Made by |
|---|---|
| `synced` | A round's activity-log entries that are done uploads, downloads, creations, moves, renames, deletions, recycles, removals and prunes (`events::from_activity`). A move or rename's path is its destination |
| `conflict` | Done `keep both` entries, and uploads that went over an edit made in OneDrive meanwhile, which was kept next to them (`sync::KEPT_THEIRS`) |
| `failure` | Failed entries with a path |
| `safety_stop` | `Control::record` when the stops change, one per stop: what and where (`sync::breaker_what`), never the code that approves it |
| `paused`, `resumed` | `Control::publish` when the sides paused change (the user's pause or the agent's own), with which sides and the agent's reason |
| `problem`, `resolved` | `Control::set_problem` when a spell begins (a new kind), `Control::record` when it clears |

Item events are the activity log's lines, made where the round writes them (`Engine::rounds`, after `print_round`), so the feed says nothing the log doesn't, and costs one pass over a round's entries. Details are cut to 300 characters and lose anything that looks like a link (an error can quote a download link, which works as a credential for a while).

**Kept in memory, bounded.** `events::Feed` keeps the latest 4,096 events; a round that makes more keeps its newest. The pipe's `events` call ([control-api.md](control-api.md#calls)) pages through them: the events after `since`, waiting up to 50 s for one, at most 48 KiB a page (a reply line is 64 KiB at most), and `missed` when the reader fell behind what's kept or the agent restarted (its `seq` then starts again). Readers that need every item use the activity log. Hooks don't read pages: they're a **sink** of the feed and hear of every event as it's added.

`oneup watch` reads from "now" and prints a line per event, or one JSON object per line with `--json`; it ends when the agent does.

## Hooks
`config.toml`:

```toml
[hooks]
paused = false

[[hook]]
name = "thumbnails"
on = ["synced", "conflict"]
paths = ["/Photos/"]
program = 'C:\Tools\thumb.exe'
args = ["{path}"]
timeout_s = 60     # 1 to 3600
per_minute = 60    # 1 to 6000
coalesce_s = 2     # 0 to 3600
enabled = true
```

`config::load_hooks` reads them (a value of the wrong type or a name used twice fails the whole list; the agent then keeps the last good one, `config::kept`); `HookSpec::check` judges each hook's shape; a hook that fails a check is listed and refused, not dropped silently. At most 32 hooks. Each data folder (main pair, extra pair, vault) has its own `config.toml` and agent, so its own hooks.

**Matching.** An event starts a hook when its kind is in `on` and, if `paths` is set, the event has a path the rules match (`RuleSet::excludes`, the exclusion rules' matcher and syntax, so `/Photos/` covers everything inside). A hook with `paths` never fires for events without a path.

**Arguments** are a list: no shell, no command line to parse. Templates: `{event}`, `{path}` (the full local path), `{relpath}`, `{action}`, `{detail}`; `{{` is a plain `{`; anything else in braces is refused at load. `{path}`, `{relpath}` and `{detail}` carry text a change in OneDrive can set (a shared file can be named anything, `a&calc.exe&.txt` included), so a hook that gives them to `cmd.exe` or a batch file, or to PowerShell other than after `-File`, is refused: those re-parse their arguments as commands. Other programs get the text as one argument (quoted for the C runtime's parser, `oneup_sys::contained`).

### The runner
`oneup_hooks::Runner`, fed by the feed's sink on the round's own thread:

* **Offering** an event only matches and queues, under one lock, with no I/O: the round never waits on a hook. Cost: events × hooks matches.
* **Coalesced per item.** Each hook has at most one pending run per item (keyed by the folded path). Another event for the item replaces the pending one (the run gets the latest event, and counts the events folded in) and puts it off to `coalesce_s` after this event, but never more than 60 s beyond `coalesce_s` after the first, so an item that never settles still gets its run.
* **Rate-limited per hook**: a token bucket of `per_minute`, holding a tenth of a minute's worth. **One run at a time per hook, two in all**, on two threads in Windows' background mode, taking hooks in turn so one busy hook can't starve the others.
* **Bounded**: at most 16,384 runs wait across hooks. More are dropped and counted (`Note::Dropped`: the agent's output and the activity log). A burst of changes to items already waiting takes no room.
* **Paused** (`[hooks] paused`): nothing is queued and the queue is emptied; events while paused never start a hook later. A hook turned off is emptied the same way.
* **Reloaded** when `config.toml`'s time changes, read at each of the agent's wake-ups (`oneup hooks` and the settings window nudge the agent with `sync_now`). A hook whose settings didn't change keeps its queue and rate; the others start afresh.
* No polling: the threads wait on a condition variable, with a timeout only for the next due run or token.

### Starting a program contained
`oneup_sys::contained::run` creates the process itself (`CreateProcessW`), so that:

* it starts **in a job object** (`PROC_THREAD_ATTRIBUTE_JOB_LIST`), suspended until EcoQoS is set, so nothing it starts escapes the job; the job holds every process at below-normal priority, can't show the crash dialog (`DIE_ON_UNHANDLED_EXCEPTION`), and ends them all when it's closed (`KILL_ON_JOB_CLOSE`). When the program ends or `timeout_s` passes, the job is ended: whatever it started ends with it, and a crash of the agent ends them too. Breaking away isn't allowed.
* it inherits **exactly two handles** (`PROC_THREAD_ATTRIBUTE_HANDLE_LIST`): an empty standard input and a pipe for its output and errors. None of the agent's, whatever else is inheritable (the agent also stops its own standard handles being inheritable).
* its environment is the agent's **without `ONEUP_*` variables** (one can hold a vault's recovery key), its current folder is the program's own (never a synced folder, which a DLL search could reach), and there's no window.
* its output is read on a thread, the first 8 KiB kept.

`bins/oneup/src/hooks.rs` writes each run to `hooks.log` in the data folder (time, hook, event, path, how it ended, how long, the events folded in, its output), moved to `hooks.log.1` past 1 MiB. The agent's log gets each run at debug level only, so a burst doesn't flood it. The activity log gets what needs attention: a refused hook, failed runs (exit code not 0, timeout, couldn't start) at most once a minute per hook with a count of the ones in between, and dropped runs.

## Where a hook's program may be
The safety rule: **a change in the cloud never becomes code that runs** ([data-safety.md](data-safety.md) invariant 17). `oneup_hooks::program::check`:

1. A full path is taken as given. A bare name (`powershell.exe`) is looked up on `PATH` by OneUpDrive (absolute entries only; `.exe` added when there's no extension), never by Windows, and the file found is what starts, by its full path. Anything else (`tools\x.exe`, `.\x.exe`) is refused.
2. The file must exist; its path is resolved (`canonicalize`: symbolic links and junctions followed).
3. The path as written and the resolved path are each compared, component by component and case-insensitively (`coexist::is_within`), with every folder OneUpDrive syncs on the PC, as written and resolved: this account's main pair, vaults and extra pairs (`pairs::local_folders`) and every other data folder's (`datafolders::all`). Then against any Cloud Files sync root (`CfGetSyncRootInfoByPath`: the OneDrive app, Dropbox, iCloud, ours), and the OneDrive app's folders (`coexist::other_engine`).
4. Literal arguments that are absolute paths (whole, or after `=`) get the same check (`check_args`): `powershell.exe -File C:\OneDrive\x.ps1` runs a script a cloud change could replace. Template arguments are the event's data and aren't checked.

The check runs when the hooks load (and the synced folders are found again when `config.toml` changes, and at most every 10 minutes otherwise) and **again right before each start**, so a program moved into a synced folder (or a folder linked into one) after loading never runs; the hook is then refused until the settings change. The message names the place and what to do: "`C:\OneUpDrive\bin\x.exe` is in `C:\OneUpDrive` (your main synced folder): a change in the cloud could replace it, so it never runs as a hook. Move it to a folder that isn't synced."

## What "read-only towards us" means
Hooks run as the user, like any program the user starts. The pipe admits that user's processes, so a hook's program can call it, `oneup approve` included, exactly as any other program of the user's can. OneUpDrive gives a hook **nothing more** than that:

* no handle of the agent's (the handle list), so not its pipe connections, database or log;
* no approval codes: no event carries one, and no template gives one;
* no tokens or keys: hooks get arguments and the agent's environment without `ONEUP_*`; the pipe's name is in the data folder, readable by the user as before;
* no file contents: only paths and the event's words;
* no new call: the pipe's calls and who may make them are as before ([control-api.md](control-api.md#transport-and-who-may-connect)).

So hooks can't approve a safety stop by any path that other programs of the user's don't also have. A guarantee against the user's own programs would need a separate account or a sandbox (AppContainer), which hooks would then also need for the files they work on: not done.

## Cost
* With no hooks and no reader, the feed costs one pass over a round's activity entries and the latest 4,096 events in memory (paths included: under 2 MB in practice).
* A burst queues at most 16,384 runs per agent: a few MB.
* Starting a contained program costs about 60 ms of a short-lived process (`cmd.exe /c exit 0`), most of it the program's own start-up at below-normal priority.
* Measured with the in-memory OneDrive (`crates/oneup-exec/tests/sync.rs`, `bench_hooks_under_a_10000_file_burst`, release build): a 10,000-file burst in ten waves, with and without a hook that starts a program for every file, medians of three runs each: sync took 32.3 s without the hook and 30.9 s with it (the difference is noise), and every file's hook ran exactly once. With the CPU fully used by other programs, the hook's below-normal programs gave way: 1 to 13 ran during each 30 s sync, and the 10,000 finished 26 minutes later. On an otherwise idle CPU a run takes about 60 ms, so two at a time keep up with about 30 files a second alongside sync; that case is still to be measured with this benchmark. `everyday_a_hook_fires_once_per_coalesced_change` checks that every item's run starts once when it changes twice within the hook's coalescing time.

## Limits
* A hard link outside the synced folders to a file inside one isn't recognised (it resolves to itself). Neither are DLLs or scripts a program loads by itself from a synced folder.
* A hook's program can start programs outside its job through other programs (Task Scheduler, a COM server, Explorer); those aren't ended with it.
* Events are kept in memory: a restart begins a new feed. Events made while a reader was disconnected are only in the activity log.
* Events of a vault pair or an extra pair come from that pair's own agent; `oneup watch --data-dir` reads one at a time.
