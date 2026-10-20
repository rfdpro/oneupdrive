# Words used in this guide

**Always keep on this device** (shown as *Always on this PC*): a file that stays downloaded and is never made online-only. See [Files On-Demand](files-on-demand.md).

**Background agent** (`oneupd`): the part of OneUpDrive that does the syncing. It runs without a window while you're signed in to Windows. The icon, the settings window and the `oneup` command all talk to it.

**Backup**: a folder that sends changes from this PC to OneDrive only. Deleting a file here never deletes it in OneDrive. See [Backups](folders.md#backups-one-way-from-this-pc-to-onedrive).

**Conflict copy**: when a file changed on both sides, you get both. One keeps its name; the other becomes `name (conflict 1).ext`.

**Data folder**: where OneUpDrive keeps its settings, records and log (`%LOCALAPPDATA%\OneUpDrive`). Each account has its own. It is not your synced folder.

**Exclude** (also *rule*, *preset*): leave a file or folder out of syncing. Excluding alone deletes nothing. The one exception is the optional **prune** setting, which removes OneDrive's copies of excluded items into OneDrive's recycle bin once they are on this PC ([details](excluding-files.md#what-happens-to-onedrives-copies)). A *preset* is a ready-made list of rules, such as `dev`. See [Choosing what syncs](excluding-files.md).

**Files On-Demand**: files show in File Explorer with their real names but download only when you open them. See [Files On-Demand](files-on-demand.md).

**Held**: an item OneUpDrive leaves unchanged because there is no evidence it was deleted (for example, it is gone from one side with no recorded deletion). Nothing is deleted on a guess. Resolve it on the **Activity** page, **Sync failures** tab, or see [What can't sync](syncing.md#what-cant-sync).

**Folder pair** (`pair` in commands): a folder on this PC and the OneDrive folder it is kept the same as. Your main folder is one; each folder you add is another. See [More folders](folders.md).

**Kept copies** (also *version store*, `.oneup\versions`): copies that OneUpDrive keeps on this PC of files it removed or replaced, so you can get them back. Not the same as *version history*.

**Online-only**: the file is in OneDrive and shows on this PC, but takes no space until you open it.

**Paused folder**: a folder whose changes wait until you resume it.

**Ransomware guard**: a safety stop that triggers when many edited files suddenly stop looking like what they are. See [Syncing in depth](syncing.md#the-content-guard-ransomware).

**Recovery key**: the line starting `OUD1-` that opens an [encrypted vault](vaults.md) if you forget its password.

**Safety stop**: OneUpDrive stops and asks before it deletes, overwrites or moves many files at once. Nothing changes until you answer. See [Syncing in depth](syncing.md#safety-stop).

**Sync failure**: an item that can't sync until you change something, such as a name OneDrive doesn't allow. See [Syncing in depth](syncing.md#what-cant-sync).

**Synced** (a status): the file on this PC and the one in OneDrive are the same.

**Synced folder**: the folder on this PC that OneUpDrive keeps the same as a OneDrive folder.

**Vault**: a folder whose files are encrypted before they reach OneDrive. See [Encrypted vaults](vaults.md).

**Version history**: the earlier versions of a file that OneDrive itself keeps. See [Earlier versions and sharing](versions-and-sharing.md).
