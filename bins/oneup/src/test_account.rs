//! The dedicated **test account**: the only place write access is allowed
//! before sync is released (AGENTS.md rule 6: live tests run against
//! dedicated test accounts only, never a real user's OneDrive).
//!
//! A test account lives in its *own* data folder (`--data-dir`), never the
//! default one. That folder holds its own token, state database and a marker
//! file, `test-account.txt`, which records:
//! * the app's client ID, so commands can run there without
//!   `ONEUP_CLIENT_ID` (it's a public client ID, kept locally, never
//!   committed);
//! * the drive ID the account was signed in to, so later write commands can
//!   check, against Graph, that the token still reaches that drive and not
//!   another one.
//!
//! Only a folder with this marker gets a `Files.ReadWrite` token. Every other
//! folder, including the default one used with a personal account, stays
//! `Files.Read`, so writing is impossible there by construction.

use std::path::{Path, PathBuf};

use anyhow::{Context, Result, bail};

const MARKER: &str = "test-account.txt";

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TestAccount {
    pub client_id: String,
    pub drive_id: String,
}

/// The default data folder (`%LOCALAPPDATA%\OneUpDrive`), which a test
/// account may never use.
pub fn default_dir() -> Result<PathBuf> {
    let base = std::env::var_os("LOCALAPPDATA").context("LOCALAPPDATA is not set; pass --data-dir")?;
    Ok(PathBuf::from(base).join("OneUpDrive"))
}

/// Whether two paths name the same folder. Canonicalised when they exist
/// (resolving `..`, case and short names); otherwise compared
/// case-insensitively, as Windows does.
pub fn same_dir(a: &Path, b: &Path) -> bool {
    match (std::fs::canonicalize(a), std::fs::canonicalize(b)) {
        (Ok(a), Ok(b)) => a == b,
        _ => oneup_sync::same_name(
            a.to_string_lossy().trim_end_matches(['\\', '/']),
            b.to_string_lossy().trim_end_matches(['\\', '/']),
        ),
    }
}

pub fn load(dir: &Path) -> Result<Option<TestAccount>> {
    let text = match std::fs::read_to_string(dir.join(MARKER)) {
        Ok(t) => t,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(None),
        Err(e) => return Err(e).context("reading the test-account marker"),
    };
    let field = |key: &str| {
        text.lines()
            .filter_map(|l| l.split_once('='))
            .find(|(k, _)| k.trim() == key)
            .map(|(_, v)| v.trim().to_owned())
            .filter(|v| !v.is_empty())
    };
    match (field("client_id"), field("drive_id")) {
        (Some(client_id), Some(drive_id)) => Ok(Some(TestAccount { client_id, drive_id })),
        _ => bail!("{} is incomplete; sign in again with `oneup login --test-account`", dir.join(MARKER).display()),
    }
}

/// Written atomically (temp file + rename); it's OneUpDrive's own file.
pub fn save(dir: &Path, t: &TestAccount) -> Result<()> {
    std::fs::create_dir_all(dir)?;
    let text = format!(
        "# OneUpDrive test account: write access is allowed for this data folder only.\n\
         client_id={}\ndrive_id={}\n",
        t.client_id, t.drive_id
    );
    let tmp = dir.join(format!("{MARKER}.tmp"));
    std::fs::write(&tmp, text)?;
    std::fs::rename(&tmp, dir.join(MARKER))?;
    Ok(())
}

pub fn remove(dir: &Path) -> Result<()> {
    match std::fs::remove_file(dir.join(MARKER)) {
        Err(e) if e.kind() != std::io::ErrorKind::NotFound => Err(e.into()),
        _ => Ok(()),
    }
}

/// The drive mirrored in the default data folder, if any: in OneDrive
/// Personal, item IDs start with the drive ID (`<drive>!<n>`). Used to refuse
/// a test-account sign-in that is really the personal account. Only reads
/// the local database, read-only and never upgraded (the default folder's
/// agent may be another build); never contacts the personal account.
pub fn personal_drive(default: &Path) -> Option<String> {
    let db = default.join("state.db");
    if !db.exists() {
        return None;
    }
    let root = oneup_store::Store::peek_remote_root_id(&db).ok()??;
    root.split_once('!').map(|(drive, _)| drive.to_lowercase())
}

/// Whether drive ID `drive` is the same drive as `personal` (from
/// [`personal_drive`]). Graph returns the drive ID in varying case and,
/// for some accounts, zero-padded to 16 digits.
pub fn is_same_drive(drive: &str, personal: &str) -> bool {
    let norm = |s: &str| s.to_lowercase().trim_start_matches('0').to_owned();
    norm(drive) == norm(personal)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn folders_that_are_not_there_compare_as_names_do() {
        let p = |s: &str| std::path::PathBuf::from(s);
        assert!(same_dir(&p(r"Z:\oneup-none\ΟΔΟΣ"), &p(r"z:\ONEUP-NONE\οδος\")));
        assert!(!same_dir(&p("Z:\\oneup-none\\\u{212A}it"), &p(r"Z:\oneup-none\kit")), "the Kelvin sign isn't k");
    }

    #[test]
    fn marker_round_trips_and_rejects_incomplete_files() {
        let dir = std::env::temp_dir().join(format!("oneup-test-account-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        assert_eq!(load(&dir).ok().flatten(), None);
        let t = TestAccount { client_id: "c".into(), drive_id: "d".into() };
        save(&dir, &t).ok();
        assert_eq!(load(&dir).ok().flatten(), Some(t));
        std::fs::write(dir.join(MARKER), "client_id=c\n").ok();
        assert!(load(&dir).is_err());
        remove(&dir).ok();
        assert_eq!(load(&dir).ok().flatten(), None);
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn drive_ids_compare_loosely() {
        assert!(is_same_drive("00AB12", "ab12"));
        assert!(!is_same_drive("ab13", "ab12"));
    }

    #[test]
    fn default_folder_is_recognised_however_spelled() {
        assert!(same_dir(Path::new(r"C:\X\OneUpDrive"), Path::new(r"c:\x\oneupdrive\")));
        assert!(!same_dir(Path::new(r"C:\X\OneUpDrive"), Path::new(r"C:\X\OneUpDrive-test")));
    }
}
