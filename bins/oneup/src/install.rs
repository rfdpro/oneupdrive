//! What happens when OneUpDrive is installed, updated or uninstalled
//! (Velopack calls the tray program for these, `oneup-tray.exe`, before it
//! shows anything; each must finish within seconds and show nothing).
//!
//! * **Uninstalling** stops the agents of every account's data folder and
//!   its vault and extra pairs, releases their Files On-Demand folders,
//!   and removes "start at sign-in" and what OneUpDrive registered for
//!   Office and Explorer. Releasing a folder makes Windows remove the files
//!   that were only online (they're in OneDrive) and turn every other
//!   placeholder into an ordinary file with its content (measured,
//!   `oneup-cfapi` test `unregistering_another_provider_removes_only_what_is_in_the_cloud`):
//!   nothing on this PC that isn't in OneDrive is lost. Then the identity
//!   package goes. The data folders (settings, database, keys) stay, for a
//!   later reinstall.
//! * **Installing and updating** add the identity package shipped with
//!   the version (`crate::identity`).

use std::path::{Path, PathBuf};
use std::time::{Duration, Instant};

/// Handles Velopack's install-time calls, if this is one (it then exits);
/// otherwise returns at once. The first thing the tray program does.
pub fn hooks() {
    velopack::VelopackApp::build()
        .on_after_install_fast_callback(|_| identity())
        .on_after_update_fast_callback(|_| identity())
        .on_before_uninstall_fast_callback(|_| uninstalling())
        .run();
}

/// Adds the identity package shipped with this version (Explorer
/// integration); without it, OneUpDrive works as before, without the
/// navigation-pane entry.
fn identity() {
    let dev = oneup_sys::identity::installed()
        .is_ok_and(|all| all.iter().any(|p| p.publisher == crate::identity::PUBLISHER_DEV));
    let step = identity_step(dev, crate::identity::is_release_build());
    if step == IdentityStep::KeepDevelopment {
        // A development build over a development identity: registered again
        // for the new programs. Installing over an existing copy deletes the
        // install folder, where its manifest was. Windows then dropped the
        // folder's Files On-Demand registration with it, and the agent stopped.
        let programs = std::env::current_exe().ok().and_then(|e| e.parent().map(Path::to_path_buf));
        if programs.is_some_and(|p| crate::identity::dev_is_current(&p)) {
            return; // the same package already: nothing to update
        }
        if let Err(e) = crate::identity::register_dev() {
            tracing::warn!(error = %format!("{e:#}"), "the development identity wasn't registered again");
        }
        return;
    }
    match crate::identity::add_installed() {
        Ok(true) if step == IdentityStep::AddSignedAndMove => run_move_identity(),
        Ok(_) => {}
        Err(e) => tracing::warn!(error = %format!("{e:#}"), "no identity package"),
    }
}

/// What installing does about the identity package.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum IdentityStep {
    /// A development build over a development identity.
    KeepDevelopment,
    AddSigned,
    /// A signed build over a development identity: the signed package goes
    /// in beside it (another publisher is another family), then
    /// [`move_identity`] moves the folders over.
    AddSignedAndMove,
}

fn identity_step(development_installed: bool, release_build: bool) -> IdentityStep {
    match (development_installed, release_build) {
        (true, true) => IdentityStep::AddSignedAndMove,
        (true, false) => IdentityStep::KeepDevelopment,
        (false, _) => IdentityStep::AddSigned,
    }
}

/// [`move_identity`] in `oneup.exe`, which gets the signed package's
/// identity: registering a folder needs it, and this process runs without.
fn run_move_identity() {
    use std::os::windows::process::CommandExt as _;
    const CREATE_NO_WINDOW: u32 = 0x0800_0000;
    let Ok(exe) = crate::launch::sibling("oneup.exe") else { return };
    let ran = std::process::Command::new(exe)
        .arg("move-identity")
        .stdin(std::process::Stdio::null())
        .creation_flags(CREATE_NO_WINDOW)
        .output();
    match ran {
        Ok(out) if out.status.success() => {}
        Ok(out) => tracing::warn!(
            error = %String::from_utf8_lossy(&out.stderr).trim(),
            "the development identity package was left in place"
        ),
        Err(e) => tracing::warn!(error = %e, "the development identity package was left in place"),
    }
}

/// What [`move_identity`] did.
#[derive(Debug, PartialEq, Eq)]
pub enum MovedIdentity {
    /// No development package here.
    NothingToDo,
    /// Every folder is the signed package's, and the development one is gone.
    Moved { folders: usize },
    /// The development package stays: `blocked` folders are still its own.
    /// Nothing is lost; the next install or a new run tries again.
    Kept { blocked: usize },
}

/// Moves this user's folders from the development identity package to the
/// signed one this program runs with, then removes the development package.
///
/// In this order, because the order decides what happens to the files:
/// removing a package first makes Windows drop its sync roots and delete
/// their online-only files, while registering the same ID from the new
/// package first moves the record to it, with every placeholder and the
/// Cloud Files identity intact, and the old package can then go (measured on
/// temporary folders, also with the program folder deleted and replaced as
/// an install does). The package goes only when the shell's records show
/// that it owns nothing.
pub async fn move_identity() -> anyhow::Result<MovedIdentity> {
    use anyhow::{Context as _, bail};
    let own = oneup_sys::identity::package_name().context("this program has no package identity")?;
    let installed = oneup_sys::identity::installed()?;
    let own = installed.iter().find(|p| p.full_name == own).context("this program's package isn't installed")?;
    if own.publisher == crate::identity::PUBLISHER_DEV {
        bail!("this program runs with the development package");
    }
    let Some(dev) = installed.iter().find(|p| p.publisher == crate::identity::PUBLISHER_DEV) else {
        return Ok(MovedIdentity::NothingToDo);
    };
    // Registering through the shell while an agent is connected leaves a
    // registration Windows drops when it disconnects
    // (docs/architecture/files-on-demand.md).
    let dirs = data_dirs();
    stop_agents(&dirs, Duration::from_secs(10)).await;
    for d in &dirs {
        if oneup_ipc::connect(d).await.is_ok() {
            bail!("an agent is still running");
        }
    }
    let sid = oneup_sys::pipe::current_user_sid()?;
    let mut folders = 0;
    for root in oneup_cfapi::shell::roots_of_user(&sid) {
        if root.owner.as_deref() != Some(dev.family.as_str()) {
            continue;
        }
        match move_folder(&root, &own.family, &sid) {
            Ok(()) => folders += 1,
            Err(e) => tracing::warn!(error = %format!("{e:#}"), "a folder wasn't moved to the signed package"),
        }
    }
    // Read back, not assumed from the above.
    let owners: Vec<_> = oneup_cfapi::shell::roots_of_user(&sid).into_iter().map(|r| r.owner).collect();
    if !crate::identity::may_remove(&dev.family, &owners) {
        let blocked = owners.iter().filter(|o| o.as_deref().is_none_or(|o| o == dev.family)).count();
        return Ok(MovedIdentity::Kept { blocked });
    }
    crate::identity::stop_shell_surrogate();
    oneup_sys::identity::remove_one(&dev.full_name)?;
    if let Some(programs) = std::env::current_exe().ok().and_then(|e| e.parent().map(Path::to_path_buf)) {
        // The development package's unpacked files (our own), beside the
        // install folder: nothing reads them now.
        let stage = crate::identity::stage_dir(&programs);
        if stage.extension().is_some_and(|x| x == "identity") {
            let _ = std::fs::remove_dir_all(stage);
        }
    }
    Ok(MovedIdentity::Moved { folders })
}

/// Registers one folder again, under this program's package, and checks
/// that it took: the record is the new package's and the Cloud Files
/// registration is unchanged.
fn move_folder(root: &oneup_cfapi::shell::OurRoot, family: &str, sid: &str) -> anyhow::Result<()> {
    use anyhow::{Context as _, ensure};
    let (provider, token) =
        oneup_cfapi::provider::registration(&root.root).context("the folder has no Cloud Files registration")?;
    ensure!(provider == oneup_cfapi::PROVIDER_NAME, "the folder is registered by another program");
    let token = String::from_utf8(token).context("the folder's registration isn't a pair's")?;
    ensure!(
        oneup_cfapi::shell::root_id(sid, &token) == root.id,
        "the folder's registration doesn't match its Explorer entry"
    );
    let shown = oneup_cfapi::shell::registered(&root.id).map_or_else(|| "OneUpDrive".to_string(), |r| r.display);
    crate::syncroot::register(&root.root, &token, &shown)?;
    let owner = oneup_cfapi::shell::roots_of_user(sid).into_iter().find(|r| r.id == root.id).and_then(|r| r.owner);
    ensure!(owner.as_deref() == Some(family), "the folder's Explorer entry stayed with the old package");
    ensure!(
        oneup_cfapi::provider::registration(&root.root).is_some_and(|(p, t)| p == provider && t == token.as_bytes()),
        "the folder's Cloud Files registration changed"
    );
    Ok(())
}

/// Every account's data folder (the default one and `OneUpDrive-<name>`)
/// and their vault and extra pairs.
fn data_dirs() -> Vec<PathBuf> {
    let mut dirs = Vec::new();
    for a in crate::accounts::list() {
        dirs.extend(crate::launch::children(&a.dir));
        dirs.push(a.dir);
    }
    dirs
}

fn uninstalling() {
    // A data folder inside the install folder would go with it: never
    // release anything then (the package id keeps them apart, see
    // packaging/build-release.ps1).
    let install = std::env::current_exe().ok().and_then(|e| e.parent().and_then(Path::parent).map(Path::to_path_buf));
    let dirs: Vec<PathBuf> = data_dirs()
        .into_iter()
        .filter(|d| !install.as_deref().is_some_and(|i| crate::test_account::same_dir(i, d) || d.starts_with(i)))
        .collect();
    if let Ok(rt) = tokio::runtime::Builder::new_current_thread().enable_all().build() {
        rt.block_on(stop_agents(&dirs, Duration::from_secs(20)));
    }
    for d in &dirs {
        release(d);
        let _ = crate::launch::set_autostart(crate::launch::RUN_KEY, d, None);
    }
    // What Explorer's flyout and (if turned on) Office read.
    let _ = crate::officeint::turn_off(Path::new(""));
    let _ = crate::officeopen::unregister();
    let _ = oneup_sys::user_registry::remove_key(crate::shellstatus::KEY);
    // Last: releasing the folders above may need it (shell registrations).
    if crate::identity::remove().is_ok()
        && let Some(programs) = std::env::current_exe().ok().and_then(|e| e.parent().map(Path::to_path_buf))
    {
        // A development identity's unpacked package, beside the install
        // folder (our own files; the package that read them is gone).
        let stage = crate::identity::stage_dir(&programs);
        if stage.extension().is_some_and(|x| x == "identity") {
            let _ = std::fs::remove_dir_all(stage);
        }
    }
}

/// Asks every agent to stop, and waits (up to `limit`) until they have.
pub async fn stop_agents(dirs: &[PathBuf], limit: Duration) {
    for d in dirs {
        if let Ok(mut c) = oneup_ipc::connect(d).await {
            let _ = c.call(&oneup_ipc::Call::Stop).await;
        }
    }
    let until = Instant::now() + limit;
    for d in dirs {
        while Instant::now() < until && oneup_ipc::connect(d).await.is_ok() {
            tokio::time::sleep(Duration::from_millis(300)).await;
        }
    }
}

/// Releases `dir`'s Files On-Demand folder, if it has one. Its database is
/// only read (`Store::peek_meta`): an agent that didn't stop in time may
/// be another build, and opening the database as a store would upgrade it
/// under that agent.
fn release(dir: &Path) {
    let db = dir.join("state.db");
    let get = |k| oneup_store::Store::peek_meta(&db, k).ok().flatten();
    if let (Some(local), Some(token)) = (get(crate::sync::KEY_LOCAL), get(crate::sync::KEY_TOKEN))
        && crate::ondemand::is_on(Path::new(&local), &token)
    {
        let _ = crate::syncroot::unregister(Path::new(&local), &token);
    }
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used)]
    use super::*;

    #[test]
    fn a_signed_build_goes_beside_a_development_identity_and_moves_the_folders() {
        assert_eq!(identity_step(true, true), IdentityStep::AddSignedAndMove);
        assert_eq!(identity_step(true, false), IdentityStep::KeepDevelopment);
        assert_eq!(identity_step(false, true), IdentityStep::AddSigned);
        assert_eq!(identity_step(false, false), IdentityStep::AddSigned);
    }

    /// Uninstalling reads each folder's database and leaves it as it was:
    /// an older one isn't upgraded under an agent that didn't stop, and
    /// none is made where there's none.
    #[test]
    fn releasing_a_folder_never_upgrades_or_makes_its_database() {
        use crate::datafolders::tests::{older_folder, version};
        let root = std::env::temp_dir().join(format!("oneup-install-peek-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&root);
        let (older, empty) = (root.join("older"), root.join("empty"));
        std::fs::create_dir_all(&empty).unwrap();
        // A synced folder but no token: nothing to release.
        older_folder(&older, &root.join("Sync"));
        release(&older);
        release(&empty);
        assert_eq!(version(&older), 23, "not upgraded");
        assert!(!empty.join("state.db").exists(), "none made");
        std::fs::remove_dir_all(&root).unwrap();
    }
}
