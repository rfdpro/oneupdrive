# When something goes wrong

OneUpDrive tells you about a problem in plain words, says whether it deals with it by itself, and what you can do. Your files are never changed by guesswork while a problem lasts.

## Where problems show
* **The icon** turns red. Its menu says "Problem: …" (or "Stopped: …" if the problem stopped OneUpDrive), with **Error details…**.
* **A notification**, for a problem that needs you (sign-in, a full disk…) straight away, and for one that usually clears by itself (no connection, OneDrive errors) once it has lasted about 15 minutes.
* **The settings window** shows a red card above every page: what happened, what to do, when OneUpDrive tries again, and:
  * **Retry now** (or **Start**, if it stopped) tries again straight away;
  * **Open log** opens the log file (`oneupd.log` in the data folder, `%LOCALAPPDATA%\OneUpDrive`);
  * **Copy details** copies what's needed for a bug report;
  * **Technical details** shows the error as the program saw it.
* **File Explorer**'s status flyout says "Problem".
* **The activity log** (settings window, **Activity**) notes when a problem began and when it cleared.
* `oneup status` prints the same, with the technical text last.

### Everything that needs you
(A **safety stop**, where OneUpDrive stops and asks before many deletions or changes, isn't a problem: answer it from the icon's **Review …**, see [When OneUpDrive waits for you](tray-icon.md#when-oneupdrive-waits-for-you) and [Safety stop](syncing.md#safety-stop).)

`oneup problems` lists, in one place, everything waiting for you, for the main folder and every other folder pair and vault, most pressing first:

* nothing set up yet, or not signed in;
* OneUpDrive not running, or stopped by a problem; another folder pair or a vault that should run with it but doesn't (not one you stopped, nor a locked vault);
* a problem below (its kind: sign-in needed, OneDrive full, disk full…);
* safety stops and a full version store, with the code to answer them;
* pauses, yours or by itself, and paused folders;
* items that can't sync, and items that failed and wait to be tried again;
* excluded items still in OneDrive;
* files changed on both sides in the last 7 days whose second copy is still there (OneDrive's version kept the name; compare the two and delete the one you don't need). The newest 100 are listed; a line says when there are more, or when older ones weren't checked.

Each line says what to do. `oneup problems --json` gives the same for scripts: each item's `kind`, `title`, `detail`, `action`, the `code` an answer takes, the `pair` (none for the main folder) and its `data_dir`, and `more_conflicts` when not all files changed on both sides are listed (`conflicts_not_checked` when older ones weren't looked at). It works whether or not OneUpDrive is running. The settings window shows the same list at the top of its **Activity** page.

OneUpDrive tries again by itself after a minute, then waits twice as long each time, up to 30 minutes. Once it has synced again, the next problem starts over at a minute. Fixing the cause and selecting **Retry now** is quicker.

## The problems

<a id="setup"></a>
### Nothing to sync yet
No folder is set up to sync with OneDrive for this account, so there's nothing for OneUpDrive to do.

**What to do:** open the settings window (OneUpDrive in the Start menu) and follow **Set up**. See [Getting started](getting-started.md).

<a id="offline"></a>
### Can't reach OneDrive
No connection to OneDrive. Changes wait on this PC and in OneDrive; nothing is lost.

**What to do:** check your internet connection (and a VPN or proxy, if you use one). Syncing resumes by itself once OneDrive is reachable.

<a id="signin"></a>
### Sign-in needed
OneDrive no longer accepts OneUpDrive's sign-in: it expired, you changed your password, or you removed OneUpDrive's access in your Microsoft account. Nothing syncs until you sign in again.

**What to do:** in the settings window, **Accounts**, select **Sign in**, and sign in with the same Microsoft account.

<a id="cloud-full"></a>
### OneDrive is full
OneDrive refused an upload because it has no room left. Uploads and new folders wait (they aren't counted as failed); downloads and everything else on this PC go on. OneUpDrive looks at how much room OneDrive has whenever it checks OneDrive (every minute or so, or every 10 minutes while notifications work) and sends what waited once there's more. If OneDrive doesn't say how much room it has (a folder someone shares with you), it tries one upload again every hour.

**What to do:** free up space in OneDrive (on the web: delete large files, then empty OneDrive's recycle bin, which counts towards your storage), or get more storage. The settings window's **Files** page sorts by **Size** to find what takes the most room.

<a id="disk-full"></a>
### This PC's disk is full
Downloads can't finish until there's room on the disk.

**What to do:** free up space on the disk. With **Files On-Demand** on (settings window, **Files**, the folder's settings), files take room only once you open them, and **Free up space** can make unused ones online-only by itself.

<a id="folder"></a>
### Synced folder unavailable
The synced folder was moved, renamed or deleted, its drive isn't connected, or another sync app (such as the OneDrive app) took it over. OneUpDrive **stops** rather than guess, so nothing in OneDrive is changed because of it.

**What to do:**
* Moved or renamed: put it back where it was (same name, same place), then **Start** from the icon's menu.
* On a drive that's disconnected: connect it, then **Start**.
* Another sync app took it over: stop that app syncing the folder (in the OneDrive app: **Settings → Account → Unlink this PC**), then **Start**.
* You want to sync a different folder: in the settings window, **Files**, select the folder's tab, **Stop syncing…**, then set up again.

<a id="records"></a>
### Sync records need repair
OneUpDrive keeps records of what's in sync, and checks them after every pass. This means the check found a mistake in them, a bug in OneUpDrive. It changes nothing until the records are right, so your files are safe, but nothing syncs meanwhile.

**What to do:**
1. Install the latest update (icon menu, **Install update …**, or settings window, **General**, **Check now**): repairs come with updates, and a new version repairs its records when it starts.
2. If that doesn't help, select **Copy details** and report it (the details hold no file contents).

<a id="onedrive"></a>
### OneDrive isn't responding
OneDrive answered with errors or asked OneUpDrive to slow down. This is almost always temporary.

**What to do:** nothing; OneUpDrive tries again by itself. If it lasts for hours, check OneDrive's service status on Microsoft's website.

<a id="other"></a>
### Sync problem
Something else went wrong. OneUpDrive tries again by itself.

**What to do:** if it keeps happening, select **Copy details** and report it, with what you were doing when it started.

## A file that fails again and again
When one file or folder can't be synced (OneDrive refuses it, or an error comes back every time), only that item waits: a minute after the first failure, then twice as long after each one in a row, up to an hour. Everything else keeps syncing. Changing the item, here or in OneDrive, ends the wait at once, and so does syncing it. The wait is kept when OneUpDrive restarts.

* `oneup status` has a "failed" line: how many items wait, and when the next is tried.
* `oneup explain PATH` says how often that item failed, why, and when it's tried again.
* `oneup activity PATH` lists each failure.

## OneUpDrive stopped
If an error stopped OneUpDrive, the icon's menu says "Stopped: …", the notification says why, and the settings window shows the problem with **Start**. Fix the cause (above), then **Start**. If OneUpDrive stopped without saying why, the end of `oneupd.log` usually does.
