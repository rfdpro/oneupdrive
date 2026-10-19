//! What the OneDrive app leaves in the registry once its personal account
//! is unlinked, cleared at the end of moving over.
//!
//! Two leftovers were found after a real move-over:
//!
//! * **`Accounts\Personal`**: the app's settings key for the personal
//!   account, left with only the app's own counters (sign-in attempts,
//!   telemetry) and none of the account's values (`UserFolder`, `cid`,
//!   `UserEmail`…).
//! * **The "OneDrive - Personal" navigation-pane entry**: the app's
//!   per-user registration of its shell namespace (class
//!   [`PANE_CLSID`], under `Software\Classes\CLSID`) and the Desktop
//!   `NameSpace` entry that shows it. It points at Windows' OneDrive known
//!   folder, which is now the folder OneUpDrive syncs, so Explorer shows
//!   OneUpDrive's folder a second time under the app's name, and opening it
//!   asks the app, which has no personal account any more.
//!
//! Each is cleared only when it is provably stale ([`review`], pure): the
//! app doesn't hold the folder any more (no other sync root there), the
//! personal account has none of an account's values, and the pane entry is
//! the app's personal one (its class targets the OneDrive known folder or
//! the folder itself). A linked account, a work or school account (other
//! keys and class IDs) and OneUpDrive's own navigation-pane entry (another
//! class ID) are never touched.
//!
//! Nothing is cleared while the OneDrive app runs (`OneDrive.exe` among
//! this user's processes): it is the only program that writes these keys,
//! and it could link the personal account again meanwhile. One clear or
//! undo runs at a time for this Windows user, whichever data folder it's
//! for, since they all share the user's registry ([`USER_LOCK`]). The keys
//! found are written to a new backup in the data folder first (never over
//! another). Then, for each key in turn: the folder must still be free of
//! other sync programs, the key exactly as found, and, checked last, the
//! app still not running; otherwise that key is left. An error partway
//! stops there, and what was removed before it is still reported and
//! noted in the activity log.
//!
//! The registry has no compare-and-delete, so a window remains between
//! that last check and the removal (one `RegDeleteTreeW` call away). For
//! the app to write a key inside it, it would have to start and link the
//! account within that moment. Should it happen, the backup holds the key
//! as found, and
//! [`undo`] (`oneup migrate leftovers --undo`) puts back what is missing
//! without changing anything there, so the app's own new values stay.
//!
//! [`undo`] puts the newest backup not yet put back (a running app may
//! also have created an empty `Personal` key again; that's harmless).
//!
//! Left alone on purpose: the app's `HideDesktopIcons` value for the same
//! class (Windows may register that class machine-wide, and removing the
//! value could put a OneDrive icon on the desktop), and Windows' OneDrive
//! known folder itself (that's Windows' setting, and it moves Explorer's
//! "OneDrive" folder).

use std::path::{Path, PathBuf};

use anyhow::{Context, Result};
use oneup_sys::user_registry::{RawValue, Tree};
use serde_json::{Value, json};

/// The OneDrive app's class ID for its personal account's shell namespace
/// (one fixed ID; work or school accounts get their own).
pub const PANE_CLSID: &str = "{018D5C66-4533-4307-9B53-224DE2ED1FE6}";
/// Windows' OneDrive known folder (`FOLDERID_SkyDrive`).
const ONEDRIVE_KNOWN_FOLDER: &str = "{A52BBA46-E9E1-435F-B3D9-28DAA648C0F6}";
/// The personal account's values the app keeps while it's linked: any of
/// them there means the account is linked, and nothing is cleared.
const ACCOUNT_VALUES: [&str; 6] = ["UserFolder", "cid", "UserEmail", "UserName", "DisplayName", "UserCID"];

/// Backups, in the data folder: `onedrive-app-leftovers-<ms>.json`.
const BACKUPS: &str = "backups";
const PREFIX: &str = "onedrive-app-leftovers-";
/// A backup being written.
const TEMP: &str = ".tmp";

/// One leftover.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Leftover {
    /// `Accounts\Personal` without an account.
    Account,
    /// The Desktop `NameSpace` entry that shows the pane entry.
    PaneEntry,
    /// The pane entry's class registration.
    PaneClass,
}

impl Leftover {
    /// In the order they're cleared: what shows the pane entry first.
    pub const ALL: [Leftover; 3] = [Leftover::PaneEntry, Leftover::PaneClass, Leftover::Account];

    /// The key, under `HKEY_CURRENT_USER`.
    pub fn key(self) -> String {
        match self {
            Leftover::Account => r"Software\Microsoft\OneDrive\Accounts\Personal".into(),
            Leftover::PaneEntry => {
                format!(r"Software\Microsoft\Windows\CurrentVersion\Explorer\Desktop\NameSpace\{PANE_CLSID}")
            }
            Leftover::PaneClass => format!(r"Software\Classes\CLSID\{PANE_CLSID}"),
        }
    }

    pub fn name(self) -> &'static str {
        match self {
            Leftover::Account => "account",
            Leftover::PaneEntry => "pane_entry",
            Leftover::PaneClass => "pane_class",
        }
    }

    pub fn label(self) -> &'static str {
        match self {
            Leftover::Account => "the OneDrive app's empty personal account settings",
            Leftover::PaneEntry => "the \"OneDrive - Personal\" navigation-pane entry",
            Leftover::PaneClass => "the \"OneDrive - Personal\" navigation-pane registration",
        }
    }

    fn from_name(s: &str) -> Option<Leftover> {
        Leftover::ALL.into_iter().find(|l| l.name() == s)
    }
}

/// The registry under `HKEY_CURRENT_USER`, as far as this needs it (a
/// fake in tests: tests never touch the real OneDrive keys).
pub trait Registry {
    fn tree(&self, key: &str) -> std::io::Result<Option<Tree>>;
    fn remove_tree(&self, key: &str) -> std::io::Result<()>;
    fn restore_missing(&self, key: &str, tree: &Tree) -> std::io::Result<usize>;
}

/// This Windows user's registry.
pub struct UserRegistry;

impl Registry for UserRegistry {
    fn tree(&self, key: &str) -> std::io::Result<Option<Tree>> {
        oneup_sys::user_registry::tree(key)
    }
    fn remove_tree(&self, key: &str) -> std::io::Result<()> {
        oneup_sys::user_registry::remove_key(key)
    }
    fn restore_missing(&self, key: &str, tree: &Tree) -> std::io::Result<usize> {
        oneup_sys::user_registry::restore_missing(key, tree)
    }
}

/// What's there now.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Found {
    pub account: Option<Tree>,
    pub pane_entry: Option<Tree>,
    pub pane_class: Option<Tree>,
}

impl Found {
    pub fn read(reg: &dyn Registry) -> std::io::Result<Found> {
        Ok(Found {
            account: reg.tree(&Leftover::Account.key())?,
            pane_entry: reg.tree(&Leftover::PaneEntry.key())?,
            pane_class: reg.tree(&Leftover::PaneClass.key())?,
        })
    }

    fn get(&self, l: Leftover) -> Option<&Tree> {
        match l {
            Leftover::Account => self.account.as_ref(),
            Leftover::PaneEntry => self.pane_entry.as_ref(),
            Leftover::PaneClass => self.pane_class.as_ref(),
        }
    }
}

/// Whether the personal account key still has an account in it.
fn linked(account: &Tree) -> bool {
    account.values.iter().any(|v| {
        // Registry value names compare case-insensitively; these are ASCII.
        ACCOUNT_VALUES.iter().any(|n| n.eq_ignore_ascii_case(&v.name))
            && match account.text(&v.name) {
                Some(t) => !t.trim().is_empty(),
                None => !v.data.is_empty(),
            }
    })
}

/// `\\?\C:\X\` and `c:\x` alike, as Windows compares paths.
fn comparable(p: &str) -> String {
    oneup_sync::fold(p.strip_prefix(r"\\?\").unwrap_or(p).trim_end_matches(['\\', '/']))
}

/// Whether the pane class is the app's personal entry for `folder`: its
/// shell folder targets the OneDrive known folder or `folder` itself.
fn targets(class: &Tree, folder: &Path) -> bool {
    let Some(bag) = class.key("Instance").and_then(|i| i.key("InitPropertyBag")) else { return false };
    // A GUID: ASCII, compared as such.
    let known = bag.text("TargetKnownFolder").is_some_and(|k| k.trim().eq_ignore_ascii_case(ONEDRIVE_KNOWN_FOLDER));
    let path = bag.text("TargetFolderPath").is_some_and(|p| comparable(&p) == comparable(&folder.to_string_lossy()));
    known || path
}

/// Only a default value (or none) and no sub-keys: how a `NameSpace`
/// entry looks.
fn bare_entry(entry: &Tree) -> bool {
    entry.keys.is_empty() && entry.values.iter().all(|v| v.name.is_empty())
}

/// Which leftovers in `found` to clear, after moving over from the app's
/// folder `folder`. `released`: no other sync program holds the folder.
/// Pure.
pub fn review(found: &Found, folder: &Path, released: bool) -> Vec<Leftover> {
    if !released || found.account.as_ref().is_some_and(linked) {
        return Vec::new();
    }
    let class = found.pane_class.as_ref().is_some_and(|c| targets(c, folder));
    Leftover::ALL
        .into_iter()
        .filter(|l| match l {
            Leftover::Account => found.account.is_some(),
            Leftover::PaneClass => class,
            // Only with the class it shows: a class registered elsewhere
            // isn't known to be the app's.
            Leftover::PaneEntry => class && found.pane_entry.as_ref().is_some_and(bare_entry),
        })
        .collect()
}

/// What [`clear`] did.
#[derive(Debug, Clone, Default)]
pub struct Cleared {
    pub removed: Vec<Leftover>,
    /// Changed between the check and removing it, or the app started or
    /// took the folder meanwhile: left.
    pub changed: Vec<Leftover>,
    pub backup: Option<PathBuf>,
    /// What stopped it partway (what was removed before stays listed).
    pub failed: Option<String>,
}

impl Cleared {
    /// Anything to tell: removed, left or failed.
    pub fn happened(&self) -> bool {
        !self.removed.is_empty() || !self.changed.is_empty() || self.failed.is_some()
    }

    pub fn to_json(&self) -> Value {
        json!({
            "removed": self.removed.iter().map(|l| l.name()).collect::<Vec<_>>(),
            "changed": self.changed.iter().map(|l| l.name()).collect::<Vec<_>>(),
            "backup": self.backup.as_ref().map(|b| b.to_string_lossy().into_owned()),
            "failed": self.failed,
        })
    }

    /// For the activity log and the command line.
    pub fn summary(&self) -> String {
        let list = |ls: &[Leftover]| ls.iter().map(|l| l.label()).collect::<Vec<_>>().join(", ");
        let mut s = if self.removed.is_empty() {
            "Removed nothing.".to_string()
        } else {
            format!("Removed {}.", list(&self.removed))
        };
        if !self.changed.is_empty() {
            s.push_str(&format!(" Skipped (changed, or the OneDrive app started): {}.", list(&self.changed)));
        }
        if let Some(why) = &self.failed {
            s.push_str(&format!(" Stopped: {why}."));
        }
        if let Some(b) = &self.backup {
            s.push_str(&format!(" Backup: {}", b.display()));
        }
        s
    }
}

/// What's checked around each removal besides the keys themselves (a fake
/// in tests).
pub trait Conditions {
    /// The OneDrive app runs, so it could link the account again or
    /// rewrite its keys at any moment.
    fn app_running(&self) -> std::io::Result<bool>;
    /// No other sync program holds the folder.
    fn released(&self) -> bool;
    /// The name of the lock that keeps one clear or undo at a time for
    /// this Windows user ([`USER_LOCK`]; tests use their own).
    fn lock_name(&self) -> &str;
}

/// One clear or undo at a time for this Windows user, whichever data folder
/// it's for: they all share the user's registry. A named mutex per user
/// rather than a file: no file in any data folder to agree on, and Windows
/// lets go of it when the process holding it ends.
pub const USER_LOCK: &str = "OneUpDrive-app-leftovers";

/// This PC: the OneDrive app's main program among this user's processes,
/// and the folder's sync root.
pub struct ThisPc<'a> {
    pub folder: &'a Path,
}

impl Conditions for ThisPc<'_> {
    fn app_running(&self) -> std::io::Result<bool> {
        oneup_sys::processes::named("OneDrive.exe").map(|p| !p.is_empty()).map_err(std::io::Error::other)
    }
    fn released(&self) -> bool {
        oneup_cfapi::other_sync_root(self.folder).is_none()
    }
    fn lock_name(&self) -> &str {
        USER_LOCK
    }
}

/// Clears what [`review`] picks, after a backup in `data_dir`, one change
/// at a time for this user ([`USER_LOCK`]). Refused while the OneDrive app
/// runs. Just before each key goes, the folder must still be released, the
/// key as found and backed up, and then the app still not running; a key
/// for which any of that fails is left. An error once a backup is made
/// stops there and is returned in [`Cleared::failed`], with what was
/// removed before it, so that's never lost.
pub fn clear(reg: &dyn Registry, cond: &dyn Conditions, data_dir: &Path, folder: &Path) -> Result<Cleared> {
    let _lock = lock(cond.lock_name())?;
    if cond.app_running()? {
        anyhow::bail!("{APP_RUNNING}");
    }
    let found = Found::read(reg).context("reading the OneDrive app's registry keys")?;
    let picked = review(&found, folder, cond.released());
    if picked.is_empty() {
        return Ok(Cleared::default());
    }
    let backup = save_backup(data_dir, &found, &picked)?;
    let mut done = Cleared { backup: Some(backup), ..Cleared::default() };
    for l in picked {
        match remove_one(reg, cond, &found, l) {
            Ok(true) => {
                tracing::info!(leftover = l.name(), "removed a OneDrive app leftover");
                done.removed.push(l);
            }
            Ok(false) => done.changed.push(l),
            Err(e) => {
                done.failed = Some(format!("{e:#}"));
                break;
            }
        }
    }
    Ok(done)
}

/// Removes leftover `l` if it still may go (see [`clear`]); `false` if it
/// was left.
fn remove_one(reg: &dyn Registry, cond: &dyn Conditions, found: &Found, l: Leftover) -> Result<bool> {
    let key = l.key();
    // The checks closest to the removal come last: the key compared, then
    // whether the app (the only writer of these keys) started.
    let still = cond.released()
        && reg.tree(&key).with_context(|| format!("reading {}", l.label()))?.as_ref() == found.get(l)
        && !cond.app_running().context("looking for the OneDrive app")?;
    if still {
        reg.remove_tree(&key).with_context(|| format!("removing {}", l.label()))?;
    }
    Ok(still)
}

/// [`clear`], then the activity log noted of whatever happened, partway
/// or not.
pub fn clear_noted(reg: &dyn Registry, cond: &dyn Conditions, data_dir: &Path, folder: &Path) -> Result<Cleared> {
    let c = clear(reg, cond, data_dir, folder)?;
    if c.happened() {
        note(data_dir, "cleared OneDrive app leftovers", c.summary());
    }
    Ok(c)
}

/// Why [`clear`] refuses while the app runs, and what to do.
pub const APP_RUNNING: &str = "the OneDrive app is running; quit it (its icon in the corner of the taskbar > Settings > Quit OneDrive), then run \
     `oneup migrate leftovers --clear`";

/// Takes the per-user lock called `name` ([`USER_LOCK`]).
fn lock(name: &str) -> Result<oneup_sys::mutex::Held> {
    oneup_sys::mutex::try_lock(name)
        .context("taking the lock for the OneDrive app's leftovers")?
        .context("the OneDrive app's leftovers are being cleared or put back already")
}

/// What [`undo`] did.
#[derive(Debug, Clone)]
pub struct Undone {
    pub restored: Vec<Leftover>,
    /// Values set back (those there again weren't changed).
    pub values: usize,
    pub backup: PathBuf,
}

/// Puts the newest backup back (keys and values that are missing; nothing
/// there is changed) and marks it used. `None` without one.
pub fn undo(reg: &dyn Registry, lock_name: &str, data_dir: &Path) -> Result<Option<Undone>> {
    let _lock = lock(lock_name)?;
    let Some(path) = newest_backup(data_dir)? else { return Ok(None) };
    let text = std::fs::read(&path).context("reading the newest backup")?;
    let v: Value = serde_json::from_slice(&text).context("reading the newest backup")?;
    let mut entries = Vec::new();
    for e in v["keys"].as_array().into_iter().flatten() {
        let l = e["leftover"].as_str().and_then(Leftover::from_name).context("an unknown entry in the backup")?;
        // Only the keys this module removes, whatever the file says.
        if e["key"].as_str() != Some(l.key().as_str()) {
            anyhow::bail!("the backup names another key than {}", l.label());
        }
        entries.push((l, tree_from_json(&e["tree"])?));
    }
    let mut values = 0;
    // The account first, then the class, then what shows it.
    for (l, tree) in entries.iter().rev() {
        values += reg.restore_missing(&l.key(), tree).with_context(|| format!("putting back {}", l.label()))?;
    }
    let mut used = path.clone().into_os_string();
    used.push(".undone");
    std::fs::rename(&path, &used)?;
    Ok(Some(Undone { restored: entries.into_iter().map(|(l, _)| l).collect(), values, backup: PathBuf::from(used) }))
}

/// Writes a new backup, never over another: numbered after every backup
/// there (put back or not), so the newest is always the last made, even if
/// the clock went back. Callers hold the lock.
fn save_backup(data_dir: &Path, found: &Found, picked: &[Leftover]) -> Result<PathBuf> {
    use std::io::Write as _;
    let dir = data_dir.join(BACKUPS);
    std::fs::create_dir_all(&dir).context("creating the backups folder")?;
    let keys: Vec<Value> = picked
        .iter()
        .filter_map(|&l| found.get(l).map(|t| json!({ "leftover": l.name(), "key": l.key(), "tree": tree_to_json(t) })))
        .collect();
    let mut at = crate::sync::now_ms().max(backups(data_dir)?.into_iter().map(|(n, _, _)| n + 1).max().unwrap_or(0));
    let bytes = serde_json::to_vec_pretty(&json!({ "at_ms": at, "keys": keys }))?;
    // Written in full and flushed under a temporary name, then linked to
    // its own: a link never replaces a file, and a reader never sees half
    // of one. A temporary file left by a crash is passed over, not reused.
    let (tmp, mut f) = new_temp(&dir).context("writing the backup")?;
    let written = (|| {
        f.write_all(&bytes)?;
        f.sync_all()?;
        drop(f);
        loop {
            let path = dir.join(format!("{PREFIX}{at}.json"));
            match std::fs::hard_link(&tmp, &path) {
                Ok(()) => return Ok(path),
                Err(e) if e.kind() == std::io::ErrorKind::AlreadyExists => at += 1,
                Err(e) => return Err(e),
            }
        }
    })();
    let _ = std::fs::remove_file(&tmp);
    written.context("writing the backup")
}

/// A new temporary file in `dir`, never one that's there: named by this
/// process, the time and a count, the count going up past any taken name.
fn new_temp(dir: &Path) -> std::io::Result<(PathBuf, std::fs::File)> {
    let stamp = std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).map_or(0, |d| d.as_nanos());
    new_temp_from(dir, &format!("{PREFIX}{}-{stamp}", std::process::id()))
}

fn new_temp_from(dir: &Path, stem: &str) -> std::io::Result<(PathBuf, std::fs::File)> {
    for n in 0..1000u32 {
        let tmp = dir.join(format!("{stem}-{n}{TEMP}"));
        match std::fs::OpenOptions::new().write(true).create_new(true).open(&tmp) {
            Ok(f) => return Ok((tmp, f)),
            Err(e) if e.kind() == std::io::ErrorKind::AlreadyExists => {}
            Err(e) => return Err(e),
        }
    }
    Err(std::io::Error::other("no free name for a temporary file"))
}

/// Every backup in the data folder: its number, whether it was put back,
/// and its path.
fn backups(data_dir: &Path) -> Result<Vec<(i64, bool, PathBuf)>> {
    let rd = match std::fs::read_dir(data_dir.join(BACKUPS)) {
        Ok(rd) => rd,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(Vec::new()),
        Err(e) => return Err(e).context("reading the backups folder"),
    };
    let mut out = Vec::new();
    for e in rd {
        let e = e?;
        let name = e.file_name().to_string_lossy().into_owned();
        let Some(rest) = name.strip_prefix(PREFIX) else { continue };
        let (num, undone) = match rest.strip_suffix(".json.undone") {
            Some(n) => (n, true),
            None => match rest.strip_suffix(".json") {
                Some(n) => (n, false),
                None => continue,
            },
        };
        if let Ok(n) = num.parse() {
            out.push((n, undone, e.path()));
        }
    }
    Ok(out)
}

/// The newest backup not yet put back.
fn newest_backup(data_dir: &Path) -> Result<Option<PathBuf>> {
    let newest = backups(data_dir)?.into_iter().filter(|(_, undone, _)| !undone).max_by_key(|(n, _, _)| *n);
    Ok(newest.map(|(_, _, p)| p))
}

fn hex(data: &[u8]) -> String {
    data.iter().map(|b| format!("{b:02x}")).collect()
}

fn unhex(s: &str) -> Option<Vec<u8>> {
    if !s.len().is_multiple_of(2) || !s.is_ascii() {
        return None;
    }
    (0..s.len()).step_by(2).map(|i| u8::from_str_radix(&s[i..i + 2], 16).ok()).collect()
}

fn tree_to_json(t: &Tree) -> Value {
    json!({
        "values": t.values.iter().map(|v| json!({ "name": v.name, "kind": v.kind, "data": hex(&v.data) })).collect::<Vec<_>>(),
        "keys": t.keys.iter().map(|(n, k)| json!({ "name": n, "tree": tree_to_json(k) })).collect::<Vec<_>>(),
    })
}

fn tree_from_json(v: &Value) -> Result<Tree> {
    let bad = || anyhow::anyhow!("the backup can't be read");
    let mut t = Tree::default();
    for x in v["values"].as_array().ok_or_else(bad)? {
        t.values.push(RawValue {
            name: x["name"].as_str().ok_or_else(bad)?.to_string(),
            kind: x["kind"].as_u64().and_then(|k| u32::try_from(k).ok()).ok_or_else(bad)?,
            data: x["data"].as_str().and_then(unhex).ok_or_else(bad)?,
        });
    }
    for x in v["keys"].as_array().ok_or_else(bad)? {
        t.keys.push((x["name"].as_str().ok_or_else(bad)?.to_string(), tree_from_json(&x["tree"])?));
    }
    Ok(t)
}

/// Whether `local` is the OneDrive app's personal folder, as the app's
/// settings or Windows' environment name it (they outlive unlinking).
pub fn is_app_folder(local: &Path) -> bool {
    crate::moveover::app_folder().is_some_and(|app| crate::test_account::same_dir(local, &app))
}

/// What [`clear_now`] would clear for the folder `local`.
pub fn pending(local: &Path) -> Result<Vec<Leftover>> {
    let found = Found::read(&UserRegistry).context("reading the OneDrive app's registry keys")?;
    Ok(review(&found, local, ThisPc { folder: local }.released()))
}

/// [`clear`] on this Windows user's registry for the app's old folder
/// `local`, then Explorer told and the activity log noted.
pub fn clear_now(data_dir: &Path, local: &Path) -> Result<Cleared> {
    let c = clear_noted(&UserRegistry, &ThisPc { folder: local }, data_dir, local)?;
    if c.removed.iter().any(|l| *l != Leftover::Account) {
        // Explorer reads its namespaces again.
        oneup_sys::assoc::changed();
    }
    Ok(c)
}

/// [`undo`] on this Windows user's registry, then Explorer told and the
/// activity log noted.
pub fn undo_now(data_dir: &Path) -> Result<Option<Undone>> {
    let u = undo(&UserRegistry, USER_LOCK, data_dir)?;
    if let Some(u) = &u {
        oneup_sys::assoc::changed();
        let list = u.restored.iter().map(|l| l.label()).collect::<Vec<_>>().join(", ");
        note(data_dir, "put back OneDrive app leftovers", format!("Put back {list}."));
    }
    Ok(u)
}

/// After the first pairing of `local`: clears the leftovers if `local` is
/// the OneDrive app's personal folder. Says what it did, if anything.
/// Never fails the pairing: a problem is logged and returned as a line to
/// show.
pub fn after_move_over(data_dir: &Path, local: &Path) -> Option<String> {
    if !is_app_folder(local) {
        return None;
    }
    match clear_now(data_dir, local) {
        Ok(c) if !c.happened() => None,
        Ok(c) => Some(format!("OneDrive app leftovers: {}", c.summary())),
        Err(e) => {
            // The error can name the data folder: details at debug only.
            tracing::warn!("the OneDrive app's leftovers weren't cleared");
            tracing::debug!("clearing the OneDrive app's leftovers: {e:#}");
            Some(format!("OneDrive app leftovers not cleared: {e:#}"))
        }
    }
}

fn note(data_dir: &Path, action: &str, detail: String) {
    let entry = oneup_store::ActivityEntry {
        seq: 0,
        at_ms: crate::sync::now_ms(),
        outcome: oneup_store::ActivityOutcome::Note,
        action: action.into(),
        path: String::new(),
        detail,
    };
    let noted = crate::setup::open_store(data_dir, 1024).and_then(|mut s| Ok(s.write(|o| o.activity_add(&[entry]))?));
    if let Err(e) = noted {
        tracing::warn!("the activity log didn't take a note about the OneDrive app's leftovers");
        tracing::debug!("noting the OneDrive app's leftovers: {e:#}");
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::cell::{Cell, RefCell};
    use std::collections::BTreeMap;
    use std::rc::Rc;

    type Keys = BTreeMap<String, Tree>;
    /// A change made "meanwhile", after a read.
    type Meanwhile = Option<Box<dyn FnMut(&mut Keys)>>;

    /// A registry in memory: whole trees by key.
    #[derive(Default)]
    struct Fake {
        keys: RefCell<Keys>,
        /// Called after each read.
        on_read: RefCell<Meanwhile>,
        /// Removals so far, and the one that fails (counted from 1).
        removals: Cell<usize>,
        fail_removal: Cell<Option<usize>>,
    }

    impl Registry for Fake {
        fn tree(&self, key: &str) -> std::io::Result<Option<Tree>> {
            let got = self.keys.borrow().get(key).cloned();
            if let Some(f) = self.on_read.borrow_mut().as_mut() {
                f(&mut self.keys.borrow_mut());
            }
            Ok(got)
        }
        fn remove_tree(&self, key: &str) -> std::io::Result<()> {
            self.removals.set(self.removals.get() + 1);
            if self.fail_removal.get() == Some(self.removals.get()) {
                return Err(std::io::Error::other("access denied"));
            }
            self.keys.borrow_mut().remove(key);
            Ok(())
        }
        fn restore_missing(&self, key: &str, tree: &Tree) -> std::io::Result<usize> {
            let mut keys = self.keys.borrow_mut();
            let have = keys.entry(key.to_string()).or_default();
            Ok(merge(have, tree))
        }
    }

    fn merge(have: &mut Tree, want: &Tree) -> usize {
        let mut n = 0;
        for v in &want.values {
            if !have.values.iter().any(|h| h.name.eq_ignore_ascii_case(&v.name)) {
                have.values.push(v.clone());
                n += 1;
            }
        }
        for (name, sub) in &want.keys {
            if !have.keys.iter().any(|(h, _)| h == name) {
                have.keys.push((name.clone(), Tree::default()));
            }
            if let Some((_, h)) = have.keys.iter_mut().find(|(h, _)| h == name) {
                n += merge(h, sub);
            }
        }
        n
    }

    fn sz(name: &str, s: &str) -> RawValue {
        RawValue { name: name.into(), kind: 1, data: s.encode_utf16().chain([0]).flat_map(u16::to_le_bytes).collect() }
    }

    fn dword(name: &str, n: u32) -> RawValue {
        RawValue { name: name.into(), kind: 4, data: n.to_le_bytes().to_vec() }
    }

    /// What unlinking the personal account left, in the shape found.
    fn unlinked_account() -> Tree {
        Tree {
            values: vec![dword("LastSignInResult", 0), sz("OneDriveDeviceId", "device")],
            keys: vec![("USQInformation".into(), Tree { values: vec![dword("lastFCAAuthAttempt", 1)], keys: vec![] })],
        }
    }

    fn linked_account() -> Tree {
        let mut t = unlinked_account();
        t.values.push(sz("UserFolder", r"D:\Sync\OneDrive"));
        t
    }

    fn pane_class(bag: Vec<RawValue>) -> Tree {
        let instance = Tree {
            values: vec![sz("CLSID", "{0E5AAE11-A475-4C5B-AB00-C66DE400274E}")],
            keys: vec![("InitPropertyBag".into(), Tree { values: bag, keys: vec![] })],
        };
        Tree {
            values: vec![sz("", "OneDrive - Personal"), dword("System.IsPinnedToNameSpaceTree", 1)],
            keys: vec![
                ("DefaultIcon".into(), Tree { values: vec![sz("", "icon")], keys: vec![] }),
                ("Instance".into(), instance),
            ],
        }
    }

    fn known_folder_class() -> Tree {
        // Written as Windows writes GUIDs: case differs.
        pane_class(vec![dword("Attributes", 17), sz("TargetKnownFolder", "{a52bba46-e9e1-435f-b3d9-28daa648c0f6}")])
    }

    fn entry() -> Tree {
        Tree { values: vec![sz("", "OneDrive - Personal")], keys: vec![] }
    }

    fn found(account: Option<Tree>, class: Option<Tree>, pane: Option<Tree>) -> Found {
        Found { account, pane_class: class, pane_entry: pane }
    }

    fn folder() -> PathBuf {
        PathBuf::from(r"D:\Sync\OneDrive")
    }

    #[test]
    fn the_unlinked_accounts_leftovers_are_picked() {
        let f = found(Some(unlinked_account()), Some(known_folder_class()), Some(entry()));
        assert_eq!(review(&f, &folder(), true), Leftover::ALL);
        // A class that targets the folder itself counts too, as Windows
        // compares paths.
        let by_path = pane_class(vec![sz("TargetFolderPath", r"\\?\d:\SYNC\onedrive\")]);
        let f = found(None, Some(by_path), Some(entry()));
        assert_eq!(review(&f, &folder(), true), [Leftover::PaneEntry, Leftover::PaneClass]);
    }

    #[test]
    fn nothing_is_picked_while_the_account_is_linked_or_the_folder_held() {
        let f = found(Some(linked_account()), Some(known_folder_class()), Some(entry()));
        assert_eq!(review(&f, &folder(), true), []);
        // Any one of the account's values counts; an empty one doesn't.
        for v in [sz("cid", "abc"), sz("UserEmail", "someone"), dword("UserCID", 1)] {
            let mut a = unlinked_account();
            a.values.push(v);
            assert_eq!(review(&found(Some(a), None, None), &folder(), true), []);
        }
        let mut a = unlinked_account();
        a.values.push(sz("UserFolder", "  "));
        assert_eq!(review(&found(Some(a), None, None), &folder(), true), [Leftover::Account]);
        // Another sync program still holds the folder: an unlinking under way.
        let f = found(Some(unlinked_account()), Some(known_folder_class()), Some(entry()));
        assert_eq!(review(&f, &folder(), false), []);
    }

    #[test]
    fn a_pane_entry_is_picked_only_when_it_is_the_apps_personal_one() {
        // A class pointing elsewhere (or with no target) isn't the app's
        // entry for this folder; nor is the entry that shows it.
        for class in [
            pane_class(vec![sz("TargetFolderPath", r"D:\Elsewhere")]),
            pane_class(vec![sz("TargetKnownFolder", "{FDD39AD0-238F-46AF-ADB4-6C85480369C7}")]),
            pane_class(vec![]),
            Tree::default(),
        ] {
            let f = found(None, Some(class), Some(entry()));
            assert_eq!(review(&f, &folder(), true), [], "{f:?}");
        }
        // An entry without the class: the class may be registered elsewhere.
        assert_eq!(review(&found(None, None, Some(entry())), &folder(), true), []);
        // An entry that holds more than a name isn't a plain entry.
        let busy = Tree { values: vec![sz("", "x"), dword("Other", 1)], keys: vec![] };
        let f = found(None, Some(known_folder_class()), Some(busy));
        assert_eq!(review(&f, &folder(), true), [Leftover::PaneClass]);
    }

    fn temp(name: &str) -> PathBuf {
        let d = std::env::temp_dir().join(format!("oneup-appleftovers-{name}-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&d);
        std::fs::create_dir_all(&d).unwrap();
        d
    }

    /// Whether the app runs and the folder is free, as a test sets them
    /// (shared with a registry's "meanwhile" change).
    #[derive(Clone)]
    struct Cond {
        running: Rc<Cell<bool>>,
        released: Rc<Cell<bool>>,
        /// Each test its own lock: tests run side by side.
        lock: String,
    }

    impl Cond {
        fn quiet() -> Cond {
            static N: std::sync::atomic::AtomicU32 = std::sync::atomic::AtomicU32::new(0);
            let n = N.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
            Cond {
                running: Rc::new(Cell::new(false)),
                released: Rc::new(Cell::new(true)),
                lock: format!("OneUpDrive-test-leftovers-{}-{n}", std::process::id()),
            }
        }
    }

    impl Conditions for Cond {
        fn app_running(&self) -> std::io::Result<bool> {
            Ok(self.running.get())
        }
        fn released(&self) -> bool {
            self.released.get()
        }
        fn lock_name(&self) -> &str {
            &self.lock
        }
    }

    fn fake_with_leftovers() -> Fake {
        let fake = Fake::default();
        let mut k = fake.keys.borrow_mut();
        k.insert(Leftover::Account.key(), unlinked_account());
        k.insert(Leftover::PaneClass.key(), known_folder_class());
        k.insert(Leftover::PaneEntry.key(), entry());
        // A work account and another program's class: never touched.
        k.insert(r"Software\Microsoft\OneDrive\Accounts\Business1".into(), linked_account());
        k.insert(r"Software\Classes\CLSID\{04271989-C4D2-4F3A-8E5B-1B2C3D4E5F60}".into(), known_folder_class());
        drop(k);
        fake
    }

    /// Runs `then` right after the registry's `n`th read (reads 1-3 are the
    /// first look; then one per key, in [`Leftover::ALL`]'s order, each the
    /// last comparison before that key's removal).
    fn after_read(fake: &Fake, n: usize, mut then: impl FnMut(&mut Keys) + 'static) {
        let mut reads = 0;
        *fake.on_read.borrow_mut() = Some(Box::new(move |keys| {
            reads += 1;
            if reads == n {
                then(keys);
            }
        }));
    }

    fn backup_files(dir: &Path) -> Vec<(i64, bool, PathBuf)> {
        let mut b = backups(dir).unwrap();
        b.sort();
        b
    }

    #[test]
    fn clearing_backs_up_removes_and_undo_puts_back() {
        let dir = temp("clear");
        let fake = fake_with_leftovers();
        let cond = Cond::quiet();
        let before = fake.keys.borrow().clone();
        let c = clear(&fake, &cond, &dir, &folder()).unwrap();
        assert_eq!(c.removed, Leftover::ALL);
        assert!(c.changed.is_empty());
        let keys: Vec<String> = fake.keys.borrow().keys().cloned().collect();
        assert_eq!(
            keys,
            [
                r"Software\Classes\CLSID\{04271989-C4D2-4F3A-8E5B-1B2C3D4E5F60}",
                r"Software\Microsoft\OneDrive\Accounts\Business1"
            ]
        );
        assert!(c.backup.as_ref().unwrap().is_file());
        assert!(
            c.summary().starts_with("Removed the \"OneDrive - Personal\" navigation-pane entry"),
            "{}",
            c.summary()
        );

        // Clearing again finds nothing, and makes no backup.
        let again = clear(&fake, &cond, &dir, &folder()).unwrap();
        assert!(again.removed.is_empty() && again.backup.is_none());

        // The app made an empty Personal key again meanwhile: undo adds
        // the values back and changes nothing that's there.
        fake.keys
            .borrow_mut()
            .insert(Leftover::Account.key(), Tree { values: vec![dword("LastSignInResult", 7)], keys: vec![] });
        let u = undo(&fake, &cond.lock, &dir).unwrap().unwrap();
        assert_eq!(u.restored, Leftover::ALL);
        let after = fake.keys.borrow().clone();
        assert_eq!(after[&Leftover::PaneClass.key()], before[&Leftover::PaneClass.key()]);
        assert_eq!(after[&Leftover::PaneEntry.key()], before[&Leftover::PaneEntry.key()]);
        let account = &after[&Leftover::Account.key()];
        assert_eq!(account.values[0], dword("LastSignInResult", 7), "kept as it was");
        assert!(account.values.contains(&sz("OneDriveDeviceId", "device")));
        assert_eq!(account.key("USQInformation"), unlinked_account().key("USQInformation"));
        // Used up: a second undo has nothing.
        assert!(undo(&fake, &cond.lock, &dir).unwrap().is_none());
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn a_key_changed_after_the_check_is_left() {
        let dir = temp("changed");
        let fake = fake_with_leftovers();
        // The personal account gains its values again before its own last
        // comparison (read 6): left as it is.
        after_read(&fake, 5, |keys| {
            keys.insert(Leftover::Account.key(), linked_account());
        });
        let c = clear(&fake, &Cond::quiet(), &dir, &folder()).unwrap();
        assert_eq!(c.removed, [Leftover::PaneEntry, Leftover::PaneClass]);
        assert_eq!(c.changed, [Leftover::Account]);
        assert_eq!(fake.keys.borrow()[&Leftover::Account.key()], linked_account());
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn the_app_starting_after_the_last_comparison_stops_that_removal() {
        let dir = temp("started");
        let fake = fake_with_leftovers();
        let cond = Cond::quiet();
        // Right after the pane class's last comparison (read 5), the app
        // starts and rewrites the class: the comparison passed, but the
        // check after it sees the app, and nothing more goes.
        let running = cond.running.clone();
        let rewritten = pane_class(vec![sz("TargetKnownFolder", ONEDRIVE_KNOWN_FOLDER), dword("New", 1)]);
        let kept = rewritten.clone();
        after_read(&fake, 5, move |keys| {
            running.set(true);
            keys.insert(Leftover::PaneClass.key(), rewritten.clone());
        });
        let c = clear(&fake, &cond, &dir, &folder()).unwrap();
        assert_eq!(c.removed, [Leftover::PaneEntry]);
        assert_eq!(c.changed, [Leftover::PaneClass, Leftover::Account]);
        let keys = fake.keys.borrow();
        assert_eq!(keys[&Leftover::PaneClass.key()], kept);
        assert_eq!(keys[&Leftover::Account.key()], unlinked_account());
        drop(keys);
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn a_folder_taken_again_after_the_backup_stops_the_removals() {
        let dir = temp("taken");
        let fake = fake_with_leftovers();
        let cond = Cond::quiet();
        // Another sync program takes the folder once the backup is made and
        // the first key went.
        let released = cond.released.clone();
        after_read(&fake, 4, move |_| released.set(false));
        let c = clear(&fake, &cond, &dir, &folder()).unwrap();
        assert_eq!(c.removed, [Leftover::PaneEntry]);
        assert_eq!(c.changed, [Leftover::PaneClass, Leftover::Account]);
        assert!(fake.keys.borrow().contains_key(&Leftover::PaneClass.key()));
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn nothing_is_cleared_while_the_app_runs() {
        let dir = temp("running");
        let fake = fake_with_leftovers();
        let before = fake.keys.borrow().clone();
        let cond = Cond::quiet();
        cond.running.set(true);
        let e = clear(&fake, &cond, &dir, &folder()).unwrap_err();
        assert!(e.to_string().contains("the OneDrive app is running"), "{e}");
        assert_eq!(*fake.keys.borrow(), before);
        assert!(backup_files(&dir).is_empty());
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn nothing_is_removed_or_backed_up_for_a_linked_account() {
        let dir = temp("linked");
        let fake = fake_with_leftovers();
        fake.keys.borrow_mut().insert(Leftover::Account.key(), linked_account());
        let before = fake.keys.borrow().clone();
        let c = clear(&fake, &Cond::quiet(), &dir, &folder()).unwrap();
        assert!(c.removed.is_empty() && c.backup.is_none());
        assert_eq!(*fake.keys.borrow(), before);
        assert!(backup_files(&dir).is_empty());
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn one_clear_or_undo_at_a_time_for_every_data_folder() {
        // Two data folders, one user's registry: a clear for the first
        // (here, its lock held by another thread) keeps the second's out.
        let (first, second) = (temp("lock-first"), temp("lock-second"));
        let fake = fake_with_leftovers();
        let cond = Cond::quiet();
        let (took, wait) = std::sync::mpsc::channel();
        let (release, released) = std::sync::mpsc::channel::<()>();
        let other = {
            let name = cond.lock.clone();
            std::thread::spawn(move || {
                let held = lock(&name).unwrap();
                took.send(()).unwrap();
                released.recv().unwrap();
                drop(held);
            })
        };
        wait.recv().unwrap();
        let e = clear(&fake, &cond, &second, &folder()).unwrap_err();
        assert!(e.to_string().contains("already"), "{e}");
        assert!(undo(&fake, &cond.lock, &first).is_err());
        assert_eq!(fake.keys.borrow().len(), 5, "nothing removed");
        assert!(backup_files(&second).is_empty());
        release.send(()).unwrap();
        other.join().unwrap();
        assert_eq!(clear(&fake, &cond, &second, &folder()).unwrap().removed, Leftover::ALL);
        let _ = std::fs::remove_dir_all(&first);
        let _ = std::fs::remove_dir_all(&second);
    }

    #[test]
    fn a_removal_that_fails_keeps_what_went_before_and_is_noted() {
        let dir = temp("fails");
        let fake = fake_with_leftovers();
        fake.fail_removal.set(Some(2));
        let c = clear_noted(&fake, &Cond::quiet(), &dir, &folder()).unwrap();
        assert_eq!(c.removed, [Leftover::PaneEntry]);
        assert!(c.failed.as_deref().is_some_and(|f| f.contains("access denied")), "{c:?}");
        assert!(c.backup.as_ref().is_some_and(|b| b.is_file()));
        let keys = fake.keys.borrow();
        assert!(!keys.contains_key(&Leftover::PaneEntry.key()));
        assert!(keys.contains_key(&Leftover::PaneClass.key()) && keys.contains_key(&Leftover::Account.key()));
        drop(keys);
        let summary = c.summary();
        assert!(summary.starts_with("Removed the \"OneDrive - Personal\" navigation-pane entry."), "{summary}");
        assert!(summary.contains("Stopped:"), "{summary}");
        // The activity log has it, removal and failure alike.
        let store = crate::setup::open_store(&dir, 1024).unwrap();
        let log = store.read(|o| o.activity_recent(None, 10)).unwrap();
        assert!(log.iter().any(|e| e.action == "cleared OneDrive app leftovers" && e.detail == summary), "{log:?}");
        drop(store);
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn a_temporary_file_left_by_a_crash_is_passed_over() {
        let dir = temp("stale");
        std::fs::write(dir.join(format!("x-0{TEMP}")), "stale").unwrap();
        std::fs::write(dir.join(format!("x-1{TEMP}")), "stale").unwrap();
        let (path, _f) = new_temp_from(&dir, "x").unwrap();
        assert_eq!(path, dir.join(format!("x-2{TEMP}")));
        assert_eq!(std::fs::read_to_string(dir.join(format!("x-0{TEMP}"))).unwrap(), "stale");
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn a_backup_never_replaces_another_and_comes_after_every_one() {
        let dir = temp("order");
        std::fs::create_dir_all(dir.join(BACKUPS)).unwrap();
        // Backups numbered ahead of the clock (it went back since): the new
        // one comes after them, and neither is touched.
        let ahead = i64::MAX / 4;
        let older = dir.join(BACKUPS).join(format!("{PREFIX}{ahead}.json"));
        let undone = dir.join(BACKUPS).join(format!("{PREFIX}{}.json.undone", ahead + 1));
        std::fs::write(&older, "keep").unwrap();
        std::fs::write(&undone, "keep too").unwrap();
        let fake = fake_with_leftovers();
        let c = clear(&fake, &Cond::quiet(), &dir, &folder()).unwrap();
        let made = c.backup.unwrap();
        assert_eq!(made, dir.join(BACKUPS).join(format!("{PREFIX}{}.json", ahead + 2)));
        assert_eq!(std::fs::read_to_string(&older).unwrap(), "keep");
        assert_eq!(std::fs::read_to_string(&undone).unwrap(), "keep too");
        assert_eq!(newest_backup(&dir).unwrap(), Some(made.clone()));
        // No temporary file is left.
        let names: Vec<_> = backup_files(&dir).into_iter().map(|(n, u, _)| (n - ahead, u)).collect();
        assert_eq!(names, [(0, false), (1, true), (2, false)]);
        assert!(
            std::fs::read_dir(dir.join(BACKUPS)).unwrap().all(|e| !e
                .unwrap()
                .file_name()
                .to_string_lossy()
                .ends_with(".tmp"))
        );
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn a_backup_naming_another_key_is_refused() {
        let dir = temp("forged");
        std::fs::create_dir_all(dir.join(BACKUPS)).unwrap();
        let v = json!({ "keys": [{ "leftover": "account", "key": r"Software\Microsoft\OneDrive\Accounts\Business1",
            "tree": tree_to_json(&unlinked_account()) }] });
        std::fs::write(dir.join(BACKUPS).join(format!("{PREFIX}5.json")), v.to_string()).unwrap();
        let fake = Fake::default();
        assert!(undo(&fake, &Cond::quiet().lock, &dir).is_err());
        assert!(fake.keys.borrow().is_empty());
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn trees_survive_the_backup_file() {
        let t = known_folder_class();
        assert_eq!(tree_from_json(&tree_to_json(&t)).unwrap(), t);
        assert_eq!(unhex("0aff"), Some(vec![10, 255]));
        assert_eq!(unhex("0"), None);
    }
}
