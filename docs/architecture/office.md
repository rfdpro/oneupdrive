# Office AutoSave

Office's AutoSave and co-authoring need Office to edit the file **in OneDrive**, not on disk: AutoSave is on for files in OneDrive and SharePoint and off elsewhere. For the OneDrive app's folder, Office maps the local path to a OneDrive address itself, through an arrangement between the two programs. OneUpDrive offers three documented routes to the same result, plus an experiment. User guide: [Office](../user-guide/office.md).

## How Office decides
* **Opening by address is documented.** Office's URI schemes (`ms-word:ofe|u|<url>`, `ms-excel:…`, `ms-powerpoint:…`) open a file from its OneDrive address, and Graph's `driveItem.webDavUrl` gives that address (`https://d.docs.live.net/<cid>/<path>`). Office opens it from OneDrive itself, with AutoSave, using the Microsoft account it's signed in with.
* **Sync programs can register with Office**, as Microsoft documents for its Cloud Storage Partner Program (`HKCU\Software\SyncEngines\Providers\<engine>`, a COM handler that answers each file's address, checksum and flags). In practice Office ignores registrations from programs other than the OneDrive app and registered storage partners: with a correct registration and a working handler, Office never calls the handler and opens files locally. So this route is an experiment only (below).

## Open with AutoSave
`oneup office PATH`, Explorer's **OneUpDrive → Open with AutoSave**, and the settings window's Office page. For a synced Word, Excel or PowerPoint file, OneUpDrive reads its `webDavUrl` and opens it with Office's URI scheme. Office edits the file in OneDrive; OneUpDrive brings each saved version down by syncing (a local edit made meanwhile meets it as a conflict, and both are kept). Refused for files in vaults (OneDrive only holds ciphertext) and backup pairs (a backup puts this PC's version back over cloud edits).

## OneUpDrive as the default program (`oneup_app::officeopen`)
`oneup-open.exe` (no window), `oneup office-opener on|off|--explain`, and the settings window's Office page.
* **Registering** (per user, no admin rights) writes a file type per extension (`.docx`, `.docm`, `.xlsx`, `.xlsm`, `.xlsb`, `.pptx`, `.pptm`), `OneUpDrive.Office.<ext>`, copied from Office's own (icon, type name, other verbs such as Print and New) with only `shell\open` replaced; the extension's `OpenWithProgids`; and `RegisteredApplications` → `Software\OneUpDrive\Capabilities\FileAssociations` for Default apps. Windows keeps the choice itself (`UserChoice`, hashed), so the person makes it in Default apps or Open with. Templates and slide shows aren't taken (their default verb isn't "open"), nor the old binary formats (no AutoSave).
* **On each open**, `officeopen::choose` takes the AutoSave route only for a supported file in a synced folder, not in a vault or a backup pair, unchanged since its last sync (size and last-write time against its Synced row), with a `webDavUrl` within 15 s. Everything else goes to Office's own file type with `ShellExecuteEx(SEE_MASK_CLASSNAME)`, so it opens exactly as Office's "open" verb would.
* Office's registration can't be copied with `RegCopyTreeW` (access denied from any process), so the copy is a plain recursive one that skips unreadable sub-keys.

## Folders left to the OneDrive app
`[onedrive_app] folders` and `top_files` in `config.toml`: a last rule layer (`Source::OneDriveApp`) that no `!` rule can undo and cloud cleanup never touches. The OneDrive app is then set to sync only those folders ("Choose folders") into its own local folder; local folders never overlap, so neither program manages the other's files. The OneDrive app always syncs files at the drive's top level; `top_files` leaves those to it too (relevant only when OneUpDrive syncs the whole drive).

## The registration experiment
`oneup office-integration on|off` (off by default) writes `HKCU\Software\SyncEngines\Providers\OneUpDrive` (`Handler` = `oneup_shell.dll`'s Office handler, `HashAlgorithm` = `SHA256`) and a sub-key per pair with `MountPoint` and `UrlNamespace`. The handler (`oneup_shell::office::Handler`, `IStorageProviderHandler`) answers from `oneup office-props PATH`: the `webDavUrl`, whether the file is unchanged since its last sync, and its SHA-256; a changed file is answered as not co-authorable, so Office opens it locally. Each request is noted (without names) in `%LOCALAPPDATA%\OneUpDrive\office-integration.log`. The switch stays in case a later Office accepts it.

The registry key must reach the real registry, not the identity package's private hive, which is why the package turns write virtualization off ([explorer.md](explorer.md#package-identity-a-sparse-package)).
