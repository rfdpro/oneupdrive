# The first-run planner (observe mode)

`crates/oneup-plan`, CLI `oneup plan [--verify]`, also used by `oneup migrate check`. It reports only: nothing is written locally or in OneDrive. `--verify` reads local files, but **never a cloud placeholder** (below).

The planner compares the **Local tree** (a folder scan, [local-tree.md](local-tree.md)) with the **Remote tree** (the OneDrive mirror, [remote-mirror.md](remote-mirror.md)) and says what a first sync *would* do. Two-way sync adds the third tree, **Synced** (what both sides last agreed on), and the reconciler in [sync-engine.md](sync-engine.md) then produces real operations for the safety gate. This comparison is its "no Synced row yet" case.

## First-run (adoption) semantics
With no Synced tree there is no record of what used to be in sync, so the planner **can't tell "deleted here" from "new there"**. It therefore never proposes a deletion:
* an item on one side only is an **upload** or a **download**;
* a disagreement is a **conflict** in which both copies would be kept.

This is also the reason a first sync, including adopting an existing OneDrive folder, must never be "make the cloud look like this folder" or the reverse.

## Structure
| Part | Pure? | What |
|---|---|---|
| `plan_folder(path, inherit, local_children, remote_children)` | yes | Pairs one folder's children by name and categorises each pair; returns entries, same-size pairs that may be content-checked, and the subfolders to visit next (on both sides, or one) |
| `names` | yes | OneDrive's name, path-length (400 UTF-16 units) and size (250 GB) limits, from Microsoft's published restrictions |
| `observe(ops, …)` | reads the DB | Depth-first walk of both trees, one indexed child query per folder and side; feeds a `Report` |
| `ContentCheck` trait | — | Implemented by the CLI on top of `oneup-local::verify`; tests use fakes |

Memory is the children of one folder plus a stack of pending folders (depth × fan-out), not the tree. The report keeps totals and a capped number of example paths per category.

## Matching rules
* **By name, case-insensitively** (as OneDrive and Windows do). Unicode normalisation is *not* folded: NTFS treats precomposed and decomposed "é" as different names, and so does the planner.
* **One fold everywhere** (`oneup_sync::fold`): each character maps to its simple upper case when that is a single character, and stays as it is otherwise, one character to one, like NTFS's upcase table. So "ΟΔΟΣ", "οδοσ" and "οδος" are one name; the Kelvin sign and `k`, the Ohm sign and `ω`, `İ` and `i`, and `ß` and `ss` are different names. The planner, the reconciler, the rules, the cloud mirror's hints, the store's queries (the SQL function `oneup_fold`, registered on every connection) the Recycle Bin's deletion evidence and the path checks against sync roots all use it, so no two parts disagree about whether two names are the same. Where a table differs from it (characters such as `ı` or `ſ`, whose upper case is ASCII, are kept apart by some upcase tables), it folds more, not less: two names Windows keeps apart may read as a clash, never one name as two.
* Several local names differing only in case (case-sensitive folders, WSL) → `conflict: name clash`; nothing below them is compared.
* **Files:** different size → `conflict: size differs`. Same size → `same size, not compared` unless content was checked: equal QuickXorHash → `identical`, different → `conflict: content differs`. A local online-only placeholder with the cloud's size → `online-only` (nothing to transfer); with a newer version in OneDrive → `online-only here, a newer version in OneDrive`.
* **Folders** on both sides → compared recursively. A local **unpopulated** placeholder folder → `online-only`, and everything below it in the cloud is counted as online-only too (it is never listed locally; that would download its file list). An unreadable local folder → `parked`.
* **Kind mismatch** (file vs folder, link vs anything, OneNote notebook vs file) → `conflict: file vs folder`, not descended into.
* **Excluded** on either side → `excluded`, contents not counted. `observe` refuses to run unless the scan and the mirror used the same rules fingerprint, otherwise every rule difference would look like a transfer.
* **The OneDrive app's own items:** its hidden system file (`.` + GUID, any folder) and, in the top folder, the Personal Vault (a cloud-only folder paired with a local `<name>.lnk` shortcut; the name is localised, so it is recognised by that pairing) → `not synced`.
* **Local only**, first applicable reason wins: excluded → link → unreadable → online-only placeholder with no cloud counterpart (data not on this PC, can't be uploaded) → name not valid Unicode → name OneDrive refuses → never synced by OneDrive (`desktop.ini`, `~$` Office lock files, `.tmp`, the `.url` shortcut the OneDrive app makes for a OneNote notebook) → path over 400 → file over 250 GB → **upload**. A folder that can't be uploaded passes its reason down: its whole subtree is counted under that reason.
* **Remote only:** excluded → `excluded`; OneNote notebook → `package` (they sync through OneNote); otherwise **download**.

## Content checks (`--verify`) and placeholders
Only same-size pairs are hashed, only when asked, and only **ordinary** local files: no reparse point, no recall/offline attributes. This is checked from the scan *and again on the open handle* before reading, and size + last-write time must still be the scan's when the file is opened. It is read as a `win::WatchedFile` (under a Read-Handle oplock, [sync-engine.md § Executor](sync-engine.md#executor-cratesoneup-exec)), so a file written, truncated or mapped for writing while it is read, or that another program needed closed, is reported as changed, not hashed into a misleading answer, and that program is never locked out. Files are opened by path, in their folder under their recorded name, and found by file ID only if they moved since the scan (a handle opened by ID would make a save by rename fail meanwhile); read-only and fully shared.

Why so strict: reading a dehydrated placeholder downloads it even when opened with `FILE_FLAG_OPEN_NO_RECALL`, and a hydrated placeholder can be dehydrated by its provider at any moment. Windows also **disguises** placeholders from our process by default, even on open handles, so the verifier first switches the process to "expose placeholders". And as an independent guard, `--verify` **refuses outright** in a folder another sync app manages (recorded by the scan, or a registered sync root now).

Scans made before placeholders were exposed are marked stale (`LOCAL_SCAN_FORMAT` in `meta`): `plan` asks for a rescan, and `scan` starts over instead of resuming them.

## Limits
Two-way sync uses the three-tree reconciler ([sync-engine.md](sync-engine.md)), which has the Synced tree this one lacks.
* Without a Synced tree, a folder moved between the two runs reads as an upload plus a download.
* Timestamps never pick a winner: first-run conflicts keep both copies.
