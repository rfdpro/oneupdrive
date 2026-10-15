# More folders, backups, shared folders and accounts

Your main synced folder is just the start. OneUpDrive can keep **any number of folders** in sync, each with its own OneDrive folder, and each either **both ways** or as a **backup**. All of this is on the settings window's **Files** page: each folder has a tab, and **Add** adds one.

## Another folder
**Files → Add → Sync another folder**: a folder in OneDrive (made if it doesn't exist) and a folder on this PC. Both may already hold files: files that are the same are matched, not copied again, and if two files with the same name differ, both are kept (one gets "(conflict 1)" in its name).

```powershell
oneup pair add /Photos D:\Photos            # starts syncing straight away
oneup pair list
oneup pair remove Photos --yes              # stops syncing; both folders keep what they hold
oneup unpair --yes                          # the same for your main folder
```

* A folder can't be inside another synced folder, here or in OneDrive. One exception: a OneDrive folder **inside your main OneDrive folder** is fine. Your main folder then leaves it alone, and `oneup explain` says why.
* Each extra folder has its own background agent, safety stops, rules and activity log. `oneup start` and `oneup stop` start and stop them all together.
* **Files On-Demand** can be on for each folder separately (not for backups).

## Stopping a folder's sync
On the folder's tab, with nothing selected, **Stop syncing…** (also for your main folder and for vaults). It tells you what happens first:

* nothing is deleted from OneDrive;
* the folder on this PC keeps the files that are here, as ordinary files;
* files that were online-only leave the folder (their content is only in OneDrive, where they stay). Tick **Download them first** to keep a full copy here instead; it says how much space that needs and how much is free.

Your other folders keep syncing. Stopping your main folder keeps your sign-in and settings, and the window offers setting up again.

## Backups: one way, from this PC to OneDrive
Each folder syncs **Both ways** or as a **Backup**; pick it on the folder's tab on the **Files** page (with nothing selected, under **This folder**) (or `oneup flow backup` / `oneup flow both`). A backup:

* sends what you add or change on this PC to OneDrive (renames and moves too);
* **never deletes anything in OneDrive** because you deleted it here: OneDrive keeps it;
* **never changes this PC**: nothing made or changed in OneDrive comes down;
* puts this PC's version back if a backed-up file was changed in OneDrive (the changed one stays in OneDrive's version history), and sends a file again if it was deleted there;
* moves aside, in OneDrive, anything there that's in the way of a file from this PC (it gets "(conflict 1)" in its name).

Switching a backup back to **Both ways** brings home what the backup kept in OneDrive after you deleted it here. Delete those again to remove them from OneDrive too.

## Backing up your Desktop, Documents and Pictures
**Files → Add → Back up a PC folder** lists Desktop, Documents, Pictures, Music and Videos, with **Back up** beside each. Each becomes a backup to a OneDrive folder of the same name (`/Documents`, …). From the command line: `oneup pair backup-pc Documents Pictures` (add `--two-way` to sync both ways instead).

Unlike the OneDrive app, OneUpDrive **doesn't move these folders** or change where Windows keeps them: it backs them up where they are. If the OneDrive app already backs one up, or it's inside your main synced folder, the page says so and leaves it be.

## Folders others shared with you
**Files → Add → Sync a folder shared with you** lists folders other people shared with your account. **Sync…** beside one asks for a folder on this PC and syncs it there (both ways, online-only files by default). From the command line: `oneup pair shared`, then `oneup pair add-shared "Trip photos" D:\Trip`.

Reading what others share needs a broader permission than your own files. If the list says so, sign in again and allow it (**Sign in again and allow it…** there, or `oneup login --shared-folders`). What you can change in a shared folder depends on what its owner allowed.

## More than one account
**Accounts** lists the Microsoft accounts on this PC. **Add account…** asks for a name (Work, Family…) and opens a settings window for it, where you sign in and choose what to sync. Each account has its own folders, settings and icon in the taskbar corner, as in the OneDrive app. **Open settings** beside an account opens its window.

Each account keeps its own data in `%LOCALAPPDATA%\OneUpDrive-<name>` (your first one in `%LOCALAPPDATA%\OneUpDrive`). From the command line, `oneup accounts` lists them, `oneup accounts add Work` adds one, and `--data-dir` works with another account.
