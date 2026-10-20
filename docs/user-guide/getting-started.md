# Getting started

## What is OneUpDrive?
OneUpDrive is a replacement for the OneDrive app on Windows. Your files stay in your normal OneDrive account (onedrive.com and the phone apps keep working), but the program on your PC that keeps everything in sync is lighter, more careful and more under your control.

## What you'll need
* Windows 10 (version 1809 or newer) or Windows 11, 64-bit
* A **personal** Microsoft account with OneDrive (work and school accounts aren't supported)
* A web browser, for signing in
* Your files on an NTFS drive (most PCs)

## Installing
Download `OneUpDrive.App-stable-Setup.exe` from the project's [releases page](https://github.com/rfdpro/oneupdrive/releases/latest) and run it. It installs for you alone, with no administrator rights, and appears in the Start menu as **OneUpDrive**. Windows SmartScreen may warn about the installer: select **More info**, then **Run anyway**.

## Setting up
When it's installed, OneUpDrive starts and its window opens on **Set up**:

1. **Sign in** with your Microsoft account. Your browser opens and asks which account to use; OneUpDrive never sees your password. The page then shows which account signed in; **Use another account** starts again.
2. **Choose folders**: one on this PC and one in OneDrive to keep the same as it. A new folder of its own on this PC is suggested (it's created for you). Use `/` for the OneDrive folder to sync all of your OneDrive. To switch from the OneDrive app, see [Moving over](migrating-from-onedrive.md): don't pick the OneDrive app's folder, OneUpDrive refuses a folder another program syncs.
3. **Exclude**: what to leave out of syncing (**Developer folders** skips programmers' build folders such as `node_modules`; **Nothing** skips none). You can change this later on the **Files** page.
4. **Files On-Demand**: when on, OneDrive files appear here but download only when you open them. Also **Start with Windows**, on unless you turn it off.
5. **Look at the preview.** Select **Preview**. It shows what the first sync would do, with totals such as "online-only 1180, upload 3". Nothing changes until you select **Start syncing**.

After that it runs quietly in the background, and the window shows your files on its **Files** page. The [OneUpDrive icon](tray-icon.md) in the corner of the taskbar shows what it's doing, and its menu has everything you need day to day. The [settings window](settings-window.md) has the rest.

Coming from the OneDrive app? Read [Moving over from the OneDrive app](migrating-from-onedrive.md) first. Unfamiliar words are in [Words used in this guide](glossary.md).

## Shortcuts to your old OneDrive folder
Shortcuts made while the OneDrive app ran break after you switch:
* **Started in a new folder:** they still point into the old `OneDrive` folder.
* **Moved over:** the path is right, but Windows can't open the shortcut any more. It was saved through the OneDrive app's entry in Explorer, which is removed when the app is unlinked.

To fix them, open **General → Update shortcuts to use OneUpDrive folder** and select **Search**. It checks the Desktop, Start menu and taskbar. **Update** fixes every shortcut listed; **Undo** restores them.

It only changes shortcuts whose file or folder is in your synced folder now. Shortcuts stored inside a synced folder are skipped, because changing them would change OneDrive too.

After setup, the window opens this section by itself if it finds any.

Command line: `oneup shortcuts` lists, `--fix` updates, `--undo` restores. `--from "D:\OneDrive"` names the old folder if it wasn't `%USERPROFILE%\OneDrive`.

## Updates
**Updates install themselves.** About once a day OneUpDrive looks for a new release, downloads it in the background and checks that it's signed by the same publisher as the copy you have; anything else is thrown away. It installs the update when nothing is syncing (pausing syncing around it) and tells you; **Install update…** in the icon's menu does it straight away. In the settings window, **General** has **Check now**, **Install updates automatically** (turn it off and an update found is installed only when you choose **Install update…**) and **Get previews**. From the command line: `oneup updates` shows the choice, `--auto on|off` and `--channel stable|preview` change it, and `--check` looks now.

## Uninstalling
Windows Settings → Apps → OneUpDrive → Uninstall. Syncing stops first, for every account. Files that were only online leave your PC (they're still in OneDrive); every file that was on your PC stays, and so do your settings, in case you install it again.

## Your data comes first
OneUpDrive never deletes anything without clear evidence that you meant to, checks every file it transfers, keeps both copies when there's a conflict, and makes a **safety stop** (it stops and asks) if it ever sees many deletions at once or files that suddenly look encrypted. [Read more](syncing.md#nothing-is-lost).
