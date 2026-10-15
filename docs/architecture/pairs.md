# More folder pairs, backups, shared folders and accounts

An account can sync several folder pairs, any of them as a one-way backup; Windows' own folders can be backed up where they are; folders others shared can be synced; and several accounts can be signed in at once. User guide: [Folders, backups and accounts](../user-guide/folders.md).

## Extra folder pairs (`oneup_app::pairs`)
A data folder syncs one pair. Extra pairs follow the vault pairs' shape ([vaults.md § Vault pairs](vaults.md#vault-pairs)): each is a data folder of its own, `<data folder>\pairs\<name>`, marked by `folder-pair.txt`, with its own store, `config.toml` (copied from the main one at creation), agent, log, lock and control pipe. It signs in as its main data folder does (`setup::auth` follows `vaultpair::main_dir`, which covers both kinds: `vaultpair::is_child_dir`).

**Its own listing, of its own folder.** Unlike a vault pair (whose Remote tree is a projection of the main listing), an extra pair mirrors only its folder: its agent's Graph client is `Graph::scoped(drive, root)`. Requests address `/drives/{drive}` (another person's drive, for a shared folder) or `/me/drive`; the scope's folder stands in for the drive root (Graph items with that ID get the `root` facet and lose their parent, `Graph::rooted`), `root()` and `latest_cursor()` address it, and path-addressed delta units are relative to it. OneDrive Personal allows folder-scoped delta on any folder, which the mirror already uses for its units. The mirror, planner, gate, executor and Files On-Demand need no change. Push stays on the account's own drive (a change anywhere wakes the agent, which then reads its folder's delta); a shared folder in someone else's drive has no push channel and uses the timer.

**Overlaps** (checked when adding): a pair's local folder may not overlap the main pair's, a vault's or another pair's (`check_local`); in the account's drive, its OneDrive folder may not hold the main pair's folder, nor overlap a vault's or another extra pair's (`check_remote`). Inside the main pair's OneDrive folder is allowed: the main pair then leaves it alone by the same last-layer rule vaults use (`rulefiles::add_separate`, `RuleSet::set_vaults`; `explain` says "a vault or folder pair synced on its own"), so no `!` rule can undo it and cloud cleanup never offers to remove it.

**Adding** (`pairs::add`): the OneDrive folder is found or made (`ensure_folder`, each level `conflictBehavior=fail`); the local folder may already hold files (Windows' own folders do), since the first round adopts equal files and keeps both copies of different ones, as any first sync does. The data folder is written in the order vault pairs use (store, scan, scoped mirror, `link_roots`, pairing keys, `LocalFs::open` writing the marker, the marker file last); a failure removes what this made, including `.oneup` in the local folder if this created it. `oneup start` / `stop`, the sign-in launcher (`launch::start_others`), the tray's start and update paths, the installer's data-folder list and `datafolders::all` include the pairs.

**Stopping a pair** (`pairs::remove`, and `pairs::unpair` for the main data folder): the folder is released (Windows removes only the online-only files and keeps the rest as ordinary files), then the pair's data folder (or, for the main one, its database) goes. The settings window can first turn Files On-Demand off, which downloads every online-only file, so the folder keeps a full copy.

## Backups: one-way flow (`oneup_sync::Flow::Backup`)
A pair's flow is in its store (`meta sync.flow`, `crate::flow`). `plan_flow(trees, folders, Flow::Backup)` changes the per-item rules in `merge`, then checks the result:

| Case | Both ways | Backup |
|---|---|---|
| Changed here | upload | upload |
| Changed in the cloud | download | upload this PC's version over it (the cloud's stays in version history) |
| Changed on both | keep both (conflict copy here) | upload this PC's version over it |
| Deleted here | delete in the cloud | nothing (the cloud keeps it) |
| Deleted in the cloud | recycle here, or re-upload if changed | upload again |
| Moved on either side | follow the move | the cloud follows this PC's layout |
| New in the cloud | download | nothing |
| New on both, different | rename here aside, both kept | upload over the cloud's (version history keeps it) |
| A cloud item in the way of a name this PC needs | rename *here* aside (`RenameLocal`) | rename the *cloud* item aside: `Op::RenameRemote`, a remote rename outside the Synced tree under `If-Match`, not counted by the breakers (like `RenameLocal`) |

Last, `plan_flow` turns anything that would still change this PC into a `HoldReason::OneWay` hold and drops cloud deletions, whatever the rules above produced. The simulation (`backups_send_this_pc_and_never_change_it`, 3,000 seeds, SQLite planning identical) checks that no backup plan changes this PC or deletes from the cloud, that settling leaves this PC as it was, and that every local file is then in the cloud at its path with its content (unless held).

**Switching back to both ways** (`flow::set`): the Synced rows of what this PC deleted while backing up (`Store::synced_gone_locally`, deepest first, a folder only once its rows below are gone) are removed first, so those cloud items read as new and come back here, instead of the old deletions being carried out in the cloud.

## PC folder backup
`oneup_sys::known::Known` reads where Windows keeps Desktop, Documents, Pictures, Music and Videos (`SHGetKnownFolderPath`, read only). `pairs::known_request` pairs one with OneDrive `/<Name>`, as a backup by default. Nothing is moved or redirected (the OneDrive app's backup moves the folders into its folder and points Windows there). A folder already inside another provider's sync root (the OneDrive app's backup) is refused by the coexistence check; the settings window shows it as "backed up by OneDrive – Personal", and one inside the main synced folder as already synced.

## Shared folders
`Graph::shared_folders` (`/me/drive/sharedWithMe`, folders only: `remoteItem` with its `driveId`) needs `Files.ReadWrite.All`, a broader permission asked for only on request: `oneup login --shared-folders` records it (`shared-folders.txt`) so refreshing keeps asking for that scope. A shared folder becomes an extra pair scoped to `(driveId, itemId)`. Folder-scoped delta on another person's drive is expected to work as on one's own; this path has had less real-world use than the others.

## Several accounts (`oneup_app::accounts`)
One data folder per account: `%LOCALAPPDATA%\OneUpDrive` and `%LOCALAPPDATA%\OneUpDrive-<name>` (with `account-name.txt`). Listing reads file names and markers only, never another account's database (opening one would upgrade it). Each account has its own settings window, tray icon (one instance per data folder) and start-at-sign-in value. Adding one opens its own settings window, whose setup page signs in.

**Who's signed in is recorded** (`accounts::SIGNED_IN_FILE`, from the drive's owner). Sign-in always shows Microsoft's account picker, since a browser otherwise reuses its last account without asking. When another account signs in to a data folder, its old listing and carry-over are dropped, and that is refused while the folder is paired; moving over from the OneDrive app refuses an account other than the app's.

## Limits
* The tray icon reflects the main pair only; extra pairs' state is on the settings window's Files page (one tab per pair) and in `oneup --data-dir <pair> status`.
* A backup with Files On-Demand is refused (a backup keeps every file here).
