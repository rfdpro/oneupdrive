# How files are used: programs, space policies, largest files, the content guard

Which programs change and open your files, freeing space from files you don't use, the largest files, and a circuit breaker for edits that look like ransomware. User guide: [the Apps page](../user-guide/settings-window.md#apps), [Files On-Demand](../user-guide/files-on-demand.md) and [the content guard](../user-guide/syncing.md#the-content-guard-ransomware).

All four read or write the store's usage tables (`oneup_store::usage`), which are bounded and indexed so cost follows activity, not the size of the tree:

| Table / index | Holds | Bound |
|---|---|---|
| `app_use (hour, app, kind)` | files and bytes per hour, program and kind (changed / opened) | 90 days (`app_use_prune`) |
| `app_file` | the latest files used: time, program, changed/opened, path relative to the synced folder, size | 5,000 rows |
| `local_used (file_id)` | the last time OneUpDrive downloaded a file for a program | one row per file; rows of files gone from the Local tree are trimmed a thousand at a time |
| index `local_by_age (mtime, file_id)` | placeholders whose content is here, neither pinned nor unpinned | partial index |
| `remote_largest` | a query on the `remote_item_copyable` size index (files ≥ 1 MiB with a hash) | `LIMIT n`; reads the whole table only on a drive with fewer than `n` such files |

## Programs (`oneup_app::apps`)
* **Opened**: each `FETCH_DATA` request carries the requesting process (`CF_CALLBACK_INFO.ProcessInfo`) and the placeholder's file ID (`Fetch::file_id`). On a completed hydration, `oneup_exec::hydrate` adds an `opened` count for the program and sets `local_used` for the file. Exact.
* **Changed**: journal records don't say who wrote. The agent's journal watcher (`oneupd::watch_journal`) parses the records it waits for and hands each file with a content change (`DATA_OVERWRITE | DATA_EXTEND | DATA_TRUNCATION`) to the `Tracker` thread over a bounded channel (`try_send`: a full channel drops the change, never blocks the watcher). Wake-ups go through a separate `journal-wake` thread that keeps the agent's 500 ms gap, so with the tracker on, the watcher reads again after 50 ms (`TRACK_GAP`) and quick writers are still caught. The tracker resolves the path by file ID, ignores anything outside the synced folder and in `.oneup`, and identifies the writer from two sources:
  * **Windows' Restart Manager** (`oneup_sys::in_use`, documented, unelevated): which processes have the file open. At most 20 lookups a second; writes are de-duplicated per file for 30 s.
  * **Closes**: the provider registers `CF_CALLBACK_TYPE_NOTIFY_FILE_CLOSE_COMPLETION` (a notification: Windows doesn't wait for it), which names the closing process; `oneup_cfapi::closes` keeps the latest 4 closes per file for 60 s, at most 8,192 files, never our own process. Windows sends these only for placeholders, so a new file isn't covered until it's synced.

  The tracker takes the first program that isn't a usual reader (search indexer, Defender, Explorer) from those with the file open and those that closed it from 2 s before it heard of the write; if there's none, the change waits up to 3 s (`SETTLE`, at most 4,096 waiting) for a close, and is otherwise "not identified". Counts are flushed to the store every 15 s (only when there are some; pruning every 10 minutes).
* **Online-only files are never looked up:** Restart Manager downloads the whole file, even from our own process (`oneup-cfapi` test `what_downloads_an_online_only_file_from_the_providers_process`), and such a "write" is OneUpDrive's own (creating or freeing a placeholder). The watcher drops records whose attributes say online-only, and the tracker checks again by attributes alone (`symlink_metadata`, no data access) just before the lookup.
* **OneUpDrive's own writes aren't a program's.** The writers note the file in `oneup_cfapi::ours` (in memory, 2 minutes, at most 8,192 files): `place_download`, the Files On-Demand effects (`replace_placeholder`, `free_up`, `mark_uploaded`) and the hydration service; the tracker skips noted files. A hydration requested by our own process (keeping a file on this device) is logged as such and not counted as a use.
* When the watcher falls more than 4 MiB of records behind, it skips to the end (the observer reads everything anyway; only attribution is lost).
* `[agent] track_apps` (default on; settings window **Apps**; re-read each flush) turns noting programs off.
* Shown by `oneup apps [--days N]` (`--files [PROGRAM]` lists the latest 100 files) and the settings window's **Apps** page, which reloads every 15 s while shown: stacked columns (hours for a day, days otherwise) for the five busiest programs plus "Others", in a categorical palette validated for light and dark backgrounds, with the legend and table carrying the labels; a program's row lists its latest 50 files. Paths stay on this PC, like the activity log's; never contents.

## Space policies (`oneup_app::space`)
`config.toml` `[space]`: `free_unused_days` (0 = never), `free_when_low`, `low_disk_percent` (default 10). Also `oneup space` and the window's Files page (a folder's details, **Free up space**).
* **Used** = the latest of last write (`mtime`), last access (`std::fs::metadata().accessed()`, meaningful only where the volume keeps access times), and `local_used`.
* After a settled round, `Engine::placement` asks `space::select` for a batch: hourly for "unused for N days"; at every pass while the disk has less than `low_disk_percent` free, then the files unused longest (and unused for at least a day) until it has 5 points more.
* Candidates come from `local_by_age` (oldest first, keyset pages of 256), at most 2,000 looked at and 64 freed per pass, continuing from a saved cursor (`meta space.cursor`) so files skipped as recently used aren't re-read each pass. Empty files never qualify (an empty file looks freed already).
* Each becomes an `Op::Dehydrate` (only with a content hash on record), reviewed by the gate (a paused "this PC" withholds it) and run by the effects layer, which frees a file only after verifying its content under an exclusive handle (data-safety invariant 11). Pinned files never qualify (excluded by the index). Logged as an activity-log note.

## Largest files
`settingsapi::largest` (`oneup largest`, and the settings window's `largest` request): the `n` largest files in the mirror, their OneDrive path, and whether each is here, online-only (`FILE_ATTRIBUTE_RECALL_ON_DATA_ACCESS`, read with placeholders exposed), not here yet, or outside the synced folder. Never reads content. **Free up space** / **Keep on this PC** are `oneup free` / `oneup pin`.

## The content guard
A circuit breaker on content, in the gate (`oneup_gate::BreakerKind::Suspicious`, [sync-engine.md § Safety gate](sync-engine.md#safety-gate-and-effects)):
* Planning (`sync::judge_content`) reads the first 4 KiB of each **edited** file about to replace its OneDrive copy (`Op::Upload { replace: Some(_) }`; at most 2,000 per round) through `LocalFs::sample` (read-only, the upload's own identity and placeholder checks), and `oneup_gate::content::looks_wrong` (pure) judges it against its extension: a signature for zip-based Office files, OLE Office files, PDF, RTF, images, audio and video, archives, executables and SQLite; plain text for text and source files (more than 2% control bytes fails; UTF-16 with a byte-order mark passes). Unknown types are never judged, so it can't get in the way of unusual work.
* The gate trips when more than `max_suspicious` (default 20) such uploads come in a round, counting the agent's 15-minute window (`Recent::suspicious`). Only those uploads are withheld (everything stops in a tripped round anyway), with their own fingerprint, approved like any breaker.
* The tray has its own notification ("Files may have been encrypted") and review text; `SafetyStop.kind = "content"` in the control API.
* Thresholds, including the ordinary breakers': `oneup safety`, the window's **Safety** page (store `meta safety.*`).
* Why only edits: encrypting in place is what ransomware does, and an edit is the only case with an earlier version in OneDrive worth protecting. Ransomware that writes new files and deletes the originals trips the ordinary deletion breaker.
