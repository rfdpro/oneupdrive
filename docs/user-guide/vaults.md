# Encrypted vaults

A **vault** is a folder whose files are encrypted on your PC before they reach OneDrive, so that nobody but you, Microsoft included, can read them. OneUpDrive uses the same vault format as [Cryptomator](https://cryptomator.org/), a free and widely used app, so your vaults also open in Cryptomator on your phone, a Mac or Linux.

**Vaults are experimental.** OneUpDrive's encryption code has had a review by its own developers ([the findings](../architecture/vault-crypto-review.md)), but not yet one by an outside security expert. Until then, don't make a vault the only copy of anything you can't afford to lose. For now, creating or adding a vault is meant for test accounts, as the `oneup vault` help says.

## What a vault protects, and what it doesn't
* **Protected:** what's in your files, and their names and folder names.
* **Not hidden:** how many files there are, roughly how big each is, and when they changed.
* Someone with access to your OneDrive could still delete vault files, put back an older copy of one, swap the contents of two files, or cut a file short at a multiple of 32 KB, without it showing. They couldn't read what's inside, or change it any other way without it showing as damaged.
* While a vault is unlocked on your PC, its files are readable there like any others. Turn on BitLocker (or Device Encryption) to protect your disk.
* **Your password is the key.** Forget it, and only the vault's **recovery key** (below) can open it again: not OneUpDrive, not Cryptomator, not Microsoft. Write the password down somewhere safe, and keep the recovery key too.

## Creating a vault
In the settings window, select **Files → Add → Create an encrypted vault**. Choose **Vault location (on OneDrive)** with **Browse**, or enter a path such as `/Private`. Choose an empty folder, or open its parent and enter a **New folder name**. Browsing creates nothing.

Enter a **Password** twice. **Always keep vault files on this PC (Cloud backup remains encrypted)** downloads and keeps every file decrypted here. Leave it off to download files when apps open them.

The **Decrypted folder on this PC** is selected automatically: a new folder in your Windows user folder, named after the vault and outside synced folders. Review the path, or use its **Browse** button to choose a new or empty folder elsewhere. If a safe suggestion isn't available, choose the folder yourself. Nothing is created on this PC until you submit the form.

Select **Create**. OneUpDrive makes the vault in OneDrive and immediately selects its new tab on the **Files** page. It starts syncing and shows its **recovery key**. From the command line: `oneup vault new /Private C:\Users\me\Private`.

The new vault is a standard Cryptomator vault: Cryptomator's apps on your phone or another computer open it with the same password.

## The recovery key, and changing the password
The settings window hides typed passwords and recovery keys. New passwords need at least eight characters and matching confirmation. **Cancel** or Esc closes password entry without sending a request. The password isn't saved. Unlocked vault keys stay only in the running sync agent's memory, unless you set up Windows Hello.

The **recovery key** is a long line of letters and numbers starting with `OUD1-`, shown when you create a vault and any time after with **Recovery key…** (or `oneup vault recovery-key Private`). With it you can set a new password if you forget yours. Print it or write it down, and keep it somewhere safe: **anyone who has it can read the vault**. It's OneUpDrive's own, so Cryptomator's apps don't read it, but the vault itself stays a normal Cryptomator vault.

**Change password…** (on the vault's tab) sets a new password with the current one, or with the recovery key (`oneup vault change-password Private`, add `--recovery-key` to use it). Nothing is re-encrypted; Cryptomator's apps and other PCs use the new password from then on. OneDrive keeps the vault's previous key file in its version history.

## Unlocking with Windows Hello
On the vault's tab, **Unlock with Windows Hello** lets your face, fingerprint or Windows PIN unlock it instead of the password (`oneup vault hello Private on`). It needs Windows Hello set up (Windows Settings → Accounts → Sign-in options), and it asks you to confirm when you turn it on and each time you unlock. The vault's keys are kept on this PC encrypted with a key that only Windows Hello, for you on this PC, can produce. Turning it off removes them. After a restart, the vault stays locked until you approve Windows Hello or enter its password.

## Syncing a vault with a folder on this PC
OneUpDrive shows a vault in OneDrive as an ordinary folder on your PC, with its real file names. Programs open and save files there as usual: OneUpDrive decrypts what they open and encrypts what they save. You don't need Cryptomator on this PC.

In the settings window, select **Files → Add → Add an existing vault**. Browse to the **Vault location (on OneDrive)** containing `vault.cryptomator`, then enter its **Password**. Review the automatically selected **Decrypted folder on this PC**, or choose a new or empty folder with **Browse**. Select **Always keep vault files on this PC (Cloud backup remains encrypted)** to keep every file decrypted here. Select **Add**. OneUpDrive immediately selects the vault's new tab on the **Files** page, with **Unlock…**, **Lock** and **Stop syncing…**.

If syncing can't start after creating or adding a vault, its tab stays in the list as **locked**. Use **Unlock…** to try again. A new vault's recovery key is still shown.

From the command line:

```powershell
oneup vault add /Vault C:\Users\me\Vault      # asks for the vault's password, then starts syncing
oneup vault list                              # your vaults, and whether each is unlocked
oneup vault lock Vault                        # stop syncing; the keys leave memory
oneup vault unlock Vault                      # start again (asks for the password)
oneup vault remove Vault --yes                # stop syncing it for good
```

* `/Vault` is a path from the top of OneDrive, not a path on this PC or a web address. It names the vault's folder in OneDrive: the one with `vault.cryptomator` in it. Make the vault with Cryptomator (or `oneup vault create`, below) first.
* The folder on this PC holds the **decrypted files** you can open and edit. It must be **new or empty**, so existing files aren't added to the vault, and outside every other synced folder. OneUpDrive refuses one that overlaps: a decrypted file must never be uploaded as it is.
* **Online-only by default.** Files show with their names and sizes but take no space until a program opens one; only then is it downloaded and decrypted. With `--full-files`, every file is kept decrypted on this PC instead.
* **Unlocking:** a vault syncs only while it's unlocked. Stopping OneUpDrive or restarting Windows locks it. Use **Unlock…** and enter the password again, or approve Windows Hello if you set it up. Vaults never unlock automatically. Keys an older version saved for automatic unlocking aren't used, and are deleted the next time you unlock with the password.
* **Locking clears the vault from this PC.** OneUpDrive first syncs what changed, then overwrites every decrypted file with zeros and makes it online-only again. Names stay visible, but nothing opens until you unlock. A file that's open in a program, or whose change couldn't be synced, is kept and listed (`oneup vault lock Vault` lists them; so does the activity log). A vault set to keep all files on this PC isn't cleared.
* **Locking by itself:** on the vault's tab, **Lock after (minutes unused)** locks it once no file in it was opened or changed for that long, and **Lock with Windows** locks it when Windows locks or goes to sleep (`oneup vault settings Vault --lock-after 30 --lock-with-windows on`). **Sync every (minutes)** sets how often it syncs (`--interval`).
* **After a crash or power cut** while a vault was unlocked, OneUpDrive clears it as soon as you sign in again. Changes that hadn't synced yet stay decrypted on this PC until you unlock, which syncs them. With **Keep unsynced changes after a crash** off (`--keep-unsent off`), they're discarded instead: those files go back to their last synced version, and files never synced are deleted.
* **What locking can't reach:** copies programs make elsewhere (thumbnails, autosave files, the page file) and old data an SSD keeps internally. Turn on BitLocker for those.
* If the vault is inside a folder you already sync, that folder leaves the vault's encrypted files alone from then on (`oneup explain` says so). Copies of them already on this PC stay as they are.
* **Removing** a vault stops syncing it. The vault in OneDrive stays, and so do the files in the folder that were on this PC. Online-only files leave the folder; they are still in the vault.
* Renaming and moving work for any name, including very long ones (Cryptomator keeps those in a folder of their own; OneUpDrive rebuilds that the way Cryptomator would).
* In File Explorer, files in a vault's folder show a lock in the Status column.

## The preview tools
For checking compatibility with Cryptomator, on vaults in a local folder:
```powershell
oneup vault create D:\Test\MyVault                 # a new vault (asks for a password twice)
oneup vault mkdir  D:\Test\MyVault "Taxes"
oneup vault put    D:\Test\MyVault C:\file.pdf "Taxes/file.pdf"
oneup vault ls     D:\Test\MyVault -r              # what's inside
oneup vault get    D:\Test\MyVault "Taxes/file.pdf" C:\copy.pdf
oneup vault check  D:\Test\MyVault                 # read and verify everything
```

They only ever add new files and folders; they never change or delete anything. They won't read online-only files: make the vault folder available on this PC first. Vaults made by Cryptomator's own vault server (Cryptomator Hub), and very old vaults, aren't supported; OneUpDrive says so if you try.
