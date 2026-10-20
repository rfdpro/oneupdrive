//! Running a rewind's steps ([`Op::Rewind`], `oneup_sync::rewind`).
//!
//! Each step changes OneDrive under its own precondition (the planned eTag,
//! or for a folder to remove, that OneDrive reports it empty), or sets this
//! PC's copy aside into the version store by handle. None writes the Synced
//! tree's agreed state: the next round sees OneDrive changed and brings
//! this PC along, through the gate, as for any change made in OneDrive.

use std::io::Seek as _;
use std::sync::Arc;

use oneup_effects::Done;
use oneup_gate::Approved;
use oneup_store::{Kind, RemoteRow};
use oneup_sync::rewind::{Mark, Spot, Step};
use oneup_sync::{Hash, LocalPre, NodeKind, Op, RemoteId, Sid};

use super::{Db, Executor, Fail, Pending, Res, Skip, need, odd, pending_of, pre_of};
use crate::blocking::OffTask;
use crate::cloud::{Cloud, CloudError, Hashing};

/// Versions of the right size tried before giving up: each is downloaded
/// to be checked by its hash (OneDrive's version list carries no hash).
/// Every version OneDrive lists is a candidate, newest first; this only
/// bounds a history far longer than OneDrive keeps.
const VERSION_TRIES: usize = 1_000;

impl<C: Cloud> Executor<C> {
    /// Runs one approved rewind step.
    pub(super) async fn rewind(&self, store: &Db<'_>, a: &Arc<Approved>, sid: Sid, step: &Step) -> Res {
        match step {
            Step::Restore { id, to, name, kind, content, .. } => {
                // Back already (a folder restored with its contents, or the
                // user meanwhile): only if it's where it belongs.
                let row = match self.cloud.item(id).await {
                    Ok(Some(row)) => {
                        need(row.parent_id.as_deref() == Some(to.0.as_str()) && row.name == *name)?;
                        row
                    }
                    Ok(None) => match self.cloud.restore(id, to, name).await {
                        Err(CloudError::Changed) => {
                            return Err(Fail::Failed(
                                "it's no longer in OneDrive's recycle bin, or the folder it goes back into is gone"
                                    .into(),
                            ));
                        }
                        r => r?,
                    },
                    Err(e) => return Err(e.into()),
                };
                self.crash_point()?;
                store.write(|o| Ok::<_, Fail>(o.upsert_item(&row, false)?)).await?;
                if let (NodeKind::File, Some((size, hash))) = (kind, content)
                    && row.qxh != Some(*hash)
                {
                    self.put_back(store, &row, *size, hash).await?;
                }
                Ok(())
            }
            Step::Reset { remote, from: _, place, content } => {
                let mut pre = remote.clone();
                if let Some((size, hash)) = content {
                    let row = store.read(|o| o.get_item(&remote.id.0))?.ok_or(Fail::Skip(Skip::Changed))?;
                    need(row.etag.as_deref() == Some(remote.etag.as_str()))?;
                    let row = self.put_back(store, &row, *size, hash).await?;
                    pre = pre_of(&row);
                    self.crash_point()?;
                }
                if let Some((to, name)) = place {
                    let row = self.cloud.move_item(&pre, to, name).await?;
                    self.crash_point()?;
                    store.write(|o| Ok::<_, Fail>(o.upsert_item(&row, false)?)).await?;
                }
                Ok(())
            }
            Step::Remove { remote, from: _ } => {
                let kind = store.read(|o| o.get_item(&remote.id.0))?.map(|r| r.kind);
                let pre = if kind == Some(Kind::Folder) {
                    // Its eTag moved as the rewind emptied it: what counts is
                    // that OneDrive has nothing in it now.
                    let now = self.cloud.item(&remote.id).await?.ok_or(Fail::Skip(Skip::Changed))?;
                    need(now.kind == Kind::Folder)?;
                    if !self.cloud.children(&remote.id).await?.is_empty() {
                        return Err(Fail::Skip(Skip::NotEmpty));
                    }
                    pre_of(&now)
                } else {
                    remote.clone()
                };
                self.cloud.delete(&pre).await?;
                self.crash_point()?;
                // Deletion evidence for its record: the next round recycles
                // this PC's copy into the version store.
                store.write(|o| Ok::<_, Fail>(o.delete_subtree(&remote.id.0).map(|_| ())?)).await
            }
            Step::SetAside { local } => {
                // Written ahead: a crash after the move into the version
                // store, before the record goes, finishes the job when the
                // record is settled (`Executor::recover`).
                store
                    .write(|o| {
                        let Some(b) = o.synced_get(sid)? else { return Err(Fail::Skip(Skip::Changed)) };
                        need(b.local == Some(local.id))?;
                        let p = Pending::Aside { sid, local: local.id };
                        Ok(o.meta_set(&pending_of(local.id), &p.encode())?)
                    })
                    .await?;
                self.crash_point()?;
                match self.off_task(a, |fs, a| fs.recycle(a, None)).await? {
                    Ok(Done::Recycled { .. }) => {}
                    Ok(_) => return Err(odd()),
                    Err(e) => return Err(e.into()),
                }
                self.crash_point()?;
                let id = local.id;
                store.write(|o| Ok::<_, Fail>(forget_aside(o, sid, id)?)).await
            }
            Step::Discard { local } => {
                // As a deletion made here: noted ahead (the change journal
                // sees only a move out of the folder), settled like a
                // recycling's if the move doesn't happen.
                store
                    .write(|o| {
                        let Some(b) = o.synced_get(sid)? else { return Err(Fail::Skip(Skip::Changed)) };
                        need(b.local == Some(local.id))?;
                        o.note_local_deleted(local.id.0)?;
                        Ok(o.meta_set(&pending_of(local.id), &Pending::Evidence(local.id).encode())?)
                    })
                    .await?;
                self.crash_point()?;
                match self.off_task(a, |fs, a| fs.recycle(a, None)).await? {
                    Ok(Done::Recycled { .. }) => {}
                    Ok(_) => return Err(odd()),
                    Err(e) => return Err(e.into()),
                }
                self.crash_point()?;
                let id = local.id;
                store
                    .write(|o| {
                        o.local_remove_subtree(id.0)?;
                        Ok::<_, Fail>(o.meta_delete(&pending_of(id))?)
                    })
                    .await
            }
            Step::Forget => {
                store
                    .write(|o| {
                        let Some(b) = o.synced_get(sid)? else { return Err(Fail::Skip(Skip::Changed)) };
                        // Only while this PC's copy is still gone.
                        if let Some(l) = b.local {
                            need(o.local_get(l.0)?.is_none())?;
                        }
                        o.synced_remove(sid)?;
                        Ok(())
                    })
                    .await
            }
        }
    }

    /// Puts file `row` back to the version of its history with `size` and
    /// `hash`, as a new version conditional on its eTag now: the version
    /// it replaces stays in the history. Each candidate is downloaded into
    /// the staging folder and checked by its hash before it's sent.
    async fn put_back(&self, store: &Db<'_>, row: &RemoteRow, size: i64, hash: &Hash) -> Res<RemoteRow> {
        let pre = pre_of(row);
        let parent = RemoteId(row.parent_id.clone().ok_or(Fail::Skip(Skip::Changed))?);
        let versions = self.cloud.versions(&pre.id).await?;
        // The first is the current version, which isn't the one sought.
        let candidates = versions.iter().skip(1).filter(|v| v.size.is_none_or(|s| s == size)).take(VERSION_TRIES);
        for v in candidates {
            let mut staged = self.fs.stage()?;
            let Some(file) = staged.file() else { return Err(odd()) };
            let mut hashing = Hashing::new(&mut *file);
            let len = match self.cloud.download_version(&pre.id, &v.id, &mut hashing).await {
                Ok(len) => len,
                Err(CloudError::Changed) => return Err(Fail::Skip(Skip::Changed)),
                Err(e) => return Err(e.into()),
            };
            if hashing.hash.finalize() != *hash || i64::try_from(len).ok() != Some(size) {
                continue;
            }
            file.rewind().map_err(|e| Fail::Failed(e.to_string()))?;
            let mut reader = OffTask::new(file.try_clone().map_err(|e| Fail::Failed(e.to_string()))?, &self.jobs);
            let (sent_row, sent) = self.cloud.upload(&parent, &row.name, Some(&pre), size, &mut reader).await?;
            let sent = sent.unwrap_or_else(|| reader.hash.finalize());
            store.write(|o| Ok::<_, Fail>(o.upsert_item(&sent_row, false)?)).await?;
            if sent_row.qxh != Some(*hash) || sent != *hash || sent_row.size != size {
                return Err(Fail::Skip(Skip::ContentMismatch));
            }
            return Ok(sent_row);
        }
        Err(Fail::Failed("that version is no longer in the file's version history in OneDrive".into()))
    }
}

/// The record of a step that set this PC's copy aside: its Local row and
/// its Synced record go, so OneDrive's copy comes back as a new file.
pub(super) fn forget_aside(o: &oneup_store::Ops<'_>, sid: Sid, local: oneup_sync::LocalId) -> oneup_store::Result<()> {
    o.local_remove_subtree(local.0)?;
    if o.synced_get(sid)?.is_some_and(|b| b.local == Some(local)) {
        o.synced_remove(sid)?;
    }
    o.meta_delete(&pending_of(local))
}

/// The state rewind step `op`, which just ran, left its item in, as the
/// store has it now (`oneup_sync::rewind::Job::ran`).
pub fn rewind_mark(o: &oneup_store::Ops<'_>, op: &Op) -> oneup_store::Result<Mark> {
    let id = match op.rewind_step() {
        Some(Step::Restore { id, .. }) => id,
        Some(Step::Reset { remote, .. }) => &remote.id,
        Some(Step::Remove { .. }) => return Ok(Mark::Absent),
        _ => return Ok(Mark::Record),
    };
    Ok(match o.get_item(&id.0)? {
        Some(row) => Mark::Cloud(Spot {
            parent: RemoteId(row.parent_id.unwrap_or_default()),
            name: row.name,
            size: row.size,
            hash: row.qxh,
        }),
        None => Mark::Absent,
    })
}

/// What this PC is to do to follow rewind step `op`, which just ran, bound
/// to the state it left the item in, as the store has it now.
pub fn rewind_follows(o: &oneup_store::Ops<'_>, op: &Op) -> oneup_store::Result<Vec<oneup_gate::Follow>> {
    use oneup_gate::Follow;
    let Op::Rewind { sid, step } = op else { return Ok(Vec::new()) };
    let mut out = Vec::new();
    match &**step {
        Step::Reset { remote, place, content, .. } => {
            let Some(row) = o.get_item(&remote.id.0)? else { return Ok(out) };
            if content.is_some() {
                let pre = oneup_sync::RemotePre { id: remote.id.clone(), etag: row.etag.clone().unwrap_or_default() };
                out.push(Follow::Download { sid: *sid, remote: pre });
            }
            if place.is_some()
                && let Some(parent) = row.parent_id.as_deref()
                && let Some(p) = o.synced_by_remote(parent)?
            {
                out.push(Follow::Move { sid: *sid, parent: p.sid, name: row.name.clone() });
            }
        }
        Step::Remove { .. } => {
            if let Some(b) = o.synced_get(*sid)?
                && let Some(l) = b.local
                && let Some(row) = o.local_get(l.0)?
            {
                let stamp = (row.kind == oneup_store::LocalKind::File).then_some((row.size, row.mtime));
                out.push(Follow::Recycle { sid: *sid, local: LocalPre { id: l, stamp } });
            }
        }
        _ => {}
    }
    Ok(out)
}

/// Whether `op` is a rewind step that sets a local copy aside (it writes a
/// record ahead).
pub(super) fn aside_of(op: &Op) -> Option<&LocalPre> {
    match op.rewind_step() {
        Some(Step::SetAside { local } | Step::Discard { local }) => Some(local),
        _ => None,
    }
}
