//! The operations the reconciler proposes.
//!
//! Each carries the state it was planned against (its *preconditions*). The
//! executor must re-check them immediately before acting and skip the
//! operation if anything differs (`data-safety.md` invariant 5). Remote ones are
//! additionally sent with `If-Match`. A skipped operation is simply planned
//! again, from fresh state, in the next round.

use crate::model::{Base, Hash, LocalId, NodeKind, RemoteId, Sid};

/// A local item must still be exactly this when the operation runs.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct LocalPre {
    pub id: LocalId,
    /// `(size, last-write time)` for files. `None` for folders, whose
    /// last-write time changes whenever their children do.
    pub stamp: Option<(i64, i64)>,
}

/// A cloud item must still have this eTag when the operation runs.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RemotePre {
    pub id: RemoteId,
    pub etag: String,
}

/// How a created or transferred item is recorded in the Synced tree.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Link {
    /// A new Synced row.
    New,
    /// Update (and re-link) an existing row: a replacement, or an item
    /// re-created on the side where it had been deleted.
    Existing(Sid),
}

/// Why an item is left alone and reported.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum HoldReason {
    /// Gone from one side without evidence of deletion. Never acted on.
    MissingWithoutEvidence,
    /// Several local names in one folder differ only in case (possible in
    /// case-sensitive folders); OneDrive can hold only one of them.
    NameClash,
    /// A one-way (backup) pair would have to change this PC to go on, which
    /// it never does ([`crate::Flow::Backup`]): e.g. a file where the cloud
    /// has a folder of the same name.
    OneWay,
}

/// A folder on one side, used to describe where an operation needs a free name.
#[derive(Debug, Clone, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub enum Dir {
    Local(LocalId),
    Remote(RemoteId),
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Op {
    // ------------------------------------------------ bookkeeping (no I/O
    // on user content)
    /// Link two new items that already agree (same folder, name, kind and,
    /// for files, the same hash): the first-sync adoption case.
    Pair {
        local: LocalPre,
        remote: RemotePre,
        parent: Sid,
        name: String,
        kind: NodeKind,
        size: i64,
        hash: Option<Hash>,
    },
    /// Replace a Synced row with what both sides now agree on (e.g. both
    /// made the same change, or a file was only touched).
    Record(Base),
    /// Drop a Synced row whose item is gone from both sides.
    Forget(Sid),
    /// Compute the local file's hash; a decision depends on it.
    Hash(LocalPre),

    // ------------------------------------------------ transfers / creation
    /// Upload a local file into cloud folder `to` as `name` (replacing
    /// `replace`, which keeps its ID and version history, if given).
    Upload {
        local: LocalPre,
        to: RemoteId,
        parent: Sid,
        name: String,
        replace: Option<RemotePre>,
        link: Link,
    },
    /// Download a cloud file into local folder `to` as `name`. A replaced
    /// local file must still be exactly `replace`. Its old content is the
    /// last synced version, so the cloud's version history has it; whether
    /// a local copy is kept too is the gate's call (`OldCopy`).
    Download {
        remote: RemotePre,
        to: LocalId,
        parent: Sid,
        name: String,
        replace: Option<LocalPre>,
        link: Link,
    },
    MkdirRemote {
        local: LocalPre,
        to: RemoteId,
        parent: Sid,
        name: String,
        link: Link,
    },
    MkdirLocal {
        remote: RemotePre,
        to: LocalId,
        parent: Sid,
        name: String,
        link: Link,
    },

    // ------------------------------------------------ moves and renames
    /// Move/rename a cloud item; the Synced row's location becomes
    /// (`parent`, `name`).
    MoveRemote {
        sid: Sid,
        remote: RemotePre,
        to: RemoteId,
        parent: Sid,
        name: String,
    },
    /// Move/rename a local item; the Synced row's location becomes
    /// (`parent`, `name`).
    MoveLocal {
        sid: Sid,
        local: LocalPre,
        to: LocalId,
        parent: Sid,
        name: String,
    },
    /// Rename a local item within its folder `dir` **without** touching the
    /// Synced tree. Used to clear a name another item needs; the next round
    /// sees it as a local rename and propagates it.
    RenameLocal {
        local: LocalPre,
        dir: LocalId,
        name: String,
    },
    /// Rename a cloud item within its folder `dir` **without** touching the
    /// Synced tree: the cloud-side mirror of [`Op::RenameLocal`], for
    /// one-way (backup) pairs, which never change this PC. Clears a name
    /// this PC's item needs, taken by something only the cloud has.
    RenameRemote {
        remote: RemotePre,
        dir: RemoteId,
        name: String,
    },
    /// Keep-both conflict: rename the local copy within `dir` and detach it
    /// from Synced row `sid`. The next round uploads it as a new item and
    /// restores the cloud version under the original name.
    ConflictCopy {
        sid: Sid,
        local: LocalPre,
        dir: LocalId,
        name: String,
    },

    // ------------------------------------------------ deletions (recoverable)
    /// Delete a cloud item (it goes to OneDrive's recycle bin). Folders only
    /// when empty.
    DeleteRemote {
        sid: Sid,
        remote: RemotePre,
    },
    /// Move a local item to the Recycle Bin. Folders only when empty.
    RecycleLocal {
        sid: Sid,
        local: LocalPre,
    },
    /// Free the content of a placeholder the user set to "Free up space"
    /// (Files On-Demand), keeping the placeholder. Never planned by the
    /// reconciler. Only if its content still hashes to `hash` and that is
    /// the cloud's current version: nothing is lost.
    Dehydrate {
        local: LocalPre,
        hash: Hash,
    },
    /// Delete a cloud item the user's rules exclude, with everything in it
    /// (`oneup cloud prune`, or the `prune` setting; never planned by the
    /// reconciler). It goes to OneDrive's recycle bin. `forget`: the Synced
    /// rows at and below it, deepest first, dropped *before* the deletion so
    /// that re-including the item later uploads the local copy instead of
    /// reading the cloud deletion as one to bring to this PC. `items`: how
    /// many items it holds, at least 1 (what the circuit breakers count).
    /// `keep`: the local copy, deepest first, every item of which must still
    /// be here with all its content, or nothing is done: a cloud copy is
    /// only removed once it's here.
    PruneRemote {
        remote: RemotePre,
        forget: Vec<Sid>,
        items: u64,
        keep: Vec<LocalPre>,
    },

    /// Leave alone and report.
    Hold {
        sid: Option<Sid>,
        local: Option<LocalId>,
        remote: Option<RemoteId>,
        reason: HoldReason,
    },
}

impl Op {
    /// The folder and name this operation will occupy, if it needs one free.
    /// (Replacements and deletions don't: they act in place.)
    pub fn target(&self) -> Option<(Dir, &str)> {
        match self {
            Op::Upload { to, name, replace: None, .. } | Op::MkdirRemote { to, name, .. } => {
                Some((Dir::Remote(to.clone()), name))
            }
            Op::MoveRemote { to, name, .. } => Some((Dir::Remote(to.clone()), name)),
            Op::RenameRemote { dir, name, .. } => Some((Dir::Remote(dir.clone()), name)),
            Op::Download { to, name, replace: None, .. } | Op::MkdirLocal { to, name, .. } => {
                Some((Dir::Local(*to), name))
            }
            Op::MoveLocal { to, name, .. } => Some((Dir::Local(*to), name)),
            Op::RenameLocal { dir, name, .. } | Op::ConflictCopy { dir, name, .. } => Some((Dir::Local(*dir), name)),
            _ => None,
        }
    }

    pub(crate) fn target_name_mut(&mut self) -> Option<&mut String> {
        match self {
            Op::Upload { name, replace: None, .. }
            | Op::MkdirRemote { name, .. }
            | Op::MoveRemote { name, .. }
            | Op::RenameRemote { name, .. }
            | Op::Download { name, replace: None, .. }
            | Op::MkdirLocal { name, .. }
            | Op::MoveLocal { name, .. }
            | Op::RenameLocal { name, .. }
            | Op::ConflictCopy { name, .. } => Some(name),
            _ => None,
        }
    }

    /// The item this operation moves out of its current slot on one side,
    /// if any. Another item may take that slot only after it's gone.
    pub fn vacates(&self) -> Option<Vacates> {
        match self {
            Op::MoveLocal { local, .. }
            | Op::RenameLocal { local, .. }
            | Op::ConflictCopy { local, .. }
            | Op::RecycleLocal { local, .. } => Some(Vacates::Local(local.id)),
            Op::MoveRemote { remote, .. }
            | Op::RenameRemote { remote, .. }
            | Op::DeleteRemote { remote, .. }
            | Op::PruneRemote { remote, .. } => Some(Vacates::Remote(remote.id.clone())),
            _ => None,
        }
    }

    /// The item (on the target's side) that the operation itself moves, so a
    /// slot it already occupies isn't counted as taken (e.g. a case-only
    /// rename).
    pub(crate) fn mover(&self) -> Option<Vacates> {
        match self {
            Op::MoveLocal { .. }
            | Op::RenameLocal { .. }
            | Op::ConflictCopy { .. }
            | Op::MoveRemote { .. }
            | Op::RenameRemote { .. } => self.vacates(),
            _ => None,
        }
    }

    /// Everything the operation is about, for "one operation per item per
    /// round".
    pub(crate) fn subjects(&self) -> (Option<Sid>, Option<LocalId>, Option<RemoteId>) {
        let l = |p: &LocalPre| Some(p.id);
        let r = |p: &RemotePre| Some(p.id.clone());
        let link = |k: &Link| match k {
            Link::New => None,
            Link::Existing(s) => Some(*s),
        };
        match self {
            Op::Pair { local, remote, .. } => (None, l(local), r(remote)),
            Op::Record(b) => (Some(b.sid), b.local, b.remote.clone()),
            Op::Forget(s) => (Some(*s), None, None),
            Op::Hash(p) => (None, l(p), None),
            Op::Upload { local, replace, link: k, .. } => (link(k), l(local), replace.as_ref().and_then(r)),
            Op::Download { remote, replace, link: k, .. } => (link(k), replace.as_ref().and_then(l), r(remote)),
            Op::MkdirRemote { local, link: k, .. } => (link(k), l(local), None),
            Op::MkdirLocal { remote, link: k, .. } => (link(k), None, r(remote)),
            Op::MoveRemote { sid, remote, .. } | Op::DeleteRemote { sid, remote } => (Some(*sid), None, r(remote)),
            Op::MoveLocal { sid, local, .. }
            | Op::RecycleLocal { sid, local }
            | Op::ConflictCopy { sid, local, .. } => (Some(*sid), l(local), None),
            Op::RenameLocal { local, .. } => (None, l(local), None),
            Op::PruneRemote { remote, .. } | Op::RenameRemote { remote, .. } => (None, None, r(remote)),
            Op::Dehydrate { local, .. } => (None, l(local), None),
            Op::Hold { sid, local, remote, .. } => (*sid, *local, remote.clone()),
        }
    }

    /// Whether the operation changes anything on this PC: content, names or
    /// places (bookkeeping and hashing don't).
    pub fn changes_local(&self) -> bool {
        matches!(
            self,
            Op::Download { .. }
                | Op::MkdirLocal { .. }
                | Op::MoveLocal { .. }
                | Op::RenameLocal { .. }
                | Op::ConflictCopy { .. }
                | Op::RecycleLocal { .. }
                | Op::Dehydrate { .. }
        )
    }

    /// Whether the operation changes anything (bookkeeping included); holds
    /// don't. A round with no changing operations is quiescent.
    pub fn is_action(&self) -> bool {
        !matches!(self, Op::Hold { .. })
    }
}

/// An item leaving its slot on one side.
#[derive(Debug, Clone, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub enum Vacates {
    Local(LocalId),
    Remote(RemoteId),
}
