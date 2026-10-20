# Earlier versions and sharing

OneDrive keeps earlier versions of your files (the *version history*; not the same as the [kept copies](glossary.md) on this PC), and can make links to share them. OneUpDrive shows both on the settings window's **Files** page: select a file, then **Versions** or **Share**. To get there for one file, right-click it in File Explorer and choose **OneUpDrive → Version history** or **OneUpDrive → Copy share link…**.

## Version history
The list shows each version OneDrive keeps: when it was saved, its size and who saved it, newest first.

* **Open** saves a copy of that version outside your synced folder (in `%TEMP%\OneUpDrive versions`) and opens it. Your file doesn't change.
* **Restore** makes that version the current one in OneDrive. The version it replaces stays in the history, so you can go back. This PC gets the restored version with the next sync, like any change from OneDrive. If the file changed in OneDrive meanwhile, nothing is restored and you're asked to look again.

From a terminal: `oneup history "C:\…\Report.docx"` lists the versions, `--open 3` opens a copy of the third, `--restore 3` restores it.

## Sharing
**Copy link** makes a link and puts it on the clipboard: either **anyone with the link can view**, or **can view and edit**. Send it however you like. From a terminal: `oneup share "C:\…\Photos"` (add `--edit` for an editing link).

Anyone who gets the link can open it, so share links with care. You can see and remove sharing links on onedrive.com.

Neither works inside an [encrypted vault](vaults.md): OneDrive only has the encrypted files there.
