# Getting things back: rewind, versions, deleted files, conflicts

The settings window's **Recovery** page gathers every way to get something back. Each has an `oneup` command too.

## Rewind a folder
Puts a folder back the way it was at a moment you choose: files deleted since come back, edited files get their content of then, moved and renamed files go back where they were, and files created since are removed. It's the way back from ransomware, a script gone wrong, or a sync that did something you didn't want.

1. Type the folder (`/Documents`, or `/` for everything) and pick the moment, then **Preview**. The list shows every change (a page at a time; **Rewind** appears once you've seen them all) the rewind would make, grouped (restore deleted, revert edited, move back, remove created since), with sizes and where each old version comes from: OneDrive's recycle bin or the file's version history in OneDrive.
2. **Rewind**. Nothing in the rewind happens yet: it waits for your approval as a [safety stop](syncing.md#safety-stop). Everything else keeps syncing meanwhile, the folder included.
3. **Approve**. If anything in the folder changed since you looked, the list changes with it: nothing is approved, and the new list is shown to look at again. Otherwise OneUpDrive carries it out in OneDrive, and your PC follows. A big folder takes a few rounds.

Nothing is destroyed. What the rewind replaces stays in OneDrive's version history; what it removes goes to OneDrive's recycle bin and to kept copies on your PC (`.oneup\versions`). So a rewind can be rewound: pick a moment just before it.

What it leaves alone:
* **Anything that changes after you approve.** A change before **Approve** gives a new list to approve (above). If you (or anyone) edit, move or delete a file after **Approve**, before the rewind gets to it, that file is left as it is; the rest goes ahead. The activity log says how many were left alone.
* **Changes not synced yet.** A file changed in OneDrive that your PC hasn't fetched yet is skipped, and listed as such. A file you changed on this PC since the last sync, without it reaching OneDrive (as when the [content guard](syncing.md#the-content-guard-ransomware) holds scrambled files back), is **set aside**: it goes to kept copies, and OneDrive's copy comes back.
* **What was there but only started syncing since** (a file a rule left out, say): a file last saved before the moment you picked isn't removed, and a folder holding such a file stays. They're listed as left alone.
* **What OneUpDrive never saw.** The rewind works from OneUpDrive's own history of what it synced. That history starts when a folder's first sync finishes and keeps 30 days (at most a million changes), so you can't rewind further back than that. Files that were never synced aren't touched.
* **What OneDrive no longer has.** OneDrive keeps deleted items in its recycle bin for a limited time (30 days for personal accounts, less if the bin is full) and a limited number of versions. What's gone from there can't come back; the rest of the rewind still runs, and the activity log says what couldn't be done.

If the list changed between the preview and **Rewind** (something synced meanwhile), nothing starts: the new list is shown to look at again.

A folder that's a [backup](folders.md) or an [encrypted vault](vaults.md) can't be rewound.

From a terminal:
```powershell
oneup rewind /Documents --to "2026-10-19 14:05"            # what would change; nothing changes
oneup rewind /Documents --to "2026-10-19 14:05" --start --expect 3f2a9c4d5e6f7081
                                                           # ask the agent to do the list with that code
oneup rewind --approve 3f2a9c4d5e6f7081                    # approve it, if the list is still that one
oneup rewind --cancel                                      # stop a rewind under way; what it did stays done
oneup rewind --set-history-days 60                         # keep more history (0 turns rewinding off)
```

### After a ransomware stop
When the content guard stops because edited files suddenly don't look like what they are, the stop's list (Activity page, **Safety stops**) has **Rewind**. It previews a rewind of the folder those files are in, to just before them: the scrambled files are set aside and OneDrive's good copies come back. From a terminal: `oneup rewind --before-stop CODE` (add `--start` to do it). Disconnect the PC from the network first if you think something on it is still scrambling files.

## Conflicts
When a file changed on both your PC and in OneDrive, you get both: the OneDrive version under the file's name and yours as `name (conflict 1).ext` beside it. The **Conflicts** list shows each pair side by side, with each copy's size, when it was saved and which is newer.

* **Keep original** keeps the file under its name. The conflict copy goes to kept copies on this PC and to OneDrive's recycle bin.
* **Keep copy** keeps the conflict copy, renamed to the file's name. The other goes the same way.
* **Keep both** changes nothing.

From a terminal: `oneup conflicts` lists them; `oneup conflicts /docs/report.docx --keep copy` keeps one. If either copy changes before OneUpDrive gets to it, nothing is done and the activity log says so.

## Kept on this PC
Files sync removed from or replaced on your PC, kept in `.oneup\versions`. **Restore** puts one back where it was (never over anything: a taken name gets ` (restored 1)`). The same as `oneup versions` and `oneup restore`; see [Getting a file back](syncing.md#getting-a-file-back).

## OneDrive versions
Type the full path of a file in your synced folder, then **Show**: the versions OneDrive keeps. **Open** saves a copy of one outside your synced folder; **Restore** makes it the current one, and the one it replaces stays in the history. The same as [Version history](versions-and-sharing.md#version-history) on the Files page.

## Deleted from OneDrive
Items deleted from OneDrive, newest first, with **Restore**: the next sync brings the item back from OneDrive's recycle bin to the folder it was in (under ` (restored 1)` if the name is taken now), and your PC gets it.

Only items OneUpDrive saw deleted are listed: OneDrive doesn't let apps read its recycle bin for personal accounts, only bring back an item it knows. Everything else is in the recycle bin on OneDrive's website. An item whose folder was deleted too can be restored once its folder is back.

From a terminal: `oneup recycle-bin` lists them, `oneup recycle-bin --restore 2` brings back the second.
