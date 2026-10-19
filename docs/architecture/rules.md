# Rules engine and user controls

Users decide what syncs, with gitignore-style rules, ready-made presets they opt into, and `oneup explain` to see why. Changing a rule takes effect without a rescan. Excluding something never deletes it anywhere. `crates/oneup-rules` holds the language, matcher and presets; `oneup_app::config` and `oneup_app::rulefiles` load them. User guide: [Choosing what syncs](../user-guide/excluding-files.md).

## Rule language
One rule per line, **gitignore syntax**, so the many people who know `.gitignore` know this already:

| Line | Meaning |
|---|---|
| `# text`, blank | comment, ignored |
| `name` | any file or folder called `name`, at any depth |
| `name/` | folders only |
| `/name`, `a/b` | anchored: relative to the folder the rule belongs to (a `/` at the start or in the middle anchors, as in git) |
| `*`, `?`, `[a-z]` | within one name; `*` never crosses a `/` |
| `**` | any number of folders (`**/x`, `a/**`, `a/**/b`) |
| `!pattern` | re-include what an earlier rule excluded |

Differences from git, on purpose:
* **Case-insensitive**, like Windows and OneDrive names, through the engine's one fold (`oneup_sync::fold`, [planner.md § Matching rules](planner.md#matching-rules)). Planning and listing fold alike, so listing never skips a folder that planning includes. Ranges such as `[a-z]` apply to the folded characters.
* **`\` is a folder separator**, like `/`, because that's how Windows users write paths. (Git uses `\` to escape; a name can't contain `\` on Windows or in OneDrive, so nothing is lost. A name starting with `#` is written `[#]…`; one starting with `!` gets a `/` or `**/` in front, so the line doesn't start with `!`. Wildcard characters in a name are written `[*]`, `[?]`, `[[]`.)

Semantics are git's:
* **The last matching rule wins.** Rules from a deeper `.oneupignore` come after those from higher up, so they win.
* **Nothing inside an excluded folder can be re-included.** `node_modules/` followed by `!node_modules/keep.js` keeps nothing. This is what lets enumeration skip excluded folders without listing them (below), and it's git's documented behaviour.

**No rule can make matching slow.** `.oneupignore` files sync from OneDrive, so their text is untrusted: a rule with several `**` is matched in time proportional to its parts times the path's depth (`oneup-rules/src/pattern.rs`), not by trying every way to split the path, which grows exponentially with the number of `**`. Names match in linear time ([`NameGlob`](../../crates/oneup-rules/src/glob.rs)). The `rules` fuzz target checks this nightly.

### Where rules come from (lowest precedence first)
1. **Presets** the user turned on, in a fixed order. Offered, never imposed: nothing is on unless the user chose it.
   * `dev`: `node_modules/`, `.venv/`, `venv/`, `__pycache__/`, `site-packages/`, `.git/`, `target/`, `.next/`, `.cache/`, `.tox/`, `.mypy_cache/`, `.pytest_cache/`, `.gradle/`.
   * `temp`: `*.swp`, `*.swo`, `*~`, `.~lock.*#`, `*.bak` (Office lock files and `*.tmp` are never synced anyway: see "not synced by nature" below).
   * `os-junk`: `Thumbs.db`, `ehthumbs.db`, `.DS_Store`, `._*`, `$RECYCLE.BIN/`, `System Volume Information/`.
2. **The data folder's `config.toml`**, section `[rules]`: `presets = ["dev"]` and `exclude = ["*.iso", "/Downloads/"]`, anchored at the synced folder. Edited with `toml_edit`, so the user's comments survive.
3. **`.oneupignore` files** in the synced folder, each anchored at its own folder. They sync like any file, so the rules travel with the folder. Only the **local** copy is read: one source of truth, and a `.oneupignore` that exists only in the cloud takes effect once it has been downloaded. Read only if it's an ordinary file (never a placeholder), at most 64 KiB; a larger or unreadable one is reported and ignored.
4. **Last layers no `!` rule can undo**: paused folders (`[pause] folders`), folders synced on their own (vaults and extra pairs inside this pair's OneDrive folder, [pairs.md](pairs.md)), and folders left to the OneDrive app ([office.md](office.md#folders-left-to-the-onedrive-app)).

Separate from user rules, and not changeable by them: **not synced by nature** (Office lock files, `*.tmp`, `desktop.ini`, the OneDrive app's marker; `oneup_plan::names`), names OneDrive refuses, files over 250 GB, and the engine's own `.oneup` folder. `explain` reports these too.

## Where rules apply
**1. Planning (the authority).** The `Parked` view (`oneup-plan/src/park.rs`) hides items OneDrive would refuse, and the user's rules, evaluated on paths relative to the synced folder, in each tree with that tree's own path:
* An excluded **local or cloud item** is hidden from the reconciler, as parked items are.
* An excluded **Synced item** (a Synced-tree row whose path matches) is hidden too. So the reconciler sees neither the item nor its record: it plans **nothing** for it, and reports no hold.

What that means for the user:
* **Excluding something that's already synced** stops syncing it. Both copies stay as they are. Nothing is deleted, on either side.
* **Changes made while excluded** wait. When the rule is removed, the Synced record is visible again and sync resumes from it, as after a pause: an edit is sent, and a deletion recorded meanwhile goes through the safety gate like any other.
* **Moving a synced item into an excluded folder** hides it on that side only. The other side still has it, so it reads as gone without evidence and is **held**. Nothing is deleted.
* Planning stays pure: the rules and the `.oneupignore` contents are loaded before the round, and `Parked` does no I/O.

**2. Enumeration (an optimisation).** The scanner, the change-journal observer and the cloud mirror skip folders excluded by name, and store each as one row marked excluded, so millions of dependency files are never listed or stored. Only rules that can be decided from a folder's name alone prune: unanchored folder rules from presets and `config.toml`, such as `node_modules/`. The cloud mirror covers the whole drive, not just the synced folder, so anchored rules and `.oneupignore` files can't apply there. Rules that can't prune still take effect through planning, which is complete on its own. Because nothing below an excluded folder can be re-included, pruning never hides something the rules would sync.

## Changing rules without a rescan
The rule set has two fingerprints:
* **Pruning fingerprint:** the name-only folder rules (`RuleSet::pruning()`), folded, behind a version for how names fold, stored with the local scan and the mirror. A change in the fold changes the version, so stored listings are brought in line once, as for any rule change (below). An empty set keeps its fingerprint: it matches nothing under any fold.
* **Planning fingerprint:** everything (presets, `config.toml`, the `.oneupignore` contents; `RuleSet::fingerprint`), stored in `meta` `rules.fingerprint`. When it differs at the start of a round, every folder is planned: a full pass over the database, not over the disk. The new value is stored only when a round ends clean, so a round cut short plans everything again.

Every round, `oneup_app::sync::Engine` loads the rules (`rulefiles::load`: `config.toml`, then every `.oneupignore` the Local tree has, shallowest first, skipping one inside an excluded folder). Rule files are found through `local_rule_file` (kept by triggers on `local_item`), and an unchanged one (same size and last-write time) comes from a cache, not the disk. An ignored rule file (too large, not text, not an ordinary file) is reported and logged once. `oneup rules` nudges a running agent; a `.oneupignore` edit is a change in the synced folder and wakes it anyway.

When the pruning rules change, the listing follows, database only:
* **A folder newly excluded:** its stored subtree is dropped and its row marked excluded, on each side. The Synced rows stay (hidden by planning).
* **A folder newly included:** its row is unmarked and the folder is queued for listing: the local scan queue, and a folder-scoped cloud enumeration. Only that folder is read.

Locally, `oneup_local::observe::reapply_rules` (one pass over the stored folders, rare) does this before the observer opens, and logs a `rules` activity note; the cloud mirror's `ingest::reapply_rules` does the same in the cloud. Pruning leaves no deletion evidence. Rule-file changes reach listing one round later than planning, which is safe: pruning is a subset of what planning excludes, and a folder still pruned when the rules re-include it reads as missing (held), never deleted. Test: `changed_folder_rules_are_reapplied_without_a_rescan` (either direction equals a fresh scan).

## Explain
`oneup explain <path>` (a full path, or one relative to the synced folder; also Explorer's **Why does this sync?** and the settings window's *Why* section) answers from the database and the rules, without touching the file:

```
oneup explain "C:\Sync\app\node_modules\left-pad\index.js"
→ not synced: inside app\node_modules, excluded by preset `dev` (node_modules/)
oneup explain "C:\Sync\notes\draft.md"
→ synced (no rule matches); last activity: upload …
```

It names the deciding rule (source, line, pattern), including a `!` rule that re-included the item, or the built-in reason for items not synced by nature. When the agent isn't running, it catches up the journal first.

## Cloud cleanup
Excluding never deletes from OneDrive by itself. What happens to OneDrive's copies of excluded items is a setting, `[rules] cloud_copies` (`oneup rules --cloud keep|prune|pull`), and `oneup cloud prune` removes them on request.

| `cloud_copies` | OneDrive's copies of excluded items |
|---|---|
| `pull` (default) | Excluding means "don't upload": OneDrive's copies still appear here (online-only with Files On-Demand) and take OneDrive's changes; changes here aren't sent. A cloud change replaces the copy here as usual (the old one recoverable as for any replace); a change here first stays here, and becomes a keep-both conflict once the cloud copy changes; an item only here stays only here; a deletion here isn't sent. Folders skipped by name while listing aren't brought here |
| `keep` | Left alone, not brought here. The two copies may drift apart; re-including an item resumes sync from its Synced record (a deletion made meanwhile goes through the safety gate, recoverably) |
| `prune` | Brought here as with `pull`, then removed from OneDrive automatically (into its recycle bin) once here, with the usual safety stops; the copies here stay |

**How `pull` plans.** Listing still skips folders by name in every mode. Their contents are on neither side, so planning hides them too: `RuleSet::upload_only` = the paused folders plus the name-only folder rules, applied to folders only. Everything else excluded stays visible to planning, and each operation for which `Parked::sends_excluded` holds (uploads, cloud folders, cloud moves and deletions of excluded items or into excluded places) is dropped. Dropped operations aren't deferred: the dirty marks clear, so nothing is re-planned every round. `prune` plans the same way, so it brings items here before removing them.

**Candidates** (`oneup_app::prune::candidates`): cloud items in the synced folder that the rules exclude **now**, outermost only. Not a folder skipped while listing that the rules no longer exclude (it's about to be listed again), never the engine's `.oneup`, never paused folders (they're waiting, not junk), and never the OneDrive app's folders not on this PC (carried over as paused folders). Unlisted folders get their size and child count from one read-only Graph request each (`counted`; Graph gives direct children only, shown as "at least").

**A cloud copy is removed only once it's here** (`prune::Candidate::ready`): every item in it synced, unchanged in OneDrive since, and files with all their content here (not online-only). Online-only files are pinned (`Engine::ready_ops`), so the placement pass downloads them; items not here yet arrive by syncing. A folder skipped by name counts as here when this PC has a folder of that name in the same place. With `keep` (which brings nothing here), a candidate with items not here isn't offered.

**Removing** is `Op::PruneRemote { remote, forget, items, keep }` (`oneup-sync`), never planned by the reconciler. The gate counts it as `items` deletions (`oneup_gate::weight`), so a big folder trips the safety stop like many single deletions would. The executor re-checks the copy here first (`LocalFs::has_content`: same file ID and stamp, a placeholder fully on disk), drops the Synced rows in `forget` (deepest first) **before** deleting in OneDrive with `If-Match` (`Cloud::prune`, which unlike `delete` may take a non-empty folder), so after a crash in between nothing is left that could later read as a cloud deletion to bring here. Then the mirror forgets the subtree (no evidence is recorded: no Synced rows remain), and the placeholders here become ordinary files (`make_ordinary`, `CfRevertPlaceholder`), as nothing backs them in OneDrive any more. A later re-include uploads the copy here.

**Asking.** `oneup cloud prune` lists (changes nothing); `--details PATH [--page N]` walks one folder in OneDrive, 100 entries a page, read-only; `--yes` brings both trees up to date and runs exactly the listed items through the gate and the executor, a safety stop on that batch counting as approved by the `--yes`. With the agent running, `--yes` accepts the agent's offer instead:
* **The offer.** In `pull` and `keep` mode the agent works out the candidates when it starts and when the rules change (`Engine::offered`) and offers them: `Status::prune_offer` (entries, items, bytes, a code: FNV-1a over the candidates' IDs and eTags, `prune::fingerprint`; a folder's eTag changes with anything inside it). `oneup status`, the tray (once per code) and the settings window (**Files → Exclude → Still in OneDrive**) show it. Accepting is the control call `answer` with `prune` and the code. Once accepted, every round works the candidates out again and plans the ready ones (counted, through the gate) until none are left; an accepted offer stays accepted while its list only shrinks (`Offer::covered`), since items go over several rounds as they arrive. A changed list gets a new code. The tray's dialog then offers "always" (`oneup rules --cloud prune`).
* **Keeping them in OneDrive** is an answer too: `keep_in_cloud` with the code (the tray's **Keep them in OneDrive**, the default of its dialog; the settings window's **Keep in OneDrive**; `oneup cloud prune --keep`). The agent saves the offer's item IDs to `meta` key `prune.kept_in_cloud` (`prune::keep`), clears the offer, and logs a note. `prune::without_kept` drops those IDs from the candidates everywhere (the offer, the listing, `--yes`), so the offer isn't made again, whatever changes inside them; items excluded later are asked about anew. Keeping means pull-only: `--keep` and the settings window switch `cloud_copies = "keep"` to `pull` first (with `prune` it refuses).
* **`prune` mode** adds the ready candidates to each round's plan before the gate (all folders when the rules changed or on a full plan; otherwise only the children of the folders that changed, so cost follows changes).

Tests (`oneup-exec/tests/sync.rs` and unit tests): `pruning_removes_only_the_cloud_copy_and_re_including_uploads_it_again` (also with a crash at each of its crash points), `pull_mode_brings_changes_here_and_never_sends`, `upload_only_hides_the_folders_skipped_by_name_and_nothing_else`, `only_what_is_or_will_be_here_is_offered`, `on_demand_pruning_waits_for_the_content_and_leaves_an_ordinary_file`, `kept_items_are_not_offered_again`, `keeping_offered_items_in_onedrive_settles_the_offer_at_once`.

## User controls
Everything here is in `config.toml` (or the data folder's database, for the safety settings), the CLI and the settings window, and the agent reads the controls before **every** round, so a change counts from the next round.

| Control | How |
|---|---|
| Pause | Whole, per direction and timed: the control API ([control-api.md](control-api.md)). **Per folder:** `oneup pause/resume --folder`, `config.toml` `[pause] folders`: a last rule layer (`RuleSet::set_paused`, `Source::Pause`, names taken literally) that no `!` rule can override and that never prunes, so it behaves exactly like excluding a synced folder (changes wait, resume continues from the Synced record) while the folder stays listed. **Automatic** (`oneup_app::autopause`, pure; conditions from `oneup_sys::conditions`, checked at every wake-up the agent has anyway, so no new timer; shown in `Status::auto_pause`): `[agent] pause_on_metered` (WinRT `ConnectionCost`), `pause_on_battery_saver` (`GetSystemPowerStatus`), `quiet_hours`, `pause_on_full_screen` (games and presentations: `SHQueryUserNotificationState` busy, Direct3D full screen, presentation mode) and `pause_on_full_screen_video` (default: what `pause_on_full_screen` says). Windows' "busy" state is the same for a video and a borderless game, so `oneup_sys::conditions::classify` goes by the foreground program: a browser or a known video player is a video; Direct3D exclusive and presentation mode are always the other kind |
| Priority | `config.toml` `[priority] first/last` (`oneup priority`), rules as for exclusions. `oneup_app::priority` reorders a round's approved transfers among themselves only; every other operation keeps its slot, and a transfer that needed something earlier would fail its precondition and be retried. `oneup sync-now PATH` (control call `sync_first`) adds the path to the agent's in-memory list, which joins the `first` rules (`literal_path_rules`: the item and everything inside it) until a round settles |
| Bandwidth | `[agent] upload_kib_per_s` / `download_kib_per_s`, applied in the transfer path (`oneup_graph::throttle`: each transfer waits out its slot at the cap; async, no thread held), re-read every round; `[agent] cap_hours` applies the caps only in those hours |
| Memory profile | `[agent] memory_profile` = `minimal` / `balanced` / `performance` (`oneup settings --memory`), read by `oneupd` at start: database cache, queue depths, upload fragment sizes, operations run at once |
| Files that keep changing | `[agent] hold_busy_files` (default on), `busy_changes` (2), `busy_window_s` (5), `busy_quiet_s` (15), `busy_max_s` (600); `oneup settings --hold-busy-files/--busy-*`, window **General → File changes** (on/off and the timeout). Re-read at every wake-up ([sync-engine.md](sync-engine.md#the-background-agent-binsoneupsrcbinoneupdrs)) |
| CPU | `[agent] background` (default on): `oneup_sys::priority::set_background` (`PROCESS_MODE_BACKGROUND_BEGIN` + EcoQoS), at start |
| Placement | `oneup pin/free` ([files-on-demand.md](files-on-demand.md#pinning-and-freeing-space)); `[space]` policies ([usage.md](usage.md#space-policies-oneup_appspace)) |
| Transparency | `oneup status`, `oneup activity`, `oneup explain`, `oneup unsynced`, `oneup apps`, plan previews (`--dry-run`, the setup preview), notifications |
| Safety knobs | `oneup safety --max-changes/--max-percent/--content-guard` (store `meta safety.*`, window **Safety**); version store: `oneup versions --set-*` |

**Not built:** per-network bandwidth profiles (the metered-connection pause covers the common case), custom memory overrides, hashing concurrency, and a `pin` rule action (always keep matching files on this PC).
