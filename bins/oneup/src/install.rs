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
    // A development identity (`oneup dev-identity --register`, on a PC in
    // Developer Mode) is registered again for the new programs: the signed
    // package can't replace it (a different publisher), and installing over
    // an existing copy deletes the install folder, where its manifest was.
    // Windows then dropped the folder's Files On-Demand registration with
    // it, and the agent stopped.
    let dev =
        oneup_sys::identity::installed_publisher().ok().flatten().is_some_and(|p| p == crate::identity::PUBLISHER_DEV);
    if dev {
        let programs = std::env::current_exe().ok().and_then(|e| e.parent().map(Path::to_path_buf));
        if programs.is_some_and(|p| crate::identity::dev_is_current(&p)) {
            return; // the same package already: nothing to update
        }
        if let Err(e) = crate::identity::register_dev() {
            tracing::warn!(error = %format!("{e:#}"), "the development identity wasn't registered again");
        }
        return;
    }
    if let Err(e) = crate::identity::add_installed() {
        tracing::warn!(error = %format!("{e:#}"), "no identity package");
    }
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

/// Releases `dir`'s Files On-Demand folder, if it has one.
fn release(dir: &Path) {
    let Ok(store) = crate::setup::open_store(dir, 1024) else { return };
    let get = |k| store.read(|o| o.meta_get(k)).ok().flatten();
    if let (Some(local), Some(token)) = (get(crate::sync::KEY_LOCAL), get(crate::sync::KEY_TOKEN))
        && crate::ondemand::is_on(Path::new(&local), &token)
    {
        let _ = crate::syncroot::unregister(Path::new(&local), &token);
    }
}
