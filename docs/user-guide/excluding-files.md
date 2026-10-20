# Choosing what syncs

Some files don't belong in the cloud: downloaded dependencies, build output, temporary files. Syncing them wastes your computer's resources. On one developer's PC, **9 out of 10** files the OneDrive app was tracking were exactly this kind of junk.

In the settings window, all of this is on the **Files** page: untick an item's box in the tree (or right-click it and choose **Exclude…**), tick it to sync it again, or see everything excluded under **Exclude**. Excluding something **never deletes it**, from your PC or from OneDrive. It stops it being **uploaded**: what's already in OneDrive still shows up here (online-only, with Files On-Demand) and opens as usual (see [What happens to OneDrive's copies](#what-happens-to-onedrives-copies)). Rule changes take effect at once, with no rescan: a running background agent applies them as soon as `oneup rules` saves them, or as soon as you save a `.oneupignore` file.

## Ticking and unticking in the file tree
Each file and folder on the **Files** page has a box:

* **Ticked**: it syncs.
* **Unticked**: it's excluded. The status beside it says why: *Excluded by preset*, *Excluded by rule* (one of yours), *By .oneupignore*, or *In excluded folder*. Point at the box for the exact rule.
* **Partly ticked** (a dash): the folder syncs, but one of your rules excludes something inside it (*Some excluded*). Rules that apply everywhere, such as `*.iso` or a preset, don't mark every folder. A folder with a `.oneupignore` file somewhere inside says so when you point at its box ("Has a .oneupignore inside").
* **Greyed out**: the window can't change it here. A paused folder is resumed from its details; a folder inside an excluded one comes back by ticking that folder; a `.oneupignore` rule is changed in that file.

**Unticking** shows first what stops syncing: how many files and how much is inside it on this PC (and how many of those are online-only) and in OneDrive ("at least" when it holds folders OneUpDrive doesn't look inside), and what then happens to OneDrive's copies (the setting below, which you can change in the same dialog; it applies to every excluded item). **Exclude** writes a rule naming exactly that item, like `/Projects/old/`, into `config.toml`. Nothing is deleted from your PC.

**Ticking** an item a preset or one of your broader rules excludes adds an exception (`!/Projects/old/`); ticking one you unticked removes its rule.

From the command line:

```powershell
oneup --data-dir … rules --exclude-path "Projects\old"         # what would stop syncing (changes nothing)
oneup --data-dir … rules --exclude-path "Projects\old" --yes   # exclude it (refused if it moved or was replaced meanwhile)
oneup --data-dir … rules --include-path "Projects\old"         # sync it again
```

## Suggestions
**Exclude** lists a few folders worth a look: the **largest** in OneDrive, and the **busiest**, those whose files changed most often in the last 7 days. A folder that's nearly all one folder inside it suggests the one inside. They're only suggestions: nothing is excluded unless you choose **Exclude**, which shows the same summary as unticking. Folders already excluded aren't suggested.

```powershell
oneup --data-dir … rules --suggest
```

The largest come from the sizes OneDrive reports for its folders, so a folder only on this PC isn't among them until it's uploaded.

## Presets
Ready-made rule sets. None is on unless you turn it on, because what's junk to one person is work to another.

| Preset | Skips |
|---|---|
| `dev` | `node_modules`, Python virtual environments, `__pycache__`, `site-packages`, `.git`, `target`, `.next`, build caches |
| `temp` | editor swap and backup files: `*.swp`, `*.swo`, `*~`, `*.bak`, LibreOffice lock files |
| `os-junk` | `Thumbs.db`, `.DS_Store`, `._*`, `$RECYCLE.BIN`, `System Volume Information` |

```powershell
oneup --data-dir … rules                 # what's on, and your own rules
oneup --data-dir … rules --on dev        # turn a preset on
oneup --data-dir … rules --off dev       # and off
```

Some files are never synced, whatever the rules say, as with the OneDrive app: Office lock files (`~$…`), `*.tmp`, `desktop.ini`, names OneDrive doesn't accept, and files over 250 GB.

## Your own rules
Rules are written like a `.gitignore` file, one per line:

| Rule | Skips |
|---|---|
| `*.iso` | every `.iso` file, in any folder |
| `renders/` | every folder called `renders` (the `/` at the end means folders only) |
| `/Downloads/` | only the `Downloads` folder directly in the synced folder (a `/` at the start or in the middle ties a rule to its place) |
| `docs/drafts` | `drafts` inside `docs`, directly in the synced folder |
| `logs/**` | everything inside `logs` |
| `!final.mp4` | brings back something an earlier rule skipped |

Upper and lower case don't matter (in any alphabet, as in Windows), and `\` works like `/`. When several rules match, the **last** one wins.

You can keep your rules in two places:

* **For the whole synced folder**, in the data folder's `config.toml`:
  ```powershell
  oneup --data-dir … rules --add "*.iso"
  oneup --data-dir … rules --remove "*.iso"
  ```
  You can also edit `config.toml` with a text editor. `oneup rules` keeps your comments.
* **For one folder and everything in it**, in a file called `.oneupignore` in that folder:
  ```gitignore
  # skip video renders in this folder and below
  renders/
  *.mp4
  # …but keep the final cut
  !final-cut.mp4
  ```
  `.oneupignore` files sync like other files, so your rules go wherever the folder goes. Rules in a deeper folder's file come after those from above, so they win.

One thing to know: **nothing inside a skipped folder can be brought back**. After `renders/`, a rule `!renders/keep.mp4` has no effect; bring back the folder itself (`!renders/`) instead. That's how `.gitignore` works too, and it's what lets OneUpDrive skip such folders without even looking inside.

## Excluding something that's already synced
Both copies stay where they are: the one on your PC and the one in OneDrive. From then on, nothing you change here is sent: an edit stays on this PC, and deleting it here doesn't delete it in OneDrive. Changes made in OneDrive (on the web, or on another device) still come here, as before; if both copies changed, you get both versions. An online-only file stays online-only and still downloads when you open it.

Remove the rule and syncing picks up where it left off: an edit you made meanwhile is sent, and if you deleted the file meanwhile, the OneDrive copy is deleted too (into OneDrive's recycle bin, with the usual safety stop for many deletions at once).

If you move a synced file *into* an excluded folder, OneUpDrive doesn't treat that as a deletion: the OneDrive copy stays, and the file is listed as held.

## What happens to OneDrive's copies
Excluding always stops uploads. What comes the other way is your choice (**Files**, **Exclude**, **Advanced**, or `oneup rules --cloud`):

```powershell
oneup --data-dir … rules --cloud pull    # bring them and their changes here, never upload (the default)
oneup --data-dir … rules --cloud keep    # leave both copies alone; OneDrive's don't appear here
oneup --data-dir … rules --cloud prune   # bring them here, then remove them from OneDrive automatically
```

* **pull** (the default): OneDrive's copies of excluded items appear here like any other file (online-only, with Files On-Demand), and a change in OneDrive is brought here. A change you make here isn't sent, and if OneDrive's copy changes too, you get both versions. Files only on your PC stay only on your PC, and deleting one here doesn't delete it in OneDrive.
* **keep:** excluded items aren't synced either way. OneDrive's copies don't appear here, and the two copies may drift apart. Re-include an item, and syncing picks up where it left off.
* **prune:** as **pull**, and then, once an item is completely on this PC, its OneDrive copy goes to OneDrive's recycle bin (usually kept there for 30 days). If you re-include it later, your copy is uploaded again. Removing a lot at once triggers the usual safety stop.

**A OneDrive copy is only ever removed once it's on this PC.** An online-only file is downloaded first (it's set to **Always keep on this device**, then becomes an ordinary file once OneDrive's copy is gone); something not here yet is brought here first. With **keep**, nothing brings excluded items here, so only what's already here is offered.

### Removing them from OneDrive once, or keeping them there
On the **Files** page, under **Exclude**, **Still in OneDrive** shows how many excluded items OneDrive still has. **Remove from OneDrive** downloads whatever isn't fully here yet, then removes them from OneDrive. The copies here stay, and aren't uploaded again. **Keep in OneDrive** leaves them there: they're handled as with **pull** (changes in OneDrive come here, nothing is sent; the setting is switched to **pull** if it was **keep**), and you aren't asked about them again, also after a restart. The background agent looks for such items when it starts and whenever you change the rules; the [OneUpDrive icon](tray-icon.md) also shows a notification with the same choices, and after removing offers to switch to **prune** from then on.

From the command line:

```powershell
oneup --data-dir … cloud prune                          # list them (changes nothing)
oneup --data-dir … cloud prune --details node_modules   # what's inside one of them, 100 at a time
oneup --data-dir … cloud prune --details node_modules --page 2
oneup --data-dir … cloud prune --yes                    # bring them here if needed, then remove them
oneup --data-dir … cloud prune --keep                   # keep them in OneDrive; not offered again
```

With the agent running, `--yes` asks it to do exactly what it found. Without it, only what's completely here is removed (downloading needs the agent); the rest is listed as not on this PC yet.

Folders OneUpDrive never looks inside (like `node_modules`, below) are removed whole when this PC has a folder of the same name in the same place, and not offered otherwise. OneDrive only says how many items such a folder holds directly, not what's in its subfolders, so the list says "at least N items", and removing one always asks first (a safety stop), however small it looks.

## Folders that aren't even looked inside
Folders that a preset or one of your `config.toml` rules skips by name alone (such as `node_modules/`) aren't even looked inside, on your PC or in OneDrive. That's what makes them cheap: millions of files there cost nothing. It also means OneDrive's copies of them don't appear here, whatever the setting above. `oneup rules` lists these names. Turn a preset off, or remove such a rule, and OneUpDrive looks through just those folders again on its own, on both sides; nothing else is rescanned. A name that a `!` rule might bring back is always looked inside, so the rules are followed exactly.

## "Sync pending" in Explorer
With Files On-Demand, File Explorer shows a blue two-arrow **Sync pending** icon on anything in your OneDrive folder that isn't a OneDrive file, which includes everything you excluded (they stay on your PC on purpose), and on the folders above it. It can also show it on folders that hold nothing unsynced at all. This is Windows' own display, not something OneUpDrive is still working on: `oneup status` says whether OneUpDrive is up to date, and `oneup unsynced` lists anything that really can't sync. `oneup explain PATH` tells you which rule excludes an item.

Ways to make Explorer show these as fine were tried and set aside.

## "Why is this file (not) syncing?"
```
oneup --data-dir … explain "C:\Projects\app\node_modules\left-pad\index.js"
/app/node_modules/left-pad/index.js
  not synced: inside /app/node_modules, excluded by preset `dev` (line 1: node_modules/)

oneup --data-dir … explain "notes\todo.md"
/notes/todo.md
  rules: no rule matches
  on this PC and synced
  last activity: 2026-10-07 09:12:44  ok    upload      /notes/todo.md
```

Give the file's full path, or its path inside the synced folder.
