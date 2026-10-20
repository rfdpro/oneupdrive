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
| **Resumable** | Progress commits per page (unit `nextLink` + entries in one transaction); Ctrl-C or a crash loses at most one page. A saved link that has expired since (HTTP 410) starts its unit over; storing entries again changes nothing |

A unit is finished without its contents only when its folder is gone by its ID. Where OneDrive refuses an ID-addressed folder delta, the unit uses the folder's path from when it was queued; renamed or moved since, the folder is then looked up by ID and listed where it is now, because catch-up reports only the rename, not the unchanged contents. The scheduler reads the next units from an index (`state, depth, path`), so finished units cost nothing.

## Data model (`oneup-store`)
| Table | Contents |
|---|---|
| `remote_item` | Every included item; excluded folders as one opaque row (`is_excluded = 1`) |
| `remote_pruned` | Tombstones `(id, parent)` for folders seen *inside* excluded subtrees, so later entries beneath them are recognised and dropped whatever order they arrive in |
| `remote_orphan` | Entries that arrived before their parent; placed later, fetching unknown parents if needed |
| `enum_unit` | Initial-enumeration work units and their resume points |
| `cursor` | The delta cursor; `Pending` until the first full catch-up, then `Current` |
| `remote_shared` | `(id, at)` for items with Graph's `shared` facet only, and when they were shared ([§ Sharing](#sharing)) |
| `sharing_intent` | Sharing changes asked of OneDrive whose outcome isn't recorded yet: item, permission, the change, keyed by its activity entry ([§ Sharing](#sharing)) |
| `meta` | Rules fingerprint and other settings |
| `remote_name`, `search_queue` | The name index for search, and the changes waiting to reach it (`remote_item.sk` keys an item's entry; [search.md](search.md)) |

**Invariants**, checked by `oneup verify` and in every test:
1. at most one root;
2. every stored non-root item's parent is stored and **not** excluded;
3. nothing is both stored and pruned;
4. only folders are excluded.

`Mirror::run` (`oneup mirror`, one-off commands) ends by checking them, together with the Local and Synced trees' own invariants. The check reads every row of all three trees: about 670 MB of page reads on a 340k-item drive, more than the 64 MB page cache holds. So the agent checks them in its first round only, and every later round uses `Mirror::update`, which brings the changes without the check and reads no table that grows with the tree (`an_update_reads_no_whole_table` checks the query plan of every statement it runs). A listing from scratch (an expired delta cursor) is always checked.

## Ingestion rules (`oneup-remote/src/ingest.rs`)
The only code that decides how an entry changes the mirror. It is pure (store transaction in, no network), so it's tested exhaustively.

* Upserts are **idempotent**. Graph re-sends parent folders, so repeats are normal.
* Parent excluded or pruned → drop the entry (prune it if it was stored, e.g. a folder moved *into* `node_modules`); folders become tombstones.
* Parent unknown → orphan buffer.
* A folder matching an exclusion rule → stored as an opaque excluded row; anything already stored below it is pruned.
* During **catch-up**, a folder that appears with content but whose contents we don't have gets its own delta unit. This covers a folder moved *out of* an excluded area, which Graph reports without its children. The same happens when a rule change re-includes a folder.
* Deletions remove the whole stored subtree (Graph reports only the top item) and its tombstones, and record deletion evidence for every Synced item in it.
* Items that are neither files nor folders (the Personal Vault, OneNote notebooks, some shortcuts) are stored as **packages**, never synced: downloading one would make an empty file.
* Every entry sets or clears the item's shared mark from its `shared` facet ([§ Sharing](#sharing)); a deletion clears it with the subtree.

## Sharing
The sharing audit (`oneup shared`, the settings window's **Shared** page; `oneup_app::sharing`) takes what may be shared from the mirror, without a request, and reads who with on demand, a bounded page at a time.

* **What arrives.** `ITEM_SELECT` asks for the `shared` facet. Microsoft's delta documentation lists no property that OneDrive (consumer) leaves out of created or modified items ("Remarks" in `driveItem: delta`), and the cursor's own delta links carry no `$select`, so full items arrive there anyway. Only shared items carry the facet, so it adds bytes to their entries alone. Any facet makes the item a candidate, even an empty one or one naming only the owner: Graph says a non-null facet means shared, what it holds varies, and the permissions read drops the false ones (`Prefer: hierarchicalsharing`, which would send an empty facet for "unique permissions, not shared", isn't used).
* **What's stored.** A side table with a row per shared item (`id`, `sharedDateTime` as `at`, and a revision `rev`, random, new each time the row is written), not a column on `remote_item`: an unshared item costs nothing, where a column would cost each of 3M rows a byte or more. Each ingested entry is one primary-key write or delete in that small table, decided by where the entry ended up: kept for an item stored or waiting as an orphan, cleared for one discarded inside an excluded folder. Delete, prune, dropping orphans and `clear_remote` remove the marks of the items they remove.
* **Inherited sharing.** Graph puts the facet on what inherits a folder's sharing too, so a shared folder's whole contents are marked, and its `sharedDateTime` can't tell an item shared on its own from one inheriting (equal or missing times are common). Only an item's permissions can: one without `inheritedFrom` is its own. So every marked item is a candidate, read in pages along the marks' primary key (`shared_page`, keyset on the ID; no whole-table read), and an item with no permission of its own (only the owner's and inherited ones) loses its mark: only the mark as read (`unmark_shared_read`, by its revision), so one the mirror wrote meanwhile, the item shared again, stays. Each item is read once, and again only after it changes and is marked again; the first audit after a large shared folder is listed reads its contents once, page by page.
* **Who with**, on demand only: `Graph::sharing` reads each candidate's eTag and permissions, ten items per JSON batch round trip. A page (`list_page`) stops at exactly 50 items listed or 500 marks read (each read from the store is cut to what remains of both) and returns where the next starts; `oneup shared` prints one page and how to go on (`--after NEXT`), `--json` the same page with its `next`; a run's cost stays bounded even when a whole shared folder's contents are still to check, and the window loads more on request. Owner and inherited permissions aren't listed. Each listed permission gets a code over the item, its eTag and everything shown (`oneup_gate::sharing::Grant::code`).
* **Items shared before the facet was kept** show once they change. `oneup shared --scan` lists the whole drive once with `$select=id,shared,deleted`, a page in memory at a time, and only adds marks, for items the mirror holds (stored, or waiting for their folder): a mark for anything else would never be listed. A page of the audit drops any mark it meets for an item the mirror doesn't hold.
* **Shared with me** is `/me/drive/sharedWithMe`, which needs `Files.Read.All` and which Microsoft deprecated: degraded until November 2026, then empty. At most 2,000 are listed.

**Stop sharing** (`DELETE /items/{id}/permissions/{perm}`) and **Set expiry** (`PATCH` of the permission's `expirationDateTime`) change who can open an item, never its content, but they are remote mutations. `oneup shared stop|expire` (the window runs the same command) goes ahead only with `--expect CODE`, the code shown for that permission (the window passes its row's): what was shown, never whatever is there by then. It reads the item and permission again; the gate's `sharing::review_permission` approves only if the code still matches, the permission is the item's own (not inherited, not the owner's, not on the drive's root) and the user confirmed, and the effect uses its token up. `oneup_effects::sharing::apply` reads both a third time immediately before the request and sends it with `If-Match` on the item's eTag (documented for both requests: 412 if it moved; the client's requests, method, URL, header and body, are checked against a loopback server). IDs go into the URL as one path segment each (a `/` in a permission's ID is encoded). Then it reads the permission back and judges the outcome only from what OneDrive has (`judge`): a removal is made when the item is still there without the permission (the item gone says nothing: it can't be told whether the removal came first), an expiry when it equals the date asked for (within a second). That also settles a request whose success answer was lost and which `Graph::send` sent again: the second answer is 404 or 412, though the change took effect. A refusal other than 404/412 is final without reading back; a 404 or 412 with the permission still there, as it was, is that refusal (a 412: the item changed), never "gone". Graph documents only `roles` as changeable on a permission, so **Set expiry** relies on undocumented behaviour: it's offered only with experimental options on (`[dev] experimental`), and an expiry OneDrive ignores, or sets to another date, is reported as not made.

**Write-ahead, then settled** (`data-safety.md` invariant 7). Before the request, one transaction records the activity entry (*Waiting*, `activity_begin`) and an intent (`sharing_intent`: item, permission, the change); after it, one transaction settles the entry in place and drops the intent (`intent_settle`). If the process ends between the two, recording the outcome fails, or the permission can't be read back, the intent stays, and the next `oneup shared` command (each settles leftovers first, before its own work: `sharing::begin`) or the window's Shared page settles it with the same judgement from what OneDrive has then (`sharing::reconcile`): *Done* ("confirmed afterwards"), *Skipped* (not made, or the item or permission gone), or left for later if OneDrive can't be read. A change holds a lock (`sharing.lock` in the data folder, opened with no sharing; released by Windows when the process ends) from the intent to its settlement, and reconciling runs only with that lock, so it never judges a change still in flight. The activity log's trimming keeps an entry a pending intent will settle, and settling requires exactly that entry: if it's gone anyway, the intent stays and the command says so. The intent and its entry commit durably (`Store::write_durable`, and `intent_begin` refuses otherwise): on disk before the request, so a power cut right after it leaves the record ([sync-engine.md § Durability](sync-engine.md)). Entries hold the permission's kind and access, and for a failure a summary (`oneup_graph::Error::summary`: status and OneDrive's error code, or "network error"), never the link, the people or anything else from OneDrive's answer, which can hold names, addresses and sharing links. The same summaries are what the command and the window show. A refusal by the gate is logged as skipped.

## Enumeration hints
The split only matters around **excluded subtrees**, whatever the user's rules are. Instead of thresholds tuned to one drive, the enumerator takes *hints* about where excluded folders are (`oneup-remote::SplitHint`, passed in `Config::hint`):
1. a finished, rule-pruned **local scan** made with the same exclusion rules: the common case when moving over from the OneDrive app, where the scanned folder is the existing OneDrive folder;
2. the excluded folders **learned** from the previous mirror: `mirror --rebuild` reads them (`remote_excluded_paths`) before clearing the Remote tree;
3. the **size heuristic** (`--split-size-mib`, `--max-depth`) as the fallback (fresh install, or `--no-hints`): below `--depth`, folders at least `--split-size-mib` keep being listed (to `--max-depth`).

A hint is the set of case-folded (`oneup_sync::fold`) **ancestor paths** of excluded folders. Below `--depth`, a discovered child folder becomes a List unit only if it is such an ancestor; every other folder becomes a Delta unit regardless of its size. So the enumerator lists exactly the way down to the excluded folders and fetches everything else in bulk. When a non-empty hint exists it replaces the size heuristic entirely. The set is capped at 250,000 paths (`MAX_HINT_PATHS`, a few tens of MiB at worst); beyond that the remaining excluded folders are handled the usual way.

Hints only change a unit's **kind** (List vs Delta), never *what* ends up in the mirror. A wrong or stale hint (folders renamed since the scan, local-only folders, garbage) costs speed, not correctness: excluded items below a Delta unit are still dropped after download. Thresholds alone don't generalise from one drive to another, which is why hints come first.

## Testing
`crates/oneup-remote/tests/mirror.rs` runs the real engine against an in-memory fake OneDrive. The fake reproduces Graph's measured behaviour: paging, re-emitted folders, current-state deltas, moved folders without contents, top-only deletes. It can also scramble delivery order and fail mid-run.

* Scenario tests: fresh mirror, scrambled + repeated delivery, no rules, changes during enumeration, moves across exclusion boundaries in both directions (including out of *never-listed* excluded folders), rule changes both ways, interrupted-and-resumed run.
* **Randomized simulation:** seeds of several rounds of random adds, deletes, moves and renames (often to excluded names), random page sizes, scrambled order, changes during enumeration, injected failures, varied split strategies (depths, size threshold, concurrency) and hints (none, correct-then-stale, garbage, mixed). After every round the mirror must equal the fake drive minus excluded subtrees. CI runs 60 seeds; `SIM_SEEDS=N` runs more, from `SIM_SEED_FROM=k`, and `SIM_SEED=n` replays one. Each run reopens the seed's database, as the agent does after a restart. On disk (the default) a seed waits on database files and runs a few seeds a second per process; `SIM_DB=memory` keeps the database in a shared-cache in-memory SQLite database that outlives every reopen (`in_memory_sim_db_keeps_its_contents_across_reopens` checks that it does), about ten times faster. The nightly run (`.github/workflows/nightly.yml`) runs 2 million seeds in memory and 12,000 on disk, for what only a file shows, a new window of seeds each time. `hint_lists_only_the_way_to_junk` checks that a correct hint downloads no excluded items at all.
* Disabling the moved-in folder enumeration, the prune-on-move-into-excluded, or orphan placement each makes the suite fail.

## Limits
* Without hints, excluded items inside folders smaller than `--split-size-mib` (or deeper than `--max-depth`) are still downloaded (inside their unit) before being dropped. OneDrive appears to budget **items** (roughly 650k per 5 minutes, then a pause of several minutes), so avoiding those downloads is the main lever for first-sync time.
* OneDrive Personal sends no `RateLimit-*` headers, so early back-off relies on `Retry-After` alone.
* **An expired cursor** (HTTP 410) makes the agent list OneDrive again by itself; items missing afterwards are held, never deleted.
* Shared items added to "My files" aren't part of the drive's listing; folders others shared are synced as pairs of their own ([pairs.md § Shared folders](pairs.md#shared-folders)).
* Shared items inside excluded folders aren't in the mirror, so the sharing audit doesn't list them (an excluded folder that is itself shared is).
