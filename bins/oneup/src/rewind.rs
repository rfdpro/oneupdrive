//! Rewinding a folder to an earlier moment: `oneup rewind`, the
//! settings window's **Recovery** page, and the content guard's stop
//! ("rewind to just before this batch").
//!
//! A rewind is planned from the Synced tree's history
//! (`oneup_sync::rewind`, `oneup_store::history`) and carried out by the
//! agent's rounds as ordinary planned operations, through the safety gate
//! and the effects layer. Starting one saves it in `meta` ([`KEY`]); each
//! round then plans its steps again. Until the user approves, they're a
//! safety stop of their own, whatever the limits, with its list and code
//! like any other stop, and nothing at all runs. The approval binds the
//! steps the stop showed, item by item ([`oneup_sync::rewind::Job`], saved
//! with the rewind): a later round runs an item's steps only while it's
//! as the preview showed it, or as the rewind's own last step left it, so
//! what changed after the preview is left alone. It ends once nothing it
//! approved is left to do, or after [`STALLS`] rounds in a row in which its
//! steps all failed or were skipped.

use std::collections::BTreeMap;

use anyhow::{Context as _, Result, bail};
use oneup_rules::RuleSet;
use oneup_store::{ActivityEntry, ActivityOutcome, Ops, Store};
use oneup_sync::rewind::{self as core, Change, Job as Approval, Key, Kind, Mark, Shown, Skip, Source, Spot};
use oneup_sync::{LocalId, Op, RemoteId, Sid};
use serde_json::{Value, json};

/// In `meta`: the rewind under way (JSON).
pub const KEY: &str = "rewind.job";

/// A rewind changes at most this many items: its approval is kept item by
/// item, with the rewind.
pub const MAX: usize = 100_000;

/// Rounds in a row in which every step failed or was skipped, after which
/// a rewind ends with what it could do.
pub const STALLS: u32 = 3;

/// A rewind under way.
#[derive(Debug, Clone, PartialEq)]
pub struct Job {
    /// The folder as the user named it (`/docs`; `""` for everything).
    pub folder: String,
    /// Its Synced record at the moment chosen.
    pub sid: Sid,
    /// The moment (Unix ms).
    pub at_ms: i64,
    pub started_ms: i64,
    /// What the user approved; `None` until then.
    pub approval: Option<Approval>,
    pub stalls: u32,
    /// Steps that ran, steps left alone (changed after the preview), steps
    /// that failed.
    pub ran: u64,
    pub left_alone: u64,
    pub failed: u64,
}

/// What a rewind would change, for the preview.
#[derive(Debug, Clone)]
pub struct Preview {
    pub folder: String,
    pub at_ms: i64,
    pub changes: Vec<Change>,
    pub skipped: Vec<(String, Skip)>,
    /// The code its safety stop will have, if nothing changes meanwhile.
    pub code: u64,
}

impl Preview {
    /// How many changes of each kind, and their total size.
    pub fn groups(&self) -> BTreeMap<Kind, (usize, i64)> {
        let mut g: BTreeMap<Kind, (usize, i64)> = BTreeMap::new();
        for c in &self.changes {
            let e = g.entry(c.kind).or_default();
            e.0 += 1;
            e.1 = e.1.saturating_add(c.size.max(0));
        }
        g
    }
}

fn hex(b: &[u8]) -> String {
    b.iter().map(|x| format!("{x:02x}")).collect()
}

fn unhex20(s: &str) -> Option<[u8; 20]> {
    if s.len() != 40 {
        return None;
    }
    let mut out = [0u8; 20];
    for (i, o) in out.iter_mut().enumerate() {
        *o = u8::from_str_radix(s.get(2 * i..2 * i + 2)?, 16).ok()?;
    }
    Some(out)
}

fn key_json(k: &Key) -> Value {
    match k {
        Key::Cloud(id) => json!({ "cloud": id.0 }),
        Key::Local(id) => json!({ "local": format!("{:x}", id.0) }),
        Key::Record(sid) => json!({ "record": sid }),
    }
}

fn key_of(v: &Value) -> Option<Key> {
    if let Some(id) = v.get("cloud").and_then(Value::as_str) {
        return Some(Key::Cloud(RemoteId(id.to_string())));
    }
    if let Some(id) = v.get("local").and_then(Value::as_str) {
        return Some(Key::Local(LocalId(u128::from_str_radix(id, 16).ok()?)));
    }
    v.get("record").and_then(Value::as_u64).map(Key::Record)
}

fn mark_json(m: &Mark) -> Value {
    match m {
        Mark::Absent => json!("absent"),
        Mark::Record => json!("record"),
        Mark::Local(stamp) => json!({ "local": stamp.map(|(s, t)| [s, t]) }),
        Mark::Cloud(s) => json!({ "cloud": {
            "parent": s.parent.0, "name": s.name, "size": s.size, "hash": s.hash.map(|h| hex(&h)),
        } }),
    }
}

fn mark_of(v: &Value) -> Option<Mark> {
    match v.as_str() {
        Some("absent") => return Some(Mark::Absent),
        Some("record") => return Some(Mark::Record),
        Some(_) => return None,
        None => {}
    }
    if let Some(l) = v.get("local") {
        let stamp = match l.as_array() {
            Some(a) => Some((a.first()?.as_i64()?, a.get(1)?.as_i64()?)),
            None if l.is_null() => None,
            None => return None,
        };
        return Some(Mark::Local(stamp));
    }
    let c = v.get("cloud")?;
    let hash = match c.get("hash") {
        Some(Value::String(h)) => Some(unhex20(h)?),
        _ => None,
    };
    Some(Mark::Cloud(Spot {
        parent: RemoteId(c.get("parent")?.as_str()?.to_string()),
        name: c.get("name")?.as_str()?.to_string(),
        size: c.get("size")?.as_i64()?,
        hash,
    }))
}

fn marks_json(m: &BTreeMap<Key, Mark>) -> Value {
    Value::Array(m.iter().map(|(k, v)| json!({ "key": key_json(k), "mark": mark_json(v) })).collect())
}

fn marks_of(v: Option<&Value>) -> Option<BTreeMap<Key, Mark>> {
    let mut out = BTreeMap::new();
    for e in v?.as_array()? {
        out.insert(key_of(e.get("key")?)?, mark_of(e.get("mark")?)?);
    }
    Some(out)
}

impl Job {
    pub fn encode(&self) -> String {
        let approval = self.approval.as_ref().map(|a| {
            json!({
                "approved": marks_json(&a.approved),
                "left": marks_json(&a.left),
                "rewound": a.rewound.iter().collect::<Vec<_>>(),
            })
        });
        json!({
            "folder": self.folder, "sid": self.sid, "at_ms": self.at_ms, "started_ms": self.started_ms,
            "approval": approval, "stalls": self.stalls,
            "ran": self.ran, "left_alone": self.left_alone, "failed": self.failed,
        })
        .to_string()
    }

    /// `None`: not a rewind this version wrote; it's dropped rather than
    /// guessed at.
    pub fn decode(s: &str) -> Option<Job> {
        let v: Value = serde_json::from_str(s).ok()?;
        let approval = match v.get("approval") {
            None | Some(Value::Null) => None,
            Some(a) => Some(Approval {
                approved: marks_of(a.get("approved"))?,
                left: marks_of(a.get("left"))?,
                rewound: a.get("rewound")?.as_array()?.iter().map(Value::as_u64).collect::<Option<_>>()?,
            }),
        };
        Some(Job {
            folder: v.get("folder")?.as_str()?.to_string(),
            sid: v.get("sid")?.as_u64()?,
            at_ms: v.get("at_ms")?.as_i64()?,
            started_ms: v.get("started_ms")?.as_i64()?,
            approval,
            stalls: u32::try_from(v.get("stalls")?.as_u64()?).ok()?,
            ran: v.get("ran")?.as_u64()?,
            left_alone: v.get("left_alone")?.as_u64()?,
            failed: v.get("failed")?.as_u64()?,
        })
    }
}

/// In `meta`: how many days, and how many changes at most, of the
/// history are kept (`oneup rewind --set-history-days/--set-history-changes`).
const KEY_DAYS: &str = "history.keep_days";
const KEY_ROWS: &str = "history.keep_changes";

/// How much history is kept; `None`: none (rewinding is off). Defaults:
/// 30 days (as long as OneDrive Personal keeps what's deleted) and at most
/// 1,000,000 changes (about 165 MB).
pub fn history_keep(o: &Ops<'_>) -> oneup_store::Result<Option<oneup_store::history::Keep>> {
    let d = oneup_store::history::Keep::default();
    let days = o.meta_get(KEY_DAYS)?.and_then(|v| v.parse().ok()).unwrap_or(d.days);
    let rows = o.meta_get(KEY_ROWS)?.and_then(|v| v.parse().ok()).unwrap_or(d.rows);
    Ok((days > 0 && rows > 0).then_some(oneup_store::history::Keep { days, rows }))
}

/// Changes how much history is kept (`None`: as it is). 0 turns keeping
/// it off, and drops what's kept.
pub fn set_history(store: &mut Store, days: Option<u32>, changes: Option<u64>) -> Result<()> {
    store.write(|o| {
        if let Some(d) = days {
            o.meta_set(KEY_DAYS, &d.to_string())?;
        }
        if let Some(c) = changes {
            o.meta_set(KEY_ROWS, &c.to_string())?;
        }
        if history_keep(o)?.is_none() {
            o.history_stop()?;
        }
        Ok(())
    })?;
    Ok(())
}

/// After a sequence of rounds settled: the history starts being kept once a
/// first sync has finished, and keeps to its limits.
pub fn keep_history(store: &mut Store, now_ms: i64) -> Result<()> {
    store.write(|o| {
        match history_keep(o)? {
            Some(keep) => {
                if o.history_start(now_ms)? {
                    tracing::info!("keeping the history of what's synced from now on, for rewinds");
                }
                o.history_prune(keep, now_ms)?;
            }
            None => o.history_stop()?,
        }
        Ok(())
    })?;
    Ok(())
}

/// The rewind under way, if any.
pub fn load(o: &Ops<'_>) -> oneup_store::Result<Option<Job>> {
    Ok(o.meta_get(KEY)?.and_then(|s| Job::decode(&s)))
}

pub fn save(o: &Ops<'_>, job: &Job) -> oneup_store::Result<()> {
    o.meta_set(KEY, &job.encode())
}

/// Names below the synced folder's top, from `/docs/sub` (or `docs\sub`).
fn parts(folder: &str) -> Vec<&str> {
    folder.split(['/', '\\']).filter(|p| !p.is_empty() && *p != ".").collect()
}

/// The folder named `folder` at moment `at_ms`, checked: the history
/// reaches back that far, and it was a folder then.
fn folder_then(o: &Ops<'_>, folder: &str, at_ms: i64, now_ms: i64) -> Result<Sid> {
    let since = o.history_since()?;
    if since == i64::MAX {
        bail!("there's no history to rewind with yet: it starts once the first sync has finished");
    }
    if at_ms < since {
        bail!("the history reaches back to {} only", when(since));
    }
    if at_ms > now_ms {
        bail!("that moment hasn't come yet");
    }
    let names = parts(folder);
    core::folder_at(o, o, &names, at_ms)?.with_context(|| format!("there was no folder {folder} then"))
}

/// Plans the rewind of folder `sid` to `at_ms` from the store, through the
/// user's rules (what they leave out isn't rewound).
pub fn plan(o: &Ops<'_>, rules: &RuleSet, sid: Sid, at_ms: i64) -> Result<core::Rewind> {
    let view = oneup_plan::Parked::with_rules(o, rules);
    core::plan(&view, o, sid, at_ms, MAX).map_err(|e| match e {
        core::Error::TooMany => anyhow::anyhow!("that would change more than {MAX} items; pick a smaller folder"),
        core::Error::Read(e) => e.into(),
    })
}

/// What rewinding `folder` to `at_ms` would change (read-only).
pub fn preview(store: &Store, rules: &RuleSet, folder: &str, at_ms: i64, now_ms: i64) -> Result<Preview> {
    store.read(|o| Ok(preview_in(o, rules, folder, at_ms, now_ms)))?
}

fn preview_in(o: &Ops<'_>, rules: &RuleSet, folder: &str, at_ms: i64, now_ms: i64) -> Result<Preview> {
    let sid = folder_then(o, folder, at_ms, now_ms)?;
    let r = plan(o, rules, sid, at_ms)?;
    let (ops, paths): (Vec<Op>, Vec<String>) =
        r.ops.iter().cloned().zip(r.paths.iter().cloned()).chain(r.later.iter().cloned()).unzip();
    Ok(Preview {
        folder: folder.to_string(),
        at_ms,
        changes: r.changes,
        skipped: r.skipped.into_iter().map(|(_, p, w)| (p, w)).collect(),
        code: oneup_gate::rewind_fingerprint(&ops, &paths).0,
    })
}

/// Starts rewinding `folder` to `at_ms`: saved for the agent's next round,
/// where it waits for the user's approval as a safety stop. Refused while
/// another rewind is under way, in a backup pair (it never changes this
/// PC), and when there's nothing to change.
pub fn start(
    store: &mut Store,
    data_dir: &std::path::Path,
    rules: &RuleSet,
    folder: &str,
    at_ms: i64,
    now_ms: i64,
) -> Result<Preview> {
    if crate::flow::get(store)? == crate::flow::Flow::Backup {
        bail!("this folder is a backup: OneUpDrive never changes this PC there, so it can't rewind it");
    }
    if crate::vaultpair::is_vault_dir(data_dir) {
        bail!("this is an encrypted vault: OneDrive only has its encrypted files, so it can't be rewound");
    }
    store.write(|o| {
        if let Some(j) = load(o)? {
            return Ok(Err(anyhow::anyhow!(
                "a rewind of {} is under way; cancel it first (`oneup rewind --cancel`)",
                shown_folder(&j.folder)
            )));
        }
        let p = match preview_in(o, rules, folder, at_ms, now_ms) {
            Ok(p) => p,
            Err(e) => return Ok(Err(e)),
        };
        if p.changes.is_empty() {
            return Ok(Err(anyhow::anyhow!("nothing to rewind: {} is as it was then", shown_folder(folder))));
        }
        let sid = match folder_then(o, folder, at_ms, now_ms) {
            Ok(s) => s,
            Err(e) => return Ok(Err(e)),
        };
        let job = Job {
            folder: folder.to_string(),
            sid,
            at_ms,
            started_ms: now_ms,
            approval: None,
            stalls: 0,
            ran: 0,
            left_alone: 0,
            failed: 0,
        };
        save(o, &job)?;
        o.activity_add(&[note(
            now_ms,
            ActivityOutcome::Waiting,
            folder,
            format!("to {}; waiting for your approval (code {:016x})", when(at_ms), p.code),
        )])?;
        Ok(Ok(p))
    })?
}

/// Cancels the rewind under way; what it did stays done. Returns whether
/// there was one.
pub fn cancel(store: &mut Store, now_ms: i64) -> Result<bool> {
    Ok(store.write(|o| {
        let Some(j) = load(o)? else { return Ok(false) };
        o.meta_delete(KEY)?;
        let detail = format!("cancelled; {} changes made", j.ran);
        o.activity_add(&[note(now_ms, ActivityOutcome::Note, &j.folder, detail)])?;
        Ok(true)
    })?)
}

/// A moment as this PC's clock shows it (`2026-10-19 14:05:09`).
pub fn when(at_ms: i64) -> String {
    let offset = time::UtcOffset::current_local_offset().unwrap_or(time::UtcOffset::UTC);
    let fmt = time::macros::format_description!("[year]-[month]-[day] [hour]:[minute]:[second]");
    time::OffsetDateTime::from_unix_timestamp_nanos(i128::from(at_ms) * 1_000_000)
        .map(|t| t.to_offset(offset))
        .ok()
        .and_then(|t| t.format(fmt).ok())
        .unwrap_or_else(|| at_ms.to_string())
}

/// A moment the user typed: `2026-10-19 14:05`, with seconds or without,
/// in this PC's time, or Unix ms.
pub fn parse_when(s: &str) -> Result<i64> {
    let s = s.trim();
    if let Ok(ms) = s.parse::<i64>() {
        return Ok(ms);
    }
    let offset = time::UtcOffset::current_local_offset().unwrap_or(time::UtcOffset::UTC);
    let long = time::macros::format_description!("[year]-[month]-[day] [hour]:[minute]:[second]");
    let short = time::macros::format_description!("[year]-[month]-[day] [hour]:[minute]");
    let t = time::PrimitiveDateTime::parse(&s.replace('T', " "), long)
        .or_else(|_| time::PrimitiveDateTime::parse(&s.replace('T', " "), short))
        .with_context(|| format!("{s:?} isn't a moment like 2026-10-19 14:05"))?;
    let ms = t.assume_offset(offset).unix_timestamp_nanos() / 1_000_000;
    Ok(i64::try_from(ms)?)
}

fn shown_folder(folder: &str) -> String {
    if parts(folder).is_empty() { "the whole folder".into() } else { folder.to_string() }
}

fn note(at_ms: i64, outcome: ActivityOutcome, folder: &str, detail: String) -> ActivityEntry {
    ActivityEntry { seq: 0, at_ms, outcome, action: "rewind".into(), path: folder.to_string(), detail }
}

/// One round's part of a rewind ([`steps`]).
#[derive(Debug, Default, Clone)]
pub struct Steps {
    /// To add to the round, each shown at the same place in `paths`.
    pub ops: Vec<Op>,
    pub paths: Vec<String>,
    /// Folders it removes in a later round, for its stop's list.
    pub later: Shown,
    /// Approved: the keys the gate takes them by.
    pub keys: Vec<oneup_gate::ItemKey>,
    /// Items it changed, whose follow-ups here the breakers don't count.
    pub rewound: Vec<Sid>,
    /// Not approved yet: every step and folder of its list, which an
    /// approval of `code` approves.
    pub preview: Option<(u64, Vec<Op>)>,
    /// Steps planned that its approval doesn't cover.
    pub left_alone: usize,
}

/// The steps a round adds for rewind `job`, planned from the store now.
pub fn steps(o: &Ops<'_>, rules: &RuleSet, job: &Job) -> Result<Steps> {
    let r = plan(o, rules, job.sid, job.at_ms)?;
    let Some(approval) = &job.approval else {
        let (ops, paths): (Vec<Op>, Vec<String>) =
            r.ops.iter().cloned().zip(r.paths.iter().cloned()).chain(r.later.iter().cloned()).unzip();
        let code = oneup_gate::rewind_fingerprint(&ops, &paths).0;
        return Ok(Steps {
            ops: r.ops,
            paths: r.paths,
            later: r.later,
            preview: Some((code, ops)),
            ..Steps::default()
        });
    };
    let (take, leave) = approval.take(r.ops, r.paths);
    let later: Shown = r.later.into_iter().filter(|(op, _)| approval.accepts(op)).collect();
    let (ops, paths): (Vec<Op>, Vec<String>) = take.into_iter().unzip();
    let keys = oneup_gate::rewind_keys(&ops, &paths);
    Ok(Steps {
        ops,
        paths,
        later,
        keys,
        rewound: approval.rewound.iter().copied().collect(),
        preview: None,
        left_alone: leave.len(),
    })
}

/// After a round: notes what the rewind's steps did, and ends the rewind
/// once nothing approved is left (or it stalled). `steps`: what the round
/// added. Returns the activity log's entry when it ended.
pub fn ran(o: &Ops<'_>, job: &mut Job, steps: &Steps, report: &oneup_exec::Report, now_ms: i64) -> Result<bool> {
    let Some(approval) = job.approval.as_mut() else {
        // Waiting for the approval, with nothing left to change (the folder
        // was put back by hand, say): it ends.
        if steps.ops.is_empty() && steps.later.is_empty() {
            o.meta_delete(KEY)?;
            o.activity_add(&[note(now_ms, ActivityOutcome::Note, &job.folder, "nothing left to rewind".into())])?;
            return Ok(true);
        }
        return Ok(false);
    };
    let mine = |op: &&Op| matches!(op, Op::Rewind { .. });
    let done: Vec<&Op> = report.done.iter().filter(mine).collect();
    for op in &done {
        approval.ran(op, oneup_exec::rewind_mark(o, op)?);
    }
    job.ran += done.len() as u64;
    job.left_alone = job.left_alone.max(steps.left_alone as u64);
    job.failed += report.failed.iter().filter(|(op, _)| matches!(op, Op::Rewind { .. })).count() as u64;
    let tried = !steps.ops.is_empty();
    job.stalls = if tried && done.is_empty() { job.stalls + 1 } else { 0 };
    let finished = (!tried && steps.later.is_empty()) || job.stalls >= STALLS;
    if !finished {
        save(o, job)?;
        return Ok(false);
    }
    o.meta_delete(KEY)?;
    let mut detail = format!("done: {} changes", job.ran);
    if job.left_alone > 0 {
        detail.push_str(&format!(", {} items changed after the preview left alone", job.left_alone));
    }
    if job.stalls >= STALLS {
        detail.push_str("; some couldn't be done (see the failures above)");
    }
    o.activity_add(&[note(now_ms, ActivityOutcome::Note, &job.folder, detail)])?;
    Ok(true)
}

/// The content guard's stop `code`: rewind the folder its list is in to
/// just before the batch. The folder is the one all its items are in; the
/// moment is a second before the first suspicious upload the safety window
/// counted, or before the stop was raised if none ran.
pub fn before_stop(store: &Store, data_dir: &std::path::Path, code: &str, now_ms: i64) -> Result<(String, i64)> {
    let list = crate::stoplist::read(data_dir, code, "", crate::stoplist::MAX_LINES)
        .with_context(|| format!("no safety stop {code} stands"))?;
    if !list.header.contains("doesn't look like their type") {
        bail!("{code} isn't a content guard stop");
    }
    let folders: Vec<Vec<&str>> = list
        .rows
        .iter()
        .map(|r| {
            let mut p = parts(&r.path);
            p.pop();
            p
        })
        .collect();
    let mut common: Vec<&str> = folders.first().cloned().unwrap_or_default();
    for f in &folders[1.min(folders.len())..] {
        let same = common.iter().zip(f).take_while(|(a, b)| oneup_sync::same_name(a, b)).count();
        common.truncate(same);
    }
    let folder = format!("/{}", common.join("/"));
    let raised = store
        .read(|o| o.activity_recent(None, 2_000))?
        .into_iter()
        .find(|e| e.action == "safety stop" && e.detail.contains(code))
        .map_or(now_ms, |e| e.at_ms);
    let first = crate::safetywindow::SafetyWindow::load(store, now_ms)?.0.first_suspicious(now_ms);
    let at = first.unwrap_or(raised).min(raised).saturating_sub(1_000);
    Ok((folder, at))
}

/// A preview's lines, for `oneup rewind` and the window: the groups, then
/// each change.
pub fn describe(p: &Preview) -> Vec<String> {
    let mut out = Vec::new();
    for (kind, (n, bytes)) in p.groups() {
        let what = match kind {
            Kind::Restore => "restore deleted",
            Kind::Revert => "revert edited",
            Kind::MoveBack => "move back",
            Kind::Remove => "remove created since",
            Kind::SetAside => "set aside changed here",
        };
        out.push(format!("{what}: {n} ({})", crate::versions::size(u64::try_from(bytes).unwrap_or(0))));
    }
    for c in &p.changes {
        let from = match c.source {
            Source::RecycleBin => "from OneDrive's recycle bin",
            Source::Versions => "from its OneDrive version history",
            Source::InPlace => "",
        };
        let line = match (c.kind, &c.then, &c.now) {
            (Kind::MoveBack, Some(t), Some(n)) => format!("move back  {n} -> {t}"),
            (Kind::Remove, _, Some(n)) => format!("remove     {n}  (to OneDrive's recycle bin and .oneup\\versions)"),
            (Kind::SetAside, _, Some(n)) => {
                format!("set aside  {n}  (to .oneup\\versions; OneDrive's copy comes back)")
            }
            (Kind::Revert, Some(t), Some(n)) if t != n => format!("revert     {n} -> {t}  ({from})"),
            (Kind::Revert, _, Some(n)) => format!("revert     {n}  ({from})"),
            (Kind::Restore, Some(t), _) if from.is_empty() => format!("restore    {t}  (OneDrive's copy)"),
            (Kind::Restore, Some(t), _) => format!("restore    {t}  ({from})"),
            _ => continue,
        };
        out.push(line);
    }
    for (path, why) in &p.skipped {
        let why = match why {
            Skip::ChangedInCloud => "changed in OneDrive and not synced yet",
            Skip::ChangedHere => "moved on this PC and not synced yet",
            Skip::Missing => "missing, held",
            Skip::FolderGone => "its folder then is gone",
            Skip::NoCloudItem => "never in OneDrive",
            Skip::NotSyncedYet => "in OneDrive, not synced yet",
            Skip::UnknownContent => "its content then wasn't recorded",
        };
        out.push(format!("skipped    {path}  ({why})"));
    }
    out
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used)]
    use oneup_store::{Kind as RKind, LocalKind, LocalRow, RemoteRow};
    use oneup_sync::{Base, NodeKind, ROOT};

    use super::*;

    /// A synced folder holding `a.txt` (sid 1) with `content`, both sides
    /// as recorded.
    fn synced(store: &mut Store, content: u8, etag: &str) {
        store
            .write(|o| {
                let local = |id: u128, parent: Option<u128>, name: &str, kind: LocalKind, size: i64| LocalRow {
                    file_id: id,
                    parent_id: parent,
                    name: name.into(),
                    name_lossy: false,
                    kind,
                    size,
                    mtime: 5,
                    attributes: 0,
                    reparse_tag: 0,
                    skip: oneup_store::Skip::None,
                };
                o.local_put(&local(1, None, "", LocalKind::Root, 0))?;
                o.local_put(&local(10, Some(1), "a.txt", LocalKind::File, 3))?;
                let remote = |id: &str, parent: Option<&str>, name: &str, kind: RKind, qxh| RemoteRow {
                    id: id.into(),
                    parent_id: parent.map(str::to_string),
                    name: name.into(),
                    kind,
                    size: if kind == RKind::File { 3 } else { 0 },
                    etag: Some(etag.into()),
                    qxh,
                    mtime_ms: None,
                };
                o.upsert_item(&remote("root", None, "root", RKind::Root, None), false)?;
                o.upsert_item(&remote("r1", Some("root"), "a.txt", RKind::File, Some([content; 20])), false)?;
                o.synced_put(&Base {
                    sid: ROOT,
                    parent: None,
                    name: String::new(),
                    kind: NodeKind::Folder,
                    size: 0,
                    hash: None,
                    local: Some(LocalId(1)),
                    local_mtime: 0,
                    remote: Some(RemoteId("root".into())),
                    remote_etag: String::new(),
                })?;
                o.synced_put(&Base {
                    sid: 1,
                    parent: Some(ROOT),
                    name: "a.txt".into(),
                    kind: NodeKind::File,
                    size: 3,
                    hash: Some([content; 20]),
                    local: Some(LocalId(10)),
                    local_mtime: 5,
                    remote: Some(RemoteId("r1".into())),
                    remote_etag: etag.into(),
                })
            })
            .unwrap();
    }

    fn now() -> i64 {
        crate::sync::now_ms()
    }

    #[test]
    fn a_rewind_is_previewed_saved_and_approved_by_its_code_only() {
        let dir = std::env::temp_dir().join(format!("oneup-rewind-app-{}", std::process::id()));
        let mut store = Store::open_in_memory().unwrap();
        let rules = RuleSet::default();
        assert!(preview(&store, &rules, "/", now(), now()).is_err(), "no history before a first sync has finished");
        synced(&mut store, 1, "e1");
        store.write(|o| o.history_start(now() - 10)).unwrap();
        std::thread::sleep(std::time::Duration::from_millis(20));
        let at = now();
        std::thread::sleep(std::time::Duration::from_millis(20));
        let e = start(&mut store, &dir, &rules, "/", at, now()).unwrap_err();
        assert!(e.to_string().contains("nothing to rewind"), "{e}");
        assert!(preview(&store, &rules, "/", 0, now()).is_err(), "before the history began");

        // Edited since, and synced.
        synced(&mut store, 2, "e2");
        let p = start(&mut store, &dir, &rules, "/", at, now()).unwrap();
        assert_eq!(p.changes.len(), 1);
        assert_eq!((p.changes[0].kind, p.changes[0].source), (Kind::Revert, Source::Versions));
        assert!(start(&mut store, &dir, &rules, "/", at, now()).is_err(), "one at a time");

        // The round shows the same list, with the code the preview gave.
        let job = store.read(load).unwrap().unwrap();
        let s = store.read(|o| Ok(steps(o, &rules, &job))).unwrap().unwrap();
        let (code, all) = s.preview.clone().unwrap();
        assert_eq!(code, p.code);
        assert!(s.keys.is_empty(), "nothing approved yet");
        // Approved: its steps are keyed for the gate.
        let mut job = Job { approval: Some(oneup_sync::rewind::Job::approve(&all)), ..job };
        let s = store.read(|o| Ok(steps(o, &rules, &job))).unwrap().unwrap();
        assert!(s.preview.is_none() && s.keys.len() == 1 && s.ops.len() == 1);
        // Edited again after the preview: left alone.
        synced(&mut store, 3, "e3");
        let s2 = store.read(|o| Ok(steps(o, &rules, &job))).unwrap().unwrap();
        assert_eq!((s2.ops.len(), s2.left_alone), (0, 1));
        // With nothing left to run, it ends, and says so.
        let report = oneup_exec::Report::default();
        assert!(store.write(|o| Ok(ran(o, &mut job, &s2, &report, now()))).unwrap().unwrap());
        assert!(store.read(load).unwrap().is_none());
        let log = store.read(|o| o.activity_recent(None, 1)).unwrap();
        assert!(log[0].detail.contains("left alone"), "{:?}", log[0]);
    }

    #[test]
    fn a_rewind_whose_steps_keep_failing_ends() {
        let mut job = Job {
            folder: "/".into(),
            sid: ROOT,
            at_ms: 0,
            started_ms: 0,
            approval: Some(oneup_sync::rewind::Job::default()),
            stalls: 0,
            ran: 0,
            left_alone: 0,
            failed: 0,
        };
        let op = Op::rewind(1, oneup_sync::rewind::Step::Forget);
        let s = Steps { ops: vec![op.clone()], paths: vec!["/a".into()], ..Steps::default() };
        let report = oneup_exec::Report { failed: vec![(op, "no".into())], ..Default::default() };
        let mut store = Store::open_in_memory().unwrap();
        for round in 1..=STALLS {
            let ended = store.write(|o| Ok(ran(o, &mut job, &s, &report, 0))).unwrap().unwrap();
            assert_eq!(ended, round == STALLS, "round {round}");
        }
        assert_eq!(job.failed, u64::from(STALLS));
    }

    #[test]
    fn moments_read_as_typed() {
        assert_eq!(parse_when("1234").unwrap(), 1234);
        let a = parse_when("2026-10-19 14:05").unwrap();
        assert_eq!(parse_when("2026-10-19 14:05:00").unwrap(), a);
        assert_eq!(parse_when("2026-10-19T14:05:30").unwrap(), a + 30_000);
        assert!(parse_when("yesterday").is_err());
    }

    #[test]
    fn a_rewind_reads_back_as_saved() {
        let mut approval = Approval::default();
        approval.approved.insert(Key::Cloud(RemoteId("A!1".into())), Mark::Absent);
        approval.approved.insert(Key::Local(LocalId(0xabc)), Mark::Local(Some((3, 4))));
        approval.approved.insert(Key::Record(7), Mark::Record);
        let spot = Spot { parent: RemoteId("A!2".into()), name: "a b.txt".into(), size: 5, hash: Some([9; 20]) };
        approval.left.insert(Key::Cloud(RemoteId("A!3".into())), Mark::Cloud(spot));
        approval.rewound.insert(7);
        let job = Job {
            folder: "/docs".into(),
            sid: 3,
            at_ms: 100,
            started_ms: 200,
            approval: Some(approval),
            stalls: 1,
            ran: 2,
            left_alone: 1,
            failed: 0,
        };
        assert_eq!(Job::decode(&job.encode()), Some(job.clone()));
        let pending = Job { approval: None, ..job };
        assert_eq!(Job::decode(&pending.encode()), Some(pending));
        assert_eq!(Job::decode("{\"folder\": 1}"), None, "not one of ours: dropped, not guessed at");
    }
}
