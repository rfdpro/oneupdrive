# File Explorer

OneUpDrive fits into File Explorer the way the OneDrive app does.

* **The navigation pane** lists your synced folder as **OneUpDrive**, with its own icon (and each [vault](vaults.md) as "OneUpDrive vault …").
* **The Status column** shows each file's state: a cloud for online-only, a green tick for on this PC, a filled green circle for always kept on this device.
* **The status button** (Windows 11 23H2 and later): the cloud at the right of Explorer's address bar, inside a OneUpDrive folder, shows whether it's up to date, syncing, paused or waiting for you, and how much of your OneDrive storage is used. Its button opens the folder on onedrive.com; the status line opens the settings window.
* **Encrypted vaults:** files in a [vault](vaults.md)'s folder show a lock in the Status column.
* **Right-click a file or folder** for Windows' own **Always keep on this device** and **Free up space**, and OneUpDrive's menu:
  * **Open with AutoSave**: open a Word, Excel or PowerPoint file from OneDrive, with AutoSave ([Office](office.md));
  * **Copy share link…** and **Version history**: [earlier versions and sharing links](versions-and-sharing.md);
  * **Why does this sync?**: which rule applies, and what sync last did with it, in the [settings window](settings-window.md#files);
  * **Sync activity**: what happened to it lately, also in the settings window;
  * **Sync this first**: send or download it before anything else that's waiting;
  * **View online**: open it on onedrive.com (for a vault, a note instead: the website only shows encrypted files).

On Windows 11, if you don't see it in the first right-click menu, it's under **Show more options**.

The menu and the status show in folders OneUpDrive registers with Windows: those using Files On-Demand (and vaults). A folder registered before this version gets the vault lock after turning Files On-Demand off and on again.

These need OneUpDrive installed from its setup program, which registers it with Windows. OneUpDrive's menu code runs in a separate helper process, never inside File Explorer itself, so it can't slow down or crash Explorer.
