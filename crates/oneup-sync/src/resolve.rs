//! The resolve pass: turns per-folder decisions into a consistent round.
//!
//! 1. **One operation per item per round.** The first decision about an item
//!    (by Synced row, local ID or cloud ID) wins; later ones wait for the
//!    next round, which plans from fresh state.
//! 2. **Free names.** Every operation that needs a free name in some folder
//!    gets one:
//!    * taken by an item that is leaving this round (moved, renamed,
//!      deleted) → use a temporary *staging name*; the next round sees the
//!      difference as a rename and finishes it. This handles swaps and
//!      chains without ordering the operations;
//!    * taken by an item that stays → rename the **local** item involved to
//!      a conflict name instead (a local rename is cheap and loses nothing);
//!      a one-way (backup) pair never changes this PC, so there it's the
//!      **cloud** item in the way that is renamed ([`Op::RenameRemote`]);
//!    * claimed by an earlier operation in this round → staging name.

use std::collections::{BTreeMap, BTreeSet, HashSet};

use crate::merge::{STAGING, conflict_name, lpre};
use crate::model::{LocalId, NodeKind, RemoteId, Trees, fold};
use crate::op::{Dir, Op, Vacates};

/// The current occupants of one folder: folded name → item.
type Occupants = BTreeMap<String, Vacates>;

struct Names<'t, T: Trees + ?Sized> {
    t: &'t T,
    occupants: BTreeMap<Dir, Occupants>,
    claimed: BTreeSet<(Dir, String)>,
}

impl<T: Trees + ?Sized> Names<'_, T> {
    fn occupants(&mut self, dir: &Dir) -> Result<&Occupants, T::Error> {
        if !self.occupants.contains_key(dir) {
            let map = match dir {
                Dir::Local(id) => {
                    self.t.local_children(*id)?.into_iter().map(|n| (fold(&n.name), Vacates::Local(n.id))).collect()
                }
                Dir::Remote(id) => {
                    self.t.remote_children(id)?.into_iter().map(|n| (fold(&n.name), Vacates::Remote(n.id))).collect()
                }
            };
            self.occupants.insert(dir.clone(), map);
        }
        Ok(&self.occupants[dir])
    }

    /// The same folder on the other side, if it is synced: a name chosen for
    /// one side should be free on both, or the next round collides there.
    fn counterpart(&self, dir: &Dir) -> Result<Option<Dir>, T::Error> {
        Ok(match dir {
            Dir::Local(id) => self.t.base_by_local(*id)?.and_then(|b| b.remote).map(Dir::Remote),
            Dir::Remote(id) => self.t.base_by_remote(id)?.and_then(|b| b.local).map(Dir::Local),
        })
    }

    /// Folded names that are unavailable in `dir`: occupied on either side
    /// or already claimed this round.
    fn unavailable(&mut self, dir: &Dir) -> Result<BTreeSet<String>, T::Error> {
        let mut taken: BTreeSet<String> = self.occupants(dir)?.keys().cloned().collect();
        if let Some(other) = self.counterpart(dir)? {
            taken.extend(self.occupants(&other)?.keys().cloned());
        }
        taken.extend(self.claimed.iter().filter(|(d, _)| d == dir).map(|(_, n)| n.clone()));
        Ok(taken)
    }

    fn claim(&mut self, dir: Dir, name: &str) {
        self.claimed.insert((dir, fold(name)));
    }

    fn staging_name(&mut self, dir: &Dir, name: &str) -> Result<String, T::Error> {
        let taken = self.unavailable(dir)?;
        let mut n = 1u32;
        loop {
            let candidate = format!("{name}{STAGING}{n}");
            if !taken.contains(&fold(&candidate)) {
                return Ok(candidate);
            }
            n += 1;
        }
    }

    fn conflict_name(&mut self, dir: &Dir, name: &str, kind: NodeKind) -> Result<String, T::Error> {
        let mut taken = self.unavailable(dir)?;
        Ok(conflict_name(name, kind, &mut taken))
    }
}

pub(crate) fn resolve<T: Trees + ?Sized>(t: &T, ops: Vec<Op>, backup: bool) -> Result<Vec<Op>, T::Error> {
    // 1. One operation per item.
    let mut sids = HashSet::new();
    let mut locals: HashSet<LocalId> = HashSet::new();
    let mut remotes: HashSet<RemoteId> = HashSet::new();
    let mut kept = Vec::with_capacity(ops.len());
    for op in ops {
        let (s, l, r) = op.subjects();
        let busy = s.is_some_and(|s| sids.contains(&s))
            || l.is_some_and(|l| locals.contains(&l))
            || r.as_ref().is_some_and(|r| remotes.contains(r));
        if busy {
            continue;
        }
        sids.extend(s);
        locals.extend(l);
        remotes.extend(r);
        kept.push(op);
    }

    // 2. Free names.
    let leaving: HashSet<Vacates> = kept.iter().filter_map(Op::vacates).collect();
    let mut names = Names { t, occupants: BTreeMap::new(), claimed: BTreeSet::new() };
    let mut out = Vec::with_capacity(kept.len());
    for mut op in kept {
        let Some((dir, name)) = op.target().map(|(d, n)| (d, n.to_owned())) else {
            out.push(op);
            continue;
        };
        let key = fold(&name);
        let mover = op.mover();
        let occupant = names.occupants(&dir)?.get(&key).cloned().filter(|o| Some(o) != mover.as_ref());
        let claimed = names.claimed.contains(&(dir.clone(), key));

        match occupant {
            None if !claimed => {
                names.claim(dir, &name);
                out.push(op);
            }
            Some(o) if !leaving.contains(&o) && !claimed => {
                // Taken by an item that stays.
                let busy = Busy { locals: &locals, remotes: &remotes, backup };
                if let Some(op) = blocked(&mut names, op, &dir, &name, o, &busy)? {
                    out.push(op);
                }
            }
            _ if mover.is_none() => {
                // A new item whose name is being vacated (or claimed) this
                // round: it waits a round. Nothing waits for a new item, so
                // this can't deadlock, and no temporary name can outlive it.
            }
            _ => {
                // A move into a name being vacated (or claimed) this round.
                // Waiting could deadlock (swaps, rotations), so it moves to a
                // staging name now and finishes next round.
                let staged = names.staging_name(&dir, &name)?;
                if let Some(n) = op.target_name_mut() {
                    n.clone_from(&staged);
                }
                names.claim(dir, &staged);
                out.push(op);
            }
        }
    }
    Ok(out)
}

/// Items with an operation this round, and the pair's flow.
struct Busy<'a> {
    locals: &'a HashSet<LocalId>,
    remotes: &'a HashSet<RemoteId>,
    backup: bool,
}

/// `op` needs `name` in `dir`, where `occupant` stays. Returns what to do
/// instead (if anything) this round.
fn blocked<T: Trees + ?Sized>(
    names: &mut Names<'_, T>,
    op: Op,
    dir: &Dir,
    name: &str,
    occupant: Vacates,
    busy: &Busy<'_>,
) -> Result<Option<Op>, T::Error> {
    let t = names.t;
    let busy_locals = busy.locals;
    match (&op, occupant) {
        (Op::RenameRemote { remote, .. }, _) => {
            // Our own rename-aside name collided: pick another.
            let kind = t.remote(&remote.id)?.present().map_or(NodeKind::File, |n| n.kind);
            let fresh = names.conflict_name(dir, name, kind)?;
            let mut op = op;
            if let Some(n) = op.target_name_mut() {
                n.clone_from(&fresh);
            }
            names.claim(dir.clone(), &fresh);
            Ok(Some(op))
        }
        // One-way: this PC's item needs a cloud name a cloud item that
        // stays has. This PC never changes, so the cloud item moves aside.
        (_, Vacates::Remote(z)) if busy.backup => {
            if busy.remotes.contains(&z) {
                return Ok(None);
            }
            let Some(zn) = t.remote(&z)?.present().cloned() else { return Ok(None) };
            let Dir::Remote(d) = dir else { return Ok(None) };
            let fresh = names.conflict_name(dir, &zn.name, zn.kind)?;
            names.claim(dir.clone(), &fresh);
            Ok(Some(Op::RenameRemote { remote: crate::merge::rpre(&zn), dir: d.clone(), name: fresh }))
        }
        // Our own conflict/rename name collided: pick another.
        (Op::RenameLocal { local, .. } | Op::ConflictCopy { local, .. }, _) => {
            let kind = if local.stamp.is_some() { NodeKind::File } else { NodeKind::Folder };
            let fresh = names.conflict_name(dir, name, kind)?;
            let mut op = op;
            if let Some(n) = op.target_name_mut() {
                n.clone_from(&fresh);
            }
            names.claim(dir.clone(), &fresh);
            Ok(Some(op))
        }
        // A remote-originated item needs a local name that a staying local
        // item has: move the local one aside, unless it's busy this round.
        (_, Vacates::Local(z)) => {
            if busy_locals.contains(&z) {
                return Ok(None);
            }
            let Some(zn) = t.local(z)?.present().cloned() else { return Ok(None) };
            let fresh = names.conflict_name(dir, &zn.name, zn.kind)?;
            names.claim(dir.clone(), &fresh);
            let Dir::Local(d) = dir else { return Ok(None) };
            Ok(Some(Op::RenameLocal { local: lpre(&zn), dir: *d, name: fresh }))
        }
        // A local-originated item needs a cloud name a staying cloud item
        // has: rename the local item; the next round propagates that name.
        (_, Vacates::Remote(_)) => {
            let local_id = match &op {
                Op::Upload { local, .. } | Op::MkdirRemote { local, .. } => Some(local.id),
                Op::MoveRemote { sid, .. } => t.base(*sid)?.and_then(|b| b.local),
                _ => None,
            };
            let Some(ln) = (match local_id {
                Some(id) => t.local(id)?.present().cloned(),
                None => None,
            }) else {
                return Ok(None);
            };
            let Some(ldir) = ln.parent else { return Ok(None) };
            let ldir = Dir::Local(ldir);
            let fresh = names.conflict_name(&ldir, &ln.name, ln.kind)?;
            names.claim(ldir.clone(), &fresh);
            let Dir::Local(d) = ldir else { return Ok(None) };
            Ok(Some(Op::RenameLocal { local: lpre(&ln), dir: d, name: fresh }))
        }
    }
}
