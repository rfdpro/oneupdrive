# FAQ

**Is this made by Microsoft?**
No. OneUpDrive is an independent open-source project. It stores your files in your normal OneDrive account using Microsoft's official API (Microsoft Graph).

**Can I use it now?**
Yes. Install it from the project's [releases page](https://github.com/rfdpro/oneupdrive/releases/latest) ([Getting started](getting-started.md)); it syncs your own OneDrive, all of it or the folders you choose. It's a first release, so expect rough edges and please report them.

**Will it work with my work or school account?**
Not at first. OneUpDrive is for personal OneDrive accounts.

**Does it replace onedrive.com or the phone app?**
No, those keep working as before. OneUpDrive only replaces the sync program on your Windows PC.

**Can I run it alongside the OneDrive app?**
Not on the same folder: two sync programs on one folder can damage your files, so OneUpDrive refuses to. On different folders, yes: you can leave some OneDrive folders to the OneDrive app (for Office's AutoSave there) and OneUpDrive the rest; see [Office](office.md#3-leave-some-folders-to-the-onedrive-app-works-today). [Moving over](migrating-from-onedrive.md) explains how to switch.

**Does Office's AutoSave work?**
Yes, with **Open with AutoSave** in the right-click menu (Office then edits the file in OneDrive itself), or for folders you leave to the OneDrive app. For double-click, make OneUpDrive the default program for Office files: files in its folders then open with AutoSave, and Office files anywhere else open as usual. All of these are on the [Office](office.md) page.

**Can I use more than one Microsoft account, or sync more than one folder?**
Both: see [More folders, backups, shared folders and accounts](folders.md).

**Why is it lighter than the OneDrive app?**
It follows Windows' own log of file changes instead of re-scanning, keeps its records on disk instead of in memory, and lets you leave out the millions of build and dependency files that rarely need syncing.

**What happens if my PC crashes mid-sync?**
OneUpDrive writes down each step before doing it, so after a restart it finishes or safely undoes whatever was in progress. Files are only ever replaced once the new copy is complete and checked.

**Can it lose my files?**
It's built so it can't without telling you first. It never treats a missing file as a deletion, keeps both copies when both sides changed, keeps a copy of anything it removes from your PC, and stops to ask before many deletions at once or when edited files suddenly look encrypted (as ransomware leaves them). Details: [Syncing in depth](syncing.md).

**Is there encryption?**
Yes: [encrypted vaults](vaults.md), compatible with the Cryptomator apps, so you can open them on your phone too.

**Does it send anything anywhere besides OneDrive?**
No. There's no telemetry. Updates are downloaded from the project's GitHub releases and installed only if they're signed by the same publisher. What the [Apps](settings-window.md#apps) page shows is counted on your PC and stays there.

**How do I uninstall it?**
Windows Settings → Apps → OneUpDrive → Uninstall. Syncing stops first; files that were only online leave your PC (they're still in OneDrive), every file that was on your PC stays, and so do your settings.
