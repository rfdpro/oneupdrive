# Databases

Some files are databases: a program keeps them open and changes them bit by bit. Password managers (KeePass, KeePassXC), Thunderbird's mail folders, Access databases, photo catalogues, browser and app data (SQLite), Outlook data files, virtual machine disks. Copy one in the middle of a change and the copy is broken: a password vault or a mailbox that won't open. Sync apps lose people's data this way.

OneUpDrive treats these files differently:

* **It sends a database only when it's whole.** A SQLite database goes once every change is written into the file itself; the program may keep it open. Other databases go while no program has them open: KeePass's vault goes up right after you save it (KeePass closes it between saves), an Access database once you close it.
* **It replaces a database only while it's closed.** A newer version from OneDrive waits while a program has the database open, and is put in place once you close it. If you changed it here meanwhile, you get both copies, as with any other conflict.
* **SQLite's working files never sync.** The `-wal`, `-shm` and `-journal` files next to a SQLite database belong to the program using it, and hold changes on their way into it. They stay on this PC; the database itself carries everything once it's sent. A copy already in OneDrive is left there, untouched. (For a database OneUpDrive knows only by its content, not its name, they sync, but are never moved, removed or replaced while the database is open or while they hold changes it needs.)

Nothing is deleted or overwritten while a database waits. It's only sent, or replaced, later. When Windows can't say whether a program has a database open, OneUpDrive tries again later rather than guess.

Turning off **Wait for programs to close databases** (`oneup databases --wait off`) lets KeePass, Access and the other databases sync while open, as other files do. SQLite databases still wait until every change is in the file, and their working files stay protected.

## Which files count
OneUpDrive recognises databases by their content and their name: SQLite (`.sqlite`, `.db` and any other name), KeePass (`.kdbx`), Access (`.accdb`, `.mdb`), Thunderbird's mail folders and `.msf` files, Outlook data files (`.pst`, `.ost`) and virtual disks (`.vhdx`, `.vhd`, `.vmdk`, `.vdi`, `.qcow2`).

For other programs' databases, turn on a preset or add your own pattern:

| Preset | Files |
|---|---|
| `finance` | QuickBooks, Quicken, GnuCash and Moneydance (`*.qbw`, `*.qbb`, `*.tlg`, `*.qdf`, `*.gnucash`, `*.moneydance`) |
| `firebird` | Firebird and InterBase (`*.fdb`, `*.gdb`) |
| `filemaker` | FileMaker (`*.fmp12`, `*.fp7`) |
| `dbase` | dBase and FoxPro (`*.dbf`, `*.cdx`, `*.mdx`, `*.ndx`) |
| `lotus-notes` | Lotus Notes (`*.nsf`, `*.ntf`) |

None is on unless you turn it on. Patterns are written like [your own rules](excluding-files.md#your-own-rules); `!` in front says a file isn't a database.

In the settings window: **Files** → **Exclude** → **Databases**. From a terminal:

```powershell
oneup databases                      # what counts, and what waits now
oneup databases --on finance         # turn a preset on (--off to turn it off)
oneup databases --add "*.ledger"     # your own pattern (--remove to drop it)
oneup databases --wait off           # KeePass, Access and the rest sync while open; SQLite still waits
```

## Seeing what waits
A database that waits says so, with the program that has it open:

* `oneup status`: `waiting  /Passwords.kdbx: KeePass database open in KeePass; sent once it's closed`
* `oneup explain "C:\Sync\Passwords.kdbx"`: what kind of database it is, and what it's waiting for
* `oneup activity` and the **Activity** page: a *waiting* entry each time
* **Files** → **Exclude** → **Databases** in the settings window

A program that keeps a SQLite database open all day and never writes everything into the file has it sent when you close the program.

## Outlook data files and virtual disks
Outlook writes its `.pst` and `.ost` files for as long as it runs, and a virtual machine its disk for as long as it runs. Each is often gigabytes. OneUpDrive syncs them only while closed, which may be seldom, and tells you about each one it meets (on the **Activity** page's problems, in `oneup status`, and under **Databases**), with two choices:

* **Exclude**: stop syncing that file (`oneup databases --exclude "Mail\archive.pst"`). It stays on this PC and in OneDrive as it is.
* **Keep syncing**: sync it while closed, without the note (`oneup databases --keep-syncing "Mail\archive.pst"`).

It never excludes one without asking. An `.ost` file is a copy of what's on the mail server, so excluding it loses nothing; Outlook's own help explains how to back up a `.pst` file.
