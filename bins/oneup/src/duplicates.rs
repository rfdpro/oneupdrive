//! Duplicates: files with the same content (`oneup duplicates`, the
//! settings window's Duplicates view), and removing the copies the user
//! chooses.
//!
//! **Listing** reads the groups a page at a time from the cloud files by
//! size index and the local-only files hashed on request
//! (`oneup_store::dupes`), most space saved first. Each copy says where it is
//! (here and in OneDrive, in OneDrive, on this PC only) and is labelled when
//! a removal there deserves a second look: in a vault's or another folder
//! pair's OneDrive folder (never counted as the copy that stays, since its
//! own agent may remove it), outside the synced folder, or matched by an
//! exclusion rule. A group repeats its copies' labels, so nothing across
//! those lines is ever suggested without saying so. **Excluded folders'
//! contents aren't checked:** neither tree stores them ([`EXCLUDED_NOTE`]).
//!
//! **Removing** follows `crate::resolve`: a preview ([`prepare`]) lists
//! exactly what goes, by the store's paths, and the code of that list; [`ask`]
//! queues the choice in the store's `meta` ([`KEY`]) only while the code is
//! still the same, and nudges the agent. Each round works it out again
//! against the state then ([`for_round`], `oneup_exec::dupes::plan`: a copy
//! changed since is skipped, the last copy of a content never goes) and
//! drops it if its code is no longer the preview's; otherwise its operations
//! join the round, where the safety gate holds them as a stop of their own
//! until the user approves its code, whatever the circuit breakers' limits. Once a round ran them, the choice
//! leaves the queue ([`ran`]); each operation's outcome is in the activity
//! log like any other, and so is each copy that was left. One choice waits
//! at a time; a new one replaces it ([`cancel`] drops it).
//!
//! **Hashing local-only files** ([`hash_local`]) happens only when asked:
//! files of 1 MiB or more in the synced folder that aren't in OneDrive,
//! ordinary files only (`oneup_local::verify`, never a placeholder), a
//! bounded number per run, resumable from where the last run stopped, and
//! stopping when sync is paused.

use std::path::{Path, PathBuf};

use anyhow::{Result, bail};
use oneup_exec::dupes::{Chosen, CopyId, Kept, Plan};
use oneup_store::{ActivityEntry, ActivityOutcome, COPY_MIN, DupCursor, DupGroup, DupSummary, Ops, Store};
use oneup_sync::{LocalId, Op, RemoteId};
use serde_json::{Value, json};

use crate::t_key;

/// Said wherever duplicates are listed.
pub const EXCLUDED_NOTE: &str = t_key!("Excluded folders' contents aren't checked for duplicates.");

/// In `meta`: the choice waiting for the agent (JSON).
pub const KEY: &str = "duplicates.remove";
/// In `meta`: where the last [`hash_local`] run stopped (a local file ID).
pub const KEY_HASH_CURSOR: &str = "duplicates.hash_cursor";
/// At most this many copies in one choice.
pub const MAX: usize = 1_000;
/// At most this many copies listed per group.
pub const COPIES_SHOWN: usize = 50;

/// Where a copy is.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Place {
    /// Synced: here and in OneDrive.
    Both,
    /// In OneDrive, not on this PC.
    Cloud,
    /// On this PC only.
    Here,
}

impl Place {
    pub fn code(self) -> &'static str {
        match self {
            Place::Both => "both",
            Place::Cloud => "cloud",
            Place::Here => "here",
        }
    }

    /// In English; the window says it in the reader's language.
    pub fn label(self) -> &'static str {
        match self {
            Place::Both => t_key!("here and in OneDrive"),
            Place::Cloud => t_key!("in OneDrive"),
            Place::Here => t_key!("on this PC only"),
        }
    }
}

/// Why a removal of a copy deserves a second look: a message (`key`, with
/// `name` for `{name}`), said in English by `oneup duplicates` and its
/// `--json`, and in the reader's language by the window.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Label {
    pub key: &'static str,
    pub name: Option<String>,
}

impl Label {
    fn fixed(key: &'static str) -> Self {
        Label { key, name: None }
    }

    pub fn english(&self) -> String {
        match &self.name {
            Some(n) => self.key.replace("{name}", n),
            None => self.key.to_string(),
        }
    }

    fn json(&self) -> Value {
        json!({ "key": self.key, "name": self.name })
    }
}

fn english(labels: &[Label]) -> Vec<String> {
    labels.iter().map(Label::english).collect()
}

/// One copy of a group.
#[derive(Debug, Clone)]
pub struct Copy {
    /// Its path in OneDrive, or on this PC for a file only here.
    pub path: String,
    pub place: Place,
    /// Why a removal there deserves a second look (see the module docs).
    pub labels: Vec<Label>,
    pub chosen: Chosen,
}

/// Content more than one file has, and its copies.
#[derive(Debug, Clone)]
pub struct Group {
    pub group: DupGroup,
    /// The first [`COPIES_SHOWN`].
    pub copies: Vec<Copy>,
    /// Its copies' labels, each once.
    pub labels: Vec<Label>,
}

/// One page of groups.
#[derive(Debug, Clone, Default)]
pub struct Page {
    pub summary: DupSummary,
    pub groups: Vec<Group>,
    /// Where the next page starts, if there is one.
    pub next: Option<String>,
}

/// What the labels need to know about the data folder.
struct Places {
    /// The synced OneDrive folder, and the folder on this PC.
    top: Option<String>,
    root: Option<PathBuf>,
    rules: Option<oneup_rules::RuleSet>,
    /// OneDrive folders synced on their own (vaults, folder pairs): label
    /// and folder.
    separate: Vec<(Label, String)>,
}

/// `path` relative to OneDrive folder `folder` (`/` itself), if it's in it.
fn under<'a>(path: &'a str, folder: &str) -> Option<&'a str> {
    let f = folder.trim_end_matches('/');
    if f.is_empty() {
        return Some(path);
    }
    oneup_sync::strip_folded_prefix(path, f).filter(|rest| rest.is_empty() || rest.starts_with('/'))
}

impl Places {
    fn load(dir: &Path, store: &Store) -> Result<Self> {
        let top = store.read(|o| o.meta_get(crate::sync::KEY_REMOTE))?;
        let root = store.read(|o| o.meta_get(crate::sync::KEY_LOCAL))?.map(PathBuf::from);
        let rules = match top {
            Some(_) => Some(crate::rulefiles::load(dir, store, &mut crate::rulefiles::Cache::default())?.0),
            None => None,
        };
        let mut separate = Vec::new();
        for v in crate::vaultpair::list(dir) {
            separate.push((Label { key: t_key!("in vault {name}"), name: Some(v.name) }, v.remote));
        }
        for p in crate::pairs::list(dir) {
            separate.push((Label { key: t_key!("in folder pair {name}"), name: Some(p.name) }, p.remote));
        }
        Ok(Self { top, root, rules, separate })
    }

    /// A cloud file at OneDrive path `path`; `synced`: linked to a file here.
    fn cloud(&self, path: &str, synced: bool) -> (Place, Vec<Label>) {
        let mut labels: Vec<Label> =
            self.separate.iter().filter(|(_, f)| under(path, f).is_some()).map(|(l, _)| l.clone()).collect();
        match self.top.as_deref().and_then(|t| under(path, t)) {
            // The rules as they are now, synced or not: a synced file a
            // rule now covers is still here until it's pruned or unticked.
            Some(rel) if self.rules.as_ref().is_some_and(|r| r.excludes(rel, false)) => {
                labels.push(Label::fixed(t_key!("excluded")));
            }
            Some(_) => {}
            None => labels.push(Label::fixed(t_key!("outside the synced folder"))),
        }
        (if synced { Place::Both } else { Place::Cloud }, labels)
    }

    /// A file only here, at `rel` in the synced folder: its full path.
    fn here(&self, rel: &str) -> String {
        let full = match &self.root {
            Some(r) => rel.split('/').filter(|p| !p.is_empty()).fold(r.clone(), |p, x| p.join(x)),
            None => PathBuf::from(rel),
        };
        let s = full.to_string_lossy().into_owned();
        s.strip_prefix(r"\\?\").map(str::to_string).unwrap_or(s)
    }
}

fn hex(b: &[u8]) -> String {
    b.iter().map(|x| format!("{x:02x}")).collect()
}

fn unhex<const N: usize>(s: &str) -> Option<[u8; N]> {
    if s.len() != N * 2 || !s.is_ascii() {
        return None;
    }
    let mut out = [0u8; N];
    for (i, b) in out.iter_mut().enumerate() {
        *b = u8::from_str_radix(&s[i * 2..i * 2 + 2], 16).ok()?;
    }
    Some(out)
}

/// A page cursor as text (`oneup duplicates --after`).
pub fn cursor_text(c: &DupCursor) -> String {
    format!("{}.{}.{}", c.saved, c.size, hex(&c.hash))
}

pub fn parse_cursor(s: &str) -> Result<DupCursor> {
    let mut parts = s.trim().split('.');
    let mut next = || parts.next().ok_or_else(|| anyhow::anyhow!("not a cursor `oneup duplicates` printed: {s}"));
    let (saved, size, hash) = (next()?, next()?, next()?);
    let bad = || anyhow::anyhow!("not a cursor `oneup duplicates` printed: {s}");
    Ok(DupCursor {
        saved: saved.parse().map_err(|_| bad())?,
        size: size.parse().map_err(|_| bad())?,
        hash: unhex::<20>(hash).ok_or_else(bad)?,
    })
}

/// The copies of `g` (the first [`COPIES_SHOWN`]).
fn copies_of(o: &Ops<'_>, places: &Places, g: &DupGroup) -> oneup_store::Result<Vec<Copy>> {
    let mut out = Vec::new();
    for r in o.dup_cloud_copies(g.size, &g.hash, COPIES_SHOWN)? {
        let path = o.path_of(&r.id)?.unwrap_or_else(|| r.name.clone());
        let synced = o.synced_by_remote(&r.id)?.is_some_and(|b| b.local.is_some());
        let (place, labels) = places.cloud(&path, synced);
        let copy = CopyId::Cloud { id: RemoteId(r.id), etag: r.etag.unwrap_or_default() };
        out.push(Copy { path, place, labels, chosen: Chosen { copy, size: g.size, hash: g.hash } });
    }
    for l in o.dup_local_copies(g.size, &g.hash, COPIES_SHOWN.saturating_sub(out.len()))? {
        let rel = o.local_path_of(l.file_id)?.unwrap_or_else(|| l.name.clone());
        let copy = CopyId::Local { id: LocalId(l.file_id), size: l.size, mtime: l.mtime };
        out.push(Copy {
            path: places.here(&rel),
            place: Place::Here,
            labels: Vec::new(),
            chosen: Chosen { copy, size: g.size, hash: g.hash },
        });
    }
    Ok(out)
}

/// Up to `limit` groups after `after`, most space saved first, with the
/// summary of all of them.
pub fn list(dir: &Path, store: &Store, after: Option<DupCursor>, limit: usize) -> Result<Page> {
    let places = Places::load(dir, store)?;
    let page = store.read(|o| {
        let summary = o.dup_summary()?;
        let mut found = o.dup_groups(after, limit + 1)?;
        let more = found.len() > limit;
        found.truncate(limit);
        let next = if more { found.last().map(|g| cursor_text(&g.cursor())) } else { None };
        let mut groups = Vec::with_capacity(found.len());
        for g in found {
            let copies = copies_of(o, &places, &g)?;
            let mut labels: Vec<Label> = Vec::new();
            for l in copies.iter().flat_map(|c| &c.labels) {
                if !labels.contains(l) {
                    labels.push(l.clone());
                }
            }
            groups.push(Group { group: g, copies, labels });
        }
        Ok(Page { summary, groups, next })
    })?;
    Ok(page)
}

/// How a copy is named between the window and here: exactly what it was
/// when listed.
pub fn key(c: &Chosen) -> Value {
    let (size, hash) = (c.size, hex(&c.hash));
    match &c.copy {
        CopyId::Cloud { id, etag } => json!({ "cloud": id.0, "etag": etag, "size": size, "hash": hash }),
        CopyId::Local { id, size: s, mtime } => {
            json!({ "local": format!("{:x}", id.0), "file_size": s, "mtime": mtime, "size": size, "hash": hash })
        }
    }
}

pub fn parse_key(v: &Value) -> Option<Chosen> {
    let size = v["size"].as_i64()?;
    let hash = unhex::<20>(v["hash"].as_str()?)?;
    let copy = if let Some(id) = v["cloud"].as_str() {
        CopyId::Cloud { id: RemoteId(id.to_string()), etag: v["etag"].as_str()?.to_string() }
    } else {
        let id = u128::from_str_radix(v["local"].as_str()?, 16).ok()?;
        CopyId::Local { id: LocalId(id), size: v["file_size"].as_i64()?, mtime: v["mtime"].as_i64()? }
    };
    Some(Chosen { copy, size, hash })
}

/// [`list`] as the window and `--json` show it.
pub fn page_json(p: &Page) -> Value {
    let groups: Vec<Value> = p
        .groups
        .iter()
        .map(|g| {
            let copies: Vec<Value> = g
                .copies
                .iter()
                .map(|c| {
                    json!({
                        "path": c.path,
                        "place": c.place.code(),
                        "where": c.place.label(),
                        "labels": english(&c.labels),
                        "label_keys": c.labels.iter().map(Label::json).collect::<Vec<_>>(),
                        "key": key(&c.chosen),
                    })
                })
                .collect();
            json!({
                "size": g.group.size,
                "copies": g.group.copies,
                "saved": g.group.saved(),
                "labels": english(&g.labels),
                "label_keys": g.labels.iter().map(Label::json).collect::<Vec<_>>(),
                "list": copies,
            })
        })
        .collect();
    json!({
        "groups": p.summary.groups,
        "files": p.summary.files,
        "bytes": p.summary.bytes,
        "page": groups,
        "next": p.next,
        "note": EXCLUDED_NOTE,
        "excluded_folders_checked": false,
    })
}

/// The copy at `path` as `oneup duplicates` lists it: an OneDrive path
/// (`/Photos/a.jpg`), or a full path on this PC for a file only here.
pub fn find(dir: &Path, store: &Store, path: &str) -> Result<Option<(String, Chosen)>> {
    let places = Places::load(dir, store)?;
    if Path::new(path).is_absolute() {
        let Some(root) = &places.root else { return Ok(None) };
        let Some(rel) = rel_in(root, path) else { return Ok(None) };
        let Some(id) = store.read(|o| crate::resolve::local_at(o, &rel))? else { return Ok(None) };
        let found = store.read(|o| {
            let Some(l) = o.local_get(id.0)? else { return Ok(None) };
            let Some(hash) = o.local_only_hash_get(l.file_id, l.size, l.mtime)? else { return Ok(None) };
            if o.synced_by_local(l.file_id)?.is_some() {
                return Ok(None);
            }
            let copy = CopyId::Local { id, size: l.size, mtime: l.mtime };
            Ok(Some((places.here(&rel), Chosen { copy, size: l.size, hash })))
        })?;
        return Ok(found);
    }
    let parts: Vec<&str> = path.split(['/', '\\']).filter(|p| !p.is_empty()).collect();
    store
        .read(|o| {
            let Some(mut cur) = o.remote_root()? else { return Ok(None) };
            for part in &parts {
                let found = o.remote_named(&cur.id, part)?;
                let exact = found.iter().position(|r| r.name == *part);
                cur = match (exact, found.len()) {
                    (Some(i), _) => found[i].clone(),
                    (None, 1) => found[0].clone(),
                    _ => return Ok(None),
                };
            }
            if cur.kind != oneup_store::Kind::File || cur.size < COPY_MIN {
                return Ok(None);
            }
            let (Some(hash), Some(etag)) = (cur.qxh, cur.etag.clone()) else { return Ok(None) };
            let shown = o.path_of(&cur.id)?.unwrap_or_else(|| path.to_string());
            let copy = CopyId::Cloud { id: RemoteId(cur.id), etag };
            Ok(Some((shown, Chosen { copy, size: cur.size, hash })))
        })
        .map_err(Into::into)
}

/// `path` (a full path) relative to the synced folder `root` (`/a/b.txt`),
/// if it's in it, names compared as Windows compares them.
fn rel_in(root: &Path, path: &str) -> Option<String> {
    use std::path::Component;
    let plain = |s: &Path| oneup_local::coexist::plain(s).to_string_lossy().into_owned();
    let (root, p) = (plain(root), plain(Path::new(path)));
    let folded = |c: Component<'_>| oneup_sync::fold(&c.as_os_str().to_string_lossy());
    let mut parts = Path::new(&p).components().filter(|c| !matches!(c, Component::CurDir));
    for r in Path::new(&root).components().filter(|c| !matches!(c, Component::CurDir)) {
        match parts.next() {
            Some(c) if folded(c) == folded(r) => {}
            _ => return None,
        }
    }
    let rest: Vec<String> = parts.map(|c| c.as_os_str().to_string_lossy().into_owned()).collect();
    Some(format!("/{}", rest.join("/")))
}

/// One line of a preview.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Line {
    pub path: String,
    pub size: i64,
    /// Where it goes.
    pub to: &'static str,
}

/// What removing a choice would do now ([`prepare`]).
#[derive(Debug, Clone, Default)]
pub struct Prepared {
    pub plan: Plan,
    /// The copies that go.
    pub lines: Vec<Line>,
    /// The copies left, and why.
    pub left: Vec<(String, String)>,
    /// What going frees.
    pub bytes: i64,
    /// Of exactly that batch: its safety stop's code if nothing changes.
    pub code: String,
}

/// Where removing a copy sends it.
fn goes_to(op: &Op) -> &'static str {
    match op {
        Op::RemoveCopy { remote: Some(_), local: Some(_), .. } => {
            t_key!("OneDrive's recycle bin; the copy here to the version store")
        }
        Op::RemoveCopy { remote: Some(_), .. } => t_key!("OneDrive's recycle bin"),
        _ => t_key!("the version store (.oneup\\versions)"),
    }
}

/// The code `plan`'s safety stop shows if nothing changes meanwhile: each
/// operation keyed with the path its list shows (`crate::sync`), read with
/// the plan, from the same snapshot.
pub fn code(o: &Ops<'_>, plan: &Plan) -> oneup_store::Result<String> {
    let paths = plan.ops.iter().map(|op| crate::sync::removal_path(o, op)).collect::<oneup_store::Result<Vec<_>>>()?;
    Ok(plan.code(&paths))
}

/// The OneDrive folders synced on their own (vaults, other folder pairs):
/// their copies are listed, but never count as the copy that stays.
pub fn separate(dir: &Path) -> Vec<String> {
    let vaults = crate::vaultpair::list(dir).into_iter().map(|v| v.remote);
    vaults.chain(crate::pairs::list(dir).into_iter().map(|p| p.remote)).collect()
}

/// Where a chosen copy is now, as the store has it (never as a caller
/// named it): its OneDrive path, or its path here for a file only here.
fn copy_path(o: &Ops<'_>, c: &Chosen) -> oneup_store::Result<String> {
    Ok(match &c.copy {
        CopyId::Cloud { id, .. } => o.path_of(&id.0)?,
        CopyId::Local { id, .. } => o.local_path_of(id.0)?,
    }
    .unwrap_or_else(|| "(gone)".into()))
}

/// Works out removing `chosen`, changing nothing. Every path it shows is
/// the store's, the same its code is made of.
/// The plan, its lines and its code are read in one transaction, from one
/// snapshot of the store: what the preview lists is what its code covers.
pub fn prepare(dir: &Path, store: &mut Store, chosen: &[Chosen]) -> Result<Prepared> {
    if chosen.len() > MAX {
        bail!("choose at most {MAX} copies at a time");
    }
    let separate = separate(dir);
    Ok(store.snapshot(|o| {
        let plan = oneup_exec::dupes::plan(o, chosen, &separate)?;
        let mut out = Prepared { code: code(o, &plan)?, ..Prepared::default() };
        for (i, ops) in &plan.removed {
            for op in ops {
                let c = &chosen[*i];
                out.bytes += c.size;
                out.lines.push(Line { path: crate::sync::removal_path(o, op)?, size: c.size, to: goes_to(op) });
            }
        }
        for (i, why) in &plan.kept {
            out.left.push((copy_path(o, &chosen[*i])?, kept_text(*why).to_string()));
        }
        out.plan = plan;
        Ok(out)
    })?)
}

/// The choice waiting ([`queued`]).
#[derive(Debug, Clone)]
pub struct Waiting {
    pub copies: Vec<Chosen>,
    /// The code of the preview it was queued from: carried out only while
    /// it's still exactly that.
    pub code: String,
}

/// The choice waiting, if any (unreadable counts as none: the user can
/// choose again).
pub fn queued(o: &Ops<'_>) -> oneup_store::Result<Option<Waiting>> {
    let Some(v) = o.meta_get(KEY)?.and_then(|s| serde_json::from_str::<Value>(&s).ok()) else { return Ok(None) };
    let copies: Vec<Chosen> = v["copies"].as_array().into_iter().flatten().filter_map(parse_key).collect();
    let code = v["code"].as_str().unwrap_or_default().to_string();
    Ok((!copies.is_empty()).then_some(Waiting { copies, code }))
}

/// Queues removing `chosen`, if it still does exactly what the preview
/// with code `expect` showed. Replaces a choice waiting. Refused in a
/// backup pair, which never deletes in OneDrive nor changes this PC, and
/// while a chosen copy hasn't finished syncing.
pub fn ask(dir: &Path, store: &mut Store, chosen: &[Chosen], expect: &str) -> Result<Prepared> {
    if crate::flow::get(store)? == crate::flow::Flow::Backup {
        bail!("this pair is a backup: it never deletes in OneDrive or changes this PC");
    }
    let p = prepare(dir, store, chosen)?;
    if p.code != expect.trim().to_ascii_lowercase() {
        bail!("what this removes changed since it was shown; look again");
    }
    if p.plan.wait {
        bail!("a copy hasn't finished syncing; preview again once it has");
    }
    if p.lines.is_empty() {
        bail!("nothing to remove");
    }
    let copies: Vec<Value> = chosen.iter().map(key).collect();
    let v = json!({ "code": p.code, "copies": copies });
    store.write(|o| o.meta_set(KEY, &v.to_string()))?;
    Ok(p)
}

/// Drops the choice waiting, if any. Returns whether there was one.
pub fn cancel(store: &mut Store) -> Result<bool> {
    Ok(store.write(|o| {
        let had = o.meta_get(KEY)?.is_some();
        o.meta_delete(KEY)?;
        Ok(had)
    })?)
}

fn entry(outcome: ActivityOutcome, path: &str, detail: String) -> ActivityEntry {
    ActivityEntry {
        seq: 0,
        at_ms: crate::control::now_ms(),
        outcome,
        action: "duplicates".into(),
        path: path.into(),
        detail,
    }
}

/// A choice a round carries ([`for_round`]).
#[derive(Debug, Clone, Default)]
pub struct Round {
    pub plan: Plan,
    pub copies: Vec<Chosen>,
}

/// At the start of a round: the choice waiting, worked out against the
/// state now, for the round to add (`None`: nothing to add now). It waits
/// while either side is paused, and while a synced copy is on its way to
/// another state (the planner carries that first). Anything it would do
/// other than what the preview showed (its code differs: a copy changed, or
/// one or a folder above it moved) leaves the queue, logged: the user
/// chooses again. So does a choice in a backup pair.
pub fn for_round(
    dir: &Path,
    store: &mut Store,
    backup: bool,
    paused: bool,
) -> Result<(Option<Round>, Vec<ActivityEntry>)> {
    let Some(Waiting { copies, code: shown }) = store.read(queued)? else { return Ok((None, Vec::new())) };
    if backup {
        store.write(|o| o.meta_delete(KEY))?;
        let why = "not done; this pair is a backup: it never deletes in OneDrive or changes this PC";
        return Ok((None, vec![entry(ActivityOutcome::Failed, "", why.into())]));
    }
    if paused {
        return Ok((None, Vec::new()));
    }
    let separate = separate(dir);
    let (plan, now) = store.snapshot(|o| {
        let plan = oneup_exec::dupes::plan(o, &copies, &separate)?;
        let now = code(o, &plan)?;
        Ok((plan, now))
    })?;
    if plan.wait {
        return Ok((None, Vec::new()));
    }
    if now != shown || plan.removed.is_empty() {
        store.write(|o| o.meta_delete(KEY))?;
        let why = "not done: what it removes changed since you chose; choose again";
        return Ok((None, vec![entry(ActivityOutcome::Failed, "", why.into())]));
    }
    Ok((Some(Round { plan, copies }), Vec::new()))
}

/// The copies a choice leaves, for the activity log.
fn left(o: &Ops<'_>, copies: &[Chosen], plan: &Plan) -> oneup_store::Result<Vec<ActivityEntry>> {
    plan.kept
        .iter()
        .map(|(i, why)| Ok(entry(ActivityOutcome::Skipped, &copy_path(o, &copies[*i])?, format!("not removed: {why}"))))
        .collect()
}

/// After a round: once it ran any of the choice's operations (done,
/// skipped or failed: the round's own log says which), the choice leaves
/// the queue and the copies it left are logged. Held for approval, put off
/// (the round touched one of its copies), or not run (paused meanwhile),
/// it stays.
pub fn ran(store: &mut Store, round: &Round, report: &oneup_exec::Report) -> Result<Vec<ActivityEntry>> {
    let ran = |op: &Op| {
        report.done.contains(op)
            || report.skipped.iter().any(|(o, _)| o == op)
            || report.failed.iter().any(|(o, _)| o == op)
    };
    if !round.plan.ops.iter().any(ran) {
        return Ok(Vec::new());
    }
    let log = store.read(|o| left(o, &round.copies, &round.plan))?;
    store.write(|o| o.meta_delete(KEY))?;
    Ok(log)
}

/// What approving removal stop `code` removes, for the tray's question:
/// the paths on its saved list (exactly what its code approves), the first
/// `shown` of them. `None` while the list isn't saved yet: nothing is
/// approved unseen.
pub fn approval_text(dir: &Path, code: &str, shown: usize) -> Option<String> {
    let l = crate::stoplist::read(dir, code, "", shown)?;
    let mut text = String::from("Remove these duplicates?\n");
    for r in &l.rows {
        text.push_str(&format!("\n{}", r.path));
    }
    let more = (l.matching + l.unwritten).saturating_sub(l.rows.len());
    if more > 0 {
        text.push_str(&format!("\nand {more} more (Show the files lists them)"));
    }
    text.push_str("\n\nCopies in OneDrive go to its recycle bin, copies on this PC to the version store.");
    Some(text)
}

/// Why a chosen copy is left, in English; the window says it in the
/// reader's language.
pub fn kept_text(kept: Kept) -> &'static str {
    match kept {
        Kept::Gone => t_key!("it's gone"),
        Kept::Changed => t_key!("it changed since you chose"),
        Kept::NotInSync => t_key!("it hasn't finished syncing"),
        Kept::LastCopy => t_key!("it's the last copy here"),
        Kept::Twice => t_key!("it's chosen twice"),
    }
}

/// Whether `kept` means the copy changed after it was chosen.
pub fn changed(kept: Kept) -> bool {
    matches!(kept, Kept::Changed | Kept::Gone)
}

/// What a [`hash_local`] run did.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct Hashed {
    pub files: usize,
    pub bytes: u64,
    /// Couldn't be read (open in another program, access denied, not an
    /// ordinary file), or changed while read.
    pub unreadable: usize,
    /// Every file was looked at: the next run starts over.
    pub finished: bool,
    /// Stopped early: sync is paused, or the caller stopped it.
    pub stopped: bool,
    /// The store keeps no more local-only hashes
    /// (`oneup_store::dupes::LOCAL_ONLY_MAX`).
    pub full: bool,
}

/// Hashes files in the synced folder that aren't in OneDrive (1 MiB or
/// more, not hashed yet), at most `max_files` and about `max_bytes` per run,
/// continuing from where the last run stopped. `stop` is asked before each
/// file (sync paused, Ctrl+C). Reads ordinary files only.
pub fn hash_local(store: &mut Store, max_files: usize, max_bytes: u64, stop: &dyn Fn() -> bool) -> Result<Hashed> {
    let Some(root) = store.read(|o| o.local_root())? else { bail!("this data folder doesn't sync a folder yet") };
    let mut verifier = oneup_local::verify::Verifier::open(&root, 1 << 20)?;
    let mut cursor: u128 =
        store.read(|o| o.meta_get(KEY_HASH_CURSOR))?.and_then(|s| u128::from_str_radix(&s, 16).ok()).unwrap_or(0);
    if cursor == 0 {
        // A new pass: what no longer stands goes first.
        store.write(|o| o.local_only_hash_trim())?;
    }
    let mut out = Hashed::default();
    'pages: loop {
        let page = store.read(|o| o.local_files_from(cursor, COPY_MIN, 256))?;
        if page.is_empty() {
            out.finished = true;
            break;
        }
        for row in page {
            let wanted = row.skip == oneup_store::Skip::None
                && !row.name_lossy
                && store.read(|o| {
                    Ok(o.synced_by_local(row.file_id)?.is_none()
                        && o.local_only_hash_get(row.file_id, row.size, row.mtime)?.is_none())
                })?;
            if wanted {
                if stop() {
                    out.stopped = true;
                    break 'pages;
                }
                if out.files >= max_files || out.bytes >= max_bytes {
                    break 'pages;
                }
                match verifier.hash(&row) {
                    oneup_local::verify::Checked::Hash(h) => {
                        if !store.write(|o| o.local_only_hash_put(row.file_id, row.size, row.mtime, &h))? {
                            out.full = true;
                            break 'pages;
                        }
                        out.files += 1;
                        out.bytes += u64::try_from(row.size).unwrap_or(0);
                    }
                    _ => out.unreadable += 1,
                }
            }
            cursor = row.file_id;
        }
        store.write(|o| o.meta_set(KEY_HASH_CURSOR, &format!("{cursor:x}")))?;
    }
    store.write(|o| {
        if out.finished || out.full {
            o.meta_delete(KEY_HASH_CURSOR)?;
        } else {
            o.meta_set(KEY_HASH_CURSOR, &format!("{cursor:x}"))?;
        }
        if out.files > 0 {
            let size = crate::versions::size(out.bytes);
            let detail = format!("hashed {} files only on this PC ({size}) to compare them", out.files);
            o.activity_add(&[entry(ActivityOutcome::Note, "", detail)])?;
        }
        Ok(())
    })?;
    Ok(out)
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used)]
    use super::*;
    use oneup_store::{Kind, LocalKind, LocalRow, RemoteRow, Skip};

    const MIB: i64 = 1 << 20;

    fn row(id: &str, parent: &str, name: &str, kind: Kind, h: u8) -> RemoteRow {
        RemoteRow {
            id: id.into(),
            parent_id: (!parent.is_empty()).then(|| parent.into()),
            name: name.into(),
            kind,
            size: if kind == Kind::File { 3 * MIB } else { 0 },
            etag: Some(format!("e-{id}")),
            qxh: (kind == Kind::File).then_some([h; 20]),
            mtime_ms: None,
        }
    }

    /// The drive: `/Sync` is synced (rules exclude `*.iso`), `/Vault` is a
    /// vault's folder, `/Other` is outside both. Three copies of one content.
    fn drive() -> (std::path::PathBuf, Store) {
        let dir =
            std::env::temp_dir().join(format!("oneup-dupes-{}-{:?}", std::process::id(), std::thread::current().id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        std::fs::write(dir.join("config.toml"), "[rules]\nexclude = [\"*.iso\"]\n").unwrap();
        let mut s = Store::open_in_memory().unwrap();
        s.write(|o| {
            o.meta_set(crate::sync::KEY_REMOTE, "/Sync")?;
            o.meta_set(crate::sync::KEY_LOCAL, r"C:\Sync")?;
            o.upsert_item(&row("root", "", "", Kind::Root, 0), false)?;
            o.upsert_item(&row("sync", "root", "Sync", Kind::Folder, 0), false)?;
            o.upsert_item(&row("other", "root", "Other", Kind::Folder, 0), false)?;
            o.upsert_item(&row("a", "sync", "a.iso", Kind::File, 1), false)?;
            o.upsert_item(&row("b", "other", "b.iso", Kind::File, 1), false)?;
            o.upsert_item(&row("c", "sync", "c.bin", Kind::File, 1), false)?;
            o.upsert_item(&row("d", "sync", "d.bin", Kind::File, 2), false)?;
            o.local_put(&LocalRow {
                file_id: 1,
                parent_id: None,
                name: String::new(),
                name_lossy: false,
                kind: LocalKind::Root,
                size: 0,
                mtime: 0,
                attributes: 0,
                reparse_tag: 0,
                skip: Skip::None,
            })
        })
        .unwrap();
        (dir, s)
    }

    #[test]
    fn copies_are_labelled_where_a_removal_deserves_a_second_look() {
        let (dir, s) = drive();
        let page = list(&dir, &s, None, 10).unwrap();
        assert_eq!((page.summary.groups, page.summary.files, page.summary.bytes), (1, 2, 6 * MIB));
        let g = &page.groups[0];
        let shown: Vec<(&str, Place, Vec<String>)> =
            g.copies.iter().map(|c| (c.path.as_str(), c.place, english(&c.labels))).collect();
        assert_eq!(
            shown,
            [
                ("/Sync/a.iso", Place::Cloud, vec!["excluded".to_string()]),
                ("/Other/b.iso", Place::Cloud, vec!["outside the synced folder".to_string()]),
                ("/Sync/c.bin", Place::Cloud, vec![]),
            ]
        );
        assert_eq!(english(&g.labels), ["excluded", "outside the synced folder"], "the group says so too");
        // A key names exactly the copy shown.
        for c in &g.copies {
            assert_eq!(parse_key(&key(&c.chosen)).as_ref(), Some(&c.chosen));
        }
        assert!(parse_cursor(&cursor_text(&g.group.cursor())).is_ok());
        std::fs::remove_dir_all(&dir).unwrap();
    }

    /// A synced file that a rule now excludes (still synced until it's
    /// pruned or unticked) is labelled excluded too: the rules as they are
    /// now apply, whatever the copy's sync state.
    #[test]
    fn a_synced_copy_a_rule_now_excludes_is_labelled_excluded() {
        let (dir, mut s) = drive();
        s.write(|o| {
            let file = LocalRow {
                file_id: 2,
                parent_id: Some(1),
                name: "a.iso".into(),
                name_lossy: false,
                kind: LocalKind::File,
                size: 3 * MIB,
                mtime: 5,
                attributes: 0,
                reparse_tag: 0,
                skip: Skip::None,
            };
            o.local_put(&file)?;
            let base = |sid, parent, local: u128, remote: &str, kind, name: &str| oneup_sync::Base {
                sid,
                parent,
                name: name.into(),
                kind,
                size: if kind == oneup_sync::NodeKind::File { 3 * MIB } else { 0 },
                hash: (kind == oneup_sync::NodeKind::File).then_some([1; 20]),
                local: Some(LocalId(local)),
                local_mtime: 5,
                remote: Some(RemoteId(remote.into())),
                remote_etag: format!("e-{remote}"),
            };
            o.synced_put(&base(oneup_sync::ROOT, None, 1, "sync", oneup_sync::NodeKind::Folder, ""))?;
            o.synced_put(&base(5, Some(oneup_sync::ROOT), 2, "a", oneup_sync::NodeKind::File, "a.iso"))
        })
        .unwrap();
        let page = list(&dir, &s, None, 10).unwrap();
        let a = page.groups[0].copies.iter().find(|c| c.path == "/Sync/a.iso").unwrap();
        assert_eq!(a.place, Place::Both, "synced");
        assert_eq!(english(&a.labels), ["excluded"]);
        std::fs::remove_dir_all(&dir).unwrap();
    }

    #[test]
    fn a_removal_queues_only_as_previewed_and_runs_once() {
        let (dir, mut s) = drive();
        let page = list(&dir, &s, None, 10).unwrap();
        let pick: Vec<Chosen> = page.groups[0].copies.iter().skip(1).map(|c| c.chosen.clone()).collect();
        let p = prepare(&dir, &mut s, &pick).unwrap();
        assert_eq!((p.lines.len(), p.bytes), (2, 6 * MIB));
        let paths: Vec<&str> = p.lines.iter().map(|l| l.path.as_str()).collect();
        assert_eq!(paths, ["/Other/b.iso", "/Sync/c.bin"], "the store's paths, those its code is made of");
        assert!(p.lines.iter().all(|l| l.to == "OneDrive's recycle bin"));
        assert!(ask(&dir, &mut s, &pick, "0000000000000000").is_err(), "another code");
        ask(&dir, &mut s, &pick, &p.code).unwrap();
        let (round, log) = for_round(&dir, &mut s, false, false).unwrap();
        assert!(log.is_empty());
        let round = round.unwrap();
        assert_eq!(s.read(|o| code(o, &round.plan)).unwrap(), p.code, "the stop's code is the preview's");
        // Held for approval: nothing ran, and it waits.
        assert!(ran(&mut s, &round, &oneup_exec::Report::default()).unwrap().is_empty());
        assert!(s.read(queued).unwrap().is_some());
        let report = oneup_exec::Report { done: round.plan.ops.clone(), ..Default::default() };
        ran(&mut s, &round, &report).unwrap();
        assert!(s.read(queued).unwrap().is_none());
        // Paused: it waits; a backup pair: refused and dropped.
        ask(&dir, &mut s, &pick, &p.code).unwrap();
        assert!(for_round(&dir, &mut s, false, true).unwrap().0.is_none());
        assert!(s.read(queued).unwrap().is_some());
        let (none, log) = for_round(&dir, &mut s, true, false).unwrap();
        assert!(none.is_none() && log[0].outcome == ActivityOutcome::Failed);
        assert!(s.read(queued).unwrap().is_none());
        std::fs::remove_dir_all(&dir).unwrap();
    }

    /// What a round would remove must be exactly what the preview showed:
    /// a copy moved meanwhile (the same file, at another path) makes another
    /// list, and the choice is dropped, never carried out as it is now.
    #[test]
    fn a_choice_that_drifted_from_its_preview_is_dropped() {
        let (dir, mut s) = drive();
        let page = list(&dir, &s, None, 10).unwrap();
        let pick: Vec<Chosen> = page.groups[0].copies.iter().skip(1).map(|c| c.chosen.clone()).collect();
        let p = prepare(&dir, &mut s, &pick).unwrap();
        ask(&dir, &mut s, &pick, &p.code).unwrap();
        // c.bin moves to /Other, keeping its ID and eTag.
        s.write(|o| {
            o.upsert_item(
                &RemoteRow { parent_id: Some("other".into()), ..row("c", "sync", "c.bin", Kind::File, 1) },
                false,
            )
        })
        .unwrap();
        let (round, log) = for_round(&dir, &mut s, false, false).unwrap();
        assert!(round.is_none());
        assert_eq!(log[0].detail, "not done: what it removes changed since you chose; choose again");
        assert!(s.read(queued).unwrap().is_none());
        std::fs::remove_dir_all(&dir).unwrap();
    }

    /// Hashing files only here is bounded per run, continues where it
    /// stopped, stops when asked, and what it finds groups like the rest.
    #[cfg(windows)]
    #[test]
    fn hashing_files_only_here_is_bounded_resumable_and_stops_when_asked() {
        let dir = std::env::temp_dir().join(format!("oneup-dupes-hash-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(dir.join("sub")).unwrap();
        let same = vec![7u8; (1 << 20) + 3];
        std::fs::write(dir.join("a.bin"), &same).unwrap();
        std::fs::write(dir.join("sub").join("b.bin"), &same).unwrap();
        std::fs::write(dir.join("c.bin"), vec![8u8; (1 << 20) + 3]).unwrap();
        std::fs::write(dir.join("small.txt"), b"small").unwrap();
        let cfg = oneup_local::Config::new(oneup_rules::ExcludeSet::none());
        let mut scan = oneup_local::Scan::open(Store::open_in_memory().unwrap(), &dir, cfg).unwrap();
        assert!(scan.run().unwrap().finished);
        let mut s = scan.into_store();

        let one = hash_local(&mut s, 1, u64::MAX, &|| false).unwrap();
        assert_eq!((one.files, one.finished), (1, false));
        let stopped = hash_local(&mut s, 10, u64::MAX, &|| true).unwrap();
        assert_eq!((stopped.files, stopped.stopped), (0, true));
        let rest = hash_local(&mut s, 10, u64::MAX, &|| false).unwrap();
        assert_eq!((rest.files, rest.finished), (2, true), "{rest:?}");
        assert!(s.read(|o| o.meta_get(KEY_HASH_CURSOR)).unwrap().is_none(), "the next run starts over");
        let groups = s.read(|o| o.dup_groups(None, 10)).unwrap();
        assert_eq!(groups.len(), 1);
        assert_eq!(groups[0].copies, 2);
        // Nothing new: a run hashes nothing again.
        assert_eq!(hash_local(&mut s, 10, u64::MAX, &|| false).unwrap().files, 0);
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// The tray asks to approve a removal only with its paths in view: the
    /// stop's own saved list.
    #[test]
    fn approving_a_removal_shows_what_goes() {
        let (dir, _s) = drive();
        assert!(approval_text(&dir, "00000000000000aa", 10).is_none(), "no list yet: nothing to approve");
        let lines: Vec<String> = (0..12).map(|i| format!("remove      /Photos/{i}.jpg  (duplicate)")).collect();
        crate::stoplist::write(&dir, "00000000000000aa", "header", &lines, 12).unwrap();
        let text = approval_text(&dir, "00000000000000aa", 10).unwrap();
        assert!(text.contains("/Photos/0.jpg") && text.contains("/Photos/9.jpg"), "{text}");
        assert!(text.contains("and 2 more"), "{text}");
        std::fs::remove_dir_all(&dir).unwrap();
    }

    #[test]
    fn choosing_every_copy_leaves_the_last_and_says_so() {
        let (dir, mut s) = drive();
        let page = list(&dir, &s, None, 10).unwrap();
        let all: Vec<Chosen> = page.groups[0].copies.iter().map(|c| c.chosen.clone()).collect();
        let p = prepare(&dir, &mut s, &all).unwrap();
        assert!(p.lines.is_empty());
        assert_eq!(p.left.len(), 3);
        assert!(p.left.iter().all(|(_, why)| why == "it's the last copy here"));
        assert!(ask(&dir, &mut s, &all, &p.code).is_err());
        std::fs::remove_dir_all(&dir).unwrap();
    }
}
