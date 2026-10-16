# The Remote tree (metadata mirror)

`crates/oneup-remote`, `crates/oneup-store`, `crates/oneup-graph`; CLI `oneup mirror`. The mirror writes nothing but OneUpDrive's own state database.

The Remote tree is OneUpDrive's local copy of *what is in OneDrive*: IDs, names, parents, sizes, hashes and timestamps, but no file contents. The reconciler compares it with the Local tree and the Synced tree to decide what to do ([sync-engine.md](sync-engine.md)).

## Getting a multi-million-item drive into the mirror quickly
One sequential `/delta` over a whole 4.7M-entry drive takes about **3.6 hours**. The mirror does better on every axis:

| Technique | Why it helps |
|---|---|
| **Cursor first** (`token=latest`) | The enumeration can take as long as it needs; every change made meanwhile is replayed afterwards from the cursor |
| **Split the drive** | Top levels are listed with `children` (to `--depth`, default 2). Each folder at that depth becomes an independent folder-scoped delta *unit* |
| **Parallel units** | Units run concurrently under one AIMD limiter (starts at 4, grows while OneDrive is happy, halves on 429/503, capped by `--max-concurrency`). A `Retry-After` pauses **all** requests, regrowth is capped below the last throttle point, and `RateLimit-*` early warnings slow us before throttling |
| **Hints, or a size-based split** | Below `--depth`, the way down to excluded folders is listed so that they're skipped rather than downloaded (below) |
| **`$top=1000`** | 5× fewer requests, 2.6× faster per unit |
| **Minimal `$select`** | ~22% lower page latency |
| **Pruning** | Excluded folders found while listing are **never requested at all**. Excluded items found inside a unit are dropped as they stream by and never stored |
| **Resumable** | Progress commits per page (unit `nextLink` + entries in one transaction); Ctrl-C or a crash loses at most one page |

## Data model (`oneup-store`)
| Table | Contents |
|---|---|
| `remote_item` | Every included item; excluded folders as one opaque row (`is_excluded = 1`) |
| `remote_pruned` | Tombstones `(id, parent)` for folders seen *inside* excluded subtrees, so later entries beneath them are recognised and dropped whatever order they arrive in |
| `remote_orphan` | Entries that arrived before their parent; placed later, fetching unknown parents if needed |
| `enum_unit` | Initial-enumeration work units and their resume points |
| `cursor` | The delta cursor; `Pending` until the first full catch-up, then `Current` |
| `meta` | Rules fingerprint and other settings |

**Invariants**, checked by `oneup verify` and in every test:
1. at most one root;
2. every stored non-root item's parent is stored and **not** excluded;
3. nothing is both stored and pruned;
4. only folders are excluded.

`Mirror::run` (`oneup mirror`, one-off commands) ends by checking them, together with the Local and Synced trees' own invariants. The check reads every row of all three trees: about 670 MB of page reads on a 340k-item drive, more than the 64 MB page cache holds. So the agent checks them in its first round only, and every later round uses `Mirror::update`, which brings the changes without the check. A listing from scratch (an expired delta cursor) is always checked.

## Ingestion rules (`oneup-remote/src/ingest.rs`)
The only code that decides how an entry changes the mirror. It is pure (store transaction in, no network), so it's tested exhaustively.

* Upserts are **idempotent**. Graph re-sends parent folders, so repeats are normal.
* Parent excluded or pruned → drop the entry (prune it if it was stored, e.g. a folder moved *into* `node_modules`); folders become tombstones.
* Parent unknown → orphan buffer.
* A folder matching an exclusion rule → stored as an opaque excluded row; anything already stored below it is pruned.
* During **catch-up**, a folder that appears with content but whose contents we don't have gets its own delta unit. This covers a folder moved *out of* an excluded area, which Graph reports without its children. The same happens when a rule change re-includes a folder.
* Deletions remove the whole stored subtree (Graph reports only the top item) and its tombstones, and record deletion evidence for every Synced item in it.
* Items that are neither files nor folders (the Personal Vault, OneNote notebooks, some shortcuts) are stored as **packages**, never synced: downloading one would make an empty file.

## Enumeration hints
The split only matters around **excluded subtrees**, whatever the user's rules are. Instead of thresholds tuned to one drive, the enumerator takes *hints* about where excluded folders are (`oneup-remote::SplitHint`, passed in `Config::hint`):
1. a finished, rule-pruned **local scan** made with the same exclusion rules: the common case when moving over from the OneDrive app, where the scanned folder is the existing OneDrive folder;
2. the excluded folders **learned** from the previous mirror: `mirror --rebuild` reads them (`remote_excluded_paths`) before clearing the Remote tree;
3. the **size heuristic** (`--split-size-mib`, `--max-depth`) as the fallback (fresh install, or `--no-hints`): below `--depth`, folders at least `--split-size-mib` keep being listed (to `--max-depth`).

A hint is the set of lower-cased **ancestor paths** of excluded folders. Below `--depth`, a discovered child folder becomes a List unit only if it is such an ancestor; every other folder becomes a Delta unit regardless of its size. So the enumerator lists exactly the way down to the excluded folders and fetches everything else in bulk. When a non-empty hint exists it replaces the size heuristic entirely. The set is capped at 250,000 paths (`MAX_HINT_PATHS`, a few tens of MiB at worst); beyond that the remaining excluded folders are handled the usual way.

Hints only change a unit's **kind** (List vs Delta), never *what* ends up in the mirror. A wrong or stale hint (folders renamed since the scan, local-only folders, garbage) costs speed, not correctness: excluded items below a Delta unit are still dropped after download. Thresholds alone don't generalise from one drive to another, which is why hints come first.

## Testing
`crates/oneup-remote/tests/mirror.rs` runs the real engine against an in-memory fake OneDrive. The fake reproduces Graph's measured behaviour: paging, re-emitted folders, current-state deltas, moved folders without contents, top-only deletes. It can also scramble delivery order and fail mid-run.

* Scenario tests: fresh mirror, scrambled + repeated delivery, no rules, changes during enumeration, moves across exclusion boundaries in both directions (including out of *never-listed* excluded folders), rule changes both ways, interrupted-and-resumed run.
* **Randomized simulation:** seeds of several rounds of random adds, deletes, moves and renames (often to excluded names), random page sizes, scrambled order, changes during enumeration, injected failures, varied split strategies (depths, size threshold, concurrency) and hints (none, correct-then-stale, garbage, mixed). After every round the mirror must equal the fake drive minus excluded subtrees. CI runs 60 seeds; `SIM_SEEDS=N` runs more. `hint_lists_only_the_way_to_junk` checks that a correct hint downloads no excluded items at all.
* Disabling the moved-in folder enumeration, the prune-on-move-into-excluded, or orphan placement each makes the suite fail.

## Limits
* Without hints, excluded items inside folders smaller than `--split-size-mib` (or deeper than `--max-depth`) are still downloaded (inside their unit) before being dropped. OneDrive appears to budget **items** (roughly 650k per 5 minutes, then a pause of several minutes), so avoiding those downloads is the main lever for first-sync time.
* OneDrive Personal sends no `RateLimit-*` headers, so early back-off relies on `Retry-After` alone.
* **An expired cursor** (HTTP 410) makes the agent list OneDrive again by itself; items missing afterwards are held, never deleted.
* Shared items added to "My files" aren't part of the drive's listing; folders others shared are synced as pairs of their own ([pairs.md § Shared folders](pairs.md#shared-folders)).
