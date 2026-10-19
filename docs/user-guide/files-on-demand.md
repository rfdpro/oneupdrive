# Files On-Demand and freeing space

With Files On-Demand, files in your synced folder can be **online-only**: you see them in File Explorer with their real names and sizes, but they take no space on your PC until you open one. Opening it downloads it, which you'll notice as a short wait.

Turn it on or off in the settings window, on the **Files** page in the folder's own settings (shown when nothing in the list is selected), or when setting up. From the command line, with the background agent stopped: `oneup on-demand on` / `off`; `oneup on-demand` shows which.

Turning it on makes every synced file already on your PC a Files On-Demand file that keeps its content. On a big folder that takes a while; if it's interrupted (say, by an update), the background agent finishes the rest by itself when it has nothing else to do, a little at a time, and notes it in **Recent activity**. Until then, those files show as **Sync pending** in File Explorer (see [Sync pending](excluding-files.md#sync-pending-in-explorer)).

## What turning it on does
* Nothing is removed from your PC. Files you already have stay downloaded.
* From then on, new files from OneDrive arrive online-only, and so does the new version of a file changed in OneDrive (the old version stays in OneDrive's version history).
* Opening an online-only file downloads it. This needs OneUpDrive running; without it, opening one fails with Windows' "the cloud file provider is not running" message, and nothing changes.
* The activity log shows each download, and which program opened the file (`opened by WINWORD.EXE`). The [Apps](settings-window.md#apps) page counts them.

## Turning it off
Downloads every online-only file first, so everything is an ordinary file on your PC again. If the disk doesn't have room, it says how much space is needed and changes nothing.

## Keeping files here, or freeing space
Right-click in File Explorer: **Always keep on this device**, or **Free up space**. The background agent acts on either within seconds.
* A file kept on this device is downloaded, and so is every new file that arrives in a folder you keep.
* Freeing space only ever removes content that is exactly what OneDrive has. A file you changed is sent first and freed afterwards, and nothing is freed while changes to this PC are paused.

From the command line: `oneup pin "Photos\2024"` (always keep) and `oneup free "Videos"` (free up space).

## Freeing space by itself
On the **Files** page, in the folder's settings (nothing selected), under **Free up space**:
* **Make files online-only when unused for** 7 days … 1 year (never, unless you choose).
* **When the disk is nearly full** (less than 10% free, or the share you choose), the files unused longest are made online-only until there's room again (5 points more free). Anything used in the last day is left alone.

"Used" means the latest of: when the file was last changed, when Windows last saw it read (where the disk keeps track of that), and when OneUpDrive last downloaded it for a program. Files set to **Always keep on this device** are never freed. The activity log notes each round ("12 files (3.1 GiB) not used for 30 days made online-only"). It checks for unused files at most once an hour, and for a full disk each time it has caught up.

From the command line: `oneup space --free-unused-days 30`, `oneup space --free-when-low on --low-disk-percent 10`, `oneup space` to show.

## Your largest files
On the **Files** page, select **Size** above the list to sort by it: a folder's size is everything inside it, so the biggest are at the top. **Exclude → Suggestions** lists the largest folders in your OneDrive. In a terminal, `oneup largest --count 20` lists the biggest files and whether each is on this PC. To make one online-only, right-click it and choose **Free up space**.

## Duplicates
On the **Files** page, **Duplicates** lists the files that have the same content, grouped, the groups that free the most space first: at the top, how much removing all but one copy of each would free ("4.2 GB in 1,830 files"). It reads OneUpDrive's list of your OneDrive, so it's quick and downloads nothing. It covers files of 1 MB or more. Excluded folders' contents aren't checked for duplicates.

Each copy says where it is: **here and in OneDrive**, **in OneDrive** (not on this PC), or **on this PC only**. Copies somewhere a removal deserves a second look are marked, and so is their group: **in vault …** or **in folder pair …** (a folder synced on its own), **outside the synced folder**, and **excluded** (a file an exclusion rule matches).

Files only on this PC (not synced yet, or held) aren't compared until you ask: **Hash** next to **Files only on this PC**. It reads those files of 1 MB or more, a few thousand at a time at low priority, and stops while sync is paused; select it again to go on.

**Removing copies.** Tick the copies to remove and select **Remove**. The preview lists every copy that goes and where it goes, and any copy that's left and why; select **Remove** to hand it to OneUpDrive. Nothing is removed yet: OneUpDrive checks each copy again and asks you to approve the removal, like a [safety stop](syncing.md#safety-stop), however few copies it is. **Approve…** in the Duplicates view (or **Review removing duplicates…** in the icon's menu) shows the list again, and **Approve** there (or `oneup approve CODE`) removes them; **Cancel** drops the removal.
* Copies in OneDrive go to OneDrive's recycle bin; copies on this PC go to the version store (`.oneup\versions`, **Kept on this PC**). Removing a synced copy removes it on both sides.
* The last copy of a file is never removed, even if you tick every copy. A copy in a vault or another folder pair doesn't count as the one kept: that folder is synced on its own.
* A copy that changed after the preview is left as it is, even one rewritten with the same size and date. If anything about the removal changed (a copy or a folder it's in moved, or a copy was edited), it isn't done: preview it again.
* While a chosen copy, or the one being kept, is being synced (renamed, edited, deleted), the removal waits until that's done.
* A backup pair never removes anything.

From the command line:
```powershell
oneup duplicates                                   # the groups, 20 at a time (--count, --after for more; --json)
oneup duplicates --hash-local                      # compare files only on this PC too
oneup duplicates remove "/Backup/a.jpg" "/Old/a copy.jpg"                       # the preview, with its code
oneup duplicates remove "/Backup/a.jpg" "/Old/a copy.jpg" --yes --expect CODE   # hand it to OneUpDrive
oneup approve CODE                                 # once `oneup status` shows it
oneup duplicates cancel                            # drop it
```
Name copies in OneDrive by their OneDrive path, and files only on this PC by their full path, as `oneup duplicates` lists them.

## Good to know
* A file is only ever filled with the exact version it stands for. If it changed in OneDrive and the change hasn't arrived yet, opening it fails once instead of showing the wrong content; OneUpDrive then catches up, usually within seconds.
* File Explorer's navigation-pane entry, status icons and right-click menu: see [File Explorer](explorer.md).
