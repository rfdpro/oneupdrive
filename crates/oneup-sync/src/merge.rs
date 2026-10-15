//! Per-folder three-way merge: decide what to do with each child of one
//! Synced folder. The rules are tabulated in `docs/architecture/sync-engine.md`;
//! this module is their implementation and nothing else.
//!
//! Each item is decided exactly once per round:
//! * a **Synced** item in the context of its *agreed* parent, wherever it is
//!   now on either side (so a move is seen as a move, not as a deletion
//!   here plus a creation there);
//! * a **new** item (no Synced row) in the context of its current parent.
//!
//! Names chosen here (conflict copies) are only a first guess. The resolve
//! pass ([`crate::resolve`]) is the authority on whether a name is free.

use std::collections::{BTreeMap, BTreeSet, HashMap};

use crate::Flow;
use crate::model::{
    Base, LocalId, LocalNode, NodeKind, ROOT, RemoteId, RemoteNode, Seen, Sid, Trees, fold, local_of, remote_of,
};
use crate::op::{HoldReason, Link, LocalPre, Op, RemotePre};

pub(crate) fn lpre(n: &LocalNode) -> LocalPre {
    LocalPre { id: n.id, stamp: (n.kind == NodeKind::File).then_some((n.size, n.mtime)) }
}

pub(crate) fn rpre(n: &RemoteNode) -> RemotePre {
    RemotePre { id: n.id.clone(), etag: n.etag.clone() }
}

/// How a local file compares with its Synced row.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum LocalChange {
    Same,
    /// Different stamp, but a fresh hash proves the content is what was
    /// synced (e.g. only the last-write time was touched).
    Touched,
    Changed,
}

fn local_change(b: &Base, l: &LocalNode) -> LocalChange {
    if b.local == Some(l.id) && l.size == b.size && l.mtime == b.local_mtime {
        LocalChange::Same
    } else if l.hash.is_some() && l.hash == b.hash && l.size == b.size {
        LocalChange::Touched
    } else {
        LocalChange::Changed
    }
}

fn remote_changed(b: &Base, r: &RemoteNode) -> bool {
    match (r.hash, b.hash) {
        (Some(now), Some(then)) => now != then || r.size != b.size,
        // Without a hash (rare; Graph sometimes omits it), any eTag change
        // counts. False positives only cost a re-download, which the
        // executor recognises as identical.
        _ => r.size != b.size || r.etag != b.remote_etag,
    }
}

/// Where a local item is now, as an agreed location (parent `sid`, name).
/// `None` if its parent isn't synced yet (it was moved into a new folder).
fn local_loc<T: Trees + ?Sized>(t: &T, n: &LocalNode) -> Result<Option<(Sid, String)>, T::Error> {
    let Some(p) = n.parent else { return Ok(None) };
    Ok(t.base_by_local(p)?.filter(|b| b.kind == NodeKind::Folder).map(|b| (b.sid, n.name.clone())))
}

fn remote_loc<T: Trees + ?Sized>(t: &T, n: &RemoteNode) -> Result<Option<(Sid, String)>, T::Error> {
    let Some(p) = &n.parent else { return Ok(None) };
    Ok(t.base_by_remote(p)?.filter(|b| b.kind == NodeKind::Folder).map(|b| (b.sid, n.name.clone())))
}

/// The local folder linked to Synced folder `sid`, if it exists now.
fn local_dir<T: Trees + ?Sized>(t: &T, sid: Sid) -> Result<Option<LocalId>, T::Error> {
    let Some(b) = t.base(sid)? else { return Ok(None) };
    Ok(local_of(t, &b)?.present().filter(|n| n.kind == NodeKind::Folder).map(|n| n.id))
}

fn remote_dir<T: Trees + ?Sized>(t: &T, sid: Sid) -> Result<Option<RemoteId>, T::Error> {
    let Some(b) = t.base(sid)? else { return Ok(None) };
    Ok(remote_of(t, &b)?.present().filter(|n| n.kind == NodeKind::Folder).map(|n| n.id.clone()))
}

/// Whether local folder `dir` is `item` or inside it (moving `item` there
/// would create a cycle). Walks parents; depth is bounded by the tree.
fn local_within<T: Trees + ?Sized>(t: &T, dir: LocalId, item: LocalId) -> Result<bool, T::Error> {
    let mut cur = Some(dir);
    while let Some(id) = cur {
        if id == item {
            return Ok(true);
        }
        cur = t.local(id)?.present().and_then(|n| n.parent);
    }
    Ok(false)
}

fn remote_within<T: Trees + ?Sized>(t: &T, dir: &RemoteId, item: &RemoteId) -> Result<bool, T::Error> {
    let mut cur = Some(dir.clone());
    while let Some(id) = cur {
        if &id == item {
            return Ok(true);
        }
        cur = t.remote(&id)?.present().and_then(|n| n.parent.clone());
    }
    Ok(false)
}

/// Suffix of the temporary names given to moves whose destination is being
/// vacated in the same round ([`crate::resolve`]).
pub(crate) const STAGING: &str = ".oneup-staging-";

/// The real name behind a staging name, if `name` is one.
fn unstaged(name: &str) -> Option<&str> {
    let i = name.rfind(STAGING)?;
    let n = &name[i + STAGING.len()..];
    (i > 0 && !n.is_empty() && n.bytes().all(|b| b.is_ascii_digit())).then(|| &name[..i])
}

/// `name (conflict N).ext`, with N chosen so the folded result isn't in
/// `taken` (which it then joins).
pub(crate) fn conflict_name(name: &str, kind: NodeKind, taken: &mut BTreeSet<String>) -> String {
    let (stem, ext) = match (kind, name.rfind('.')) {
        // A leading dot (".gitignore") is part of the stem, not an extension.
        (NodeKind::File, Some(i)) if i > 0 => name.split_at(i),
        _ => (name, ""),
    };
    let mut n = 1u32;
    loop {
        let candidate = format!("{stem} (conflict {n}){ext}");
        if taken.insert(fold(&candidate)) {
            return candidate;
        }
        n += 1;
    }
}

struct Ctx<'t, T: Trees + ?Sized> {
    t: &'t T,
    /// One-way (backup): only this PC's changes travel ([`Flow::Backup`]).
    backup: bool,
    folder: Base,
    /// This folder on the local side, if it exists there now.
    local_dir: Option<LocalId>,
    /// Folded names present on either side, plus conflict names handed out.
    taken: BTreeSet<String>,
    out: Vec<Op>,
}

/// Decides every child of Synced folder `sid`. Returns no operations if the
/// row is gone or isn't a folder.
pub(crate) fn plan_folder<T: Trees + ?Sized>(t: &T, sid: Sid, flow: Flow) -> Result<Vec<Op>, T::Error> {
    let Some(folder) = t.base(sid)? else { return Ok(Vec::new()) };
    if folder.kind != NodeKind::Folder {
        return Ok(Vec::new());
    }
    let local = local_of(t, &folder)?;
    let remote = remote_of(t, &folder)?;
    let local_dir = local.present().filter(|n| n.kind == NodeKind::Folder).map(|n| n.id);
    let remote_dir = remote.present().filter(|n| n.kind == NodeKind::Folder).map(|n| n.id.clone());
    // Sorted by ID so the plan doesn't depend on the order a backend returns
    // rows in: SQLite and the simulation must plan identically (the
    // simulation checks this), and conflict names are handed out in order.
    let mut local_kids = match local_dir {
        Some(id) => t.local_children(id)?,
        None => Vec::new(),
    };
    local_kids.sort_by_key(|n| n.id);
    let mut remote_kids = match &remote_dir {
        Some(id) => t.remote_children(id)?,
        None => Vec::new(),
    };
    remote_kids.sort_by(|a, b| a.id.cmp(&b.id));
    let taken = local_kids.iter().map(|n| fold(&n.name)).chain(remote_kids.iter().map(|n| fold(&n.name))).collect();
    let mut cx = Ctx { t, backup: flow == Flow::Backup, folder, local_dir, taken, out: Vec::new() };

    let mut new_local = Vec::new();
    for n in local_kids {
        if t.base_by_local(n.id)?.is_none() {
            new_local.push(n);
        }
    }
    let mut new_remote = Vec::new();
    for n in remote_kids {
        if t.base_by_remote(&n.id)?.is_none() {
            new_remote.push(n);
        }
    }

    // Atomic saves: the editor wrote a temp file and renamed it over the
    // original, so the original's file ID was deleted and a new one sits at
    // its name. That's an edit of the Synced file, not delete + create: the
    // cloud item keeps its ID and version history.
    let mut base_kids = t.base_children(sid)?;
    base_kids.sort_by_key(|b| b.sid);
    let mut adopted: HashMap<Sid, LocalNode> = HashMap::new();
    for b in &base_kids {
        if b.kind == NodeKind::File && matches!(local_of(t, b)?, Seen::Deleted) {
            let key = fold(&b.name);
            if let Some(i) = new_local.iter().position(|n| n.kind == NodeKind::File && fold(&n.name) == key) {
                adopted.insert(b.sid, new_local.remove(i));
            }
        }
    }

    for b in base_kids {
        let adopted = adopted.remove(&b.sid);
        decide_synced(&mut cx, b, adopted)?;
    }
    decide_new(&mut cx, new_local, new_remote)?;
    Ok(cx.out)
}

fn hold(b: &Base, reason: HoldReason) -> Op {
    Op::Hold { sid: Some(b.sid), local: b.local, remote: b.remote.clone(), reason }
}

fn decide_synced<T: Trees + ?Sized>(cx: &mut Ctx<'_, T>, b: Base, adopted: Option<LocalNode>) -> Result<(), T::Error> {
    let t = cx.t;
    let l = match adopted {
        Some(n) => Seen::Present(n),
        None => local_of(t, &b)?,
    };
    let r = remote_of(t, &b)?;
    match (l, r) {
        (Seen::Present(l), Seen::Present(r)) => both(cx, &b, l, r),
        (Seen::Deleted, Seen::Present(r)) => local_gone(cx, &b, r),
        (Seen::Present(l), Seen::Deleted) => remote_gone(cx, &b, l),
        (Seen::Present(_), Seen::Missing) | (Seen::Missing, Seen::Present(_)) => {
            cx.out.push(hold(&b, HoldReason::MissingWithoutEvidence));
            Ok(())
        }
        // Gone from both sides: nothing left to act on, only the record. A
        // folder's record goes last, once its children's records are gone.
        _ => {
            if b.kind == NodeKind::File || t.base_children(b.sid)?.is_empty() {
                cx.out.push(Op::Forget(b.sid));
            }
            Ok(())
        }
    }
}

/// Present on both sides.
fn both<T: Trees + ?Sized>(cx: &mut Ctx<'_, T>, b: &Base, l: LocalNode, r: RemoteNode) -> Result<(), T::Error> {
    let t = cx.t;
    let file = b.kind == NodeKind::File && l.kind == NodeKind::File && r.kind == NodeKind::File;
    let lc = if file { local_change(b, &l) } else { LocalChange::Same };
    let rc = file && remote_changed(b, &r);

    // Both changed content: identical edits are just recorded; otherwise
    // keep both (never last-writer-wins, `data-safety.md` invariant 9).
    if lc == LocalChange::Changed && rc {
        match l.hash {
            None => cx.out.push(Op::Hash(lpre(&l))),
            Some(h) if r.hash == Some(h) => cx.out.push(Op::Record(Base {
                local: Some(l.id),
                local_mtime: l.mtime,
                size: r.size,
                hash: r.hash,
                remote_etag: r.etag.clone(),
                ..b.clone()
            })),
            // One-way: this PC's version wins; the cloud's goes into its
            // version history.
            Some(_) if cx.backup => upload_over(cx, b, &l, &r)?,
            Some(_) => {
                let Some(dir) = l.parent else { return Ok(()) };
                let name = conflict_name(&l.name, NodeKind::File, &mut cx.taken);
                cx.out.push(Op::ConflictCopy { sid: b.sid, local: lpre(&l), dir, name });
            }
        }
        return Ok(());
    }

    // Location first; content (if also changed) next round, so each
    // operation's preconditions describe what the planner saw.
    let here = b.parent.map(|p| (p, b.name.clone()));
    let lloc = local_loc(t, &l)?;
    let rloc = remote_loc(t, &r)?;
    let lm = lloc != here;
    let rm = rloc != here;
    if cx.backup && (lm || rm) {
        // One-way: the cloud follows this PC's layout, whoever moved it.
        if lm && rm && lloc == rloc {
            let Some((parent, _)) = lloc else { return Ok(()) };
            cx.out.push(Op::Record(Base {
                parent: Some(parent),
                name: l.name.clone(),
                remote_etag: r.etag.clone(),
                ..b.clone()
            }));
            return Ok(());
        }
        if let Some((psid, _)) = &lloc
            && b.kind == NodeKind::Folder
            && let Some(to) = remote_dir(t, *psid)?
            && remote_within(t, &to, &r.id)?
        {
            // The cloud's layout would put the folder inside itself; wait
            // until this PC's moves settle it.
            return Ok(());
        }
        move_remote_to(cx, b, &r, lloc)?;
        return Ok(());
    }
    if lm && rm && lloc == rloc {
        // Both moved to the same place. (Both "unknown" — moved into folders
        // not synced yet — is not the same place: wait for those folders.)
        let Some((parent, _)) = lloc else { return Ok(()) };
        cx.out.push(Op::Record(Base {
            parent: Some(parent),
            name: l.name.clone(),
            remote_etag: r.etag.clone(),
            ..b.clone()
        }));
        return Ok(());
    }
    if rm {
        // The cloud moved it (and if both moved, the cloud's location wins:
        // that's a move, not a loss).
        let Some((psid, name)) = rloc else { return Ok(()) };
        let Some(to) = cx.local_dir_for(psid)? else { return Ok(()) };
        if b.kind == NodeKind::Folder && local_within(t, to, l.id)? {
            // Moved into each other on the two sides: following the cloud
            // would put the folder inside itself here, so undo the cloud's
            // move toward the local layout instead.
            move_remote_to(cx, b, &r, lloc)?;
        } else {
            cx.out.push(Op::MoveLocal { sid: b.sid, local: lpre(&l), to, parent: psid, name });
        }
        return Ok(());
    }
    if lm {
        let Some((psid, _)) = &lloc else { return Ok(()) };
        if b.kind == NodeKind::Folder
            && let Some(to) = remote_dir(t, *psid)?
            && remote_within(t, &to, &r.id)?
        {
            // Mirror case of the above: undo the local move, back to the
            // agreed location (where the cloud still has it).
            if let Some((psid, name)) = here
                && let Some(to) = cx.local_dir_for(psid)?
            {
                cx.out.push(Op::MoveLocal { sid: b.sid, local: lpre(&l), to, parent: psid, name });
            }
            return Ok(());
        }
        move_remote_to(cx, b, &r, lloc)?;
        return Ok(());
    }

    // Both sides settled on a staging name (its move's other half never
    // happened: the item was deleted on that side and then re-created from
    // this one, or a crash interrupted the round). Restore the real name
    // locally; the next round propagates it like any rename.
    if let Some(real) = unstaged(&b.name)
        && let Some(dir) = l.parent
        && !cx.backup
    {
        cx.out.push(Op::RenameLocal { local: lpre(&l), dir, name: real.to_owned() });
        return Ok(());
    }

    // One-way: a file changed in the cloud gets this PC's version back (the
    // cloud's stays in OneDrive's version history).
    if cx.backup && rc {
        return upload_over(cx, b, &l, &r);
    }
    match (lc, rc) {
        (LocalChange::Changed, _) => {
            if let Some((psid, _)) = here
                && let Some(to) = cx.remote_dir_for(psid)?
            {
                cx.out.push(Op::Upload {
                    local: lpre(&l),
                    to,
                    parent: psid,
                    name: b.name.clone(),
                    replace: Some(rpre(&r)),
                    link: Link::Existing(b.sid),
                });
            }
        }
        (_, true) => {
            if let Some((psid, _)) = here
                && let Some(to) = cx.local_dir_for(psid)?
            {
                cx.out.push(Op::Download {
                    remote: rpre(&r),
                    to,
                    parent: psid,
                    name: b.name.clone(),
                    replace: Some(lpre(&l)),
                    link: Link::Existing(b.sid),
                });
            }
        }
        (LocalChange::Touched, false) => {
            cx.out.push(Op::Record(Base { local: Some(l.id), local_mtime: l.mtime, ..b.clone() }));
        }
        (LocalChange::Same, false) => {}
    }
    Ok(())
}

/// One-way: sends this PC's version of a synced file over the cloud's.
fn upload_over<T: Trees + ?Sized>(
    cx: &mut Ctx<'_, T>,
    b: &Base,
    l: &LocalNode,
    r: &RemoteNode,
) -> Result<(), T::Error> {
    if let Some(psid) = b.parent
        && let Some(to) = cx.remote_dir_for(psid)?
    {
        cx.out.push(Op::Upload {
            local: lpre(l),
            to,
            parent: psid,
            name: b.name.clone(),
            replace: Some(rpre(r)),
            link: Link::Existing(b.sid),
        });
    }
    Ok(())
}

/// Moves the cloud item to agreed location `loc` (once that folder exists
/// in the cloud).
fn move_remote_to<T: Trees + ?Sized>(
    cx: &mut Ctx<'_, T>,
    b: &Base,
    r: &RemoteNode,
    loc: Option<(Sid, String)>,
) -> Result<(), T::Error> {
    let Some((psid, name)) = loc else { return Ok(()) };
    let Some(to) = cx.remote_dir_for(psid)? else { return Ok(()) };
    cx.out.push(Op::MoveRemote { sid: b.sid, remote: rpre(r), to, parent: psid, name });
    Ok(())
}

/// Deleted locally (with evidence), present in the cloud.
fn local_gone<T: Trees + ?Sized>(cx: &mut Ctx<'_, T>, b: &Base, r: RemoteNode) -> Result<(), T::Error> {
    if cx.backup {
        // One-way: deletions on this PC never reach the cloud.
        return Ok(());
    }
    let t = cx.t;
    let here = b.parent.map(|p| (p, b.name.clone()));
    let rloc = remote_loc(t, &r)?;
    let rc = b.kind == NodeKind::File && remote_changed(b, &r);
    if rc || rloc != here {
        // Changed or moved in the cloud since: that change wins over the
        // deletion. Re-create it here at the cloud's location.
        let Some((psid, name)) = rloc else { return Ok(()) };
        let Some(to) = cx.local_dir_for(psid)? else { return Ok(()) };
        let link = Link::Existing(b.sid);
        cx.out.push(match b.kind {
            NodeKind::File => Op::Download { remote: rpre(&r), to, parent: psid, name, replace: None, link },
            NodeKind::Folder => Op::MkdirLocal { remote: rpre(&r), to, parent: psid, name, link },
        });
        return Ok(());
    }
    // Deleting a cloud folder deletes everything in it, including anything
    // we haven't seen yet, so only empty folders are deleted: children go
    // first, each under its own preconditions. Their Synced rows must be
    // settled first too: deleting the folder drops its row, and a child row
    // left pointing at it would never be planned again.
    if b.kind == NodeKind::File || (t.remote_children(&r.id)?.is_empty() && t.base_children(b.sid)?.is_empty()) {
        cx.out.push(Op::DeleteRemote { sid: b.sid, remote: rpre(&r) });
    }
    Ok(())
}

/// Deleted in the cloud (with evidence), present locally.
fn remote_gone<T: Trees + ?Sized>(cx: &mut Ctx<'_, T>, b: &Base, l: LocalNode) -> Result<(), T::Error> {
    let t = cx.t;
    let here = b.parent.map(|p| (p, b.name.clone()));
    let lloc = local_loc(t, &l)?;
    // "Touched" counts as unchanged: the hash proved the content is what
    // was synced, so the cloud's recycle bin and version history hold it.
    let lc = b.kind == NodeKind::File && local_change(b, &l) == LocalChange::Changed;
    // One-way: what this PC has, the cloud gets back.
    if lc || lloc != here || cx.backup {
        let Some((psid, name)) = lloc else { return Ok(()) };
        let Some(to) = cx.remote_dir_for(psid)? else { return Ok(()) };
        let link = Link::Existing(b.sid);
        cx.out.push(match b.kind {
            NodeKind::File => Op::Upload { local: lpre(&l), to, parent: psid, name, replace: None, link },
            NodeKind::Folder => Op::MkdirRemote { local: lpre(&l), to, parent: psid, name, link },
        });
        return Ok(());
    }
    if b.kind == NodeKind::File || (t.local_children(l.id)?.is_empty() && t.base_children(b.sid)?.is_empty()) {
        cx.out.push(Op::RecycleLocal { sid: b.sid, local: lpre(&l) });
    }
    Ok(())
}

impl<T: Trees + ?Sized> Ctx<'_, T> {
    /// The local folder for agreed folder `psid`, as a destination. If it was
    /// deleted locally, something must now go into it, so it is re-created
    /// first ("resurrected", `sync-engine.md` § Folders) and the caller
    /// waits a round.
    fn local_dir_for(&mut self, psid: Sid) -> Result<Option<LocalId>, T::Error> {
        let t = self.t;
        if let Some(d) = local_dir(t, psid)? {
            return Ok(Some(d));
        }
        if self.backup {
            return Ok(None);
        }
        if let Some(pb) = t.base(psid)?
            && pb.sid != ROOT
            && matches!(local_of(t, &pb)?, Seen::Deleted)
            && let Seen::Present(pr) = remote_of(t, &pb)?
        {
            self.out.extend(resurrect_local(t, pb.sid, &pr)?);
        }
        Ok(None)
    }

    /// Mirror image of [`Self::local_dir_for`].
    fn remote_dir_for(&mut self, psid: Sid) -> Result<Option<RemoteId>, T::Error> {
        let t = self.t;
        if let Some(d) = remote_dir(t, psid)? {
            return Ok(Some(d));
        }
        if let Some(pb) = t.base(psid)?
            && pb.sid != ROOT
            && matches!(remote_of(t, &pb)?, Seen::Deleted)
            && let Seen::Present(pl) = local_of(t, &pb)?
        {
            self.out.extend(resurrect_remote(t, pb.sid, &pl)?);
        }
        Ok(None)
    }
}

/// Items without a Synced row, paired by folded name.
fn decide_new<T: Trees + ?Sized>(
    cx: &mut Ctx<'_, T>,
    new_local: Vec<LocalNode>,
    new_remote: Vec<RemoteNode>,
) -> Result<(), T::Error> {
    let mut groups: BTreeMap<String, (Vec<LocalNode>, Vec<RemoteNode>)> = BTreeMap::new();
    for n in new_local {
        groups.entry(fold(&n.name)).or_default().0.push(n);
    }
    for n in new_remote {
        groups.entry(fold(&n.name)).or_default().1.push(n);
    }
    let parent = cx.folder.sid;
    for (_, (mut locals, mut remotes)) in groups {
        if locals.len() > 1 {
            // Case-sensitive folder with names differing only in case.
            for l in locals {
                cx.out.push(Op::Hold { sid: None, local: Some(l.id), remote: None, reason: HoldReason::NameClash });
            }
            continue;
        }
        match (locals.pop(), remotes.pop()) {
            (Some(l), Some(r)) => pair_new(cx, l, r),
            (Some(l), None) => {
                let Some(to) = cx.remote_dir_for(parent)? else { continue };
                let (local, name, link) = (lpre(&l), l.name, Link::New);
                cx.out.push(match l.kind {
                    NodeKind::File => Op::Upload { local, to, parent, name, replace: None, link },
                    NodeKind::Folder => Op::MkdirRemote { local, to, parent, name, link },
                });
            }
            // One-way: what's only in the cloud stays there.
            (None, Some(_)) if cx.backup => {}
            (None, Some(r)) => {
                let Some(to) = cx.local_dir_for(parent)? else { continue };
                let (remote, name, link) = (rpre(&r), r.name, Link::New);
                cx.out.push(match r.kind {
                    NodeKind::File => Op::Download { remote, to, parent, name, replace: None, link },
                    NodeKind::Folder => Op::MkdirLocal { remote, to, parent, name, link },
                });
            }
            (None, None) => {}
        }
    }
    Ok(())
}

/// A new local item and a new cloud item with the same name: the first-sync
/// adoption case, or both sides created the same name independently.
fn pair_new<T: Trees + ?Sized>(cx: &mut Ctx<'_, T>, l: LocalNode, r: RemoteNode) {
    let parent = cx.folder.sid;
    // The Synced row takes the *local* name; if the cloud's differs in case,
    // the next round renames the local item to match the cloud.
    let pair = |kind, size, hash| Op::Pair {
        local: lpre(&l),
        remote: rpre(&r),
        parent,
        name: l.name.clone(),
        kind,
        size,
        hash,
    };
    let same = match (l.kind, r.kind) {
        (NodeKind::Folder, NodeKind::Folder) => {
            cx.out.push(pair(NodeKind::Folder, 0, None));
            return;
        }
        (NodeKind::File, NodeKind::File) if l.size == r.size && r.hash.is_some() => match l.hash {
            None => {
                cx.out.push(Op::Hash(lpre(&l)));
                return;
            }
            Some(h) => r.hash == Some(h),
        },
        _ => false,
    };
    if same {
        cx.out.push(pair(NodeKind::File, r.size, r.hash));
    } else if cx.backup {
        // One-way: this PC's file replaces the cloud's (kept in its version
        // history). A file and a folder can't replace each other: held.
        let to = cx.folder.remote.clone();
        match (l.kind, r.kind, to) {
            (NodeKind::File, NodeKind::File, Some(to)) => cx.out.push(Op::Upload {
                local: lpre(&l),
                to,
                parent,
                name: l.name.clone(),
                replace: Some(rpre(&r)),
                link: Link::New,
            }),
            _ => cx.out.push(Op::Hold {
                sid: None,
                local: Some(l.id),
                remote: Some(r.id.clone()),
                reason: HoldReason::OneWay,
            }),
        }
    } else if let Some(dir) = cx.local_dir {
        // Different content (or file vs folder): keep both. The local one
        // moves aside and is uploaded under its new name next round.
        let name = conflict_name(&l.name, l.kind, &mut cx.taken);
        cx.out.push(Op::RenameLocal { local: lpre(&l), dir, name });
    }
}

/// Re-creates Synced folder `sid` locally (at the cloud's location `r`). If
/// its parent was deleted locally too, the parent goes first: the walk goes
/// up to the highest deleted ancestor, and later rounds work back down. The
/// depth is bounded by the tree's.
fn resurrect_local<T: Trees + ?Sized>(t: &T, sid: Sid, r: &RemoteNode) -> Result<Option<Op>, T::Error> {
    let Some((psid, name)) = remote_loc(t, r)? else { return Ok(None) };
    if let Some(to) = local_dir(t, psid)? {
        return Ok(Some(Op::MkdirLocal { remote: rpre(r), to, parent: psid, name, link: Link::Existing(sid) }));
    }
    let Some(pb) = t.base(psid)? else { return Ok(None) };
    if pb.sid == ROOT || !matches!(local_of(t, &pb)?, Seen::Deleted) {
        return Ok(None);
    }
    match remote_of(t, &pb)? {
        Seen::Present(pr) => resurrect_local(t, pb.sid, &pr),
        _ => Ok(None),
    }
}

/// Mirror image of [`resurrect_local`].
fn resurrect_remote<T: Trees + ?Sized>(t: &T, sid: Sid, l: &LocalNode) -> Result<Option<Op>, T::Error> {
    let Some((psid, name)) = local_loc(t, l)? else { return Ok(None) };
    if let Some(to) = remote_dir(t, psid)? {
        return Ok(Some(Op::MkdirRemote { local: lpre(l), to, parent: psid, name, link: Link::Existing(sid) }));
    }
    let Some(pb) = t.base(psid)? else { return Ok(None) };
    if pb.sid == ROOT || !matches!(remote_of(t, &pb)?, Seen::Deleted) {
        return Ok(None);
    }
    match local_of(t, &pb)? {
        Seen::Present(pl) => resurrect_remote(t, pb.sid, &pl),
        _ => Ok(None),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn conflict_names_keep_the_extension_and_skip_taken_ones() {
        let mut taken: BTreeSet<String> = ["report (conflict 1).docx".to_owned()].into();
        assert_eq!(conflict_name("Report.docx", NodeKind::File, &mut taken), "Report (conflict 2).docx");
        assert_eq!(conflict_name(".gitignore", NodeKind::File, &mut taken), ".gitignore (conflict 1)");
        assert_eq!(
            conflict_name("v1.2", NodeKind::Folder, &mut taken),
            "v1.2 (conflict 1)",
            "folders have no extension"
        );
        assert!(taken.contains("report (conflict 2).docx"), "handed-out names are reserved");
    }

    #[test]
    fn staging_names_are_recognised_exactly() {
        assert_eq!(unstaged("a.txt.oneup-staging-1"), Some("a.txt"));
        assert_eq!(unstaged("a.oneup-staging-12"), Some("a"));
        assert_eq!(unstaged("a.oneup-staging-"), None);
        assert_eq!(unstaged("a.oneup-staging-x"), None);
        assert_eq!(unstaged(".oneup-staging-1"), None, "no real name left");
        assert_eq!(unstaged("plain.txt"), None);
    }
}
