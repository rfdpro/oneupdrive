# Finding files

OneUpDrive can find any file or folder in your OneDrive by name, straight away, online-only files included. It doesn't download anything or ask OneDrive: it searches its own list of what's in your OneDrive, which it keeps up to date as it syncs. Windows Search can't find online-only files without downloading them.

## Searching
Open search in any of these ways:
* the **Search by name** box in **Files**;
* **Search** in the [OneUpDrive icon's](tray-icon.md) menu;
* a hotkey of your choice, like Ctrl+Alt+F (none until you set one; see below);
* `oneup find` on the command line.

Open **Filters** beside the search box and choose **Synced folder** to search the selected folder, including files that are still waiting to upload. Choose **OneDrive** to search the indexed drive, including files outside your synced folders. Filter either scope to **Files only** or **Folders only**. Search results use the full width. When browsing the tree, select **Details** to hide or show the right panel.

In **Filters**, choose how to match names:

* **Contains text** finds the exact text anywhere in the name, ignoring case.
* **All words** finds every word anywhere in the same name, in any order. For example, `pdf budget 24` finds *Budget-24.pdf*. In OneDrive scope, include at least one word with three or more characters.
* **Regex** matches a regular expression such as `^report.*\.pdf$`. Press **Enter** to run it. Regex searches can take longer on large drives.
* **Words in path** matches words across folder and file names. For example, `acme invoice` finds *Clients/Acme/Final/invoice.pdf*.

With **Contains text** and the default filters in OneDrive scope, type at least three characters. For example, `port` finds *Report.docx* and *Passport scan.pdf*. Exact names come first, followed by names that start with the search text. If you see **Too many matches. Type more.**, add more characters to narrow the search.

### Narrow and sort results

To search a folder and its subfolders, right-click the folder in **Files** and choose **Search within this folder**. Choose **Clear folder** to return to the full synced folder.

In **Filters**, enter an **Extension** such as `pdf`, a **Modified on or after** date, or a **Minimum size (MB)**. Use **Availability** to find items available offline, online-only placeholders, or items absent from this PC. You can leave the search box empty to use filters alone. Availability filters use the last recorded state on this PC.

Under **Sort by**, choose **Newest**, **Largest**, or **Path A–Z**. Press **Search** or **Enter** to run a path search or apply the new filters and sort. Select **Load more** to show another 50 matches, up to 500. If a one-word search returns no matches, OneUpDrive can offer a spelling suggestion. Select **Try** with the suggested word to run it.

### Save a search

Open **Filters → Saved searches**. Enter a **Search name** and select **Save** to keep the current text, scope, filters, and sort on this PC. Select a saved search and choose **Run** to restore it. Save under the same name to update it, or select **Remove** to delete the saved search.

### Search local drives

Turn on **Index local drives** in **Set Up** or **General → Search**. When a drive's first scan finishes, choose **This PC** in **Filters** to find names on that drive. **Everywhere** searches all enabled indexes, OneDrive, and your configured OneDrive shared folders.

The local index covers accessible folders on fixed NTFS and ReFS drives that have a Windows change journal. These scopes support **Contains text**, **All words**, and the file and folder filter. Name queries stay on this PC. The index stores file names and metadata, and skips configured vault folders, disallowed locations, and files inside unpopulated cloud folders.

The index follows your exclusions (**Rules**, or `[rules]` in `config.toml`) on every drive: the presets you turned on, such as `dev` for `node_modules` and `.git`, and your own rules that name things anywhere, such as `*.iso` or `build/`. A rule for a place inside your synced folder, such as `/Documents/drafts/`, applies to the synced folder only. When you change your exclusions, the index updates in place, without scanning the drive again.

Next to the drive status, **Index size** shows how much disk space the indexes take and how much **Compact** frees at least. A drive's index is compacted once by itself after its first scan. An update that changes how the index is stored scans your drives again by itself; until a drive's scan ends, it shows **Indexing…** and isn't in **This PC** results.

In **General → Search**, select **Refresh** to see which drive indexes are ready. A drive shows **Indexing…** while it's scanned; the first scan of a large drive can take hours. A drive needs its change journal to appear in results. If a drive shows **Index unavailable**, `drive-index.log` in the data folder (`%LOCALAPPDATA%\OneUpDrive`) says why. While a drive is scanned, the same file notes every five minutes how many items it has indexed and how fast. Drives that are already **Ready** stay up to date meanwhile. If Windows resets the journal or discards the changes the index still needs, the drive returns to results after a fresh scan. **Pause indexing** leaves local catalogs searchable while their journals are available. Turning off **Index local drives** hides **This PC** and keeps the catalogs for later use.

### Search network shares

Map an SMB share to a drive letter in Windows. Turn on **Index network shares** in **Set Up** or **General → Search**, then choose **Network shares** in **Filters**. **Everywhere** also includes these results. The index stores names on your PC. Searches check that the results are still accessible using your Windows account.

The share must support Windows directory listings and change notifications. When a share disconnects, indexing pauses, or Windows loses notifications, that catalog stops appearing in results. It returns after a fresh scan. Select **Refresh** in **General → Search** to check its status.

Each result says where it is:

| | |
|---|---|
| **On this PC** | Its content is here. |
| **On a network share** | It opens through the mapped drive. |
| **Online-only** | It's in your synced folder, and opening it downloads it. See [Files On-Demand](files-on-demand.md). |
| **Not on this PC yet** | It's in your synced folder but hasn't been downloaded yet. |
| **Excluded** | Your rules leave it out, so it's only in OneDrive. See [Choosing what syncs](excluding-files.md). |
| **Not synced** | It's in a part of OneDrive outside the folder you sync. |
| **Stays in OneDrive** | A OneNote notebook or your Personal Vault: OneUpDrive never syncs these; they work in OneDrive itself. |

Click a result to open a file in its default app, open a folder in File Explorer, or open an item that only exists in OneDrive on the web. Opening an online-only file downloads it first.

Use the buttons beside a result for other actions:
* **Show** selects the item in the Files tree when it belongs to a synced folder.
* **Sync this first** (for an item not on this PC yet) downloads it before anything else waiting.
* **Open in OneDrive** (for an item with no copy on this PC) opens it on OneDrive's website.

## What search doesn't find
* **Items inside excluded folders.** OneUpDrive never lists what's inside a folder your rules exclude (a `node_modules` folder, say), so it can't find what's in it. The excluded folder itself is found. Every result list says so.
* **Files on this PC that haven't been uploaded yet** appear only in the **Synced folder** scope.
* **Text inside files**, unless you turn on OneDrive's search (below).

The first time OneUpDrive starts with search, it builds the search index: from a few seconds up to about a minute for the largest drives. A search made before that finishes builds the rest first, and says so.

## Searching inside files with OneDrive
OneDrive can search the text inside your documents too. To allow OneUpDrive to **send what you search for to Microsoft**, turn on **Search inside files with OneDrive** in **Settings > General > Search**. In **Files**, choose **OneDrive**, **Contains text**, and the default metadata filters and sort. Press **Search** or Enter. Results appear under **Inside files (OneDrive's search)**.

On the command line, add `--onedrive` to a search, each time you want it.

## The hotkey
In **Settings > General > Search**, type the keys under **Hotkey**, like `Ctrl+Alt+F`: Ctrl, Alt or Win (or several), with a letter, a digit, F1 to F24 or Space. Pressing them in any program opens Files with the search box focused. They work while OneUpDrive's icon is in the notification area. If another program already uses those keys, the icon tells you; choose others. Clear the box to have none.

## On the command line
```
oneup find budget 2024
oneup find invoice --limit 200
oneup find --words --kind files pdf budget 24
oneup find --regex --kind folders "^project [0-9]+$"
oneup find --json passport
oneup find --this-pc invoice
oneup find --network-shares invoice
oneup find --everywhere invoice
oneup find --onedrive "quarterly results"
```
`oneup find` prints each result's place and its path in OneDrive (folders end in `/`). `--json` gives the same as JSON, for scripts.

Use `--kind files` or `--kind folders` to filter by item type. `--words` and `--regex` search names. Use `--onedrive` with plain text to also search inside files.

```
oneup search
oneup search --onedrive on
oneup search --hotkey Ctrl+Alt+F
oneup search --hotkey off
oneup search --local-drives on
oneup search --network-shares on
oneup search --pause-index on
oneup search --compact-index
```
`--compact-index` compacts the local drive indexes. With indexing running, they're compacted once the update under way ends.
`oneup search` shows or changes the search settings: whether the settings window also asks OneDrive to search inside files, and the hotkey. They're saved in the `[search]` section of `config.toml` in the data folder.
