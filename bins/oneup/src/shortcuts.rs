//! Shortcuts broken by the switch from the OneDrive app, made to work again.
//!
//! They break two ways:
//! * **Set up in a new folder** (`%USERPROFILE%\OneUpDrive`): shortcuts made
//!   into the app's folder (`%USERPROFILE%\OneDrive`) still lead there.
//!   Where the same item is now comes from the pairs: the app's folder holds
//!   all of OneDrive, so `<app folder>\<remote path>\x` is `<local folder>\x`.
//! * **Moved over** (same folder): a shortcut stores its target as a chain of
//!   shell items too, and the app's chains go through its own Explorer entry,
//!   which goes when the app is unlinked. The shell then can't follow them
//!   (it names no target, and the shortcut opens nothing), though the plain
//!   path the shortcut also keeps (its LinkInfo) is still right. Saving it
//!   again with that path rebuilds the chain.
//!
//! This looks on the Desktop, in the Start menu and among the taskbar's
//! pins, and changes a shortcut **only when asked**, and only if its new
//! target exists. Each changed shortcut is copied first to
//! `<data folder>\shortcuts\<time>\`, with a list of what changed, so
//! [`undo`] puts the last batch back. Shortcuts inside a synced folder (ours
//! or another program's) are left alone: changing one there is a change to
//! user content that sync would carry to OneDrive. Folders and shortcuts
//! that are cloud placeholders are never listed or read (reading one can
//! make its provider download it).

use std::path::{Path, PathBuf};
use std::time::UNIX_EPOCH;

use anyhow::{Context, Result};
use oneup_local::coexist::{is_within, plain};
use serde_json::{Value, json};

const BACKUPS: &str = "shortcuts";
const LIST: &str = "fixed.json";
const MAX_DEPTH: usize = 6;
const MAX_ENTRIES: usize = 20_000;

#[derive(Debug, Clone, PartialEq, Eq)]
struct Mapping {
    from: PathBuf,
    to: PathBuf,
}

#[derive(Debug, Clone, Default, PartialEq, Eq)]
struct Folders {
    maps: Vec<Mapping>,
    /// The folders of our pairs with the account's own OneDrive: what the
    /// app's folder held, so its dead links into them are rebuilt.
    ours: Vec<PathBuf>,
    /// Every folder we sync (shared folders and vaults too): never changed.
    protected: Vec<PathBuf>,
}

impl Folders {
    fn ours(&self, p: &Path) -> bool {
        self.ours.iter().any(|o| is_within(p, o))
    }

    fn protects(&self, p: &Path) -> bool {
        self.protected.iter().any(|o| is_within(p, o))
    }

    fn map(&self, p: &Path) -> Option<PathBuf> {
        if self.ours(p) {
            return None;
        }
        let m = self.maps.iter().filter(|m| is_within(p, &m.from)).max_by_key(|m| m.from.components().count())?;
        Some(m.to.join(p.components().skip(m.from.components().count()).collect::<PathBuf>()))
    }

    fn new_target(&self, target: &Path, dead: bool) -> Option<PathBuf> {
        self.map(target).or_else(|| (dead && self.ours(target)).then(|| target.to_path_buf()))
    }

    fn plan(
        &self,
        target: &Path,
        working_dir: Option<&Path>,
        dead: bool,
        exists: impl Fn(&Path) -> bool,
    ) -> Option<(PathBuf, Option<PathBuf>)> {
        let new = self.new_target(target, dead).filter(|n| exists(n))?;
        let wd = working_dir.and_then(|w| self.map(w)).filter(|w| exists(w));
        Some((new, wd))
    }
}

fn app_folders(noted: Vec<PathBuf>, from: Option<&Path>, ours: &[PathBuf]) -> Vec<PathBuf> {
    let candidates: Vec<PathBuf> = match from {
        Some(f) => vec![f.to_path_buf()],
        None => noted
            .into_iter()
            .chain(crate::moveover::app_folder())
            .chain(std::env::var_os("USERPROFILE").map(|h| Path::new(&h).join("OneDrive")))
            .collect(),
    };
    let same = |a: &Path, b: &Path| is_within(a, b) && is_within(b, a);
    let mut out: Vec<PathBuf> = Vec::new();
    for c in candidates {
        if c.as_os_str().is_empty() || out.iter().any(|o| same(o, &c)) || ours.iter().any(|o| is_within(o, &c)) {
            continue;
        }
        out.push(c);
    }
    out
}

/// Shared folders aren't in the app's folder under their remote path.
fn folders(dir: &Path, from: Option<&Path>) -> Result<Folders> {
    let mut pairs: Vec<(PathBuf, String)> = Vec::new();
    let mut noted: Vec<PathBuf> = Vec::new();
    {
        let store = crate::setup::open_store(dir, 1024)?;
        let get = |k: &str| store.read(|o| o.meta_get(k));
        if let (Some(l), Some(r)) = (get(crate::sync::KEY_LOCAL)?, get(crate::sync::KEY_REMOTE)?) {
            pairs.push((plain(Path::new(&l)), r));
        }
        noted.extend(get(crate::moveover::KEY_APP_FOLDER)?.map(PathBuf::from));
    }
    noted.extend(crate::carryover::load(dir).ok().flatten().map(|c| c.folder));
    let mut protected: Vec<PathBuf> = pairs.iter().map(|(l, _)| l.clone()).collect();
    for e in crate::pairs::list_strict(dir).context("reading the folder pairs")? {
        let d = crate::pairs::describe(&e).with_context(|| format!("reading the folder pair {}", e.name))?;
        protected.push(plain(&d.local));
        if !matches!(d.kind, crate::pairs::Kind::Shared { .. }) {
            pairs.push((plain(&d.local), e.remote));
        }
    }
    for v in crate::vaultpair::list_strict(dir).context("reading the vaults")? {
        let (folder, _) = crate::vaultpair::describe(&v).with_context(|| format!("reading the vault {}", v.name))?;
        protected.push(plain(&folder));
    }
    let ours: Vec<PathBuf> = pairs.iter().map(|(l, _)| l.clone()).collect();
    let mut maps = Vec::new();
    for app in app_folders(noted, from, &protected) {
        for (local, remote) in &pairs {
            let from = remote.split(['/', '\\']).filter(|s| !s.is_empty()).fold(app.clone(), |acc, s| acc.join(s));
            maps.push(Mapping { from, to: local.clone() });
        }
    }
    Ok(Folders { maps, ours, protected })
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Place {
    Desktop,
    StartMenu,
    Taskbar,
}

impl Place {
    pub fn label(self) -> &'static str {
        match self {
            Place::Desktop => "Desktop",
            Place::StartMenu => "Start menu",
            Place::Taskbar => "Taskbar",
        }
    }
}

#[cfg(windows)]
fn places() -> Vec<(Place, PathBuf)> {
    use oneup_sys::known;
    [
        (Place::Desktop, known::Known::Desktop.path()),
        (Place::StartMenu, known::start_menu()),
        (Place::Taskbar, known::user_pinned()),
    ]
    .into_iter()
    .filter_map(|(p, f)| Some((p, f.ok()?)))
    .collect()
}

/// A broken shortcut, and where it should lead.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Found {
    pub place: Place,
    pub link: PathBuf,
    pub target: PathBuf,
    pub new_target: PathBuf,
    /// The folder it starts in, when that moves too.
    pub new_working_dir: Option<PathBuf>,
    /// It opens nothing now (the old target is gone, or the shell can't
    /// follow it), rather than just leading to the old place.
    pub broken: bool,
    /// The shortcut file's contents as found ([`fingerprint`]).
    sha: String,
}

impl Found {
    fn to_json(&self) -> Value {
        json!({
            "place": self.place.label(),
            "link": self.link.to_string_lossy(),
            "name": self.link.file_stem().map(|n| n.to_string_lossy()),
            "target": self.target.to_string_lossy(),
            "new_target": self.new_target.to_string_lossy(),
            "broken": self.broken,
        })
    }
}

#[derive(Debug, Default)]
pub struct Scan {
    pub found: Vec<Found>,
    /// Shortcuts of ours to fix whose item isn't here (excluded, renamed,
    /// or deleted since): left as they are.
    pub unmatched: usize,
}

struct Target {
    path: PathBuf,
    working_dir: Option<PathBuf>,
    /// The shell can't follow it: `path` is the one the file keeps.
    dead: bool,
}

#[cfg(windows)]
fn target_of(r: &mut oneup_sys::shortcut::Reader, path: &Path) -> Option<Target> {
    let link = r.read(path).ok()?;
    let dead = link.target.is_none();
    Some(Target { path: link.target.or(link.stored)?, working_dir: link.working_dir, dead })
}

#[cfg(windows)]
fn scan(places: &[(Place, PathBuf)], f: &Folders) -> Result<Scan> {
    // Placeholders are disguised from our programs unless asked.
    oneup_cfapi::expose_placeholders()?;
    let mut reader = oneup_sys::shortcut::Reader::new()?;
    let mut s = Scan::default();
    for (place, root) in places {
        let mut budget = MAX_ENTRIES;
        walk(&mut Walk { place: *place, f, reader: &mut reader, budget: &mut budget, s: &mut s }, root, 0);
    }
    Ok(s)
}

#[cfg(windows)]
struct Walk<'a> {
    place: Place,
    f: &'a Folders,
    reader: &'a mut oneup_sys::shortcut::Reader,
    budget: &'a mut usize,
    s: &'a mut Scan,
}

#[cfg(windows)]
fn walk(w: &mut Walk<'_>, dir: &Path, depth: usize) {
    use std::os::windows::fs::MetadataExt as _;
    if w.f.protects(dir) || oneup_cfapi::sync_root_containing(dir).is_some() {
        return;
    }
    let Ok(entries) = std::fs::read_dir(dir) else { return };
    for e in entries.flatten() {
        if *w.budget == 0 {
            return;
        }
        *w.budget -= 1;
        // From the directory listing: nothing is opened.
        let Ok(md) = e.metadata() else { continue };
        let path = e.path();
        if md.file_attributes() & oneup_ntfs::dirent::NOT_PLAIN != 0 {
            continue;
        }
        if md.is_dir() {
            if depth + 1 < MAX_DEPTH {
                walk(w, &path, depth + 1);
            }
            continue;
        }
        if !path.extension().is_some_and(|x| x.eq_ignore_ascii_case("lnk")) {
            continue;
        }
        let Some(Target { path: target, working_dir, dead }) = target_of(w.reader, &path) else { continue };
        if w.f.new_target(&target, dead).is_none() {
            continue;
        }
        let exists = |p: &Path| std::fs::symlink_metadata(p).is_ok();
        let Ok(bytes) = plain_bytes(&path) else { continue };
        let sha = fingerprint(&bytes);
        match w.f.plan(&target, working_dir.as_deref(), dead, exists) {
            Some((new_target, new_working_dir)) => {
                tracing::debug!(link = %path.display(), dead, "a shortcut to fix");
                w.s.found.push(Found {
                    place: w.place,
                    link: path,
                    broken: dead || !exists(&target),
                    target,
                    new_target,
                    new_working_dir,
                    sha,
                });
            }
            None => w.s.unmatched += 1,
        }
    }
}

/// A shortcut's contents, by their SHA-256: whether it changed, whatever
/// its size and time say.
fn fingerprint(bytes: &[u8]) -> String {
    use sha2::Digest as _;
    sha2::Sha256::digest(bytes).iter().map(|b| format!("{b:02x}")).collect()
}

/// The contents of the plain file at `p` (not a placeholder, or a link put
/// in its place, whose reading another program might answer).
#[cfg(windows)]
fn plain_bytes(p: &Path) -> std::io::Result<Vec<u8>> {
    use std::os::windows::fs::MetadataExt as _;
    let md = std::fs::symlink_metadata(p)?;
    if !md.is_file() || md.file_attributes() & oneup_ntfs::dirent::NOT_PLAIN != 0 {
        return Err(std::io::Error::new(std::io::ErrorKind::InvalidData, "it isn't a plain file any more"));
    }
    std::fs::read(p)
}

/// A shortcut file's size and modification time, as an earlier version
/// noted what it left (it had no [`fingerprint`]).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct Stamp {
    size: u64,
    modified_ns: u64,
}

impl Stamp {
    fn of(md: &std::fs::Metadata) -> Stamp {
        let ns = md.modified().ok().and_then(|t| t.duration_since(UNIX_EPOCH).ok()).map_or(0, |d| d.as_nanos() as u64);
        Stamp { size: md.len(), modified_ns: ns }
    }
}

/// One shortcut of a batch, as its list records it: written before the new
/// shortcut is made, again with its fingerprint before it replaces the old
/// one, and again after.
#[derive(Debug, Clone)]
struct Done {
    link: PathBuf,
    backup: String,
    target: PathBuf,
    new_target: PathBuf,
    /// The new shortcut's [`fingerprint`]: undo puts the copy back only
    /// over exactly that.
    made: Option<String>,
    /// The shortcut as it was left, once it was replaced.
    left: Option<Stamp>,
    /// Undo is done with it (put back, or changed by someone since).
    settled: bool,
}

impl Done {
    fn to_json(&self) -> Value {
        json!({
            "link": self.link.to_string_lossy(), "backup": self.backup,
            "target": self.target.to_string_lossy(), "new_target": self.new_target.to_string_lossy(),
            "made": self.made,
            "size": self.left.map(|l| l.size), "modified_ns": self.left.map(|l| l.modified_ns),
            "undone": self.settled,
        })
    }

    /// Strict: an entry missing anything, or with half a stamp, is damaged.
    fn from_json(v: &Value) -> Option<Done> {
        let text = |k: &str| v.get(k)?.as_str().map(str::to_owned);
        let number = |k: &str| match v.get(k) {
            None | Some(Value::Null) => Some(None),
            Some(n) => n.as_u64().map(Some),
        };
        let left = match (number("size")?, number("modified_ns")?) {
            (Some(size), Some(modified_ns)) => Some(Stamp { size, modified_ns }),
            (None, None) => None,
            _ => return None,
        };
        let made = match v.get("made") {
            None | Some(Value::Null) => None,
            Some(m) => Some(m.as_str()?.to_owned()),
        };
        Some(Done {
            link: text("link")?.into(),
            backup: text("backup")?,
            target: text("target")?.into(),
            new_target: text("new_target")?.into(),
            made,
            left,
            settled: match v.get("undone") {
                None => false,
                Some(u) => u.as_bool()?,
            },
        })
    }
}

/// A new, empty file beside `path` for its replacement, never one that
/// exists.
fn temp_beside(path: &Path) -> Result<(PathBuf, std::fs::File)> {
    let parent = path.parent().context("no folder")?;
    let name = path.file_name().context("no name")?.to_string_lossy();
    for n in 0..1000 {
        let tmp = parent.join(format!(".{name}.{}-{n}.oneup-tmp", std::process::id()));
        match std::fs::OpenOptions::new().write(true).create_new(true).open(&tmp) {
            Ok(f) => return Ok((tmp, f)),
            Err(e) if e.kind() == std::io::ErrorKind::AlreadyExists => continue,
            Err(e) => return Err(e).with_context(|| format!("writing {}", tmp.display())),
        }
    }
    anyhow::bail!("no free name beside {}", path.display())
}

/// Writes one of our own files (in the data folder, under [`lock`])
/// through a temporary file, both on disk before this returns.
#[cfg(windows)]
fn write_durably(path: &Path, bytes: &[u8]) -> Result<()> {
    use std::io::Write as _;
    let (tmp, mut f) = temp_beside(path)?;
    let written = f.write_all(bytes).and_then(|()| f.sync_all());
    drop(f);
    let done = written
        .map_err(anyhow::Error::from)
        .and_then(|()| Ok(oneup_sys::shortcut::move_over(&tmp, path)?))
        .with_context(|| format!("writing {}", path.display()));
    if done.is_err() {
        let _ = std::fs::remove_file(&tmp);
    }
    done
}

#[cfg(windows)]
fn write_list(batch: &Path, done: &[Done]) -> Result<()> {
    let undone = done.iter().all(|d| d.settled);
    let v = json!({ "undone": undone, "fixed": done.iter().map(Done::to_json).collect::<Vec<_>>() });
    write_durably(&batch.join(LIST), &serde_json::to_vec_pretty(&v)?)
}

/// Replaces the shortcut at `link` with what `make` writes to a temporary
/// file beside it, if `link`'s contents are still `expected` right before:
/// the shortcut is never half-written, and keeps its permissions and other
/// streams.
#[cfg(windows)]
fn replace_link(link: &Path, expected: &str, make: impl FnOnce(&Path) -> Result<()>) -> Result<()> {
    let flush = |p: &Path| -> Result<()> {
        let f = std::fs::OpenOptions::new().write(true).open(p).with_context(|| format!("writing {}", p.display()))?;
        Ok(f.sync_all()?)
    };
    let (tmp, f) = temp_beside(link)?;
    drop(f);
    let ready = make(&tmp).and_then(|()| flush(&tmp)).and_then(|()| {
        anyhow::ensure!(fingerprint(&plain_bytes(link)?) == expected, "it changed since");
        Ok(())
    });
    if let Err(e) = ready {
        // The shortcut is as the person or another program left it.
        let _ = std::fs::remove_file(&tmp);
        return Err(e);
    }
    if let Err(e) = oneup_sys::shortcut::replace(link, &tmp) {
        // A replacement that failed partway with the shortcut gone keeps
        // the new one, at the shortcut's name or the temporary one.
        if link.exists() || !tmp.exists() {
            let _ = std::fs::remove_file(&tmp);
            return Err(e.into());
        }
        return Err(anyhow::Error::from(e).context(format!("the new shortcut is at {}", tmp.display())));
    }
    // On disk before the list says it's done.
    flush(link)
}

/// One change to shortcuts at a time, across the window and the command
/// line. Windows lets go of the file if the process ends.
#[cfg(windows)]
fn lock(dir: &Path) -> Result<std::fs::File> {
    use std::os::windows::fs::OpenOptionsExt as _;
    let backups = dir.join(BACKUPS);
    std::fs::create_dir_all(&backups).with_context(|| format!("creating {}", backups.display()))?;
    std::fs::OpenOptions::new()
        .write(true)
        .create(true)
        .truncate(false)
        .share_mode(0)
        .open(backups.join("lock"))
        .context("another change to shortcuts is running")
}

/// A new batch folder, never one that exists.
fn new_batch(dir: &Path) -> Result<PathBuf> {
    let mut at = crate::sync::now_ms();
    loop {
        let batch = dir.join(BACKUPS).join(at.to_string());
        match std::fs::create_dir(&batch) {
            Ok(()) => return Ok(batch),
            Err(e) if e.kind() == std::io::ErrorKind::AlreadyExists => at += 1,
            Err(e) => return Err(e).with_context(|| format!("creating {}", batch.display())),
        }
    }
}

/// Whether a shortcut at `link` may be changed: not in a folder we sync
/// (as the pairs are now) nor in any other sync root.
#[cfg(windows)]
fn may_change(protected: &Folders, link: &Path) -> Result<()> {
    let parent = link.parent().context("no folder")?;
    anyhow::ensure!(
        !protected.protects(parent) && oneup_cfapi::sync_root_containing(parent).is_none(),
        "it's in a synced folder"
    );
    Ok(())
}

/// What [`fix`] or [`undo`] did.
#[derive(Debug, Default)]
pub struct Outcome {
    pub changed: usize,
    /// Shortcuts left as they were, and why.
    pub skipped: Vec<(PathBuf, String)>,
}

/// Points each shortcut in `found` at its new target, keeping a copy of it
/// as it was in data folder `dir`. A shortcut that changed since it was
/// found is skipped.
#[cfg(windows)]
pub fn fix(dir: &Path, found: &[Found]) -> Result<Outcome> {
    let _lock = lock(dir)?;
    // The folders we sync now, not when the search ran.
    let protected = folders(dir, None)?;
    let batch = new_batch(dir)?;
    let mut reader = oneup_sys::shortcut::Reader::new()?;
    let mut out = Outcome::default();
    let mut done: Vec<Done> = Vec::new();
    for (n, f) in found.iter().enumerate() {
        match fix_one(&mut reader, &protected, &batch, n, f, &mut done) {
            Ok(()) => out.changed += 1,
            Err(e) => out.skipped.push((f.link.clone(), format!("{e:#}"))),
        }
    }
    if done.is_empty() {
        let _ = std::fs::remove_dir_all(&batch);
    }
    Ok(out)
}

/// Fixes one shortcut. Its copy, and an entry naming the new shortcut by
/// its fingerprint, are on disk before it's replaced, so a fix stopped at
/// any point still undoes.
#[cfg(windows)]
fn fix_one(
    reader: &mut oneup_sys::shortcut::Reader,
    protected: &Folders,
    batch: &Path,
    n: usize,
    f: &Found,
    done: &mut Vec<Done>,
) -> Result<()> {
    may_change(protected, &f.link)?;
    // As it is now, not as it was when found.
    let bytes = plain_bytes(&f.link)?;
    anyhow::ensure!(fingerprint(&bytes) == f.sha, "it changed since it was found");
    let now = target_of(reader, &f.link).map(|t| t.path);
    anyhow::ensure!(now.as_deref() == Some(f.target.as_path()), "it changed since it was found");
    anyhow::ensure!(std::fs::symlink_metadata(&f.new_target).is_ok(), "what it should open is gone");
    let backup = format!("{n}.lnk");
    write_durably(&batch.join(&backup), &bytes)?;
    done.push(Done {
        link: f.link.clone(),
        backup,
        target: f.target.clone(),
        new_target: f.new_target.clone(),
        made: None,
        left: None,
        settled: false,
    });
    write_list(batch, done)?;
    replace_link(&f.link, &f.sha, |tmp| {
        anyhow::ensure!(std::fs::symlink_metadata(&f.new_target).is_ok(), "what it should open is gone");
        oneup_sys::shortcut::retarget_to(&f.link, tmp, &f.new_target, f.new_working_dir.as_deref())?;
        if let Some(d) = done.last_mut() {
            d.made = Some(fingerprint(&std::fs::read(tmp)?));
        }
        write_list(batch, done)
    })?;
    let left = Stamp::of(&std::fs::metadata(&f.link)?);
    if let Some(d) = done.last_mut() {
        d.left = Some(left);
    }
    write_list(batch, done)
}

/// The newest batch not undone yet: its folder, when, and what it changed.
/// Only a batch without a list (nothing in it was changed) is passed over:
/// a list that can't be read stops the search, since undoing an older batch
/// instead would be the wrong change.
fn last_batch(dir: &Path) -> Result<Option<(PathBuf, i64, Vec<Done>)>> {
    let entries = match std::fs::read_dir(dir.join(BACKUPS)) {
        Ok(e) => e,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(None),
        Err(e) => return Err(e).context("reading the shortcuts' copies"),
    };
    let mut batches: Vec<(i64, PathBuf)> =
        entries.flatten().filter_map(|e| Some((e.file_name().to_str()?.parse().ok()?, e.path()))).collect();
    batches.sort_by_key(|(at, _)| std::cmp::Reverse(*at));
    for (at, path) in batches {
        let list = path.join(LIST);
        let text = match std::fs::read(&list) {
            Ok(t) => t,
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => continue,
            Err(e) => return Err(e).with_context(|| format!("reading {}", list.display())),
        };
        let damaged = || format!("the list {} is damaged", list.display());
        let v: Value = serde_json::from_slice(&text).with_context(damaged)?;
        let undone = v.get("undone").and_then(Value::as_bool).with_context(damaged)?;
        if undone {
            continue;
        }
        let entries = v.get("fixed").and_then(Value::as_array).with_context(damaged)?;
        let done: Option<Vec<Done>> = entries.iter().map(Done::from_json).collect();
        return Ok(Some((path, at, done.with_context(damaged)?)));
    }
    Ok(None)
}

fn last_fix(dir: &Path) -> Option<(i64, usize)> {
    last_batch(dir).ok().flatten().map(|(_, at, d)| (at, d.iter().filter(|d| !d.settled).count()))
}

/// Puts the shortcuts the last fix changed back as they were, each only if
/// nothing changed it since. One that couldn't be put back for a reason
/// that may pass stays in the batch for the next undo.
#[cfg(windows)]
pub fn undo(dir: &Path) -> Result<Outcome> {
    let _lock = lock(dir)?;
    let (batch, _, mut done) = last_batch(dir)?.context("there's no change to shortcuts to undo")?;
    let protected = folders(dir, None)?;
    let mut out = Outcome::default();
    for d in done.iter_mut().filter(|d| !d.settled) {
        match undo_one(&protected, &batch, d) {
            Ok(None) => {
                d.settled = true;
                out.changed += 1;
            }
            Ok(Some(why)) => {
                d.settled = true;
                out.skipped.push((d.link.clone(), why));
            }
            Err(e) => out.skipped.push((d.link.clone(), format!("{e:#}"))),
        }
    }
    write_list(&batch, &done)?;
    Ok(out)
}

/// Puts one shortcut back: `None` when it did, `Some(why)` when it never
/// will (someone changed or removed the shortcut since); an error may pass,
/// so the entry stays.
#[cfg(windows)]
fn undo_one(protected: &Folders, batch: &Path, d: &Done) -> Result<Option<String>> {
    may_change(protected, &d.link)?;
    let bytes = match plain_bytes(&d.link) {
        Ok(b) => b,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(Some("it isn't there any more".into())),
        Err(e) if e.kind() == std::io::ErrorKind::InvalidData => return Ok(Some(e.to_string())),
        Err(e) => return Err(e.into()),
    };
    let now = fingerprint(&bytes);
    let backup = std::fs::read(batch.join(&d.backup))?;
    match (&d.made, d.left) {
        (Some(made), _) if *made == now => {}
        (Some(_), None) | (None, None) if now == fingerprint(&backup) => {
            return Ok(Some("it was never changed".into()));
        }
        // An earlier version noted only the size and time it left.
        (None, Some(left)) if Stamp::of(&std::fs::metadata(&d.link)?) == left => {}
        _ => return Ok(Some("it changed since".into())),
    }
    replace_link(&d.link, &now, |tmp| {
        use std::io::Write as _;
        let mut f = std::fs::OpenOptions::new().write(true).truncate(true).open(tmp)?;
        f.write_all(&backup)?;
        Ok(f.sync_all()?)
    })?;
    Ok(None)
}

pub fn scan_json(dir: &Path, s: &Scan) -> Value {
    json!({
        "found": s.found.iter().map(Found::to_json).collect::<Vec<_>>(),
        "unmatched": s.unmatched,
        "last_fix": last_fix(dir).map(|(at, n)| json!({ "at_ms": at, "count": n })),
    })
}

/// Looks for shortcuts to fix for data folder `dir`, everywhere they're kept.
#[cfg(windows)]
pub fn find(dir: &Path, from: Option<&Path>) -> Result<Scan> {
    let f = folders(dir, from)?;
    if f.protected.is_empty() {
        return Ok(Scan::default());
    }
    scan(&places(), &f)
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used)]
    use super::*;

    /// The temp folder by its long name: CI's is an 8.3 short path
    /// (`RUNNER~1`), and the shell stores shortcut paths long.
    fn long_temp() -> PathBuf {
        let t = std::fs::canonicalize(std::env::temp_dir()).unwrap();
        let s = t.to_string_lossy();
        PathBuf::from(s.strip_prefix(r"\\?\").unwrap_or(&s))
    }

    fn m(from: &str, to: &str) -> Mapping {
        Mapping { from: from.into(), to: to.into() }
    }

    fn fl(maps: impl Into<Vec<Mapping>>) -> Folders {
        let maps = maps.into();
        let ours: Vec<PathBuf> = maps.iter().map(|m| m.to.clone()).collect();
        Folders { protected: ours.clone(), ours, maps }
    }

    #[test]
    fn paths_move_by_the_longest_matching_folder() {
        let f = fl([
            m(r"C:\Users\Me\OneDrive", r"C:\Users\Me\OneUpDrive"),
            m(r"C:\Users\Me\OneDrive\Photos", r"D:\Photos"),
        ]);
        assert_eq!(
            f.map(Path::new(r"c:\users\me\onedrive\Docs\a.docx")),
            Some(PathBuf::from(r"C:\Users\Me\OneUpDrive\Docs\a.docx"))
        );
        assert_eq!(f.map(Path::new(r"C:\Users\Me\OneDrive\Photos\x.jpg")), Some(PathBuf::from(r"D:\Photos\x.jpg")));
        assert_eq!(f.map(Path::new(r"C:\Users\Me\OneDrive")), Some(PathBuf::from(r"C:\Users\Me\OneUpDrive")));
        assert_eq!(f.map(Path::new(r"C:\Users\Me\OneDriveOld\a")), None, "whole names only");
        assert_eq!(f.map(Path::new(r"C:\Users\Me\OneUpDrive\a")), None, "already here");
        assert_eq!(f.map(Path::new(r"C:\Windows\notepad.exe")), None);
    }

    #[test]
    fn only_shortcuts_whose_item_is_here_change() {
        let f = fl([m(r"C:\OneDrive", r"C:\OneUpDrive")]);
        let here = |p: &Path| p.starts_with(r"C:\OneUpDrive\Docs");
        assert_eq!(
            f.plan(Path::new(r"C:\OneDrive\Docs\a.txt"), Some(Path::new(r"C:\OneDrive\Docs")), false, here),
            Some((PathBuf::from(r"C:\OneUpDrive\Docs\a.txt"), Some(PathBuf::from(r"C:\OneUpDrive\Docs"))))
        );
        assert_eq!(
            f.plan(Path::new(r"C:\OneDrive\Docs\a.txt"), Some(Path::new(r"C:\Temp")), false, here),
            Some((PathBuf::from(r"C:\OneUpDrive\Docs\a.txt"), None)),
            "a working folder elsewhere stays"
        );
        assert_eq!(f.plan(Path::new(r"C:\OneDrive\Gone\a.txt"), None, false, here), None);
        let a = Path::new(r"C:\OneUpDrive\Docs\a.txt");
        assert_eq!(f.plan(a, None, true, here), Some((a.to_path_buf(), None)));
        assert_eq!(f.plan(a, None, false, here), None, "one the shell can follow is fine");
        assert_eq!(f.plan(Path::new(r"C:\OneUpDrive\Gone.txt"), None, true, here), None);
        assert_eq!(f.new_target(Path::new(r"C:\Windows\x.exe"), true), None, "only our folders");
        // A shared folder or a vault is synced but was never in the app's
        // folder: its links never went through the app's Explorer entry.
        let mut f = f;
        f.protected.push(PathBuf::from(r"C:\Shared"));
        assert_eq!(f.new_target(Path::new(r"C:\Shared\a.txt"), true), None);
    }

    #[test]
    fn the_app_folder_is_never_one_of_ours() {
        let ours = [PathBuf::from(r"C:\Users\Me\OneDrive")];
        let got = app_folders(vec![r"C:\Users\Me\OneDrive".into()], None, &ours);
        assert!(!got.iter().any(|g| is_within(g, &ours[0])), "moving over keeps the folder: {got:?}");
        let got = app_folders(Vec::new(), Some(Path::new(r"D:\Old OneDrive")), &ours);
        assert_eq!(got, vec![PathBuf::from(r"D:\Old OneDrive")]);
    }

    #[cfg(windows)]
    #[test]
    fn shortcuts_are_fixed_with_a_copy_and_undone() {
        let root = long_temp().join(format!("oneup-shortcuts-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&root);
        let (old, new, desk, data) =
            (root.join("OneDrive"), root.join("OneUpDrive"), root.join("Desktop"), root.join("data"));
        for d in [old.join("Docs"), new.join("Docs"), desk.join("Sub"), data.clone()] {
            std::fs::create_dir_all(d).unwrap();
        }
        std::fs::write(new.join("Docs").join("a.txt"), b"a").unwrap();
        let (a, gone, other) = (desk.join("a.lnk"), desk.join("Sub").join("gone.lnk"), desk.join("other.lnk"));
        oneup_sys::shortcut::create(&a, &old.join("Docs").join("a.txt"), Some(&old.join("Docs"))).unwrap();
        oneup_sys::shortcut::create(&gone, &old.join("Docs").join("gone.txt"), None).unwrap();
        oneup_sys::shortcut::create(&other, &data, None).unwrap();
        let before = std::fs::read(&a).unwrap();

        let f = fl([Mapping { from: old.clone(), to: new.clone() }]);
        let s = scan(&[(Place::Desktop, desk.clone())], &f).unwrap();
        assert_eq!(s.found.len(), 1, "{s:?}");
        assert_eq!(s.unmatched, 1, "the shortcut to an item not here stays");
        let found = &s.found[0];
        assert!(found.broken, "the old target was never there");
        assert_eq!(
            (found.link.as_path(), found.new_target.as_path()),
            (a.as_path(), new.join("Docs").join("a.txt").as_path())
        );

        let done = fix(&data, &s.found).unwrap();
        assert_eq!((done.changed, done.skipped.len()), (1, 0));
        let now = oneup_sys::shortcut::read(&a).unwrap();
        assert_eq!(now.target, Some(new.join("Docs").join("a.txt")));
        assert_eq!(now.working_dir, Some(new.join("Docs")));
        assert_eq!(last_fix(&data).map(|(_, n)| n), Some(1));
        assert!(scan(&[(Place::Desktop, desk.clone())], &f).unwrap().found.is_empty(), "nothing left to fix");

        let again = fix(&data, &s.found).unwrap();
        assert_eq!((again.changed, again.skipped.len()), (0, 1));

        let back = undo(&data).unwrap();
        assert_eq!((back.changed, back.skipped.len()), (1, 0));
        assert_eq!(std::fs::read(&a).unwrap(), before, "put back byte for byte");
        assert_eq!(last_fix(&data), None);
        assert!(undo(&data).is_err());
        std::fs::remove_dir_all(&root).unwrap();
    }

    #[cfg(windows)]
    #[test]
    fn undo_leaves_a_shortcut_changed_since() {
        let root = long_temp().join(format!("oneup-shortcuts-undo-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&root);
        let (old, new, desk, data) =
            (root.join("OneDrive"), root.join("OneUpDrive"), root.join("Desktop"), root.join("data"));
        for d in [&old, &new, &desk, &data] {
            std::fs::create_dir_all(d).unwrap();
        }
        std::fs::write(new.join("b.txt"), b"b").unwrap();
        let b = desk.join("b.lnk");
        oneup_sys::shortcut::create(&b, &old.join("b.txt"), None).unwrap();
        let f = fl([Mapping { from: old.clone(), to: new.clone() }]);
        let s = scan(&[(Place::Desktop, desk.clone())], &f).unwrap();
        assert_eq!(fix(&data, &s.found).unwrap().changed, 1);
        oneup_sys::shortcut::retarget(&b, &data, None).unwrap();
        let back = undo(&data).unwrap();
        assert_eq!((back.changed, back.skipped.len()), (0, 1));
        assert_eq!(oneup_sys::shortcut::read(&b).unwrap().target, Some(data.clone()));
        std::fs::remove_dir_all(&root).unwrap();
    }

    #[cfg(windows)]
    #[test]
    fn a_shortcut_the_shell_cant_follow_is_saved_again() {
        let root = long_temp().join(format!("oneup-shortcuts-dead-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&root);
        let (ours, menu, data) = (root.join("OneDrive"), root.join("Menu"), root.join("data"));
        for d in [ours.join("Tools"), menu.clone(), data.clone()] {
            std::fs::create_dir_all(d).unwrap();
        }
        let exe = ours.join("Tools").join("tool.exe");
        std::fs::write(&exe, b"x").unwrap();
        let lnk = menu.join("tool.lnk");
        oneup_sys::shortcut::create(&lnk, &exe, None).unwrap();
        // Its first item, a root folder by CLSID, made one nothing registers
        // (as `oneup_sys::shortcut`'s own test does).
        let mut b = std::fs::read(&lnk).unwrap();
        b[0x4C + 6..0x4C + 22].copy_from_slice(&[0xA5; 16]);
        std::fs::write(&lnk, &b).unwrap();
        assert_eq!(oneup_sys::shortcut::read(&lnk).unwrap().target, None, "the shell can't follow it");

        let f = Folders { ours: vec![ours.clone()], protected: vec![ours.clone()], ..Folders::default() };
        let s = scan(&[(Place::StartMenu, menu.clone())], &f).unwrap();
        assert_eq!(s.found.len(), 1, "{s:?}");
        assert!(s.found[0].broken);
        assert_eq!(s.found[0].new_target, exe);
        assert_eq!(fix(&data, &s.found).unwrap().changed, 1);
        assert_eq!(oneup_sys::shortcut::read(&lnk).unwrap().target, Some(exe.clone()));
        assert!(scan(&[(Place::StartMenu, menu)], &f).unwrap().found.is_empty());
        std::fs::remove_dir_all(&root).unwrap();
    }

    /// Old, new and Desktop folders and a data folder, in a temp folder.
    #[cfg(windows)]
    fn sandbox(name: &str) -> (PathBuf, PathBuf, PathBuf, PathBuf, PathBuf) {
        let root = long_temp().join(format!("oneup-shortcuts-{name}-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&root);
        let (old, new, desk, data) =
            (root.join("OneDrive"), root.join("OneUpDrive"), root.join("Desktop"), root.join("data"));
        for d in [&old, &new, &desk, &data] {
            std::fs::create_dir_all(d).unwrap();
        }
        (root, old, new, desk, data)
    }

    /// A folder we sync (a shared folder's or a vault's too) is never
    /// looked in: changing a shortcut there would change OneDrive.
    #[cfg(windows)]
    #[test]
    fn shortcuts_inside_our_folders_are_left_alone() {
        let (root, old, new, desk, _) = sandbox("ours");
        let shared = desk.join("Shared");
        std::fs::create_dir_all(&shared).unwrap();
        std::fs::write(new.join("a.txt"), b"a").unwrap();
        oneup_sys::shortcut::create(&shared.join("a.lnk"), &old.join("a.txt"), None).unwrap();
        let mut f = fl([Mapping { from: old.clone(), to: new.clone() }]);
        assert_eq!(scan(&[(Place::Desktop, desk.clone())], &f).unwrap().found.len(), 1);
        f.protected.push(shared);
        assert!(scan(&[(Place::Desktop, desk)], &f).unwrap().found.is_empty());
        std::fs::remove_dir_all(&root).unwrap();
    }

    /// A fix that stopped between saving a shortcut and noting it: undo
    /// puts back the one that was saved, and leaves one that wasn't.
    #[cfg(windows)]
    #[test]
    fn a_fix_stopped_around_saving_still_undoes() {
        let (root, old, new, desk, data) = sandbox("stopped");
        std::fs::write(new.join("a.txt"), b"a").unwrap();
        let a = desk.join("a.lnk");
        oneup_sys::shortcut::create(&a, &old.join("a.txt"), None).unwrap();
        let before = std::fs::read(&a).unwrap();
        let f = fl([Mapping { from: old.clone(), to: new.clone() }]);
        let s = scan(&[(Place::Desktop, desk.clone())], &f).unwrap();
        assert_eq!(fix(&data, &s.found).unwrap().changed, 1);
        let (batch, _, mut done) = last_batch(&data).unwrap().unwrap();
        // As if it stopped right after replacing it.
        done[0].left = None;
        write_list(&batch, &done).unwrap();
        assert_eq!(undo(&data).unwrap().changed, 1);
        assert_eq!(std::fs::read(&a).unwrap(), before);
        // As if it stopped right before replacing it: the shortcut is as it was.
        write_list(&batch, &done).unwrap();
        let o = undo(&data).unwrap();
        assert_eq!((o.changed, o.skipped.len()), (0, 1));
        assert_eq!(std::fs::read(&a).unwrap(), before);
        std::fs::remove_dir_all(&root).unwrap();
    }

    #[cfg(windows)]
    #[test]
    fn one_change_to_shortcuts_at_a_time() {
        let (root, _, _, _, data) = sandbox("lock");
        let held = lock(&data).unwrap();
        assert!(lock(&data).is_err(), "a second one waits its turn");
        drop(held);
        assert!(lock(&data).is_ok());
        assert_ne!(new_batch(&data).unwrap(), new_batch(&data).unwrap(), "never the same batch");
        std::fs::remove_dir_all(&root).unwrap();
    }

    /// Undo can't write a shortcut another program holds open: it stays in
    /// the batch, and the next undo puts it back.
    #[cfg(windows)]
    #[test]
    fn a_shortcut_undo_couldnt_write_is_put_back_next_time() {
        use std::os::windows::fs::OpenOptionsExt as _;
        let (root, old, new, desk, data) = sandbox("retry");
        std::fs::write(new.join("a.txt"), b"a").unwrap();
        let a = desk.join("a.lnk");
        oneup_sys::shortcut::create(&a, &old.join("a.txt"), None).unwrap();
        let before = std::fs::read(&a).unwrap();
        let f = fl([Mapping { from: old.clone(), to: new.clone() }]);
        let s = scan(&[(Place::Desktop, desk.clone())], &f).unwrap();
        assert_eq!(fix(&data, &s.found).unwrap().changed, 1);
        let held = std::fs::OpenOptions::new().read(true).share_mode(0).open(&a).unwrap();
        let o = undo(&data).unwrap();
        assert_eq!((o.changed, o.skipped.len()), (0, 1));
        drop(held);
        assert_eq!(last_fix(&data).map(|(_, n)| n), Some(1), "still to undo");
        assert_eq!(undo(&data).unwrap().changed, 1);
        assert_eq!(std::fs::read(&a).unwrap(), before);
        assert_eq!(last_fix(&data), None);
        std::fs::remove_dir_all(&root).unwrap();
    }

    /// A list with half a stamp is damaged: undo stops rather than guess.
    #[cfg(windows)]
    #[test]
    fn a_damaged_list_stops_undo() {
        let (root, old, new, desk, data) = sandbox("damaged");
        std::fs::write(new.join("a.txt"), b"a").unwrap();
        oneup_sys::shortcut::create(&desk.join("a.lnk"), &old.join("a.txt"), None).unwrap();
        let f = fl([Mapping { from: old.clone(), to: new.clone() }]);
        let s = scan(&[(Place::Desktop, desk.clone())], &f).unwrap();
        assert_eq!(fix(&data, &s.found).unwrap().changed, 1);
        let (batch, _, _) = last_batch(&data).unwrap().unwrap();
        let list = batch.join(LIST);
        let mut v: Value = serde_json::from_slice(&std::fs::read(&list).unwrap()).unwrap();
        v["fixed"][0]["size"] = Value::Null;
        std::fs::write(&list, serde_json::to_vec(&v).unwrap()).unwrap();
        assert!(undo(&data).unwrap_err().to_string().contains("damaged"));
        std::fs::remove_dir_all(&root).unwrap();
    }

    /// Another program rewrites the shortcut between the search and the
    /// fix, at the same size and with its old time: its contents tell.
    #[cfg(windows)]
    #[test]
    fn a_shortcut_rewritten_at_the_same_size_and_time_is_left_alone() {
        let (root, old, new, desk, data) = sandbox("rewritten");
        std::fs::write(new.join("a.txt"), b"a").unwrap();
        let a = desk.join("a.lnk");
        oneup_sys::shortcut::create(&a, &old.join("a.txt"), None).unwrap();
        let f = fl([Mapping { from: old.clone(), to: new.clone() }]);
        let s = scan(&[(Place::Desktop, desk.clone())], &f).unwrap();
        let when = std::fs::metadata(&a).unwrap().modified().unwrap();
        let mut b = std::fs::read(&a).unwrap();
        let last = b.len() - 1;
        b[last] ^= 0x01;
        std::fs::write(&a, &b).unwrap();
        std::fs::File::options().write(true).open(&a).unwrap().set_modified(when).unwrap();
        let o = fix(&data, &s.found).unwrap();
        assert_eq!((o.changed, o.skipped.len()), (0, 1));
        assert_eq!(std::fs::read(&a).unwrap(), b, "the other program's version stays");
        std::fs::remove_dir_all(&root).unwrap();
    }

    /// A shortcut the person deletes while it's being fixed stays deleted,
    /// and nothing is left behind.
    #[cfg(windows)]
    #[test]
    fn a_shortcut_deleted_meanwhile_stays_deleted() {
        let (root, _, _, desk, _) = sandbox("replace");
        let a = desk.join("a.lnk");
        std::fs::write(&a, b"old").unwrap();
        let r = replace_link(&a, &fingerprint(b"old"), |tmp| {
            std::fs::write(tmp, b"new")?;
            std::fs::remove_file(&a)?;
            Ok(())
        });
        assert!(r.is_err());
        assert!(!a.exists());
        assert_eq!(std::fs::read_dir(&desk).unwrap().count(), 0, "no temporary file left");
        std::fs::remove_dir_all(&root).unwrap();
    }

    /// What a shortcut should open, gone between the search and the fix:
    /// the shortcut stays as it was.
    #[cfg(windows)]
    #[test]
    fn a_target_gone_since_the_search_is_left_alone() {
        let (root, old, new, desk, data) = sandbox("gone");
        std::fs::write(new.join("a.txt"), b"a").unwrap();
        let a = desk.join("a.lnk");
        oneup_sys::shortcut::create(&a, &old.join("a.txt"), None).unwrap();
        let before = std::fs::read(&a).unwrap();
        let f = fl([Mapping { from: old.clone(), to: new.clone() }]);
        let s = scan(&[(Place::Desktop, desk.clone())], &f).unwrap();
        std::fs::remove_file(new.join("a.txt")).unwrap();
        let o = fix(&data, &s.found).unwrap();
        assert_eq!((o.changed, o.skipped.len()), (0, 1));
        assert_eq!(std::fs::read(&a).unwrap(), before);
        std::fs::remove_dir_all(&root).unwrap();
    }

    #[test]
    fn strict_listings_are_empty_only_when_there_is_nothing() {
        let dir = long_temp().join(format!("oneup-shortcuts-lists-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        assert!(crate::pairs::list_strict(&dir).unwrap().is_empty(), "no pairs folder");
        assert!(crate::vaultpair::list_strict(&dir).unwrap().is_empty(), "no vaults folder");
        std::fs::remove_dir_all(&dir).unwrap();
    }
}
