//! Moving over from the OneDrive app: what the setup window's **Move over**
//! path and `oneup migrate check` share.
//!
//! The check is read-only: it finds the app's folder, says whether the app
//! still manages it, and summarises this data folder's scan of it.
//!
//! **Before unlinking** (4.7.3): when the app is unlinked, Windows itself
//! removes its online-only files from this PC (measured, `oneup-cfapi`
//! test `unregistering_another_provider_removes_only_what_is_in_the_cloud`).
//! Their content is only in OneDrive, so the check verifies, against this
//! data folder's cloud listing, that every one of them is there at the same
//! path. While the app still manages the folder, it also records the
//! choices made there (`crate::carryover`), which are only visible then.
//!
//! **A stalled unlinking**:
//! the app removed its account but never released the folder (its sync
//! root stays registered, every online-only file still there), and can
//! spin like that for days. [`App::stalled`] says so; [`restart_app`],
//! [`reset_app`] (the app's own documented `/reset`) and, as a last resort,
//! [`release`] (OneUpDrive finishes it, through the safety gate's
//! [`oneup_gate::release`] decision) get it finished.

use std::path::{Path, PathBuf};
use std::sync::atomic::AtomicBool;

use anyhow::{Context, Result};
use oneup_plan::{Category, Park, Report};
use oneup_store::Store;
use serde_json::{Value, json};

use crate::carryover::CarryOver;

/// The OneDrive app's personal folder: from its registry settings, else
/// from the environment.
pub fn app_folder() -> Option<PathBuf> {
    let reg = oneup_sys::user_registry::get(APP_KEY, "UserFolder").ok().flatten();
    reg.map(PathBuf::from)
        .or_else(|| std::env::var_os("OneDriveConsumer").map(PathBuf::from))
        .or_else(|| std::env::var_os("OneDrive").map(PathBuf::from))
        .filter(|p| !p.as_os_str().is_empty())
}

/// The OneDrive app's folder on this PC, as it is now.
#[derive(Debug, Clone)]
pub struct App {
    pub folder: PathBuf,
    /// The account the app syncs: its drive ID (the app's `cid`) and e-mail
    /// address, from the app's own settings.
    pub drive: Option<String>,
    pub email: Option<String>,
    /// The app (or another sync program) still manages it.
    pub linked: Option<String>,
    /// The app's own settings no longer have its personal account: it was
    /// unlinked (or is unlinking). With `linked`, the unlinking stalled.
    pub account_removed: bool,
    /// When the app noted the unlinking (ms since 1970), if it did.
    pub unlinked_at_ms: Option<i64>,
    /// Where the app's programs are, and how many of its processes run.
    pub app_dir: PathBuf,
    pub running: usize,
    /// A work or school account is also set up in the app (its `/reset`
    /// would unlink that too).
    pub other_accounts: bool,
    /// Windows' own folders (Desktop, Documents…) and whether each is inside
    /// the app's folder (the app's PC folder backup moves them there).
    pub windows_folders: Vec<(&'static str, PathBuf, bool)>,
}

/// The app's folder, if there is one on this PC.
pub fn app() -> Option<App> {
    let folder = app_folder().filter(|f| f.is_dir())?;
    let linked = oneup_cfapi::other_sync_root(&folder).map(|info| {
        if info.provider_name.is_empty() { "the OneDrive app".to_string() } else { info.provider_name.clone() }
    });
    let windows_folders = oneup_sys::known::Known::ALL
        .iter()
        .filter_map(|k| {
            let p = k.path().ok()?;
            let inside = oneup_local::coexist::is_within(&p, &folder);
            Some((k.name(), p, inside))
        })
        .collect();
    let setting = |name: &str| {
        oneup_sys::user_registry::get(APP_KEY, name)
            .ok()
            .flatten()
            .map(|v| v.trim().to_string())
            .filter(|v| !v.is_empty())
    };
    let account_removed = setting("UserFolder").is_none();
    let unlinked_at_ms = account_removed
        .then(|| oneup_sys::user_registry::get_number(ONEDRIVE_KEY, "PersonalUnlinkedTimeStamp").ok().flatten())
        .flatten()
        .and_then(|t| i64::try_from(t).ok())
        .map(|t| t * 1000);
    let app_dir = app_dir();
    let running = oneup_sys::processes::inside_dir(&app_dir).map(|p| p.len()).unwrap_or(0);
    let other_accounts = (1..=9).any(|n| {
        oneup_sys::user_registry::get(&format!(r"{ACCOUNTS_KEY}\Business{n}"), "UserFolder")
            .ok()
            .flatten()
            .is_some_and(|f| !f.trim().is_empty())
    });
    Some(App {
        folder,
        linked,
        drive: setting("cid").map(|c| c.to_lowercase()),
        email: setting("UserEmail"),
        account_removed,
        unlinked_at_ms,
        app_dir,
        running,
        other_accounts,
        windows_folders,
    })
}

/// [`app`], with the account the app synced taken from the choices
/// recorded by the check when the app's own settings forgot it (they do on
/// unlinking).
pub fn app_in(data_dir: &Path) -> Option<App> {
    let mut a = app()?;
    if a.drive.is_none()
        && let Some(c) = crate::carryover::load(data_dir)
            .ok()
            .flatten()
            .filter(|c| crate::test_account::same_dir(&c.folder, &a.folder))
    {
        a.drive = c.drive;
        a.email = a.email.or(c.email);
    }
    Some(a)
}

/// Where the OneDrive app's programs are: the folder of the program it
/// starts with (`OneDriveTrigger`), else its usual per-user place.
fn app_dir() -> PathBuf {
    oneup_sys::user_registry::get(ONEDRIVE_KEY, "OneDriveTrigger")
        .ok()
        .flatten()
        .and_then(|t| Path::new(t.trim()).parent().map(Path::to_path_buf))
        .filter(|d| !d.as_os_str().is_empty())
        .or_else(|| std::env::var_os("LOCALAPPDATA").map(|l| Path::new(&l).join(r"Microsoft\OneDrive")))
        .unwrap_or_default()
}

const APP_KEY: &str = r"Software\Microsoft\OneDrive\Accounts\Personal";
const ACCOUNTS_KEY: &str = r"Software\Microsoft\OneDrive\Accounts";
const ONEDRIVE_KEY: &str = r"Software\Microsoft\OneDrive";

impl App {
    /// The app removed its account but still holds the folder: unlinking
    /// started and hasn't finished.
    pub fn stalled(&self) -> bool {
        self.linked.is_some() && self.account_removed
    }
}

/// Whether `drive` is another account than the one the app syncs (`false`
/// when either isn't known).
pub fn other_account(app: &App, drive: Option<&str>) -> bool {
    matches!((&app.drive, drive), (Some(a), Some(d)) if !crate::test_account::is_same_drive(d, a))
}

/// Refuses to pair the app's folder with another account's OneDrive than
/// the one the app synced (`store`: the data folder, listed). Other
/// folders pass.
pub fn refuse_other_account(store: &Store, local: &Path, data_dir: &Path) -> Result<()> {
    let Some(app) = app_in(data_dir) else { return Ok(()) };
    if !crate::test_account::same_dir(local, &app.folder) {
        return Ok(());
    }
    if other_account(&app, crate::accounts::listed_drive(store).as_deref()) {
        anyhow::bail!(
            "{} is the OneDrive app's folder for {}, but this data folder is signed in with another Microsoft account; \
             sign out and sign in with that one",
            app.folder.display(),
            app.email.as_deref().unwrap_or("another account")
        );
    }
    Ok(())
}

impl App {
    pub fn to_json(&self) -> Value {
        json!({
            "folder": self.folder.to_string_lossy(),
            "linked": self.linked,
            "email": self.email,
            "stalled": self.stalled(),
            "unlinked_at_ms": self.unlinked_at_ms,
            "app_running": self.running,
            "other_accounts": self.other_accounts,
            "windows_folders": self.windows_folders.iter()
                .map(|(n, p, inside)| json!({ "name": n, "path": p.to_string_lossy(), "inside": inside }))
                .collect::<Vec<_>>(),
        })
    }
}

/// This data folder's scan of the app's folder.
#[derive(Debug, Clone, Default)]
pub struct Scanned {
    pub finished: bool,
    pub files: i64,
    pub bytes: i64,
    pub on_pc: i64,
    pub online_only: i64,
    pub online_only_bytes: i64,
    pub pinned: i64,
    pub excluded_folders: i64,
    /// Cloud placeholders found by a scan made when no sync app held the
    /// folder any more: left behind by a release that skipped a folder some
    /// program had open (`oneup-cfapi` test
    /// `a_folder_held_open_is_left_orphaned_until_released_again`).
    pub left_behind: i64,
}

/// What unlinking removes, checked against the cloud listing.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CloudCheck {
    /// Online-only files (and folders never opened) in OneDrive at the same
    /// path and size.
    pub in_cloud: (u64, u64),
    /// Online-only files whose OneDrive copy is now a newer version.
    pub newer: u64,
    /// Online-only items with nothing at their path in this OneDrive.
    pub missing: (u64, u64),
    /// Some of the missing ones.
    pub examples: Vec<String>,
}

impl CloudCheck {
    pub fn from_report(report: &Report) -> Self {
        let ok = report.tally(Category::OnlineOnly);
        let missing = report.tally(Category::Parked(Park::OnlineOnlyNotInCloud));
        CloudCheck {
            in_cloud: (ok.files, ok.folders),
            newer: report.tally(Category::OnlineOnlyChanged).files,
            missing: (missing.files, missing.folders),
            examples: report
                .examples
                .get(&Category::Parked(Park::OnlineOnlyNotInCloud))
                .into_iter()
                .flatten()
                .cloned()
                .collect(),
        }
    }

    /// Unlinking loses nothing on this PC.
    pub fn all_in_cloud(&self) -> bool {
        self.missing == (0, 0)
    }
}

/// The whole check.
#[derive(Debug, Clone)]
pub struct Check {
    pub app: App,
    pub scanned: Option<Scanned>,
    /// `Err`: not checked yet, and why (no cloud listing, typically).
    pub cloud: Option<Result<CloudCheck, String>>,
    /// The app's choices, recorded now (only while it manages the folder).
    pub carried: Option<CarryOver>,
    /// This data folder is signed in with another account than the app's:
    /// nothing was compared or recorded.
    pub other_account: bool,
}

/// Runs the check (see the module docs). `None` without an app folder.
pub fn check(data_dir: &Path, store: &Store) -> Result<Option<Check>> {
    let Some(app) = app_in(data_dir) else { return Ok(None) };
    let root = store.read(|o| o.local_root())?.filter(|r| {
        oneup_local::coexist::is_within(Path::new(&r.path), &app.folder)
            && oneup_local::coexist::is_within(&app.folder, Path::new(&r.path))
    });
    // Another account's listing would make every online-only file look
    // missing, and its choices noted against the wrong OneDrive.
    let drive =
        crate::accounts::listed_drive(store).or_else(|| crate::accounts::signed_in_as(data_dir).map(|s| s.drive));
    if other_account(&app, drive.as_deref()) {
        match std::fs::remove_file(data_dir.join(crate::carryover::FILE)) {
            Err(e) if e.kind() != std::io::ErrorKind::NotFound => return Err(e.into()),
            _ => {}
        }
        return Ok(Some(Check { app, scanned: None, cloud: None, carried: None, other_account: true }));
    }
    let Some(root) = root else {
        return Ok(Some(Check { app, scanned: None, cloud: None, carried: None, other_account: false }));
    };
    let st = store.read(|o| o.local_stats())?;
    let scanned = Scanned {
        finished: root.finished_at_ms.is_some(),
        files: st.files,
        bytes: st.file_bytes,
        on_pc: st.files - st.online_only_files,
        online_only: st.online_only_files,
        online_only_bytes: st.online_only_bytes,
        pinned: st.pinned_files,
        excluded_folders: st.excluded_folders,
        left_behind: if root.sync_root_provider.is_none() { st.placeholders } else { 0 },
    };
    let cloud = if scanned.finished && (st.online_only_files > 0 || st.unpopulated_folders > 0) {
        match store.read(|o| Ok(oneup_plan::observe(o, 10, None, &Default::default(), &AtomicBool::new(false))))? {
            Ok((_, report)) => Some(Ok(CloudCheck::from_report(&report))),
            Err(oneup_plan::Error::NotReady(why)) => Some(Err(why.to_string())),
            Err(e) => return Err(e.into()),
        }
    } else {
        None
    };
    let recorded = crate::carryover::load(data_dir)?.filter(|c| crate::test_account::same_dir(&c.folder, &app.folder));
    let carried = match recorded {
        // Recorded while the app still had its account: kept, as the app's
        // half-done unlinking (or a release since) may have changed the
        // folder.
        Some(c) if app.stalled() || app.linked.is_none() => Some(c),
        _ if app.linked.is_some() && scanned.finished => {
            let c = crate::carryover::capture(store, &app)?;
            crate::carryover::save(data_dir, &c)?;
            Some(c)
        }
        _ => None,
    };
    Ok(Some(Check { app, scanned: Some(scanned), cloud, carried, other_account: false }))
}

impl Check {
    pub fn to_json(&self) -> Value {
        json!({
            "app": self.app.to_json(),
            "other_account": self.other_account,
            "scanned": self.scanned.as_ref().map(|s| json!({
                "finished": s.finished, "files": s.files, "bytes": s.bytes, "on_pc": s.on_pc,
                "online_only": s.online_only, "online_only_bytes": s.online_only_bytes,
                "pinned": s.pinned, "excluded_folders": s.excluded_folders, "left_behind": s.left_behind,
            })),
            "cloud": match &self.cloud {
                None => Value::Null,
                Some(Err(why)) => json!({ "not_ready": why }),
                Some(Ok(c)) => json!({
                    "in_cloud_files": c.in_cloud.0, "in_cloud_folders": c.in_cloud.1, "newer": c.newer,
                    "missing_files": c.missing.0, "missing_folders": c.missing.1, "examples": c.examples,
                    "all_in_cloud": c.all_in_cloud(),
                }),
            },
            "carried": self.carried.as_ref().map(|c| json!({
                "pinned": c.pinned.len(), "file_kinds": c.file_kinds, "not_on_pc": c.not_on_pc.len(),
                "not_on_pc_examples": c.not_on_pc.iter().take(5).collect::<Vec<_>>(),
            })),
        })
    }
}

// ------------------------------------------------- a stalled unlinking

/// The steps that get a stalled unlinking finished, gentlest first.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Step {
    /// Restart the app: it may pick its sign-out up again and finish.
    Restart,
    /// The app's own documented reset (`OneDrive.exe /reset`), which
    /// disconnects its accounts. Skipped while a work or school account is
    /// set up in it.
    Reset,
    /// OneUpDrive releases the folder itself ([`release`]).
    Release,
}

impl Step {
    pub const ALL: [Step; 3] = [Step::Restart, Step::Reset, Step::Release];

    pub fn name(self) -> &'static str {
        match self {
            Step::Restart => "restart",
            Step::Reset => "reset",
            Step::Release => "release",
        }
    }
}

/// Ends the app's processes (this Windows user's). Returns how many.
pub fn stop_app(app: &App) -> Result<usize> {
    let mut n = 0;
    for p in oneup_sys::processes::inside_dir(&app.app_dir)? {
        if oneup_sys::processes::terminate(&p, 15_000)? {
            n += 1;
        }
    }
    Ok(n)
}

/// Starts the app's main program with `args`, detached: it must not hold
/// this process's output open (the settings window reads it to the end).
fn start_app(app: &App, args: &[&str]) -> Result<()> {
    let exe = app.app_dir.join("OneDrive.exe");
    if !exe.is_file() {
        anyhow::bail!("the OneDrive app's program isn't at {}", exe.display());
    }
    oneup_sys::inherit::keep_std_handles();
    std::process::Command::new(&exe)
        .args(args)
        .stdin(std::process::Stdio::null())
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::null())
        .spawn()
        .with_context(|| format!("starting {}", exe.display()))?;
    Ok(())
}

/// [`Step::Restart`]: stops the app and starts it again.
pub fn restart_app(app: &App) -> Result<()> {
    stop_app(app)?;
    start_app(app, &[])
}

/// [`Step::Reset`]: the app's own reset. Refused while another account
/// is set up in the app.
pub fn reset_app(app: &App) -> Result<()> {
    if app.other_accounts {
        anyhow::bail!(
            "a work or school account is also set up in the OneDrive app, and resetting it would unlink that too"
        );
    }
    stop_app(app)?;
    start_app(app, &["/reset"])
}

/// Waits up to `wait` for the folder to be released (checked every two
/// seconds, as the setup window does). Whether it was.
pub fn wait_released(folder: &Path, wait: std::time::Duration) -> bool {
    let until = std::time::Instant::now() + wait;
    loop {
        if oneup_cfapi::other_sync_root(folder).is_none() {
            return true;
        }
        if std::time::Instant::now() >= until {
            return false;
        }
        std::thread::sleep(std::time::Duration::from_secs(2));
    }
}

/// What [`release`] did.
#[derive(Debug, Clone)]
pub struct Released {
    /// Online-only files and never-listed folders Windows removed from this
    /// PC (all in OneDrive, checked just before).
    pub online_only: (u64, u64),
    /// Folders left out by the exclusion choice: never listed, so their
    /// online-only files weren't counted; Windows removes those too (their
    /// content is in OneDrive, as with any unlinking).
    pub excluded_folders: i64,
    pub provider: String,
}

/// [`Step::Release`]: OneUpDrive finishes the app's stalled unlinking by
/// releasing the folder, as the app should have. Windows then removes the
/// online-only files here (their content is only in OneDrive) and keeps
/// everything else as ordinary files. Only through the safety gate, on
/// evidence gathered now: `scanned`/`listed` say the caller just brought
/// this data folder's scan of the folder and its OneDrive listing up to
/// date (ms since 1970).
///
/// `starting` is told once the gate approved, just before Windows starts:
/// removing hundreds of thousands of placeholders takes many minutes (≈17
/// for ≈234,000), in that one call.
pub fn release(
    data_dir: &Path,
    store: &mut Store,
    listed_at_ms: i64,
    confirmed: bool,
    starting: impl FnOnce(&Released),
) -> Result<Released> {
    use oneup_gate::release::Holder;
    use oneup_gate::release::{Evidence, review_release};
    let app = app_in(data_dir).context("no OneDrive app folder on this PC")?;
    stop_app(&app)?;
    let c = check(data_dir, store)?.context("no OneDrive app folder on this PC")?;
    let left_behind = c.scanned.as_ref().map_or(0, |s| s.left_behind);
    let held = oneup_cfapi::provider::registration(&app.folder);
    match &held {
        Some(_) if !app.stalled() => {
            anyhow::bail!("the OneDrive app isn't stuck unlinking {}", app.folder.display())
        }
        None if left_behind == 0 => anyhow::bail!("nothing is left to finish in {}", app.folder.display()),
        _ => {}
    }
    let provider = held.as_ref().map_or_else(|| "placeholders left behind".to_string(), |(p, _)| p.clone());
    let cloud = match &c.cloud {
        Some(Ok(cc)) => Some(cc.clone()),
        _ => None,
    };
    let scanned_at_ms = store
        .read(|o| o.local_root())?
        .filter(|_| c.scanned.as_ref().is_some_and(|s| s.finished))
        .and_then(|r| r.finished_at_ms);
    let same_account = match (&app.drive, crate::accounts::listed_drive(store)) {
        (Some(a), Some(d)) => Some(crate::test_account::is_same_drive(&d, a)),
        _ => None,
    };
    let evidence = Evidence {
        root: app.folder.clone(),
        held: held.map(|(provider, identity)| Holder {
            ours: provider == oneup_cfapi::PROVIDER_NAME,
            provider,
            identity,
        }),
        left_behind: u64::try_from(left_behind).unwrap_or(0),
        account_removed: app.account_removed,
        app_dir: app.app_dir.clone(),
        app_running: oneup_sys::processes::inside_dir(&app.app_dir)?.len(),
        same_account,
        scanned_at_ms,
        listed_at_ms: Some(listed_at_ms),
        now_ms: crate::sync::now_ms(),
        in_cloud: cloud.as_ref().map_or((0, 0), |c| c.in_cloud),
        missing: cloud.as_ref().map_or((0, 0), |c| c.missing),
        choices_recorded: c.carried.is_some(),
        confirmed,
    };
    let token = review_release(&evidence).map_err(|why| anyhow::anyhow!("not released: {why}"))?;
    let online_only = evidence.in_cloud;
    let excluded_folders = c.scanned.as_ref().map_or(0, |s| s.excluded_folders);
    let released = Released { online_only, excluded_folders, provider: provider.clone() };
    starting(&released);
    oneup_effects::release::release_root(&token).map_err(|e| anyhow::anyhow!("not released: {e}"))?;
    store.write(|o| {
        o.activity_add(&[oneup_store::ActivityEntry {
            seq: 0,
            at_ms: crate::sync::now_ms(),
            outcome: oneup_store::ActivityOutcome::Note,
            action: "finished unlinking".into(),
            path: String::new(),
            detail: format!(
                "The OneDrive app's unlinking had stalled; OneUpDrive finished it ({provider}). Windows removed the \
                 online-only files from this PC: {} files and {} folders checked to be in OneDrive{}.",
                online_only.0,
                online_only.1,
                left_out(excluded_folders)
            ),
        }])
    })?;
    Ok(released)
}

/// What a release left behind, cleared item by item (Windows' own pass
/// skips any folder a program has open, and on a busy PC some always is):
/// through the same gate decision as [`release`], on the evidence of a
/// scan and listing the caller just brought up to date. Folders left out
/// by the exclusion choice aren't listed, so aren't looked into.
pub fn clear_left_behind(
    data_dir: &Path,
    store: &mut Store,
    listed_at_ms: i64,
    confirmed: bool,
) -> Result<oneup_effects::release::Cleared> {
    use oneup_effects::release::LeftItem;
    use oneup_gate::release::{Evidence, review_release};
    let app = app_in(data_dir).context("no OneDrive app folder on this PC")?;
    stop_app(&app)?;
    let c = check(data_dir, store)?.context("no OneDrive app folder on this PC")?;
    let left_behind = c.scanned.as_ref().map_or(0, |s| s.left_behind);
    if oneup_cfapi::provider::registration(&app.folder).is_some() || left_behind == 0 {
        anyhow::bail!("nothing is left behind in {}", app.folder.display());
    }
    let cloud = match &c.cloud {
        Some(Ok(cc)) => Some(cc.clone()),
        _ => None,
    };
    let root = store.read(|o| o.local_root())?.context("the folder hasn't been scanned")?;
    let same_account = match (&app.drive, crate::accounts::listed_drive(store)) {
        (Some(a), Some(d)) => Some(crate::test_account::is_same_drive(&d, a)),
        _ => None,
    };
    let token = review_release(&Evidence {
        root: app.folder.clone(),
        held: None,
        left_behind: u64::try_from(left_behind).unwrap_or(0),
        account_removed: app.account_removed,
        app_dir: app.app_dir.clone(),
        app_running: oneup_sys::processes::inside_dir(&app.app_dir)?.len(),
        same_account,
        scanned_at_ms: root.finished_at_ms,
        listed_at_ms: Some(listed_at_ms),
        now_ms: crate::sync::now_ms(),
        in_cloud: cloud.as_ref().map_or((0, 0), |c| c.in_cloud),
        missing: cloud.as_ref().map_or((0, 0), |c| c.missing),
        choices_recorded: c.carried.is_some(),
        confirmed,
    })
    .map_err(|why| anyhow::anyhow!("not released: {why}"))?;
    // Paths from the scan (of this folder: `check` made sure), under the
    // folder as the token names it.
    let mut files = Vec::new();
    let mut folders = Vec::new();
    for (id, folder, size) in store.read(|o| o.local_cloud_items())? {
        let Some(rel) = store.read(|o| o.local_path_of(id))? else { continue };
        let path = rel.trim_start_matches('/').split('/').fold(app.folder.clone(), |p, x| p.join(x));
        let item = LeftItem { path, id, folder, size };
        if folder { folders.push(item) } else { files.push(item) }
    }
    folders.sort_by_key(|f| std::cmp::Reverse(f.path.components().count()));
    let done = oneup_effects::release::clear_left_behind(&token, files, folders)
        .map_err(|e| anyhow::anyhow!("not released: {e}"))?;
    store.write(|o| {
        o.activity_add(&[oneup_store::ActivityEntry {
            seq: 0,
            at_ms: crate::sync::now_ms(),
            outcome: oneup_store::ActivityOutcome::Note,
            action: "finished unlinking".into(),
            path: String::new(),
            detail: format!(
                "Placeholders the OneDrive app's unlinking left behind, cleared one by one: {} online-only files removed \
                 (in OneDrive), {} files and {} folders made ordinary with their content, {} left as they were (changed \
                 or in use).",
                done.removed, done.files, done.folders, done.skipped
            ),
        }])
    })?;
    Ok(done)
}

/// ", and those in the N folders left out…", for messages about a release.
pub fn left_out(excluded_folders: i64) -> String {
    if excluded_folders > 0 {
        format!(
            ", and those inside the {excluded_folders} folders left out by your exclusion choice (never listed; their \
             content stays in OneDrive)"
        )
    } else {
        String::new()
    }
}

/// Whether this data folder holds choices recorded from the app, waiting
/// for the folder to be paired.
pub fn carried_waiting(data_dir: &Path) -> bool {
    crate::carryover::load(data_dir).ok().flatten().is_some()
}

#[cfg(test)]
mod tests {
    use super::*;
    use oneup_plan::Entry;

    #[test]
    fn counts_what_unlinking_would_remove() {
        let mut r = Report::new(10);
        let mut add = |path: &str, category, is_folder, bytes| {
            r.add(Entry { path: path.into(), category, is_folder, bytes });
        };
        add("/a.txt", Category::OnlineOnly, false, 5);
        add("/lazy", Category::OnlineOnly, true, 0);
        add("/b.txt", Category::OnlineOnlyChanged, false, 9);
        add("/gone.txt", Category::Parked(Park::OnlineOnlyNotInCloud), false, 3);
        add("/here.txt", Category::Identical, false, 3);
        let c = CloudCheck::from_report(&r);
        assert_eq!((c.in_cloud, c.newer, c.missing), ((1, 1), 1, (1, 0)));
        assert_eq!(c.examples, ["/gone.txt"]);
        assert!(!c.all_in_cloud());
    }
}
