//! Rewinding a folder to how it was at an earlier moment (F2).
//!
//! The Synced tree's history ([`History`]: what each Synced row agreed on
//! until each change, written as it changes) says, for any moment it
//! covers, which items a folder held, under which names, with which
//! content. [`plan`] compares that with now and proposes what puts it back:
//! * an item deleted since comes back from OneDrive's recycle bin, by its
//!   ID ([`Step::Restore`]), with its content of then if it changed before
//!   it went;
//! * an item moved or edited since is put back in OneDrive: its place, and
//!   its content of then from its version history ([`Step::Reset`]);
//! * an item created since is removed from OneDrive, into its recycle bin
//!   ([`Step::Remove`]; a folder once empty);
//! * a file changed on this PC since the last sync is moved into the
//!   version store and its record forgotten, so OneDrive's copy comes back
//!   ([`Step::SetAside`]); one deleted here only has its record forgotten
//!   ([`Step::Forget`]).
//!
//! Every step acts on OneDrive (or sets this PC's copy aside) and leaves
//! the Synced tree alone, so the next rounds bring this PC along as for any
//! change made in OneDrive. Nothing is destroyed: what a step replaces or
//! removes stays in OneDrive's version history or recycle bin, or in the
//! version store, so a rewind can itself be rewound.
//!
//! A rewind takes several rounds: a step may wait for another (a name to
//! be freed, a folder to be restored), names swapped since go through a
//! temporary name, and each round plans again from what's there. What the
//! user approved is kept item by item ([`Job`]): the state each item had
//! in the preview. A later round runs an item's step only while the item
//! is still in that state, or in the state the rewind's own last step left
//! it in, so anything changed after the preview is left alone.
//!
//! Like the reconciler, this is pure: it reads the trees and the history
//! and returns operations ([`Op::Rewind`]), which the caller passes to the
//! safety gate, where they always wait for the user's approval.
//!
//! Only items the history covers are rewound: those synced at the chosen
//! moment or since. What changed on either side and isn't synced yet is
//! left alone and reported ([`Skip`]), except this PC's edits to files the
//! history covers, which are set aside.

use std::collections::{BTreeMap, BTreeSet, HashMap};

use crate::model::{Base, Hash, LocalId, NodeKind, ROOT, RemoteId, RemoteNode, Seen, Sid, Trees, fold, same_name};
use crate::op::{LocalPre, Op, RemotePre};

/// An item as it was agreed at some moment: where, and with what content.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Was {
    /// `None` only for [`ROOT`].
    pub parent: Option<Sid>,
    pub name: String,
    pub kind: NodeKind,
    pub size: i64,
    pub hash: Option<Hash>,
    pub remote: Option<RemoteId>,
}

impl From<&Base> for Was {
    fn from(b: &Base) -> Self {
        Was {
            parent: b.parent,
            name: b.name.clone(),
            kind: b.kind,
            size: b.size,
            hash: b.hash,
            remote: b.remote.clone(),
        }
    }
}

/// Read access to the Synced tree's history: for each change to a Synced
/// row, what the row was until then. Implementations must be side-effect
/// free, like [`Trees`].
pub trait History {
    type Error;

    /// The first change to `sid` after moment `at` (Unix ms): `Some(Some(w))`
    /// it was `w` until then, `Some(None)` it was created then (so it
    /// didn't exist at `at`), `None` no change since (it's as it is now).
    fn change_after(&self, sid: Sid, at: i64) -> Result<Option<Option<Was>>, Self::Error>;

    /// Items whose agreed folder was `parent` until some change after `at`:
    /// with the folder's children now, every item that may have been in it
    /// at `at`.
    fn children_after(&self, parent: Sid, at: i64) -> Result<Vec<Sid>, Self::Error>;
}

/// One step of a rewind, for [`Op::Rewind`].
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub enum Step {
    /// Bring back deleted item `id` from OneDrive's recycle bin, into cloud
    /// folder `to` as `name`. A file whose content then was `content`
    /// (size, hash) is put back to it from its version history if it was
    /// edited before it was deleted.
    Restore { id: RemoteId, to: RemoteId, name: String, kind: NodeKind, content: Option<(i64, Hash)> },
    /// Put cloud item `remote`, which is `from` now, back into folder and
    /// name `place`, and (files) back to content `content` from its version
    /// history. The version it replaces stays in that history.
    Reset { remote: RemotePre, from: Spot, place: Option<(RemoteId, String)>, content: Option<(i64, Hash)> },
    /// Delete cloud item `remote`, which is `from` now, into OneDrive's
    /// recycle bin: it was created since. A folder only once it's empty
    /// (its eTag changes as its contents go, so it's read again first).
    Remove { remote: RemotePre, from: Spot },
    /// Move this PC's copy, changed since the last sync, into the version
    /// store, and forget the item's record, so OneDrive's copy (as the
    /// rewind leaves it) comes back as a new file.
    SetAside { local: LocalPre },
    /// Forget the record of an item deleted on this PC and not yet in
    /// OneDrive, so OneDrive's copy comes back as a new file.
    Forget,
    /// Move this PC's copy of a synced file into the version store as a
    /// deletion made here: its record stays, with the deletion noted, so
    /// the next round deletes OneDrive's copy too, into its recycle bin.
    /// Not part of a rewind: the copy a user answering a conflict doesn't
    /// keep (`oneup_exec::recover::conflict_ops`).
    Discard { local: LocalPre },
}

/// Where a cloud item is now and what it holds: part of a step's identity,
/// so a step approved for one state doesn't run on another (the item moved
/// or changed after the preview).
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub struct Spot {
    pub parent: RemoteId,
    pub name: String,
    pub size: i64,
    pub hash: Option<Hash>,
}

impl Spot {
    /// Where cloud item `r` is and what it holds.
    pub fn of(r: &RemoteNode) -> Spot {
        Spot {
            parent: r.parent.clone().unwrap_or_else(|| RemoteId(String::new())),
            name: r.name.clone(),
            size: r.size,
            hash: r.hash,
        }
    }
}

/// What a step does, for the preview's groups.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub enum Kind {
    /// Deleted since: restored.
    Restore,
    /// Edited since: its content of then put back.
    Revert,
    /// Moved or renamed since: put back.
    MoveBack,
    /// Created since: removed.
    Remove,
    /// Changed on this PC and not synced: set aside.
    SetAside,
}

/// Where the old version comes from.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Source {
    /// OneDrive's recycle bin.
    RecycleBin,
    /// The file's version history in OneDrive.
    Versions,
    /// Nothing old needed (a move, a removal, a setting aside): OneDrive
    /// already has the item.
    InPlace,
}

/// One change of a rewind, as the preview lists it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Change {
    pub sid: Sid,
    pub kind: Kind,
    pub item: NodeKind,
    /// Where it was at the chosen moment (`None`: it didn't exist).
    pub then: Option<String>,
    /// Where it is now (`None`: deleted).
    pub now: Option<String>,
    /// The size of the content put back, removed or set aside.
    pub size: i64,
    pub source: Source,
}

/// Why an item the history covers isn't rewound.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub enum Skip {
    /// Changed in OneDrive and not synced yet.
    ChangedInCloud,
    /// A folder moved on this PC and not synced yet.
    ChangedHere,
    /// Missing on this PC or in OneDrive without evidence (held).
    Missing,
    /// The folder it was in then is gone and isn't being restored.
    FolderGone,
    /// Its OneDrive item was never recorded.
    NoCloudItem,
    /// In OneDrive, but not linked to this PC yet.
    NotSyncedYet,
    /// Its content then wasn't recorded (no hash), so it can't be checked.
    UnknownContent,
}

/// A rewind's plan: the operations, in the order they must run, the path
/// each is shown at (the same place in `paths`), and the preview.
#[derive(Debug, Default, Clone, PartialEq, Eq)]
pub struct Rewind {
    pub ops: Vec<Op>,
    /// Where each operation's item was at the chosen moment, or, for one
    /// created since, where it is now: what a stop's list shows, and part of
    /// what an approval covers.
    pub paths: Vec<String>,
    pub changes: Vec<Change>,
    pub skipped: Vec<(Sid, String, Skip)>,
    /// Folders to remove that wait until this PC has caught up with what
    /// moved out of them, or until what's in them goes: a later round
    /// removes them. Part of the preview (and of what an approval covers),
    /// not of this round's operations; with their paths.
    pub later: Vec<(Op, String)>,
}

/// How a planned rewind fails.
#[derive(Debug)]
pub enum Error<E> {
    /// The trees or the history couldn't be read.
    Read(E),
    /// More than the limit given would change.
    TooMany,
}

impl<E> From<E> for Error<E> {
    fn from(e: E) -> Self {
        Error::Read(e)
    }
}

/// Item `sid` as it was agreed at moment `at` (`None`: it didn't exist).
pub fn state_at<T, H>(t: &T, h: &H, sid: Sid, at: i64) -> Result<Option<Was>, T::Error>
where
    T: Trees + ?Sized,
    H: History<Error = T::Error> + ?Sized,
{
    match h.change_after(sid, at)? {
        Some(w) => Ok(w),
        None => Ok(t.base(sid)?.map(|b| Was::from(&b))),
    }
}

/// The items in folder `parent` at moment `at`, as they were then, by name.
pub fn children_at<T, H>(t: &T, h: &H, parent: Sid, at: i64) -> Result<Vec<(Sid, Was)>, T::Error>
where
    T: Trees + ?Sized,
    H: History<Error = T::Error> + ?Sized,
{
    let mut candidates: BTreeSet<Sid> = h.children_after(parent, at)?.into_iter().collect();
    candidates.extend(t.base_children(parent)?.into_iter().map(|b| b.sid));
    let mut out = Vec::new();
    for c in candidates {
        if c == parent {
            continue;
        }
        if let Some(w) = state_at(t, h, c, at)?
            && w.parent == Some(parent)
        {
            out.push((c, w));
        }
    }
    out.sort_by(|a, b| a.1.name.cmp(&b.1.name).then(a.0.cmp(&b.0)));
    Ok(out)
}

/// The folder at `path` (names below the root, compared as Windows does)
/// at moment `at`, if there was one.
pub fn folder_at<T, H>(t: &T, h: &H, path: &[&str], at: i64) -> Result<Option<Sid>, T::Error>
where
    T: Trees + ?Sized,
    H: History<Error = T::Error> + ?Sized,
{
    let mut cur = ROOT;
    for part in path {
        let next = children_at(t, h, cur, at)?
            .into_iter()
            .find(|(_, w)| w.kind == NodeKind::Folder && same_name(&w.name, part));
        match next {
            Some((sid, _)) => cur = sid,
            None => return Ok(None),
        }
    }
    Ok(Some(cur))
}

/// What an item looked like then: its state, path and depth.
type Then = (Was, String, usize);

/// Plans rewinding folder `folder` (a Synced item; [`folder_at`]) to how it
/// was at moment `at`: everything in it then and now, at any depth, and
/// the folder itself unless it's the root. `max`: refuse if more than this
/// many items would change. Each round of a rewind plans again: what's
/// done no longer shows, and what's left goes on.
pub fn plan<T, H>(t: &T, h: &H, folder: Sid, at: i64, max: usize) -> Result<Rewind, Error<T::Error>>
where
    T: Trees + ?Sized,
    H: History<Error = T::Error> + ?Sized,
{
    let walk_max = max.saturating_mul(16).max(1 << 20);
    // What the folder held then.
    let mut then: BTreeMap<Sid, Then> = BTreeMap::new();
    let mut todo: Vec<(Sid, String, usize)> = Vec::new();
    if folder == ROOT {
        todo.push((ROOT, String::new(), 0));
    } else if let Some(w) = state_at(t, h, folder, at)? {
        let path = path_then(t, h, folder, at)?;
        if w.kind == NodeKind::Folder {
            todo.push((folder, path.clone(), 0));
        }
        then.insert(folder, (w, path, 0));
    }
    while let Some((p, path, depth)) = todo.pop() {
        for (c, w) in children_at(t, h, p, at)? {
            let cpath = format!("{path}/{}", w.name);
            if w.kind == NodeKind::Folder {
                todo.push((c, cpath.clone(), depth + 1));
            }
            then.insert(c, (w, cpath, depth + 1));
        }
        if then.len() > walk_max {
            return Err(Error::TooMany);
        }
    }

    // What it holds now, and where what it held then is now.
    let mut now: BTreeMap<Sid, (Base, String)> = BTreeMap::new();
    let mut todo: Vec<(Sid, String)> = Vec::new();
    if let Some(b) = t.base(folder)? {
        let path = path_now(t, &b)?;
        if b.kind == NodeKind::Folder {
            todo.push((folder, path.clone()));
        }
        if folder != ROOT {
            now.insert(folder, (b, path));
        }
    }
    // Items moved out since come back, and with them what's in them now.
    let moved_out: Vec<Sid> = then.keys().filter(|s| !now.contains_key(s)).copied().collect();
    for sid in moved_out {
        if now.contains_key(&sid) {
            continue; // inside another one moved out
        }
        if let Some(b) = t.base(sid)? {
            let path = path_now(t, &b)?;
            if b.kind == NodeKind::Folder {
                todo.push((sid, path.clone()));
            }
            now.insert(sid, (b, path));
        }
    }
    while let Some((p, path)) = todo.pop() {
        for b in t.base_children(p)? {
            let cpath = format!("{path}/{}", b.name);
            if b.kind == NodeKind::Folder {
                todo.push((b.sid, cpath.clone()));
            }
            now.insert(b.sid, (b, cpath));
        }
        if now.len() > walk_max {
            return Err(Error::TooMany);
        }
    }

    // An item whose record was dropped and made again since (a copy set
    // aside that came back, a restored item) is one item: by its OneDrive
    // ID, the record then stands for the record now.
    let mut then_by_remote: HashMap<RemoteId, Sid> = HashMap::new();
    for (sid, (w, _, _)) in &then {
        if let Some(r) = &w.remote {
            then_by_remote.insert(r.clone(), *sid);
        }
    }
    let mut alias: HashMap<Sid, Sid> = HashMap::new(); // now -> then
    for (sid, (b, _)) in &now {
        if then.contains_key(sid) {
            continue;
        }
        if let Some(r) = &b.remote
            && let Some(old) = then_by_remote.get(r)
            && !now.contains_key(old)
        {
            alias.insert(*sid, *old);
        }
    }
    let aliased: BTreeSet<Sid> = alias.values().copied().collect();
    // Items moved in since were somewhere else then: they go back there.
    let moved_in: Vec<Sid> = now.keys().filter(|s| !then.contains_key(s) && !alias.contains_key(s)).copied().collect();
    for sid in moved_in {
        if let Some(w) = state_at(t, h, sid, at)? {
            let path = path_then(t, h, sid, at)?;
            let depth = depth_of(&path);
            then.insert(sid, (w, path, depth));
        }
    }
    // A file whose OneDrive item was replaced since (deleted in OneDrive and
    // sent again from this PC): the item then comes back from the recycle
    // bin, with its version history, and the one made since goes. (A
    // folder made again stays: what's inside is put back into it.)
    let mut split: BTreeSet<Sid> = BTreeSet::new();
    for (sid, (b, _)) in &now {
        let then_sid = alias.get(sid).copied().unwrap_or(*sid);
        if b.kind == NodeKind::File
            && let Some((w, _, _)) = then.get(&then_sid)
            && let Some(r) = &w.remote
            && b.remote.as_ref() != Some(r)
            && matches!(t.remote(r)?, Seen::Deleted | Seen::Missing)
        {
            split.insert(*sid);
        }
    }

    // Where an item goes in OneDrive: its folder then, by its cloud ID now;
    // for a folder this rewind restores (a restored item keeps its ID) or
    // brings back in place of the one made since, its ID then.
    let dest = |parent: Sid| -> Result<Option<RemoteId>, T::Error> {
        if !split.contains(&parent) {
            if let Some((b, _)) = now.get(&parent) {
                return Ok(b.remote.clone());
            }
            if let Some(b) = t.base(parent)? {
                return Ok(b.remote);
            }
        }
        Ok(then.get(&parent).and_then(|(w, _, _)| w.remote.clone()))
    };

    let mut b = Builder::default();
    let sids: BTreeSet<Sid> = then.keys().chain(now.keys()).copied().collect();
    for sid in sids {
        let current = now.get(&sid);
        if current.is_none() && aliased.contains(&sid) {
            continue; // planned with the item's record now
        }
        let target = then.get(&alias.get(&sid).copied().unwrap_or(sid));
        match (target, current) {
            (None, None) => {}
            (None, Some((base, path))) => b.created(t, sid, base, path)?,
            (Some(then_item), None) => b.deleted(t, sid, then_item, &dest)?,
            (Some(then_item), Some((base, path))) if split.contains(&sid) => {
                b.created(t, sid, base, path)?;
                b.deleted(t, sid, then_item, &dest)?;
            }
            (Some(then_item), Some((base, path))) => b.kept(t, sid, then_item, base, path, &dest)?,
        }
        if b.out.changes.len() > max {
            return Err(Error::TooMany);
        }
    }
    // A folder goes only once everything recorded in it goes too, in this
    // round or before: an item moved out of it in OneDrive and not yet on
    // this PC would otherwise have this PC's copy of the folder made again
    // in OneDrive to hold it.
    let mut gone: BTreeSet<Sid> = b.remove_files.iter().filter_map(|(op, _)| sid_of(op)).collect();
    b.remove_folders.sort_by_key(|(d, _, _)| std::cmp::Reverse(*d));
    let mut folders = Vec::new();
    for (d, op, p) in std::mem::take(&mut b.remove_folders) {
        let Some(sid) = sid_of(&op) else { continue };
        let mut empty = true;
        for c in t.base_children(sid)? {
            empty &= gone.contains(&c.sid);
        }
        if empty {
            gone.insert(sid);
            folders.push((d, op, p));
        } else {
            b.out.later.push((op, p));
        }
    }
    b.remove_folders = folders;
    Ok(b.finish())
}

fn sid_of(op: &Op) -> Option<Sid> {
    match op {
        Op::Rewind { sid, .. } => Some(*sid),
        _ => None,
    }
}

/// The steps as they're found, by kind, for [`plan`]'s order.
#[derive(Default)]
struct Builder {
    out: Rewind,
    aside: Vec<(Op, String)>,
    remove_files: Vec<(Op, String)>,
    restores: Vec<(usize, Op, String)>,
    resets: Vec<(usize, Op, String)>,
    remove_folders: Vec<(usize, Op, String)>,
    /// Folders whose move goes into a folder inside them now: they wait
    /// until that one has moved out.
    inside: BTreeSet<RemoteId>,
}

impl Builder {
    fn skip(&mut self, sid: Sid, path: &str, why: Skip) {
        self.out.skipped.push((sid, path.to_string(), why));
    }

    fn change(&mut self, sid: Sid, kind: Kind, item: NodeKind, then: Option<&str>, now: Option<&str>, size: i64) {
        let source = match kind {
            Kind::Restore => Source::RecycleBin,
            Kind::Revert => Source::Versions,
            _ => Source::InPlace,
        };
        self.out.changes.push(Change {
            sid,
            kind,
            item,
            then: then.map(str::to_string),
            now: now.map(str::to_string),
            size,
            source,
        });
    }

    /// Created since: removed (this PC's unsynced edit set aside first).
    fn created<T: Trees + ?Sized>(&mut self, t: &T, sid: Sid, base: &Base, path: &str) -> Result<(), T::Error> {
        let local = local_state(t, base)?;
        if matches!(local, Local::Missing) {
            self.skip(sid, path, Skip::Missing);
            return Ok(());
        }
        if base.kind == NodeKind::Folder && matches!(local, Local::Changed(_)) {
            self.skip(sid, path, Skip::ChangedHere);
            return Ok(());
        }
        let Some(r) = cloud_settled(t, base)? else {
            let why = cloud_skip(t, base)?;
            self.skip(sid, path, why);
            return Ok(());
        };
        // An edit here would bring it back to OneDrive (a change beats a
        // deletion), so it's set aside.
        if let Local::Changed(pre) = local {
            self.aside.push((Op::rewind(sid, Step::SetAside { local: pre }), path.to_string()));
        }
        self.change(sid, Kind::Remove, base.kind, None, Some(path), base.size);
        let pre = RemotePre { id: r.id.clone(), etag: r.etag.clone() };
        let op = Op::rewind(sid, Step::Remove { remote: pre, from: Spot::of(&r) });
        if base.kind == NodeKind::Folder {
            self.remove_folders.push((depth_of(path), op, path.to_string()));
        } else {
            self.remove_files.push((op, path.to_string()));
        }
        Ok(())
    }

    /// Deleted since (or replaced by another item): back from the recycle bin.
    fn deleted<T: Trees + ?Sized>(
        &mut self,
        t: &T,
        sid: Sid,
        (w, path, depth): &Then,
        dest: &dyn Fn(Sid) -> Result<Option<RemoteId>, T::Error>,
    ) -> Result<(), T::Error> {
        let Some(id) = w.remote.clone() else {
            self.skip(sid, path, Skip::NoCloudItem);
            return Ok(());
        };
        if !matches!(t.remote(&id)?, Seen::Deleted | Seen::Missing) {
            self.skip(sid, path, Skip::NotSyncedYet);
            return Ok(());
        }
        let Some(parent) = w.parent else { return Ok(()) };
        let Some(to) = dest(parent)? else {
            self.skip(sid, path, Skip::FolderGone);
            return Ok(());
        };
        let content = match (w.kind, w.hash) {
            (NodeKind::File, Some(hash)) => Some((w.size, hash)),
            _ => None,
        };
        self.change(sid, Kind::Restore, w.kind, Some(path), None, w.size);
        let step = Step::Restore { id, to, name: w.name.clone(), kind: w.kind, content };
        self.restores.push((*depth, Op::rewind(sid, step), path.clone()));
        Ok(())
    }

    /// There then and now: put back where and what it was.
    fn kept<T: Trees + ?Sized>(
        &mut self,
        t: &T,
        sid: Sid,
        (w, path_then, depth): &Then,
        base: &Base,
        path_now: &str,
        dest: &dyn Fn(Sid) -> Result<Option<RemoteId>, T::Error>,
    ) -> Result<(), T::Error> {
        let local = local_state(t, base)?;
        if matches!(local, Local::Missing) {
            self.skip(sid, path_now, Skip::Missing);
            return Ok(());
        }
        if base.kind == NodeKind::Folder && matches!(local, Local::Changed(_)) {
            self.skip(sid, path_now, Skip::ChangedHere);
            return Ok(());
        }
        let Some(r) = cloud_settled(t, base)? else {
            let why = cloud_skip(t, base)?;
            self.skip(sid, path_now, why);
            return Ok(());
        };
        let edited = base.kind == NodeKind::File && (w.size != base.size || w.hash != base.hash);
        if edited && w.hash.is_none() {
            self.skip(sid, path_now, Skip::UnknownContent);
            return Ok(());
        }
        let place = match w.parent {
            Some(parent) => {
                let Some(to) = dest(parent)? else {
                    self.skip(sid, path_now, Skip::FolderGone);
                    return Ok(());
                };
                (r.parent.as_ref() != Some(&to) || r.name != w.name).then(|| (to, w.name.clone()))
            }
            None => None,
        };
        if let Some((to, _)) = &place
            && base.kind == NodeKind::Folder
            && within(t, to, &r.id)?
        {
            self.inside.insert(r.id.clone());
        }
        let content = if edited { w.hash.map(|h| (w.size, h)) } else { None };
        if place.is_some() || content.is_some() {
            let kind = if content.is_some() { Kind::Revert } else { Kind::MoveBack };
            self.change(sid, kind, base.kind, Some(path_then), Some(path_now), w.size);
            let pre = RemotePre { id: r.id.clone(), etag: r.etag.clone() };
            let step = Step::Reset { remote: pre, from: Spot::of(&r), place, content };
            self.resets.push((*depth, Op::rewind(sid, step), path_then.clone()));
        }
        match local {
            Local::Changed(pre) => {
                let size = pre.stamp.map_or(0, |s| s.0);
                self.change(sid, Kind::SetAside, base.kind, Some(path_then), Some(path_now), size);
                self.aside.push((Op::rewind(sid, Step::SetAside { local: pre }), path_then.clone()));
            }
            Local::Deleted => {
                self.change(sid, Kind::Restore, base.kind, Some(path_then), None, w.size);
                self.aside.push((Op::rewind(sid, Step::Forget), path_then.clone()));
            }
            Local::Settled | Local::Missing => {}
        }
        Ok(())
    }

    /// Names are freed before they're taken, folders made before what goes
    /// into them and emptied before they go: this PC's copies set aside,
    /// files removed, restores from the top down, moves (a move into a
    /// name another move frees after it; names swapped through a temporary
    /// name), then folders removed from the bottom up. What can't go yet (a
    /// restore into a name a move frees) waits for the next round.
    fn finish(mut self) -> Rewind {
        self.restores.sort_by_key(|(d, _, _)| *d);
        self.resets.sort_by_key(|(d, _, _)| *d);
        self.remove_folders.sort_by_key(|(d, _, _)| std::cmp::Reverse(*d));
        // A name held by an item that leaves only later (a folder that goes
        // once emptied, perhaps of the very item that needs the name; for a
        // restore, an item that moves on, perhaps into what's restored):
        // the item waits under a temporary name beside it, and the next
        // round moves it on.
        let held = |ops: &[(usize, Op, String)]| -> BTreeSet<Slot> {
            ops.iter()
                .filter_map(|(_, op, _)| match op.rewind_step()? {
                    Step::Remove { from, .. } | Step::Reset { from, place: Some(_), .. } => {
                        Some((from.parent.clone(), fold(&from.name)))
                    }
                    _ => None,
                })
                .collect()
        };
        let later: Vec<(usize, Op, String)> = self.out.later.iter().map(|(o, p)| (0, o.clone(), p.clone())).collect();
        let mut emptied = held(&self.remove_folders);
        emptied.extend(held(&later));
        let moved = held(&self.resets);
        for (_, op, _) in &mut self.restores {
            if let Op::Rewind { sid, step } = op
                && let Step::Restore { to, name, .. } = &mut **step
                && (emptied.contains(&(to.clone(), fold(name))) || moved.contains(&(to.clone(), fold(name))))
            {
                *name = temporary(name, *sid);
            }
        }
        for (_, op, _) in &mut self.resets {
            if let Op::Rewind { sid, step } = op
                && let Step::Reset { from, place: Some((to, name)), .. } = &mut **step
                && emptied.contains(&(to.clone(), fold(name)))
                && !(from.parent == *to && is_temporary(&from.name))
            {
                *name = temporary(name, *sid);
            }
        }
        let resets =
            order_moves(std::mem::take(&mut self.resets).into_iter().map(|(_, o, p)| (o, p)).collect(), &self.inside);
        let mut out = self.out;
        let all = self
            .aside
            .into_iter()
            .chain(self.remove_files)
            .chain(self.restores.into_iter().map(|(_, o, p)| (o, p)))
            .chain(resets)
            .chain(self.remove_folders.into_iter().map(|(_, o, p)| (o, p)));
        for (op, path) in all {
            out.ops.push(op);
            out.paths.push(path);
        }
        out
    }
}

/// A cloud folder and a name in it, as OneDrive compares names.
type Slot = (RemoteId, String);

fn slot_of(op: &Op) -> Option<(Slot, Slot)> {
    match op {
        Op::Rewind { step, .. } => match &**step {
            Step::Reset { from, place: Some((to, name)), .. } => {
                Some(((to.clone(), fold(name)), (from.parent.clone(), fold(&from.name))))
            }
            _ => None,
        },
        _ => None,
    }
}

/// Orders moves so that one goes after the move that frees its name, and
/// breaks a cycle (names swapped since) by sending one of its moves to a
/// temporary name in its folder first; the next round moves it on.
fn order_moves(mut todo: Vec<(Op, String)>, inside: &BTreeSet<RemoteId>) -> Vec<(Op, String)> {
    let mover = |op: &Op| match op.rewind_step() {
        Some(Step::Reset { remote, .. }) => Some(remote.id.clone()),
        _ => None,
    };
    let mut out = Vec::with_capacity(todo.len());
    // Slots held now by items that move away.
    let mut held: HashMap<Slot, usize> = HashMap::new();
    for (i, (op, _)) in todo.iter().enumerate() {
        if let Some((target, now)) = slot_of(op)
            && target != now
        {
            held.insert(now, i);
        }
    }
    let mut done = vec![false; todo.len()];
    loop {
        let mut progress = false;
        for i in 0..todo.len() {
            if done[i] {
                continue;
            }
            let free = match slot_of(&todo[i].0) {
                None => true,
                _ if mover(&todo[i].0).is_some_and(|id| inside.contains(&id)) => false,
                Some((target, now)) => target == now || held.get(&target).is_none_or(|&j| j == i),
            };
            if free {
                if let Some((_, now)) = slot_of(&todo[i].0) {
                    held.remove(&now);
                }
                done[i] = true;
                out.push(todo[i].clone());
                progress = true;
            }
        }
        if done.iter().all(|d| *d) {
            break;
        }
        if !progress {
            // Every move left waits for another: a cycle. One goes to a
            // temporary name in the folder it goes to (where it is, for one
            // that would go inside itself), which frees its name and takes
            // it out of what it was in.
            let waiting = |i: &usize| !done[*i];
            let free_to_go = |i: &usize| mover(&todo[*i].0).is_none_or(|id| !inside.contains(&id));
            let Some(i) = (0..todo.len()).filter(waiting).find(free_to_go).or_else(|| (0..todo.len()).find(waiting))
            else {
                break;
            };
            let blocked = !free_to_go(&i);
            if let Op::Rewind { sid, step } = &mut todo[i].0
                && let Step::Reset { from, place, .. } = &mut **step
                && !is_temporary(&from.name)
            {
                let folder = match place {
                    Some((to, _)) if !blocked => to.clone(),
                    _ => from.parent.clone(),
                };
                *place = Some((folder, temporary(&from.name, *sid)));
                if let Some(now) = held.iter().find(|(_, j)| **j == i).map(|(s, _)| s.clone()) {
                    held.remove(&now);
                }
            }
            done[i] = true;
            out.push(todo[i].clone());
        }
    }
    out
}

/// The temporary name an item waits under while its name is held (see
/// [`order_moves`]): unlike the planner's staging names, which the planner
/// renames back by itself, only the rewind moves it on.
fn temporary(name: &str, sid: Sid) -> String {
    format!("{name}.oneup-rewind-{sid}")
}

/// Whether cloud folder `folder` is `item` or inside it now.
fn within<T: Trees + ?Sized>(t: &T, folder: &RemoteId, item: &RemoteId) -> Result<bool, T::Error> {
    let mut cur = Some(folder.clone());
    for _ in 0..4096 {
        let Some(id) = cur else { return Ok(false) };
        if id == *item {
            return Ok(true);
        }
        cur = match t.remote(&id)? {
            Seen::Present(r) => r.parent,
            _ => None,
        };
    }
    Ok(false)
}

/// Whether `name` is one [`temporary`] made: an item already waiting
/// isn't moved to another temporary name, it waits.
fn is_temporary(name: &str) -> bool {
    name.rsplit_once(".oneup-rewind-").is_some_and(|(_, n)| !n.is_empty() && n.bytes().all(|b| b.is_ascii_digit()))
}

fn depth_of(path: &str) -> usize {
    path.matches('/').count()
}

/// How a Synced item's local side stands against its record.
enum Local {
    /// As synced.
    Settled,
    /// Changed since the last sync (a file's stamp, or the place).
    Changed(LocalPre),
    /// Deleted with evidence, not yet in OneDrive.
    Deleted,
    /// Gone without evidence (held).
    Missing,
}

fn local_state<T: Trees + ?Sized>(t: &T, b: &Base) -> Result<Local, T::Error> {
    let Some(id) = b.local else { return Ok(Local::Deleted) };
    let node = match t.local(id)? {
        Seen::Present(n) => n,
        Seen::Deleted => return Ok(Local::Deleted),
        Seen::Missing => return Ok(Local::Missing),
    };
    let parent_local = match b.parent {
        Some(p) => t.base(p)?.and_then(|pb| pb.local),
        None => None,
    };
    let placed = b.parent.is_none() || (node.parent == parent_local && node.name == b.name);
    let stamp = (b.kind == NodeKind::File).then_some((node.size, node.mtime));
    let same = placed && (b.kind == NodeKind::Folder || (node.size == b.size && node.mtime == b.local_mtime));
    Ok(if same { Local::Settled } else { Local::Changed(LocalPre { id, stamp }) })
}

/// The cloud item of `b` if it's exactly as synced (place and content).
fn cloud_settled<T: Trees + ?Sized>(t: &T, b: &Base) -> Result<Option<RemoteNode>, T::Error> {
    let Some(id) = &b.remote else { return Ok(None) };
    let Seen::Present(r) = t.remote(id)? else { return Ok(None) };
    let parent_remote = match b.parent {
        Some(p) => t.base(p)?.and_then(|pb| pb.remote),
        None => None,
    };
    let placed = b.parent.is_none() || (r.parent == parent_remote && r.name == b.name);
    let same = placed && (b.kind == NodeKind::Folder || (r.size == b.size && r.hash == b.hash));
    Ok(same.then_some(r))
}

/// Why an item whose cloud side isn't as synced is skipped.
fn cloud_skip<T: Trees + ?Sized>(t: &T, b: &Base) -> Result<Skip, T::Error> {
    let Some(id) = &b.remote else { return Ok(Skip::NoCloudItem) };
    Ok(match t.remote(id)? {
        Seen::Missing => Skip::Missing,
        _ => Skip::ChangedInCloud,
    })
}

/// `sid`'s path at moment `at` (`/a/b`), from its folders then.
pub fn path_then<T, H>(t: &T, h: &H, sid: Sid, at: i64) -> Result<String, T::Error>
where
    T: Trees + ?Sized,
    H: History<Error = T::Error> + ?Sized,
{
    let mut parts = Vec::new();
    let mut cur = sid;
    for _ in 0..4096 {
        if cur == ROOT {
            break;
        }
        let Some(w) = state_at(t, h, cur, at)? else { break };
        parts.push(w.name);
        match w.parent {
            Some(p) => cur = p,
            None => break,
        }
    }
    parts.reverse();
    Ok(parts.iter().map(|p| format!("/{p}")).collect())
}

/// `b`'s path now (`/a/b`), from the Synced tree.
fn path_now<T: Trees + ?Sized>(t: &T, b: &Base) -> Result<String, T::Error> {
    let mut parts = Vec::new();
    let mut cur = Some(b.clone());
    for _ in 0..4096 {
        let Some(c) = cur else { break };
        if c.sid == ROOT {
            break;
        }
        parts.push(c.name.clone());
        cur = match c.parent {
            Some(p) => t.base(p)?,
            None => None,
        };
    }
    parts.reverse();
    Ok(parts.iter().map(|p| format!("/{p}")).collect())
}

// ------------------------------------------------------------------ job

/// Steps, each with the path a stop's list shows for it.
pub type Shown = Vec<(Op, String)>;

/// What a step's approval is bound to: the item it acts on.
#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub enum Key {
    /// A OneDrive item (restored, reset or removed).
    Cloud(RemoteId),
    /// A copy on this PC set aside.
    Local(LocalId),
    /// A record forgotten.
    Record(Sid),
}

/// The state an item was in when its step was planned.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Mark {
    /// Not in OneDrive (in its recycle bin).
    Absent,
    /// In OneDrive, there and with that content.
    Cloud(Spot),
    /// On this PC, with that size and last-write time.
    Local(Option<(i64, i64)>),
    /// A record, its copy here gone.
    Record,
}

/// What `op`'s step acts on and the state it was planned against.
pub fn mark_of(op: &Op) -> Option<(Key, Mark)> {
    let Op::Rewind { sid, step } = op else { return None };
    Some(match &**step {
        Step::Restore { id, .. } => (Key::Cloud(id.clone()), Mark::Absent),
        Step::Reset { remote, from, .. } | Step::Remove { remote, from } => {
            (Key::Cloud(remote.id.clone()), Mark::Cloud(from.clone()))
        }
        Step::SetAside { local } | Step::Discard { local } => (Key::Local(local.id), Mark::Local(local.stamp)),
        Step::Forget => (Key::Record(*sid), Mark::Record),
    })
}

/// A rewind the user approved, item by item: the state each item was in
/// when the preview listed it, and the state the rewind's own last step on
/// it left it in. A later round's step runs only from one of those (see
/// the module docs).
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Job {
    pub approved: BTreeMap<Key, Mark>,
    pub left: BTreeMap<Key, Mark>,
    /// The Synced items its steps changed: this PC following those changes
    /// was approved with the rewind (`oneup_gate::Context::rewound`).
    pub rewound: BTreeSet<Sid>,
}

impl Job {
    /// The approval of exactly the steps `ops` (a stop's list).
    pub fn approve(ops: &[Op]) -> Job {
        Job { approved: ops.iter().filter_map(mark_of).collect(), ..Job::default() }
    }

    /// Whether `op` is a step of this rewind from a state it may run from.
    pub fn accepts(&self, op: &Op) -> bool {
        let Some((key, mark)) = mark_of(op) else { return false };
        self.approved.get(&key) == Some(&mark) || self.left.get(&key) == Some(&mark)
    }

    /// Of a round's steps `ops` (each shown at the path of the same place
    /// in `paths`), those to run and those left alone: an item's steps run
    /// only if this approval covers each of them, so an item changed after
    /// the preview (on either side) is left alone whole.
    pub fn take(&self, ops: Vec<Op>, paths: Vec<String>) -> (Shown, Shown) {
        let refused: BTreeSet<Sid> =
            ops.iter().filter(|op| !self.accepts(op)).filter_map(|op| op.subjects().0).collect();
        ops.into_iter()
            .zip(paths)
            .partition(|(op, _)| self.accepts(op) && op.subjects().0.is_none_or(|s| !refused.contains(&s)))
    }

    /// Step `op` ran and left its item in state `now`.
    pub fn ran(&mut self, op: &Op, now: Mark) {
        if let Some((key, _)) = mark_of(op) {
            self.left.insert(key, now);
        }
        if let Op::Rewind { sid, .. } = op {
            self.rewound.insert(*sid);
        }
    }
}
