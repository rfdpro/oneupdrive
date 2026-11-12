//! Read-only folder suggestions for the settings window's vault forms.

use std::path::{Path, PathBuf};

use anyhow::{Context, Result, bail};
use oneup_local::coexist::{is_within, plain};

/// Suggest a new folder without creating it or adopting an existing one.
/// Read other agents' databases without opening/upgrading their stores.
#[cfg(windows)]
pub fn default_folder(main: &Path, remote: &str) -> Result<PathBuf> {
    let profile = std::env::var_os("USERPROFILE").context("Windows user folder unavailable")?;
    let mut dirs = crate::datafolders::all_strict()?;
    dirs.push(main.to_path_buf());
    dirs.extend(crate::vaultpair::list_strict(main)?.into_iter().map(|v| v.dir));
    dirs.extend(crate::pairs::list_strict(main)?.into_iter().map(|p| p.dir));
    let mut excluded = Vec::new();
    for dir in dirs {
        let db = dir.join("state.db");
        if db.try_exists()?
            && let Some(local) = oneup_store::Store::peek_meta(&db, crate::sync::KEY_LOCAL)?
        {
            excluded.push(PathBuf::from(local));
        }
        excluded.push(dir);
    }
    let sid = oneup_sys::pipe::current_user_sid()?;
    for root in oneup_cfapi::shell::roots_of_user(&sid)? {
        excluded.push(root.root.context("a cloud sync folder's location is unavailable")?);
    }
    excluded.push(Path::new(&profile).join("AppData"));
    excluded
        .extend(["LOCALAPPDATA", "APPDATA", "PROGRAMDATA"].into_iter().filter_map(std::env::var_os).map(PathBuf::from));
    suggest(Path::new(&profile), remote, &excluded, |folder| {
        oneup_local::coexist::other_engine(folder).is_some() || oneup_cfapi::sync_root_containing(folder).is_some()
    })
}

fn overlaps(a: &Path, b: &Path) -> bool {
    is_within(a, b) || is_within(b, a)
}

/// Resolve links in the existing prefix of a location that may be missing.
/// A broken link is an error, never an apparently unoccupied location.
fn resolved_location(path: &Path) -> Result<PathBuf> {
    if !path.is_absolute() {
        bail!("a synced folder's location is not absolute");
    }
    let mut parent = path;
    let mut missing = Vec::new();
    loop {
        match std::fs::canonicalize(parent) {
            Ok(mut resolved) => {
                for part in missing.iter().rev() {
                    resolved.push(part);
                }
                return Ok(resolved);
            }
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => {
                match std::fs::symlink_metadata(parent) {
                    Ok(_) => bail!("a synced folder's location cannot be resolved"),
                    Err(e) if e.kind() == std::io::ErrorKind::NotFound => {}
                    Err(e) => return Err(e.into()),
                }
                missing.push(parent.file_name().context("a synced folder's location cannot be resolved")?);
                parent = parent.parent().context("a synced folder's location cannot be resolved")?;
            }
            Err(e) => return Err(e.into()),
        }
    }
}

/// Only direct, nonexistent children of the resolved profile are considered.
/// Checking the existing parent also catches raw Cloud Files registrations
/// that cannot be queried through a child which does not exist yet.
fn suggest(
    profile: &Path,
    remote: &str,
    excluded: &[PathBuf],
    cloud_managed: impl Fn(&Path) -> bool,
) -> Result<PathBuf> {
    let resolved = std::fs::canonicalize(profile).context("reading the Windows user folder")?;
    if !resolved.is_dir() || cloud_managed(profile) || cloud_managed(&resolved) {
        bail!("Choose a new folder outside synced folders with Browse");
    }
    let last = remote.split(['/', '\\']).rfind(|s| !s.trim().is_empty()).unwrap_or("Private");
    let name: String = last
        .chars()
        .map(|c| if c.is_alphanumeric() || c == '-' || c == '_' || c == ' ' { c } else { '-' })
        .take(60)
        .collect();
    let name = name.trim_matches([' ', '-']);
    let name = if name.is_empty() { "Private" } else { name };
    let mut forbidden = excluded.to_vec();
    for path in excluded {
        forbidden.push(resolved_location(path).context("checking synced folder locations")?);
    }
    if forbidden.iter().any(|p| is_within(profile, p) || is_within(&resolved, p)) {
        bail!("Choose a new folder outside synced folders with Browse");
    }
    for n in 1..=1_000 {
        let leaf = if n == 1 { format!("{name} Vault") } else { format!("{name} Vault ({n})") };
        let raw = profile.join(&leaf);
        let candidate = resolved.join(&leaf);
        if forbidden.iter().any(|p| overlaps(&raw, p) || overlaps(&candidate, p)) {
            continue;
        }
        // symlink_metadata also refuses an existing dangling link. Neither
        // an empty folder nor a provider placeholder is ours to adopt.
        match std::fs::symlink_metadata(&candidate) {
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => {}
            Err(e) => return Err(e).context("checking the suggested vault folder"),
            Ok(_) => continue,
        }
        if !cloud_managed(&raw) && !cloud_managed(&candidate) {
            return Ok(plain(&candidate));
        }
    }
    bail!("Choose a new folder outside synced folders with Browse")
}

#[cfg(test)]
mod tests {
    use super::*;

    fn profile() -> tempfile::TempDir {
        tempfile::tempdir().unwrap()
    }

    #[test]
    fn suggestion_uses_the_vault_name_and_never_adopts_or_creates_a_folder() {
        let p = profile();
        let first = p.path().join("Tax records Vault");
        std::fs::create_dir(&first).unwrap();
        let second = p.path().join("Tax records Vault (2)");
        std::fs::write(&second, b"keep").unwrap();
        let next = suggest(p.path(), "/Private/Tax records", &[], |_| false).unwrap();
        assert_eq!(next, plain(&p.path().join("Tax records Vault (3)")));
        assert!(!next.exists());
        assert_eq!(std::fs::read(&second).unwrap(), b"keep");
        assert_eq!(std::fs::read_dir(p.path()).unwrap().count(), 2);
    }

    #[test]
    fn suggestions_stay_outside_sync_vault_provider_and_data_roots_in_both_directions() {
        let p = profile();
        let excluded = vec![
            p.path().join("Private Vault"),
            p.path().join("Private Vault (2)").join("synced-child"),
            p.path().join("Private Vault (3)"),
            p.path().join("Private Vault (4)"),
        ];
        let next = suggest(p.path(), "/Private", &excluded, |path| path.ends_with("Private Vault (5)")).unwrap();
        assert_eq!(next, plain(&p.path().join("Private Vault (6)")));
        assert_eq!(std::fs::read_dir(p.path()).unwrap().count(), 0);
        assert!(suggest(p.path(), "/Private", &[p.path().to_path_buf()], |_| false).is_err());
        assert!(suggest(p.path(), "/Private", &[], |path| path == p.path()).is_err());
    }

    #[test]
    fn remote_names_cannot_escape_the_profile_or_form_reserved_windows_names() {
        let p = profile();
        for remote in ["", "/..", "/CON", "/a:b*?", "/a/\\..\\"] {
            let next = suggest(p.path(), remote, &[], |_| false).unwrap();
            assert_eq!(next.parent(), Some(plain(p.path()).as_path()));
            assert!(next.file_name().unwrap().to_string_lossy().ends_with(" Vault"));
            assert!(!next.exists());
        }
    }

    #[test]
    fn nonexistent_locations_resolve_their_existing_parent() {
        let p = profile();
        let missing = p.path().join("missing").join("child");
        assert_eq!(
            resolved_location(&missing).unwrap(),
            std::fs::canonicalize(p.path()).unwrap().join("missing").join("child")
        );
        assert!(resolved_location(Path::new("relative")).is_err());
        assert!(!missing.exists());
    }

    #[cfg(windows)]
    #[test]
    fn a_missing_sync_root_below_a_junction_still_excludes_its_resolved_location() {
        use std::os::windows::process::CommandExt;
        let temp = profile();
        let profile = temp.path().join("profile");
        std::fs::create_dir(&profile).unwrap();
        let alias = temp.path().join("alias");
        let made = std::process::Command::new("cmd.exe")
            .args(["/d", "/c", "mklink", "/J"])
            .arg(&alias)
            .arg(&profile)
            .creation_flags(0x0800_0000)
            .output()
            .unwrap();
        assert!(made.status.success(), "{}", String::from_utf8_lossy(&made.stderr));
        let excluded = [alias.join("Private Vault").join("nested-sync")];
        let next = suggest(&profile, "/Private", &excluded, |_| false).unwrap();
        assert_eq!(next, plain(&profile.join("Private Vault (2)")));
        assert!(!next.exists());
        std::fs::remove_dir(&alias).unwrap();
    }
}
