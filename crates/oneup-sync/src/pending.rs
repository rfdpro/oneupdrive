//! Folder deletions still to come.
//!
//! A folder is deleted only once it's empty (`merge::local_gone`,
//! `merge::remote_gone`), so deleting a tree takes a round per level: its
//! files first, then the deepest folders, and so on up. The safety gate
//! judges each round, so a large tree could cost a safety stop per level.
//! [`pending_deletes`] names, from one round's plan, the folders that plan
//! empties and that the planner will delete once it has: the gate lists
//! them with the round's batch, and one approval covers the whole tree.

use std::collections::{HashMap, HashSet, VecDeque};

use crate::merge::{local_loc, lpre, remote_loc, rpre};
use crate::model::{NodeKind, ROOT, Seen, Sid, Trees, local_of, remote_of};
use crate::{Flow, Op};

/// The folder deletions that follow `ops` (a round's plan over `t`): every
/// folder above one of its deletions that is deleted on the same side with
/// explicit evidence (`data-safety.md` invariant 3), is still where it was
/// agreed on the other side, and holds nothing but what `ops` and these
/// deletions remove. Each is the operation the planner makes for it once
/// it's empty (the cloud folder's eTag will have changed by then). A folder
/// without evidence, one that keeps anything, or one above such a folder is
/// never named. Nothing for a one-way pair, which never deletes in the
/// cloud and never changes this PC.
///
/// `t` is the view the round planned over (what is deleted, and where);
/// `all` is the same trees unfiltered, for what a folder holds: an item the
/// view hides (excluded by the rules, or never synced by nature) is still
/// in the folder, which then isn't emptied.
///
/// Cost: the folders above the plan's deletions and their children, which
/// is the size of what is being deleted.
pub fn pending_deletes<T, A>(t: &T, all: &A, ops: &[Op], flow: Flow) -> Result<Vec<Op>, T::Error>
where
    T: Trees + ?Sized,
    A: Trees<Error = T::Error> + ?Sized,
{
    if flow == Flow::Backup {
        return Ok(Vec::new());
    }
    // Items gone from both sides leave their folder too (`Op::Forget`).
    let forgotten: HashSet<Sid> =
        ops.iter().filter_map(|o| if let Op::Forget(s) = o { Some(*s) } else { None }).collect();
    let mut out = Vec::new();
    for side in [Gone::Cloud, Gone::Here] {
        let gone: HashSet<Sid> = ops.iter().filter_map(|o| side.deletes(o)).collect();
        if !gone.is_empty() {
            out.extend(side.folders(t, all, &gone, &forgotten)?);
        }
    }
    Ok(out)
}

/// The side the deletions happen on.
#[derive(Clone, Copy, PartialEq, Eq)]
enum Gone {
    /// Deleted here, so deleted in OneDrive (`Op::DeleteRemote`).
    Cloud,
    /// Deleted in OneDrive, so recycled here (`Op::RecycleLocal`).
    Here,
}

/// A folder that may follow, with its children as Synced items.
struct Candidate {
    op: Op,
    parent: Option<Sid>,
    /// Its Synced children, and the Synced items its present children on
    /// the deleting side are linked to.
    kids: HashSet<Sid>,
    /// It has a child there that isn't synced (something new), so it's
    /// never emptied.
    keeps: bool,
}

impl Gone {
    fn deletes(self, op: &Op) -> Option<Sid> {
        match (self, op) {
            (Gone::Cloud, Op::DeleteRemote { sid, .. }) | (Gone::Here, Op::RecycleLocal { sid, .. }) => Some(*sid),
            _ => None,
        }
    }

    fn folders<T, A>(self, t: &T, all: &A, gone: &HashSet<Sid>, forgotten: &HashSet<Sid>) -> Result<Vec<Op>, T::Error>
    where
        T: Trees + ?Sized,
        A: Trees<Error = T::Error> + ?Sized,
    {
        // The folders above the deletions, each looked at once, stopping at
        // the first that won't be deleted (nothing above it can empty).
        let mut seen: HashSet<Sid> = gone.clone();
        let mut order: Vec<Sid> = Vec::new();
        let mut found: HashMap<Sid, Candidate> = HashMap::new();
        let mut starts: Vec<Sid> = gone.iter().copied().collect();
        starts.sort_unstable();
        for start in starts {
            let mut at = t.base(start)?.and_then(|b| b.parent);
            while let Some(sid) = at {
                if sid == ROOT || !seen.insert(sid) {
                    break;
                }
                let Some(c) = self.candidate(t, all, sid)? else { break };
                order.push(sid);
                found.insert(sid, c);
                at = t.base(sid)?.and_then(|b| b.parent);
            }
        }
        // Bottom up: a folder follows once all it holds goes, folders that
        // follow included. Each counts what of it isn't known to go yet.
        let leaving: HashSet<Sid> = gone.union(forgotten).copied().collect();
        let mut left: HashMap<Sid, usize> = HashMap::new();
        let mut ready: VecDeque<Sid> = VecDeque::new();
        for sid in &order {
            let c = &found[sid];
            if c.keeps {
                continue;
            }
            let n = c.kids.iter().filter(|k| !leaving.contains(k)).count();
            left.insert(*sid, n);
            if n == 0 {
                ready.push_back(*sid);
            }
        }
        let mut out: Vec<Op> = Vec::new();
        while let Some(sid) = ready.pop_front() {
            let c = &found[&sid];
            out.push(c.op.clone());
            if let Some(p) = c.parent
                && let Some(n) = left.get_mut(&p)
            {
                *n -= 1;
                if *n == 0 {
                    ready.push_back(p);
                }
            }
        }
        Ok(out)
    }

    /// Synced folder `sid`, if it's deleted on this side with evidence and
    /// still where it was agreed on the other: the deletion the planner
    /// makes for it once empty, and what it holds.
    fn candidate<T, A>(self, t: &T, all: &A, sid: Sid) -> Result<Option<Candidate>, T::Error>
    where
        T: Trees + ?Sized,
        A: Trees<Error = T::Error> + ?Sized,
    {
        let Some(b) = t.base(sid)? else { return Ok(None) };
        if b.kind != NodeKind::Folder {
            return Ok(None);
        }
        let here = b.parent.map(|p| (p, b.name.clone()));
        // Evidence first: a deletion inside a folder that's still there
        // costs three lookups, not a listing.
        let mut kids: HashSet<Sid> = HashSet::new();
        let mut keeps = false;
        let op = match self {
            Gone::Cloud => {
                let (Seen::Deleted, Seen::Present(r)) = (local_of(t, &b)?, remote_of(t, &b)?) else { return Ok(None) };
                if r.kind != NodeKind::Folder || remote_loc(t, &r)? != here {
                    return Ok(None);
                }
                for c in all.remote_children(&r.id)? {
                    match all.base_by_remote(&c.id)? {
                        Some(cb) => {
                            kids.insert(cb.sid);
                        }
                        None => keeps = true,
                    }
                }
                Op::DeleteRemote { sid, remote: rpre(&r) }
            }
            Gone::Here => {
                let (Seen::Present(l), Seen::Deleted) = (local_of(t, &b)?, remote_of(t, &b)?) else { return Ok(None) };
                if l.kind != NodeKind::Folder || local_loc(t, &l)? != here {
                    return Ok(None);
                }
                for c in all.local_children(l.id)? {
                    match all.base_by_local(c.id)? {
                        Some(cb) => {
                            kids.insert(cb.sid);
                        }
                        None => keeps = true,
                    }
                }
                Op::RecycleLocal { sid, local: lpre(&l) }
            }
        };
        kids.extend(all.base_children(sid)?.into_iter().map(|c| c.sid));
        Ok(Some(Candidate { op, parent: b.parent, kids, keeps }))
    }
}

#[cfg(test)]
mod tests {
    use std::collections::BTreeMap;
    use std::convert::Infallible;

    use super::*;
    use crate::model::{Base, LocalId, LocalNode, RemoteId, RemoteNode};
    use crate::{LocalPre, RemotePre, plan};

    /// The three trees in memory: what is on each side, and what is gone
    /// with evidence (anything else not there is missing without it).
    #[derive(Default)]
    struct Mem {
        bases: BTreeMap<Sid, Base>,
        local: BTreeMap<u128, LocalNode>,
        local_gone: HashSet<u128>,
        remote: BTreeMap<String, RemoteNode>,
        remote_gone: HashSet<String>,
    }

    impl Trees for Mem {
        type Error = Infallible;
        fn base(&self, sid: Sid) -> Result<Option<Base>, Infallible> {
            Ok(self.bases.get(&sid).cloned())
        }
        fn base_children(&self, sid: Sid) -> Result<Vec<Base>, Infallible> {
            Ok(self.bases.values().filter(|b| b.parent == Some(sid)).cloned().collect())
        }
        fn base_by_local(&self, id: LocalId) -> Result<Option<Base>, Infallible> {
            Ok(self.bases.values().find(|b| b.local == Some(id)).cloned())
        }
        fn base_by_remote(&self, id: &RemoteId) -> Result<Option<Base>, Infallible> {
            Ok(self.bases.values().find(|b| b.remote.as_ref() == Some(id)).cloned())
        }
        fn local(&self, id: LocalId) -> Result<Seen<LocalNode>, Infallible> {
            Ok(match self.local.get(&id.0) {
                Some(n) => Seen::Present(n.clone()),
                None if self.local_gone.contains(&id.0) => Seen::Deleted,
                None => Seen::Missing,
            })
        }
        fn remote(&self, id: &RemoteId) -> Result<Seen<RemoteNode>, Infallible> {
            Ok(match self.remote.get(&id.0) {
                Some(n) => Seen::Present(n.clone()),
                None if self.remote_gone.contains(&id.0) => Seen::Deleted,
                None => Seen::Missing,
            })
        }
        fn local_children(&self, id: LocalId) -> Result<Vec<LocalNode>, Infallible> {
            Ok(self.local.values().filter(|n| n.parent == Some(id)).cloned().collect())
        }
        fn remote_children(&self, id: &RemoteId) -> Result<Vec<RemoteNode>, Infallible> {
            Ok(self.remote.values().filter(|n| n.parent.as_ref() == Some(id)).cloned().collect())
        }
    }

    impl Mem {
        /// A synced tree: `items` are (sid, parent sid, name, folder?),
        /// each present on both sides with local ID = sid and cloud ID
        /// `r<sid>`.
        fn synced(items: &[(Sid, Sid, &str, bool)]) -> Self {
            let mut m = Mem::default();
            m.put(ROOT, None, "", true);
            for &(sid, parent, name, folder) in items {
                m.put(sid, Some(parent), name, folder);
            }
            m
        }

        fn put(&mut self, sid: Sid, parent: Option<Sid>, name: &str, folder: bool) {
            let kind = if folder { NodeKind::Folder } else { NodeKind::File };
            let size = if folder { 0 } else { 1 };
            let rid = RemoteId(format!("r{sid}"));
            self.bases.insert(
                sid,
                Base {
                    sid,
                    parent,
                    name: name.into(),
                    kind,
                    size,
                    hash: None,
                    local: Some(LocalId(u128::from(sid))),
                    local_mtime: 1,
                    remote: Some(rid.clone()),
                    remote_etag: "e".into(),
                },
            );
            self.local.insert(
                u128::from(sid),
                LocalNode {
                    id: LocalId(u128::from(sid)),
                    parent: parent.map(|p| LocalId(u128::from(p))),
                    name: name.into(),
                    kind,
                    size,
                    mtime: 1,
                    hash: None,
                },
            );
            self.remote.insert(
                rid.0.clone(),
                RemoteNode {
                    id: rid,
                    parent: parent.map(|p| RemoteId(format!("r{p}"))),
                    name: name.into(),
                    kind,
                    size,
                    hash: None,
                    etag: "e".into(),
                },
            );
        }

        /// Deleted on this PC, with evidence (a journal delete record).
        fn delete_here(&mut self, sids: &[Sid]) {
            for s in sids {
                self.local.remove(&u128::from(*s));
                self.local_gone.insert(u128::from(*s));
            }
        }

        /// Deleted in OneDrive, with evidence (a delta `deleted` facet).
        fn delete_in_cloud(&mut self, sids: &[Sid]) {
            for s in sids {
                self.remote.remove(&format!("r{s}"));
                self.remote_gone.insert(format!("r{s}"));
            }
        }

        fn pending(&self) -> Vec<Op> {
            let ops = plan(self, crate::all_folders(self).unwrap()).unwrap();
            pending_deletes(self, self, &ops, Flow::TwoWay).unwrap()
        }
    }

    fn cloud_folder(sid: Sid) -> Op {
        Op::DeleteRemote { sid, remote: RemotePre { id: RemoteId(format!("r{sid}")), etag: "e".into() } }
    }

    fn local_folder(sid: Sid) -> Op {
        Op::RecycleLocal { sid, local: LocalPre { id: LocalId(u128::from(sid)), stamp: None } }
    }

    /// `a/b/c`, each with a file, beside `keep/k.txt`.
    fn tree() -> Mem {
        Mem::synced(&[
            (1, ROOT, "a", true),
            (2, 1, "b", true),
            (3, 2, "c", true),
            (11, 1, "a.txt", false),
            (12, 2, "b.txt", false),
            (13, 3, "c.txt", false),
            (20, ROOT, "keep", true),
            (21, 20, "k.txt", false),
        ])
    }

    #[test]
    fn a_tree_deleted_here_names_every_level_its_files_empty() {
        let mut m = tree();
        m.delete_here(&[1, 2, 3, 11, 12, 13]);
        let ops = plan(&m, crate::all_folders(&m).unwrap()).unwrap();
        // This round deletes the files only: the folders aren't empty yet.
        let mut deleted: Vec<Sid> =
            ops.iter().filter_map(|o| if let Op::DeleteRemote { sid, .. } = o { Some(*sid) } else { None }).collect();
        deleted.sort_unstable();
        assert_eq!(deleted, [11, 12, 13]);
        assert_eq!(
            pending_deletes(&m, &m, &ops, Flow::TwoWay).unwrap(),
            [cloud_folder(3), cloud_folder(2), cloud_folder(1)]
        );
        assert!(pending_deletes(&m, &m, &ops, Flow::Backup).unwrap().is_empty(), "a backup never deletes in the cloud");
    }

    #[test]
    fn a_tree_deleted_in_onedrive_names_every_level_too() {
        let mut m = tree();
        m.delete_in_cloud(&[1, 2, 3, 11, 12, 13]);
        assert_eq!(m.pending(), [local_folder(3), local_folder(2), local_folder(1)]);
    }

    /// A folder gone without a recorded deletion is held, never deleted:
    /// it's never named, nor is anything above it.
    #[test]
    fn folders_without_evidence_are_never_named() {
        let mut m = tree();
        m.delete_here(&[1, 3, 11, 12, 13]);
        // `b` is gone from this PC too, but nothing recorded its deletion:
        // `c` below it goes, `b` and `a` above it don't.
        m.local.remove(&2);
        assert_eq!(m.pending(), [cloud_folder(3)]);

        // Only the folder itself, with its files' deletions recorded.
        let mut m = tree();
        m.delete_here(&[3, 13]);
        assert_eq!(m.pending(), [cloud_folder(3)]);
    }

    /// A folder that keeps anything isn't emptied, so it isn't named, nor
    /// is anything above it.
    #[test]
    fn folders_that_keep_anything_are_never_named() {
        // A file in `b` without a recorded deletion (held).
        let mut m = tree();
        m.delete_here(&[1, 2, 3, 11, 13]);
        m.local.remove(&12);
        assert_eq!(m.pending(), [cloud_folder(3)]);

        // Something new in the cloud folder `b` (not synced yet).
        let mut m = tree();
        m.delete_here(&[1, 2, 3, 11, 12, 13]);
        m.remote.insert(
            "new".into(),
            RemoteNode {
                id: RemoteId("new".into()),
                parent: Some(RemoteId("r2".into())),
                name: "new.txt".into(),
                kind: NodeKind::File,
                size: 1,
                hash: None,
                etag: "e".into(),
            },
        );
        assert_eq!(m.pending(), [cloud_folder(3)]);

        // `b` moved in the cloud: that move wins over the deletion.
        let mut m = tree();
        m.delete_here(&[1, 2, 3, 11, 12, 13]);
        if let Some(r) = m.remote.get_mut("r2") {
            r.parent = Some(RemoteId("r20".into()));
        }
        assert_eq!(m.pending(), [cloud_folder(3)]);
    }

    /// A view hiding some items, as the planner's (`oneup_plan::Parked`)
    /// hides what the rules exclude and what never syncs by nature: hidden
    /// items read as missing and aren't listed.
    struct Hiding<'a> {
        m: &'a Mem,
        local: HashSet<u128>,
        remote: HashSet<String>,
    }

    impl Trees for Hiding<'_> {
        type Error = Infallible;
        fn base(&self, sid: Sid) -> Result<Option<Base>, Infallible> {
            self.m.base(sid)
        }
        fn base_children(&self, sid: Sid) -> Result<Vec<Base>, Infallible> {
            let hidden = |b: &Base| {
                b.local.is_some_and(|l| self.local.contains(&l.0))
                    || b.remote.as_ref().is_some_and(|r| self.remote.contains(&r.0))
            };
            Ok(self.m.base_children(sid)?.into_iter().filter(|b| !hidden(b)).collect())
        }
        fn base_by_local(&self, id: LocalId) -> Result<Option<Base>, Infallible> {
            self.m.base_by_local(id)
        }
        fn base_by_remote(&self, id: &RemoteId) -> Result<Option<Base>, Infallible> {
            self.m.base_by_remote(id)
        }
        fn local(&self, id: LocalId) -> Result<Seen<LocalNode>, Infallible> {
            Ok(if self.local.contains(&id.0) { Seen::Missing } else { self.m.local(id)? })
        }
        fn remote(&self, id: &RemoteId) -> Result<Seen<RemoteNode>, Infallible> {
            Ok(if self.remote.contains(&id.0) { Seen::Missing } else { self.m.remote(id)? })
        }
        fn local_children(&self, id: LocalId) -> Result<Vec<LocalNode>, Infallible> {
            Ok(self.m.local_children(id)?.into_iter().filter(|n| !self.local.contains(&n.id.0)).collect())
        }
        fn remote_children(&self, id: &RemoteId) -> Result<Vec<RemoteNode>, Infallible> {
            Ok(self.m.remote_children(id)?.into_iter().filter(|n| !self.remote.contains(&n.id.0)).collect())
        }
    }

    /// A folder still holding something the planner's view hides isn't
    /// emptied by the round's deletions: an excluded item (synced before
    /// it was excluded) or one that never syncs by nature (`desktop.ini`,
    /// an Office lock file). Neither is named, nor anything above it.
    #[test]
    fn folders_holding_hidden_items_are_never_named() {
        // Deleted in OneDrive; `b` here still holds `b.log`, excluded since.
        let mut m = tree();
        m.put(14, Some(2), "b.log", false);
        m.delete_in_cloud(&[1, 2, 3, 11, 12, 13]);
        let view = Hiding { m: &m, local: [14].into(), remote: ["r14".to_string()].into() };
        let ops = plan(&view, crate::all_folders(&view).unwrap()).unwrap();
        assert_eq!(pending_deletes(&view, &m, &ops, Flow::TwoWay).unwrap(), [local_folder(3)]);
        assert_eq!(
            pending_deletes(&view, &view, &ops, Flow::TwoWay).unwrap(),
            [local_folder(3), local_folder(2), local_folder(1)],
            "the planner's own view would name them"
        );

        // `b` here holds `desktop.ini`, never synced.
        let mut m = tree();
        m.local.insert(
            99,
            LocalNode {
                id: LocalId(99),
                parent: Some(LocalId(2)),
                name: "desktop.ini".into(),
                kind: NodeKind::File,
                size: 1,
                mtime: 1,
                hash: None,
            },
        );
        m.delete_in_cloud(&[1, 2, 3, 11, 12, 13]);
        let view = Hiding { m: &m, local: [99].into(), remote: HashSet::new() };
        let ops = plan(&view, crate::all_folders(&view).unwrap()).unwrap();
        assert_eq!(pending_deletes(&view, &m, &ops, Flow::TwoWay).unwrap(), [local_folder(3)]);

        // The same in OneDrive: `b` there holds an item hidden here.
        let mut m = tree();
        m.put(14, Some(2), "b.log", false);
        m.delete_here(&[1, 2, 3, 11, 12, 13]);
        let view = Hiding { m: &m, local: HashSet::new(), remote: ["r14".to_string()].into() };
        let ops = plan(&view, crate::all_folders(&view).unwrap()).unwrap();
        assert_eq!(pending_deletes(&view, &m, &ops, Flow::TwoWay).unwrap(), [cloud_folder(3)]);
    }

    /// Run round by round, every folder named is deleted by the planner in
    /// a later round, as named except for its eTag, and nothing else is.
    #[test]
    fn the_named_folders_are_what_the_planner_deletes_later() {
        let mut m = tree();
        m.delete_here(&[1, 2, 3, 11, 12, 13]);
        let named = m.pending();
        let mut later = Vec::new();
        for round in 0..5 {
            let ops = plan(&m, crate::all_folders(&m).unwrap()).unwrap();
            for op in &ops {
                if let Op::DeleteRemote { sid, remote } = op {
                    if round > 0 {
                        later.push(*sid);
                    }
                    m.remote.remove(&remote.id.0);
                    m.bases.remove(sid);
                    // A folder's eTag changes with its contents.
                    for r in m.remote.values_mut() {
                        r.etag = format!("e{round}");
                    }
                }
            }
        }
        let named: Vec<Sid> =
            named.iter().filter_map(|o| if let Op::DeleteRemote { sid, .. } = o { Some(*sid) } else { None }).collect();
        assert_eq!(later, named);
        assert!(m.bases.contains_key(&20) && m.remote.contains_key("r21"), "the rest stays");
    }
}
