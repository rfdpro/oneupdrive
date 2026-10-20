<div align="center">

<img src="assets/icon.png" width="96" height="96" alt="OneUpDrive logo">

# OneUpDrive

### One up your OneDrive.

Built for speed, privacy and control over your data.<br>
A Windows OneDrive client rebuilt from scratch in Rust.<br>

[![CI](https://github.com/rfdpro/oneupdrive/actions/workflows/ci.yml/badge.svg)](https://github.com/rfdpro/oneupdrive/actions/workflows/ci.yml)
[![Latest release](https://img.shields.io/github/v/release/rfdpro/oneupdrive?include_prereleases&label=release)](https://github.com/rfdpro/oneupdrive/releases/latest)
[![License: MIT](https://img.shields.io/badge/license-MIT-blue.svg)](LICENSE)
[![Platform: Windows 10/11](https://img.shields.io/badge/platform-Windows%2010%20%7C%2011-0078D6?logo=windows)](docs/user-guide/getting-started.md#what-youll-need)
[![Rust 1.88+](https://img.shields.io/badge/rust-1.88%2B-orange?logo=rust)](https://www.rust-lang.org/)

[**Download**](https://github.com/rfdpro/oneupdrive/releases/latest) ·
[User guide](docs/user-guide/README.md) ·
[How it works](docs/architecture/overview.md) ·
[Contributing](CONTRIBUTING.md)

</div>

---

Same OneDrive cloud with a lighter, faster, and more careful engine that syncs what *you* tell it to.

> [!NOTE]
> OneUpDrive is in **beta**. Expect rough edges, and please [report them as issues](https://github.com/rfdpro/oneupdrive/issues).

## ✨ Features

- 🪶 **Lightweight.** Uses under 100 MB of RAM while delivering maximum search and sync speeds.
- 🔒 **Private.** No tracking, no telemetry, no data collection.
- ⚡ **Fast sync.** Our own indexing follows the NTFS change journal, like [Everything](https://www.voidtools.com/), instead of rescanning your folders every minute. When nothing changes, nothing changes.
- ☁️ **Files On-Demand.** Files available only when you need them. Free up files you haven't used lately or keep them on your computer ready for a flight.
- 📝 **Office AutoSave.** Open Word, Excel and PowerPoint files with AutoSave, straight from the right-click menu.
- 🎯 **You decide what syncs.** Leave things out with familiar `.gitignore` syntax. No more `node_modules`, build caches or other junk filling your OneDrive.
- 🚀 **Big uploads.** Adaptive chunking scales by your connection speed.

**Also included:**

- 🔐 **Vaults** (experimental). Encrypted folders, decrypted on demand and unlocked with Windows Hello or password. Only you hold the key: not the app, not Microsoft. Compatible with [Cryptomator](https://cryptomator.org/).
- 🕓 **Version history.** See what changed in any file, when, and which program changed it.

## ⚖️ How it compares

| | The OneDrive app | OneUpDrive |
|---|---|---|
| **Noticing changes** | Scans and compares | Follows the NTFS change journal, like [Everything](https://www.voidtools.com/). Idle means idle |
| **Memory** | Grows with your file count | Bounded, and you pick the profile |
| **Leaving things out** | Not really | `.oneupignore` files (gitignore syntax), ready-made lists like `dev`, paused folders |
| **Files On-Demand** | ✅ | ✅ Same Windows Cloud Files API, plus "free up files I haven't used in 30 days" and "free space when the disk is nearly full" |
| **Explorer** | ✅ | ✅ Navigation pane, status icons, the status button, and a right-click menu with **Version history**, **Share**, **Open with AutoSave** and **Why does this sync?** |
| **Which folders** | One folder per account (plus Desktop/Documents/Pictures, moved into it) | Any number of folders per account, **one-way backups** (deleting here never deletes there), your Desktop and Documents backed up *where they are*, folders others shared with you, several accounts |
| **Control** | Pause | Pause (whole, per direction, per folder, timed), automatic pauses for games, battery saver and metered networks, quiet hours, bandwidth caps, sync order |
| **Ransomware** | Detects afterwards and offers a restore (Microsoft 365 plans) | **Stops first.** Edited files that suddenly stop looking like what they are (a `.docx` that isn't a zip anymore) are held back and you're asked |
| **Knowing what's going on** | A spinner | An activity log, `explain` for any file, and which programs change and open your files, with a chart |
| **End-to-end encryption** | Personal Vault (encrypted with keys Microsoft manages) | [Cryptomator](https://cryptomator.org/)-compatible vaults, decrypted on demand, unlocked with Windows Hello if you like, with a recovery key. Only you hold the key. Experimental until an outside security review |
| **Office AutoSave** | ✅ | ✅ **Open with AutoSave** from the right-click menu, or leave chosen folders to the OneDrive app; double-click integration is an experiment |
| **Big uploads** | Fixed pieces | Piece size adapts to your connection (≈2× faster on our test line), and files OneDrive already has are copied server-side (300 MB in ~2 s) |

## 📦 Getting started

**You'll need** Windows 10 (1809 or newer) or Windows 11, 64-bit, and a **personal** Microsoft account with OneDrive.

1. Download `OneUpDrive.App-stable-Setup.exe` from the [releases page](https://github.com/rfdpro/oneupdrive/releases/latest).
2. Run it. It installs for you alone, with no administrator rights.
3. Sign in, choose your folders and what to leave out. A preview shows what the first sync would do, and nothing changes until you select **Start syncing**.

Switching from the OneDrive app? Read [Moving over from the OneDrive app](docs/user-guide/migrating-from-onedrive.md) first.

> [!IMPORTANT]
> Releases aren't code-signed yet, so Windows SmartScreen may warn you when you run the installer. Signed releases and updates are coming soon.

Want to build it yourself? See [Development in CONTRIBUTING.md](CONTRIBUTING.md#development).

## 🛡️ Your data comes first

OneUpDrive's data-safety rules are enforced in code, not in good intentions ([details](docs/architecture/data-safety.md)):

- **No deletion without evidence.** A file that's simply *missing* (unplugged drive, failed mount) is never treated as deleted.
- **Every change re-checked right before it happens.** OneDrive changes carry eTag preconditions; local changes re-check file ID, size and time.
- **Every transfer verified** by OneDrive's own content hash.
- **Conflicts keep both copies.** Always.
- **Safety stops.** Too many deletions, overwrites or moves at once, or edited files that look encrypted, and OneUpDrive stops and asks.
- **Anything it removes from your PC is kept** as a copy on your PC first, unless OneDrive provably has the exact same content.
- **One gatekeeper.** All destructive operations live in one small module, behind tokens only the safety gate can issue.

## 📚 Documentation

| | |
|---|---|
| **Using it** | [User guide](docs/user-guide/README.md) · [FAQ](docs/user-guide/faq.md) · [Troubleshooting](docs/user-guide/troubleshooting.md) |
| **How it works** | [Architecture overview](docs/architecture/overview.md) · [Data safety](docs/architecture/data-safety.md) |
| **Contributing** | [CONTRIBUTING.md](CONTRIBUTING.md) · [AGENTS.md](AGENTS.md) (for AI coding agents) |
| **Security** | [SECURITY.md](SECURITY.md) |

## 🧭 Scope and known issues

OneUpDrive supports OneDrive **Personal** on **Windows 10/11 (x64)**. Work and school accounts, ARM64 and other operating systems are out of scope for now.

Known issues:

- File Explorer can keep showing the "syncing" status icon after a sync has finished, because of the way Explorer checks sync status.

## 🤝 Contributing

Bug reports, ideas and pull requests are welcome. Start with [CONTRIBUTING.md](CONTRIBUTING.md), and report security problems privately as described in [SECURITY.md](SECURITY.md).

## 📄 License

[MIT](LICENSE). Third-party components and their licenses are listed in [THIRD-PARTY-NOTICES.txt](THIRD-PARTY-NOTICES.txt).

<sub>OneUpDrive is an independent project, not affiliated with or endorsed by Microsoft. OneDrive is a trademark of Microsoft Corporation.</sub>
