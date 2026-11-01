# Settings on another PC

OneUpDrive can save exclusions, the sync interval, push notification preference and database rules in **oneup.settings** at the top of your OneDrive. This is optional. Passwords, sign-in, local folder paths, pin choices, paused folders, resource limits, hooks and deletion approvals stay on each PC. Decisions about initial local-only files also stay on that PC.

On the first PC, open **Settings > General > Shared settings**, then **Review**. The comparison shows this PC's portable settings and the settings currently in OneDrive. Select **Publish** to save this PC's settings in OneDrive. Turn on **Publish future settings changes** before publishing if you want later local changes shared automatically. **Disable** stops automatic publication and keeps the file in OneDrive.

On another PC, setup looks for the file after sign-in. Review the comparison and select **Import** to use the cloud settings. You can import once, or turn on **Publish future settings changes** to share later edits from this PC too. The comparison identifies the OneDrive folder the exclusions concern; setup uses that folder after import. Existing folder pairs cannot import settings for a different OneDrive folder.

An import changes only the portable fields shown. Changes made on another PC are never silently imported. If the settings change while you review them, select **Review** again. If another PC changes the cloud file before automatic publication, publication stops and Activity records the problem. Review the two versions before importing or publishing again. Local preferences remain available throughout.

Automatic publication waits while uploads are paused or the network policy holds file transfers. It creates no separate polling timer.

## Choosing initial files

An older OneDrive folder can contain local-only files that another PC already removed from the cloud. Setup cannot infer whether they are new files or old copies. Under **Choose initial files**, select the ones you want to upload. Unselected local-only files remain on this PC. Clear cloud-only items you want to leave in OneDrive. Neither choice deletes content.

Choices follow item identities and survive restarts and rescans. Other new files sync normally; new files inside a folder you left out remain inside that excluded folder. Later, select an item in **Files** to include it, or use the command line. To exclude an item that has already synced, use **Files > Exclude**.

```powershell
oneup setup-files
oneup setup-files --path /old/needed.txt --sync on
oneup setup-files --side cloud
oneup setup-files --side cloud --path /cloud-only.txt --sync off
```

Lists contain at most 100 items. Pass the returned `next` value with `--after` to read the next page.

During migration, **Check your OneDrive folder > Review** lists every reported online-only item missing from OneDrive, in pages. Select items you no longer need to acknowledge them. Refreshing this report clears acknowledgements so they cannot apply to a changed list. Acknowledgement removes nothing and does not bypass the automatic unlinking safety check: OneUpDrive still requires cloud copies before it can release another provider's folder. Finish unlinking through the OneDrive app. If that app cannot finish, keep the folder until the missing items can be reconciled.

```powershell
oneup setup-files --side missing --refresh
oneup setup-files --side missing --path /old-online-only.txt --acknowledge
```

## Command line

```powershell
oneup shared-settings review
oneup shared-settings import --code <review-code>
oneup shared-settings publish --code <review-code> --automatic
oneup shared-settings disable
```

The review prints both documents and a code for those exact settings. If either version changes, review again. For command-line setup, `oneup sync <folder> --remote / --dry-run --review-initial` prepares the initial choices; review them with `oneup setup-files` before syncing with `--review-initial`.
