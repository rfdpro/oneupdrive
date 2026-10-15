# The Local tree (folder scan and change journal)

`crates/oneup-local`, `crates/oneup-ntfs`, `crates/oneup-store`; CLI `oneup scan` and `oneup observe`. Scanning and observing are read-only: folders are opened only to *list* them and items are read by file ID with attribute access, so file contents are never read and online-only files are never downloaded.

The Local tree is OneUpDrive's record of *what is in a folder on this PC*: names, parents, sizes, timestamps, attributes and 128-bit file IDs. It is one of the three trees the reconciler compares ([sync-engine.md](sync-engine.md)), and it also serves:
1. **Enumeration hints** for the remote mirror: the scan finds exactly where excluded folders are, so the mirror can list the way to them and skip them without downloading their contents ([remote-mirror.md § Enumeration hints](remote-mirror.md#enumeration-hints)).
2. **Observe mode**: compared with the Remote tree, it shows what a first sync *would* do, without doing anything ([planner.md](planner.md), `oneup plan`, `oneup migrate check`).

## How a scan works
| Step | What | Why |
|---|---|---|
| Identify the root | Open the folder by path once; record the volume serial, the root's file ID and normalized path | Data-safety invariant 4: a different folder at the same path (or another disk) must never be mistaken for this one |
| Bookmark the journal | Record the change journal's ID and next USN **before** listing anything | Changes made during the scan are replayed afterwards instead of rescanning |
| Queue | Folders to list live in `local_scan_queue`, taken deepest first | Resumable; the queue stays small |
| List | Open each folder **by file ID**; list it with `FileIdExtdDirectoryInfo` (one call returns many entries with IDs, sizes, times, attributes and reparse tags) | No `MAX_PATH` limit, unaffected by renames during the scan, no file opened |
| Commit | A step's entries, new subfolders and dequeues are written in **one transaction** | A crash or Ctrl-C loses at most one step; re-listing a folder upserts the same rows |

## What is not descended into
Each of these is stored as **one row** that says why its contents weren't listed (`local_item.skip`):

| Case | Recorded as | Reason |
|---|---|---|
| Folder matching an exclusion rule | `skip = excluded` | The user chose to leave it out. Mirrors `remote_item.is_excluded` |
| Cloud placeholder folder not yet populated | `skip = unpopulated` | Listing it would make its cloud provider (e.g. the OneDrive app) download its file list. Detected from attributes *and* the Cloud Files `PARTIAL` state; both clear once the folder is listed |
| Folder that can't be opened or listed | `skip = unreadable` | Usually access denied; also a folder deleted while queued. Counted and reported, never fatal |
| Symlink, junction, mount point, or a folder reparse point of an unknown kind (ProjFS, container layers, …) | `kind = link` | Its contents live elsewhere, may be virtualised by another driver, or loop back into the tree |

Files are always recorded, including online-only placeholders (from their attributes alone).

**Placeholders must be exposed first.** Windows disguises cloud placeholders from our process by default: the reparse attribute and tag vanish from listings and handles alike. `Scan::open` calls `oneup_cfapi::expose_placeholders()` before looking at anything. Scans made without it are recognised by the missing `local_scan_format` marker and are started over rather than resumed.

**Hard links** (one file, several names) appear once, under the first name seen; later names are counted as duplicates and refused by sync. NTFS folders can't be hard-linked, so this can't create cycles.

## Another sync app on the same folder
Two sync engines on one folder fight and can destroy data. The scan checks whether the folder is inside a registered Cloud Files sync root (any provider: the OneDrive app, Dropbox, iCloud, …; OneUpDrive's own is recognised, [files-on-demand.md](files-on-demand.md)) and whether it is inside or *contains* the folders named by `%OneDrive%`, `%OneDriveConsumer%` and `%OneDriveCommercial%`. The result is shown to the user and stored in `local_root.sync_root_provider`. Reading such a folder is allowed (it's how moving over from the OneDrive app starts); anything that would *manage* it refuses.

## Data model (`oneup-store`)
| Table | Contents |
|---|---|
| `local_root` | The one scanned folder: path, identity, rules fingerprint, journal bookmark, start/finish times, other sync engine |
| `local_item` | One row per entry, keyed by the 128-bit file ID (16-byte little-endian BLOB); `parent_id`, `name`, `name_lossy`, `kind`, `size`, `mtime` (FILETIME), `attributes`, `reparse_tag`, `skip` |
| `local_scan_queue` | Folders still to be listed, with their depth |

A new scan **replaces** the stored one when the folder's identity or the exclusion rules differ, or when the stored scan is complete. An unfinished scan of the same folder with the same rules is resumed. `oneup mirror --rebuild` clears only the Remote tree, so a scan survives it.

**Invariants** (checked by `oneup verify` and in tests): at most one root, matching `local_root`; every non-root row's parent is a stored, listed folder; only folders are skipped; every queued folder is a stored, listable folder.

## Keeping it current: the change-journal observer
`crates/oneup-local/src/observe.rs`. After a finished scan, the Local tree is kept current from the volume's USN change journal instead of rescanning. It starts from the bookmark the scan took, and each update advances the bookmark (`local_root.start_usn`) in the same transaction as the changes, so a crash replays at most one batch.

For each batch of records, the **last** record of each file ID decides:

| Last record | Parent (from the record) | Effect |
|---|---|---|
| `FILE_DELETE` | any | Row removed. If a Synced row links the ID, its deletion is recorded as **evidence** (`local_gone`), even if the row had already left the tree (see below) |
| anything else | a listed folder in the tree | The item is read **by file ID** (`Volume::stat_by_id`: one open with `FILE_READ_ATTRIBUTES`, no data read). Its name comes from there, because unprivileged records carry none. The row is stored or updated (create, edit, rename, move). A folder new to the tree is queued and listed with the scanner's code, so a folder moved in from elsewhere arrives whole |
| anything else | outside the tree, or an excluded folder | Rows removed **without** evidence: moved out isn't deleted. The reconciler sees Missing and holds; then, if the user's Recycle Bin index has the item's path or a folder above it, the agent records the evidence (below) |

Details that matter:
* **POSIX deletes.** Deleting a file that another program still has open (antivirus, an indexer) first moves it into `$Extend\$Deleted`, which looks like a move out. The delete record follows when the last handle closes. Synced rows outlive Local rows, so that later record still becomes evidence.
* A folder delete is evidence for that folder only. Its children have their own delete records.
* Deleting the scanned folder itself stops the batch, rolls it back and asks for a rescan; it is never evidence for anything inside.
* Renaming a folder to an excluded name (e.g. to `node_modules`) drops its contents without evidence.
* **A rescan is required** (and records no evidence) when the journal was reset (new ID) or wrapped past our bookmark, when the folder is gone or replaced, or when the scan is unfinished, older or used other rules. The agent does it by itself for journal loss on the same folder ([sync-engine.md § Journal loss](sync-engine.md#journal-loss)).
* **Cost** follows the journal, not the tree. Each record costs one indexed lookup, each item inside the tree one open-by-ID, and a folder is listed only when it enters the tree. The journal is volume-wide, so a busy disk costs lookups and nothing more.
* **An item back in the tree loses its deletion evidence.** Triggers on `local_item` drop the evidence of any item inserted or updated (it's there, so it wasn't deleted), so a file restored from the Recycle Bin before its deletion was carried out is never deleted in OneDrive.

Tested end to end on a temp folder (`tests/observe.rs`): creates, edits, renames, moves, a plain delete, an atomic save, a delete while another handle is open, a folder moved out, a folder with subfolders moved in, and an excluded folder created. The observed tree must equal a fresh scan, and exactly the deleted Synced files must get evidence.

The executor's own changes come back through the journal ("echo"); the reconciler recognises them because the Synced tree already records the result. The stored root path isn't updated if the root folder is renamed (it's tracked by ID, so observing still works). The journal is also read by the agent's watcher thread, which only wakes the agent and hands written files to the program tracker ([usage.md](usage.md)).

## The Recycle Bin is evidence
File Explorer deletes by moving into the Recycle Bin, which the journal records as a move out of the tree, so on its own such an item would be held forever and never deleted in OneDrive. An item found in the user's own Recycle Bin was deleted by the user (data-safety invariant 3). After each round, for the items the gate held as missing without evidence and no longer here, `Engine::note_recycled` reads the index files of `<drive>:\$Recycle.Bin\<user SID>` (`oneup_app::recyclebin`: the `$I` files' original paths, version 1 and 2 layouts; ordinary small files only, never the `$R` items themselves) and, for each item whose synced path or a folder above it is there, records the evidence (`note_local_deleted`) and marks its folders (`dirty_mark_synced`). The next round deletes it in OneDrive through the gate, so a large batch stops and asks. Done after the fact, it also settles items held before, and those a rescan found missing. The bin is read only when there are such holds.

## Performance
Measured on `C:\Windows` (~367k entries, 155k folders): **18 s warm, peak memory 42 MiB** (28 MiB with `--memory-profile minimal`). On a real OneDrive folder (299k files, 57k folders, `dev` exclusions): **17 s, 35 MiB**. The database is the first bottleneck: per-commit WAL work and checkpoints made 256-folder steps 6.5× slower than 4,096-folder steps, so steps are large.

## Limits
* A scan of a live folder is not an atomic snapshot. The journal bookmark makes that recoverable: what changed during and after the scan is replayed.
* Whether the *root* folder itself is unpopulated is judged from its handle's attributes, which may not carry the recall flags that listings do. In practice providers keep their root folders populated.
* Listing is single-threaded. Parallel listing could roughly halve warm scan time, at a CPU cost; not needed at the target scale.
* Only local NTFS/ReFS volumes (file IDs, open-by-ID) are supported.
