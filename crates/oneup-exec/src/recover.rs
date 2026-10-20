//! Recovery: putting back a kept copy, a OneDrive version or an item
//! from OneDrive's recycle bin, and answering a conflict (the settings
//! window's **Recovery** page, `oneup restore`, `oneup history`, `oneup
//! recycle-bin`, `oneup conflicts`).
//!
//! Each is planned operations the caller queues for the agent's next
//! round, approved by the user's answer (its keys, `oneup_gate::rewind_keys`),
//! and run through the safety gate, the executor and the effects layer like
//! the rest, each under its own precondition:
//! * a kept copy ([`kept_op`], [`Step::RestoreKept`]) goes back into the
//!   synced folder, never over anything; the next round sends it;
//! * a OneDrive version ([`version_op`], [`Step::RevertTo`]) is uploaded as
//!   a new version under `If-Match` with the eTag the answer saw (Graph's
//!   `restoreVersion` ignores it on OneDrive Personal), so the version it
//!   replaces stays in the history and a file changed since is left alone;
//! * a deleted item ([`deleted_op`], [`Step::Restore`]) comes back from the
//!   recycle bin under a free name, replacing nothing;
//! * a conflict's answer ([`conflict_ops`]) removes or renames a copy here.
//!
//! This PC then gets what changed in OneDrive by syncing, as any change.

use oneup_store::{Kind, LocalKind, Ops};
use oneup_sync::rewind::Step;
use oneup_sync::{LocalId, LocalPre, NodeKind, Op, RemoteId, RemotePre, Sid};

/// The step that puts kept copy `slot` (its version store slot) back into
/// the synced folder, at `to` (a path in it) or where it came from.
pub fn kept_op(slot: &str, to: Option<&str>) -> Op {
    Op::rewind(0, Step::RestoreKept { slot: slot.to_string(), to: to.map(str::to_string) })
}

/// The step that makes version `version` of OneDrive file `id` its current
/// one, bound to the file as the store has it now (its eTag). `None`: it
/// isn't a file OneDrive is known to have.
pub fn version_op(o: &Ops<'_>, id: &RemoteId, version: &str) -> oneup_store::Result<Option<Op>> {
    let Some(row) = o.get_item(&id.0)? else { return Ok(None) };
    let Some(etag) = row.etag.filter(|_| row.kind == Kind::File) else { return Ok(None) };
    let sid = o.synced_by_remote(&id.0)?.map_or(0, |b| b.sid);
    let remote = RemotePre { id: id.clone(), etag };
    Ok(Some(Op::rewind(sid, Step::RevertTo { remote, version: version.to_string() })))
}

/// The `n`th name [`deleted_op`] tries after `name` itself:
/// `name (restored N).ext`, as the version store's restore names them.
pub fn restored_name(name: &str, n: u32, kind: NodeKind) -> String {
    match name.rfind('.').filter(|&i| i > 0 && kind == NodeKind::File) {
        Some(i) => format!("{} (restored {n}){}", &name[..i], &name[i..]),
        None => format!("{name} (restored {n})"),
    }
}

/// The step that brings deleted item `id` (record `sid`) back from
/// OneDrive's recycle bin into folder `parent`, as `name` or, if the store
/// lists that as taken, `name (restored N)`. Nothing is replaced: a name
/// taken meanwhile fails it (OneDrive refuses), and it can be asked again.
pub fn deleted_op(
    o: &Ops<'_>,
    sid: Sid,
    id: &RemoteId,
    parent: &RemoteId,
    name: &str,
    kind: NodeKind,
) -> oneup_store::Result<Op> {
    let taken: std::collections::HashSet<String> =
        o.remote_children(&parent.0)?.into_iter().map(|(r, _)| oneup_sync::fold(&r.name)).collect();
    let mut chosen = name.to_string();
    let mut n = 0;
    while taken.contains(&oneup_sync::fold(&chosen)) {
        n += 1;
        chosen = restored_name(name, n, kind);
    }
    Ok(Op::rewind(
        sid,
        Step::Restore { id: id.clone(), to: parent.clone(), name: chosen, kind, content: None, gone: None },
    ))
}

/// Two copies of a file that changed on both sides: the one under the
/// file's name, and the copy kept under a conflict name beside it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Conflict {
    pub folder: Sid,
    pub original: Copy,
    pub copy: Copy,
}

/// One side of a [`Conflict`], as this PC has it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Copy {
    pub sid: Sid,
    pub name: String,
    pub local: LocalId,
    pub size: i64,
    /// Last-write time (FILETIME).
    pub mtime: i64,
}

/// The answer to a conflict.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Keep {
    /// The file under its name; the conflict copy goes (into the version
    /// store here, OneDrive's recycle bin there).
    Original,
    /// The conflict copy, under the file's name; the other goes the same way.
    Copy,
}

/// The file name a conflict copy's name was made from
/// ([`oneup_sync::conflict_candidate`]: `stem (conflict N).ext` from
/// `stem.ext`); `None` if `name` isn't one.
pub fn conflict_original(name: &str) -> Option<String> {
    let at = name.rfind(" (conflict ")?;
    let rest = &name[at + " (conflict ".len()..];
    let close = rest.find(')')?;
    let (n, ext) = (&rest[..close], &rest[close + 1..]);
    if n.is_empty() || !n.bytes().all(|b| b.is_ascii_digit()) || n.starts_with('0') {
        return None;
    }
    // What follows is the extension, or nothing.
    if !(ext.is_empty() || (ext.starts_with('.') && !ext.contains(['(', ')']))) {
        return None;
    }
    let stem = &name[..at];
    (!stem.is_empty()).then(|| format!("{stem}{ext}"))
}

/// Whether `name` is a conflict copy's name of `of`: `stem (conflict N).ext`.
pub fn is_conflict_copy(name: &str, of: &str) -> bool {
    conflict_original(name).is_some_and(|o| oneup_sync::same_name(&o, of))
}

/// The conflicts in Synced folder `folder`: each synced file with a synced
/// conflict copy beside it, both on this PC as recorded. One indexed read
/// of the folder's records, one pass over them (each name parsed once,
/// matched through the folded names), and each pair's two local rows.
pub fn conflicts_in(o: &Ops<'_>, folder: Sid) -> oneup_store::Result<Vec<Conflict>> {
    let kids = o.synced_children(folder)?;
    let files: std::collections::HashMap<String, &oneup_sync::Base> =
        kids.iter().filter(|b| b.kind == NodeKind::File).map(|b| (oneup_sync::fold(&b.name), b)).collect();
    let mut out = Vec::new();
    for copy in kids.iter().filter(|b| b.kind == NodeKind::File) {
        let Some(name) = conflict_original(&copy.name) else { continue };
        let Some(original) = files.get(&oneup_sync::fold(&name)).filter(|b| b.sid != copy.sid) else { continue };
        let (Some(a), Some(b)) = (local_copy(o, original)?, local_copy(o, copy)?) else { continue };
        out.push(Conflict { folder, original: a, copy: b });
    }
    out.sort_by(|a, b| a.original.name.cmp(&b.original.name).then(a.copy.name.cmp(&b.copy.name)));
    Ok(out)
}

fn local_copy(o: &Ops<'_>, b: &oneup_sync::Base) -> oneup_store::Result<Option<Copy>> {
    let Some(l) = b.local else { return Ok(None) };
    let Some(row) = o.local_get(l.0)? else { return Ok(None) };
    if row.kind != LocalKind::File {
        return Ok(None);
    }
    Ok(Some(Copy { sid: b.sid, name: row.name, local: l, size: row.size, mtime: row.mtime }))
}

/// The operations that answer conflict `c` with `keep`, from what the
/// store holds now: the copy not kept goes into the version store as a
/// deletion made here ([`Step::Discard`]; the next round deletes it from
/// OneDrive, into its recycle bin), and, keeping the conflict copy, that
/// copy is renamed to the file's name once the other has freed it (the
/// claims keep that order). Each runs under its own precondition (the
/// file's size and last-write time as listed), so a copy changed since is
/// skipped. The answer is the user's approval of the discarding step: the
/// caller passes its key as approved (`oneup_gate::rewind_keys`). `None`:
/// they're no longer as listed.
pub fn conflict_ops(o: &Ops<'_>, c: &Conflict, keep: Keep) -> oneup_store::Result<Option<Vec<Op>>> {
    let now = conflicts_in(o, c.folder)?;
    if !now.contains(c) {
        return Ok(None);
    }
    let Some(dir) = o.synced_get(c.folder)?.and_then(|b| b.local) else { return Ok(None) };
    let pre = |x: &Copy| LocalPre { id: x.local, stamp: Some((x.size, x.mtime)) };
    let discard = |x: &Copy| Op::rewind(x.sid, Step::Discard { local: pre(x) });
    Ok(Some(match keep {
        Keep::Original => vec![discard(&c.copy)],
        Keep::Copy => vec![
            discard(&c.original),
            // A rename here, like the user's own: the next round takes it
            // to OneDrive.
            Op::RenameLocal { local: pre(&c.copy), dir, name: c.original.name.clone() },
        ],
    }))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn conflict_and_restored_names() {
        assert!(is_conflict_copy("report (conflict 1).docx", "report.docx"));
        assert!(is_conflict_copy("REPORT (conflict 12).DOCX", "report.docx"));
        assert!(!is_conflict_copy("report (conflict).docx", "report.docx"));
        assert!(!is_conflict_copy("other (conflict 1).docx", "report.docx"));
        // Any number: names aren't guessed one by one.
        assert!(is_conflict_copy("report (conflict 150).docx", "report.docx"));
        assert_eq!(conflict_original("a.b (conflict 2).txt").as_deref(), Some("a.b.txt"));
        assert_eq!(conflict_original(".gitignore (conflict 1)").as_deref(), Some(".gitignore"));
        assert_eq!(conflict_original("notes (conflict 3)").as_deref(), Some("notes"));
        assert_eq!(conflict_original("x (conflict 1) y.txt"), None);
        assert_eq!(conflict_original(" (conflict 1).txt"), None);
        assert_eq!(restored_name("a.txt", 2, NodeKind::File), "a (restored 2).txt");
        assert_eq!(restored_name("dir.d", 1, NodeKind::Folder), "dir.d (restored 1)");
    }
}
