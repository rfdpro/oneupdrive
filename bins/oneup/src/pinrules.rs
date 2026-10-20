//! Pin rules (`config.toml` `[rules] pin`, `oneup rules --pin`): with Files
//! On-Demand, the items they match are always kept on this PC, as if
//! "Always keep on this device" had been chosen for each one in File
//! Explorer. See `docs/architecture/files-on-demand.md` § Pin rules.
//!
//! The agent sets the pin attribute on each matching placeholder after a
//! settled round (`Engine::placement`); the placement pass then downloads
//! pinned files as it does for File Explorer's pins, a bounded share at a
//! time ([`FetchLimit`]), and nothing frees a pinned file: the space
//! policies and "Free up space" skip what a pin rule matches
//! (`crate::space`, `Engine::placement`), and the effects layer refuses to
//! dehydrate a placeholder pinned on its handle.
//!
//! **Only the rules' own pins are undone.** Every item a rule pinned is
//! noted in `pin_owned`; when the rules change, those the new rules don't
//! match go back to the default, and no other. An item the user had pinned
//! already isn't noted, so removing a rule never unpins it.
//!
//! **Nothing is forgotten.** An item whose pin can't be set yet (not a
//! placeholder yet, say) waits in `pin_retry` and is tried again with a
//! growing back-off; when that table is full, another walk follows once it
//! has drained.
//!
//! Cost follows changes:
//! * **When the pin rules change** (their fingerprint differs from `meta`
//!   [`KEY_APPLIED`]), the rules' own pins are looked at (`pin_owned`, a
//!   page at a time), then one walk over the Local tree in the database
//!   pins what the new rules match. The walk reads each folder a page at a
//!   time ([`PAGE`]) and goes depth first, so it holds the folders on the
//!   way down with a cursor each, never a whole folder; that state is
//!   saved in `meta` [`KEY_WALK`] after each slice, so a walk cut short (the
//!   agent stopped) carries on where it was. The new fingerprint is stored
//!   once the walk is done.
//! * **Otherwise** only the items the activity log names since the last
//!   pass ([`KEY_SEQ`]): what was downloaded, uploaded, created or moved.
//!   Placeholders created in a pinned folder inherit its pin from Windows
//!   anyway; this covers items a pattern like `*.psd` matches anywhere.
//!
//! Setting a pin never reads or writes a file's content (an attribute-only
//! handle, `oneup_cfapi::provider::open`; see [`FsPinner`]).

use std::path::{Path, PathBuf};
use std::time::{Duration, Instant};

use anyhow::Result;
use oneup_rules::{ENGINE_DIR, PinSet};
use oneup_store::{LocalKind, LocalRow, Skip, Store};
use serde_json::{Value, json};

/// The fingerprint of the pin rules last applied in full.
pub const KEY_APPLIED: &str = "pins.applied";
/// The last activity-log entry looked at.
pub const KEY_SEQ: &str = "pins.seq";
/// A walk under way ([`Walk`], as JSON).
pub const KEY_WALK: &str = "pins.walk";
/// Set when `pin_retry` was full: walk again once it has drained.
const KEY_REWALK: &str = "pins.rewalk";
/// Activity entries looked at per pass; more wait for the next one.
const PER_PASS: usize = 2_000;
/// Children read from the database at a time.
pub const PAGE: usize = 256;
/// Retries looked at per pass.
const RETRIES: usize = 256;
/// `FILE_ATTRIBUTE_PINNED`, `FILE_ATTRIBUTE_RECALL_ON_DATA_ACCESS`.
const PINNED: u32 = 0x0008_0000;
const RECALL: u32 = 0x0040_0000;

/// What an item's pin should become.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Change {
    /// Always keep on this device.
    Pin,
    /// Back to the default (only for an item a rule pinned).
    Unpin,
}

/// Sets pin states (the Cloud Files API in the agent; a fake in tests).
pub trait Pinner {
    /// Sets the pin of the item at `rel` (relative to the synced folder).
    fn set(&mut self, rel: &str, change: Change) -> std::io::Result<()>;
}

/// The real one: an attribute-only handle on the item, never its content.
pub struct FsPinner(pub PathBuf);

impl Pinner for FsPinner {
    fn set(&mut self, rel: &str, change: Change) -> std::io::Result<()> {
        use oneup_cfapi::provider;
        let full = rel.split('/').filter(|p| !p.is_empty()).fold(self.0.clone(), |p, x| p.join(x));
        let pin = match change {
            Change::Pin => provider::Pin::Pinned,
            Change::Unpin => provider::Pin::Unspecified,
        };
        provider::open(&full).and_then(|f| provider::set_pin(&f, pin)).map_err(std::io::Error::other)
    }
}

/// How long an item waits before its pin is tried again, after `tries`
/// failures: a minute, doubling, at most six hours.
pub fn back_off_ms(tries: u32) -> i64 {
    60_000i64.saturating_mul(1 << tries.min(9)).min(6 * 3_600_000)
}

/// A walk under way (rules changed), saved between slices.
#[derive(Debug, Clone, PartialEq, Eq)]
struct Walk {
    /// The new rules' fingerprint.
    fingerprint: String,
    /// The activity log's last entry when the walk started: entries after
    /// it are looked at once the walk is done.
    seq: i64,
    /// First the rules' own pins are looked at (`pin_owned` after this
    /// ID; `Some(None)`: from the start), then the tree.
    owned: Option<Option<u128>>,
    /// The folders on the way down, each with the last child looked at.
    stack: Vec<Frame>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
struct Frame {
    id: u128,
    rel: String,
    after: Option<u128>,
}

impl Walk {
    fn to_json(&self) -> String {
        let stack: Vec<Value> = self
            .stack
            .iter()
            .map(|f| json!({ "id": f.id.to_string(), "rel": f.rel, "after": f.after.map(|a| a.to_string()) }))
            .collect();
        let owned = self.owned.map(|o| o.map(|a| a.to_string()).unwrap_or_default());
        json!({ "fp": self.fingerprint, "seq": self.seq, "owned": owned, "stack": stack }).to_string()
    }

    fn from_json(text: &str) -> Option<Self> {
        let v: Value = serde_json::from_str(text).ok()?;
        let id = |v: &Value| v.as_str().and_then(|s| s.parse::<u128>().ok());
        let stack = v["stack"]
            .as_array()?
            .iter()
            .map(|f| Some(Frame { id: id(&f["id"])?, rel: f["rel"].as_str()?.to_string(), after: id(&f["after"]) }))
            .collect::<Option<Vec<_>>>()?;
        let owned = match &v["owned"] {
            Value::Null => None,
            Value::String(s) if s.is_empty() => Some(None),
            o => Some(Some(id(o)?)),
        };
        Some(Self { fingerprint: v["fp"].as_str()?.to_string(), seq: v["seq"].as_i64()?, owned, stack })
    }
}

/// What a pass did.
#[derive(Debug, Default, Clone, PartialEq, Eq)]
pub struct Pass {
    pub pinned: usize,
    pub unpinned: usize,
    /// Pins that couldn't be set now; tried again later.
    pub waiting: usize,
    /// Work is left for the next pass.
    pub more: bool,
}

/// One pass: brings pin states in line with `pins`, for up to `budget`.
/// The walk's state lives in the store, so a pass can stop anywhere.
pub fn apply(store: &mut Store, pinner: &mut dyn Pinner, pins: &PinSet, budget: Duration, now_ms: i64) -> Result<Pass> {
    let started = Instant::now();
    let out_of_time = || started.elapsed() >= budget;
    let mut pass = Pass::default();
    retry(store, pinner, pins, now_ms, &mut pass)?;

    let fp = pins.fingerprint();
    let last = store.read(|o| o.activity_last_seq())?;
    let seen: Option<i64> = store.read(|o| o.meta_get(KEY_SEQ))?.and_then(|s| s.parse().ok());
    let mut applied = store.read(|o| o.meta_get(KEY_APPLIED))?;
    // Entries dropped from the log before they were looked at, or items
    // the retry table had no room for: walk again.
    let rewalk = store.read(|o| Ok(o.meta_get(KEY_REWALK)?.is_some() && o.pin_retry_count()? == 0))?;
    if seen.is_some_and(|s| last - s > oneup_store::activity::KEEP) || rewalk {
        applied = None;
        store.write(|o| {
            o.meta_delete(KEY_REWALK)?;
            o.meta_delete(KEY_APPLIED)
        })?;
    }
    if applied.as_deref() != Some(fp.as_str()) {
        let walk = store.read(|o| o.meta_get(KEY_WALK))?.and_then(|t| Walk::from_json(&t));
        // No rules, and none of theirs to undo: nothing to walk for.
        if pins.is_empty() && walk.is_none() && store.read(|o| o.pin_owned_page(None, 1))?.is_empty() {
            store.write(|o| {
                o.meta_set(KEY_APPLIED, &fp)?;
                o.meta_set(KEY_SEQ, &last.to_string())
            })?;
            return Ok(pass);
        }
        let mut w = match walk {
            Some(w) if w.fingerprint == fp => w,
            _ => {
                let Some(r) = store.read(|o| o.local_root())? else { return Ok(pass) };
                Walk {
                    fingerprint: fp.clone(),
                    seq: last,
                    owned: Some(None),
                    stack: vec![Frame { id: r.root_file_id, rel: String::new(), after: None }],
                }
            }
        };
        // The rules' own pins the new rules don't keep.
        while let Some(after) = w.owned {
            let page = store.read(|o| o.pin_owned_page(after, PAGE))?;
            let Some(&end) = page.last() else {
                w.owned = None;
                break;
            };
            for id in page {
                let row = store.read(|o| o.local_get(id))?;
                let rel = match &row {
                    Some(_) => store.read(|o| o.local_path_of(id))?.map(|p| crate::filetree::clean(&p)),
                    None => None,
                };
                let (Some(row), Some(rel)) = (row, rel) else {
                    store.write(|o| o.pin_owned_remove(id))?;
                    continue;
                };
                if pins.pins(&rel, row.kind != LocalKind::File) {
                    continue;
                }
                if row.attributes & PINNED == 0 {
                    // Not pinned any more (the user chose otherwise): not ours.
                    store.write(|o| o.pin_owned_remove(id))?;
                } else if pinner.set(&rel, Change::Unpin).is_ok() {
                    store.write(|o| o.pin_owned_remove(id))?;
                    pass.unpinned += 1;
                }
                // A failure keeps the note: the next change of rules
                // looks at it again.
            }
            w.owned = Some(Some(end));
            if out_of_time() {
                return save_walk(store, &w, pass);
            }
        }
        // The tree, depth first, a page of one folder at a time (nothing to
        // pin without rules).
        if pins.is_empty() {
            w.stack.clear();
        }
        while let Some(top) = w.stack.last().cloned() {
            let page = store.read(|o| o.local_children_page(top.id, top.after, PAGE))?;
            if page.is_empty() {
                w.stack.pop();
                continue;
            }
            let mut down = None;
            for c in page {
                if let Some(f) = w.stack.last_mut() {
                    f.after = Some(c.file_id);
                }
                if top.rel.is_empty() && oneup_sync::same_name(&c.name, ENGINE_DIR) {
                    continue;
                }
                let rel = if top.rel.is_empty() { c.name.clone() } else { format!("{}/{}", top.rel, c.name) };
                visit(store, pinner, &c, &rel, pins, now_ms, &mut pass)?;
                if c.kind == LocalKind::Folder && c.skip == Skip::None {
                    down = Some(Frame { id: c.file_id, rel, after: None });
                    break;
                }
            }
            if let Some(f) = down {
                w.stack.push(f);
            }
            if out_of_time() && !w.stack.is_empty() {
                return save_walk(store, &w, pass);
            }
        }
        let seq = w.seq;
        store.write(|o| {
            o.meta_delete(KEY_WALK)?;
            o.meta_set(KEY_APPLIED, &fp)?;
            o.meta_set(KEY_SEQ, &seq.to_string())
        })?;
        pass.more = true; // the entries since the walk began
        return Ok(pass);
    }
    // The rules are applied: only what changed since.
    let seen = seen.unwrap_or(last);
    if pins.is_empty() {
        if seen != last {
            store.write(|o| o.meta_set(KEY_SEQ, &last.to_string()))?;
        }
        return Ok(pass);
    }
    let entries = store.read(|o| o.activity_done_after(seen, PER_PASS))?;
    let Some(r) = store.read(|o| o.local_root())? else { return Ok(pass) };
    let mut upto = seen;
    let mut cut = false;
    for (seq, path, detail) in &entries {
        upto = *seq;
        for p in std::iter::once(path.as_str()).chain(moved_to(detail)) {
            let rel = crate::filetree::clean(p);
            if let Some(row) = store.read(|o| row_at(o, r.root_file_id, &rel))? {
                visit(store, pinner, &row, &rel, pins, now_ms, &mut pass)?;
            }
        }
        if out_of_time() {
            cut = true;
            break;
        }
    }
    pass.more = cut || entries.len() == PER_PASS;
    if upto != seen {
        store.write(|o| o.meta_set(KEY_SEQ, &upto.to_string()))?;
    }
    Ok(pass)
}

fn save_walk(store: &mut Store, w: &Walk, mut pass: Pass) -> Result<Pass> {
    store.write(|o| o.meta_set(KEY_WALK, &w.to_json()))?;
    pass.more = true;
    Ok(pass)
}

/// Tries again the pins that couldn't be set before, those that are due.
fn retry(store: &mut Store, pinner: &mut dyn Pinner, pins: &PinSet, now_ms: i64, pass: &mut Pass) -> Result<()> {
    for (id, tries) in store.read(|o| o.pin_retry_due(now_ms, RETRIES))? {
        let row = store.read(|o| o.local_get(id))?;
        let rel = match &row {
            Some(_) => store.read(|o| o.local_path_of(id))?.map(|p| crate::filetree::clean(&p)),
            None => None,
        };
        let (Some(row), Some(rel)) = (row, rel) else {
            store.write(|o| o.pin_retry_remove(id))?;
            continue;
        };
        if !pins.pins(&rel, row.kind != LocalKind::File) || row.attributes & PINNED != 0 {
            store.write(|o| o.pin_retry_remove(id))?;
            continue;
        }
        match pinner.set(&rel, Change::Pin) {
            Ok(()) => {
                store.write(|o| {
                    o.pin_owned_add(id)?;
                    o.pin_retry_remove(id)
                })?;
                pass.pinned += 1;
            }
            Err(_) => {
                store.write(|o| o.pin_retry_put(id, tries + 1, now_ms + back_off_ms(tries)).map(|_| ()))?;
                pass.waiting += 1;
            }
        }
    }
    Ok(())
}

/// Where an activity entry's item moved to (`-> /new/path; note`).
fn moved_to(detail: &str) -> Option<&str> {
    let rest = detail.strip_prefix("-> ")?;
    Some(rest.split_once("; ").map_or(rest, |(to, _)| to))
}

/// The Local row at `rel` (relative to the synced folder), by name as
/// Windows compares names: the exact spelling, else the only one alike.
pub fn row_at(o: &oneup_store::Ops<'_>, root: u128, rel: &str) -> oneup_store::Result<Option<LocalRow>> {
    let mut cur = root;
    let mut row = None;
    for part in rel.split('/').filter(|p| !p.is_empty()) {
        let mut found = o.local_named(cur, part)?;
        let pick = match found.iter().position(|r| r.name == part) {
            Some(i) => Some(found.swap_remove(i)),
            None if found.len() == 1 => found.pop(),
            None => None,
        };
        let Some(r) = pick else { return Ok(None) };
        cur = r.file_id;
        row = Some(r);
    }
    Ok(row)
}

/// Pins one item if the rules match it and it isn't pinned yet, noting it
/// as the rules' own; one whose pin can't be set now waits for a retry.
fn visit(
    store: &mut Store,
    pinner: &mut dyn Pinner,
    row: &LocalRow,
    rel: &str,
    pins: &PinSet,
    now_ms: i64,
    pass: &mut Pass,
) -> Result<()> {
    if !matches!(row.kind, LocalKind::File | LocalKind::Folder) || row.attributes & PINNED != 0 {
        return Ok(());
    }
    if !pins.pins(rel, row.kind != LocalKind::File) {
        return Ok(());
    }
    let id = row.file_id;
    match pinner.set(rel, Change::Pin) {
        Ok(()) => {
            store.write(|o| o.pin_owned_add(id))?;
            pass.pinned += 1;
        }
        Err(e) => {
            tracing::debug!(error = %e, "a pin rule's item couldn't be pinned now; tried again later");
            let kept = store.write(|o| o.pin_retry_put(id, 1, now_ms + back_off_ms(0)))?;
            if !kept {
                store.write(|o| o.meta_set(KEY_REWALK, "1"))?;
            }
            pass.waiting += 1;
        }
    }
    Ok(())
}

/// Pins `rel` (a file set to "Free up space" that a pin rule keeps),
/// noting it as the rule's. Whether it worked.
pub fn pin_back(store: &mut Store, pinner: &mut dyn Pinner, id: u128, rel: &str) -> Result<bool> {
    if pinner.set(rel, Change::Pin).is_err() {
        return Ok(false);
    }
    store.write(|o| o.pin_owned_add(id))?;
    Ok(true)
}

/// What pin rules `lines` would download: the online-only files they'd
/// pin, not pinned yet (count, bytes), and whether all were looked at
/// (`false`: only those found within `budget`, "at least"). A walk over
/// the Local tree in the database, paged as [`apply`]'s; asked for when
/// someone adds a pin rule.
pub fn preview(store: &Store, lines: &[String], budget: Duration) -> Result<(u64, u64, bool)> {
    let pins = PinSet::new(lines);
    let started = Instant::now();
    let Some(r) = store.read(|o| o.local_root())? else { return Ok((0, 0, true)) };
    let (mut files, mut bytes) = (0u64, 0u64);
    let mut stack = vec![Frame { id: r.root_file_id, rel: String::new(), after: None }];
    while let Some(top) = stack.last().cloned() {
        if started.elapsed() >= budget {
            return Ok((files, bytes, false));
        }
        let page = store.read(|o| o.local_children_page(top.id, top.after, PAGE))?;
        if page.is_empty() {
            stack.pop();
            continue;
        }
        let mut down = None;
        for c in page {
            if let Some(f) = stack.last_mut() {
                f.after = Some(c.file_id);
            }
            if top.rel.is_empty() && oneup_sync::same_name(&c.name, ENGINE_DIR) {
                continue;
            }
            let rel = if top.rel.is_empty() { c.name.clone() } else { format!("{}/{}", top.rel, c.name) };
            if c.kind == LocalKind::File
                && c.attributes & RECALL != 0
                && c.attributes & PINNED == 0
                && pins.pins(&rel, false)
            {
                files += 1;
                bytes += u64::try_from(c.size).unwrap_or(0);
            }
            if c.kind == LocalKind::Folder && c.skip == Skip::None {
                down = Some(Frame { id: c.file_id, rel, after: None });
                break;
            }
        }
        if let Some(f) = down {
            stack.push(f);
        }
    }
    Ok((files, bytes, true))
}

/// Pin rules that would download more than this ask first.
pub const ASK_BYTES: u64 = 1 << 30;

/// Whether downloading `bytes` needs the person's go-ahead: more than
/// [`ASK_BYTES`], or more than the disk has free.
pub fn must_ask(bytes: u64, free: Option<u64>) -> bool {
    bytes > ASK_BYTES || free.is_some_and(|f| bytes > f)
}

/// A pass's limits for downloading pinned files: asked before each file
/// (`oneup_exec::placement::fetch_pinned`'s `may`). Stops at a pause, when
/// the disk would keep less free than `reserve`, and once the pass has
/// used its bytes or time (its first file may be larger than the bytes, so
/// a big file still gets its turn).
#[derive(Debug)]
pub struct FetchLimit {
    pub bytes_left: u64,
    pub until: Instant,
    pub reserve: u64,
    pub first: bool,
}

/// A pass's share of downloads: at most this many bytes...
pub const FETCH_BYTES: u64 = 2 << 30;
/// ...and this long (checked between files).
pub const FETCH_TIME: Duration = Duration::from_secs(60);

impl FetchLimit {
    pub fn new(reserve: u64, now: Instant) -> Self {
        Self { bytes_left: FETCH_BYTES, until: now + FETCH_TIME, reserve, first: true }
    }

    /// Pure: whether a file of `size` may download now.
    pub fn may(&mut self, size: u64, paused: bool, free: Option<u64>, now: Instant) -> bool {
        if paused || now >= self.until {
            return false;
        }
        if free.is_some_and(|f| f < size.saturating_add(self.reserve)) {
            return false;
        }
        if !self.first && size > self.bytes_left {
            return false;
        }
        self.first = false;
        self.bytes_left = self.bytes_left.saturating_sub(size);
        true
    }
}

/// The free space a pass of downloads leaves alone: a gibibyte, or the
/// space policy's low-disk share of the disk if that's more.
pub fn reserve(total: Option<u64>, low_disk_percent: u32) -> u64 {
    let share = total.map_or(0, |t| t / 100 * u64::from(low_disk_percent));
    share.max(1 << 30)
}

/// The free space on the disk of `root`, if it can be read.
pub fn free_space(root: &Path) -> Option<u64> {
    oneup_sys::disk::free_bytes(root).ok()
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used)]
    use super::*;
    use oneup_store::{ActivityEntry, ActivityOutcome, LocalRoot};

    /// Pins in memory: which paths are pinned, and which fail.
    #[derive(Default)]
    struct Fake {
        set: Vec<(String, Change)>,
        failing: Vec<String>,
    }

    impl Pinner for Fake {
        fn set(&mut self, rel: &str, change: Change) -> std::io::Result<()> {
            if self.failing.iter().any(|f| f == rel) {
                return Err(std::io::Error::other("not a placeholder yet"));
            }
            self.set.push((rel.to_string(), change));
            Ok(())
        }
    }

    fn row(id: u128, parent: u128, name: &str, kind: LocalKind, attributes: u32) -> LocalRow {
        LocalRow {
            file_id: id,
            parent_id: Some(parent),
            name: name.into(),
            name_lossy: false,
            kind,
            size: 10,
            mtime: 0,
            attributes,
            reparse_tag: 0,
            skip: Skip::None,
        }
    }

    fn store(rows: &[LocalRow]) -> Store {
        let mut s = Store::open_in_memory().unwrap();
        s.write(|o| {
            let root = LocalRow { parent_id: None, ..row(1, 0, "", LocalKind::Root, 0x10) };
            let info = LocalRoot {
                path: r"C:\Sync".into(),
                volume_serial: 1,
                root_file_id: 1,
                rules: String::new(),
                journal: None,
                started_at_ms: 0,
                finished_at_ms: Some(0),
                sync_root_provider: None,
            };
            o.local_begin(&info, &root)?;
            for r in rows {
                o.local_put(r)?;
            }
            Ok(())
        })
        .unwrap();
        s
    }

    /// Records the attributes a pin set would have (as the observer would).
    fn observe(s: &mut Store, fake: &Fake, ids: &[(u128, &str)]) {
        for (id, rel) in ids {
            let last = fake.set.iter().rev().find(|(r, _)| r == rel).map(|(_, c)| *c);
            if let Some(c) = last {
                s.write(|o| {
                    let mut r = o.local_get(*id)?.unwrap();
                    r.attributes = match c {
                        Change::Pin => r.attributes | PINNED,
                        Change::Unpin => r.attributes & !PINNED,
                    };
                    o.local_update(&r)
                })
                .unwrap();
            }
        }
    }

    fn run(s: &mut Store, fake: &mut Fake, pins: &PinSet, now: i64) -> Pass {
        let mut total = Pass::default();
        for _ in 0..50 {
            let p = apply(s, fake, pins, Duration::from_secs(60), now).unwrap();
            total.pinned += p.pinned;
            total.unpinned += p.unpinned;
            total.waiting += p.waiting;
            if !p.more {
                break;
            }
        }
        total
    }

    /// A rule over a file the user pinned already changes nothing, and
    /// removing it later leaves that file pinned: only the rule's own pins
    /// are undone.
    #[test]
    fn removing_a_rule_undoes_only_its_own_pins() {
        let mut s = store(&[
            row(2, 1, "a.psd", LocalKind::File, RECALL),
            row(3, 1, "mine.psd", LocalKind::File, PINNED),
            row(4, 1, "notes.txt", LocalKind::File, RECALL),
        ]);
        let mut fake = Fake::default();
        let p = run(&mut s, &mut fake, &PinSet::new(&["*.psd"]), 0);
        assert_eq!(p.pinned, 1);
        assert_eq!(fake.set, [("a.psd".to_string(), Change::Pin)], "mine.psd was pinned already");
        observe(&mut s, &fake, &[(2, "a.psd")]);
        fake.set.clear();
        let p = run(&mut s, &mut fake, &PinSet::new::<&str>(&[]), 0);
        assert_eq!(p.unpinned, 1);
        assert_eq!(fake.set, [("a.psd".to_string(), Change::Unpin)], "the user's own pin stays");
        assert!(s.read(|o| o.pin_owned_page(None, 10)).unwrap().is_empty());
    }

    /// An item whose pin can't be set yet isn't forgotten: it waits, is
    /// tried again after its back-off, and is pinned once it can be.
    #[test]
    fn a_pin_that_fails_is_tried_again_until_it_is_set() {
        let mut s = store(&[row(2, 1, "a.psd", LocalKind::File, 0)]);
        let mut fake = Fake { failing: vec!["a.psd".into()], ..Fake::default() };
        let pins = PinSet::new(&["*.psd"]);
        let p = run(&mut s, &mut fake, &pins, 0);
        assert_eq!((p.pinned, p.waiting), (0, 1));
        assert_eq!(s.read(|o| o.meta_get(KEY_APPLIED)).unwrap(), Some(pins.fingerprint()), "the walk is done");
        assert_eq!(s.read(|o| o.pin_retry_count()).unwrap(), 1, "but the item waits");
        // Still failing: tried again only once due.
        let p = run(&mut s, &mut fake, &pins, back_off_ms(0));
        assert_eq!(p.waiting, 1);
        // It became a placeholder (converted after its upload, say).
        fake.failing.clear();
        let p = run(&mut s, &mut fake, &pins, back_off_ms(0) - 1);
        assert_eq!(p.pinned, 0, "not due yet");
        let p = run(&mut s, &mut fake, &pins, back_off_ms(0) + back_off_ms(1));
        assert_eq!(p.pinned, 1);
        assert_eq!(s.read(|o| o.pin_retry_count()).unwrap(), 0);
        assert!(s.read(|o| o.pin_owned_has(2)).unwrap());
    }

    /// With the rules applied, a new item the activity log names is pinned
    /// by the next pass (a file converted to a placeholder after upload).
    #[test]
    fn a_new_item_the_activity_log_names_is_pinned() {
        let mut s = store(&[]);
        let mut fake = Fake::default();
        let pins = PinSet::new(&["*.psd"]);
        run(&mut s, &mut fake, &pins, 0);
        s.write(|o| {
            o.local_put(&row(5, 1, "new.psd", LocalKind::File, 0))?;
            o.activity_add(&[ActivityEntry {
                seq: 0,
                at_ms: 1,
                outcome: ActivityOutcome::Done,
                action: "upload".into(),
                path: "/new.psd".into(),
                detail: String::new(),
            }])
        })
        .unwrap();
        let p = run(&mut s, &mut fake, &pins, 0);
        assert_eq!(p.pinned, 1);
        assert_eq!(fake.set, [("new.psd".to_string(), Change::Pin)]);
    }

    /// A walk over a large folder stops when its time is up, holding one
    /// page at most, and carries on from where it was in a later pass (also
    /// from the store alone, as after a restart), each item once.
    #[test]
    fn a_walk_goes_in_slices_and_resumes_from_the_store() {
        let mut rows: Vec<LocalRow> = vec![row(2, 1, "big", LocalKind::Folder, 0x10)];
        for i in 0..(PAGE as u128 * 3 + 7) {
            rows.push(row(100 + i, 2, &format!("f{i}.psd"), LocalKind::File, 0));
        }
        let mut s = store(&rows);
        let mut fake = Fake::default();
        let pins = PinSet::new(&["*.psd"]);
        let mut passes = 0;
        loop {
            passes += 1;
            let p = apply(&mut s, &mut fake, &pins, Duration::ZERO, 0).unwrap();
            if !p.more || passes > 100 {
                break;
            }
            assert!(s.read(|o| o.meta_get(KEY_WALK)).unwrap().is_some() || p.more);
        }
        assert!(passes >= 4, "several slices: {passes}");
        let mut got: Vec<&str> = fake.set.iter().map(|(r, _)| r.as_str()).collect();
        got.sort_unstable();
        got.dedup();
        assert_eq!(got.len(), PAGE * 3 + 7, "every file pinned");
        assert_eq!(fake.set.len(), PAGE * 3 + 7, "each once");
        assert_eq!(s.read(|o| o.meta_get(KEY_APPLIED)).unwrap(), Some(pins.fingerprint()));
    }

    #[test]
    fn the_walk_state_round_trips() {
        let w = Walk {
            fingerprint: "p2:x".into(),
            seq: 7,
            owned: Some(Some(u128::MAX)),
            stack: vec![
                Frame { id: 1, rel: String::new(), after: Some(5) },
                Frame { id: 9, rel: "a b".into(), after: None },
            ],
        };
        assert_eq!(Walk::from_json(&w.to_json()), Some(w.clone()));
        let w = Walk { owned: Some(None), ..w };
        assert_eq!(Walk::from_json(&w.to_json()), Some(w.clone()));
        let w = Walk { owned: None, ..w };
        assert_eq!(Walk::from_json(&w.to_json()), Some(w));
    }

    #[test]
    fn the_preview_counts_online_only_files_not_pinned_yet() {
        let s = store(&[
            row(2, 1, "a.psd", LocalKind::File, RECALL),
            row(3, 1, "b.psd", LocalKind::File, 0),
            row(4, 1, "c.psd", LocalKind::File, RECALL | PINNED),
            row(5, 1, "d.txt", LocalKind::File, RECALL),
        ]);
        assert_eq!(preview(&s, &["*.psd".into()], Duration::from_secs(5)).unwrap(), (1, 10, true));
        assert!(must_ask(ASK_BYTES + 1, None) && must_ask(10, Some(5)) && !must_ask(10, Some(50)));
    }

    /// Downloads stop at a pause, before the disk gets too full, and once a
    /// pass has used its share of bytes or time.
    #[test]
    fn downloads_keep_to_pauses_free_space_and_a_share_per_pass() {
        let now = Instant::now();
        let mut l = FetchLimit::new(100, now);
        assert!(!l.may(10, true, None, now), "paused");
        assert!(!l.may(10, false, Some(105), now), "would leave less than the reserve");
        assert!(l.may(FETCH_BYTES + 5, false, None, now), "the first file, however large");
        assert!(!l.may(1, false, None, now), "the share is used");
        let mut l = FetchLimit::new(0, now);
        assert!(l.may(1, false, None, now));
        assert!(!l.may(1, false, None, now + FETCH_TIME), "the time is up");
        assert_eq!(reserve(Some(100 << 30), 10), 10 << 30);
        assert_eq!(reserve(None, 10), 1 << 30);
    }
}
