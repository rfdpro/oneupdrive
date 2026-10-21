//! Which data folders exist on this PC, so a path in a synced folder can be
//! traced back to the data folder that syncs it (Explorer's context-menu
//! commands only know the file). Each agent records its data folder when
//! it starts (`HKCU\Software\OneUpDrive\DataFolders`); the default data
//! folder and vault pairs are found without that.

use std::path::{Path, PathBuf};

const KEY: &str = r"Software\OneUpDrive\DataFolders";

fn name_of(dir: &Path) -> String {
    let mut h: u64 = 0xcbf2_9ce4_8422_2325;
    for b in dir.to_string_lossy().to_lowercase().trim_end_matches('\\').bytes() {
        h = (h ^ u64::from(b)).wrapping_mul(0x0100_0000_01b3);
    }
    format!("{h:016x}")
}

/// Records `dir` as a data folder in use.
pub fn record(dir: &Path) {
    let dir = std::fs::canonicalize(dir).unwrap_or_else(|_| dir.to_path_buf());
    let shown = dir.to_string_lossy();
    let shown = shown.strip_prefix(r"\\?\").unwrap_or(&shown);
    if let Err(e) = oneup_sys::user_registry::set(KEY, &name_of(Path::new(shown)), shown) {
        tracing::debug!(error = %e, "data folder not recorded");
    }
}

/// The data folders agents recorded (main ones and pairs alike).
pub fn recorded() -> Vec<PathBuf> {
    oneup_sys::user_registry::values(KEY)
        .map(|v| v.into_iter().map(|(_, d)| PathBuf::from(d)).collect())
        .unwrap_or_default()
}

/// The data folders known on this PC: recorded ones, the default one, and
/// their vault and extra pairs.
pub fn all() -> Vec<PathBuf> {
    let mut out: Vec<PathBuf> = Vec::new();
    if let Ok(d) = crate::test_account::default_dir() {
        out.push(d);
    }
    if let Ok(values) = oneup_sys::user_registry::values(KEY) {
        out.extend(values.into_iter().map(|(_, v)| PathBuf::from(v)));
    }
    let mains = out.clone();
    for m in mains {
        out.extend(crate::launch::children(&m));
    }
    out.retain(|d| d.join("state.db").exists());
    out.dedup_by(|a, b| crate::test_account::same_dir(a, b));
    out
}

/// Whether data folder `d` has a database. Only one that isn't there (the
/// folder gone too) is `false`; one that can't be looked at is an error,
/// not a folder missed.
fn has_database(d: &Path) -> std::io::Result<bool> {
    d.join("state.db").try_exists()
}

/// Like [`all`], but strict: a registry read or a pair/vault listing that
/// fails is an error, not an empty list, so a caller that must not miss a
/// data folder (the hooks' safety checks) can refuse instead of guessing.
pub fn all_strict() -> std::io::Result<Vec<PathBuf>> {
    let mut out: Vec<PathBuf> = Vec::new();
    if let Ok(d) = crate::test_account::default_dir() {
        out.push(d);
    }
    out.extend(oneup_sys::user_registry::values(KEY)?.into_iter().map(|(_, v)| PathBuf::from(v)));
    let to_io = |e: std::io::Error| e;
    for m in out.clone() {
        out.extend(crate::vaultpair::list_strict(&m).map_err(to_io)?.into_iter().map(|v| v.dir));
        out.extend(crate::pairs::list_strict(&m).map_err(to_io)?.into_iter().map(|p| p.dir));
    }
    let mut kept = Vec::with_capacity(out.len());
    for d in out {
        if has_database(&d)? {
            kept.push(d);
        }
    }
    kept.dedup_by(|a, b| crate::test_account::same_dir(a, b));
    Ok(kept)
}

/// The data folder whose synced folder holds `path`, and `path` relative
/// to it (`/`-separated, empty for the folder itself).
pub fn for_path(path: &Path) -> Option<(PathBuf, String)> {
    for_path_in(path, all())
}

/// [`for_path`] among the data folders `dirs`.
pub fn for_path_in(path: &Path, dirs: Vec<PathBuf>) -> Option<(PathBuf, String)> {
    let path = std::fs::canonicalize(path).ok()?;
    let mut best: Option<(PathBuf, String, usize)> = None;
    for d in dirs {
        // Read only: opening the database as a store would upgrade it
        // under that folder's agent, maybe another build.
        let Some(local) = oneup_store::Store::peek_meta(&d.join("state.db"), crate::sync::KEY_LOCAL).ok().flatten()
        else {
            continue;
        };
        let local = std::fs::canonicalize(&local).unwrap_or_else(|_| PathBuf::from(&local));
        if let Ok(rel) = path.strip_prefix(&local) {
            // The innermost synced folder wins (a vault folder inside another).
            let depth = local.components().count();
            if best.as_ref().is_none_or(|b| depth > b.2) {
                let rel = rel.to_string_lossy().replace('\\', "/");
                best = Some((d, rel, depth));
            }
        }
    }
    best.map(|(d, rel, _)| (d, rel))
}

#[cfg(test)]
pub(crate) mod tests {
    #![allow(clippy::unwrap_used)]
    use super::*;

    /// A data folder at `dir` syncing `local`, whose database is at schema
    /// version 23, older than this build's: opening it as a store would
    /// upgrade it.
    pub(crate) fn older_folder(dir: &Path, local: &Path) {
        std::fs::create_dir_all(dir).unwrap();
        let db = dir.join("state.db");
        {
            let mut s = oneup_store::Store::open(&db, 1024).unwrap();
            s.write(|o| o.meta_set(crate::sync::KEY_LOCAL, &local.to_string_lossy())).unwrap();
        }
        rusqlite::Connection::open(&db).unwrap().execute_batch("PRAGMA user_version = 23;").unwrap();
    }

    pub(crate) fn version(dir: &Path) -> i64 {
        rusqlite::Connection::open(dir.join("state.db"))
            .unwrap()
            .query_row("PRAGMA user_version", [], |r| r.get(0))
            .unwrap()
    }

    /// A database that isn't there leaves its folder out of the strict
    /// listing; one that can't be looked at fails it.
    #[test]
    fn only_a_missing_database_leaves_a_folder_out() {
        let root = std::env::temp_dir().join(format!("oneup-datafolders-has-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&root);
        std::fs::create_dir_all(root.join("with")).unwrap();
        std::fs::write(root.join("with").join("state.db"), b"").unwrap();
        std::fs::write(root.join("file"), b"").unwrap();
        assert!(has_database(&root.join("with")).unwrap());
        assert!(!has_database(&root.join("gone")).unwrap(), "the folder gone");
        assert!(!has_database(&root.join("file")).unwrap(), "a file where the folder was");
        // A path Windows can't look up at all.
        assert!(has_database(&root.join("bad\u{0}name")).is_err(), "can't be looked at");
        std::fs::remove_dir_all(&root).unwrap();
    }

    /// Finding which data folder a path is in reads the folders' databases
    /// and leaves them as they were: not upgraded, none made.
    #[test]
    fn finding_a_paths_folder_never_upgrades_or_makes_a_database() {
        let root = std::env::temp_dir().join(format!("oneup-datafolders-peek-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&root);
        let (synced, older, empty) = (root.join("Sync"), root.join("older"), root.join("empty"));
        std::fs::create_dir_all(synced.join("Docs")).unwrap();
        std::fs::create_dir_all(&empty).unwrap();
        older_folder(&older, &synced);
        let (dir, rel) = for_path_in(&synced.join("Docs"), vec![empty.clone(), older.clone()]).unwrap();
        assert_eq!((dir, rel.as_str()), (older.clone(), "Docs"));
        assert_eq!(version(&older), 23, "not upgraded");
        assert!(!empty.join("state.db").exists(), "none made");
        std::fs::remove_dir_all(&root).unwrap();
    }
}
