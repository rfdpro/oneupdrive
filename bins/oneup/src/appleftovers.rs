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
//! class ID) are never touched. The keys found are written to a backup in
//! the data folder first, and each is read again just before it goes: one
//! that changed since is left. [`undo`] puts the newest backup back, without
//! changing anything that's there again (the app, still running for a
//! work account, may create an empty `Personal` key again; that's harmless).
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
    /// Changed between the check and removing it: left.
    pub changed: Vec<Leftover>,
    pub backup: Option<PathBuf>,
}

impl Cleared {
    pub fn to_json(&self) -> Value {
        json!({
            "removed": self.removed.iter().map(|l| l.name()).collect::<Vec<_>>(),
            "changed": self.changed.iter().map(|l| l.name()).collect::<Vec<_>>(),
            "backup": self.backup.as_ref().map(|b| b.to_string_lossy().into_owned()),
        })
    }

    /// For the activity log and the command line.
    pub fn summary(&self) -> String {
        let list = |ls: &[Leftover]| ls.iter().map(|l| l.label()).collect::<Vec<_>>().join(", ");
        let mut s = format!("Removed {}.", list(&self.removed));
        if !self.changed.is_empty() {
            s.push_str(&format!(" Skipped (changed meanwhile): {}.", list(&self.changed)));
        }
        if let Some(b) = &self.backup {
            s.push_str(&format!(" Backup: {}", b.display()));
        }
        s
    }
}

/// Clears what [`review`] picks, after a backup in `data_dir`. Each key is
/// read again just before it's removed, and left if it changed.
pub fn clear(reg: &dyn Registry, data_dir: &Path, folder: &Path, released: bool) -> Result<Cleared> {
    let found = Found::read(reg).context("reading the OneDrive app's registry keys")?;
    let picked = review(&found, folder, released);
    if picked.is_empty() {
        return Ok(Cleared::default());
    }
    let backup = save_backup(data_dir, &found, &picked)?;
    let mut done = Cleared { backup: Some(backup), ..Cleared::default() };
    for l in picked {
        let key = l.key();
        if reg.tree(&key)?.as_ref() != found.get(l) {
            done.changed.push(l);
            continue;
        }
        reg.remove_tree(&key).with_context(|| format!("removing {}", l.label()))?;
        tracing::info!(leftover = l.name(), "removed a OneDrive app leftover");
        done.removed.push(l);
    }
    Ok(done)
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
pub fn undo(reg: &dyn Registry, data_dir: &Path) -> Result<Option<Undone>> {
    let Some(path) = newest_backup(data_dir)? else { return Ok(None) };
    let text = std::fs::read(&path).with_context(|| format!("reading {}", path.display()))?;
    let v: Value = serde_json::from_slice(&text).with_context(|| format!("reading {}", path.display()))?;
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

fn save_backup(data_dir: &Path, found: &Found, picked: &[Leftover]) -> Result<PathBuf> {
    let dir = data_dir.join(BACKUPS);
    std::fs::create_dir_all(&dir).with_context(|| format!("creating {}", dir.display()))?;
    let mut at = crate::sync::now_ms();
    let path = loop {
        let p = dir.join(format!("{PREFIX}{at}.json"));
        if !p.exists() {
            break p;
        }
        at += 1;
    };
    let keys: Vec<Value> = picked
        .iter()
        .filter_map(|&l| found.get(l).map(|t| json!({ "leftover": l.name(), "key": l.key(), "tree": tree_to_json(t) })))
        .collect();
    let v = json!({ "at_ms": at, "keys": keys });
    crate::ownfile::write(&path, &serde_json::to_vec_pretty(&v)?)
        .with_context(|| format!("writing {}", path.display()))?;
    Ok(path)
}

/// The newest backup not yet put back (names sort by time).
fn newest_backup(data_dir: &Path) -> Result<Option<PathBuf>> {
    let dir = data_dir.join(BACKUPS);
    let rd = match std::fs::read_dir(&dir) {
        Ok(rd) => rd,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(None),
        Err(e) => return Err(e.into()),
    };
    let mut best: Option<(i64, PathBuf)> = None;
    for e in rd {
        let e = e?;
        let name = e.file_name().to_string_lossy().into_owned();
        let Some(ms) = name.strip_prefix(PREFIX).and_then(|r| r.strip_suffix(".json")).and_then(|m| m.parse().ok())
        else {
            continue;
        };
        if best.as_ref().is_none_or(|(b, _)| ms > *b) {
            best = Some((ms, e.path()));
        }
    }
    Ok(best.map(|(_, p)| p))
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
    Ok(review(&found, local, oneup_cfapi::other_sync_root(local).is_none()))
}

/// [`clear`] on this Windows user's registry for the app's old folder
/// `local`, then Explorer told and the activity log noted.
pub fn clear_now(data_dir: &Path, local: &Path) -> Result<Cleared> {
    let released = oneup_cfapi::other_sync_root(local).is_none();
    let c = clear(&UserRegistry, data_dir, local, released)?;
    if c.removed.iter().any(|l| *l != Leftover::Account) {
        // Explorer reads its namespaces again.
        oneup_sys::assoc::changed();
    }
    if !c.removed.is_empty() || !c.changed.is_empty() {
        note(data_dir, "cleared OneDrive app leftovers", c.summary());
    }
    Ok(c)
}

/// [`undo`] on this Windows user's registry, then Explorer told and the
/// activity log noted.
pub fn undo_now(data_dir: &Path) -> Result<Option<Undone>> {
    let u = undo(&UserRegistry, data_dir)?;
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
        Ok(c) if c.removed.is_empty() && c.changed.is_empty() => None,
        Ok(c) => Some(format!("OneDrive app leftovers: {}", c.summary())),
        Err(e) => {
            tracing::warn!("clearing the OneDrive app's leftovers: {e:#}");
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
        tracing::warn!("noting the OneDrive app's leftovers: {e:#}");
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::cell::RefCell;
    use std::collections::BTreeMap;

    type Keys = BTreeMap<String, Tree>;
    /// A change made "meanwhile", after a read.
    type Meanwhile = Option<Box<dyn FnMut(&mut Keys)>>;

    /// A registry in memory: whole trees by key.
    #[derive(Default)]
    struct Fake {
        keys: RefCell<Keys>,
        /// Called after each read.
        on_read: RefCell<Meanwhile>,
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

    #[test]
    fn clearing_backs_up_removes_and_undo_puts_back() {
        let dir = temp("clear");
        let fake = fake_with_leftovers();
        let before = fake.keys.borrow().clone();
        let c = clear(&fake, &dir, &folder(), true).unwrap();
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
        let again = clear(&fake, &dir, &folder(), true).unwrap();
        assert!(again.removed.is_empty() && again.backup.is_none());

        // The app made an empty Personal key again meanwhile: undo adds
        // the values back and changes nothing that's there.
        fake.keys
            .borrow_mut()
            .insert(Leftover::Account.key(), Tree { values: vec![dword("LastSignInResult", 7)], keys: vec![] });
        let u = undo(&fake, &dir).unwrap().unwrap();
        assert_eq!(u.restored, Leftover::ALL);
        let after = fake.keys.borrow().clone();
        assert_eq!(after[&Leftover::PaneClass.key()], before[&Leftover::PaneClass.key()]);
        assert_eq!(after[&Leftover::PaneEntry.key()], before[&Leftover::PaneEntry.key()]);
        let account = &after[&Leftover::Account.key()];
        assert_eq!(account.values[0], dword("LastSignInResult", 7), "kept as it was");
        assert!(account.values.contains(&sz("OneDriveDeviceId", "device")));
        assert_eq!(account.key("USQInformation"), unlinked_account().key("USQInformation"));
        // Used up: a second undo has nothing.
        assert!(undo(&fake, &dir).unwrap().is_none());
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn a_key_changed_after_the_check_is_left() {
        let dir = temp("changed");
        let fake = fake_with_leftovers();
        // The app signs the personal account in again between the check and
        // the removal: its key gains the account's values.
        let mut reads = 0;
        *fake.on_read.borrow_mut() = Some(Box::new(move |keys| {
            reads += 1;
            if reads == 5 {
                keys.insert(Leftover::Account.key(), linked_account());
            }
        }));
        let c = clear(&fake, &dir, &folder(), true).unwrap();
        assert_eq!(c.removed, [Leftover::PaneEntry, Leftover::PaneClass]);
        assert_eq!(c.changed, [Leftover::Account]);
        assert_eq!(fake.keys.borrow()[&Leftover::Account.key()], linked_account());
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn nothing_is_removed_or_backed_up_for_a_linked_account() {
        let dir = temp("linked");
        let fake = fake_with_leftovers();
        fake.keys.borrow_mut().insert(Leftover::Account.key(), linked_account());
        let before = fake.keys.borrow().clone();
        let c = clear(&fake, &dir, &folder(), true).unwrap();
        assert!(c.removed.is_empty() && c.backup.is_none());
        assert_eq!(*fake.keys.borrow(), before);
        assert!(!dir.join(BACKUPS).exists());
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
        assert!(undo(&fake, &dir).is_err());
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
