//! Registering a folder pair's folder as OneUpDrive's sync root (Files
//! On-Demand), and removing that again: with the Windows shell when this
//! program has package identity (`docs/architecture/explorer.md`: the folder
//! then shows in File Explorer's navigation pane, with its name and icon),
//! with Cloud Files alone otherwise. Both give the same sync root: Cloud
//! Files reports it as OneUpDrive's, with the pair's token as its identity
//! (measured), so everything that checks that is unchanged.
//!
//! Removing must use the route that registered it: Cloud Files' own
//! unregistration leaves the shell's navigation entry behind (measured).

use std::path::Path;

use anyhow::{Context, Result};

/// The shell's ID for the pair with `token`.
fn shell_id(token: &str) -> Result<String> {
    Ok(oneup_cfapi::shell::root_id(&oneup_sys::pipe::current_user_sid()?, token))
}

/// Registers `root` for the pair with `token`; `name` is what Explorer
/// shows (e.g. "OneUpDrive").
pub fn register(root: &Path, token: &str, name: &str) -> Result<()> {
    if oneup_sys::identity::package_name().is_some() {
        let icon = icon()?;
        let shown = root.to_string_lossy();
        let shown = Path::new(shown.strip_prefix(r"\\?\").unwrap_or(&shown));
        oneup_cfapi::shell::register(shown, &shell_id(token)?, token, name, &icon, env!("CARGO_PKG_VERSION"))
            .context("registering the folder with Windows")?;
        return Ok(());
    }
    oneup_cfapi::provider::register(root, token.as_bytes()).context("registering the folder for Files On-Demand")
}

/// How many online-only files [`repair`] looks for.
const SAMPLE: usize = 200;

/// What [`repair`] did.
#[derive(Debug, PartialEq, Eq)]
pub enum Repair {
    /// The folder is registered as ours, or wasn't ours to begin with.
    NotNeeded,
    Repaired,
    /// Not registered again, for this reason.
    Refused(&'static str),
}

/// Registers `root` (pair `token`) again when it was our Files On-Demand
/// folder and Windows no longer has it as ours: an install over an
/// existing copy deleted the development identity's folder, and Windows
/// dropped the registration with it.
///
/// Only when the files that were online-only (`store`'s last look) are
/// still here: losing the registration can make Windows delete them all,
/// and registering again would have their absence read as deletions to
/// send. A sample proves it, as Windows removes all or none. Otherwise the
/// agent stays stopped and the user decides (`LocalFs::open` explains).
pub fn repair(root: &Path, token: &str, store: &oneup_store::Store) -> Result<Repair> {
    if crate::ondemand::is_on(root, token) || !oneup_effects::LocalFs::was_on_demand(root) {
        return Ok(Repair::NotNeeded);
    }
    if let Some(other) = oneup_cfapi::other_sync_root(root) {
        tracing::warn!(provider = %other.provider_name, "the folder is now another program's");
        return Ok(Repair::Refused("another program has taken the folder over"));
    }
    // Paths from the database, looked at by attributes only (never opened).
    let sample = store.read(|o| {
        o.local_online_only_sample(SAMPLE)?
            .into_iter()
            .map(|id| o.local_path_of(id))
            .collect::<oneup_store::Result<Vec<_>>>()
    })?;
    let here = |rel: &Option<String>| {
        rel.as_deref().is_some_and(|rel| {
            let full = rel.trim_start_matches('/').split('/').fold(root.to_path_buf(), |p, x| p.join(x));
            std::fs::symlink_metadata(full).is_ok()
        })
    };
    if !sample.iter().all(here) {
        return Ok(Repair::Refused("files that were online-only are missing; Windows may have removed them"));
    }
    register(root, token, "OneUpDrive")?;
    Ok(if crate::ondemand::is_on(root, token) {
        Repair::Repaired
    } else {
        Repair::Refused("registering it again didn't take")
    })
}

/// The icon Explorer shows for our folders: this version's `oneupd.exe`.
fn icon() -> Result<String> {
    Ok(format!("{},0", crate::launch::sibling("oneupd.exe")?.display()))
}

/// Brings the shell registration of `root` (pair `token`) up to date with
/// the programs running now; `true` if it was out of date. The icon is a
/// program's path, fixed when the folder was registered: a folder paired by
/// another copy (a development build, an older install folder) would keep
/// showing that copy's icon, or none once it's gone. Registering again
/// updates the registration in place (measured on a temporary folder:
/// same navigation entry, new icon, contents and Cloud Files
/// identity unchanged). A folder registered without the shell is left alone.
pub fn refresh(root: &Path, token: &str) -> Result<bool> {
    if oneup_sys::identity::package_name().is_none() {
        return Ok(false);
    }
    let Some(now) = oneup_cfapi::shell::registered(&shell_id(token)?) else { return Ok(false) };
    // Also when the Cloud Files registration lost its identity (the pair's
    // token), as Windows has been seen to leave it: registering
    // again writes it back.
    let identity_kept = oneup_cfapi::provider::registration(root)
        .is_some_and(|(name, identity)| name == oneup_cfapi::PROVIDER_NAME && identity == token.as_bytes());
    if identity_kept && now.icon == icon()? && now.version == env!("CARGO_PKG_VERSION") {
        return Ok(false);
    }
    register(root, token, &now.display)?;
    Ok(true)
}

/// Removes the registration of `root` (pair `token`), whichever way it was
/// made.
pub fn unregister(root: &Path, token: &str) -> Result<()> {
    // Not expected to be ours any more (else the next open would halt).
    oneup_effects::LocalFs::forget_on_demand(root).context("updating the sync folder's own files")?;
    if oneup_cfapi::shell::unregister(&shell_id(token)?).is_ok() && oneup_cfapi::provider::registration(root).is_none()
    {
        return Ok(());
    }
    oneup_cfapi::provider::unregister(root).context("unregistering the folder")
}
