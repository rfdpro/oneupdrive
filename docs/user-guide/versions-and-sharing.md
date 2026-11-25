# Earlier versions and sharing

OneDrive keeps earlier versions of your files (the *version history*; not the same as the [kept copies](glossary.md) on this PC), and can make links to share them. OneUpDrive shows both on the settings window's **Files** page: select a file, then **Versions** or **Share**. The **Sharing** page lists everything you share. To get there for one file, right-click it in File Explorer and choose **OneUpDrive → Version history** or **OneUpDrive → Copy share link…**.

## Version history
The list shows each version OneDrive keeps: when it was saved, its size and who saved it, newest first.

* **Open** saves a copy of that version outside your synced folder (in `%TEMP%\OneUpDrive versions`) and opens it. Your file doesn't change.
* **Restore** makes that version the current one in OneDrive, with OneUpDrive's next sync. The version it replaces stays in the history, so you can go back. This PC then gets it like any change from OneDrive. If the file changed in OneDrive meanwhile, nothing is restored, and **Recent activity** says so.

From a terminal: `oneup history "C:\…\Report.docx"` lists the versions, `--open 3` opens a copy of the third, `--restore 3` restores it.

The settings window's **Recovery** page shows the same for any file you name, next to the other ways to get things back: [rewinding a whole folder](recovery.md#rewind-a-folder), conflicts and deleted files.

## Sharing
**Copy link** makes a link and puts it on the clipboard: either **anyone with the link can view**, or **can view and edit**. Send it however you like. From a terminal: `oneup share "C:\…\Photos"` (add `--edit` for an editing link).

Anyone who gets the link can open it, so share links with care. The settings window's **Sharing** page lists every link you made and lets you remove them.

## What you share
The settings window's **Sharing** page lists everything you share in OneDrive: each file or folder, each link or person it's shared with, whether they can view or edit, when the link expires, and when you shared it. Links that anyone can open and that never expire are counted at the top. What's inside a shared folder is shared with it and isn't listed on its own (unless it's also shared by itself). Items inside folders you excluded from syncing aren't listed (an excluded folder that's shared itself is). OneUpDrive asks OneDrive a page at a time; **Show more** goes on. Recent results are reused while the settings window is open. Select **Refresh** to check OneDrive again. After you share a large folder, the first look takes longer: OneUpDrive checks once what's inside it.

* **Stop sharing** removes one link or one person's access. A removed link stops working for everyone who has it. The file itself doesn't change.
* **Set expiry** (only with experimental options on, `oneup settings --experimental on`) sets the last day a link works. OneDrive's documentation doesn't cover changing an expiry, so OneUpDrive checks what OneDrive did and tells you the date it now has.

If the item or the link changed in OneDrive since the list was shown, nothing is changed and you're asked to look again. A link shared through a folder can only be removed on that folder. Each change is in **Activity → Recent**, recorded before OneDrive is asked: the kind of link and what happened, never the link itself or the people. If OneUpDrive stops before it knows how a change ended, the next look at **Sharing** (or `oneup shared`) checks with OneDrive and records it.

**Shared with you** lists what other people share with your account. Microsoft is retiring the part of OneDrive that answers this, so the list may come back empty.

From a terminal:

* `oneup shared` lists a page of what you share, with each link's ID and code, and ends with how to get the next page (`--after NEXT`); `--json` gives the same page for scripts. `--with-me` lists what others share with you.
* `oneup shared stop "/Photos/Trip" ID` shows what it would remove and a code; `--expect CODE --yes` removes it, only if it's still exactly as shown. Where two names differ only in case, use the item's ID from `--json`.
* `oneup shared expire "/Photos/Trip" ID --on 2026-12-31` shows the change and its code; `--expect CODE --yes` sets the last day (experimental).
* Items you shared before this version of OneUpDrive appear once they change. `oneup shared --scan` lists all of OneDrive once to find them now; on a large OneDrive that takes a while.

None of this works inside an [encrypted vault](vaults.md): OneDrive only has the encrypted files there.

## Share a file directly

1. Ask the recipient to open **Sharing → Peer-to-peer sharing** and select **Create recipient code**. They can also run OneUp Receive to get a code.
2. Select a file in **Files** and choose **Share → P2P share**. In File Explorer, use **OneUpDrive → P2P share**.
3. Enter the recipient code and select **Share**. When the offer is ready, select **Copy ticket** and send the ticket to the same person.
4. Keep Settings open while the recipient downloads. Select **Stop** to close the offer early. Offers expire after 24 hours.

Choose an ordinary file on this PC. Folders, vault files, and Files On-Demand placeholders are unavailable. The transfer does not use your OneDrive account. Use a private conversation to exchange the recipient code and ticket.

**Peer-to-peer sharing** shows the open connections and whether they are direct or use a relay. OneUpDrive uses public Iroh relays when needed. To require a direct route, select **Direct connection only** when sending and receiving. Both devices must be able to reach each other over the network.

## Receive a file

1. Open **Sharing → Peer-to-peer sharing**, or select **Files → Add → Receive via P2P**.
2. Select **Create recipient code**, then **Copy code**. Send the code to the sender in a private conversation.
3. Paste the ticket the sender returns and select **Receive**. Use **Pause** and **Resume** to continue an interrupted transfer while the offer is open.
4. When the file is received, select **Open folder**. OneUpDrive saves the verified file in its receive folder and preserves any file already there.

### Receive with OneUp Receive

On Windows, select **Show receiver app** in **Peer-to-peer sharing** to find `oneup-receive.exe`. You can copy this executable and the accompanying license notices to another Windows PC. The standalone app needs no OneUpDrive installation.

Run `oneup-receive` in a terminal and send the displayed code to the sender. Paste the returned ticket. The app prints the path of the saved file. Press **Ctrl+C** to pause and use the printed `--session` command to resume. Add `--network direct` to require a direct connection.

The [receiver build instructions](../../CONTRIBUTING.md#build-oneup-receive) cover Windows, Linux, and macOS.
