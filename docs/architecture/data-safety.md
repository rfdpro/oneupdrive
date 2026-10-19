# Data safety

Never losing user data comes before every other goal. The rules below are enforced in code and asserted in tests; they are requirements, not guidelines. Code comments refer to them as "invariant N".

## Invariants

1. **No destructive local action without a cloud-verified copy or a local backup.** Before deleting, overwriting or dehydrating local content, either (a) the cloud holds the same content (QuickXorHash match on the current version), or (b) the old content is moved to OneUpDrive's local version store.
   **Exception:** (c) the cloud holds the same content in its **version history or recycle bin**, proved by a QuickXorHash match, read under an exclusive handle, against the last synced version. This is allowed in two cases only. First, a file replaced by a newer cloud version without a conflict: the default, and `keep_replaced` turns it off. Second, a batch of removed files that doesn't fit in the version store, after the user chose "discard" for exactly that batch. By default such a batch is held untouched until the user decides; the user may instead choose to always keep (store over its limit) or to rotate the oldest kept items out. Anything not proved stays in the version store. Details: [sync-engine.md § Version-store overflow and replaced files](sync-engine.md#version-store-overflow-and-replaced-files).
2. **No remote delete or overwrite without a precondition.** Every mutation carries `If-Match: <eTag>`; a mismatch means re-plan, never force. Graph checks an upload session's `If-Match` only when the session starts, not when its last fragment commits, so a large replacement re-reads the eTag just before its last fragment (at most 320 KiB) and cancels the session if it moved on. An edit that lands between that check and the commit is found in the file's version history afterwards and kept as a new file under a conflict name (invariant 9).
3. **Deletions come only from explicit evidence.** A USN delete record, a delta `deleted` facet, the item's synced path (or a folder above it) recorded in this Windows user's own Recycle Bin index (`$I` files): File Explorer deletes by moving into the Recycle Bin, which the journal records only as a move out of the folder. Or the user's own answer for a held item, naming it (`oneup unsynced resolve`, the settings window's **Delete in OneDrive** and **Delete here**), noted only if the item is still gone from that side and still on the other when the agent carries it out. Absence from a scan is *suspicion*, not evidence: such items are held, and deletions pass the safety gate (with user confirmation above thresholds).
4. **Sync-root identity check.** Volume serial + root file ID + a `.oneup` marker must match before any operation runs. A missing, unmounted or replaced root ⇒ **halt**, never "everything was deleted".
5. **Compare-and-swap execution.** Immediately before acting, open the item by file ID and re-check it on that handle: a file's size and last-write time, or that a folder is still a folder. Any drift ⇒ abort the operation and re-plan.
6. **Verify every transfer.** Downloads are hashed before they're placed; uploads compare the server's QuickXorHash and confirm the file didn't change mid-upload. Reading a file never locks its program out: an upload watches for writes instead (a read oplock), and one written meanwhile stops before it commits.
7. **Crash consistency.** Write-ahead intent records (fsync'd) → do → mark done. Recovery replays or rolls back idempotently.
8. **Circuit breakers** in both directions. Mass delete/move/overwrite above thresholds (count and percentage) pauses that direction and asks the user, showing a preview. An approval covers exactly the items on that list (and the folders their deletions empty, each deleted with evidence); anything not on it needs a stop of its own. The background agent counts what it carried out in the last 15 minutes on top of each round, and keeps that count across restarts.
9. **Conflicts keep both copies.** No last-writer-wins on content, ever.
10. **Safe defaults.** The first sync shows a preview before anything changes; `--dry-run` everywhere; `oneup panic` stops all destructive operations at once.

### Files On-Demand

These come from experiments with the Windows Cloud Files API that reproduced real data loss in the raw API.

11. **Never call plain `CfDehydratePlaceholder`.** Dehydrate only by (a) opening the file with an **exclusive** handle (share-none, which blocks every other writer), (b) verifying content against the cloud hash and our database under that handle, then (c) calling `CfUpdatePlaceholder(DEHYDRATE | VERIFY_IN_SYNC)` **through that same handle**.
12. **The platform's in-sync bit is advisory, not truth.** Windows ignores writes made by any process running the provider's executable (so the bit stays "in sync" after such a write). The agent never writes user content in the sync root except through Cloud Files transfer operations. User-facing tools (`oneup.exe`, the tray, the settings window) are **separate executables** from the provider (`oneupd.exe`).
13. **Hydration validates versions.** Each placeholder's file identity encodes (item ID, eTag, size, QuickXorHash). `FETCH_DATA` fails the request if the cloud's current version or the placeholder's size differs, then re-plans. It never serves bytes from a different version into a placeholder (doing so silently produces zero-filled files).
14. `CfUpdatePlaceholder`'s `UpdateUsn` precondition does not behave as a usable compare-and-swap (the call itself changes the USN before comparing), so it is **not** used as a safety mechanism.
15. **A Files On-Demand folder that stops being ours halts.** If OneUpDrive's sync-root registration disappears without OneUpDrive removing it (another program unregistered the folder), Windows will have removed the online-only placeholders. The folder then fails its identity check, so those removals are never sent to OneDrive as deletions.
16. **Another program's sync root is released only through the gate.** Finishing a stalled unlinking of the OneDrive app (`oneup migrate finish-unlink`) unregisters the app's root, which removes its online-only placeholders. It runs only with the gate's `release` token: a fresh scan and listing show every online-only item in OneDrive, the app is stopped and has removed its account, its choices are recorded, and the person agreed. The effect re-checks the registration and the app's processes immediately before acting.

## Recovery

A bounded, configurable local version store (`oneup versions`, `oneup restore`), the activity log (`oneup activity`), plus OneDrive's own version history and recycle bin.

## Code structure

* Destructive filesystem and network calls live in **one** crate (`oneup-effects`), behind typed tokens that the safety gate (`oneup-gate`) issues. The planner can't call them, and nothing else can create the tokens.
* The planner and the safety gate are pure (no I/O), so they are deterministic and run under simulation ([sync-engine.md § Simulation](sync-engine.md#simulation-cratesoneup-synctestssimrs)).
* Changes to the safety gate or the effects layer need two reviews: the maintainer's and, while there is one maintainer, an independent model review recorded in the PR ([CONTRIBUTING.md](../../CONTRIBUTING.md#what-we-look-for-in-a-pr)). No release ships with an open data-safety issue.
