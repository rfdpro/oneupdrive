# Files On-Demand and freeing space

With Files On-Demand, files in your synced folder can be **online-only**: you see them in File Explorer with their real names and sizes, but they take no space on your PC until you open one. Opening it downloads it, which you'll notice as a short wait.

Turn it on or off in the settings window, on the **Files** page in the folder's own settings (shown when nothing in the list is selected), or when setting up. From the command line, with the background agent stopped: `oneup on-demand on` / `off`; `oneup on-demand` shows which.

Turning it on makes every synced file already on your PC a Files On-Demand file that keeps its content. On a big folder that takes a while; if it's interrupted (say, by an update), the background agent finishes the rest by itself when it has nothing else to do, a little at a time, and notes it in **Recent activity**. Until then, those files show as **Sync pending** in File Explorer.

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
Under **Storage → Free up space automatically**:
* **Make files online-only when unused for** 7 days … 1 year (never, unless you choose).
* **When the disk is nearly full** (less than 10% free, or the share you choose), the files unused longest are made online-only until there's room again (5 points more free). Anything used in the last day is left alone.

"Used" means the latest of: when the file was last changed, when Windows last saw it read (where the disk keeps track of that), and when OneUpDrive last downloaded it for a program. Files set to **Always keep on this device** are never freed. The activity log notes each round ("12 files (3.1 GiB) not used for 30 days made online-only"). It checks for unused files at most once an hour, and for a full disk each time it has caught up.

From the command line: `oneup space --free-unused-days 30`, `oneup space --free-when-low on --low-disk-percent 10`, `oneup space` to show.

## Your largest files
**Storage → Largest files in your OneDrive** lists the ten biggest files in your OneDrive, whether each is on this PC, online-only or outside your synced folder, with **Show**, **Free up space** and **Keep on this PC**. `oneup largest --count 20` does the same in a terminal.

## Good to know
* A file is only ever filled with the exact version it stands for. If it changed in OneDrive and the change hasn't arrived yet, opening it fails once instead of showing the wrong content; OneUpDrive then catches up, usually within seconds.
* File Explorer's navigation-pane entry, status icons and right-click menu: see [File Explorer](explorer.md).
