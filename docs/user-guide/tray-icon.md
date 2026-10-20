# The OneUpDrive icon

While OneUpDrive runs, its icon (a blue cloud lifted one step above its base) sits in the corner of the taskbar. If you don't see it, click the **^** arrow: Windows puts new icons there at first. Drag it onto the taskbar to keep it in view.

## What the icon tells you
| Icon | Means |
|---|---|
| Blue icon | Up to date |
| Blue icon, dark blue badge with a ring | Syncing |
| Grey icon, pause badge | Paused, by you or by itself (on a metered connection, with battery saver on, during a game or presentation, in quiet hours) |
| Blue icon, yellow **!** | Needs your review: something waits for your decision |
| Blue icon, red **!** | A problem: OneUpDrive retries by itself, or stopped (see [When something goes wrong](troubleshooting.md)) |
| Faded grey icon | OneUpDrive isn't running |

Point at the icon to see the same in words, for example "OneUpDrive: Paused: battery saver is on".

"Up to date · 12 items failed" (in the tooltip; the menu shows **Sync failure: 12 items**) means everything else is synced, and 12 items need you to change something before they can be (see [What can't sync](syncing.md#what-cant-sync)). The icon stays blue: trying again wouldn't help, so OneUpDrive isn't stuck on them.

While OneUpDrive is sending or getting files, pointing at the icon also shows how far it is and about how long is left, for example:

> OneUpDrive: Syncing 40,000 of 59,218 items (67%)
> 1.0 GiB of 3.0 GiB · about 20 min left

It updates every second while you point at it. A big sync goes in several passes ("pass 2", "pass 3"): each pass counts only what it has to do, so the count starts again with each one. For the first few seconds of a pass it says "working out the time left". The time left is an estimate from how fast things have gone so far, so it can change. `oneup status` shows the same on its "progress" line.

## The menu
Click the icon for its menu. It's light or dark to match your taskbar (Windows 10 version 1903 or later).

At the top, a coloured dot and a few words say how things are: green when up to date, blue while syncing (with how far it is on a second line), yellow when it waits for you, red after a problem, grey when paused or stopped. Click that line to open the [settings window](settings-window.md).

Below it, grouped:
* **Open folder**: your synced folder in File Explorer.
* **Sync now**: check both sides for changes straight away.
* **Pause** for 30 minutes, 2 hours, 8 hours or until you resume, and **Resume**.
* **Recent activity**: what OneUpDrive did lately, on the settings window's [Activity page](settings-window.md#activity).
* **Error details…** appears with a problem ("Problem: …" or "Stopped: …" at the top): it opens the settings window's problem card, with what happened, what to do, **Retry now** and **Copy details**. See [When something goes wrong](troubleshooting.md).
* **Review …** and **Possibly encrypted: … files** appear when something needs your review (below).
* **Excluded, in OneDrive: …** appears when excluded items still have copies in OneDrive: **Show list**, or **Remove or keep…** (below).
* **Sync failure: … items** appears when some items can't sync until you change them: it opens the Activity page, which lists each one and why.
* **Install update …** when an update is ready.
* **Search**: find any file in your OneDrive by name, on the settings window's [Search page](search.md).
* **Settings**: the [settings window](settings-window.md).
* **Stop OneUpDrive**, or **Start OneUpDrive** when it isn't running.
* **Hide icon**: syncing carries on. Starting OneUpDrive from the Start menu brings it back.

## When OneUpDrive waits for you
It shows a notification once when it needs a decision. Click the notification, or the matching menu entry:
* **Files may be encrypted** (the ransomware guard): edited files look encrypted, the way ransomware leaves them. They were **not** uploaded. Open a few: if they don't open, choose **Not now**, disconnect the PC from the network and get help; OneDrive still has the good versions. If they open fine (you really did convert them), **Upload them**.
* **A safety stop:** many files would be deleted, overwritten or moved at once. Nothing changes until you answer. **Show the files** shows every item and what would happen to it on the Activity page (pressing Enter does this too), then asks again. **Go ahead** carries them out, and the icon stops saying it's waiting at once, even if a large batch takes a while. **Not now** leaves everything as it is, and OneUpDrive keeps waiting.
* **The version store is full:** files removed in OneDrive don't fit in the room kept for copies on this PC. **Yes** removes them here and keeps copies anyway, **No** removes them without a copy (you're asked once more), **Cancel** decides later. Until then they stay on your PC, untouched.
* **Left-out items still in OneDrive**:
  * **Delete from OneDrive** first downloads anything that isn't fully on your PC, then deletes them from OneDrive: they go to OneDrive's recycle bin (kept there for 30 days, less if it fills up) and are no longer on your other devices or on the web. The copies on your PC stay. You're then asked whether to do this automatically from now on.
  * **Keep in OneDrive** (what Enter chooses) leaves them there for good: a change made in OneDrive still comes to this PC, but nothing you change here is sent. You aren't asked about these items again, also after a restart. Items you exclude later are asked about anew.
  * **Show list** lists them on the Activity page, then asks again. **Decide later** changes nothing.

It also tells you if OneUpDrive stopped when nobody asked it to. Pausing by itself (a game, battery saver) only changes the icon, since that comes and goes often.

## Starting and hiding it
The icon starts with OneUpDrive, from the Start menu or at sign-in (**General → Start OneUpDrive when I sign in**). One icon shows per synced folder pair. **General → Show the OneUpDrive icon** turns it off for good.

The icon is only a window onto the background agent: closing it, or the icon crashing, never stops or changes syncing.
