# Syncing in depth

Day to day you won't need any of the commands on this page: the [settings window](settings-window.md) and the [icon's menu](tray-icon.md) do the same. This page explains what happens underneath, with the commands for scripts and for testing.

## What it does
`oneup sync` keeps one folder on your PC and one folder in your OneDrive the same, in both directions:
* new files and folders are copied to the other side;
* edits are sent across (in OneDrive, as a new version of the same file, so its version history continues);
* renames and moves are repeated on the other side, without copying the file again. OneDrive won't move or rename a folder with more than 5,000 items inside it in one step, so OneUpDrive does it for you: it makes the folder at its new place, moves everything into it, then deletes the old, now empty folder (into OneDrive's recycle bin). A sharing link to that folder stops working (links to the files inside keep working). Changing only the capitals in such a folder's name ("bookmarks" to "Bookmarks") can't be done this way and is reported as failed: rename it to another name first, then to the one you want;
* deletions are repeated on the other side, but always recoverably (see below).

```powershell
oneup --data-dir "$env:LOCALAPPDATA\OneUpDrive-test" sync C:\Path\To\Folder --remote /My-Test-Folder
```

The first run looks through the folder once, and creates the OneDrive folder if it's missing. Later runs only look at what changed since, so they're quick however big the folder is. A data folder (`--data-dir`) syncs one pair of folders. Use another data folder for another pair.

Add `--dry-run` to see what would happen without changing anything (`--limit 200` lists at most 200 changes; the totals always come last).

**Setting up with the settings window.** When a data folder syncs nothing yet, the settings window opens on **Set up**: sign in, choose the two folders, what to leave out and whether to use Files On-Demand, look at the preview, then press **Start syncing**. It runs the same commands: `oneup sync … --dry-run` for the preview, then `oneup sync … --pair-only` (with `--on-demand` if chosen), which pairs the folders and leaves the syncing to the background agent, which it starts. With `--on-demand`, the folder becomes a Files On-Demand folder before anything is synced, so OneDrive's files arrive online-only instead of downloading in full first.

## Keeping it in sync in the background
`oneupd` is the background agent. Once `oneup sync` has paired the folders, it keeps them in sync for as long as it runs. Start it in the background, with no window:

```powershell
oneup --data-dir "$env:LOCALAPPDATA\OneUpDrive-test" start
oneup --data-dir "$env:LOCALAPPDATA\OneUpDrive-test" stop     # after its current step
```

`oneup start` returns as soon as the agent answers, and shows its status. The agent's messages go to `oneupd.log` in the data folder (the previous run's are kept in `oneupd.log.1`). If it can't start, for example because you aren't signed in, `oneup start` shows why. If it's already running, `oneup start` just shows its status.

To have it start whenever you sign in to Windows:

```powershell
oneup --data-dir … autostart on     # start at sign-in
oneup --data-dir … autostart off    # don't
oneup --data-dir … autostart        # show which
```

This is off until you turn it on. It's set for your Windows account only and needs no administrator rights: Windows runs `oneupd-start.exe`, a small launcher that shows no window, starts the agent and exits. You'll find it in Task Manager's **Startup apps** as "OneUpDrive agent (*data folder name*)", and you can turn it off there too. Each data folder has its own setting. If you move the OneUpDrive programs, run `autostart on` again.

You can also run the agent in a terminal window, where it prints what it does:

```powershell
oneupd --data-dir "$env:LOCALAPPDATA\OneUpDrive-test"
```

* Changes on your PC are sent within a few seconds.
* Changes in OneDrive usually arrive within about 40 seconds. OneDrive notifies `oneupd` that something changed (this takes OneDrive 15–30 seconds), and `oneupd` then fetches it. If notifications don't get through, it checks OneDrive every 60 seconds instead (`--poll-seconds` changes this, minimum 15). `--no-push` turns notifications off.
* It only does work when something in the synced folder changed. While nothing does, it uses almost no processor time.
* It prints what it does (to `oneupd.log` when started with `oneup start`), in the same form as `oneup sync`.
* `oneup stop`, or Ctrl+C in its window, stops it after the current step; pressing Ctrl+C again stops it at once.

While `oneupd` runs, `oneup sync`, `oneup versions` and `oneup restore` refuse to touch the same data folder ("another OneUpDrive process is using this data folder"). Stop `oneupd` first.

### Checking on it and pausing it
While `oneupd` runs, these commands talk to it directly. Use the same `--data-dir`. Nothing needs restarting.

```powershell
oneup --data-dir … status                  # what it's doing, and anything waiting for you
oneup --data-dir … pause                   # nothing syncs until you resume
oneup --data-dir … pause --for 2h          # resumes by itself after 2 hours (also 30m, 1d)
oneup --data-dir … pause --only local      # OneDrive's changes wait; yours are still sent
oneup --data-dir … pause --only remote     # your changes wait; OneDrive's still arrive
oneup --data-dir … resume                  # everything syncs again (--only works here too)
oneup --data-dir … sync-now                # look for changes now instead of waiting
oneup --data-dir … sync-now "docs\report.docx"   # …and send or fetch this first
oneup --data-dir … pause --folder Videos   # only this folder (and everything in it)
oneup --data-dir … resume --folder Videos
```

A pause is remembered: if `oneupd` restarts, it's still paused. While paused, changes are noted but not acted on. Nothing is missed, and they sync once you resume.

A pause takes effect at once, even in the middle of a long sync: nothing more starts on the paused side, and a download under way stops (it starts over when you resume). An upload that's already sending finishes. Opening an online-only file still downloads it: you asked for it.

A paused folder is saved in the data folder's `config.toml`, so it stays paused until you resume it, even if `oneupd` isn't running when you pause it. Both copies stay as they are meanwhile; nothing is deleted. `oneup explain` shows a file as paused.

### How it uses your PC
```powershell
oneup --data-dir … settings                       # show
oneup --data-dir … settings --upload-kib 500      # at most 500 KiB/s up (0 = no cap)
oneup --data-dir … settings --download-kib 2000   # and down
oneup --data-dir … settings --memory minimal      # minimal, balanced (the default) or performance
oneup --data-dir … settings --background off      # normal priority instead of background
oneup --data-dir … settings --quiet-hours 22:00-07:00     # no syncing at night (`off` to remove)
oneup --data-dir … settings --pause-on-metered off        # sync on metered connections too
oneup --data-dir … settings --pause-on-battery-saver off  # and with battery saver on
oneup --data-dir … settings --pause-on-full-screen off    # and during games and presentations
oneup --data-dir … settings --pause-on-full-screen-video off  # and while a video plays full screen
oneup --data-dir … settings --busy-quiet-s 30             # send a file that keeps changing after 30 s of quiet
oneup --data-dir … settings --hold-busy-files off         # send every change right away
```

* **Pausing by itself:** by default `oneupd` pauses while the internet connection is metered (a phone hotspot, a data plan Windows knows is limited), while battery saver is on, while a game, a presentation or another full-screen program has the screen, and while a video plays full screen (in a browser or a video player), and resumes when that ends. Games and videos are separate settings, so you can pause for one and not the other. Windows reports a full-screen video and a game that runs in a borderless window the same way, so OneUpDrive goes by the program: a browser or a video player is a video, anything else a game. Quiet hours (none by default) pause it every day between those times. `oneup status` shows when and why it paused by itself. It checks whenever it would wake up anyway, and every 30 seconds while it syncs, so it may take up to 10 minutes to notice, while idle, that the connection or battery saver changed. A pause it notices while syncing stops that sync as `oneup pause` does.

* **Bandwidth caps** apply to file contents, at once, even while `oneupd` runs. With `--cap-hours 09:00-18:00` they apply only in those hours (`--cap-hours always` to undo).
* **Background priority** is on by default: `oneupd` gives way to your programs for processor, disk and memory, and Windows may run it on power-efficient cores. Syncing a large batch can take a little longer.
* **Memory profile** sets how much memory the database may use for caching (4, 16 or 64 MiB, plus, with background priority off, up to four times that of the database file read straight from Windows' disk cache by `oneupd`), how large the pieces are that big files are uploaded in (up to 10, 40 or 60 MiB), and how many files are sent or received at once (4, 8 or 16, of which big uploads 1, 2 or 4) with as many requests to OneDrive under way as that needs. `performance` syncs many small files fastest. Larger pieces upload faster on a good connection: OneUpDrive measures the connection as it goes and picks the size, within the profile's limit.
* **Copies of files already in OneDrive aren't uploaded again.** If you copy a large file (1 MiB or more) that's already in your OneDrive, for example a video into a second folder, OneUpDrive checks that the contents are identical and asks OneDrive to make the copy itself. That takes a couple of seconds whatever the size. `oneup activity` shows it as copied in OneDrive.
* **An item that fails waits before it's tried again:** a minute, then twice as long after each failure in a row, up to an hour. Other files keep syncing, and a change to the item (here or in OneDrive) ends the wait. See [When something goes wrong](troubleshooting.md#a-file-that-fails-again-and-again).
* **While OneDrive is full, uploads and new folders wait.** Downloads go on, and what waited is sent once OneDrive has more room. The icon and `oneup status` say "OneDrive is full" meanwhile ([details](troubleshooting.md#cloud-full)).
* **Saving a file never fails because OneUpDrive is sending it.** If you save while it's being sent, the send stops and the saved version goes instead.
* **Files that keep changing wait until they're left alone.** A file saved twice within 5 seconds (a database, an app that saves its settings every few seconds) isn't sent after every save: OneUpDrive waits until it has gone 15 seconds without a change, then sends it once. A file that never stops changing is still sent at least every 10 minutes. `oneup activity` shows such a file as waiting. In the settings window: **General → File changes**; `--busy-changes`, `--busy-window-s` and `--busy-max-s` set how many changes in how many seconds make a file busy and the longest it waits. Downloads never wait.
* **The database is compacted** when `oneupd` starts, if deleted entries left most of it empty (it can otherwise stay at its largest size). It takes a few seconds and shows in `oneup activity`. `oneup settings --compact-database off` turns it off.
* Priority and memory profile apply the next time `oneupd` starts (`oneup stop`, then `oneup start`).

These settings are in the `[agent]` section of `config.toml` in the data folder, which you can also edit by hand.

### What goes first
When many files are waiting, you can say which to send or fetch first, and which last. The rules are written as in [Choosing what syncs](excluding-files.md):

```powershell
oneup --data-dir … priority --first "/Work/"     # the Work folder first
oneup --data-dir … priority --first "*.docx"     # documents first
oneup --data-dir … priority --last "*.iso"       # disk images last
oneup --data-dir … priority --remove "*.iso"
oneup --data-dir … priority                      # show
```

Only the order of transfers changes; what syncs stays the same.

Only you can talk to `oneupd` this way: other users of the PC, and other computers, can't.

### When it stops
`oneupd` stops by itself, and says why, when continuing could do harm:
* the synced folder was replaced or moved away.

Windows keeps a record of changes on each drive, but only so much of it. If the PC or `oneupd` was off for a while and the drive was busy meanwhile, the record may no longer reach back far enough. Then `oneupd` (or `oneup sync`) looks the whole synced folder through again by itself, and `oneup activity` shows a `rescan` entry. Nothing is deleted because of what this look-through doesn't find: a synced file that's gone from your PC without a record of its deletion stays in OneDrive and is reported as held. This can also happen while `oneupd` runs, if the drive is very busy (deleting a large build or dependency folder, for example). `oneupd` watches the record as it's written and remembers the synced files deleted meanwhile, so those deletions still reach OneDrive. The `rescan` entry gives their number as "deletions found in the journal".

If it can't reach OneDrive, it waits and tries again, longer each time.

A **safety stop** (see below) doesn't end `oneupd`: it changes nothing and waits for your answer. Neither do files deleted in OneDrive that don't fit in the version store: `oneupd` leaves them alone, keeps syncing everything else, and tells you once (see [When the version store is full](#when-the-version-store-is-full)). `oneup status` shows both, with their codes.

## Nothing is lost
* **Edited on both sides?** You get both. Your PC's copy is renamed to `name (conflict 1).ext` and uploaded, and the OneDrive version keeps the original name.
  * If the OneDrive edit lands in the last moment of a large upload from your PC, your PC's version keeps the original name instead and the OneDrive version arrives as `name (conflict 1).ext`. The activity log says so.
* **Deleted on your PC?** The OneDrive copy goes to OneDrive's recycle bin, where you can restore it from the website.
* **Deleted in OneDrive?** Your PC's copy is moved into the hidden `.oneup\versions` folder inside your synced folder, with a note of where it came from. (If that would make the folder too big, you're asked first: see [When the version store is full](#when-the-version-store-is-full).)
* **Replaced by a newer version from OneDrive?** Your PC gets the new version. The old one isn't kept on your PC, because OneDrive keeps it in the file's **version history** (on the website: right-click the file, *Version history*). This only happens when you hadn't changed the file on your PC; if you had, you get both copies (see above). Before the old copy is removed, OneUpDrive reads it to make sure it really is the version OneDrive has. If it isn't, it's kept in `.oneup\versions` instead. To keep old versions on your PC as well, run `oneup versions --set-keep-replaced on`.
* **A file that's open in another program** is left alone and tried again on the next run.
* **A file that disappeared without a trace** (for example, a disk that wasn't connected) is never taken as a deletion. It's left alone in both places.

## Safety stop
If a single run would delete, overwrite or move a lot of files at once (more than 200, or more than 20 that are also over 10% of everything synced), OneUpDrive changes **nothing** and stops:

```
Safety stop: this round would delete, overwrite or move 30 items in OneDrive (41 items are synced).
Nothing was changed. See the list with --dry-run. If it's what you meant, run again with --approve 895536c20c4bec17
```

Check the list with `--dry-run`. If it's what you meant, run again with `--approve` and the code. The code approves exactly that list: if anything else changed in the meantime, you'll get a new code.

`oneupd` also counts what it did in the **last 15 minutes**, so a mass deletion that happens slowly still triggers the stop. The message then says how many came before. `oneupd` keeps running but changes nothing until you answer. If the list is what you meant, run `oneup approve CODE` (`oneup status` shows the code again). To look first, open the list `oneupd` saves for each stop: `safety-stops\CODE.txt` in its data folder (`%LOCALAPPDATA%\OneUpDrive`), one line per item with what would happen to it. The icon in the notification area tells you too, and **Review…** in its menu answers. Once you approve, the stop is no longer shown, even while a large batch is still being carried out.

### The content guard (ransomware)
Ransomware encrypts your files where they are, keeping their names, so a sync app would dutifully send every scrambled file to OneDrive. Before sending an edited file, OneUpDrive looks at its first few kilobytes: a `.docx` must still look like a Word document, a `.pdf` like a PDF, a `.txt` like text, and so on for common types. If more than **20** edited files (within 15 minutes) suddenly don't, it stops and asks before sending any of them:

```
Safety stop: 25 files would replace their OneDrive copy with content that doesn't look like their type (as when files are encrypted by ransomware)
```

If you didn't expect this, don't approve. Disconnect the PC from the network and get help: OneDrive still has the good versions. File types it can't judge (anything unusual) are never flagged, so it doesn't get in the way of ordinary work.

### Changing the limits
```powershell
oneup safety                          # show them
oneup safety --max-changes 500        # stop above 500 deletions/replacements/moves on one side
oneup safety --max-percent 20         # ...or above 20% of everything synced
oneup safety --content-guard 50       # content guard: stop above 50 files; `off` turns it off
```
The settings window has the same choices, under **Safety**.

## What isn't synced
* Files the official OneDrive app doesn't sync either, because they only make sense on one PC: `desktop.ini`, Office's `~$…` lock files and `*.tmp` files.
* Names OneDrive doesn't accept (for example ones containing `: * ? " < > |`, or ending with a space or a dot), and files over 250 GB. The run lists these as "not synced" so you can rename or move them.
* Folders you chose to leave out (see [Choosing what syncs](excluding-files.md)).
* Hard links (one file that appears in two places, as conda and other package managers make them) and symbolic links. Changing the copy here would change the other one too, which may be outside your OneDrive folder. They're listed once as "refused", and OneUpDrive looks at them again only when they change. If OneDrive has a folder where you have a symbolic link, that folder isn't brought here ("the name is taken"). A Python or conda environment is better left out of syncing altogether: see [Choosing what syncs](excluding-files.md).

### Deleting
Delete a synced file or folder the usual way, in File Explorer: it goes to your Recycle Bin, and OneUpDrive deletes it in OneDrive too, into OneDrive's recycle bin. Both bins keep it for a while, so you can get it back either way: restore it from your Recycle Bin and it's uploaded again, or, if OneUpDrive hasn't deleted it in OneDrive yet (say, a safety stop is still waiting for you), it simply stays. Deleting a lot at once makes OneUpDrive stop and ask first (see [Safety stop](#safety-stop)). Something that disappears any other way, without a deletion Windows records (say, moved to another drive), isn't deleted in OneDrive on a guess: it's listed under what can't sync, below.

### What can't sync
Of these, the ones you'd have to change before they can sync (a name OneDrive refuses, a file over 250 GB, a hard link, a name taken by a link) are kept in a list until they're fixed, together with what OneUpDrive **holds** on purpose: an item gone from one side without a recorded deletion (it's never deleted on the other side on a guess: delete or restore it yourself), or two names in a folder that differ only in capitals. Trying again wouldn't help, so OneUpDrive counts as up to date without them, and says so: the [icon](tray-icon.md)'s tooltip shows "Up to date · 12 items failed", its menu has **Sync failure: 12 items**, and `oneup status` has a "can't sync" line. To see them, with the reason for each:

```powershell
oneup --data-dir … unsynced
```

An item leaves the list once it syncs, once it's gone, or once you exclude it. Excluded items are never on it: they're left out on purpose. How File Explorer shows them is explained in [Choosing what syncs](excluding-files.md#sync-pending-in-explorer).

## What happened to my file?
OneUpDrive keeps a log of what sync did: every upload, download, move, rename, deletion and conflict copy, anything it had to skip or couldn't do, and the times it waited for you (safety stops, a full version store).

```powershell
oneup --data-dir … activity                      # the latest 50 entries, newest last
oneup --data-dir … activity "docs\report.docx"   # one file
oneup --data-dir … activity docs --limit 200      # a folder and everything in it
```

Give the path inside the synced folder (as above), or the file's full path on your PC (`C:\…\docs\report.docx`).

```
2026-10-06 23:53:47  ok    upload      /activity-test.txt
2026-10-06 23:53:53  ok    recycle     /activity-test.txt  (to .oneup\versions)
```

It works whether or not `oneupd` is running. The log keeps the latest 50,000 entries and lives in your data folder, so nobody else sees it. It records names and folders, never what's inside your files. A file that's skipped again and again (say, it stays open in another program) is listed once, not every time it's retried.

## Getting a file back
Files that sync removed from your PC are kept in `.oneup\versions` inside your synced folder (and replaced files too, if you turned that on). In the settings window: the folder's **Kept on this PC**, on the Files page with nothing selected, with **Restore** beside each.

```powershell
oneup --data-dir … versions                       # what's kept, newest last, with IDs
oneup --data-dir … restore 1791337982375-3        # put it back where it was
oneup --data-dir … restore 1791337982375-3 --to "docs\old notes.txt"
```

Restoring never replaces anything. If the name is taken, you get `name (restored 1).ext` beside it. The next `oneup sync` sends the restored file to OneDrive too.

Files deleted from OneDrive are in OneDrive's own recycle bin (on the website). Older versions of a file are in its version history there.

## Making room
The version store is trimmed automatically at the end of each `oneup sync`, and by `oneupd` whenever it has caught up (not while changes to this PC are paused). It is careful:
* anything kept **less than 30 days ago is never removed**, however big the store gets;
* older items are removed, oldest first, **only while the store is bigger than 5 GiB or 2% of the disk**, whichever is smaller. While it's under that, old items stay.

`oneup versions` shows the current settings. To change them:

```powershell
oneup --data-dir … versions --set-min-days 60                  # protect items for 60 days
oneup --data-dir … versions --set-max-gib 20 --set-max-percent 0  # a fixed 20 GiB limit
oneup --data-dir … versions --set-max-gib 0 --set-max-percent 0   # never trim automatically
```

You can also remove old items yourself:

```powershell
oneup --data-dir … versions --older-than-days 30             # shows what would go
oneup --data-dir … versions --older-than-days 30 --yes       # removes them
oneup --data-dir … versions --max-gib 5 --yes                # oldest first, until under 5 GiB
```

**Rotation** (off unless you turn it on): when new items don't fit, the oldest kept items are removed to make room, even if they're less than 30 days old. It never removes anything to make room for a batch that wouldn't fit even in an empty store.

```powershell
oneup --data-dir … versions --set-rotate on
```

### When the version store is full
If the files that were deleted in OneDrive add up to more than the version store has room for, sync doesn't decide for you. By default it leaves them **untouched on your PC**, keeps syncing everything else, and asks you once for all of them:

```
Version store full: 2 files (12.4 GiB) deleted or replaced in OneDrive would go into the version store, which has room for 3.1 GiB.
They are still on this PC, untouched; everything else keeps syncing. Decide once for all of them, by running `oneup sync` (or restarting `oneupd`) with:
  --keep 308bb40746eae0b2     keep them anyway; the store stays over its limit until they are 30 days old
  --discard 308bb40746eae0b2  remove them from this PC without a local copy. ...
```

* `--keep CODE` moves them into the version store anyway. The store is then bigger than its limit until they're old enough to be trimmed. With `oneupd` running, use `oneup keep CODE`.
* `--discard CODE` removes them from your PC **without keeping a copy here** (`oneup discard CODE` with `oneupd` running). OneDrive's recycle bin usually still has them for up to 30 days, but it empties sooner when it's full, so don't count on it for long. A file that turns out to differ from the version OneDrive has is kept anyway.

The code covers exactly the files listed. If anything changes in the meantime, you get a new code. "Room" means the size limit minus what's in the store and can't be trimmed yet (kept less than 30 days ago).

To decide automatically from now on instead of being asked:

```powershell
oneup --data-dir … versions --set-overflow keep    # always keep them, and tell me
oneup --data-dir … versions --set-overflow hold    # ask me (the default)
```
