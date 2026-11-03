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
//!   the version (`crate::identity`) and put the install folder on the
//!   user's `PATH`, so `oneup` works in a new terminal.

use std::path::{Path, PathBuf};
use std::time::{Duration, Instant};

/// Handles Velopack's install-time calls, if this is one (it then exits);
/// otherwise returns at once. The first thing the tray program does.
pub fn hooks() {
    velopack::VelopackApp::build()
        .on_after_install_fast_callback(|_| installed())
        .on_after_update_fast_callback(|_| installed())
        .on_before_uninstall_fast_callback(|_| uninstalling())
        .run();
}

fn installed() {
    // First: it takes no time, and the identity package may use up the
    // hook's limit.
    edit_path(with_path_entry);
    // On its own thread: each window may take its time to answer, and the
    // hook's limit is for the identity package.
    drop(std::thread::Builder::new().spawn(oneup_sys::user_registry::announce_environment_change));
    identity();
}

/// The user's environment variables in the registry.
const ENVIRONMENT: &str = "Environment";
/// A variable can't be longer than this.
const MAX_PATH_VARIABLE: usize = 32_000;

/// Applies `edit` to the user's `PATH` for this program's folder. The value
/// is read and written as stored: expanding `%VARIABLES%` or changing its
/// type would damage what other programs put there, so anything unclear
/// leaves it alone.
fn edit_path(edit: fn(&str, &str) -> Option<String>) {
    use oneup_sys::user_registry as reg;
    const REG_SZ: u32 = 1;
    const REG_EXPAND_SZ: u32 = 2;
    let Some(dir) = std::env::current_exe().ok().and_then(|e| e.parent().and_then(|d| d.to_str().map(String::from)))
    else {
        return;
    };
    let tree = match reg::tree(ENVIRONMENT) {
        Ok(tree) => tree.unwrap_or_default(),
        Err(e) => {
            tracing::warn!(error = %e, "PATH left alone: it couldn't be read");
            return;
        }
    };
    let stored = tree.values.iter().find(|v| v.name.eq_ignore_ascii_case("Path"));
    let (now, kind) = match stored {
        None => (String::new(), REG_EXPAND_SZ),
        Some(v) if v.kind == REG_SZ || v.kind == REG_EXPAND_SZ => {
            let wide: Vec<u16> = v.data.as_chunks::<2>().0.iter().map(|&c| u16::from_le_bytes(c)).collect();
            let end = wide.iter().position(|&c| c == 0).unwrap_or(wide.len());
            match String::from_utf16(&wide[..end]) {
                Ok(text) => (text, v.kind),
                Err(_) => return,
            }
        }
        Some(_) => return,
    };
    let Some(new) = edit(&now, &dir) else { return };
    let done = if new.is_empty() {
        reg::remove(ENVIRONMENT, "Path").map(|_| ())
    } else {
        reg::set_as(ENVIRONMENT, "Path", &new, kind)
    };
    if let Err(e) = done {
        tracing::warn!(error = %e, "PATH wasn't changed");
    }
}

fn same_path_entry(a: &str, b: &str) -> bool {
    let plain = |s: &str| s.trim().trim_matches('"').trim_end_matches(['\\', '/']).to_lowercase();
    plain(a) == plain(b)
}

/// `path` with `dir` added last, or `None` if it's there already or doesn't
/// fit. Last, so that nothing the person already runs changes meaning.
fn with_path_entry(path: &str, dir: &str) -> Option<String> {
    if dir.contains(';') || path.split(';').any(|e| same_path_entry(e, dir)) {
        return None;
    }
    let new = match path.trim_end_matches(';') {
        "" => dir.to_string(),
        rest => format!("{rest};{dir}"),
    };
    (new.len() <= MAX_PATH_VARIABLE).then_some(new)
}

/// `path` without `dir`, or `None` if it isn't there.
fn without_path_entry(path: &str, dir: &str) -> Option<String> {
    path.split(';')
        .any(|e| same_path_entry(e, dir))
        .then(|| path.split(';').filter(|e| !same_path_entry(e, dir)).collect::<Vec<_>>().join(";"))
}

/// Adds the identity package shipped with this version (Explorer
/// integration); without it, OneUpDrive works as before, without the
/// navigation-pane entry.
fn identity() {
    let dev = oneup_sys::identity::installed()
        .map(|all| all.iter().any(|p| p.publisher == crate::identity::PUBLISHER_DEV))
        .ok();
    match identity_step(dev, crate::identity::is_release_build()) {
        IdentityStep::KeepDevelopment => {
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
        }
        // Beside a development package, if there is one: another publisher is
        // another family. That one stays until the icon's first start moves
        // the folders off it (`move_then_start`): this hook runs under a time
        // limit and with OneUpDrive's programs in an unknown state.
        IdentityStep::AddPackage => {
            if let Err(e) = crate::identity::add_installed() {
                tracing::warn!(error = %format!("{e:#}"), "no identity package");
            }
        }
        IdentityStep::Nothing => tracing::warn!("the identity package wasn't updated: packages couldn't be listed"),
    }
}

/// What installing does about the identity package.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum IdentityStep {
    /// A development build over a development identity.
    KeepDevelopment,
    AddPackage,
    /// A development build that can't tell what's installed: its package
    /// is unsigned, so adding it can't work, and a development identity may
    /// be there to keep.
    Nothing,
}

fn identity_step(development_installed: Option<bool>, release_build: bool) -> IdentityStep {
    match (development_installed, release_build) {
        (_, true) | (Some(false), false) => IdentityStep::AddPackage,
        (Some(true), false) => IdentityStep::KeepDevelopment,
        (None, false) => IdentityStep::Nothing,
    }
}

/// What [`move_identity`] did.
#[derive(Debug, PartialEq, Eq)]
pub enum MovedIdentity {
    /// No development package here.
    NothingToDo,
    /// Every folder is the signed package's, and the development one is gone.
    Moved { folders: usize },
    /// The development package stays, and with it every folder still on it:
    /// `why`, for each folder that wasn't moved.
    Kept { why: Vec<String> },
}

impl MovedIdentity {
    pub fn kept_message(why: &[String]) -> String {
        format!("Development package kept: {}", why.join("; "))
    }
}

/// How the icon tells [`move_then_start`] started it: don't try again.
pub const AFTER_MOVE: &str = "--after-move";

/// Whether this PC has folders to move: a signed build with its own package
/// next to a development one. Never on a development build, and not when
/// anything is unclear.
pub fn move_is_due() -> bool {
    crate::identity::is_release_build()
        && oneup_sys::identity::installed().is_ok_and(|all| {
            let has = |p: &str| all.iter().any(|i| i.publisher == p);
            has(crate::identity::PUBLISHER_DEV) && crate::identity::built_publisher().is_some_and(has)
        })
}

/// The tray's way of moving: the icon has handed over and is ending. Stops
/// the agents, moves, then starts again what ran and reports a failure.
///
/// Refusing is always the safe answer here ([`move_identity`]): the folders
/// stay as they were and sync as before.
pub async fn move_then_start(tray_args: &[std::ffi::OsString]) {
    // A second start from the Start menu finds the icon gone and comes here too.
    let Ok(Some(_only_one)) = oneup_sys::tray::single_instance("OneUpDrive-identity-move") else { return };
    let mut trouble = match stop_and_move(tray_args).await {
        Ok(MovedIdentity::NothingToDo | MovedIdentity::Moved { .. }) => Vec::new(),
        Ok(MovedIdentity::Kept { why }) => vec![MovedIdentity::kept_message(&why)],
        Err(e) => vec![format!("Moving folders to the signed package failed: {e:#}")],
    };
    let args: Vec<std::ffi::OsString> = tray_args.iter().cloned().chain([AFTER_MOVE.into()]).collect();
    if let Err(e) = crate::launch::start_tray_args(&args) {
        trouble.push(format!("The OneUpDrive icon didn't start: {e:#}"));
    }
    if !trouble.is_empty() {
        oneup_sys::tray::error_box("OneUpDrive", &trouble.join("\n\n"));
    }
}

/// Waits until none of `names` runs but this program, up to `limit`.
async fn wait_for_others(names: &[&str], limit: Duration) -> anyhow::Result<()> {
    let until = Instant::now() + limit;
    loop {
        match others_running(names) {
            Err(e) if Instant::now() < until => drop(e),
            done => return done,
        }
        tokio::time::sleep(Duration::from_millis(300)).await;
    }
}

async fn stop_and_move(tray_args: &[std::ffi::OsString]) -> anyhow::Result<MovedIdentity> {
    // What nothing here asks to end refuses the move, so before anything
    // stops: another account's icon or an open window, say. A few seconds
    // are for the icon that started this.
    wait_for_others(&["oneup-tray.exe", "oneup-settings.exe", "oneup-open.exe"], Duration::from_secs(10)).await?;
    let dirs = data_dirs();
    let mut running = Vec::new();
    for d in &dirs {
        if oneup_ipc::connect(d).await.is_ok() {
            running.push(d.clone());
        }
    }
    stop_agents(&dirs, Duration::from_secs(120)).await;
    let result = match wait_for_others(OUR_PROGRAMS, Duration::from_secs(30)).await {
        // Its own task: a panic must not stop the icon from coming back.
        Ok(()) => match tokio::spawn(move_identity()).await {
            Ok(result) => result,
            Err(e) => Err(anyhow::anyhow!("{e}")),
        },
        Err(e) => Err(e),
    };
    restart_other_accounts(tray_args, &running).await;
    result
}

/// Starts the agents that ran, for the accounts the icon doesn't start again.
async fn restart_other_accounts(tray_args: &[std::ffi::OsString], running: &[PathBuf]) {
    let mut args = tray_args.iter();
    let tray_dir = args.by_ref().find(|a| *a == "--data-dir").and(args.next()).map(PathBuf::from);
    let Some(tray_dir) = tray_dir.or_else(|| crate::test_account::default_dir().ok()) else { return };
    for a in crate::accounts::list() {
        if crate::test_account::same_dir(&a.dir, &tray_dir) {
            continue;
        }
        if running.contains(&a.dir) {
            drop(crate::launch::start_agent(&a.dir));
        }
        if crate::launch::children(&a.dir).iter().any(|c| running.contains(c)) {
            drop(crate::launch::start_others(&a.dir).await);
        }
    }
}

/// OneUpDrive's programs: any could register a folder, connect to one or
/// start something that does.
const OUR_PROGRAMS: &[&str] =
    &["oneupd.exe", "oneupd-start.exe", "oneup-tray.exe", "oneup-settings.exe", "oneup-open.exe", "oneup.exe"];

/// Refuses while any other of OneUpDrive's programs runs, of any copy.
pub fn only_this_program_running() -> anyhow::Result<()> {
    others_running(OUR_PROGRAMS)
}

fn others_running(names: &[&str]) -> anyhow::Result<()> {
    for name in names {
        let others = oneup_sys::processes::named(name)?;
        anyhow::ensure!(
            others.iter().all(|p| p.pid == std::process::id()),
            "{name} is running. Stop OneUpDrive first."
        );
    }
    Ok(())
}

/// Moves this user's folders from the development identity package to the
/// signed one this program runs with, then removes the development package.
/// Only a PC that ran a development build has anything to do here. Refuses
/// rather than guesses: see [`move_then_start`] for how it's normally run.
///
/// In this order, because the order decides what happens to the files:
/// removing a package first makes Windows drop its sync roots and delete
/// their online-only files, while registering the same ID from the new
/// package first moves the record to it, with every placeholder and the
/// Cloud Files identity intact, and the old package can then go (measured on
/// temporary folders, also with the program folder deleted and replaced as
/// an install does). The package goes only when a complete list of the
/// shell's records, read back, shows that it owns nothing.
///
/// Nothing else of ours may run meanwhile. Registering through the shell
/// while an agent is connected leaves a registration Windows drops when it
/// disconnects (docs/architecture/files-on-demand.md), and a development
/// copy registering a folder after the last look would put it back under
/// the package being removed. Each folder's data-folder lock is held too,
/// until the package is gone: a folder whose data folder can't be found or
/// locked stays, and so does the package.
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
    only_this_program_running()?;
    let sid = oneup_sys::pipe::current_user_sid()?;
    let dev_owned = |r: &oneup_cfapi::shell::UserRoot| r.owner.as_deref() == Some(dev.family.as_str());
    let folders: Vec<_> = oneup_cfapi::shell::roots_of_user(&sid)?.into_iter().filter(dev_owned).collect();

    let candidates = data_dirs();
    let mut locks = Vec::new();
    let mut moved = 0;
    let mut why = Vec::new();
    for folder in &folders {
        let token = folder
            .root
            .as_deref()
            .and_then(oneup_cfapi::provider::registration)
            .and_then(|(_, token)| String::from_utf8(token).ok());
        let dir = token.and_then(|t| candidates.iter().find(|d| pair_token(d).as_deref() == Some(t.as_str())));
        let Some(dir) = dir else {
            why.push("a folder has no data folder here".to_string());
            continue;
        };
        let lock = match crate::setup::DirLock::acquire(dir) {
            Ok(lock) => lock,
            Err(e) => {
                why.push(format!("a folder's data folder is in use: {e:#}"));
                continue;
            }
        };
        match move_folder(folder, &own.family, &sid) {
            Ok(()) => moved += 1,
            Err(e) => why.push(format!("a folder wasn't moved: {e:#}")),
        }
        locks.push(lock);
    }

    // Read back, not assumed from the above, and last: nothing may have
    // started since the first look.
    crate::identity::stop_shell_surrogate();
    only_this_program_running()?;
    let owners: Vec<_> = oneup_cfapi::shell::roots_of_user(&sid)?.into_iter().map(|r| r.owner).collect();
    if !crate::identity::may_remove(&dev.family, &owners) {
        if why.is_empty() {
            why.push("a folder is still registered with it".to_string());
        }
        for w in &why {
            tracing::warn!(reason = %w, "the development package stays");
        }
        return Ok(MovedIdentity::Kept { why });
    }
    oneup_sys::identity::remove_one(&dev.full_name)?;
    remove_development_files();
    drop(locks);
    Ok(MovedIdentity::Moved { folders: moved })
}

/// The pair token in `dir`'s database, only read: it may be another build's.
fn pair_token(dir: &Path) -> Option<String> {
    oneup_store::Store::peek_meta(&dir.join("state.db"), crate::sync::KEY_TOKEN).ok().flatten()
}

/// The development package's unpacked files (our own), beside the install
/// folder: nothing reads them once the package is gone.
fn remove_development_files() {
    if let Some(programs) = std::env::current_exe().ok().and_then(|e| e.parent().map(Path::to_path_buf)) {
        let stage = crate::identity::stage_dir(&programs);
        if stage.extension().is_some_and(|x| x == "identity") {
            let _ = std::fs::remove_dir_all(stage);
        }
    }
}

/// Registers one folder again, under this program's package, and checks
/// that it took: the record is the new package's and the Cloud Files
/// registration is unchanged.
fn move_folder(folder: &oneup_cfapi::shell::UserRoot, family: &str, sid: &str) -> anyhow::Result<()> {
    use anyhow::{Context as _, ensure};
    let root = folder.root.as_deref().context("the folder's record has no path")?;
    let (provider, token) =
        oneup_cfapi::provider::registration(root).context("the folder has no Cloud Files registration")?;
    ensure!(provider == oneup_cfapi::PROVIDER_NAME, "the folder is registered by another program");
    let token = String::from_utf8(token).context("the folder's registration isn't a pair's")?;
    ensure!(
        oneup_cfapi::shell::root_id(sid, &token) == folder.id,
        "the folder's registration doesn't match its Explorer entry"
    );
    let shown = oneup_cfapi::shell::registered(&folder.id).map_or_else(|| "OneUpDrive".to_string(), |r| r.display);
    crate::syncroot::register(root, &token, &shown)?;
    let owner = oneup_cfapi::shell::roots_of_user(sid)?.into_iter().find(|r| r.id == folder.id).and_then(|r| r.owner);
    ensure!(owner.as_deref() == Some(family), "the folder's Explorer entry stayed with the old package");
    ensure!(
        oneup_cfapi::provider::registration(root).is_some_and(|(p, t)| p == provider && t == token.as_bytes()),
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
    edit_path(without_path_entry);
    // Last: releasing the folders above may need it (shell registrations).
    // A package that still has a folder stays: removing it would make Windows
    // delete the folder's online-only files, and a folder not released here
    // (another data folder, a release that failed) is still someone's.
    match crate::identity::remove_unowned() {
        Ok(done) if done.kept == 0 => remove_development_files(),
        Ok(done) => tracing::warn!(kept = done.kept, "an identity package was left in place: it has a folder"),
        Err(e) => tracing::warn!(error = %format!("{e:#}"), "the identity packages were left in place"),
    }
    oneup_sys::user_registry::announce_environment_change();
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
        && let Err(e) = crate::syncroot::unregister(Path::new(&local), &token)
    {
        tracing::warn!(error = %format!("{e:#}"), "a folder wasn't released");
    }
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used)]
    use super::*;

    #[test]
    fn a_signed_build_always_adds_its_package_and_a_development_build_only_keeps_what_is_there() {
        assert_eq!(identity_step(Some(true), true), IdentityStep::AddPackage);
        assert_eq!(identity_step(Some(false), true), IdentityStep::AddPackage);
        assert_eq!(identity_step(None, true), IdentityStep::AddPackage);
        assert_eq!(identity_step(Some(true), false), IdentityStep::KeepDevelopment);
        assert_eq!(identity_step(Some(false), false), IdentityStep::AddPackage);
        assert_eq!(identity_step(None, false), IdentityStep::Nothing);
    }

    #[test]
    fn path_gets_this_folder_once_and_keeps_everything_else_as_written() {
        let dir = r"C:\Users\Sam\AppData\Local\OneUpDrive.App\current";
        assert_eq!(with_path_entry("", dir).as_deref(), Some(dir));
        let added = with_path_entry(r"%USERPROFILE%\bin;C:\Tools;", dir).unwrap();
        assert_eq!(added, format!(r"%USERPROFILE%\bin;C:\Tools;{dir}"), "unexpanded, and last");
        assert_eq!(with_path_entry(&added, dir), None);
        assert_eq!(with_path_entry(&format!("C:\\Tools;{}\\", dir.to_uppercase()), dir), None, "same folder");
        assert_eq!(with_path_entry(&"x".repeat(MAX_PATH_VARIABLE), dir), None, "doesn't fit");
        assert_eq!(with_path_entry("C:\\Tools", r"C:\a;b"), None, "a folder a list can't hold");
    }

    #[test]
    fn path_loses_only_this_folder() {
        let dir = r"C:\Programs\current";
        assert_eq!(without_path_entry(r"C:\Tools;%X%\bin", dir), None);
        assert_eq!(without_path_entry(dir, dir).as_deref(), Some(""));
        assert_eq!(
            without_path_entry(r"%X%\bin;C:\PROGRAMS\current\;C:\Programs\current2", dir).as_deref(),
            Some(r"%X%\bin;C:\Programs\current2"),
        );
    }

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
