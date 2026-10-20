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
//! A placeholder created in a folder a rule pinned takes the pin from
//! Windows; it's noted as the rule's too when the rules match it, so
//! removing the rule undoes it with its folder.
//!
//! **Nothing is forgotten.** An item whose pin can't be set yet (not a
//! placeholder yet, say) waits in `pin_retry` and is tried again with a
//! growing back-off; when that table is full, another walk follows as soon
//! as it has room again.
//!
//! **"Free up space" on the file itself wins; on a folder, the rule does.**
//! A file a rule keeps that someone frees by name (`oneup free FILE`, or
//! File Explorer's "Free up space" on the file) is released from the rules
//! (`pin_released`): it's freed and stays so until someone keeps it on this
//! device again. Freed through a folder above it, it keeps its pin: the
//! rule's pin, like a pin a folder passes on, wins over a folder's "Free up
//! space" and over the automatic space policies. File Explorer doesn't say
//! which was chosen; it marks every item it frees "free up space", the
//! folder too when the folder was chosen, so a file whose folder isn't
//! marked was freed by name ([`freed_by_name`]). Items losing a rule's pin
//! are queued by the store from the change journal (`pin_changed`).
//!
//! **Downloads are asked for.** Saving pin rules that add pins shows what
//! they'd download and needs a yes ([`preview`], [`approve`]); the agent
//! counts what it downloads for the rules' items against what was
//! approved (`meta` [`KEY_BUDGET`]), and past [`ASK_BYTES`] more than that
//! it stops and waits for a go-ahead ([`KEY_WAITING`], [`allow`]), however
//! the rules changed (a hand edit of config.toml too).
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
use oneup_exec::placement::Stop;
use oneup_rules::{ENGINE_DIR, PinSet};
use oneup_store::pins::PinOp;
use oneup_store::{LocalKind, LocalRow, Skip, Store};
use serde_json::{Value, json};

/// The fingerprint of the pin rules last applied in full (a rescan of the
/// folder forgets it, `oneup_store::pins`).
pub const KEY_APPLIED: &str = oneup_store::pins::KEY_APPLIED;
/// A walk of a moved folder under way ([`Walk`], as JSON; its fingerprint
/// is the folder's ID).
const KEY_SUBWALK: &str = "pins.subwalk";
/// The last activity-log entry looked at.
pub const KEY_SEQ: &str = "pins.seq";
/// A walk under way ([`Walk`], as JSON).
pub const KEY_WALK: &str = "pins.walk";
/// Set when `pin_retry` was full: walk again as soon as it has room.
const KEY_REWALK: &str = "pins.rewalk";
/// Bytes approved for the rules' downloads and not spent yet (negative:
/// downloaded beyond what was approved).
pub const KEY_BUDGET: &str = "pins.budget";
/// An approval for one set of rules: `fingerprint bytes`.
pub const KEY_APPROVE: &str = "pins.approve";
/// The download the agent stopped at, waiting for a go-ahead: its bytes.
pub const KEY_WAITING: &str = "pins.waiting";
/// Budget reserved for the downloads under way: `id:size,…`.
pub const KEY_RESERVED: &str = "pins.reserved";
/// Of those, the downloads started: `id,…`.
const KEY_STARTED: &str = "pins.started";
/// Queued items whose pin was lost, looked at per pass.
const CHANGED: usize = 256;
/// Activity entries looked at per pass; more wait for the next one.
const PER_PASS: usize = 2_000;
/// Children read from the database at a time.
pub const PAGE: usize = 256;
/// Retries looked at per pass.
const RETRIES: usize = 256;
/// `FILE_ATTRIBUTE_PINNED`, `FILE_ATTRIBUTE_UNPINNED`,
/// `FILE_ATTRIBUTE_RECALL_ON_DATA_ACCESS`.
const PINNED: u32 = 0x0008_0000;
const UNPINNED: u32 = 0x0010_0000;
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
/// Every change goes through a handle on which the item's identity is
/// checked first: a path names whatever is there now, and that may be
/// another item than the one the database knows by that name.
pub trait Pinner {
    /// The identity (file ID, creation time) of the item at `rel` (relative
    /// to the synced folder), read on a handle to it. `expected` is the file
    /// ID the database has there (the real pinner reads, never assumes it).
    fn identify(&mut self, rel: &str, expected: u128) -> std::io::Result<(u128, i64)>;

    /// Sets the pin of the item at `rel`, through a handle on which its file
    /// ID (and, if given, creation time) is checked to be `expect`'s first;
    /// another item there is an error of kind `InvalidData`, nothing set.
    fn set(&mut self, rel: &str, expect: (u128, Option<i64>), change: Change) -> std::io::Result<()>;
}

/// The error for "another item is there now".
fn not_that_item() -> std::io::Error {
    std::io::Error::new(std::io::ErrorKind::InvalidData, "another item is there now")
}

/// Whether an error from [`Pinner::set`] means another item is there.
fn is_other_item(e: &std::io::Error) -> bool {
    e.kind() == std::io::ErrorKind::InvalidData
}

/// The real one: an attribute-only handle on the item, never its content.
pub struct FsPinner(pub PathBuf);

impl FsPinner {
    fn open(&self, rel: &str) -> std::io::Result<std::fs::File> {
        let full = rel.split('/').filter(|p| !p.is_empty()).fold(self.0.clone(), |p, x| p.join(x));
        oneup_cfapi::provider::open(&full).map_err(std::io::Error::other)
    }
}

impl Pinner for FsPinner {
    fn identify(&mut self, rel: &str, _expected: u128) -> std::io::Result<(u128, i64)> {
        let f = self.open(rel)?;
        oneup_cfapi::provider::identity(&f).map_err(std::io::Error::other)
    }

    fn set(&mut self, rel: &str, expect: (u128, Option<i64>), change: Change) -> std::io::Result<()> {
        use oneup_cfapi::provider;
        let f = self.open(rel)?;
        let (id, created) = provider::identity(&f).map_err(std::io::Error::other)?;
        if id != expect.0 || expect.1.is_some_and(|c| c != created) {
            return Err(not_that_item());
        }
        let pin = match change {
            Change::Pin => provider::Pin::Pinned,
            Change::Unpin => provider::Pin::Unspecified,
        };
        provider::set_pin(&f, pin).map_err(std::io::Error::other)
    }
}

/// How pinning one item by a rule ended ([`pin_item`]).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Pinned {
    Done,
    /// Another item is at that path now: nothing set, nothing noted.
    Other,
    /// Couldn't be set now (noted for a retry by the caller).
    Failed,
}

/// Pins item `id` at `rel` for the rules: reads its identity on a handle,
/// writes the rule's note (with that creation time) before setting the pin,
/// so a crash in between leaves it the rule's (counted against the rules'
/// download budget, unpinnable by them), then sets the pin through a handle
/// on which the identity is checked again. A failure takes the note back.
fn pin_item(store: &mut Store, pinner: &mut dyn Pinner, id: u128, rel: &str, at_seq: i64) -> Result<Pinned> {
    // The user's (claimed with `oneup pin`, or found pinned): never the rule's.
    if store.read(|o| o.pin_user_has(id))? {
        return Ok(Pinned::Other);
    }
    let created = match pinner.identify(rel, id) {
        Ok((got, created)) if got == id => created,
        Ok(_) => return Ok(Pinned::Other),
        Err(e) => {
            tracing::debug!(error = %e, "a pin rule's item couldn't be looked at now");
            return Ok(Pinned::Failed);
        }
    };
    // The intent, durably, before the pin. Not written (claimed by the
    // user meanwhile, or gone from the tree): not the rule's to pin.
    if !store.write_durable(|o| o.pin_owned_add(id, created, at_seq))? {
        return Ok(Pinned::Other);
    }
    match pinner.set(rel, (id, Some(created)), Change::Pin) {
        Ok(()) => {
            store.write(|o| o.pin_retry_remove(id))?;
            Ok(Pinned::Done)
        }
        Err(e) => {
            store.write(|o| o.pin_owned_remove(id))?;
            Ok(if is_other_item(&e) { Pinned::Other } else { Pinned::Failed })
        }
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
    /// Items a rule keeps that lost their pin through a folder's "Free up
    /// space", pinned again.
    pub repinned: usize,
    /// Files a rule kept that were freed by name: released from the rules.
    pub released: usize,
    /// Work is left for the next pass.
    pub more: bool,
}

/// One pass: brings pin states in line with `pins`, for up to `budget`.
/// The walk's state lives in the store, so a pass can stop anywhere.
pub fn apply(store: &mut Store, pinner: &mut dyn Pinner, pins: &PinSet, budget: Duration, now_ms: i64) -> Result<Pass> {
    let started = Instant::now();
    let out_of_time = || started.elapsed() >= budget;
    let mut pass = Pass::default();
    let fp = pins.fingerprint();
    take_approval(store, &fp)?;
    // After a rescan, first: what it set aside is the rules' and the
    // user's again, and a user's folder waiting for its pin is queued.
    store.write(|o| o.pins_rebind())?;
    retry(store, pinner, pins, now_ms, &mut pass)?;
    repin(store, pinner, pins, now_ms, &mut pass)?;

    let last = store.read(|o| o.activity_last_seq())?;
    let seen: Option<i64> = store.read(|o| o.meta_get(KEY_SEQ))?.and_then(|s| s.parse().ok());
    let mut applied = store.read(|o| o.meta_get(KEY_APPLIED))?;
    // Entries dropped from the log before they were looked at, or items
    // the retry table had no room for (as soon as it has room again): walk
    // again. The mark stays until that walk is under way.
    let rewalk = store
        .read(|o| Ok(o.meta_get(KEY_REWALK)?.is_some() && o.pin_retry_count()? < oneup_store::pins::PIN_RETRY_MAX))?;
    let lost = seen.is_some_and(|s| last - s > oneup_store::activity::KEEP);
    if lost || rewalk {
        applied = None;
    }
    if applied.as_deref() != Some(fp.as_str()) {
        let walk = store.read(|o| o.meta_get(KEY_WALK))?.and_then(|t| Walk::from_json(&t));
        // No rules, and none of theirs to undo: nothing to walk for.
        if pins.is_empty() && walk.is_none() && no_owned(store)? {
            store.write(|o| {
                o.meta_set(KEY_APPLIED, &fp)?;
                o.meta_set(KEY_SEQ, &last.to_string())
            })?;
            return Ok(pass);
        }
        let mut w = match walk {
            // A walk under way carries on (a mark set meanwhile brings
            // another once it's done); entries lost from the log start over.
            Some(w) if w.fingerprint == fp && !lost => w,
            _ => {
                let Some(r) = store.read(|o| o.local_root())? else { return Ok(pass) };
                let w = Walk {
                    fingerprint: fp.clone(),
                    seq: last,
                    owned: Some(None),
                    stack: vec![Frame { id: r.root_file_id, rel: String::new(), after: None }],
                };
                // Under way, saved: the reasons to walk again are settled.
                store.write(|o| {
                    o.meta_set(KEY_WALK, &w.to_json())?;
                    o.meta_delete(KEY_REWALK)?;
                    o.meta_delete(KEY_APPLIED)?;
                    o.meta_set(KEY_SEQ, &last.to_string())
                })?;
                w
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
                if !pins.pins(&rel, row.kind != LocalKind::File) {
                    unpin(store, pinner, &row, &rel, now_ms, &mut pass)?;
                }
            }
            w.owned = Some(Some(end));
            if out_of_time() {
                return save_walk(store, KEY_WALK, &w, pass);
            }
        }
        // The tree, depth first, a page of one folder at a time (nothing to
        // pin without rules).
        if pins.is_empty() {
            w.stack.clear();
        }
        if !walk_tree(store, pinner, pins, &mut w, last, now_ms, &out_of_time, &mut pass)? {
            return save_walk(store, KEY_WALK, &w, pass);
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
    // Folders moved since: their contents at the new place.
    if !moved_folders(store, pinner, pins, last, now_ms, &out_of_time, &mut pass)? {
        pass.more = true;
        return Ok(pass);
    }
    // The rules are applied: only what changed since.
    let seen = seen.unwrap_or(last);
    if pins.is_empty() && no_owned(store)? {
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
                reconcile(store, pinner, &row, &rel, pins, Some(*seq), last, now_ms, &mut pass)?;
                // A folder moved: everything in it, at its new place.
                if moved_to(detail).is_some() && row.kind == LocalKind::Folder {
                    store.write(|o| o.pin_subtree_add(row.file_id))?;
                    pass.more = true;
                }
            }
        }
        if out_of_time() {
            cut = true;
            break;
        }
    }
    pass.more |= cut || entries.len() == PER_PASS;
    if upto != seen {
        store.write(|o| o.meta_set(KEY_SEQ, &upto.to_string()))?;
    }
    Ok(pass)
}

fn no_owned(store: &Store) -> Result<bool> {
    Ok(store.read(|o| o.pin_owned_page(None, 1))?.is_empty())
}

fn save_walk(store: &mut Store, key: &str, w: &Walk, mut pass: Pass) -> Result<Pass> {
    store.write(|o| o.meta_set(key, &w.to_json()))?;
    pass.more = true;
    Ok(pass)
}

/// Walks `w`'s stack depth first, a page of one folder at a time,
/// reconciling each item with the rules. `false`: out of time (the walk
/// holds where it is).
#[allow(clippy::too_many_arguments)]
fn walk_tree(
    store: &mut Store,
    pinner: &mut dyn Pinner,
    pins: &PinSet,
    w: &mut Walk,
    last: i64,
    now_ms: i64,
    out_of_time: &dyn Fn() -> bool,
    pass: &mut Pass,
) -> Result<bool> {
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
            reconcile(store, pinner, &c, &rel, pins, None, last, now_ms, pass)?;
            if c.kind == LocalKind::Folder && c.skip == Skip::None {
                down = Some(Frame { id: c.file_id, rel, after: None });
                break;
            }
        }
        if let Some(f) = down {
            w.stack.push(f);
        }
        if out_of_time() && !w.stack.is_empty() {
            return Ok(false);
        }
    }
    Ok(true)
}

/// Checks the contents of the folders moved since against the rules at
/// their new place (`pin_subtree`), one at a time, a walk saved between
/// passes. `false`: out of time.
fn moved_folders(
    store: &mut Store,
    pinner: &mut dyn Pinner,
    pins: &PinSet,
    last: i64,
    now_ms: i64,
    out_of_time: &dyn Fn() -> bool,
    pass: &mut Pass,
) -> Result<bool> {
    loop {
        let mut w = match store.read(|o| o.meta_get(KEY_SUBWALK))?.and_then(|t| Walk::from_json(&t)) {
            Some(w) => w,
            None => {
                let Some(id) = store.read(|o| o.pin_subtree_next())? else { return Ok(true) };
                let rel = store.read(|o| o.local_path_of(id))?.map(|p| crate::filetree::clean(&p));
                let Some(rel) = rel else {
                    store.write(|o| o.pin_subtree_done(id))?;
                    continue;
                };
                let w = Walk {
                    fingerprint: id.to_string(),
                    seq: last,
                    owned: None,
                    stack: vec![Frame { id, rel, after: None }],
                };
                store.write(|o| o.meta_set(KEY_SUBWALK, &w.to_json()))?;
                w
            }
        };
        if !walk_tree(store, pinner, pins, &mut w, last, now_ms, out_of_time, pass)? {
            store.write(|o| o.meta_set(KEY_SUBWALK, &w.to_json()))?;
            return Ok(false);
        }
        let id: u128 = w.fingerprint.parse().unwrap_or(0);
        store.write(|o| {
            o.pin_subtree_done(id)?;
            o.meta_delete(KEY_SUBWALK)
        })?;
        if out_of_time() {
            return Ok(false);
        }
    }
}

/// Tries again what couldn't be set before, those due: pins the rules
/// still want, and unpins of the rules' own items they no longer match.
fn retry(store: &mut Store, pinner: &mut dyn Pinner, pins: &PinSet, now_ms: i64, pass: &mut Pass) -> Result<()> {
    let last = store.read(|o| o.activity_last_seq())?;
    for (id, op, tries) in store.read(|o| o.pin_retry_due(now_ms, RETRIES))? {
        let row = store.read(|o| o.local_get(id))?;
        let rel = match &row {
            Some(_) => store.read(|o| o.local_path_of(id))?.map(|p| crate::filetree::clean(&p)),
            None => None,
        };
        let (Some(row), Some(rel)) = (row, rel) else {
            store.write(|o| o.pin_retry_remove(id))?;
            continue;
        };
        let pinned = row.attributes & PINNED != 0;
        let released = !pinned && store.read(|o| o.pin_released_has(id))?;
        let users = store.read(|o| o.pin_user_has(id))?;
        let matches = !released && !users && pins.pins(&rel, row.kind != LocalKind::File);
        let owned = !users && store.read(|o| o.pin_owned_has(id))?;
        let change = match op {
            PinOp::Pin if matches && !pinned => Change::Pin,
            PinOp::Unpin if !matches && owned && pinned => Change::Unpin,
            _ => {
                // Settled meanwhile (an unpin no longer wanted keeps the note).
                store.write(|o| {
                    if op == PinOp::Unpin && !matches && !pinned {
                        o.pin_owned_remove(id)?;
                    }
                    o.pin_retry_remove(id)
                })?;
                continue;
            }
        };
        let failed = match change {
            Change::Pin => match pin_item(store, pinner, id, &rel, last)? {
                Pinned::Done => {
                    pass.pinned += 1;
                    false
                }
                Pinned::Other => {
                    store.write(|o| o.pin_retry_remove(id))?;
                    false
                }
                Pinned::Failed => true,
            },
            Change::Unpin => {
                // Read again right before acting: the user may have claimed it.
                let (users, created) = store.read(|o| Ok((o.pin_user_has(id)?, o.pin_owned_created(id)?)))?;
                if users || created.is_none() {
                    store.write(|o| o.pin_retry_remove(id))?;
                    continue;
                }
                match pinner.set(&rel, (id, created), Change::Unpin) {
                    Ok(()) => {
                        store.write(|o| {
                            o.pin_owned_remove(id)?;
                            o.pin_retry_remove(id)
                        })?;
                        pass.unpinned += 1;
                        false
                    }
                    // Not provably the rule's item: the user's.
                    Err(e) if is_other_item(&e) => {
                        store.write(|o| {
                            o.pin_owned_remove(id)?;
                            o.pin_retry_remove(id)
                        })?;
                        false
                    }
                    Err(_) => true,
                }
            }
        };
        if failed {
            store.write(|o| o.pin_retry_put(id, op, tries + 1, now_ms + back_off_ms(tries)).map(|_| ()))?;
            pass.waiting += 1;
        }
    }
    Ok(())
}

/// Pins again the items a rule keeps that lost their pin since (queued by
/// the store from the journal's attribute changes); one no rule keeps any
/// more isn't the rules' any more. A queued item leaves the queue only
/// once pinned, handed to `pin_retry`, or needing nothing.
fn repin(store: &mut Store, pinner: &mut dyn Pinner, pins: &PinSet, now_ms: i64, pass: &mut Pass) -> Result<()> {
    for id in store.read(|o| o.pin_changed_page(CHANGED))? {
        let row = store.read(|o| o.local_get(id))?;
        let rel = match &row {
            Some(_) => store.read(|o| o.local_path_of(id))?.map(|p| crate::filetree::clean(&p)),
            None => None,
        };
        let (Some(row), Some(rel)) = (row, rel) else {
            store.write(|o| o.pin_changed_done(id))?;
            continue;
        };
        if row.attributes & PINNED != 0 {
            // A folder the user pinned: its creation time, read on a
            // handle, so its pin is only ever put back on this folder.
            if row.kind == LocalKind::Folder
                && store.read(|o| Ok(o.pin_user_has(id)? && o.pin_user_created(id)?.is_none()))?
            {
                match pinner.identify(&rel, id) {
                    Ok((got, created)) if got == id => store.write(|o| o.pin_user_set_created(id, created))?,
                    Ok(_) => {}
                    // Can't be read now: it stays queued for the next pass.
                    Err(_) => continue,
                }
            }
            store.write(|o| o.pin_changed_done(id))?;
            continue;
        }
        // A folder the user pinned: a "Free up space" on a folder above
        // doesn't undo it (it's pinned again); on the folder itself, it's
        // the user's choice and the note goes (one pass later).
        if row.kind == LocalKind::Folder && store.read(|o| o.pin_user_has(id))? {
            if !kept_through_folder_above(store, &row)? {
                // The folder above may be marked by a record this pass
                // hasn't seen yet: one more pass before it's the user's
                // choice on the folder itself.
                if store.read(|o| o.pin_changed_seen(id))? {
                    store.write(|o| {
                        o.pin_user_remove(id)?;
                        o.pin_changed_done(id)
                    })?;
                } else {
                    store.write(|o| o.pin_changed_seen_mark(id))?;
                }
                continue;
            }
            // Only on the folder the user pinned: its creation time on the
            // handle, once read. (A note put back by a rescan always has
            // one; until it's read, the row itself, kept since, is proof.)
            let created = store.read(|o| o.pin_user_created(id))?;
            match pinner.set(&rel, (id, created), Change::Pin) {
                Ok(()) => {
                    store.write(|o| o.pin_changed_done(id))?;
                    pass.repinned += 1;
                }
                Err(e) if is_other_item(&e) => {
                    store.write(|o| {
                        o.pin_user_remove(id)?;
                        o.pin_changed_done(id)
                    })?;
                }
                // Not now (in use, say): it stays queued for the next pass.
                Err(_) => pass.waiting += 1,
            }
            continue;
        }
        if !pins.pins(&rel, row.kind != LocalKind::File) {
            store.write(|o| {
                o.pin_owned_remove(id)?;
                o.pin_changed_done(id)
            })?;
            continue;
        }
        if freed_by_name(store, &row)? {
            // "Free up space" on the file itself: the person's choice wins.
            store.write(|o| {
                o.pin_released_add(id)?;
                o.pin_changed_done(id)
            })?;
            pass.released += 1;
            continue;
        }
        let created = store.read(|o| o.pin_owned_created(id))?;
        let set = pinner.set(&rel, (id, created), Change::Pin);
        if set.is_ok() {
            store.write(|o| o.pin_changed_done(id))?;
            pass.repinned += 1;
        } else if set.as_ref().is_err_and(is_other_item) {
            // Another item there: not the rule's.
            store.write(|o| {
                o.pin_owned_remove(id)?;
                o.pin_changed_done(id)
            })?;
        } else {
            // Handed to the retries, durably, before it leaves the queue;
            // with those full it stays queued for the next pass.
            store.write(|o| {
                if o.pin_retry_put(id, PinOp::Pin, 1, now_ms + back_off_ms(0))? {
                    o.pin_changed_done(id)?;
                }
                Ok(())
            })?;
            pass.waiting += 1;
        }
    }
    Ok(())
}

/// Whether a folder above `row` is set to "free up space" (File Explorer
/// marks every folder down to the one chosen): a walk up the parents.
fn kept_through_folder_above(store: &Store, row: &LocalRow) -> Result<bool> {
    let mut cur = row.parent_id;
    for _ in 0..4096 {
        let Some(p) = cur else { return Ok(false) };
        let Some(parent) = store.read(|o| o.local_get(p))? else { return Ok(false) };
        if parent.attributes & UNPINNED != 0 {
            return Ok(true);
        }
        cur = parent.parent_id;
    }
    Ok(false)
}

/// Whether `row`, set to "free up space" and not pinned, was freed by name
/// rather than through a folder above it: File Explorer marks each item it
/// frees, the chosen folder too, so a file whose folder isn't marked was
/// chosen itself (a folder freed later, on its own, can't be told apart).
pub fn freed_by_name(store: &Store, row: &LocalRow) -> Result<bool> {
    // Only "Free up space" on a file itself wins over a rule; a folder a
    // rule keeps stays kept, whoever frees it.
    if row.kind != LocalKind::File || row.attributes & PINNED != 0 || row.attributes & UNPINNED == 0 {
        return Ok(false);
    }
    let parent = match row.parent_id {
        Some(p) => store.read(|o| o.local_get(p))?,
        None => None,
    };
    // The synced folder too: freed itself, it was a folder-level request.
    Ok(parent.is_none_or(|p| p.attributes & UNPINNED == 0))
}

/// Whether a pin rule keeps the file `id` at `rel`: the rules match it and
/// it wasn't freed by name since (`pin_released`).
pub fn kept_by_rule(store: &Store, pins: &PinSet, id: u128, rel: &str) -> Result<bool> {
    Ok(pins.pins(rel, false) && !store.read(|o| o.pin_released_has(id))?)
}

/// `oneup pin PATH`: the person keeps the item at `rel` on this device
/// themselves, so it's theirs: no rule's note, retry or release stays on it
/// or, for a folder, on anything in it (which `oneup pin` pins too). Read a
/// page at a time. Returns how many items it made the user's.
/// Each page's claims are one durable transaction (written ahead of the
/// pin the caller sets).
pub fn claim_for_user(store: &mut Store, rel: &str) -> Result<usize> {
    let rel = crate::filetree::clean(rel);
    let Some(r) = store.read(|o| o.local_root())? else { return Ok(0) };
    let Some(row) = store.read(|o| row_at(o, r.root_file_id, &rel))? else { return Ok(0) };
    store.write_durable(|o| o.pin_claim_for_user(row.file_id))?;
    let mut n = 1;
    if row.kind != LocalKind::Folder {
        return Ok(n);
    }
    let mut stack = vec![Frame { id: row.file_id, rel, after: None }];
    while let Some(top) = stack.last().cloned() {
        let page = store.read(|o| o.local_children_page(top.id, top.after, PAGE))?;
        if page.is_empty() {
            stack.pop();
            continue;
        }
        // Up to the first folder (which the walk goes into next): claimed in
        // one transaction; the rest of the page is read again from there.
        let upto = page.iter().position(|c| c.kind == LocalKind::Folder && c.skip == Skip::None);
        let take = upto.map_or(page.len(), |i| i + 1);
        store.write_durable(|o| {
            for c in &page[..take] {
                o.pin_claim_for_user(c.file_id)?;
            }
            Ok(())
        })?;
        n += take;
        let last = &page[take - 1];
        if let Some(f) = stack.last_mut() {
            f.after = Some(last.file_id);
        }
        if upto.is_some() {
            stack.push(Frame { id: last.file_id, rel: String::new(), after: None });
        }
    }
    Ok(n)
}

/// `oneup pin PATH`: the person's claim on the item (and a folder's
/// contents) is recorded first, durably ([`claim_for_user`]), then `pin`
/// sets the pin. A crash or an agent pass in between finds the item the
/// user's, so no rule undoes the pin; running it again finishes it.
pub fn keep_by_hand(store: &mut Store, rel: &str, pin: &mut dyn FnMut() -> Result<()>) -> Result<usize> {
    let n = claim_for_user(store, rel)?;
    pin()?;
    Ok(n)
}

/// Whether the rules' note on item `id` at `rel` still names that item:
/// its file ID and creation time, read on a handle, are the recorded ones.
/// A note that doesn't (a rescan put it back on an ID another file reuses)
/// is dropped: the item is the user's. What counts against the rules'
/// download budget asks this first.
pub fn owned_verified(store: &mut Store, pinner: &mut dyn Pinner, id: u128, rel: &str) -> Result<Option<bool>> {
    let Some(created) = store.read(|o| o.pin_owned_created(id))? else { return Ok(Some(false)) };
    match pinner.identify(rel, id) {
        Ok((got, c)) if got == id && c == created => Ok(Some(true)),
        Ok(_) => {
            store.write(|o| o.pin_owned_remove(id))?;
            Ok(Some(false))
        }
        // Can't be read now: unproven either way; the caller leaves it
        // alone this pass (neither charged nor fetched).
        Err(_) => Ok(None),
    }
}

/// Whether a folder above `row` keeps it on this device: one pinned, or
/// one the user pinned (`pin_user`, kept even while a "Free up space" on a
/// folder above has taken its attribute). A walk up the parents, for one
/// candidate at a time.
pub fn kept_by_folder_above(store: &Store, row: &LocalRow) -> Result<bool> {
    let mut cur = row.parent_id;
    for _ in 0..4096 {
        let Some(p) = cur else { return Ok(false) };
        let Some(parent) = store.read(|o| o.local_get(p))? else { return Ok(false) };
        if parent.attributes & PINNED != 0 || store.read(|o| o.pin_user_has(p))? {
            return Ok(true);
        }
        cur = parent.parent_id;
    }
    Ok(false)
}

/// Marks file `id` as started, durably, before its download: a reservation
/// a crash leaves is never refunded for a started file (bytes may have
/// come), so an approval can't be spent twice.
pub fn mark_started(store: &mut Store, id: u128) -> Result<()> {
    store.write_durable(|o| {
        let mut list = o.meta_get(KEY_STARTED)?.unwrap_or_default();
        if !list.is_empty() {
            list.push(',');
        }
        list.push_str(&id.to_string());
        o.meta_set(KEY_STARTED, &list)
    })?;
    Ok(())
}

/// What a pass of downloads ([`fetch_pass`]) did.
#[derive(Debug, Default)]
pub struct Fetch {
    /// Downloaded: path, file ID, size.
    pub done: Vec<(String, u128, u64)>,
    /// Tried and failed (charged all the same).
    pub attempted: Vec<u128>,
    /// What stopped the pass before the end of its batch.
    pub stopped: Option<Stop>,
    /// More to fetch after this pass.
    pub more: bool,
    /// The first of the rules' files past the budget (ID, size): the
    /// download that needs a go-ahead.
    pub blocked: Option<(u128, u64)>,
    /// Files read (at most [`FETCH_READ_MAX`]).
    pub read: usize,
}

/// The most files kept on this device a download pass reads (and, for
/// the rules' own, opens to check), downloaded or passed over.
pub const FETCH_READ_MAX: usize = 1024;

/// Where the next download pass's scan starts reading (`meta`, a file ID;
/// none: from the first). It only moves forward, wrapping at the end.
pub const KEY_FETCH_AFTER: &str = "pins.fetch_after";

/// The rules' file that waits for a go-ahead (`meta`, a file ID): the one
/// the go-ahead is for, tried first by each pass until it's downloaded or
/// no longer the rules' to fetch.
pub const KEY_FETCH_WAITING: &str = "pins.fetch_waiting";

/// What the remembered waiting file is this pass.
enum Waiting {
    /// None remembered, or forgotten now.
    None,
    /// Fits the budget now: fetched first.
    Fetch(u128, u64),
    /// Still past the budget, or unreadable now: remembered, its bytes
    /// held from the rules' other files.
    Held(u128, u64),
}

/// The remembered waiting file, checked: forgotten if it's gone, here
/// already, no longer kept on this device, released or no longer the
/// rules'.
fn waiting_file(store: &mut Store, pinner: &mut dyn Pinner, budget: i64) -> Result<Waiting> {
    let Some(id) = store.read(|o| o.meta_get(KEY_FETCH_WAITING))?.and_then(|v| v.parse::<u128>().ok()) else {
        return Ok(Waiting::None);
    };
    let row = store.read(|o| o.local_get(id))?;
    let to_fetch = row
        .as_ref()
        .is_some_and(|r| r.kind == LocalKind::File && r.attributes & PINNED != 0 && r.attributes & RECALL != 0);
    let rel = store.read(|o| o.local_path_of(id))?.map(|p| crate::filetree::clean(&p));
    let owned = match (to_fetch, rel) {
        (true, Some(rel)) if !store.read(|o| o.pin_released_has(id))? => owned_verified(store, pinner, id, &rel)?,
        _ => Some(false),
    };
    let size = row.map_or(0, |r| u64::try_from(r.size).unwrap_or(0));
    Ok(match owned {
        Some(false) => {
            store.write(|o| o.meta_delete(KEY_FETCH_WAITING))?;
            Waiting::None
        }
        Some(true) if within_budget(budget, size) => Waiting::Fetch(id, size),
        _ => Waiting::Held(id, size),
    })
}

/// One pass of downloads of the files kept on this device whose content
/// isn't here. First the rules' file that waits for a go-ahead
/// ([`KEY_FETCH_WAITING`]): downloaded once it fits the budget (that's the
/// go-ahead), its bytes held from the rules' other files until then. Then
/// a scan, up to a batch, read a page at a time by file ID from where the
/// last scan stopped ([`KEY_FETCH_AFTER`], wrapping around at the end), at
/// most [`FETCH_READ_MAX`] files. A file the scan won't download is passed
/// over and the scan moves on: one of the rules' past the budget (the
/// first is remembered as the waiting file, if none is), or one whose
/// identity can't be read now (neither charged nor fetched).
/// `may` says whether a file of a size may download now (a pause, the
/// disk, the pass's share), and stops the pass if not; the next scan then
/// starts at that file. `fetch` downloads one (whether it's here now).
/// Budget is reserved durably before any download and each of the rules'
/// own is marked started before its own; the caller settles after. `more`
/// only when this pass downloaded or tried something: a pass that only
/// passed files over doesn't bring another soon.
pub fn fetch_pass(
    store: &mut Store,
    pinner: &mut dyn Pinner,
    may: &mut dyn FnMut(u64) -> Result<(), Stop>,
    fetch: &mut dyn FnMut(&str) -> bool,
) -> Result<Fetch> {
    let batch = oneup_exec::placement::BATCH;
    let mut budget = budget(store)?;
    let mut out = Fetch::default();
    // Each candidate with the scan position before it (none for the
    // waiting file), to resume at if the pass stops there.
    let mut candidates: Vec<(u128, u64, bool, Option<Option<u128>>)> = Vec::with_capacity(batch);
    let waiting = waiting_file(store, pinner, budget)?;
    let mut remembered = None;
    match waiting {
        Waiting::None => {}
        Waiting::Fetch(id, size) => {
            budget = budget.saturating_sub(i64::try_from(size).unwrap_or(i64::MAX));
            candidates.push((id, size, true, None));
            remembered = Some(id);
        }
        Waiting::Held(id, size) => {
            if !within_budget(budget, size) {
                out.blocked = Some((id, size));
            }
            // The bytes approved for it are its own.
            budget = budget.saturating_sub(i64::try_from(size).unwrap_or(i64::MAX));
            remembered = Some(id);
        }
    }
    let start: Option<u128> = store.read(|o| o.meta_get(KEY_FETCH_AFTER))?.and_then(|v| v.parse().ok());
    let (mut after, mut upto, mut wrapped) = (start, None, start.is_none());
    let mut read = 0;
    // Past the budget this pass: the rest of the rules' files wait, unread.
    let mut past = false;
    // Where the next scan starts: after the last file read, or from the
    // first once every file was read.
    let mut resume = None;
    'pages: loop {
        let want = batch.min(FETCH_READ_MAX - read);
        let page = store.read(|o| o.local_to_fetch_after(after, upto, want))?;
        for r in &page {
            let before = after;
            after = Some(r.file_id);
            read += 1;
            if Some(r.file_id) == remembered {
                continue;
            }
            let size = u64::try_from(r.size).unwrap_or(0);
            let owned = if store.read(|o| o.pin_owned_has(r.file_id))? {
                if past {
                    continue;
                }
                let Some(rel) = store.read(|o| o.local_path_of(r.file_id))? else { continue };
                match owned_verified(store, pinner, r.file_id, &crate::filetree::clean(&rel))? {
                    Some(o) => o,
                    None => continue,
                }
            } else {
                false
            };
            if owned {
                if !within_budget(budget, size) {
                    past = true;
                    if remembered.is_none() {
                        store.write(|o| o.meta_set(KEY_FETCH_WAITING, &r.file_id.to_string()))?;
                        remembered = Some(r.file_id);
                        out.blocked = Some((r.file_id, size));
                    }
                    continue;
                }
                budget = budget.saturating_sub(i64::try_from(size).unwrap_or(i64::MAX));
            }
            candidates.push((r.file_id, size, owned, Some(before)));
            if candidates.len() >= batch {
                out.more = true;
                resume = after;
                break 'pages;
            }
        }
        if read >= FETCH_READ_MAX {
            resume = after;
            break;
        }
        if page.len() < want {
            // The end: once around from the first, up to where this scan
            // began.
            if wrapped {
                break;
            }
            (after, upto, wrapped) = (None, start, true);
        }
    }
    let list: Vec<(u128, u64, bool)> = candidates.iter().map(|c| (c.0, c.1, c.2)).collect();
    let (reserved, blocked) = reserve_budget(store, &list)?;
    out.blocked = out.blocked.or(blocked);
    for &(id, size, owned, before) in &candidates {
        // Not reserved after all (the budget changed meanwhile): it waits.
        if owned && !reserved.contains(&id) {
            continue;
        }
        if let Err(why) = may(size) {
            out.stopped = Some(why);
            out.more = true;
            // The next scan starts at this file (the waiting file is
            // tried first anyway).
            if let Some(before) = before {
                resume = before;
            } else {
                resume = start;
            }
            break;
        }
        let Some(rel) = store.read(|o| o.local_path_of(id))? else { continue };
        if owned {
            mark_started(store, id)?;
            if before.is_none() {
                // The go-ahead's file: tried, so no longer waiting.
                store.write(|o| o.meta_delete(KEY_FETCH_WAITING))?;
            }
        }
        if fetch(&rel) {
            out.done.push((rel, id, size));
        } else {
            out.attempted.push(id);
        }
    }
    if out.done.is_empty() && out.attempted.is_empty() && out.stopped.is_none() {
        out.more = false;
    }
    out.read = read;
    store.write(|o| match resume {
        Some(id) => o.meta_set(KEY_FETCH_AFTER, &id.to_string()),
        None => o.meta_delete(KEY_FETCH_AFTER),
    })?;
    Ok(out)
}

/// What `oneup free PATH` did ([`free_plan`]).
#[derive(Debug, Default, PartialEq, Eq)]
pub struct FreePlan {
    /// Items set to "free up space".
    pub freed: usize,
    /// Kept: pinned, inside a pinned folder, or kept by a pin rule; files.
    pub skipped: usize,
    /// The file named, kept on this device before: released from its pin
    /// (and from the pin rule that kept it, if one did).
    pub released: Option<u128>,
}

/// What freeing `rel` does. A file named itself is freed even if it's kept
/// on this device (its pin is dropped; one a pin rule keeps is released from
/// it, so the rule doesn't pin it again). A folder is freed with what's in
/// it, except what's kept: a pinned file or folder (and what's inside a
/// pinned folder) and what a pin rule keeps are skipped and counted.
/// `free(rel)` sets one item to "free up space"; the plan is read from the
/// database a page at a time, never held whole.
pub fn free_plan(
    store: &mut Store,
    pins: &PinSet,
    rel: &str,
    free: &mut dyn FnMut(&str) -> Result<()>,
) -> Result<FreePlan> {
    let rel = crate::filetree::clean(rel);
    let Some(r) = store.read(|o| o.local_root())? else { return Ok(FreePlan::default()) };
    let Some(row) = store.read(|o| row_at(o, r.root_file_id, &rel))? else {
        free(&rel)?;
        return Ok(FreePlan { freed: 1, ..FreePlan::default() });
    };
    if row.kind == LocalKind::File {
        // The release is recorded first, durably, then the pin changed: a
        // crash or an agent pass in between finds the file released, never
        // pins it again, and running it again finishes it.
        let released = (row.attributes & PINNED != 0 || pins.pins(&rel, false)).then_some(row.file_id);
        if let Some(id) = released {
            store.write_durable(|o| o.pin_released_add(id))?;
        }
        free(&rel)?;
        return Ok(FreePlan { freed: 1, released, skipped: 0 });
    }
    free(&rel)?;
    let mut plan = FreePlan { freed: 1, ..FreePlan::default() };
    let mut stack = vec![Frame { id: row.file_id, rel, after: None }];
    while let Some(top) = stack.last().cloned() {
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
            let child = format!("{}/{}", top.rel, c.name);
            let folder = c.kind == LocalKind::Folder;
            let kept = c.attributes & PINNED != 0
                || (pins.pins(&child, folder) && !store.read(|o| o.pin_released_has(c.file_id))?);
            if kept {
                plan.skipped += if folder { count_files(store, c.file_id)? } else { 1 };
                continue;
            }
            free(&child)?;
            plan.freed += 1;
            if folder && c.skip == Skip::None {
                down = Some(Frame { id: c.file_id, rel: child, after: None });
                break;
            }
        }
        if let Some(f) = down {
            stack.push(f);
        }
    }
    Ok(plan)
}

/// The files inside the folder `id`, at any depth (paged).
fn count_files(store: &Store, id: u128) -> Result<usize> {
    let mut n = 0;
    let mut stack = vec![Frame { id, rel: String::new(), after: None }];
    while let Some(top) = stack.last().cloned() {
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
            match c.kind {
                LocalKind::File => n += 1,
                LocalKind::Folder if c.skip == Skip::None => {
                    down = Some(Frame { id: c.file_id, rel: String::new(), after: None });
                    break;
                }
                _ => {}
            }
        }
        if let Some(f) = down {
            stack.push(f);
        }
    }
    Ok(n)
}

/// Records the person's yes to downloading `bytes` for the pin rules whose
/// fingerprint is `fingerprint` (what they were shown before saving them).
/// The agent adds it to the rules' budget once those are the rules.
pub fn approve(store: &mut Store, fingerprint: &str, bytes: u64) -> Result<()> {
    store.write(|o| o.meta_set(KEY_APPROVE, &format!("{fingerprint} {bytes}")))?;
    Ok(())
}

/// The go-ahead for the download the agent waits at (and `bytes` more),
/// added to the budget as it stands: what was spent past the approval
/// stays spent, so each go-ahead doesn't bring a new cushion.
pub fn allow(store: &mut Store, bytes: u64) -> Result<()> {
    store.write(|o| {
        let waiting: u64 = o.meta_get(KEY_WAITING)?.and_then(|v| v.parse().ok()).unwrap_or(0);
        let budget: i64 = o.meta_get(KEY_BUDGET)?.and_then(|v| v.parse().ok()).unwrap_or(0);
        let add = i64::try_from(waiting.saturating_add(bytes)).unwrap_or(i64::MAX);
        o.meta_set(KEY_BUDGET, &budget.saturating_add(add).to_string())?;
        o.meta_delete(KEY_WAITING)
    })?;
    Ok(())
}

/// An approval for these rules (`fp`) joins the budget as it stands
/// (spent past an earlier approval: still spent).
fn take_approval(store: &mut Store, fp: &str) -> Result<()> {
    store.write(|o| {
        let Some(a) = o.meta_get(KEY_APPROVE)? else { return Ok(()) };
        let Some((f, bytes)) = a.split_once(' ') else { return o.meta_delete(KEY_APPROVE) };
        if f != fp {
            // For rules that aren't these (not saved yet, or changed since).
            return Ok(());
        }
        let bytes: i64 = bytes.parse().unwrap_or(0);
        let budget: i64 = o.meta_get(KEY_BUDGET)?.and_then(|v| v.parse().ok()).unwrap_or(0);
        o.meta_set(KEY_BUDGET, &budget.saturating_add(bytes).to_string())?;
        o.meta_delete(KEY_WAITING)?;
        o.meta_delete(KEY_APPROVE)
    })?;
    Ok(())
}

/// The rules' download budget now (bytes; negative: past what was approved).
pub fn budget(store: &Store) -> Result<i64> {
    Ok(store.read(|o| o.meta_get(KEY_BUDGET))?.and_then(|v| v.parse().ok()).unwrap_or(0))
}

/// Whether the rules may download a file of `size` with `budget` left:
/// at most [`ASK_BYTES`] past what was approved.
pub fn within_budget(budget: i64, size: u64) -> bool {
    budget.saturating_sub(i64::try_from(size).unwrap_or(i64::MAX)) >= -i64::try_from(ASK_BYTES).unwrap_or(i64::MAX)
}

/// Reserves budget, durably and before any download, for the rules' own
/// files among `candidates` (file ID, size, whether the rules' own), in
/// the order they'd be fetched, while the budget allows: one transaction
/// takes their sizes off it and records them in `meta` [`KEY_RESERVED`].
/// Returns the reserved IDs and the first of the rules' files it couldn't
/// reserve (the download that needs a go-ahead), if any. A crash before
/// [`settle`] can't let an approval be used twice: the bytes are already
/// off the budget.
pub fn reserve_budget(store: &mut Store, candidates: &[(u128, u64, bool)]) -> Result<Reserved> {
    store
        .write_durable(|o| {
            let mut b: i64 = o.meta_get(KEY_BUDGET)?.and_then(|v| v.parse().ok()).unwrap_or(0);
            let mut taken = Vec::new();
            let mut blocked = None;
            for &(id, size, owned) in candidates {
                if !owned {
                    continue;
                }
                if !within_budget(b, size) {
                    blocked = Some((id, size));
                    break;
                }
                b = b.saturating_sub(i64::try_from(size).unwrap_or(i64::MAX));
                taken.push((id, size));
            }
            if !taken.is_empty() {
                let list = taken.iter().map(|(id, size)| format!("{id}:{size}")).collect::<Vec<_>>().join(",");
                o.meta_set(KEY_BUDGET, &b.to_string())?;
                o.meta_set(KEY_RESERVED, &list)?;
            }
            Ok((taken.into_iter().map(|(id, _)| id).collect(), blocked))
        })
        .map_err(Into::into)
}

/// What [`reserve_budget`] reserved: the file IDs, and the first of the
/// rules' files it couldn't (ID, size).
pub type Reserved = (std::collections::HashSet<u128>, Option<(u128, u64)>);

/// Settles the reservation [`reserve_budget`] made: files in `done`, or that
/// `hydrated` says are on this PC by now (downloaded before a crash), stay
/// spent; the rest go back to the budget. Run after each pass and, for a
/// reservation a crash left, before the next.
pub fn settle(store: &mut Store, done: &[u128], hydrated: &dyn Fn(u128) -> bool) -> Result<()> {
    let Some(list) = store.read(|o| o.meta_get(KEY_RESERVED))? else { return Ok(()) };
    // Started downloads are never refunded (bytes may have come).
    let started: Vec<u128> = store
        .read(|o| o.meta_get(KEY_STARTED))?
        .unwrap_or_default()
        .split(',')
        .filter_map(|i| i.parse().ok())
        .collect();
    let mut back: i64 = 0;
    for item in list.split(',').filter(|i| !i.is_empty()) {
        let Some((id, size)) = item.split_once(':') else { continue };
        let (Ok(id), Ok(size)) = (id.parse::<u128>(), size.parse::<i64>()) else { continue };
        if !done.contains(&id) && !started.contains(&id) && !hydrated(id) {
            back = back.saturating_add(size);
        }
    }
    store.write(|o| {
        let b: i64 = o.meta_get(KEY_BUDGET)?.and_then(|v| v.parse().ok()).unwrap_or(0);
        o.meta_set(KEY_BUDGET, &b.saturating_add(back).to_string())?;
        o.meta_delete(KEY_STARTED)?;
        o.meta_delete(KEY_RESERVED)
    })?;
    Ok(())
}

/// Notes the download the agent stopped at for want of a go-ahead.
pub fn wait_for_go_ahead(store: &mut Store, bytes: u64) -> Result<()> {
    store.write(|o| o.meta_set(KEY_WAITING, &bytes.to_string()))?;
    Ok(())
}

/// What the agent waits for a go-ahead to download, if anything.
pub fn waiting(store: &Store) -> Result<Option<u64>> {
    Ok(store.read(|o| o.meta_get(KEY_WAITING))?.and_then(|v| v.parse().ok()))
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

/// Brings one item in line with the rules: pinned if they match it and it
/// isn't (noted as theirs); if they don't and it's theirs, back to the
/// default. A placeholder already pinned that the rules match counts as
/// theirs only if it took the pin from a folder they pinned, after it
/// (`entry` is the activity entry that named it, later than the folder's
/// note); one pinned before is the user's. What can't be set now waits in
/// `pin_retry`, with its operation.
#[allow(clippy::too_many_arguments)]
fn reconcile(
    store: &mut Store,
    pinner: &mut dyn Pinner,
    row: &LocalRow,
    rel: &str,
    pins: &PinSet,
    entry: Option<i64>,
    last: i64,
    now_ms: i64,
    pass: &mut Pass,
) -> Result<()> {
    if !matches!(row.kind, LocalKind::File | LocalKind::Folder) {
        return Ok(());
    }
    let id = row.file_id;
    let pinned = row.attributes & PINNED != 0;
    // The user's (claimed with `oneup pin`, possibly not pinned on disk yet):
    // no rule pins or unpins it.
    if store.read(|o| o.pin_user_has(id))? {
        return Ok(());
    }
    // Freed by name: no rule keeps it until it's pinned again.
    let released = !pinned && store.read(|o| o.pin_released_has(id))?;
    if released || !pins.pins(rel, row.kind != LocalKind::File) {
        if store.read(|o| o.pin_owned_has(id))? {
            unpin(store, pinner, row, rel, now_ms, pass)?;
        }
        return Ok(());
    }
    if pinned {
        let Some(seq) = entry else {
            // Found pinned by a walk (the rules looking at it for the first
            // time, or a folder's contents after its pin): the user's, unless
            // it's the rules' already.
            store.write(|o| if o.pin_owned_has(id)? { Ok(()) } else { o.pin_user_add(id) })?;
            return Ok(());
        };
        // Named by the activity log after its folder took a rule's pin, and
        // not found pinned before by a walk of that folder: it took the pin
        // from the folder, so it's the rules'. While the folder's walk is
        // still due, it waits for that walk instead.
        let inherited = match row.parent_id {
            Some(p) => store.read(|o| {
                Ok(o.pin_owned_at(p)?.is_some_and(|at| at < seq)
                    && !o.pin_subtree_has(p)?
                    && !o.pin_user_has(id)?
                    && !o.pin_owned_has(id)?)
            })?,
            None => false,
        };
        // Only with its identity read on a handle; unread, it's the user's.
        if inherited
            && let Ok((got, created)) = pinner.identify(rel, id)
            && got == id
        {
            store.write(|o| o.pin_owned_add(id, created, seq).map(|_| ()))?;
        }
        return Ok(());
    }
    match pin_item(store, pinner, id, rel, last)? {
        Pinned::Done => {
            // A folder pinned from a change (not a walk): what's in it is
            // looked at by a walk of its own, which also tells the user's
            // pins in it from those it will pass on.
            if entry.is_some() && row.kind == LocalKind::Folder {
                store.write(|o| o.pin_subtree_add(id))?;
            }
            pass.pinned += 1;
        }
        Pinned::Other => {}
        Pinned::Failed => {
            wait(store, id, PinOp::Pin, now_ms)?;
            pass.waiting += 1;
        }
    }
    Ok(())
}

/// Sets one of the rules' own items (that they no longer match) back to the
/// default; a failure waits in `pin_retry` as an unpin, keeping the note.
fn unpin(
    store: &mut Store,
    pinner: &mut dyn Pinner,
    row: &LocalRow,
    rel: &str,
    now_ms: i64,
    pass: &mut Pass,
) -> Result<()> {
    let id = row.file_id;
    if row.attributes & PINNED == 0 {
        // Not pinned any more (the user chose otherwise): not theirs.
        store.write(|o| o.pin_owned_remove(id))?;
        return Ok(());
    }
    // Only the item the rule pinned: same file ID and creation time, read
    // on the handle that unpins it. Anything else there is the user's, and so
    // is what the user claimed meanwhile (read again right before acting).
    let (users, created) = store.read(|o| Ok((o.pin_user_has(id)?, o.pin_owned_created(id)?)))?;
    if users || created.is_none() {
        store.write(|o| o.pin_owned_remove(id))?;
        return Ok(());
    }
    match pinner.set(rel, (id, created), Change::Unpin) {
        Ok(()) => {
            store.write(|o| {
                o.pin_owned_remove(id)?;
                o.pin_retry_remove(id)
            })?;
            pass.unpinned += 1;
        }
        Err(e) if is_other_item(&e) => {
            store.write(|o| {
                o.pin_owned_remove(id)?;
                o.pin_retry_remove(id)
            })?;
        }
        Err(_) => {
            wait(store, id, PinOp::Unpin, now_ms)?;
            pass.waiting += 1;
        }
    }
    Ok(())
}

/// Puts `op` on `id` in the retries; with them full, another walk follows
/// as soon as they have room.
fn wait(store: &mut Store, id: u128, op: PinOp, now_ms: i64) -> Result<()> {
    let kept = store.write(|o| o.pin_retry_put(id, op, 1, now_ms + back_off_ms(0)))?;
    if !kept {
        store.write(|o| o.meta_set(KEY_REWALK, "1"))?;
    }
    Ok(())
}

/// Pins `rel` (a file set to "Free up space" that a pin rule keeps),
/// noting it as the rule's. Whether it worked.
pub fn pin_back(store: &mut Store, pinner: &mut dyn Pinner, id: u128, rel: &str) -> Result<bool> {
    let last = store.read(|o| o.activity_last_seq())?;
    Ok(pin_item(store, pinner, id, rel, last)? == Pinned::Done)
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

/// Downloads for the rules' items past what was approved: this much, then
/// the agent asks.
pub const ASK_BYTES: u64 = 1 << 30;

/// Whether saving pin rules needs the person's yes, from their
/// [`preview`]: whenever they'd download anything, and always when the
/// preview couldn't look at everything.
pub fn must_ask(files: u64, all: bool) -> bool {
    files > 0 || !all
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

    /// Pure: whether a file of `size` may download now, or why not. Free
    /// space that can't be read stops it: nothing is downloaded blind.
    pub fn may(&mut self, size: u64, paused: bool, free: Option<u64>, now: Instant) -> Result<(), Stop> {
        if paused {
            return Err(Stop::Paused);
        }
        let Some(free) = free else { return Err(Stop::SpaceUnknown) };
        if free < size.saturating_add(self.reserve) {
            return Err(Stop::LowDisk);
        }
        if now >= self.until || (!self.first && size > self.bytes_left) {
            return Err(Stop::Share);
        }
        self.first = false;
        self.bytes_left = self.bytes_left.saturating_sub(size);
        Ok(())
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
    use oneup_store::pins::PinOp;
    use oneup_store::{ActivityEntry, ActivityOutcome, LocalRoot};

    /// Pins in memory: which paths are pinned, which fail, and which hold
    /// another item than the database says (`others`: path to that item's
    /// file ID and creation time; otherwise the expected ID, created 0).
    #[derive(Default)]
    struct Fake {
        set: Vec<(String, Change)>,
        failing: Vec<String>,
        others: std::collections::HashMap<String, (u128, i64)>,
        seen: std::collections::HashMap<String, u128>,
    }

    impl Pinner for Fake {
        fn identify(&mut self, rel: &str, expected: u128) -> std::io::Result<(u128, i64)> {
            if self.failing.iter().any(|f| f == rel) {
                return Err(std::io::Error::other("not a placeholder yet"));
            }
            self.seen.insert(rel.to_string(), expected);
            Ok(self.others.get(rel).copied().unwrap_or((expected, 0)))
        }

        fn set(&mut self, rel: &str, expect: (u128, Option<i64>), change: Change) -> std::io::Result<()> {
            if self.failing.iter().any(|f| f == rel) {
                return Err(std::io::Error::other("not a placeholder yet"));
            }
            let (id, created) = self.others.get(rel).copied().unwrap_or((expect.0, 0));
            if id != expect.0 || expect.1.is_some_and(|c| c != created) {
                return Err(not_that_item());
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

    /// Records the attributes a pin set would have, through the observer.
    fn observe(s: &mut Store, fake: &Fake, ids: &[(u128, &str)]) {
        let mut marks = Vec::new();
        for (id, rel) in ids {
            let last = fake.set.iter().rev().find(|(r, _)| r == rel).map(|(_, c)| *c);
            if let Some(c) = last {
                let a = s.read(|o| o.local_get(*id)).unwrap().unwrap().attributes;
                marks.push((*id, if c == Change::Pin { a | PINNED } else { a & !PINNED }));
            }
        }
        journal(s, &marks);
    }

    /// Feeds the observer (`apply_records`) one attribute-change record per
    /// item, as the change journal would after File Explorer or a pin set
    /// `attributes` on it; the synced folder (ID 1) too.
    fn journal(s: &mut Store, items: &[(u128, u32)]) {
        use oneup_ntfs::FileId;
        use oneup_ntfs::dirent::DirEntry;
        use oneup_ntfs::record::{UsnRecord, reason};
        let mut records = Vec::new();
        let mut live = Vec::new();
        for &(id, attributes) in items {
            let r = s.read(|o| o.local_get(id)).unwrap().unwrap();
            records.push(UsnRecord {
                major_version: 3,
                file_id: FileId(id),
                parent_id: FileId(r.parent_id.unwrap_or(0)),
                usn: 0,
                timestamp: 0,
                reason: reason::BASIC_INFO_CHANGE | reason::CLOSE,
                source_info: 0,
                attributes,
                name: None,
                name_lossy: false,
            });
            live.push(DirEntry {
                file_id: FileId(id),
                name: r.name.clone(),
                name_lossy: false,
                size: u64::try_from(r.size).unwrap(),
                attributes,
                reparse_tag: r.reparse_tag,
                created: 0,
                modified: r.mtime,
                changed: 0,
            });
        }
        let mut stat = |l: &oneup_local::observe::Lookup| match live.iter().find(|e| e.file_id == l.id) {
            Some(e) => oneup_local::observe::Stat::Found(e.clone()),
            None => oneup_local::observe::Stat::Gone,
        };
        let mut ch = oneup_local::observe::Changes::default();
        let rules = oneup_rules::ExcludeSet::none();
        s.try_write(|o| oneup_local::observe::apply_records(o, 1, &rules, &records, &mut stat, &mut ch)).unwrap();
        assert_eq!(ch.unreadable, 0);
    }

    fn entry(path: &str) -> ActivityEntry {
        ActivityEntry {
            seq: 0,
            at_ms: 1,
            outcome: ActivityOutcome::Done,
            action: "download".into(),
            path: path.into(),
            detail: String::new(),
        }
    }

    fn run(s: &mut Store, fake: &mut Fake, pins: &PinSet, now: i64) -> Pass {
        let mut total = Pass::default();
        for _ in 0..50 {
            let p = apply(s, fake, pins, Duration::from_secs(60), now).unwrap();
            total.pinned += p.pinned;
            total.unpinned += p.unpinned;
            total.waiting += p.waiting;
            total.repinned += p.repinned;
            total.released += p.released;
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

    /// A placeholder created in a folder a rule pinned takes the folder's
    /// pin from Windows: it's the rule's too, so removing the rule undoes it.
    #[test]
    fn an_item_that_took_its_pin_from_a_rule_folder_is_the_rules() {
        let mut s = store(&[row(2, 1, "Work", LocalKind::Folder, 0x10)]);
        let mut fake = Fake::default();
        let pins = PinSet::new(&["/Work/"]);
        run(&mut s, &mut fake, &pins, 0);
        observe(&mut s, &fake, &[(2, "Work")]);
        // A new file arrives in Work, pinned by Windows already.
        s.write(|o| {
            o.local_put(&row(3, 2, "new.txt", LocalKind::File, PINNED | RECALL))?;
            o.activity_add(&[entry("/Work/new.txt")])
        })
        .unwrap();
        fake.set.clear();
        run(&mut s, &mut fake, &pins, 0);
        assert!(fake.set.is_empty(), "nothing to set");
        assert!(s.read(|o| o.pin_owned_has(3)).unwrap(), "the rule's");
        let p = run(&mut s, &mut fake, &PinSet::new::<&str>(&[]), 0);
        assert_eq!(p.unpinned, 2);
        assert!(fake.set.contains(&("Work/new.txt".to_string(), Change::Unpin)));
    }

    /// With the retry table full, another walk starts as soon as it has
    /// room again, even with rows in it that keep failing.
    #[test]
    fn a_full_retry_table_brings_a_walk_as_soon_as_it_has_room() {
        let max = oneup_store::pins::PIN_RETRY_MAX as u128;
        let mut rows: Vec<LocalRow> =
            (0..=max).map(|i| row(100 + i, 1, &format!("f{i}.psd"), LocalKind::File, 0)).collect();
        rows.push(row(50, 1, "a.psd", LocalKind::File, 0));
        let mut s = store(&rows);
        // Every file fails: the table fills, one is left out.
        let mut fake = Fake { failing: rows.iter().map(|r| r.name.clone()).collect(), ..Fake::default() };
        let pins = PinSet::new(&["*.psd"]);
        run(&mut s, &mut fake, &pins, 0);
        assert_eq!(s.read(|o| o.pin_retry_count()).unwrap(), max as usize);
        assert!(s.read(|o| o.meta_get(KEY_REWALK)).unwrap().is_some(), "one had no room");
        // The items in the table keep failing for good; the two left out
        // could be pinned now. One file in the table goes: room again.
        let waiting: Vec<u128> =
            s.read(|o| o.pin_retry_due(i64::MAX, max as usize)).unwrap().into_iter().map(|(id, _, _)| id).collect();
        let left_out: Vec<String> =
            rows.iter().filter(|r| !waiting.contains(&r.file_id)).map(|r| r.name.clone()).collect();
        assert_eq!(left_out.len(), 2);
        fake.failing.retain(|f| !left_out.contains(f));
        s.write(|o| o.local_remove_subtree(waiting[0])).unwrap();
        fake.set.clear();
        run(&mut s, &mut fake, &pins, 0);
        assert!(s.read(|o| o.meta_get(KEY_REWALK)).unwrap().is_none(), "walked again");
        let mut got: Vec<String> = fake.set.iter().map(|(r, _)| r.clone()).collect();
        got.sort_unstable();
        let mut want = left_out;
        want.sort_unstable();
        assert_eq!(got, want, "the left-out ones are pinned");
    }

    /// Sets "free up space" on items, as File Explorer does, seen by the
    /// observer from the journal.
    fn explorer_free(s: &mut Store, ids: &[u128]) {
        let marks: Vec<(u128, u32)> = ids
            .iter()
            .map(|id| {
                let a = s.read(|o| o.local_get(*id)).unwrap().unwrap().attributes;
                (*id, (a & !PINNED) | UNPINNED)
            })
            .collect();
        journal(s, &marks);
    }

    /// "Free up space" through a folder (File Explorer marks the folder and
    /// everything in it) doesn't free a file a rule keeps: seen only as
    /// attribute changes from the journal, it's pinned again by the next pass.
    #[test]
    fn free_up_space_through_a_folder_leaves_the_rules_files_pinned() {
        let mut s = store(&[row(2, 1, "Art", LocalKind::Folder, 0x10), row(3, 2, "a.psd", LocalKind::File, RECALL)]);
        let mut fake = Fake::default();
        let pins = PinSet::new(&["*.psd"]);
        run(&mut s, &mut fake, &pins, 0);
        observe(&mut s, &fake, &[(3, "Art/a.psd")]);
        explorer_free(&mut s, &[2, 3]);
        fake.set.clear();
        let p = run(&mut s, &mut fake, &pins, 0);
        assert_eq!((p.repinned, p.released), (1, 0));
        assert_eq!(fake.set, [("Art/a.psd".to_string(), Change::Pin)]);
    }

    /// "Free up space" on the file itself drops the rule's pin: the file is
    /// released, and neither the next pass nor a new walk of the rules pins
    /// it again, until someone keeps it on this device again.
    #[test]
    fn free_up_space_on_the_file_itself_releases_it_from_the_rule() {
        let mut s = store(&[row(2, 1, "Art", LocalKind::Folder, 0x10), row(3, 2, "a.psd", LocalKind::File, RECALL)]);
        let mut fake = Fake::default();
        let pins = PinSet::new(&["*.psd"]);
        run(&mut s, &mut fake, &pins, 0);
        observe(&mut s, &fake, &[(3, "Art/a.psd")]);
        explorer_free(&mut s, &[3]);
        fake.set.clear();
        let p = run(&mut s, &mut fake, &pins, 0);
        assert_eq!((p.repinned, p.released), (0, 1));
        assert!(fake.set.is_empty());
        assert!(s.read(|o| o.pin_released_has(3)).unwrap() && !s.read(|o| o.pin_owned_has(3)).unwrap());
        // The rules change (a walk): still released.
        run(&mut s, &mut fake, &PinSet::new(&["*.psd", "*.png"]), 0);
        assert!(fake.set.is_empty(), "{:?}", fake.set);
        // Kept again in File Explorer: the release ends.
        s.write(|o| {
            let mut r = o.local_get(3)?.unwrap();
            r.attributes = (r.attributes & !UNPINNED) | PINNED;
            o.local_update(&r)
        })
        .unwrap();
        assert!(!s.read(|o| o.pin_released_has(3)).unwrap());
    }

    /// `oneup free`: a file named itself is freed even if kept (and released
    /// from the rule); through a folder, what's kept (pinned, in a pinned
    /// folder, or by a pin rule) is skipped and counted.
    #[test]
    fn freeing_by_name_frees_the_file_and_through_a_folder_skips_what_is_kept() {
        let mut s = store(&[
            row(2, 1, "Docs", LocalKind::Folder, 0x10),
            row(3, 2, "a.txt", LocalKind::File, 0),
            row(4, 2, "mine.txt", LocalKind::File, PINNED),
            row(5, 2, "art.psd", LocalKind::File, 0),
            row(6, 2, "Kept", LocalKind::Folder, 0x10 | PINNED),
            row(7, 6, "x.txt", LocalKind::File, 0),
            row(8, 6, "y.txt", LocalKind::File, 0),
            row(9, 2, "Sub", LocalKind::Folder, 0x10),
            row(10, 9, "b.txt", LocalKind::File, 0),
        ]);
        let pins = PinSet::new(&["*.psd"]);
        let mut freed = Vec::new();
        let plan = free_plan(&mut s, &pins, "Docs", &mut |r| {
            freed.push(r.to_string());
            Ok(())
        })
        .unwrap();
        freed.sort();
        assert_eq!(freed, ["Docs", "Docs/Sub", "Docs/Sub/b.txt", "Docs/a.txt"]);
        assert_eq!((plan.skipped, plan.released), (4, None), "mine.txt, art.psd, Kept's two files");
        let mut freed = Vec::new();
        let plan = free_plan(&mut s, &pins, "Docs/art.psd", &mut |r| {
            freed.push(r.to_string());
            Ok(())
        })
        .unwrap();
        assert_eq!((freed, plan.released), (vec!["Docs/art.psd".to_string()], Some(5)));
        let plan = free_plan(&mut s, &pins, "Docs/mine.txt", &mut |_| Ok(())).unwrap();
        assert_eq!(plan.released, Some(4), "a pinned file named itself is freed too");
    }

    /// What the rules download is counted against what was approved, across
    /// changes of rules; past that by more than [`ASK_BYTES`], it waits.
    #[test]
    fn downloads_count_against_the_approval_whatever_changes_the_rules() {
        let mut s = store(&[]);
        let a = PinSet::new(&["*.psd"]);
        approve(&mut s, &a.fingerprint(), 100).unwrap();
        // An approval for other rules waits for those.
        take_approval(&mut s, &PinSet::new(&["*.png"]).fingerprint()).unwrap();
        assert_eq!(budget(&s).unwrap(), 0);
        take_approval(&mut s, &a.fingerprint()).unwrap();
        assert_eq!(budget(&s).unwrap(), 100);
        let spend = |s: &mut Store, bytes: u64| {
            reserve_budget(s, &[(9, bytes, true)]).unwrap();
            settle(s, &[9], &|_| false).unwrap();
        };
        spend(&mut s, 100);
        assert!(within_budget(budget(&s).unwrap(), ASK_BYTES), "a gibibyte past it");
        spend(&mut s, ASK_BYTES);
        // Another small change of rules (a hand edit) doesn't start over.
        assert!(!within_budget(budget(&s).unwrap(), 1), "asks now");
        wait_for_go_ahead(&mut s, 7).unwrap();
        assert_eq!(waiting(&s).unwrap(), Some(7));
        allow(&mut s, 0).unwrap();
        assert!(within_budget(budget(&s).unwrap(), 7) && waiting(&s).unwrap().is_none());
    }

    /// Budget is reserved, durably, before a download: a crash between the
    /// download and its settling can't let an approval be used twice. What
    /// wasn't downloaded comes back; what is on this PC by now (downloaded
    /// before the crash) stays spent.
    #[test]
    fn budget_is_reserved_before_downloading_and_settled_after_a_crash() {
        let mut s = store(&[]);
        approve(&mut s, &PinSet::new(&["*.psd"]).fingerprint(), 100).unwrap();
        take_approval(&mut s, &PinSet::new(&["*.psd"]).fingerprint()).unwrap();
        let (got, blocked) = reserve_budget(&mut s, &[(1, 60, true), (2, 5, false), (3, 30, true)]).unwrap();
        assert_eq!((got.len(), blocked), (2, None));
        assert_eq!(budget(&s).unwrap(), 10, "off before any download");
        // A crash: no settling. The next pass settles first: 1 is on this
        // PC by now, 3 isn't.
        settle(&mut s, &[], &|id| id == 1).unwrap();
        assert_eq!(budget(&s).unwrap(), 40, "only 3's bytes back");
        assert!(s.read(|o| o.meta_get(KEY_RESERVED)).unwrap().is_none());
        // Past the budget by more than ASK_BYTES: the first that doesn't fit.
        let (got, blocked) = reserve_budget(&mut s, &[(4, ASK_BYTES, true), (5, 100, true)]).unwrap();
        assert_eq!((got.len(), blocked), (1, Some((5, 100))));
    }

    /// Rule files 10.., more than a batch, ahead (by file ID) of a file the
    /// user pinned (ID 1000), all online-only, each `size`; the rules'
    /// pinned through a pass and seen pinned by the observer.
    fn many_kept(n: u128, size: i64) -> (Store, Fake, Vec<String>) {
        let mut rows: Vec<LocalRow> = (10..10 + n)
            .map(|i| LocalRow { size, ..row(i, 1, &format!("a{i}.psd"), LocalKind::File, RECALL) })
            .collect();
        rows.push(row(1000, 1, "u.txt", LocalKind::File, RECALL));
        let mut s = store(&rows);
        let mut fake = Fake::default();
        run(&mut s, &mut fake, &PinSet::new(&["*.psd"]), 0);
        let names: Vec<String> = (10..10 + n).map(|i| format!("a{i}.psd")).collect();
        let ids: Vec<(u128, &str)> = (10..10 + n).zip(names.iter().map(String::as_str)).collect();
        observe(&mut s, &fake, &ids);
        journal(&mut s, &[(1000, RECALL | PINNED)]);
        (s, fake, names)
    }

    /// Rule files whose identity can't be read now fill more than a
    /// batch: they're passed over, and the user's file after them is
    /// still downloaded.
    #[test]
    fn unreadable_rule_files_dont_hold_up_the_rest() {
        let n = oneup_exec::placement::BATCH as u128 + 8;
        let (mut s, mut fake, names) = many_kept(n, 10);
        fake.failing = names;
        let mut got = Vec::new();
        let f = fetch_pass(&mut s, &mut fake, &mut |_| Ok(()), &mut |rel| {
            got.push(rel.to_string());
            true
        })
        .unwrap();
        assert_eq!(got, ["/u.txt"]);
        assert_eq!((f.stopped, f.more, f.blocked), (None, false, None));
        assert!(s.read(|o| o.meta_get(KEY_STARTED)).unwrap().is_none(), "none of the rules' charged");
    }

    /// Rule files past the budget fill more than a batch: they wait for
    /// the go-ahead, and the user's file after them is downloaded. A
    /// pause still stops the pass.
    #[test]
    fn rule_files_past_the_budget_dont_hold_up_the_rest() {
        let n = oneup_exec::placement::BATCH as u128 + 8;
        let (mut s, mut fake, _) = many_kept(n, 2 << 30);
        fake.seen.clear();
        let mut got = Vec::new();
        let f = fetch_pass(&mut s, &mut fake, &mut |_| Ok(()), &mut |rel| {
            got.push(rel.to_string());
            true
        })
        .unwrap();
        assert_eq!(got, ["/u.txt"]);
        assert_eq!(f.blocked, Some((10, 2 << 30)), "the first asks");
        assert_eq!((f.stopped, f.more), (None, false));
        assert_eq!(fake.seen.len(), 1, "the rest past the budget aren't read");
        got.clear();
        let f = fetch_pass(&mut s, &mut fake, &mut |_| Err(Stop::Paused), &mut |rel| {
            got.push(rel.to_string());
            true
        })
        .unwrap();
        assert!(got.is_empty());
        assert_eq!(f.stopped, Some(Stop::Paused));
    }

    /// A go-ahead or a new approval is added to the budget as it stands:
    /// what a pass spent past the approval isn't forgiven.
    #[test]
    fn a_go_ahead_doesnt_forgive_what_was_spent_past_the_approval() {
        const MIB: i64 = 1 << 20;
        let (mut s, mut fake, _) = many_kept(3, 600 * MIB);
        let f = fetch_pass(&mut s, &mut fake, &mut |_| Ok(()), &mut |_| true).unwrap();
        let done: Vec<u128> = f.done.iter().map(|d| d.1).collect();
        settle(&mut s, &done, &|_| false).unwrap();
        assert_eq!(done, [10, 1000], "the first of the rules', and the user's");
        assert_eq!(f.blocked, Some((11, 600 << 20)));
        assert_eq!(budget(&s).unwrap(), -600 * MIB);
        // Downloaded: the observer sees them here.
        journal(&mut s, &[(10, PINNED), (1000, PINNED)]);
        wait_for_go_ahead(&mut s, 600 << 20).unwrap();
        allow(&mut s, 2 << 30).unwrap();
        assert_eq!(budget(&s).unwrap(), -600 * MIB + 600 * MIB + (2 << 30));
        // Spent down past zero again, then a new approval for these rules.
        let f = fetch_pass(&mut s, &mut fake, &mut |_| Ok(()), &mut |_| true).unwrap();
        let done: Vec<u128> = f.done.iter().map(|d| d.1).collect();
        settle(&mut s, &done, &|_| false).unwrap();
        assert_eq!(budget(&s).unwrap(), (2 << 30) - 1200 * MIB);
        let pins = PinSet::new(&["*.psd"]);
        s.write(|o| o.meta_set(KEY_BUDGET, &(-900 * MIB).to_string())).unwrap();
        approve(&mut s, &pins.fingerprint(), 1 << 30).unwrap();
        apply(&mut s, &mut fake, &pins, Duration::from_secs(60), 0).unwrap();
        assert_eq!(budget(&s).unwrap(), -900 * MIB + (1 << 30));
    }

    /// The cost of a pass is bounded, whatever is ahead of the files it
    /// can download: more rule files than a pass reads, each unreadable,
    /// ahead (in the index's order) of a file the user pinned. The first
    /// pass reads at most its bound, opens no more, downloads nothing and
    /// brings no pass soon; the next resumes where it stopped and reaches
    /// the user's file.
    #[test]
    fn a_download_pass_reads_a_bounded_number_of_files_and_resumes() {
        struct Counting(Fake, usize);
        impl Pinner for Counting {
            fn identify(&mut self, rel: &str, expected: u128) -> std::io::Result<(u128, i64)> {
                self.1 += 1;
                self.0.identify(rel, expected)
            }
            fn set(&mut self, rel: &str, expect: (u128, Option<i64>), change: Change) -> std::io::Result<()> {
                self.0.set(rel, expect, change)
            }
        }
        let n = FETCH_READ_MAX as u128 + 50;
        // Last in the index's order (file ID blobs, low byte first).
        let user: u128 = 0x00FF_FFFF;
        let mut rows: Vec<LocalRow> =
            (10..10 + n).map(|i| row(i, 1, &format!("a{i}.psd"), LocalKind::File, RECALL)).collect();
        rows.push(row(user, 1, "u.txt", LocalKind::File, RECALL));
        let mut s = store(&rows);
        let mut fake = Fake::default();
        run(&mut s, &mut fake, &PinSet::new(&["*.psd"]), 0);
        let names: Vec<String> = (10..10 + n).map(|i| format!("a{i}.psd")).collect();
        let ids: Vec<(u128, &str)> = (10..10 + n).zip(names.iter().map(String::as_str)).collect();
        observe(&mut s, &fake, &ids);
        journal(&mut s, &[(user, RECALL | PINNED)]);
        fake.failing = names;
        let mut p = Counting(fake, 0);
        let mut got = Vec::new();
        let f = fetch_pass(&mut s, &mut p, &mut |_| Ok(()), &mut |rel| {
            got.push(rel.to_string());
            true
        })
        .unwrap();
        assert!(got.is_empty());
        assert_eq!((f.read, f.more), (FETCH_READ_MAX, false), "bounded, and nothing soon");
        assert!(p.1 <= FETCH_READ_MAX, "opened {}", p.1);
        p.1 = 0;
        let f = fetch_pass(&mut s, &mut p, &mut |_| Ok(()), &mut |rel| {
            got.push(rel.to_string());
            true
        })
        .unwrap();
        assert_eq!(got, ["/u.txt"], "resumed where the last pass stopped");
        assert!(f.read <= FETCH_READ_MAX && p.1 <= FETCH_READ_MAX);
    }

    /// The file a go-ahead was given for is the one downloaded, however
    /// many files the pass read: the next pass starts at it. The user's
    /// files in the window still download meanwhile.
    #[test]
    fn a_go_ahead_downloads_the_file_it_was_given_for() {
        let n = FETCH_READ_MAX as u128 + 76;
        let (mut s, mut fake, _) = many_kept(n, 2 << 30);
        let mut got = Vec::new();
        let f = fetch_pass(&mut s, &mut fake, &mut |_| Ok(()), &mut |rel| {
            got.push(rel.to_string());
            true
        })
        .unwrap();
        let (first, size) = f.blocked.unwrap();
        assert_eq!(f.read, FETCH_READ_MAX);
        settle(&mut s, &[], &|_| false).unwrap();
        wait_for_go_ahead(&mut s, size).unwrap();
        allow(&mut s, 0).unwrap();
        got.clear();
        let f = fetch_pass(&mut s, &mut fake, &mut |_| Ok(()), &mut |rel| {
            got.push(rel.to_string());
            true
        })
        .unwrap();
        let rel = s.read(|o| o.local_path_of(first)).unwrap().unwrap();
        assert_eq!(got.first(), Some(&rel), "the waiting file first");
        assert!(f.done.iter().any(|d| d.1 == first));
    }

    /// A user's folder's creation time is read once it can be (queued
    /// until then) and kept when its pin comes back after a restore, so a
    /// rescan keeps the note.
    #[test]
    fn a_users_folders_creation_time_is_read_and_kept() {
        let mut s = store(&[row(2, 1, "Docs", LocalKind::Folder, 0x10), row(3, 2, "Kept", LocalKind::Folder, 0x10)]);
        journal(&mut s, &[(3, 0x10 | PINNED)]);
        let mut fake = Fake { failing: vec!["Docs/Kept".into()], ..Fake::default() };
        apply(&mut s, &mut fake, &PinSet::new::<&str>(&[]), Duration::from_secs(60), 0).unwrap();
        assert_eq!(s.read(|o| o.pin_user_created(3)).unwrap(), None);
        assert_eq!(s.read(|o| o.pin_changed_page(10)).unwrap(), [3], "unread: still queued");
        fake.failing.clear();
        fake.others.insert("Docs/Kept".into(), (3, 7));
        run(&mut s, &mut fake, &PinSet::new::<&str>(&[]), 0);
        assert_eq!(s.read(|o| o.pin_user_created(3)).unwrap(), Some(7));
        // "Free up space" on Docs; the pass pins Kept again, and the
        // observer sees the pin come back.
        explorer_free(&mut s, &[2, 3]);
        run(&mut s, &mut fake, &PinSet::new::<&str>(&[]), 0);
        assert_eq!(fake.set, [("Docs/Kept".to_string(), Change::Pin)]);
        journal(&mut s, &[(3, 0x10 | PINNED)]);
        assert_eq!(s.read(|o| o.pin_user_created(3)).unwrap(), Some(7), "kept");
        rescan(&mut s, &[row(2, 1, "Docs", LocalKind::Folder, 0x10), row(3, 2, "Kept", LocalKind::Folder, 0x10)]);
        run(&mut s, &mut fake, &PinSet::new::<&str>(&[]), 0);
        assert!(s.read(|o| o.pin_user_has(3)).unwrap(), "the note survives the rescan");
    }

    /// More rule files past the budget than a pass reads, with a file the
    /// user pinned after them and one sorting before them: the scan moves
    /// on, so both download while the go-ahead is outstanding. The
    /// go-ahead then downloads exactly the remembered file, even after a
    /// pass where it couldn't be read (its bytes aren't spent on another).
    #[test]
    fn rule_files_waiting_for_a_go_ahead_dont_freeze_the_scan() {
        let n = FETCH_READ_MAX as u128 + 76;
        // In the index's order (file ID blobs, low byte first): first, and
        // last.
        let (early, late): (u128, u128) = (0x0001_0000, 0x00FF_FFFF);
        let mut rows: Vec<LocalRow> = (10..10 + n)
            .map(|i| LocalRow { size: 2 << 30, ..row(i, 1, &format!("a{i}.psd"), LocalKind::File, RECALL) })
            .collect();
        rows.push(row(early, 1, "early.txt", LocalKind::File, RECALL));
        rows.push(row(late, 1, "late.txt", LocalKind::File, RECALL));
        let mut s = store(&rows);
        let mut fake = Fake::default();
        run(&mut s, &mut fake, &PinSet::new(&["*.psd"]), 0);
        let names: Vec<String> = (10..10 + n).map(|i| format!("a{i}.psd")).collect();
        let ids: Vec<(u128, &str)> = (10..10 + n).zip(names.iter().map(String::as_str)).collect();
        observe(&mut s, &fake, &ids);
        journal(&mut s, &[(early, RECALL | PINNED), (late, RECALL | PINNED)]);
        let pass = |s: &mut Store, fake: &mut Fake| {
            let mut got = Vec::new();
            let f = fetch_pass(s, fake, &mut |_| Ok(()), &mut |rel| {
                got.push(rel.to_string());
                true
            })
            .unwrap();
            let done: Vec<u128> = f.done.iter().map(|d| d.1).collect();
            settle(s, &done, &|_| false).unwrap();
            // Downloaded: the observer sees them here.
            let here: Vec<(u128, u32)> = done.iter().map(|id| (*id, PINNED)).collect();
            journal(s, &here);
            (f, got)
        };
        let mut got = Vec::new();
        let mut asked = None;
        for _ in 0..3 {
            let (f, g) = pass(&mut s, &mut fake);
            asked = asked.or(f.blocked);
            got.extend(g);
        }
        got.sort_unstable();
        assert_eq!(got, ["/early.txt", "/late.txt"], "the user's, none of the rules'");
        let (waiting, size) = asked.unwrap();
        assert_eq!(s.read(|o| o.meta_get(KEY_FETCH_WAITING)).unwrap(), Some(waiting.to_string()));
        // The go-ahead; the waiting file can't be read on the next pass.
        wait_for_go_ahead(&mut s, size).unwrap();
        allow(&mut s, 0).unwrap();
        let rel = s.read(|o| o.local_path_of(waiting)).unwrap().unwrap();
        fake.failing = vec![crate::filetree::clean(&rel)];
        let (_, g) = pass(&mut s, &mut fake);
        assert!(g.is_empty(), "nothing else paid with its bytes: {g:?}");
        assert_eq!(s.read(|o| o.meta_get(KEY_FETCH_WAITING)).unwrap(), Some(waiting.to_string()), "remembered");
        fake.failing.clear();
        let (_, g) = pass(&mut s, &mut fake);
        assert_eq!(g, [rel], "exactly the remembered file");
        assert!(s.read(|o| o.meta_get(KEY_FETCH_WAITING)).unwrap().is_none());
    }

    /// A rescan of the folder (the change journal lost) deletes every Local
    /// row and so the rules' notes; the rules are applied again and the
    /// notes put back, so removing the rule later still unpins its items.
    #[test]
    fn a_rescan_keeps_the_rules_pins_theirs() {
        let mut s = store(&[row(2, 1, "a.psd", LocalKind::File, RECALL)]);
        let mut fake = Fake::default();
        let pins = PinSet::new(&["*.psd"]);
        run(&mut s, &mut fake, &pins, 0);
        observe(&mut s, &fake, &[(2, "a.psd")]);
        // The rescan.
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
            o.local_put(&row(2, 1, "a.psd", LocalKind::File, RECALL | PINNED)).map(|_| ())
        })
        .unwrap();
        fake.set.clear();
        run(&mut s, &mut fake, &pins, 0);
        assert!(s.read(|o| o.pin_owned_has(2)).unwrap(), "the rule's again");
        let p = run(&mut s, &mut fake, &PinSet::new::<&str>(&[]), 0);
        assert_eq!(p.unpinned, 1);
        assert_eq!(fake.set, [("a.psd".to_string(), Change::Unpin)]);
    }

    fn moved(from: &str, to: &str) -> ActivityEntry {
        ActivityEntry { action: "move".into(), detail: format!("-> {to}"), ..entry(from) }
    }

    /// A folder moved out of a rule's place takes its contents along: the
    /// rule's pins in it are undone; moved in, its contents are pinned. A
    /// file the rule pinned moved out is unpinned.
    #[test]
    fn moves_bring_whole_folders_in_line_with_the_rules() {
        let mut s = store(&[
            row(2, 1, "Work", LocalKind::Folder, 0x10),
            row(3, 2, "a.txt", LocalKind::File, RECALL),
            row(4, 1, "Other", LocalKind::Folder, 0x10),
            row(5, 4, "b.txt", LocalKind::File, RECALL),
            row(6, 2, "c.txt", LocalKind::File, RECALL),
        ]);
        let mut fake = Fake::default();
        let pins = PinSet::new(&["/Work/"]);
        run(&mut s, &mut fake, &pins, 0);
        observe(&mut s, &fake, &[(2, "Work"), (3, "Work/a.txt"), (6, "Work/c.txt")]);
        // Work is renamed Old (out of the rule); Other is renamed Work
        // happens later. And c.txt moves out of Work first.
        let rename = |s: &mut Store, id: u128, parent: u128, name: &str| {
            s.write(|o| {
                let mut r = o.local_get(id)?.unwrap();
                r.name = name.into();
                r.parent_id = Some(parent);
                o.local_update(&r)
            })
            .unwrap();
        };
        rename(&mut s, 6, 1, "c.txt");
        s.write(|o| o.activity_add(&[moved("/Work/c.txt", "/c.txt")])).unwrap();
        rename(&mut s, 2, 1, "Old");
        s.write(|o| o.activity_add(&[moved("/Work", "/Old")])).unwrap();
        fake.set.clear();
        run(&mut s, &mut fake, &pins, 0);
        let mut got = fake.set.clone();
        got.sort_by(|a, b| a.0.cmp(&b.0));
        assert_eq!(
            got,
            [
                ("Old".to_string(), Change::Unpin),
                ("Old/a.txt".to_string(), Change::Unpin),
                ("c.txt".to_string(), Change::Unpin),
            ]
        );
        observe(&mut s, &fake, &[(2, "Old"), (3, "Old/a.txt"), (6, "c.txt")]);
        // Other becomes Work: it and what's in it are pinned.
        rename(&mut s, 4, 1, "Work");
        s.write(|o| o.activity_add(&[moved("/Other", "/Work")])).unwrap();
        fake.set.clear();
        run(&mut s, &mut fake, &pins, 0);
        let mut got = fake.set.clone();
        got.sort_by(|a, b| a.0.cmp(&b.0));
        assert_eq!(got, [("Work".to_string(), Change::Pin), ("Work/b.txt".to_string(), Change::Pin)]);
    }

    /// A file the user pinned in a folder before a rule pinned the folder
    /// stays the user's, whatever the activity log says of it later: only
    /// what came after takes the folder's pin as the rule's.
    #[test]
    fn a_pin_from_before_the_folders_stays_the_users() {
        let mut s = store(&[
            row(2, 1, "Work", LocalKind::Folder, 0x10),
            row(3, 2, "mine.txt", LocalKind::File, PINNED),
            row(4, 2, "other.txt", LocalKind::File, RECALL),
        ]);
        let mut fake = Fake::default();
        let pins = PinSet::new(&["/Work/"]);
        run(&mut s, &mut fake, &pins, 0);
        observe(&mut s, &fake, &[(2, "Work"), (4, "Work/other.txt")]);
        // mine.txt is edited and uploaded: the log names it.
        s.write(|o| o.activity_add(&[entry("/Work/mine.txt")])).unwrap();
        run(&mut s, &mut fake, &pins, 0);
        assert!(!s.read(|o| o.pin_owned_has(3)).unwrap(), "the user's");
        fake.set.clear();
        run(&mut s, &mut fake, &PinSet::new::<&str>(&[]), 0);
        assert!(!fake.set.iter().any(|(r, _)| r == "Work/mine.txt"), "{:?}", fake.set);
    }

    /// An unpin that fails now waits as an unpin and is done later; the
    /// rules count as applied meanwhile.
    #[test]
    fn an_unpin_that_fails_is_tried_again() {
        let mut s = store(&[row(2, 1, "a.psd", LocalKind::File, RECALL)]);
        let mut fake = Fake::default();
        run(&mut s, &mut fake, &PinSet::new(&["*.psd"]), 0);
        observe(&mut s, &fake, &[(2, "a.psd")]);
        fake.failing = vec!["a.psd".into()];
        let none = PinSet::new::<&str>(&[]);
        let p = run(&mut s, &mut fake, &none, 0);
        assert_eq!((p.unpinned, p.waiting), (0, 1));
        assert_eq!(s.read(|o| o.pin_retry_due(i64::MAX, 10)).unwrap(), [(2, PinOp::Unpin, 1)]);
        assert!(s.read(|o| o.pin_owned_has(2)).unwrap(), "still the rule's");
        fake.failing.clear();
        fake.set.clear();
        let p = run(&mut s, &mut fake, &none, back_off_ms(0));
        assert_eq!(p.unpinned, 1);
        assert_eq!(fake.set, [("a.psd".to_string(), Change::Unpin)]);
        assert!(!s.read(|o| o.pin_owned_has(2)).unwrap());
    }

    /// A queued "Free up space" leaves the queue only once pinned again or
    /// handed to the retries: with those full and the pin failing, it stays.
    #[test]
    fn a_lost_pin_stays_queued_until_it_is_handled() {
        let max = oneup_store::pins::PIN_RETRY_MAX as u128;
        let mut rows: Vec<LocalRow> =
            (0..max).map(|i| row(100 + i, 1, &format!("x{i}.bin"), LocalKind::File, 0)).collect();
        rows.push(row(2, 1, "a.psd", LocalKind::File, RECALL));
        let mut s = store(&rows);
        let mut fake = Fake::default();
        let pins = PinSet::new(&["*.psd"]);
        run(&mut s, &mut fake, &pins, 0);
        observe(&mut s, &fake, &[(2, "a.psd")]);
        s.write(|o| {
            for i in 0..max {
                assert!(o.pin_retry_put(100 + i, PinOp::Pin, 1, i64::MAX)?);
            }
            let mut r = o.local_get(2)?.unwrap();
            r.attributes &= !PINNED;
            o.local_update(&r)
        })
        .unwrap();
        fake.failing = vec!["a.psd".into()];
        apply(&mut s, &mut fake, &pins, Duration::from_secs(60), 0).unwrap();
        assert_eq!(s.read(|o| o.pin_changed_page(10)).unwrap(), [2], "still queued");
        fake.failing.clear();
        let p = apply(&mut s, &mut fake, &pins, Duration::from_secs(60), 0).unwrap();
        assert_eq!(p.repinned, 1);
        assert!(s.read(|o| o.pin_changed_page(10)).unwrap().is_empty());
    }

    /// A rule unpins only the item it pinned: same file ID and creation
    /// time, checked on the handle that unpins. A file put in its place (a
    /// new file, or one reusing the ID) is the user's and keeps its pin.
    #[test]
    fn a_rule_never_unpins_an_item_that_replaced_its_own() {
        for other in [(2u128, 9i64), (77, 0)] {
            let mut s = store(&[row(2, 1, "a.psd", LocalKind::File, RECALL)]);
            let mut fake = Fake::default();
            run(&mut s, &mut fake, &PinSet::new(&["*.psd"]), 0);
            observe(&mut s, &fake, &[(2, "a.psd")]);
            // Replaced by a file the user pinned (same ID reused, or another).
            fake.others.insert("a.psd".into(), other);
            fake.set.clear();
            let p = run(&mut s, &mut fake, &PinSet::new::<&str>(&[]), 0);
            assert_eq!((p.unpinned, fake.set.len()), (0, 0), "{other:?}: {:?}", fake.set);
            assert!(!s.read(|o| o.pin_owned_has(2)).unwrap(), "not provably the rule's: dropped");
        }
    }

    /// The rule's note is written before the pin is set: a crash in between
    /// (the pin set, the process gone) leaves the item the rule's, so its
    /// download counts against the rules' budget and removing the rule
    /// unpins it.
    #[test]
    fn the_rules_note_is_written_before_the_pin() {
        struct Crash;
        impl Pinner for Crash {
            fn identify(&mut self, _rel: &str, expected: u128) -> std::io::Result<(u128, i64)> {
                Ok((expected, 0))
            }
            fn set(&mut self, _rel: &str, _expect: (u128, Option<i64>), _change: Change) -> std::io::Result<()> {
                panic!("the process ends here");
            }
        }
        let mut s = store(&[row(2, 1, "a.psd", LocalKind::File, RECALL)]);
        let pins = PinSet::new(&["*.psd"]);
        let crashed = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
            apply(&mut s, &mut Crash, &pins, Duration::from_secs(60), 0)
        }));
        assert!(crashed.is_err());
        assert!(s.read(|o| o.pin_owned_has(2)).unwrap(), "the intent is there");
    }

    /// After a rescan, the rule's notes are put back by file ID; an ID a
    /// replacement reused proves nothing, so that item keeps the user's pin.
    #[test]
    fn after_a_rescan_a_reused_id_is_the_users() {
        let mut s = store(&[row(2, 1, "a.psd", LocalKind::File, RECALL)]);
        let mut fake = Fake::default();
        let pins = PinSet::new(&["*.psd"]);
        run(&mut s, &mut fake, &pins, 0);
        observe(&mut s, &fake, &[(2, "a.psd")]);
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
            // Another file, pinned by the user, with the old ID.
            o.local_put(&row(2, 1, "b.psd", LocalKind::File, PINNED)).map(|_| ())
        })
        .unwrap();
        fake.others.insert("b.psd".into(), (2, 5));
        run(&mut s, &mut fake, &pins, 0);
        fake.set.clear();
        run(&mut s, &mut fake, &PinSet::new::<&str>(&[]), 0);
        assert!(fake.set.is_empty(), "{:?}", fake.set);
    }

    /// A file freed by name stays freed across a rescan: its release is
    /// put back with the rule's other records.
    #[test]
    fn a_release_survives_a_rescan() {
        let mut s = store(&[row(2, 1, "Art", LocalKind::Folder, 0x10), row(3, 2, "a.psd", LocalKind::File, RECALL)]);
        let mut fake = Fake::default();
        let pins = PinSet::new(&["*.psd"]);
        run(&mut s, &mut fake, &pins, 0);
        observe(&mut s, &fake, &[(3, "Art/a.psd")]);
        explorer_free(&mut s, &[3]);
        run(&mut s, &mut fake, &pins, 0);
        assert!(s.read(|o| o.pin_released_has(3)).unwrap());
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
            o.local_put(&row(2, 1, "Art", LocalKind::Folder, 0x10))?;
            o.local_put(&row(3, 2, "a.psd", LocalKind::File, RECALL | UNPINNED)).map(|_| ())
        })
        .unwrap();
        fake.set.clear();
        run(&mut s, &mut fake, &pins, 0);
        assert!(fake.set.is_empty(), "not pinned again: {:?}", fake.set);
        assert!(s.read(|o| o.pin_released_has(3)).unwrap());
    }

    /// "Free up space" on a folder a rule keeps (on the folder itself, its
    /// own folder above not marked) doesn't release it: only a file's own
    /// request does. It's pinned again.
    #[test]
    fn freeing_a_rule_folder_by_name_doesnt_release_it() {
        let mut s = store(&[row(2, 1, "Work", LocalKind::Folder, 0x10)]);
        let mut fake = Fake::default();
        let pins = PinSet::new(&["/Work/"]);
        run(&mut s, &mut fake, &pins, 0);
        observe(&mut s, &fake, &[(2, "Work")]);
        explorer_free(&mut s, &[2]);
        fake.set.clear();
        let p = run(&mut s, &mut fake, &pins, 0);
        assert_eq!((p.released, p.repinned), (0, 1));
        assert!(!s.read(|o| o.pin_released_has(2)).unwrap());
    }

    /// `oneup pin` on a folder makes it and everything in it the user's: a
    /// rule removed later leaves their pins alone.
    #[test]
    fn keeping_it_oneself_takes_it_from_the_rule() {
        let mut s = store(&[row(2, 1, "Work", LocalKind::Folder, 0x10), row(3, 2, "a.psd", LocalKind::File, RECALL)]);
        let mut fake = Fake::default();
        let pins = PinSet::new(&["/Work/"]);
        run(&mut s, &mut fake, &pins, 0);
        observe(&mut s, &fake, &[(2, "Work"), (3, "Work/a.psd")]);
        assert_eq!(claim_for_user(&mut s, "Work").unwrap(), 2);
        fake.set.clear();
        run(&mut s, &mut fake, &PinSet::new::<&str>(&[]), 0);
        assert!(fake.set.is_empty(), "{:?}", fake.set);
    }

    /// `oneup pin` records the person's claim before the pin, `oneup free`
    /// the release: a crash right after leaves the record, so an agent pass
    /// neither undoes the user's pin nor pins the freed file again.
    #[test]
    fn hand_pins_and_frees_are_recorded_before_acting() {
        let mut s = store(&[row(2, 1, "a.psd", LocalKind::File, RECALL), row(3, 1, "b.psd", LocalKind::File, RECALL)]);
        let mut fake = Fake::default();
        let pins = PinSet::new(&["*.psd"]);
        run(&mut s, &mut fake, &pins, 0);
        observe(&mut s, &fake, &[(2, "a.psd"), (3, "b.psd")]);
        // `oneup pin a.psd`, crashing before the pin is set.
        let crash = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
            keep_by_hand(&mut s, "a.psd", &mut || panic!("the process ends here"))
        }));
        assert!(crash.is_err());
        assert!(s.read(|o| Ok(o.pin_user_has(2)? && !o.pin_owned_has(2)?)).unwrap(), "the claim is there");
        // `oneup free b.psd`, crashing before the pin is changed.
        let crash = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
            free_plan(&mut s, &pins, "b.psd", &mut |_| panic!("the process ends here"))
        }));
        assert!(crash.is_err());
        assert!(s.read(|o| o.pin_released_has(3)).unwrap(), "the release is there");
        // The agent: the rule removed later leaves a.psd pinned; b.psd, once
        // freed, isn't pinned again.
        fake.set.clear();
        run(&mut s, &mut fake, &PinSet::new::<&str>(&[]), 0);
        assert!(fake.set.is_empty(), "{:?}", fake.set);
        explorer_free(&mut s, &[3]);
        run(&mut s, &mut fake, &pins, 0);
        assert!(fake.set.is_empty(), "{:?}", fake.set);
    }

    /// Claiming a folder of many files takes a durable transaction per page
    /// (and per folder gone into), not one per item.
    #[test]
    fn claiming_a_big_folder_commits_a_page_at_a_time() {
        let n = PAGE as u128 * 4 + 3;
        let mut rows = vec![row(2, 1, "Big", LocalKind::Folder, 0x10)];
        rows.extend((0..n).map(|i| row(100 + i, 2, &format!("f{i}"), LocalKind::File, 0)));
        let mut s = store(&rows);
        let before = s.read(|o| o.durable_commits()).unwrap();
        assert_eq!(claim_for_user(&mut s, "Big").unwrap(), n as usize + 1);
        let commits = s.read(|o| o.durable_commits()).unwrap() - before;
        assert!(commits <= 6, "{commits} commits for {n} files");
        assert!(s.read(|o| o.pin_user_has(100 + n - 1)).unwrap());
    }

    /// The rules' budget counts an item only while its note still names it:
    /// a file that reuses the ID after a rescan is the user's, and its note
    /// goes.
    #[test]
    fn the_budget_counts_only_what_the_rule_provably_pinned() {
        let mut s = store(&[row(2, 1, "a.psd", LocalKind::File, RECALL)]);
        let mut fake = Fake::default();
        run(&mut s, &mut fake, &PinSet::new(&["*.psd"]), 0);
        assert_eq!(owned_verified(&mut s, &mut fake, 2, "a.psd").unwrap(), Some(true));
        // Unreadable now: unproven, left alone this pass.
        fake.failing = vec!["a.psd".into()];
        assert_eq!(owned_verified(&mut s, &mut fake, 2, "a.psd").unwrap(), None);
        fake.failing.clear();
        fake.others.insert("a.psd".into(), (2, 9));
        assert_eq!(owned_verified(&mut s, &mut fake, 2, "a.psd").unwrap(), Some(false));
        assert!(!s.read(|o| o.pin_owned_has(2)).unwrap());
    }

    /// Bytes of a download that was tried count, whether it finished or not:
    /// failures can't download past the approval again and again.
    #[test]
    fn a_tried_download_keeps_its_reservation() {
        let mut s = store(&[]);
        approve(&mut s, &PinSet::new(&["*.psd"]).fingerprint(), 100).unwrap();
        take_approval(&mut s, &PinSet::new(&["*.psd"]).fingerprint()).unwrap();
        reserve_budget(&mut s, &[(1, 60, true), (2, 30, true)]).unwrap();
        // 1 was tried (and failed partway); 2 never started.
        settle(&mut s, &[1], &|_| false).unwrap();
        assert_eq!(budget(&s).unwrap(), 40);
    }

    /// `oneup pin` with the rule still there: claimed before the pin is set
    /// on disk, the item isn't taken by the rule (no note, no retry), so
    /// removing the rule later leaves the user's pin.
    #[test]
    fn a_claim_before_its_pin_is_never_taken_by_a_rule() {
        let mut s = store(&[row(2, 1, "Work", LocalKind::Folder, 0x10), row(3, 2, "a.psd", LocalKind::File, RECALL)]);
        let mut fake = Fake::default();
        let pins = PinSet::new(&["*.psd", "/Work/"]);
        claim_for_user(&mut s, "Work").unwrap();
        // The agent passes with the rule there, before `oneup pin` sets the pin.
        run(&mut s, &mut fake, &pins, 0);
        assert!(fake.set.is_empty(), "{:?}", fake.set);
        assert!(!s.read(|o| o.pin_owned_has(3)).unwrap() && s.read(|o| o.pin_retry_count()).unwrap() == 0);
        // The pin is set (by hand); the rule goes: nothing undone.
        s.write(|o| {
            for id in [2, 3] {
                let mut r = o.local_get(id)?.unwrap();
                r.attributes |= PINNED;
                o.local_update(&r)?;
            }
            Ok(())
        })
        .unwrap();
        run(&mut s, &mut fake, &PinSet::new::<&str>(&[]), 0);
        assert!(fake.set.is_empty(), "{:?}", fake.set);
    }

    /// "Free up space" on the synced folder itself is a folder-level request:
    /// the rule's files at the top stay pinned.
    #[test]
    fn freeing_the_synced_folder_leaves_the_rules_files_pinned() {
        let mut s = store(&[row(3, 1, "a.psd", LocalKind::File, RECALL)]);
        let mut fake = Fake::default();
        let pins = PinSet::new(&["*.psd"]);
        run(&mut s, &mut fake, &pins, 0);
        observe(&mut s, &fake, &[(3, "a.psd")]);
        explorer_free(&mut s, &[1, 3]);
        fake.set.clear();
        let p = run(&mut s, &mut fake, &pins, 0);
        assert_eq!((p.released, p.repinned), (0, 1));
    }

    /// ...and a folder the user pinned at the top keeps its pin and stays
    /// the user's.
    #[test]
    fn freeing_the_synced_folder_leaves_a_top_folder_the_user_pinned() {
        let mut s = store(&[row(2, 1, "Kept", LocalKind::Folder, 0x10)]);
        journal(&mut s, &[(2, 0x10 | PINNED)]);
        assert!(s.read(|o| o.pin_user_has(2)).unwrap(), "noted as the user's");
        explorer_free(&mut s, &[1, 2]);
        let mut fake = Fake::default();
        let p = run(&mut s, &mut fake, &PinSet::new::<&str>(&[]), 0);
        assert_eq!(p.repinned, 1);
        assert_eq!(fake.set, [("Kept".to_string(), Change::Pin)]);
        assert!(s.read(|o| o.pin_user_has(2)).unwrap(), "still the user's");
    }

    /// A started download is never refunded, whatever happened to it.
    #[test]
    fn a_started_download_is_never_refunded() {
        let mut s = store(&[]);
        approve(&mut s, &PinSet::new(&["*.psd"]).fingerprint(), 100).unwrap();
        take_approval(&mut s, &PinSet::new(&["*.psd"]).fingerprint()).unwrap();
        reserve_budget(&mut s, &[(1, 60, true), (2, 30, true)]).unwrap();
        mark_started(&mut s, 1).unwrap();
        // A crash; on the next start nothing says 1 is here.
        settle(&mut s, &[], &|_| false).unwrap();
        assert_eq!(budget(&s).unwrap(), 40, "1 stays charged, 2 comes back");
    }

    /// A folder the user pinned keeps what's in it from being freed, and a
    /// "Free up space" on a folder above doesn't undo its pin; on the folder
    /// itself, it does.
    #[test]
    fn a_folder_the_user_pinned_wins_over_a_folder_above() {
        let mut s = store(&[
            row(2, 1, "Docs", LocalKind::Folder, 0x10),
            row(3, 2, "Kept", LocalKind::Folder, 0x10),
            row(4, 3, "a.txt", LocalKind::File, 0),
        ]);
        // The user pins Kept in File Explorer (seen by the observer).
        s.write(|o| o.local_update(&row(3, 2, "Kept", LocalKind::Folder, 0x10 | PINNED))).unwrap();
        assert!(s.read(|o| o.pin_user_has(3)).unwrap(), "noted as the user's");
        let a = s.read(|o| o.local_get(4)).unwrap().unwrap();
        assert!(kept_by_folder_above(&s, &a).unwrap());
        // "Free up space" on Docs: Explorer marks Docs, Kept and a.txt.
        explorer_free(&mut s, &[2, 3, 4]);
        let mut fake = Fake::default();
        let p = run(&mut s, &mut fake, &PinSet::new::<&str>(&[]), 0);
        assert_eq!(p.repinned, 1);
        assert_eq!(fake.set, [("Docs/Kept".to_string(), Change::Pin)]);
        assert!(kept_by_folder_above(&s, &a).unwrap(), "still kept: the user's folder");
        // "Free up space" on Kept itself (Docs not marked): the user's choice.
        s.write(|o| {
            o.local_update(&row(2, 1, "Docs", LocalKind::Folder, 0x10))?;
            o.local_update(&row(3, 2, "Kept", LocalKind::Folder, 0x10 | PINNED))?;
            o.local_update(&row(3, 2, "Kept", LocalKind::Folder, 0x10 | UNPINNED))
        })
        .unwrap();
        fake.set.clear();
        // Left queued once (Docs's record may come later), then dropped.
        run(&mut s, &mut fake, &PinSet::new::<&str>(&[]), 0);
        assert!(s.read(|o| o.pin_user_has(3)).unwrap(), "one more pass");
        run(&mut s, &mut fake, &PinSet::new::<&str>(&[]), 0);
        assert!(fake.set.is_empty() && !s.read(|o| o.pin_user_has(3)).unwrap());
        assert!(!kept_by_folder_above(&s, &a).unwrap());
    }

    /// A rescan of the same folder: every Local row again, as `rows`.
    fn rescan(s: &mut Store, rows: &[LocalRow]) {
        s.write(|o| {
            let root = o.local_get(1)?.unwrap();
            let info = o.local_root()?.unwrap();
            o.local_begin(&info, &root)?;
            for r in rows {
                o.local_put(r)?;
            }
            Ok(())
        })
        .unwrap();
    }

    /// A folder pinned when the folder was scanned is the user's: a "Free
    /// up space" on the folder above doesn't undo it.
    #[test]
    fn a_folder_found_pinned_by_the_scan_is_the_users() {
        let mut s =
            store(&[row(2, 1, "Docs", LocalKind::Folder, 0x10), row(3, 2, "Kept", LocalKind::Folder, 0x10 | PINNED)]);
        let mut fake = Fake::default();
        run(&mut s, &mut fake, &PinSet::new::<&str>(&[]), 0);
        assert!(s.read(|o| o.pin_user_has(3)).unwrap(), "noted as the user's");
        explorer_free(&mut s, &[2, 3]);
        let p = run(&mut s, &mut fake, &PinSet::new::<&str>(&[]), 0);
        assert_eq!(p.repinned, 1);
        assert_eq!(fake.set, [("Docs/Kept".to_string(), Change::Pin)]);
    }

    /// A user's folder whose pin a "Free up space" above took, not restored
    /// yet when the folder is scanned again, is still the user's and gets
    /// its pin back.
    #[test]
    fn a_rescan_keeps_a_users_folder_waiting_to_be_restored() {
        for reused in [false, true] {
            let mut s =
                store(&[row(2, 1, "Docs", LocalKind::Folder, 0x10), row(3, 2, "Kept", LocalKind::Folder, 0x10)]);
            journal(&mut s, &[(3, 0x10 | PINNED)]);
            assert!(s.read(|o| o.pin_user_has(3)).unwrap());
            // A pass reads its creation time on a handle.
            let mut fake = Fake::default();
            run(&mut s, &mut fake, &PinSet::new::<&str>(&[]), 0);
            assert_eq!(s.read(|o| o.pin_user_created(3)).unwrap(), Some(0));
            explorer_free(&mut s, &[2, 3]);
            rescan(
                &mut s,
                &[
                    row(2, 1, "Docs", LocalKind::Folder, 0x10 | UNPINNED),
                    row(3, 2, "Kept", LocalKind::Folder, 0x10 | UNPINNED),
                ],
            );
            if reused {
                // Another folder took the ID meanwhile: not the user's pin.
                fake.others.insert("Docs/Kept".into(), (3, 99));
            }
            let p = run(&mut s, &mut fake, &PinSet::new::<&str>(&[]), 0);
            if reused {
                assert_eq!(p.repinned, 0);
                assert!(fake.set.is_empty(), "{:?}", fake.set);
                assert!(!s.read(|o| o.pin_user_has(3)).unwrap());
            } else {
                assert_eq!(p.repinned, 1);
                assert_eq!(fake.set, [("Docs/Kept".to_string(), Change::Pin)]);
                assert!(s.read(|o| o.pin_user_has(3)).unwrap(), "still the user's");
            }
        }
    }

    /// "Free up space" on a folder above whose own record comes in a later
    /// batch than the user's folder's: the folder stays queued and is
    /// pinned again once the folder above is seen marked.
    #[test]
    fn a_users_folder_waits_for_the_folder_above_to_be_seen() {
        let mut s = store(&[row(2, 1, "Docs", LocalKind::Folder, 0x10), row(3, 2, "Kept", LocalKind::Folder, 0x10)]);
        journal(&mut s, &[(3, 0x10 | PINNED)]);
        let mut fake = Fake::default();
        run(&mut s, &mut fake, &PinSet::new::<&str>(&[]), 0);
        // Only Kept's record in this batch.
        journal(&mut s, &[(3, 0x10 | UNPINNED)]);
        let p = apply(&mut s, &mut fake, &PinSet::new::<&str>(&[]), Duration::from_secs(60), 0).unwrap();
        assert_eq!(p.repinned, 0);
        assert!(s.read(|o| o.pin_user_has(3)).unwrap(), "not dropped yet");
        assert_eq!(s.read(|o| o.pin_changed_page(10)).unwrap(), [3]);
        // Docs's record, in the next batch.
        journal(&mut s, &[(2, 0x10 | UNPINNED)]);
        let p = apply(&mut s, &mut fake, &PinSet::new::<&str>(&[]), Duration::from_secs(60), 0).unwrap();
        assert_eq!(p.repinned, 1);
        assert_eq!(fake.set, [("Docs/Kept".to_string(), Change::Pin)]);
        assert!(s.read(|o| o.pin_user_has(3)).unwrap());
    }

    /// A user's folder whose pin can't be restored now stays queued and
    /// gets it on a later pass.
    #[test]
    fn a_users_folder_that_cant_be_pinned_now_stays_queued() {
        let mut s = store(&[row(2, 1, "Docs", LocalKind::Folder, 0x10), row(3, 2, "Kept", LocalKind::Folder, 0x10)]);
        journal(&mut s, &[(3, 0x10 | PINNED)]);
        explorer_free(&mut s, &[2, 3]);
        let mut fake = Fake { failing: vec!["Docs/Kept".into()], ..Fake::default() };
        let p = apply(&mut s, &mut fake, &PinSet::new::<&str>(&[]), Duration::from_secs(60), 0).unwrap();
        assert_eq!((p.repinned, p.waiting), (0, 1));
        assert_eq!(s.read(|o| o.pin_changed_page(10)).unwrap(), [3], "still queued");
        assert!(s.read(|o| o.pin_user_has(3)).unwrap());
        fake.failing.clear();
        let p = run(&mut s, &mut fake, &PinSet::new::<&str>(&[]), 0);
        assert_eq!(p.repinned, 1);
        assert_eq!(fake.set, [("Docs/Kept".to_string(), Change::Pin)]);
        assert!(s.read(|o| o.pin_changed_page(10)).unwrap().is_empty());
    }

    /// `oneup pin` (another process) claims the file while a pass is about
    /// to pin it for a rule: no note is written, and the rule doesn't pin.
    #[test]
    fn a_claim_while_a_rule_pins_wins() {
        struct Claims {
            fake: Fake,
            db: PathBuf,
        }
        impl Pinner for Claims {
            fn identify(&mut self, rel: &str, expected: u128) -> std::io::Result<(u128, i64)> {
                let mut other = Store::open(&self.db, 1024).unwrap();
                other.write_durable(|o| o.pin_claim_for_user(expected)).unwrap();
                self.fake.identify(rel, expected)
            }
            fn set(&mut self, rel: &str, expect: (u128, Option<i64>), change: Change) -> std::io::Result<()> {
                self.fake.set(rel, expect, change)
            }
        }
        let dir = std::env::temp_dir().join(format!("oneup-pins-claim-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        let db = dir.join("state.db");
        {
            let mut s = Store::open(&db, 1024).unwrap();
            let mem = store(&[row(2, 1, "a.psd", LocalKind::File, RECALL)]);
            s.write(|o| {
                o.local_begin(
                    &mem.read(|m| m.local_root()).unwrap().unwrap(),
                    &mem.read(|m| m.local_get(1)).unwrap().unwrap(),
                )?;
                o.local_put(&row(2, 1, "a.psd", LocalKind::File, RECALL))
            })
            .unwrap();
            let mut p = Claims { fake: Fake::default(), db: db.clone() };
            let pass = apply(&mut s, &mut p, &PinSet::new(&["*.psd"]), Duration::from_secs(60), 0).unwrap();
            assert_eq!(pass.pinned, 0);
            assert!(p.fake.set.is_empty(), "{:?}", p.fake.set);
            assert!(!s.read(|o| o.pin_owned_has(2)).unwrap() && s.read(|o| o.pin_user_has(2)).unwrap());
        }
        let _ = std::fs::remove_dir_all(&dir);
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
        // Asked whenever anything would download, and always when the
        // preview is incomplete, however small what it found.
        assert!(must_ask(1, true) && must_ask(0, false) && !must_ask(0, true));
        let (files, _, all) = preview(&s, &["*.psd".into()], Duration::ZERO).unwrap();
        assert!(!all && must_ask(files, all), "cut short: asked");
    }

    /// Downloads stop at a pause, before the disk gets too full, and once a
    /// pass has used its share of bytes or time.
    #[test]
    fn downloads_keep_to_pauses_free_space_and_a_share_per_pass() {
        let now = Instant::now();
        let big = Some(u64::MAX);
        let mut l = FetchLimit::new(100, now);
        assert_eq!(l.may(10, true, big, now), Err(Stop::Paused));
        assert_eq!(l.may(10, false, Some(105), now), Err(Stop::LowDisk), "would leave less than the reserve");
        assert_eq!(l.may(10, false, None, now), Err(Stop::SpaceUnknown), "free space unknown: nothing blind");
        assert_eq!(l.may(FETCH_BYTES + 5, false, big, now), Ok(()), "the first file, however large");
        assert_eq!(l.may(1, false, big, now), Err(Stop::Share), "the share is used");
        let mut l = FetchLimit::new(0, now);
        assert_eq!(l.may(1, false, big, now), Ok(()));
        assert_eq!(l.may(1, false, big, now + FETCH_TIME), Err(Stop::Share), "the time is up");
        assert_eq!(reserve(Some(100 << 30), 10), 10 << 30);
        assert_eq!(reserve(None, 10), 1 << 30);
    }
}
