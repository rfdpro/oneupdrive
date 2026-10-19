# Architecture overview

This page explains how OneUpDrive works in plain terms. Each section links to the detailed design.

## The pieces
* **`oneupd`**: the background agent. It runs in your user session without admin rights, does all the syncing, and is the Files On-Demand provider. One agent runs per data folder (an account, an extra folder pair or a vault).
* **`oneup`**: the command-line tool (status, pause, explain, …). It's a *separate program* on purpose: Windows treats file writes by the provider's own program specially, so user-facing tools must never share its executable. It talks to the running agent through a private channel that only you can open ([control-api.md](control-api.md)).
* **`oneup-tray`**: a small native icon, menu and notifications, also separate. It follows the agent over the same private channel ([control-api.md § The tray](control-api.md#the-tray)). It's the Start menu's program: launching it starts everything.
* **`oneup-settings`**: the settings window (a page in WebView2). It changes the same settings the command line does, with the same checks, and talks to the agent over the same channel. Its Files page, a tree of each synced folder, is the main way to manage what syncs ([settings-window.md](settings-window.md)).
* **`oneup_shell.dll`** and the **`OneUpDrive.Sync` identity package**: File Explorer integration (navigation pane, status, the right-click menu), loaded in a separate helper process, never in Explorer ([explorer.md](explorer.md)).
* **`oneup-open`**: optionally the default program for Office files, so files in OneUpDrive's folders open with AutoSave.
* An installer with signed automatic updates (Velopack, GitHub Releases; [releases.md](releases.md)).

| Crate | Role |
|---|---|
| `oneup-ntfs`, `oneup-local` | The change journal, folder listing and the Local tree ([local-tree.md](local-tree.md)) |
| `oneup-graph`, `oneup-remote` | Microsoft Graph and the Remote tree ([remote-mirror.md](remote-mirror.md)) |
| `oneup-store` | The SQLite state database: the three trees, deletion evidence, dirty marks, activity, usage, the name index for search ([search.md](search.md)) |
| `oneup-rules` | Exclusion rules, presets and `explain` ([rules.md](rules.md)) |
| `oneup-plan`, `oneup-sync` | The first-run planner ([planner.md](planner.md)) and the three-tree reconciler ([sync-engine.md](sync-engine.md)); both pure |
| `oneup-gate` | The safety gate: breakers, the content guard, approvals; pure ([data-safety.md](data-safety.md)) |
| `oneup-effects` | The only code that changes user content, behind the gate's tokens |
| `oneup-exec` | The executor, hydration service and placement pass |
| `oneup-cfapi` | The Cloud Files provider ([files-on-demand.md](files-on-demand.md)) |
| `oneup-ipc` | The control API |
| `oneup-hooks` | Event hooks: the user's programs started on sync events, contained ([hooks.md](hooks.md)) |
| `oneup-vault`, `oneup-vaultsync` | Cryptomator vaults and vault pairs ([vaults.md](vaults.md)) |
| `oneup-sys`, `oneup-shellext` | Windows helpers; the Explorer extension |

## How it notices your changes
NTFS keeps a journal of every change on a volume. OneUpDrive keeps a bookmark in that journal and asks Windows to wake it up when something new is written, much like the search tool *Everything*. While nothing changes, it uses no CPU. After a restart it reads only what changed since its bookmark. Every file is tracked by its NTFS file ID, so renames and moves cost nothing ([local-tree.md](local-tree.md)).

## How it notices cloud changes
OneDrive pushes a "something changed" signal over a websocket. OneUpDrive then asks Microsoft Graph's `/delta` for exactly what changed since last time. The first time, it builds its picture of your OneDrive in parallel and skips excluded folders entirely, instead of one multi-hour listing ([remote-mirror.md](remote-mirror.md)).

## How it decides what to do
OneUpDrive keeps three views of your files: what's on disk (**Local**), what's in OneDrive (**Remote**), and the last state both agreed on (**Synced**). Comparing each side to Synced tells it *which side changed*, so it never guesses from timestamps. A pure planning step turns the differences into a list of operations. A safety gate checks them, and an executor carries them out, recording each step so a crash can never leave things half-done ([sync-engine.md](sync-engine.md)). Only folders touched by a change are planned again, so the cost follows changes, not the size of the tree.

## What syncs
Gitignore-style rules (`.oneupignore` files and the settings), presets you opt into, paused folders, and `oneup explain` to say why any item syncs or not. Changing a rule never needs a rescan, and excluding never deletes anything ([rules.md](rules.md)).

## Files On-Demand
Files can exist as placeholders that look normal but download when opened (Windows Cloud Files API). Freeing up space only happens under a strict verify-then-dehydrate protocol, designed after experiments reproduced a data-loss path in the raw Windows API ([files-on-demand.md](files-on-demand.md)).

## More folders, backups and accounts
An account can sync any number of folder pairs: each extra pair is a data folder of its own, with its own agent and a cloud listing of just its folder (which may be in someone else's drive, for a folder they shared). Any pair can be a one-way backup, planned by the same reconciler under a rule set that never changes the PC or deletes from the cloud. Several accounts are several data folders ([pairs.md](pairs.md)).

## Office
Office edits a file in OneDrive itself when it's opened from its OneDrive address, which is how **Open with AutoSave** works. OneUpDrive can be the default program for Office files (`oneup-open.exe`: its folders' files open that way, others in Office as usual), and folders can be left to the OneDrive app ([office.md](office.md)).

## Encrypted vaults
Folders encrypted in the Cryptomator format, so OneDrive only ever sees ciphertext. A vault in OneDrive syncs with a folder on the PC, decrypted (online-only files decrypted when opened, by default), by an agent of its own that runs only while the vault is unlocked ([vaults.md](vaults.md)).

## Finding files
Every name in OneDrive is already in the Remote tree, so search needs no network: a trigram index of those names, kept current by the same kind of database triggers as the dirty marks, finds any part of a name across millions of items in milliseconds, online-only files included, and each result says whether it's here, online-only or excluded. OneDrive's own search inside files is an opt-in extra that sends the search text to Microsoft ([search.md](search.md)).

## Knowing how files are used
The agent counts which programs open online-only files (Windows names them in each download request) and which write files (asked of Windows' Restart Manager as the change journal reports each write), per hour, on the PC only. The same records drive the automatic space policies, which make long-unused files online-only, and the content guard reads the start of each edited file before upload to catch files that suddenly look encrypted ([usage.md](usage.md)).

## Languages and accessibility
What the window, the tray and the command line say is looked up by its English text in a catalogue for the user's display language, with English wherever a translation is missing; numbers and dates follow the regional format ([localisation.md](localisation.md)). The settings window works from the keyboard and with a screen reader, and follows Windows' contrast themes ([settings-window.md § Accessibility](settings-window.md#accessibility)).

## Scripts and hooks
The agent's events (synced, conflict, failure, safety stop, paused/resumed, problem) are a feed: `oneup watch` and scripts read it over the control channel, and hooks start the user's programs on them, in the background, bounded and contained. A program in a synced folder never runs as a hook, so a change in the cloud can't become code on the PC ([hooks.md](hooks.md)).

## Data safety
No deletion without explicit evidence, every change checked against the latest state right before it happens, every transfer verified by hash, circuit breakers on mass changes and on edited files that look encrypted, and both copies kept on conflict. The full list of invariants: [data-safety.md](data-safety.md).
