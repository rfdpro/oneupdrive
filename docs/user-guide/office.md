# Office: AutoSave and editing together

With the OneDrive app, Word, Excel and PowerPoint save files in your OneDrive folder as you type (**AutoSave**) and let others edit at the same time. Office does that by working with the file in OneDrive itself, and it recognises the OneDrive app's folder through a private arrangement between the two. OneUpDrive offers these ways to get the same, all on the settings window's **Office** page.

## 1. Open with AutoSave (works today)
Right-click a Word, Excel or PowerPoint file in your synced folder, then **OneUpDrive → Open with AutoSave**. Or on the **Office** page, **Choose a file…**, or from a terminal: `oneup office "C:\…\Report.docx"`.

Office then opens the file **from OneDrive**: AutoSave is on, others can edit with you, and the title bar shows it's in OneDrive. OneUpDrive brings every saved version to the file on this PC as it syncs.

* Office must be signed in with the same Microsoft account as OneUpDrive (Office: **File → Account**).
* AutoSave works with today's formats (`.docx`, `.xlsx`, `.pptx` and their kin). Older ones (`.doc`, `.xls`) open the same way but without AutoSave, as in the OneDrive app.
* Double-clicking opens the file on this PC as an ordinary file (without AutoSave), unless you use one of the next two options.

## 2. Double-click with AutoSave: OneUpDrive as the default program (works today)
OneUpDrive can be the program Windows uses for Word, Excel and PowerPoint files (`.docx`, `.docm`, `.xlsx`, `.xlsm`, `.xlsb`, `.pptx`, `.pptm`). It then decides each time you open one:

* **In a folder OneUpDrive syncs:** the file opens from OneDrive, with AutoSave, as with **Open with AutoSave**.
* **Anywhere else** (your Desktop, a USB stick, an e-mail attachment, the OneDrive app's folder…): it goes straight to Word, Excel or PowerPoint, exactly as before.
* Also opened in Office as usual, on this PC: a file you changed here that hasn't synced yet (so you see your latest version), a file in an encrypted vault or a backup folder, and any time OneDrive can't be reached.

To set it up:
1. On the **Office** page, turn on **Let OneUpDrive open Office files** (or `oneup office-opener on`). This only offers OneUpDrive to Windows; nothing changes yet.
2. Windows lets only you choose default programs, so its **Default apps** page opens: under OneUpDrive, set it as the default for the file types you want. (Or right-click an Office file, **Open with → Choose another app**, pick OneUpDrive and tick **Always**.)

Your files keep their Word, Excel and PowerPoint icons, and the other right-click commands (Print, New, …) still go to Office. To see what would happen with a file: `oneup office-opener --explain "C:\…\Report.docx"`. To stop, turn the switch off (or `oneup office-opener off`); if OneUpDrive was the default, Windows asks which program to use the next time you open such a file: pick Word, Excel or PowerPoint. Uninstalling OneUpDrive removes it too.

## 3. Leave some folders to the OneDrive app (works today)
If you want double-click to give you AutoSave, let the OneDrive app keep the folders where you work on Office files, and OneUpDrive everything else:

1. On the **Office** page, under **Leave folders to the OneDrive app**, add those OneDrive folders (like `/Office`), or `oneup onedrive-app --add /Office`. OneUpDrive leaves them alone from then on, whatever your other rules say, and never removes them from OneDrive.
2. In the OneDrive app: **Settings → Account → Choose folders**, and tick only those folders. Its folder on this PC must be a different one from OneUpDrive's (the OneDrive app's own, usually `C:\Users\you\OneDrive`).
3. Open Office files from the OneDrive app's folder: AutoSave works as before.

The OneDrive app always syncs the files at the very top of your OneDrive (outside any folder), whatever you tick. To keep the two programs from both syncing them, also tick **Also the files at the top of your OneDrive** (or `--top-files on`); this matters only when OneUpDrive syncs your whole OneDrive.

## 4. Tell Office about OneUpDrive's folders (experimental, doesn't work yet)
Microsoft documents how Office recognises *other* sync programs' folders. OneUpDrive can register itself that way, for developers trying it: experimental options are hidden until you turn them on with `oneup settings --experimental on` (or `experimental = true` under `[dev]` in `config.toml`). Then, on the **Office** page, turn on **Tell Office about OneUpDrive's folders** (or `oneup office-integration on`). Office may then open files you double-click in OneUpDrive's folders from OneDrive, with AutoSave.

**Tried in October 2026, it didn't work:** Office (Microsoft 365, Word) never asked OneUpDrive about the file, and opened it as an ordinary local file. Microsoft's documentation describes this for storage services that have joined its partner program and been given a service ID, which OneUpDrive can't get, so use options 1 to 3. The switch stays in case a later Office changes this: if you try it, OneUpDrive notes each time Office asks about a file (without file names) in `%LOCALAPPDATA%\OneUpDrive\office-integration.log`. A file changed on this PC since it last synced always opens as an ordinary local file, so your unsynced changes can't be bypassed. Turning it off removes everything it set.
