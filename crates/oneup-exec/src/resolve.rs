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
//! or the folder it would go into is missing. A [`Change`] has a
//! fingerprint of every record it concerns, so a caller can tell that
//! what it carries out is exactly what the user was shown.
//!
//! A folder's records are walked a folder at a time, keeping only the
//! folders still to visit and, for a deletion, the items it notes.

use oneup_store::{LocalKind, Ops};
use oneup_sync::{Base, LocalId, LocalPre, NodeKind, Op, ROOT, RemoteId, Sid};
use sha2::{Digest, Sha256};

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
pub struct Change {
    what: What,
    items: usize,
    /// Of every record it concerns (sid and both linked IDs), in walk order.
    fingerprint: [u8; 32],
}

#[derive(Debug, Clone, PartialEq, Eq)]
enum What {
    /// Note these Synced items deleted on this PC.
    GoneHere(Vec<(Sid, LocalId)>),
    /// Note these Synced items deleted in OneDrive.
    GoneThere(Vec<(Sid, RemoteId)>),
    /// Drop record `sid` with the records below it, and plan folder
    /// `folder` again, where it reads as new.
    Forget { sid: Sid, folder: Sid },
    /// Run this in the next round.
    Rename(Op),
}

/// Fingerprints the records a change concerns.
struct Print(Sha256);

impl Print {
    fn new(kind: &str) -> Self {
        let mut h = Sha256::new();
        h.update(kind.as_bytes());
        Self(h)
    }

    fn record(&mut self, b: &Base) {
        self.0.update(b.sid.to_le_bytes());
        self.0.update(b.local.map_or(0, |l| l.0).to_le_bytes());
        let r = b.remote.as_ref().map_or("", |r| r.0.as_str());
        self.0.update((r.len() as u64).to_le_bytes());
        self.0.update(r.as_bytes());
    }

    fn done(self, what: What, items: usize) -> Change {
        Change { what, items, fingerprint: self.0.finalize().into() }
    }
}

impl Change {
    /// How many items it concerns.
    pub fn items(&self) -> usize {
        self.items
    }

    /// The records it concerns, as 32 hex digits: the same exactly when it
    /// would change the same items (as they're linked now) the same way.
    pub fn fingerprint(&self) -> String {
        self.fingerprint[..16].iter().map(|b| format!("{b:02x}")).collect()
    }

    /// Writes it, and marks what it concerns for planning. Returns the
    /// operation to add to the round, for a rename.
    pub fn commit(self, o: &Ops<'_>) -> oneup_store::Result<Option<Op>> {
        match self.what {
            What::GoneHere(items) => {
                for (_, l) in &items {
                    o.note_local_deleted(l.0)?;
                }
                o.dirty_mark_synced(&items.iter().map(|(s, _)| *s).collect::<Vec<_>>())?;
            }
            What::GoneThere(items) => {
                for (_, r) in &items {
                    o.note_remote_deleted(&r.0)?;
                }
                o.dirty_mark_synced(&items.iter().map(|(s, _)| *s).collect::<Vec<_>>())?;
            }
            What::Forget { sid, folder } => {
                o.synced_remove(sid)?;
                o.dirty_mark_synced(&[folder])?;
            }
            What::Rename(op) => return Ok(Some(op)),
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

/// Calls `f` for `b` and every record below it, parents first, holding
/// one folder's records at a time.
fn walk(o: &Ops<'_>, b: Base, mut f: impl FnMut(&Base) -> Result<()>) -> Result<()> {
    let mut folders = std::collections::VecDeque::new();
    f(&b)?;
    if b.kind == NodeKind::Folder {
        folders.push_back(b.sid);
    }
    while let Some(sid) = folders.pop_front() {
        for k in o.synced_children(sid)? {
            f(&k)?;
            if k.kind == NodeKind::Folder {
                folders.push_back(k.sid);
            }
        }
    }
    Ok(())
}

/// Record `sid` is gone from this PC and still in OneDrive: delete it in
/// OneDrive, with everything under it that is the same (gone from this PC,
/// still in OneDrive). What's gone from both sides, or still here, is left.
pub fn delete_there(o: &Ops<'_>, sid: Sid, present: &dyn Fn(LocalId) -> bool) -> Result<Change> {
    let b = record(o, sid)?;
    gone_here_still_there(o, &b, present)?;
    let (mut items, mut print) = (Vec::new(), Print::new("gone here"));
    walk(o, b, |b| {
        if let Some(l) = b.local
            && still(gone_here_still_there(o, b, present))?
        {
            print.record(b);
            items.push((b.sid, l));
        }
        Ok(())
    })?;
    let n = items.len();
    Ok(print.done(What::GoneHere(items), n))
}

/// Record `sid` is gone from OneDrive and still on this PC: delete it here,
/// with everything under it that is the same (gone from OneDrive, still
/// here).
pub fn delete_here(o: &Ops<'_>, sid: Sid) -> Result<Change> {
    let b = record(o, sid)?;
    gone_there_still_here(o, &b)?;
    let (mut items, mut print) = (Vec::new(), Print::new("gone there"));
    walk(o, b, |b| {
        if let Some(r) = &b.remote
            && still(gone_there_still_here(o, b))?
        {
            print.record(b);
            items.push((b.sid, r.clone()));
        }
        Ok(())
    })?;
    let n = items.len();
    Ok(print.done(What::GoneThere(items), n))
}

/// Every record at and below `b`, for dropping: how many, fingerprinted.
fn forget(o: &Ops<'_>, b: Base, kind: &str, folder: Sid) -> Result<Change> {
    let sid = b.sid;
    let (mut n, mut print) = (0, Print::new(kind));
    walk(o, b, |b| {
        print.record(b);
        n += 1;
        Ok(())
    })?;
    Ok(print.done(What::Forget { sid, folder }, n))
}

/// Whether a check passed: a refusal is `false`, a store error an error.
fn still<T>(r: Result<T>) -> oneup_store::Result<bool> {
    match r {
        Ok(_) => Ok(true),
        Err(Error::Refused(_)) => Ok(false),
        Err(Error::Store(e)) => Err(e),
    }
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
    forget(o, b, "restore", folder.sid)
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
    forget(o, b, "upload", folder.sid)
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
    let mut print = Print::new("rename");
    for x in [id.0, parent, row.size as u128, row.mtime as u128] {
        print.0.update(x.to_le_bytes());
    }
    print.0.update(name.as_bytes());
    let op = Op::RenameLocal { local: LocalPre { id, stamp }, dir: LocalId(parent), name: name.to_string() };
    Ok(print.done(What::Rename(op), 1))
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
