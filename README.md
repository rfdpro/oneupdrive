<img src="assets/icon.svg" width="64" height="64" alt="">

# OneUpDrive

**Your OneDrive, minus the drama.**

OneUpDrive is a from-scratch OneDrive client for Windows, written in Rust. Same OneDrive account, same files, same onedrive.com and phone apps. Just a sync program on your PC that sips resources instead of gulping them, refuses to lose your data, and does what *you* tell it.

> 🧪 **Status: first release.** Two-way sync and backups, several folders and accounts, Files On-Demand, Explorer integration, encrypted vaults, Office AutoSave, a settings window and signed self-updates all work today. It's young software: expect rough edges, and please report them as issues.

## Why does this exist?

Because one developer's OneDrive app was tracking **3.3 million items** in **~6 GB of local databases**, and **89%** of them were `node_modules`, Python virtual environments, `.git` folders and build caches that nobody wanted in the cloud. OneDrive Personal had no way to say "please don't".

So we built the client we wanted:

| | The OneDrive app | OneUpDrive |
|---|---|---|
| **Noticing changes** | Scans and compares | Follows the NTFS change journal, like [Everything](https://www.voidtools.com/). Idle means idle |
| **Memory** | Grows with your file count | Bounded, and you pick the profile |
| **Leaving things out** | Not really | `.oneupignore` files (gitignore syntax), ready-made lists like `dev`, paused folders |
| **Files On-Demand** | ✅ | ✅ Same Windows Cloud Files API, plus "free up files I haven't used in 30 days" and "free space when the disk is nearly full" |
| **Explorer** | ✅ | ✅ Navigation pane, status icons, the status button, and a right-click menu with **Version history**, **Share**, **Open with AutoSave** and **Why does this sync?** |
| **Which folders** | One folder per account (plus Desktop/Documents/Pictures, moved into it) | Any number of folder pairs per account, **one-way backups** (deleting here never deletes there), your Desktop and Documents backed up *where they are*, folders others shared with you, several accounts |
| **Control** | Pause | Pause (whole, per direction, per folder, timed), automatic pauses for games, battery saver and metered networks, quiet hours, bandwidth caps, sync order |
| **Ransomware** | Detects afterwards and offers a restore (Microsoft 365 plans) | **Stops first.** Edited files that suddenly stop looking like what they are (a `.docx` that isn't a zip anymore) are held back and you're asked |
| **Knowing what's going on** | A spinner | An activity log, `explain` for any file, and which programs change and open your files, with a chart |
| **End-to-end encryption** | Personal Vault (encrypted with keys Microsoft manages) | [Cryptomator](https://cryptomator.org/)-compatible vaults, decrypted on demand, unlocked with Windows Hello if you like, with a recovery key. Only you hold the key |
| **Office AutoSave** | ✅ | ✅ **Open with AutoSave** from the right-click menu, or leave chosen folders to the OneDrive app; double-click integration is an experiment |
| **Big uploads** | Fixed pieces | Piece size adapts to your connection (≈2× faster on our test line), and files OneDrive already has are copied server-side (300 MB in ~2 s) |

## The non-negotiables

OneUpDrive's data-safety rules are enforced in code, not in good intentions ([details](docs/architecture/data-safety.md)):

* **No deletion without evidence.** A file that's simply *missing* (unplugged drive, failed mount) is never treated as deleted.
* **Every change re-checked right before it happens.** OneDrive changes carry eTag preconditions, local changes re-check file ID, size and time.
* **Every transfer verified** by OneDrive's own content hash.
* **Conflicts keep both copies.** Always.
* **Circuit breakers.** Too many deletions, overwrites or moves at once, or edited files that look encrypted, and OneUpDrive stops and asks.
* **Anything it removes from your PC is kept** in a local version store first, unless OneDrive provably has the exact same content.
* All destructive operations live in one small module, behind tokens only the safety gate can issue.

## Getting it

Download `OneUpDrive.App-stable-Setup.exe` from the GitHub releases page: per-user, no admin rights, Authenticode-signed, and updating itself from signed releases only. The first launch walks you through setup and shows a preview before anything changes.

Building from source: see [CONTRIBUTING.md](CONTRIBUTING.md#development).

## Documentation

* **Using it:** [User guide](docs/user-guide/README.md)
* **How it works:** [Architecture overview](docs/architecture/overview.md) · [Data safety](docs/architecture/data-safety.md)
* **Contributing:** [CONTRIBUTING.md](CONTRIBUTING.md) · [AGENTS.md](AGENTS.md) (for AI coding agents)
* **Security:** [SECURITY.md](SECURITY.md)

## Scope

OneDrive **Personal** on **Windows 10/11 (x64)**. Work and school accounts, ARM64 and other operating systems are out of scope for now.

## License

[MIT](LICENSE). Third-party components and their licenses: [THIRD-PARTY-NOTICES.txt](THIRD-PARTY-NOTICES.txt).

OneUpDrive is an independent project, not affiliated with or endorsed by Microsoft. OneDrive is a trademark of Microsoft Corporation.
