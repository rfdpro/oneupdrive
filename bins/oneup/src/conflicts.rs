//! Conflicts: a file changed on both sides, kept as two copies (the file
//! and `name (conflict N).ext` beside it). `oneup conflicts` and the
//! settings window's **Recovery** page list them side by side, with which
//! is newer, and take the answer: keep this one, keep that one, or keep
//! both.
//!
//! Keeping one is queued in `meta` ([`KEY`]) for the agent, whose next
//! round adds the answer's steps (`oneup_exec::recover::conflict_ops`) to
//! its plan: the copy not kept goes into the version store as a deletion
//! made here, and OneDrive's copy into its recycle bin; keeping the
//! conflict copy also renames it to the file's name. They go through the
//! safety gate and the effects layer like any operation, each under its own
//! precondition, so a copy changed since the answer is left alone. Keeping
//! both changes nothing.

use anyhow::{Result, bail};
use oneup_exec::recover::{self, Conflict, Copy, Keep};
use oneup_store::{ActivityEntry, ActivityOutcome, Ops, Store};
use oneup_sync::{LocalId, Op};
use serde_json::{Value, json};

/// In `meta`: the answers waiting for the agent (JSON).
pub const KEY: &str = "conflicts.answer";

/// At most this many wait, and are listed.
pub const MAX: usize = 1_000;

/// A conflict as listed: the two copies and where they are.
#[derive(Debug, Clone)]
pub struct Listed {
    pub conflict: Conflict,
    /// The file's path, relative to the synced folder (`/docs/r.txt`).
    pub path: String,
    pub copy_path: String,
}

/// The conflicts in the synced folder, at most [`MAX`]. Found by name (a
/// search of the Local tree's names, as the Files page's search is: for
/// someone asking, not for anything that runs by itself).
pub fn list(o: &Ops<'_>) -> oneup_store::Result<Vec<Listed>> {
    let mut folders: Vec<oneup_sync::Sid> = Vec::new();
    for row in o.local_find(" (conflict ", MAX * 4)? {
        if let Some(b) = o.synced_by_local(row.file_id)?
            && let Some(p) = b.parent
            && !folders.contains(&p)
        {
            folders.push(p);
        }
    }
    let mut out = Vec::new();
    for f in folders {
        for c in recover::conflicts_in(o, f)? {
            let path = o.local_path_of(c.original.local.0)?.unwrap_or_default();
            let copy_path = o.local_path_of(c.copy.local.0)?.unwrap_or_default();
            out.push(Listed { conflict: c, path: slashed(&path), copy_path: slashed(&copy_path) });
            if out.len() >= MAX {
                return Ok(out);
            }
        }
    }
    out.sort_by(|a, b| a.path.cmp(&b.path).then(a.copy_path.cmp(&b.copy_path)));
    Ok(out)
}

fn slashed(p: &str) -> String {
    let p = p.replace('\\', "/");
    if p.starts_with('/') { p } else { format!("/{p}") }
}

fn copy_json(c: &Copy) -> Value {
    json!({ "sid": c.sid, "name": c.name, "local": format!("{:x}", c.local.0), "size": c.size, "mtime": c.mtime })
}

fn copy_of(v: &Value) -> Option<Copy> {
    Some(Copy {
        sid: v.get("sid")?.as_u64()?,
        name: v.get("name")?.as_str()?.to_string(),
        local: LocalId(u128::from_str_radix(v.get("local")?.as_str()?, 16).ok()?),
        size: v.get("size")?.as_i64()?,
        mtime: v.get("mtime")?.as_i64()?,
    })
}

/// A conflict as JSON, for the window and `--json`: both copies, their
/// sizes and last-write times, and which is newer.
pub fn to_json(l: &Listed) -> Value {
    let c = &l.conflict;
    let newer = match c.original.mtime.cmp(&c.copy.mtime) {
        std::cmp::Ordering::Greater => "original",
        std::cmp::Ordering::Less => "copy",
        std::cmp::Ordering::Equal => "",
    };
    json!({
        "folder": c.folder,
        "path": l.path,
        "copy_path": l.copy_path,
        "original": copy_json(&c.original),
        "copy": copy_json(&c.copy),
        // FILETIME to Unix ms, for showing.
        "original_ms": filetime_ms(c.original.mtime),
        "copy_ms": filetime_ms(c.copy.mtime),
        "newer": newer,
    })
}

/// FILETIME (100 ns since 1601) to Unix ms.
pub fn filetime_ms(ft: i64) -> i64 {
    ft / 10_000 - 11_644_473_600_000
}

/// The conflict as `v` lists it ([`to_json`]).
pub fn from_json(v: &Value) -> Option<Conflict> {
    Some(Conflict {
        folder: v.get("folder")?.as_u64()?,
        original: copy_of(v.get("original")?)?,
        copy: copy_of(v.get("copy")?)?,
    })
}

fn keep_name(k: Keep) -> &'static str {
    match k {
        Keep::Original => "original",
        Keep::Copy => "copy",
    }
}

fn read_queue(o: &Ops<'_>) -> oneup_store::Result<Vec<(Conflict, Keep)>> {
    let Some(text) = o.meta_get(KEY)? else { return Ok(Vec::new()) };
    let v: Value = serde_json::from_str(&text).unwrap_or(Value::Null);
    Ok(v.as_array()
        .into_iter()
        .flatten()
        .filter_map(|a| {
            let keep = match a.get("keep")?.as_str()? {
                "original" => Keep::Original,
                "copy" => Keep::Copy,
                _ => return None,
            };
            Some((from_json(a.get("conflict")?)?, keep))
        })
        .collect())
}

fn write_queue(o: &Ops<'_>, q: &[(Conflict, Keep)]) -> oneup_store::Result<()> {
    if q.is_empty() {
        return o.meta_delete(KEY);
    }
    let v: Vec<Value> = q
        .iter()
        .map(|(c, k)| {
            json!({ "conflict": {
                "folder": c.folder, "original": copy_json(&c.original), "copy": copy_json(&c.copy),
            }, "keep": keep_name(*k) })
        })
        .collect();
    o.meta_set(KEY, &Value::Array(v).to_string())
}

/// Queues the answer `keep` for conflict `c`, which must still be exactly as
/// listed (both copies' sizes and times). The caller nudges the agent.
pub fn answer(store: &mut Store, c: &Conflict, keep: Keep) -> Result<()> {
    store.write(|o| {
        if recover::conflict_ops(o, c, keep)?.is_none() {
            return Ok(Err(anyhow::anyhow!("that conflict has changed since it was listed; look again")));
        }
        let mut q = read_queue(o)?;
        if q.iter().any(|(x, _)| x == c) {
            return Ok(Err(anyhow::anyhow!("that conflict is answered already")));
        }
        if q.len() >= MAX {
            return Ok(Err(anyhow::anyhow!("{MAX} answers wait already; let the agent catch up")));
        }
        q.push((c.clone(), keep));
        write_queue(o, &q)?;
        Ok(Ok(()))
    })?
}

/// The answers a round takes in ([`queued`]).
#[derive(Debug, Default, Clone)]
pub struct Queued {
    pub ops: Vec<Op>,
    /// For each answer still standing: the conflict, and its steps.
    answers: Vec<(Conflict, Keep, Vec<Op>)>,
    pub log: Vec<ActivityEntry>,
}

fn entry(outcome: ActivityOutcome, path: String, detail: String) -> ActivityEntry {
    ActivityEntry { seq: 0, at_ms: crate::sync::now_ms(), outcome, action: "conflict".into(), path, detail }
}

/// The queued answers' steps for this round. An answer whose conflict
/// changed since is dropped, and logged.
pub fn queued(store: &mut Store) -> Result<Queued> {
    Ok(store.write(|o| {
        let q = read_queue(o)?;
        let mut out = Queued::default();
        let mut keep = Vec::new();
        for (c, k) in q {
            let path = o.local_path_of(c.original.local.0)?.map(|p| slashed(&p)).unwrap_or_default();
            match recover::conflict_ops(o, &c, k)? {
                Some(ops) => {
                    out.ops.extend(ops.iter().cloned());
                    out.answers.push((c.clone(), k, ops));
                    keep.push((c, k));
                }
                None => out.log.push(entry(
                    ActivityOutcome::Failed,
                    path,
                    "not answered: one of the copies changed or went since".into(),
                )),
            }
        }
        write_queue(o, &keep)?;
        Ok(out)
    })?)
}

/// After the round: an answer whose steps all ran is done; one with a step
/// skipped or failed is dropped (its conflict changed meanwhile); one whose
/// steps didn't run (a safety stop, a pause) waits for the next round.
pub fn ran(store: &mut Store, q: &Queued, report: &oneup_exec::Report) -> Result<()> {
    if q.answers.is_empty() {
        return Ok(());
    }
    let ran = |op: &Op| report.done.contains(op);
    let stuck = |op: &Op| report.skipped.iter().any(|(o, _)| o == op) || report.failed.iter().any(|(o, _)| o == op);
    store.write(|o| {
        let mut left = read_queue(o)?;
        let mut log = Vec::new();
        for (c, k, ops) in &q.answers {
            let path = o.local_path_of(c.original.local.0)?.map(|p| slashed(&p)).unwrap_or_default();
            if ops.iter().all(ran) {
                let kept = match k {
                    Keep::Original => format!("kept {}; {} to the version store", c.original.name, c.copy.name),
                    Keep::Copy => {
                        format!("kept {} as {}; the other to the version store", c.copy.name, c.original.name)
                    }
                };
                log.push(entry(ActivityOutcome::Done, path, kept));
            } else if ops.iter().any(stuck) {
                log.push(entry(ActivityOutcome::Failed, path, "not answered: a copy changed meanwhile".into()));
            } else {
                continue;
            }
            left.retain(|(x, _)| x != c);
        }
        write_queue(o, &left)?;
        o.activity_add(&log)?;
        Ok(())
    })?;
    Ok(())
}

/// Parses `original`, `copy` or `both`.
pub fn parse_keep(s: &str) -> Result<Option<Keep>> {
    Ok(match s {
        "original" | "this" => Some(Keep::Original),
        "copy" | "that" => Some(Keep::Copy),
        "both" => None,
        _ => bail!("keep original, copy or both"),
    })
}
