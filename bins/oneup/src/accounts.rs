//! Several Microsoft accounts on one PC: one data folder each (as
//! `--data-dir` always allowed), listed and added from the settings window.
//! Each account has its own sign-in, agent, notification-area icon and
//! settings window, as the OneDrive app has one icon per account.
//!
//! Finding them reads only file names and small marker files: never an
//! account's database (opening one could upgrade it) and nothing online.

use std::path::{Path, PathBuf};

use anyhow::{Context, Result, bail};

/// In a data folder: the name the person gave the account.
pub const NAME_FILE: &str = "account-name.txt";
/// Data folders made from the window: `%LOCALAPPDATA%\OneUpDrive-<name>`.
const PREFIX: &str = "OneUpDrive-";

/// One account's data folder.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Account {
    pub dir: PathBuf,
    pub name: String,
    pub signed_in: bool,
    pub test_account: bool,
    /// The default data folder (`%LOCALAPPDATA%\OneUpDrive`).
    pub default: bool,
}

fn local_app_data() -> Result<PathBuf> {
    Ok(PathBuf::from(std::env::var_os("LOCALAPPDATA").context("LOCALAPPDATA is not set")?))
}

/// The account in `dir`, if `dir` is a main data folder.
pub fn read(dir: &Path) -> Option<Account> {
    if !dir.is_dir() || crate::vaultpair::is_child_dir(dir) {
        return None;
    }
    let default = crate::test_account::default_dir().is_ok_and(|d| crate::test_account::same_dir(&d, dir));
    let named =
        std::fs::read_to_string(dir.join(NAME_FILE)).ok().map(|s| s.trim().to_string()).filter(|s| !s.is_empty());
    let leaf = dir.file_name().map(|n| n.to_string_lossy().into_owned()).unwrap_or_default();
    let name = named.unwrap_or_else(|| match leaf.strip_prefix(PREFIX) {
        _ if default => "Personal".into(),
        Some(rest) if !rest.is_empty() => rest.to_string(),
        _ => leaf.clone(),
    });
    Some(Account {
        dir: dir.to_path_buf(),
        name,
        signed_in: dir.join("token.dpapi").is_file(),
        test_account: dir.join("test-account.txt").is_file(),
        default,
    })
}

/// Every account data folder on this PC: the default one, `OneUpDrive-*`
/// folders next to it, and data folders agents recorded.
pub fn list() -> Vec<Account> {
    let mut dirs: Vec<PathBuf> = Vec::new();
    if let Ok(d) = crate::test_account::default_dir() {
        dirs.push(d);
    }
    if let Ok(base) = local_app_data() {
        for e in std::fs::read_dir(base).into_iter().flatten().flatten() {
            let leaf = e.file_name().to_string_lossy().into_owned();
            // Only folders that hold an account (spike leftovers have no sign-in).
            if leaf.starts_with(PREFIX)
                && (e.path().join("token.dpapi").is_file() || e.path().join(NAME_FILE).is_file())
            {
                dirs.push(e.path());
            }
        }
    }
    #[cfg(windows)]
    dirs.extend(crate::datafolders::recorded());
    let mut out: Vec<Account> = Vec::new();
    for d in dirs {
        if out.iter().any(|a| crate::test_account::same_dir(&a.dir, &d)) {
            continue;
        }
        if let Some(a) = read(&d) {
            out.push(a);
        }
    }
    out
}

/// A data folder for a new account called `name` (letters, digits, spaces,
/// `-` and `_`), with the name recorded. Signing in is then the person's,
/// in the settings window it opens.
pub fn add(name: &str) -> Result<PathBuf> {
    let name = name.trim();
    if name.is_empty() || name.len() > 40 {
        bail!("give the account a short name, like Work or Family");
    }
    if !name.chars().all(|c| c.is_alphanumeric() || matches!(c, ' ' | '-' | '_')) {
        bail!("use letters, digits, spaces, - and _ in the name");
    }
    if list().iter().any(|a| a.name.eq_ignore_ascii_case(name)) {
        bail!("there's already an account called {name}");
    }
    let dir = local_app_data()?.join(format!("{PREFIX}{}", name.replace(' ', "-")));
    if dir.exists() {
        bail!("{} already exists; pick another name", dir.display());
    }
    std::fs::create_dir_all(&dir)?;
    std::fs::write(dir.join(NAME_FILE), format!("{name}\n"))?;
    Ok(dir)
}

/// In a data folder: who signed in, as `<drive id>\n<display name>\n`
/// (written by `oneup login`, removed by `oneup logout`), so the window can
/// say which account it is without asking OneDrive.
pub const SIGNED_IN_FILE: &str = "signed-in-as.txt";

/// The account signed in to a data folder, as [`SIGNED_IN_FILE`] records it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SignedIn {
    pub drive: String,
    pub name: String,
}

pub fn signed_in_as(dir: &Path) -> Option<SignedIn> {
    let text = std::fs::read_to_string(dir.join(SIGNED_IN_FILE)).ok()?;
    let mut lines = text.lines();
    let drive = lines.next()?.trim().to_string();
    let name = lines.next().unwrap_or_default().trim().to_string();
    (!drive.is_empty()).then_some(SignedIn { drive, name })
}

/// The OneDrive a data folder's cloud listing is of: the drive part of its
/// root item's ID (`<drive>!<item>`), lower case.
pub fn listed_drive(store: &oneup_store::Store) -> Option<String> {
    let root = store.read(|o| o.remote_root()).ok()??;
    root.id.split_once('!').map(|(drive, _)| drive.to_lowercase())
}

/// What a new sign-in did to the data folder.
#[derive(Debug, PartialEq, Eq)]
pub enum Switched {
    /// Same account as before (or nothing listed yet).
    No,
    /// Another account: the old one's cloud listing and the OneDrive app
    /// choices noted against it were dropped (nothing was paired).
    Cleared,
}

/// Records the account `drive` (named `name`) signed in to `dir`. A data
/// folder holds one account's listing: when another account signs in, the
/// old listing goes (it would otherwise be continued with the wrong
/// account), or, if the folder already syncs a pair with the old account,
/// the sign-in is refused (the caller signs back out).
pub fn record_sign_in(dir: &Path, drive: &str, name: &str) -> Result<Switched> {
    let mut switched = Switched::No;
    if dir.join("state.db").exists() {
        let mut store = crate::setup::open_store(dir, 1024)?;
        if let Some(listed) = listed_drive(&store)
            && !crate::test_account::is_same_drive(drive, &listed)
        {
            if store.read(|o| o.meta_get(crate::sync::KEY_LOCAL))?.is_some() {
                bail!(
                    "this data folder syncs another Microsoft account's OneDrive; sign in with that account, \
                     or stop syncing first (`oneup unpair`)"
                );
            }
            store.write(|o| o.clear_remote())?;
            switched = Switched::Cleared;
        }
    }
    if switched == Switched::Cleared {
        match std::fs::remove_file(dir.join(crate::carryover::FILE)) {
            Err(e) if e.kind() != std::io::ErrorKind::NotFound => return Err(e.into()),
            _ => {}
        }
    }
    std::fs::write(dir.join(SIGNED_IN_FILE), format!("{}\n{}\n", drive.to_lowercase(), name.trim()))?;
    Ok(switched)
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used)]
    use super::*;

    #[test]
    fn an_account_is_named_by_its_file_or_its_folder() {
        let base = std::env::temp_dir().join(format!("oneup-accounts-{}", std::process::id()));
        let a = base.join("OneUpDrive-Family");
        std::fs::create_dir_all(&a).unwrap();
        assert_eq!(read(&a).unwrap().name, "Family");
        assert!(!read(&a).unwrap().signed_in);
        std::fs::write(a.join(NAME_FILE), "Family photos\n").unwrap();
        std::fs::write(a.join("token.dpapi"), b"x").unwrap();
        let acc = read(&a).unwrap();
        assert_eq!((acc.name.as_str(), acc.signed_in, acc.default), ("Family photos", true, false));
        let child = a.join("pairs").join("p");
        std::fs::create_dir_all(&child).unwrap();
        std::fs::write(child.join(crate::pairs::MARKER), "/x").unwrap();
        assert!(read(&child).is_none(), "a pair's data folder isn't an account");
        let _ = std::fs::remove_dir_all(&base);
    }

    #[test]
    fn signing_in_with_another_account_drops_the_old_listing_unless_paired() {
        use oneup_store::{Kind, RemoteRow};
        let dir = std::env::temp_dir().join(format!("oneup-signin-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        let root = |id: &str| RemoteRow {
            id: id.into(),
            parent_id: None,
            name: "root".into(),
            kind: Kind::Root,
            size: 0,
            etag: None,
            qxh: None,
            mtime_ms: None,
        };
        let mut store = crate::setup::open_store(&dir, 1024).unwrap();
        store.write(|o| o.upsert_item(&root("AAA!101"), false)).unwrap();
        drop(store);
        std::fs::write(dir.join(crate::carryover::FILE), "{}").unwrap();

        // The same account: kept.
        assert_eq!(record_sign_in(&dir, "0aaa", "Sam").unwrap(), Switched::No);
        assert!(dir.join(crate::carryover::FILE).exists());
        assert_eq!(signed_in_as(&dir), Some(SignedIn { drive: "0aaa".into(), name: "Sam".into() }));
        // Another account: the listing and what was noted against it go.
        assert_eq!(record_sign_in(&dir, "bbb", "Alex").unwrap(), Switched::Cleared);
        assert!(!dir.join(crate::carryover::FILE).exists());
        let mut store = crate::setup::open_store(&dir, 1024).unwrap();
        assert_eq!(listed_drive(&store), None);
        // Paired with the first account: another one is refused.
        store.write(|o| o.upsert_item(&root("AAA!101"), false)).unwrap();
        store.write(|o| o.meta_set(crate::sync::KEY_LOCAL, r"C:\x")).unwrap();
        drop(store);
        assert!(record_sign_in(&dir, "bbb", "Alex").is_err());
        assert_eq!(signed_in_as(&dir).unwrap().name, "Alex", "unchanged by the refusal");
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn new_account_names_are_checked() {
        assert!(add("").is_err());
        assert!(add("a/b").is_err());
        assert!(add(&"x".repeat(41)).is_err());
    }
}
