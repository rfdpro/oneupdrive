//! OneUpDrive updating itself, with Velopack (MIT): releases on
//! GitHub, installed per user, downloaded in the background and applied
//! when nothing is syncing.
//!
//! * The release feed is the GitHub repository this build was made for
//!   (`ONEUP_UPDATE_REPO` at build time), or `[updates] repo`. `stable` sees
//!   releases, `preview` pre-releases too. Not installed with the installer
//!   (a developer build), or no repository: no updates.
//! * **An update is installed only if its programs are signed by the same
//!   publisher as the running one** (Authenticode, `oneup_sys::
//!   authenticode`), checked on the downloaded package before anything is
//!   replaced. The feed comes over HTTPS from GitHub; the signature check
//!   means a compromised feed alone can't install anything. An unsigned
//!   installation is never updated by itself.
//! * The tray (`oneup-tray`, the installed program) asks for it; applying
//!   stops the agents first and starts them again afterwards
//!   (`oneup-tray --start`).

use std::io::Read as _;
use std::path::{Path, PathBuf};

use anyhow::{Context, Result, bail};
use velopack::sources::GithubSource;
use velopack::{UpdateCheck, UpdateInfo, UpdateManager};

/// The repository releases come from, set when the release is built.
pub const BUILT_FOR: Option<&str> = option_env!("ONEUP_UPDATE_REPO");

/// An update downloaded and checked, ready to apply.
pub struct Ready {
    pub version: String,
    info: UpdateInfo,
    um: UpdateManager,
}

impl std::fmt::Debug for Ready {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Ready").field("version", &self.version).finish_non_exhaustive()
    }
}

/// The event the settings window raises to make `data_dir`'s tray look
/// for an update now (`oneup_sys::signal`).
pub fn check_signal(data_dir: &Path) -> String {
    crate::tray::instance_name(data_dir).replace("-tray-", "-update-check-")
}

/// The update manager for this installation; `None` when this isn't an
/// installed copy or there's nowhere to update from.
pub fn manager(data_dir: &Path) -> Result<Option<UpdateManager>> {
    let cfg = crate::config::load_updates(data_dir)?;
    let repo = if cfg.repo.is_empty() { BUILT_FOR.unwrap_or_default() } else { cfg.repo.as_str() };
    if repo.is_empty() || url_parts(repo).is_none() {
        return Ok(None);
    }
    let source = GithubSource::new(repo, None, cfg.channel == "preview");
    // Fails when this copy wasn't installed (a developer build).
    Ok(UpdateManager::new(source, None, None).ok())
}

/// `https://github.com/owner/repo` only.
fn url_parts(repo: &str) -> Option<(&str, &str)> {
    let rest = repo.strip_prefix("https://github.com/")?.trim_end_matches('/');
    let (owner, name) = rest.split_once('/')?;
    (!owner.is_empty() && !name.is_empty() && !name.contains('/')).then_some((owner, name))
}

/// Looks for a newer release; downloads and checks it if there is one.
/// Blocking (network): run it off the UI thread.
pub fn check(um: UpdateManager) -> Result<Option<Ready>> {
    let info = match um.check_for_updates().context("checking for updates")? {
        UpdateCheck::UpdateAvailable(info) => *info,
        _ => return Ok(None),
    };
    um.download_updates(&info, None).context("downloading the update")?;
    let package = packages_dir()?.join(&info.TargetFullRelease.FileName);
    if let Err(e) = verify(&package) {
        // Ours, just downloaded: never left to be applied by anyone.
        let _ = std::fs::remove_file(&package);
        return Err(e);
    }
    Ok(Some(Ready { version: info.TargetFullRelease.Version.clone(), info, um }))
}

/// Where Velopack keeps downloaded packages: `<install>\packages`, next to
/// `<install>\current` where the programs are.
fn packages_dir() -> Result<PathBuf> {
    let exe = std::env::current_exe()?;
    let root = exe.parent().and_then(Path::parent).context("not an installed copy")?;
    Ok(root.join("packages"))
}

/// The package's `oneupd.exe` must be signed by the running program's
/// publisher.
fn verify(package: &Path) -> Result<()> {
    let mine = oneup_sys::authenticode::signer(&std::env::current_exe()?)?
        .context("this copy of OneUpDrive isn't signed, so it doesn't install updates by itself")?;
    let file = std::fs::File::open(package).with_context(|| format!("opening {}", package.display()))?;
    let mut zip = zip::ZipArchive::new(file).context("reading the update package")?;
    let name = (0..zip.len())
        .filter_map(|i| zip.name_for_index(i).map(str::to_string))
        .find(|n| n.replace('\\', "/").to_lowercase().ends_with("/oneupd.exe"))
        .context("the update package has no oneupd.exe")?;
    let tmp = package.with_extension("verify.exe");
    {
        let mut entry = zip.by_name(&name)?;
        let mut out = std::fs::File::create(&tmp)?;
        // At most 256 MiB (it's a few MiB).
        std::io::copy(&mut (&mut entry).take(256 << 20), &mut out)?;
    }
    let theirs = oneup_sys::authenticode::signer(&tmp);
    let _ = std::fs::remove_file(&tmp);
    match theirs? {
        Some(s) if s == mine => Ok(()),
        Some(s) => bail!("the update is signed by {s}, not by {mine}; it wasn't installed"),
        None => bail!("the update isn't signed; it wasn't installed"),
    }
}

/// Applies `ready` and restarts the tray with `args` (this process exits).
/// The caller stops the agents first.
pub fn apply(ready: Ready, args: Vec<String>) -> Result<()> {
    ready.um.apply_updates_and_restart_with_args(&ready.info, args).context("installing the update")?;
    Ok(())
}

#[cfg(test)]
mod tests {
    #[test]
    fn only_github_repository_urls_are_used() {
        assert_eq!(super::url_parts("https://github.com/someone/oneupdrive"), Some(("someone", "oneupdrive")));
        assert_eq!(super::url_parts("https://github.com/someone/oneupdrive/"), Some(("someone", "oneupdrive")));
        assert_eq!(super::url_parts("http://github.com/someone/oneupdrive"), None);
        assert_eq!(super::url_parts("https://example.com/someone/oneupdrive"), None);
        assert_eq!(super::url_parts("https://github.com/someone"), None);
    }
}
