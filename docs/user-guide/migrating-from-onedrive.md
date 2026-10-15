# Moving over from the OneDrive app

You can switch to OneUpDrive without re-downloading your files.

## Before you start
* **The OneDrive app and OneUpDrive must never sync the same folder at the same time.** OneUpDrive will refuse to start on a folder the OneDrive app still manages.
* Files that are "online-only" (cloud icon ☁️) in the OneDrive app disappear from your PC when the OneDrive app is unlinked: Windows removes them, because their contents were only ever in the cloud. (If the app gets stuck unlinking, they stay until it's finished: see [If unlinking gets stuck](#if-unlinking-gets-stuck).) They are **still safe in OneDrive**, and OneUpDrive brings them back as its own online-only files. Files that were on your PC stay, as ordinary files, including changes the OneDrive app hadn't uploaded yet. The check below makes sure beforehand that every online-only file really is in your OneDrive.
* If you want a full local copy of something first, right-click it in the OneDrive app and choose **Always keep on this device** before unlinking.
* **Your choices in the OneDrive app carry over.** The check (below) notes, while the app still manages its folder, what you set to **Always keep on this device**, the kinds of files the app is set not to upload (an organisation's policy), and the OneDrive folders that aren't on this PC (most likely unticked in the app's **Choose folders**). When that folder is then paired with all of your OneDrive, the same files stay on this PC, the same kinds of files aren't uploaded, and the folders that weren't on this PC are **paused**: they stay in OneDrive only, aren't shown here, and are never offered for removal. You can change any of it afterwards on the **Files** page (resume a paused folder to bring it here).

## In the settings window
When OneUpDrive is first set up on a PC with the OneDrive app, **Set up** offers **Move over from the OneDrive app** (or **Start in a new folder**, which leaves the app alone). Moving over keeps your OneDrive folder where it is and syncs it with all of your OneDrive:

1. **Sign in** with the Microsoft account the OneDrive app uses. If it's another one, the page says so, and the check and moving over refuse it (moving the folder over to another account's OneDrive would copy everything there).
2. **Choose what to exclude** and whether to use Files On-Demand (on is best: online-only files stay online-only).
3. **Check your OneDrive folder.** OneUpDrive lists the folder and your OneDrive by name (it opens, downloads and changes nothing), confirms that every online-only file is in OneDrive, and notes your choices in the app so they carry over. On a big OneDrive this takes a few minutes. If it finds online-only files that aren't in OneDrive, don't unlink: check again (OneDrive's list may just be older than a recent change), and look for them online or set them to **Always keep on this device** first.
4. **Unlink the OneDrive app**: its icon in the corner of the taskbar → **Settings → Account → Unlink this PC**, for your personal account. A work or school account stays with the app. The page notices by itself when it's done, and says if Windows moved your Desktop, Documents or Pictures out of the folder while unlinking. If the app gets stuck on **Signing out**, the page says so and offers **Finish unlinking** (see [If unlinking gets stuck](#if-unlinking-gets-stuck)).
5. **Preview**, then **Start syncing.**

What happens then: files already on your PC are matched with OneDrive by their contents (read once, never uploaded or downloaded again when they're the same). A file that differs from OneDrive's copy is kept as well, under a new name; nothing is overwritten. Online-only files come back as OneUpDrive's online-only files. The OneDrive app's leftovers (its hidden marker file, the `Personal Vault.lnk` and OneNote `.url` shortcuts) stay on this PC and aren't uploaded.

## From the command line
The same steps, for a data folder signed in with `oneup login`:

```powershell
oneup scan $env:OneDrive --exclude dev   # lists your OneDrive folder; never opens or downloads a file
oneup mirror                              # lists what's in your OneDrive online
oneup migrate check                       # checks, and notes your choices in the app
# unlink the OneDrive app, then:
oneup sync $env:OneDrive --remote / --exclude dev --pair-only --on-demand
oneup start
```

If the app gets stuck unlinking, `oneup migrate finish-unlink` gets it finished (see below).

`--exclude dev` leaves out programmers' build folders such as `node_modules`; use `--exclude none` to include everything. `oneup migrate check --json` gives the result as JSON.

To see in more detail what a switch would involve before deciding, `oneup plan` compares the scan with the listing and sorts every file into one of these groups (it changes nothing, and the OneDrive app can keep running):

| Group | Means |
|---|---|
| **Already in sync** | Same name and size on both sides (or identical contents, with `--verify`), or online-only here and present in the cloud. Nothing would be transferred |
| **Would be transferred** | Only in OneDrive (would download) or only on this PC (would upload) |
| **Needs your attention** | *Different on each side*: both copies would be kept, never overwritten. *Can't be uploaded*: a name OneDrive doesn't allow, a path longer than 400 characters, a file over 250 GB, a shortcut/link, or a folder Windows wouldn't let us read |
| **Left out** | Excluded by your choice, OneNote notebooks (they sync through OneNote), or files OneDrive never syncs anyway (`desktop.ini`, Office's `~$` lock files, `.tmp` files) |

`oneup plan --verify` also compares the *contents* of same-size files by reading them. It won't run on a folder the OneDrive app still manages: every file there is one of the app's cloud files, and reading one can make the app download it. `oneup observe` brings the scan up to date later from Windows' change journal, without a new scan.

## If unlinking gets stuck
Sometimes the OneDrive app says **Signing out** and never finishes, even after hours, with the app and its *OneDrive Sync Service* each busy on one processor core. It has removed your account but still holds the folder, so OneUpDrive can't take the folder over yet. Your files are all still there, and the choices OneUpDrive noted during the check are kept.

**Set up** says when this has happened (and since when), and `oneup migrate check` does too. **Finish unlinking** (or `oneup migrate finish-unlink`) then tries, in order, stopping at the first that works:

1. **Restarting the OneDrive app.** It may finish by itself. OneUpDrive waits up to 3 minutes.
2. **The app's own reset** (Microsoft's `OneDrive.exe /reset`), and waits again. This is skipped if a work or school account is also set up in the app, because the reset would unlink that too. If the app then opens asking you to sign in, close it.
3. **OneUpDrive finishes it itself.** It stops the app, looks through the folder again and brings its OneDrive listing up to date, and goes ahead only if every online-only file is in your OneDrive. Then Windows does what unlinking does: it removes the online-only files from this PC (they stay in OneDrive and come back as OneUpDrive's), and files on this PC stay as ordinary files. This is noted in **Recent activity**.

   This last part can take a long time on a big folder: about 17 minutes for 234,000 online-only files on one PC. Leave it running; the command line prints a line every minute. Online-only files inside folders you chose to leave out (such as `node_modules` with the `dev` choice) are removed from this PC too. Their content stays in OneDrive, but as you left those folders out, OneUpDrive doesn't bring them back.

To skip the first two, use **Finish it now** in Set up, or `oneup migrate finish-unlink --step release`. Windows skips any folder a program has open at that moment (often Documents) and leaves its files as they were. OneUpDrive looks again afterwards and finishes those itself, file by file, which programs that are open don't get in the way of: you don't need to close anything or restart. If an unlinking left files like that behind earlier, running `oneup migrate finish-unlink` again finishes them; until then, OneUpDrive won't start syncing the folder. Inside folders you chose to leave out (such as `node_modules`), OneUpDrive doesn't look, so files left behind there stay as they are: their content is in OneDrive, but they can't be opened on this PC.

It asks before starting; on the command line, step 3 asks you to type `yes` (or pass `--yes`). `--step restart` or `--step reset` runs only that step, and `--wait-minutes` changes how long it waits for the app.

If OneUpDrive later finds that its own Files On-Demand folder was released by another program (for example, the OneDrive app finishing an old sign-out), it stops syncing that folder instead of treating the files Windows removed as deleted. Stop syncing it and set it up again: nothing is deleted from OneDrive, and the online-only files come back.

## What about the junk already in my OneDrive?
Excluding a folder (for example `node_modules`) only stops OneUpDrive syncing it from now on. It never deletes anything from your cloud. If you want to clean up what was uploaded in the past, `oneup cloud prune` shows exactly what it would remove and asks first. Removed items go to OneDrive's recycle bin, so you can restore them.
