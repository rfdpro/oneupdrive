//! Restores the user asked for: a kept copy back into the synced folder
//! (`oneup restore`), a OneDrive version made current again (`oneup
//! history --restore`), an item back from OneDrive's recycle bin (`oneup
//! recycle-bin --restore`), and the same from the settings window.
//!
//! None of them changes anything by itself. The answer is queued in `meta`
//! ([`KEY`]) as the exact step it asks for (`oneup_exec::recover`), bound
//! to what the user saw (a OneDrive file's eTag, say); the agent's next
//! round adds it to its plan, approved by the answer (its key), and runs
//! it through the safety gate, the executor and the effects layer like any
//! operation. A step whose item changed since is skipped and logged.

use anyhow::{Result, bail};
use oneup_store::{ActivityEntry, ActivityOutcome, Ops, Store};
use oneup_sync::rewind::{Spot, Step};
use oneup_sync::{LocalId, LocalPre, NodeKind, Op, RemoteId, RemotePre};
use serde_json::{Value, json};

/// In `meta`: the restores waiting for the agent (JSON).
pub const KEY: &str = "recovery.restore";

/// At most this many wait.
pub const MAX: usize = 1_000;

/// A restore as queued: its step, and what the log calls it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Asked {
    pub op: Op,
    pub path: String,
}

fn kind_name(k: NodeKind) -> &'static str {
    match k {
        NodeKind::File => "file",
        NodeKind::Folder => "folder",
    }
}

fn to_json(a: &Asked) -> Option<Value> {
    let Op::Rewind { sid, step } = &a.op else { return None };
    Some(match &**step {
        Step::RestoreKept { slot, item, dir, name } => json!({
            "kept": slot, "item": format!("{:x}", item.id.0), "stamp": item.stamp.map(|(s, m)| [s, m]),
            "dir": format!("{:x}", dir.0), "name": name, "path": a.path,
        }),
        Step::RevertTo { remote, version } => json!({
            "sid": sid, "version": version, "id": remote.id.0, "etag": remote.etag, "path": a.path,
        }),
        Step::Restore { id, to, name, kind, content: None, gone: None } => json!({
            "sid": sid, "deleted": id.0, "parent": to.0, "name": name, "kind": kind_name(*kind), "path": a.path,
        }),
        // A restore's follow-up (`oneup_exec::recover::follow_up`).
        Step::Reset { remote, from, place: Some((to, name)), content: None, toward: None } => json!({
            "sid": sid, "move": remote.id.0, "etag": remote.etag, "parent": to.0, "name": name, "path": a.path,
            "from": { "parent": from.parent.0, "name": from.name, "size": from.size, "hash": from.hash.map(|h| hex(&h)) },
        }),
        _ => return None,
    })
}

fn from_json(v: &Value) -> Option<Asked> {
    let s = |k: &str| v.get(k).and_then(Value::as_str).map(str::to_string);
    let path = s("path").unwrap_or_default();
    let sid = v.get("sid").and_then(Value::as_u64).unwrap_or(0);
    let op = if let Some(id) = s("move") {
        let f = v.get("from")?;
        let fs = |k: &str| f.get(k).and_then(Value::as_str).map(str::to_string);
        let hash = match f.get("hash").and_then(Value::as_str) {
            Some(h) => Some(unhex(h)?),
            None => None,
        };
        let from = Spot { parent: RemoteId(fs("parent")?), name: fs("name")?, size: f.get("size")?.as_i64()?, hash };
        let remote = RemotePre { id: RemoteId(id), etag: s("etag")? };
        Op::rewind(
            sid,
            Step::Reset {
                remote,
                from,
                place: Some((RemoteId(s("parent")?), s("name")?)),
                content: None,
                toward: None,
            },
        )
    } else if let Some(slot) = s("kept") {
        let id = |k: &str| u128::from_str_radix(v.get(k)?.as_str()?, 16).ok();
        let stamp = match v.get("stamp").and_then(Value::as_array) {
            Some(a) => Some((a.first()?.as_i64()?, a.get(1)?.as_i64()?)),
            None => None,
        };
        let item = LocalPre { id: LocalId(id("item")?), stamp };
        Op::rewind(0, Step::RestoreKept { slot, item, dir: LocalId(id("dir")?), name: s("name")? })
    } else if let Some(version) = s("version") {
        let remote = RemotePre { id: RemoteId(s("id")?), etag: s("etag")? };
        Op::rewind(sid, Step::RevertTo { remote, version })
    } else {
        let kind = match s("kind")?.as_str() {
            "folder" => NodeKind::Folder,
            _ => NodeKind::File,
        };
        let step = Step::Restore {
            id: RemoteId(s("deleted")?),
            to: RemoteId(s("parent")?),
            name: s("name")?,
            kind,
            content: None,
            gone: None,
        };
        Op::rewind(sid, step)
    };
    Some(Asked { op, path })
}

fn hex(b: &[u8]) -> String {
    b.iter().map(|x| format!("{x:02x}")).collect()
}

fn unhex(s: &str) -> Option<[u8; 20]> {
    let mut out = [0u8; 20];
    if s.len() != 40 {
        return None;
    }
    for (i, o) in out.iter_mut().enumerate() {
        *o = u8::from_str_radix(s.get(2 * i..2 * i + 2)?, 16).ok()?;
    }
    Some(out)
}

fn read_queue(o: &Ops<'_>) -> oneup_store::Result<Vec<Asked>> {
    let Some(text) = o.meta_get(KEY)? else { return Ok(Vec::new()) };
    let v: Value = serde_json::from_str(&text).unwrap_or(Value::Null);
    Ok(v.as_array().into_iter().flatten().filter_map(from_json).collect())
}

fn write_queue(o: &Ops<'_>, q: &[Asked]) -> oneup_store::Result<()> {
    if q.is_empty() {
        return o.meta_delete(KEY);
    }
    o.meta_set(KEY, &Value::Array(q.iter().filter_map(to_json).collect()).to_string())
}

/// Queues `asked` for the agent's next round. The caller nudges the agent.
pub fn ask(store: &mut Store, asked: Asked) -> Result<()> {
    if to_json(&asked).is_none() {
        bail!("that isn't a restore");
    }
    store.write(|o| {
        let mut q = read_queue(o)?;
        if q.iter().any(|x| x.op == asked.op) {
            return Ok(Err(anyhow::anyhow!("that restore is asked for already")));
        }
        if q.len() >= MAX {
            return Ok(Err(anyhow::anyhow!("{MAX} restores wait already; let the agent catch up")));
        }
        q.push(asked);
        write_queue(o, &q)?;
        Ok(Ok(()))
    })?
}

/// The restores waiting, oldest first.
pub fn waiting(o: &Ops<'_>) -> oneup_store::Result<Vec<Asked>> {
    read_queue(o)
}

/// Kept copy `slot` (as `oneup versions` lists it) back into the synced
/// folder, at `to` or where it came from. Returns that path.
pub fn kept(store: &mut Store, slot: &str, to: Option<&str>) -> Result<String> {
    let fs = crate::sync::open_paired(store)?;
    if !fs.versions()?.iter().any(|s| s.id == slot) {
        bail!("there's no kept item {slot} (see `oneup versions`)");
    }
    // Bound to the kept item and the folder it goes into, by their IDs.
    let t = fs.kept_target(slot, to).map_err(|e| match e {
        oneup_effects::Error::Changed => anyhow::anyhow!("the folder it goes into isn't there; give another (--to)"),
        e => anyhow::anyhow!("{e}"),
    })?;
    let path = format!("/{}", t.rel.replace('\\', "/"));
    ask(store, Asked { op: oneup_exec::recover::kept_op(slot, &t), path: path.clone() })?;
    Ok(path)
}

/// Version `version` of OneDrive file `id` made current again, if the file
/// is still as this PC last saw it when the agent gets to it.
pub fn version(store: &mut Store, id: &RemoteId, version: &str, path: &str) -> Result<()> {
    let Some(op) = store.read(|o| oneup_exec::recover::version_op(o, id, version))? else {
        bail!("OneUpDrive doesn't know that file in OneDrive; sync first");
    };
    ask(store, Asked { op, path: path.to_string() })
}

/// Deleted item `seq` (as `crate::cloudbin::list` lists it) back from
/// OneDrive's recycle bin into the folder it was in. Returns where it goes.
pub fn deleted(store: &mut Store, seq: i64) -> Result<String> {
    let found = store.read(|o| {
        let Some(d) = crate::cloudbin::list(o, crate::cloudbin::MAX)?.into_iter().find(|d| d.seq == seq) else {
            return Ok(None);
        };
        let Some(folder) = d.folder.clone() else {
            return Ok(Some(Err("the folder it was in is gone too: bring that back first")));
        };
        if d.elsewhere {
            return Ok(Some(Err("restore it from OneDrive's recycle bin on the web")));
        }
        let op = oneup_exec::recover::deleted_op(d.sid, &d.id, &folder, &d.name, d.kind);
        Ok(Some(Ok((d, op))))
    })?;
    let (d, op) = match found {
        None => bail!("that item isn't listed as deleted any more"),
        Some(Err(why)) => bail!("{why}"),
        Some(Ok(x)) => x,
    };
    let Op::Rewind { step, .. } = &op else { bail!("not a restore") };
    let Step::Restore { name, .. } = &**step else { bail!("not a restore") };
    let at = format!("{}/{name}", d.path.rsplit_once('/').map_or("", |(p, _)| p));
    ask(store, Asked { op, path: at.clone() })?;
    Ok(at)
}

/// The restores a round takes in ([`queued`]).
#[derive(Debug, Default, Clone)]
pub struct Queued {
    pub ops: Vec<Op>,
    asked: Vec<Asked>,
}

fn entry(outcome: ActivityOutcome, path: String, detail: String) -> ActivityEntry {
    ActivityEntry { seq: 0, at_ms: crate::sync::now_ms(), outcome, action: "restore".into(), path, detail }
}

/// The queued restores' steps for this round.
pub fn queued(store: &Store) -> Result<Queued> {
    let asked = store.read(read_queue)?;
    Ok(Queued { ops: asked.iter().map(|a| a.op.clone()).collect(), asked })
}

fn done_detail(op: &Op) -> &'static str {
    match op {
        Op::Rewind { step, .. } => match &**step {
            Step::RestoreKept { .. } => "from the version store; OneDrive gets it next",
            Step::RevertTo { .. } => "an old version is current again; the one it replaced stays in the history",
            Step::Reset { .. } => "moved where it was asked back to",
            _ => "from OneDrive's recycle bin",
        },
        _ => "",
    }
}

/// After the round: a restore that ran is done; one skipped or failed is
/// dropped and says why; one that didn't run (a safety stop, a pause)
/// waits for the next round.
pub fn ran(store: &mut Store, q: &Queued, report: &oneup_exec::Report) -> Result<()> {
    if q.asked.is_empty() {
        return Ok(());
    }
    store.write(|o| {
        let mut left = read_queue(o)?;
        let mut log = Vec::new();
        for a in &q.asked {
            if report.done.contains(&a.op) {
                // Back somewhere other than asked: its move follows, bound to
                // it as it came back, through the gate like the restore.
                if let Some(next) = oneup_exec::recover::follow_up(o, &a.op)? {
                    let came = match next.rewind_step() {
                        Some(Step::Reset { from, .. }) => from.name.clone(),
                        _ => String::new(),
                    };
                    let detail = format!("OneDrive brought it back as {came}; moved where asked next");
                    log.push(entry(ActivityOutcome::Note, a.path.clone(), detail));
                    left.push(Asked { op: next, path: a.path.clone() });
                } else {
                    log.push(entry(ActivityOutcome::Done, a.path.clone(), done_detail(&a.op).into()));
                }
            } else if let Some((_, s)) = report.skipped.iter().find(|(o, _)| *o == a.op) {
                let why = format!("not restored: {}", crate::sync::skip_label(*s));
                log.push(entry(ActivityOutcome::Failed, a.path.clone(), why));
            } else if let Some((_, e)) = report.failed.iter().find(|(o, _)| *o == a.op) {
                log.push(entry(ActivityOutcome::Failed, a.path.clone(), format!("not restored: {e}")));
            } else {
                continue;
            }
            left.retain(|x| x.op != a.op);
        }
        write_queue(o, &left)?;
        o.activity_add(&log)?;
        Ok(())
    })?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_queued_restore_reads_back_as_the_same_step() {
        let ops = [
            Op::rewind(
                0,
                Step::RestoreKept {
                    slot: "slot-1".into(),
                    item: LocalPre { id: LocalId(0xabc), stamp: Some((3, 99)) },
                    dir: LocalId(0x12),
                    name: "a.txt".into(),
                },
            ),
            Op::rewind(
                0,
                Step::RestoreKept {
                    slot: "slot-2".into(),
                    item: LocalPre { id: LocalId(0xdef), stamp: None },
                    dir: LocalId(0x5),
                    name: "d".into(),
                },
            ),
            Op::rewind(
                7,
                Step::RevertTo {
                    remote: RemotePre { id: RemoteId("A!1".into()), etag: "e1".into() },
                    version: "3.0".into(),
                },
            ),
            Op::rewind(
                9,
                Step::Restore {
                    id: RemoteId("A!2".into()),
                    to: RemoteId("A!0".into()),
                    name: "b (restored 1).txt".into(),
                    kind: NodeKind::File,
                    content: None,
                    gone: None,
                },
            ),
            Op::rewind(
                9,
                Step::Reset {
                    remote: RemotePre { id: RemoteId("A!2".into()), etag: "e2".into() },
                    from: Spot { parent: RemoteId("A!0".into()), name: "b 1.txt".into(), size: 3, hash: Some([7; 20]) },
                    place: Some((RemoteId("A!0".into()), "b.txt".into())),
                    content: None,
                    toward: None,
                },
            ),
        ];
        for op in ops {
            let a = Asked { op, path: "/x".into() };
            assert_eq!(from_json(&to_json(&a).unwrap()).as_ref(), Some(&a));
        }
    }

    /// Brought back somewhere other than asked: the restore isn't done, its
    /// move bound to the item as it came back waits in the queue.
    #[test]
    fn a_restore_back_elsewhere_queues_its_move() {
        use oneup_store::{Kind, RemoteRow};
        let mut store = Store::open_in_memory().unwrap();
        let row = |name: &str| RemoteRow {
            id: "A!2".into(),
            parent_id: Some("A!0".into()),
            name: name.into(),
            kind: Kind::File,
            size: 3,
            etag: Some("e2".into()),
            qxh: None,
            mtime_ms: None,
        };
        store.write(|o| o.upsert_item(&row("b 1.txt"), false).map(|_| ())).unwrap();
        let restore = Op::rewind(
            9,
            Step::Restore {
                id: RemoteId("A!2".into()),
                to: RemoteId("A!0".into()),
                name: "b.txt".into(),
                kind: NodeKind::File,
                content: None,
                gone: None,
            },
        );
        ask(&mut store, Asked { op: restore.clone(), path: "/b.txt".into() }).unwrap();
        let q = queued(&store).unwrap();
        let report = oneup_exec::Report { done: vec![restore.clone()], ..Default::default() };
        ran(&mut store, &q, &report).unwrap();
        let left = store.read(waiting).unwrap();
        assert_eq!(left.len(), 1);
        let Some(Step::Reset { remote, from, place, content: None, .. }) = left[0].op.rewind_step() else {
            panic!("{left:?}")
        };
        assert_eq!((remote.etag.as_str(), from.name.as_str()), ("e2", "b 1.txt"));
        assert_eq!(place.as_ref().map(|(_, n)| n.as_str()), Some("b.txt"));
        // Where asked: done.
        store.write(|o| o.upsert_item(&row("b.txt"), false).map(|_| ())).unwrap();
        let q = queued(&store).unwrap();
        let report = oneup_exec::Report { done: q.ops.clone(), ..Default::default() };
        ran(&mut store, &q, &report).unwrap();
        assert!(store.read(waiting).unwrap().is_empty());
    }
}
