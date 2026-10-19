# The settings window

Open it from the OneUpDrive icon's menu: **Settings**, or from OneUpDrive in the Start menu. Changes are saved as you make them, and the background agent picks them up straight away (the few that wait for a restart say so). The window follows Windows' light or dark mode and uses memory only while it's open.

## Set up
Shown only until a folder is set up: sign in, choose the folders, what to exclude and Files On-Demand, look at the preview, **Start syncing**. See [Getting started](getting-started.md#setting-up). On a PC with the OneDrive app, it first offers **Move over from the OneDrive app** (see the [glossary](glossary.md) for unfamiliar words): your OneDrive folder stays where it is, after a check and unlinking the app. See [Moving over from the OneDrive app](migrating-from-onedrive.md#in-the-settings-window).

At the top of every page, a light and a word say whether OneUpDrive is running and what it's doing. Select it for more detail and for **Sync now**, **Pause**/**Resume**, **Start** and **Stop**. While there's a problem, a red card above every page says what happened, what to do and when OneUpDrive tries again, with **Retry now**, **Open log** and **Copy details** (see [When something goes wrong](troubleshooting.md)). The pages: **Files**, **Search**, **Apps** and **Activity**, then the settings: **General**, **Accounts**, **Office**, **Safety**, **Network** and **About**. On the Files page, the magnifier at the right of **All files**, **Exclude** and **Duplicates** opens a search box when you point at it or Tab to it.

## Files
Everything about what syncs, in one place.

* **Along the top, one tab per synced folder**: your main folder, each extra folder or backup, and each encrypted vault (marked *stopped* or *locked* when it isn't running). **Add** syncs another folder, backs up a PC folder (Desktop, Documents, Pictures…), syncs a folder shared with you, or creates or adds an encrypted vault. See [More folders, backups, shared folders and accounts](folders.md) and [Encrypted vaults](vaults.md).
* **All files**: the folder's contents as a tree, on this PC and in OneDrive together, each with a box (ticked: it syncs; unticked: excluded; partly ticked: something inside is excluded; untick to exclude it, after a summary of what stops syncing, see [Choosing what syncs](excluding-files.md#ticking-and-unticking-in-the-file-tree)) and its status: *Synced*, *Online-only*, *Always on this PC*, *Uploading*, *Downloading*, *Excluded*, *Paused*, or *Synced separately* (a folder that another tab syncs). Select **Name**, **Status** or **Size** above the list to sort by it (again to reverse); a folder's size is everything inside it, so sorting by size shows what takes the most room. Use the arrow keys to move around the tree, the right and left arrows to open and close folders, and Space to tick or untick (see [Keyboard, screen readers and contrast themes](#keyboard-screen-readers-and-contrast-themes)).
* **Right-click** an item (or press the menu key) for quick actions: open it, show it in File Explorer, **Exclude…** or **Sync again**, **Pause syncing**, **Always keep on this PC** or **Make online-only**, **Sync this first** (while it's waiting to sync), get a link for viewers or editors, its version history, and why it syncs.
* **Search by name** finds files and folders anywhere in the folder; **Show** takes you to one in the tree.
* **Duplicates**: files with the same content, the groups that free the most space first, each copy marked where it is (and when it's in a vault, another folder pair, outside the synced folder or excluded); **Hash** compares files only on this PC too; tick copies and **Remove** to preview and hand the removal to OneUpDrive, then **Approve** it. See [Duplicates](files-on-demand.md#duplicates).
* **Exclude**: everything that keeps things from syncing, each with a way to undo it: the ready-made lists (developer folders, temporary files, system clutter), what you excluded yourself, exceptions, paused folders (and **Pause a folder…**) and `.oneupignore` files. **Suggestions** lists the largest and busiest folders, each with **Exclude**. When OneDrive still has excluded items, **Still in OneDrive** offers **Remove from OneDrive**. **Rules as text** has your rules as lines like a `.gitignore` file, the sync order as text, and what happens to OneDrive's copies of excluded items. See [Choosing what syncs](excluding-files.md).

On the right, the details of what you selected:

* **Share**, beside the name: point at it (or select it) for **Get link for viewers** or **Get link for editors** (the link is copied).
* **Sync**: **Sync this file/folder** on or off (off excludes it: it stays where it is, here and in OneDrive, and nothing here is uploaded any more; changes made in OneDrive still come here; nothing is deleted), **Pause this folder** (changes wait until you resume it), and **Sync order** (first, normal or last when a lot is waiting).
* **On this PC** (with Files On-Demand): **Always keep** or **Online-only**.
* **Versions**: earlier versions in OneDrive, to **Open** a copy or **Restore** one. See [Earlier versions and sharing](versions-and-sharing.md).
* **Recent activity**: what sync did with it (or, for a folder, with anything inside it), newest first.
* **Why**: which rule applies and what sync last did with it.

With nothing selected, it shows the folder itself: where it is on this PC and in OneDrive, **Both ways** or **Backup**, **Files On-Demand**, **Free up space** automatically (after a week, a month… unused, or when the disk is nearly full; see [Files On-Demand](files-on-demand.md#freeing-space-by-itself)), **Start** and **Pause** for extra folders, and **Stop syncing…** (see [Stopping a folder's sync](folders.md#stopping-a-folders-sync)); **Kept on this PC**, the copies sync kept when it removed or replaced files here, each with **Restore**; and the folder's **Recent activity**. For a vault: **Unlock…**, **Lock**, **Unlock with Windows Hello**, **Recovery key…**, **Change password…** and **Stop syncing…**.

## Search
Finds any file or folder in your OneDrive by name as you type, online-only files included, and says where each one is, with **Open**, **Show** and **Sync this first**. Below it: **Search inside files with OneDrive** (off; it sends the search text to Microsoft) and the **Hotkey** that opens this page from any program. See [Finding files](search.md).

## Apps
Which programs changed and opened files in your synced folder, over the last day, week, month or 90 days: a chart per day (or hour), and a table per program with files changed, files downloaded on open and their size. **Click a program** to see the files it used lately, newest first. The page updates by itself while it's open; a change shows within about 20 seconds.

* **Opened** is exact: Windows tells OneUpDrive which program asked for an online-only file.
* **Changed** is OneUpDrive's best guess: when a file is written, it asks Windows which programs have the file open, and which program closed it just after. That works even for a program that writes and closes a file in a blink, for files OneUpDrive already synced. A brand-new file that a program creates and closes in a blink may still show as **Not identified**. OneUpDrive's own work (downloading, keeping files on this device, freeing up space) isn't counted.
* Counts are kept for 90 days, and the latest 5,000 files used, all on this PC only. **Track which programs use my files** turns it off.

## Activity
At the top, while anything needs you: one line per item, with what to do (the same list as `oneup problems`, see [When something goes wrong](troubleshooting.md#everything-that-needs-you)).

Below, the lists the OneUpDrive icon's menu points to, one tab each. Type in the box to show only the items whose name or folder matches; **Refresh** reads the list again (it also follows syncing by itself). With more than one synced folder, choose which one at the top right.

* **Recent**: what OneUpDrive did lately, newest first, with whether it went through (*Done*, *Skipped*, *Failed*, *Waiting*). **Problems only** hides the rest; **Show older** goes further back.
* **Sync failures**: items that won't sync until you do something about them, grouped by why, with a button for each way to resolve one (see [Syncing in depth](syncing.md#resolving-what-cant-sync)): **Delete in OneDrive** or **Restore** for items gone from this PC, **Delete here** or **Upload** for items gone from OneDrive, **Rename** for a name OneDrive doesn't allow, **Exclude** for a file over 250 GB or a hard link, **Contents** for a folder that can't be deleted because it isn't empty. The heading of a group says why once and resolves all its items at once. Items inside a listed folder are shown under it: the folder's buttons cover them. You're asked before anything is deleted. **Show** opens an item in File Explorer. A choice shows as *Queued* until OneUpDrive carries it out, then the item leaves the list. Excluded items aren't listed.
* **Excluded in OneDrive**: excluded items that OneDrive still has, their size and whether they're on this PC yet. Looking changes nothing; the OneUpDrive icon asks what to do with them.
* **Safety stops**: every change a waiting safety stop holds back. Decide from the OneUpDrive icon's menu.

The numbers on the tabs count sync failures and waiting safety stops.

## Safety
* **Stop and ask when**: how many deletions, replacements or moves at once make OneUpDrive stop and ask you first (default: more than 200, or more than 10% of everything synced; never for 20 items or fewer); and the **ransomware guard**: stop when edited files suddenly no longer look like what they are (default: more than 20). See [Syncing in depth](syncing.md#safety-stop).
* **Kept copies** (the version store): what OneUpDrive keeps when it removes a file from this PC, how long, how much room it may take, and what happens when it's full. **Open folder** shows them.

## Office
**Open with AutoSave**, **Let OneUpDrive open Office files** (double-click with AutoSave once you pick OneUpDrive as the default program), folders left to the OneDrive app, and, only with experimental options turned on (`oneup settings --experimental on`, for developers), the experimental Office integration. See [Office](office.md).

## Network
Upload and download limits (and the hours they apply), and pausing by itself: on metered connections, with battery saver on, during games and presentations, and in quiet hours.

## General
Starting at sign-in, the taskbar icon, memory use and background priority, **Memory and hashing** (each number the memory profile sets, and how many files are hashed at once; empty is the default; see [Syncing in depth](syncing.md#how-it-uses-your-pc)) (these apply when OneUpDrive next starts), file changes (**Timeout highly active files**), **Hooks** (**Run hooks**, and a switch for each hook in `config.toml`, or why one doesn't run; see [Scripts and hooks](hooks.md)), **Update shortcuts to use OneUpDrive folder** (see [Shortcuts to your old OneDrive folder](getting-started.md#shortcuts-to-your-old-onedrive-folder)), and updates: install automatically, whether to get preview versions, and **Check now**.

## Accounts
The Microsoft accounts on this PC, each with its own settings window (**Open settings**), and **Add account…**. This window's account has **Sign out…** (syncing stops until you sign in again; your files stay) or **Sign in…**. See [More folders, backups, shared folders and accounts](folders.md#more-than-one-account).

## About
The version, and the data folder (database, settings, logs) with a shortcut to the log.

## Language and formats
OneUpDrive is in English for now. When a translation for your Windows display language is added, the window, the icon's menu and the command line use it, and English wherever a translation is missing. Numbers, sizes and dates follow your regional format (Settings > Time & language > Region), like 1.830 or 1 830 instead of 1,830.

## Keyboard, screen readers and contrast themes
Everything in the window works from the keyboard. Tab moves between controls; the focused one has an outline.

* **Tabs** (the folders along the top of Files, **All files** and **Exclude**): Tab reaches the chosen one; the left and right arrows (and Home, End) choose another.
* **The file tree**: the up and down arrows move, right opens a folder (again: its first item), left closes it (again: its parent), Home and End go to the first and last item, a letter goes to the next item starting with it, Enter opens a file or folder, Space ticks or unticks, and the menu key or Shift+F10 opens the quick actions. Esc clears the selection, showing the folder's own settings.
* **Menus** (the status at the top, **Add**, the quick actions): the up and down arrows move, Enter chooses, Esc closes.
* **Dialogs**: Esc closes them without changing anything; focus goes back where it was.
* **Share**, beside an item's name: Enter goes to its two links.

Screen readers (Narrator, NVDA) read each control's name, each tree item's status, size and whether it's ticked, and say when the status, a list or a search result changes. The window follows Windows' contrast themes (Settings > Accessibility > Contrast themes): the selection, the current page and switches that are on use the theme's highlight colour. With **Animation effects** off (Settings > Accessibility > Visual effects), nothing in the window moves. In a narrow window, or zoomed in (Ctrl and +), the details panel goes under the file tree.
