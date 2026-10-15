//! Opening a data folder: its database, its sign-in, its exclusion rules,
//! and the lock that lets only one process sync it at a time. Shared by the
//! `oneup` command-line tool and the `oneupd` background agent.

use std::fs::File;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::Duration;

use anyhow::{Context, Result, bail};
use oneup_graph::auth::{READ_ONLY_SCOPE, READ_WRITE_ALL_SCOPE, READ_WRITE_SCOPE};
use oneup_graph::{Auth, DpapiFileStore};
use oneup_rules::ExcludeSet;
use oneup_store::Store;

use crate::test_account;

/// Meta key of the exclusion preset chosen on the first `mirror`/`scan`/`sync`.
pub const KEY_PRESET: &str = "exclude_preset";

/// In a data folder: its sign-in also allows reading (and, for a test
/// account, changing) what others share with the account. Written by
/// `oneup login --test-account --shared-folders` once that sign-in worked.
pub const SHARED_MARKER: &str = "shared-folders.txt";

/// Opens (creating if needed) the data folder's state database. `cache_kib`
/// bounds SQLite's page cache (the memory profile).
pub fn open_store(dir: &Path, cache_kib: u32) -> Result<Store> {
    std::fs::create_dir_all(dir).with_context(|| format!("creating {}", dir.display()))?;
    Ok(Store::open(&dir.join("state.db"), cache_kib)?)
}

/// While on, two-way sync is pre-release: only a test account's data folder
/// may sync, and every other sign-in is read-only (AGENTS.md rule 6). Off
/// in releases, which also allows pairing the whole drive
/// (`sync::WHOLE_DRIVE_ALLOWED`). Developer commands that change OneDrive
/// directly stay test-only either way.
pub const PRE_RELEASE: bool = false;

/// The app registration a release signs people in through, set when it's
/// built (`packaging/build-release.ps1 -ClientId`; a public client ID, not
/// a secret, kept out of the repository).
pub const BUILT_CLIENT_ID: Option<&str> = option_env!("ONEUP_CLIENT_ID");

/// The data folder's sign-in. While [`PRE_RELEASE`], only a test-account
/// folder (see [`test_account`]) gets write access; every other folder is
/// read-only.
/// `client_id`: from `--client-id` / `ONEUP_CLIENT_ID`, if given, else
/// [`BUILT_CLIENT_ID`].
pub fn auth(dir: &Path, client_id: Option<&str>, http: &reqwest::Client) -> Result<Arc<Auth>> {
    // A vault pair signs in as its main data folder does.
    let dir = &crate::vaultpair::main_dir(dir);
    let test = test_account::load(dir)?;
    let client_id = match (client_id, &test) {
        (Some(c), Some(t)) if c != t.client_id => {
            bail!("this test-account folder was signed in with a different client ID")
        }
        (Some(c), _) => c.to_owned(),
        (None, Some(t)) => t.client_id.clone(),
        (None, None) => BUILT_CLIENT_ID
            .context("no client ID: set ONEUP_CLIENT_ID or pass --client-id (see CONTRIBUTING.md)")?
            .to_owned(),
    };
    let scope = match (&test, dir.join(SHARED_MARKER).is_file()) {
        (Some(_), true) => READ_WRITE_ALL_SCOPE,
        (Some(_), false) => READ_WRITE_SCOPE,
        (None, _) if PRE_RELEASE => READ_ONLY_SCOPE,
        (None, true) => READ_WRITE_ALL_SCOPE,
        (None, false) => READ_WRITE_SCOPE,
    };
    let store = DpapiFileStore::new(dir.join("token.dpapi"));
    Ok(Arc::new(Auth::new(http.clone(), client_id, scope, Box::new(store))))
}

pub fn http() -> Result<reqwest::Client> {
    tls();
    Ok(reqwest::Client::builder()
        .user_agent(concat!("OneUpDrive/", env!("CARGO_PKG_VERSION")))
        .connect_timeout(Duration::from_secs(20))
        .timeout(Duration::from_secs(120))
        .build()?)
}

/// Chooses rustls' cryptography for the whole process. Two backends are
/// built in (reqwest's aws-lc-rs, and ring from the updater's HTTP client),
/// and with two, rustls refuses to guess and every HTTPS connection fails.
/// Harmless if called again.
pub fn tls() {
    let _ = rustls::crypto::aws_lc_rs::default_provider().install_default();
}

/// The rules of a stored preset name (`none` / `dev`).
pub fn preset_rules(name: &str) -> Option<ExcludeSet> {
    match name {
        "none" => Some(ExcludeSet::none()),
        "dev" => Some(ExcludeSet::dev()),
        _ => None,
    }
}

/// In a pre-release build ([`PRE_RELEASE`]), refuses anything but a test
/// account's data folder (AGENTS.md rule 6), so a real user's OneDrive
/// stays read-only.
pub fn require_test_account(dir: &Path) -> Result<()> {
    if PRE_RELEASE && test_account::load(&crate::vaultpair::main_dir(dir))?.is_none() {
        bail!(
            "sync is pre-release and only runs with a test account's data folder \
             (see `oneup login --test-account`); your own OneDrive stays read-only"
        );
    }
    Ok(())
}

/// Held while a process syncs or changes a data folder's synced folder:
/// `oneup sync`, `oneup versions`/`restore` and `oneupd` would otherwise
/// race on the same database and folder. Released when dropped (and by
/// Windows when the process ends, however it ends, so it can't go stale).
pub struct DirLock {
    _file: File,
    path: PathBuf,
}

impl DirLock {
    /// Takes the lock, or says who has it. The lock is the file opened with
    /// no sharing allowed: a second open anywhere fails until it is closed.
    pub fn acquire(dir: &Path) -> Result<Self> {
        use std::os::windows::fs::OpenOptionsExt;
        std::fs::create_dir_all(dir).with_context(|| format!("creating {}", dir.display()))?;
        let path = dir.join("sync.lock");
        match std::fs::OpenOptions::new().read(true).write(true).create(true).truncate(false).share_mode(0).open(&path)
        {
            Ok(file) => Ok(Self { _file: file, path }),
            // ERROR_SHARING_VIOLATION
            Err(e) if e.raw_os_error() == Some(32) => bail!(
                "another OneUpDrive process (oneupd, or oneup sync/versions/restore) is using this data folder; \
                 stop it first"
            ),
            Err(e) => Err(e).with_context(|| format!("opening {}", path.display())),
        }
    }

    pub fn path(&self) -> &Path {
        &self.path
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn only_one_holder_at_a_time() {
        let dir = std::env::temp_dir().join(format!("oneup-lock-{}", std::process::id()));
        let first = DirLock::acquire(&dir).unwrap();
        assert!(DirLock::acquire(&dir).is_err());
        drop(first);
        assert!(DirLock::acquire(&dir).is_ok());
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn presets_map_to_rules() {
        assert!(preset_rules("dev").is_some() && preset_rules("none").is_some());
        assert!(preset_rules("everything").is_none());
    }
}

#[cfg(test)]
mod tls_tests {
    /// Both rustls backends are compiled in (reqwest's, and the updater's
    /// HTTP client's): without a chosen one, building any TLS client panics
    /// (found when the updater was added).
    #[test]
    fn https_clients_can_be_built() {
        super::tls();
        let _ = rustls::ClientConfig::builder();
        assert!(super::http().is_ok());
    }
}
