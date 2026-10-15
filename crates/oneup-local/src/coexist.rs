//! Is this folder (or part of it) managed by another sync engine?
//!
//! Two sync engines on one folder fight, and can destroy data. A read-only
//! scan of such a folder is fine (it's the first step of migrating away from
//! the official client), but anything that would *manage* it must refuse.
//! The scan records the answer in `local_root.sync_root_provider` so later
//! phases can enforce that.
//!
//! Signals, combined because each one alone can miss:
//! 1. **Cloud Files registration** (`CfGetSyncRootInfoByPath`): covers every
//!    Files On-Demand provider (OneDrive, Dropbox, iCloud, ...), but only for
//!    paths *inside* a sync root.
//! 2. **The official client's folders**, which also reveal a OneDrive folder
//!    *below* the scanned folder (e.g. scanning the whole user profile): the
//!    folder of each account set up in it (its settings' `UserFolder`), and
//!    `%OneDrive%`, `%OneDriveConsumer%`, `%OneDriveCommercial%`. Those
//!    variables outlive unlinking (seen after a stalled unlinking
//!    was finished), so one counts only while its folder is still a sync root
//!    or an account's folder: a leftover variable alone isn't a manager.

use std::path::{Component, Path, PathBuf};

/// Environment variables the official OneDrive client sets per linked account.
const ONEDRIVE_ENV: [&str; 3] = ["OneDriveConsumer", "OneDriveCommercial", "OneDrive"];
/// The official client's settings for each account it has set up.
const ACCOUNTS_KEY: &str = r"Software\Microsoft\OneDrive\Accounts";

/// How a scanned folder relates to another sync engine's folder.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum OtherEngine {
    /// The scanned folder is (inside) that engine's folder.
    Inside { provider: String },
    /// That engine's folder is inside the scanned folder.
    Contains { provider: String, folder: PathBuf },
}

impl OtherEngine {
    /// A short description to store (`local_root.sync_root_provider`).
    pub fn label(&self) -> String {
        match self {
            OtherEngine::Inside { provider } => provider.clone(),
            OtherEngine::Contains { provider, .. } => format!("{provider} (in a subfolder)"),
        }
    }
}

/// Checks `path` (ideally already normalized) against all signals.
pub fn other_engine(path: &Path) -> Option<OtherEngine> {
    if let Some(info) = oneup_cfapi::other_sync_root(path) {
        let provider = if info.provider_name.is_empty() { "a cloud sync app".into() } else { info.provider_name };
        return Some(OtherEngine::Inside { provider });
    }
    check_folders(path, &onedrive_folders())
}

/// The folders the official client manages now (see the module docs).
fn onedrive_folders() -> Vec<PathBuf> {
    let env: Vec<PathBuf> = ONEDRIVE_ENV.iter().filter_map(std::env::var_os).map(PathBuf::from).collect();
    managed_folders(&account_folders(), &env, |f| oneup_cfapi::other_sync_root(f).is_some())
}

/// The accounts' folders, and each variable's folder that is one of them or
/// still a sync root (`is_root`).
fn managed_folders(accounts: &[PathBuf], env: &[PathBuf], is_root: impl Fn(&Path) -> bool) -> Vec<PathBuf> {
    let mut out = accounts.to_vec();
    for f in env.iter().filter(|f| !f.as_os_str().is_empty()) {
        if accounts.iter().any(|a| is_within(f, a) && is_within(a, f)) || is_root(f) {
            out.push(f.clone());
        }
    }
    out
}

/// Each account's folder in the official client's settings.
#[cfg(windows)]
fn account_folders() -> Vec<PathBuf> {
    std::iter::once("Personal".to_string())
        .chain((1..=9).map(|n| format!("Business{n}")))
        .filter_map(|a| oneup_sys::user_registry::get(&format!(r"{ACCOUNTS_KEY}\{a}"), "UserFolder").ok().flatten())
        .map(|f| PathBuf::from(f.trim()))
        .filter(|f| !f.as_os_str().is_empty())
        .collect()
}

#[cfg(not(windows))]
fn account_folders() -> Vec<PathBuf> {
    Vec::new()
}

/// The environment-variable signal, separated out for testing.
fn check_folders(path: &Path, onedrive_folders: &[PathBuf]) -> Option<OtherEngine> {
    let provider = || "OneDrive".to_string();
    if onedrive_folders.iter().any(|f| !f.as_os_str().is_empty() && is_within(path, f)) {
        return Some(OtherEngine::Inside { provider: provider() });
    }
    onedrive_folders
        .iter()
        .find(|f| !f.as_os_str().is_empty() && is_within(f, path))
        .map(|f| OtherEngine::Contains { provider: provider(), folder: f.clone() })
}

/// Whether `child` is `parent` or below it, comparing path components
/// case-insensitively (as Windows does) and ignoring `\\?\` prefixes and
/// trailing separators.
pub fn is_within(child: &Path, parent: &Path) -> bool {
    let norm = |p: &Path| -> Vec<String> {
        p.components()
            .filter_map(|c| match c {
                Component::Prefix(px) => {
                    // `\\?\C:` and `C:` are the same volume.
                    let s = px.as_os_str().to_string_lossy().to_lowercase();
                    Some(s.trim_start_matches(r"\\?\").trim_start_matches(r"\\.\").to_string())
                }
                Component::RootDir | Component::CurDir => None,
                other => Some(other.as_os_str().to_string_lossy().to_lowercase()),
            })
            .collect()
    };
    let (c, p) = (norm(child), norm(parent));
    !p.is_empty() && c.len() >= p.len() && c[..p.len()] == p[..]
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn within_is_case_insensitive_and_component_wise() {
        assert!(is_within(Path::new(r"C:\Users\Me\OneDrive\Docs"), Path::new(r"c:\users\me\onedrive")));
        assert!(is_within(Path::new(r"C:\Users\Me\OneDrive"), Path::new(r"C:\Users\Me\OneDrive\")));
        assert!(is_within(Path::new(r"\\?\C:\Users\Me\OneDrive\x"), Path::new(r"C:\Users\Me\OneDrive")));
        assert!(!is_within(Path::new(r"C:\Users\Me\OneDriveBackup"), Path::new(r"C:\Users\Me\OneDrive")));
        assert!(!is_within(Path::new(r"D:\Users\Me\OneDrive"), Path::new(r"C:\Users\Me\OneDrive")));
        assert!(!is_within(Path::new(r"C:\Users"), Path::new(r"C:\Users\Me")));
    }

    #[test]
    fn a_leftover_variable_alone_is_not_a_manager() {
        let od = PathBuf::from(r"C:\Users\Me\OneDrive");
        let env = vec![od.clone(), PathBuf::new()];
        // Unlinked: no account has the folder and it's no sync root.
        assert!(managed_folders(&[], &env, |_| false).is_empty());
        // Still a sync root, or still an account's folder: it counts.
        assert_eq!(managed_folders(&[], &env, |_| true), vec![od.clone()]);
        let by_account = managed_folders(std::slice::from_ref(&od), &env, |_| false);
        assert!(!by_account.is_empty() && by_account.iter().all(|f| f == &od));
    }

    #[test]
    fn env_folders_inside_and_containing() {
        let od = vec![PathBuf::from(r"C:\Users\Me\OneDrive")];
        assert_eq!(
            check_folders(Path::new(r"C:\Users\Me\OneDrive\Projects"), &od),
            Some(OtherEngine::Inside { provider: "OneDrive".into() })
        );
        let contains = check_folders(Path::new(r"C:\Users\Me"), &od).unwrap();
        assert_eq!(contains.label(), "OneDrive (in a subfolder)");
        assert_eq!(check_folders(Path::new(r"D:\Data"), &od), None);
        assert_eq!(check_folders(Path::new(r"D:\Data"), &[PathBuf::new()]), None, "empty env values are ignored");
    }
}
