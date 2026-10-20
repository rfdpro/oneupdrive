# Finding files

OneUpDrive can find any file or folder in your OneDrive by name, straight away, online-only files included. It doesn't download anything or ask OneDrive: it searches its own list of what's in your OneDrive, which it keeps up to date as it syncs. Windows Search can't find online-only files without downloading them.

## Searching
Open search in any of these ways:
* the settings window's **Search** page;
* **Search** in the [OneUpDrive icon's](tray-icon.md) menu;
* a hotkey of your choice, like Ctrl+Alt+F (none until you set one; see below);
* `oneup find` on the command line.

Type at least 3 characters of a name, in any case: `port` finds *Report.docx* and *Passport scan.pdf*. Results appear as you type. Names that match exactly come first, then names that start with what you typed, then the rest. The first 50 are listed; when there are more, add to the search to narrow it. **Too many matches. Type more.** means what you typed is too common to search through: add a few characters.

Each result says where it is:

| | |
|---|---|
| **On this PC** | Its content is here. |
| **Online-only** | It's in your synced folder, and opening it downloads it. See [Files On-Demand](files-on-demand.md). |
| **Not on this PC yet** | It's in your synced folder but hasn't been downloaded yet. |
| **Excluded** | Your rules leave it out, so it's only in OneDrive. See [Choosing what syncs](excluding-files.md). |
| **Not synced** | It's in a part of OneDrive outside the folder you sync. |
| **Stays in OneDrive** | A OneNote notebook or your Personal Vault: OneUpDrive never syncs these; they work in OneDrive itself. |

Beside each result:
* **Open** opens it (an online-only file downloads first).
* **Show** opens its folder in File Explorer, with it selected.
* **Sync this first** (for an item not on this PC yet) downloads it before anything else waiting.
* **Open in OneDrive** (for an item with no copy on this PC) opens it on OneDrive's website.

## What search doesn't find
* **Items inside excluded folders.** OneUpDrive never lists what's inside a folder your rules exclude (a `node_modules` folder, say), so it can't find what's in it. The excluded folder itself is found. Every result list says so.
* **Files on this PC that haven't been uploaded yet.** Windows Search finds those.
* **Text inside files**, unless you turn on OneDrive's search (below).

The first time OneUpDrive starts with search, it builds the search index: from a few seconds up to about a minute for the largest drives. A search made before that finishes builds the rest first, and says so.

## Searching inside files with OneDrive
OneDrive can search the text inside your documents too. This is off until you turn it on, because it **sends what you search for to Microsoft**: on the Search page, turn on **Search inside files with OneDrive**. It runs only when you press **Search** (or Enter), never while you type, and its results are listed under **Inside files (OneDrive's search)**. Each result says where it is, as above.

On the command line, add `--onedrive` to a search, each time you want it.

## The hotkey
On the Search page, type the keys under **Hotkey**, like `Ctrl+Alt+F`: Ctrl, Alt or Win (or several), with a letter, a digit, F1 to F24 or Space. Pressing them in any program opens the Search page. They work while OneUpDrive's icon is in the notification area. If another program already uses those keys, the icon tells you; choose others. Clear the box to have none.

## On the command line
```
oneup find budget 2024
oneup find invoice --limit 200
oneup find --json passport
oneup find --onedrive "quarterly results"
```
`oneup find` prints each result's place and its path in OneDrive (folders end in `/`). `--json` gives the same as JSON, for scripts.

```
oneup search
oneup search --onedrive on
oneup search --hotkey Ctrl+Alt+F
oneup search --hotkey off
```
`oneup search` shows or changes the search settings: whether the settings window also asks OneDrive to search inside files, and the hotkey. They're saved in the `[search]` section of `config.toml` in the data folder.
