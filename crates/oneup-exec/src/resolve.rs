//! The user's answers for items sync holds (`oneup unsynced resolve`).
//!
//! None of these touches user content. Each turns the user's explicit
//! choice into state the planner already acts on, so the change itself is
//! planned, reviewed by the safety gate (its thresholds and circuit
//! breakers included) and carried out by the executor with the usual
//! preconditions (`data-safety.md` invariants 2, 5 and 8):
//!
//! * **Delete there**, for an item gone from this PC without evidence: the
//!   choice is the evidence (invariant 3). The local deletion is noted for
//!   the item and everything under it that is gone too, and the planner
//!   then deletes the cloud copies (to OneDrive's recycle bin, `If-Match`
//!   on the eTag it saw). One changed in OneDrive since it last synced
//!   comes back here instead: a change wins over a deletion.
//! * **Delete here**, for an item gone from OneDrive without evidence: the
//!   mirror image. The local copies go to the version store, and one
//!   changed here since it last synced is uploaded instead.
//! * **Restore** and **upload again**: the item's Synced record, and those
//!   under it, are dropped, so the side that still has it reads as new and
//!   is transferred to the other. A record only says what last agreed;
//!   dropping it deletes nothing, and a name taken on the other side
//!   meanwhile is paired by content or kept as both (invariant 9).
//! * **Rename**: an [`Op::RenameLocal`] for the caller to add to a round,
//!   which the gate reviews and the effects layer carries out on the item
//!   opened by its ID, checked against the size and last-write time seen
//!   here, never over another name.
//!
//! Each is checked against the state now, and refused ([`Refused`]) if it
//! no longer fits: the item came back, or is gone from the other side too,
//! or the folder it would go into is missing.

use oneup_store::{LocalKind, Ops};
use oneup_sync::{Base, LocalId, LocalPre, NodeKind, Op, ROOT, RemoteId, Sid};

/// Why a choice can't be carried out now.
#[derive(Debug, Clone, Copy, PartialEq, Eq, thiserror::Error)]
pub enum Refused {
    #[error("it isn't synced any more")]
    NotSynced,
    #[error("it's on this PC")]
    Here,
    #[error("it's in OneDrive")]
    There,
    #[error("it's gone from this PC too")]
    GoneHereToo,
    #[error("it's gone from OneDrive too")]
    GoneThereToo,
    #[error("the folder it's in is gone from this PC too; restore that folder first")]
    FolderGoneHere,
    #[error("the folder it's in is gone from OneDrive too; upload that folder first")]
    FolderGoneThere,
    #[error("Windows doesn't allow that name")]
    BadName,
    #[error("that's its name already")]
    SameName,
    #[error("another item in its folder has that name")]
    NameTaken,
    #[error("OneDrive has another item of that name in its folder")]
    NameTakenThere,
}

#[derive(Debug, thiserror::Error)]
pub enum Error {
    #[error(transparent)]
    Refused(#[from] Refused),
    #[error(transparent)]
    Store(#[from] oneup_store::Error),
}

pub type Result<T> = std::result::Result<T, Error>;

/// What a choice changes in the store, worked out without changing
/// anything ([`Change::commit`] does).
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Change {
    /// Note these Synced items deleted on this PC.
    GoneHere(Vec<(Sid, LocalId)>),
    /// Note these Synced items deleted in OneDrive.
    GoneThere(Vec<(Sid, RemoteId)>),
    /// Drop record `sid` with the `items` records at and below it, and
    /// plan folder `folder` again, where it reads as new.
    Forget { sid: Sid, items: usize, folder: Sid },
    /// Run this in the next round.
    Rename(Op),
}

impl Change {
    /// How many items it concerns.
    pub fn items(&self) -> usize {
        match self {
            Change::GoneHere(v) => v.len(),
            Change::GoneThere(v) => v.len(),
            Change::Forget { items, .. } => *items,
            Change::Rename(_) => 1,
        }
    }

    /// Writes it, and marks what it concerns for planning. Returns the
    /// operation to add to the round, for a rename.
    pub fn commit(self, o: &Ops<'_>) -> oneup_store::Result<Option<Op>> {
        match self {
            Change::GoneHere(items) => {
                for (_, l) in &items {
                    o.note_local_deleted(l.0)?;
                }
                o.dirty_mark_synced(&items.iter().map(|(s, _)| *s).collect::<Vec<_>>())?;
            }
            Change::GoneThere(items) => {
                for (_, r) in &items {
                    o.note_remote_deleted(&r.0)?;
                }
                o.dirty_mark_synced(&items.iter().map(|(s, _)| *s).collect::<Vec<_>>())?;
            }
            Change::Forget { sid, folder, .. } => {
                o.synced_remove(sid)?;
                o.dirty_mark_synced(&[folder])?;
            }
            Change::Rename(op) => return Ok(Some(op)),
        }
        Ok(None)
    }
}

/// Whether local item `id` is on this PC: in the Local tree, or still
/// opening by its ID inside the sync folder (`present`, which the caller
/// asks the file system; `LocalFs::is_present`).
fn here(o: &Ops<'_>, id: LocalId, present: &dyn Fn(LocalId) -> bool) -> oneup_store::Result<bool> {
    Ok(o.local_get(id.0)?.is_some() || present(id))
}

fn there(o: &Ops<'_>, id: &RemoteId) -> oneup_store::Result<bool> {
    Ok(o.get_item(&id.0)?.is_some())
}

/// Record `sid`, if it's an item's (not the root's).
fn record(o: &Ops<'_>, sid: Sid) -> Result<Base> {
    match o.synced_get(sid)? {
        Some(b) if sid != ROOT => Ok(b),
        _ => Err(Refused::NotSynced.into()),
    }
}

/// `b` and every record below it, parents first.
fn subtree(o: &Ops<'_>, b: Base) -> oneup_store::Result<Vec<Base>> {
    let mut out = vec![b];
    let mut i = 0;
    while i < out.len() {
        if out[i].kind == NodeKind::Folder {
            let kids = o.synced_children(out[i].sid)?;
            out.extend(kids);
        }
        i += 1;
    }
    Ok(out)
}

/// Record `sid` is gone from this PC and still in OneDrive: delete it in
/// OneDrive, with everything under it that is gone from this PC too.
pub fn delete_there(o: &Ops<'_>, sid: Sid, present: &dyn Fn(LocalId) -> bool) -> Result<Change> {
    let b = record(o, sid)?;
    gone_here_still_there(o, &b, present)?;
    let mut items = Vec::new();
    for b in subtree(o, b)? {
        if let Some(l) = b.local
            && !here(o, l, present)?
        {
            items.push((b.sid, l));
        }
    }
    Ok(Change::GoneHere(items))
}

/// Record `sid` is gone from OneDrive and still on this PC: delete it here,
/// with everything under it that is gone from OneDrive too.
pub fn delete_here(o: &Ops<'_>, sid: Sid) -> Result<Change> {
    let b = record(o, sid)?;
    gone_there_still_here(o, &b)?;
    let mut items = Vec::new();
    for b in subtree(o, b)? {
        if let Some(r) = b.remote
            && !there(o, &r)?
        {
            items.push((b.sid, r));
        }
    }
    Ok(Change::GoneThere(items))
}

/// Record `sid` is gone from this PC and still in OneDrive: bring it back
/// here, into the folder that matches its OneDrive folder.
pub fn restore(o: &Ops<'_>, sid: Sid, present: &dyn Fn(LocalId) -> bool) -> Result<Change> {
    let b = record(o, sid)?;
    let r = gone_here_still_there(o, &b, present)?;
    let row = o.get_item(&r.0)?.ok_or(Refused::GoneThereToo)?;
    let folder = row
        .parent_id
        .map(|p| o.synced_by_remote(&p))
        .transpose()?
        .flatten()
        .filter(|f| f.kind == NodeKind::Folder)
        .ok_or(Refused::FolderGoneHere)?;
    match folder.local {
        Some(l) if o.local_get(l.0)?.is_some() => {}
        _ => return Err(Refused::FolderGoneHere.into()),
    }
    let items = subtree(o, b)?.len();
    Ok(Change::Forget { sid, items, folder: folder.sid })
}

/// Record `sid` is gone from OneDrive and still on this PC: upload it
/// again, into the OneDrive folder that matches its folder here.
pub fn upload(o: &Ops<'_>, sid: Sid) -> Result<Change> {
    let b = record(o, sid)?;
    let l = gone_there_still_here(o, &b)?;
    let row = o.local_get(l.0)?.ok_or(Refused::GoneHereToo)?;
    let folder = row
        .parent_id
        .map(|p| o.synced_by_local(p))
        .transpose()?
        .flatten()
        .filter(|f| f.kind == NodeKind::Folder)
        .ok_or(Refused::FolderGoneThere)?;
    match &folder.remote {
        Some(r) if there(o, r)? => {}
        _ => return Err(Refused::FolderGoneThere.into()),
    }
    let items = subtree(o, b)?.len();
    Ok(Change::Forget { sid, items, folder: folder.sid })
}

/// Renames local item `id` to `name` in its folder. The name must be one
/// Windows allows, and free here and in the matching OneDrive folder, as
/// Windows compares names. The caller checks it's one OneDrive allows.
pub fn rename(o: &Ops<'_>, id: LocalId, name: &str) -> Result<Change> {
    let row = o.local_get(id.0)?.ok_or(Refused::GoneHereToo)?;
    let Some(parent) = row.parent_id.filter(|_| row.kind != LocalKind::Root) else {
        return Err(Refused::NotSynced.into());
    };
    let at_root = o.local_get(parent)?.is_some_and(|p| p.kind == LocalKind::Root);
    if !oneup_effects::name::is_valid_name(name, at_root) {
        return Err(Refused::BadName.into());
    }
    if name == row.name {
        return Err(Refused::SameName.into());
    }
    if o.local_named(parent, name)?.iter().any(|n| n.file_id != id.0) {
        return Err(Refused::NameTaken.into());
    }
    let own = o.synced_by_local(id.0)?.and_then(|b| b.remote);
    if let Some(folder) = o.synced_by_local(parent)?.and_then(|b| b.remote)
        && o.remote_named(&folder.0, name)?.iter().any(|r| own.as_ref().is_none_or(|own| own.0 != r.id))
    {
        return Err(Refused::NameTakenThere.into());
    }
    let stamp = (row.kind == LocalKind::File).then_some((row.size, row.mtime));
    Ok(Change::Rename(Op::RenameLocal { local: LocalPre { id, stamp }, dir: LocalId(parent), name: name.to_string() }))
}

/// `b` is gone from this PC and still in OneDrive (what the planner holds
/// as missing without evidence). Returns its cloud item.
fn gone_here_still_there(o: &Ops<'_>, b: &Base, present: &dyn Fn(LocalId) -> bool) -> Result<RemoteId> {
    // A record without a local link was let go by the executor itself,
    // which is evidence: nothing is held.
    let l = b.local.ok_or(Refused::NotSynced)?;
    if here(o, l, present)? {
        return Err(Refused::Here.into());
    }
    match &b.remote {
        Some(r) if there(o, r)? => Ok(r.clone()),
        _ => Err(Refused::GoneThereToo.into()),
    }
}

/// `b` is gone from OneDrive and still on this PC. Returns its local item.
fn gone_there_still_here(o: &Ops<'_>, b: &Base) -> Result<LocalId> {
    let r = b.remote.as_ref().ok_or(Refused::NotSynced)?;
    if there(o, r)? {
        return Err(Refused::There.into());
    }
    match b.local {
        Some(l) if o.local_get(l.0)?.is_some() => Ok(l),
        _ => Err(Refused::GoneHereToo.into()),
    }
}
