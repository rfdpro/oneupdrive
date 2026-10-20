//! Removing duplicate copies the user chose (`oneup duplicates remove`, the
//! settings window's Duplicates view).
//!
//! A choice names each copy as it was shown: a cloud file by ID and eTag, a
//! file only this PC has by ID, size and last-write time, each with the
//! content (size and QuickXorHash) its group had. [`plan`] checks every copy
//! against the store as it is now and turns each that still fits into one
//! [`Op::RemoveCopy`], which the safety gate holds as a stop of its own
//! until the user approves exactly that list (`oneup_gate::is_removal`),
//! whatever the circuit breakers' limits, and the executor runs with the
//! usual preconditions (`data-safety.md` invariants 2 and 5): OneDrive's copy
//! to its recycle bin under `If-Match`, then the copy here to the version
//! store, its content checked against the chosen hash under an exclusive
//! handle. Each only where the user was shown it: its folder and name, and
//! the full path above them ([`Up`]), on both sides. OneDrive refusing leaves the copy here untouched; the copy here
//! refusing once OneDrive's went (it changed) undoes the removal (it's
//! uploaded again) rather than leaving a deletion to finish without its
//! checks.
//!
//! * A copy that changed since it was shown (another eTag, size or time,
//!   another hash, gone, synced since) is skipped, never removed.
//! * A synced copy goes only while both sides are as they last synced, at
//!   the place they were synced (folder and name); otherwise the whole
//!   choice waits ([`Plan::wait`]).
//! * **The last copy of a content never goes.** Each group keeps a named
//!   survivor ([`Keep`]), a copy not being removed in the same data folder's
//!   own files: never one in a folder synced on its own (a vault, another
//!   folder pair, `separate`), whose agent could remove it meanwhile. Every
//!   removal carries it, and the executor checks it, as it is then (in
//!   OneDrive, by re-reading it), just before acting. A round whose plan
//!   also touches a copy being removed or its survivor (deletes, moves or
//!   replaces it) runs none of the removal ([`touched`]): it waits for a
//!   round that doesn't.

use oneup_store::{LocalKind, Ops};
use oneup_sync::{Base, Hash, Keep, LocalId, LocalPre, Op, RemoteId, RemotePre, Sid, Up, Vacates, fold};

/// A copy as the user was shown it.
#[derive(Debug, Clone, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub enum CopyId {
    /// A file in OneDrive (synced here or not).
    Cloud { id: RemoteId, etag: String },
    /// A file only this PC has.
    Local { id: LocalId, size: i64, mtime: i64 },
}

/// A copy chosen for removal, with the content its group had.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Chosen {
    pub copy: CopyId,
    pub size: i64,
    pub hash: Hash,
}

/// Why a chosen copy isn't removed.
#[derive(Debug, Clone, Copy, PartialEq, Eq, thiserror::Error)]
pub enum Kept {
    #[error("it's gone")]
    Gone,
    #[error("it changed since you chose")]
    Changed,
    #[error("it hasn't finished syncing")]
    NotInSync,
    #[error("it's the last copy here")]
    LastCopy,
    #[error("it's chosen twice")]
    Twice,
}

/// What removing a choice does now ([`plan`]).
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Plan {
    /// For the round, one per copy.
    pub ops: Vec<Op>,
    /// The chosen copies they remove, by index into the choice, each with
    /// its operation (in [`Plan::ops`] too).
    pub removed: Vec<(usize, Vec<Op>)>,
    /// The others, by index, and why.
    pub kept: Vec<(usize, Kept)>,
    /// A synced copy isn't as it last synced (a change on its way): the
    /// whole choice waits rather than runs in part.
    pub wait: bool,
}

impl Plan {
    /// The code its safety stop will show if nothing changes meanwhile
    /// (16 hex digits), each operation keyed with the path its list shows
    /// for it (`paths`, one per operation, as the caller's stop lists
    /// describe them).
    pub fn code(&self, paths: &[String]) -> String {
        let none = String::new();
        let items: Vec<(&Op, &str)> =
            self.ops.iter().enumerate().map(|(i, o)| (o, paths.get(i).unwrap_or(&none).as_str())).collect();
        format!("{:016x}", oneup_gate::removal_fingerprint(&items).0)
    }
}

/// `path` is OneDrive folder `folder` or inside it.
fn under(path: &str, folder: &str) -> bool {
    let f = folder.trim_end_matches('/');
    f.is_empty()
        || oneup_sync::strip_folded_prefix(path, f).is_some_and(|rest| rest.is_empty() || rest.starts_with('/'))
}

/// A removal before its survivor is known.
struct Draft {
    sid: Option<Sid>,
    remote: Option<RemotePre>,
    remote_dir: Option<RemoteId>,
    remote_up: Up<RemoteId>,
    local: Option<LocalPre>,
    local_dir: Option<LocalId>,
    local_up: Up<LocalId>,
    name: String,
}

/// What one chosen copy turns into.
enum Fits {
    Go(Box<Draft>),
    Kept(Kept),
}

/// At most this many copies of a content are looked at for a survivor.
const SURVIVORS_LOOKED_AT: usize = 1024;

/// Works out what removing `chosen` does now, changing nothing. `separate`:
/// the OneDrive folders synced on their own (vaults, other folder pairs),
/// whose copies never count as survivors.
pub fn plan(o: &Ops<'_>, chosen: &[Chosen], separate: &[String]) -> oneup_store::Result<Plan> {
    let mut fits: Vec<Fits> = Vec::with_capacity(chosen.len());
    let mut wait = false;
    for (i, c) in chosen.iter().enumerate() {
        if chosen[..i].iter().any(|d| d.copy == c.copy) {
            fits.push(Fits::Kept(Kept::Twice));
            continue;
        }
        let f = check(o, c)?;
        wait |= matches!(f, Fits::Kept(Kept::NotInSync));
        fits.push(f);
    }
    // A survivor per content: a copy not going, in this data folder's own
    // files, as it is now.
    let mut keeps: Vec<((i64, Hash), Option<Keep>)> = Vec::new();
    for (c, f) in chosen.iter().zip(&fits) {
        let content = (c.size, c.hash);
        if !matches!(f, Fits::Go(_)) || keeps.iter().any(|(k, _)| *k == content) {
            continue;
        }
        let going: Vec<&CopyId> = chosen
            .iter()
            .zip(&fits)
            .filter(|(d, f)| (d.size, d.hash) == content && matches!(f, Fits::Go(_)))
            .map(|(d, _)| &d.copy)
            .collect();
        let n = going.len() + SURVIVORS_LOOKED_AT;
        let mut keep = None;
        for r in o.dup_cloud_copies(c.size, &c.hash, n)? {
            if going.iter().any(|g| matches!(g, CopyId::Cloud { id, .. } if id.0 == r.id)) {
                continue;
            }
            let path = o.path_of(&r.id)?.unwrap_or_default();
            if separate.iter().any(|f| under(&path, f)) {
                continue;
            }
            if let Some(etag) = r.etag {
                keep = Some(Keep::Remote(RemotePre { id: RemoteId(r.id), etag }));
                break;
            }
        }
        if keep.is_none() {
            for l in o.dup_local_copies(c.size, &c.hash, n)? {
                if !going.iter().any(|g| matches!(g, CopyId::Local { id, .. } if id.0 == l.file_id)) {
                    keep = Some(Keep::Local(LocalPre { id: LocalId(l.file_id), stamp: Some((l.size, l.mtime)) }));
                    break;
                }
            }
        }
        keeps.push((content, keep));
    }
    let mut out = Plan { wait, ..Plan::default() };
    for (i, (c, f)) in chosen.iter().zip(fits).enumerate() {
        match f {
            Fits::Go(d) => match keeps.iter().find(|(k, _)| *k == (c.size, c.hash)).and_then(|(_, k)| k.clone()) {
                Some(keep) => {
                    let op = Op::RemoveCopy {
                        sid: d.sid,
                        remote: d.remote,
                        remote_dir: d.remote_dir,
                        remote_up: d.remote_up,
                        local: d.local,
                        local_dir: d.local_dir,
                        local_up: d.local_up,
                        name: d.name,
                        hash: c.hash,
                        keep,
                    };
                    out.ops.push(op.clone());
                    out.removed.push((i, vec![op]));
                }
                None => out.kept.push((i, Kept::LastCopy)),
            },
            Fits::Kept(k) => out.kept.push((i, k)),
        }
    }
    Ok(out)
}

/// Whether a round whose plan is `planned` touches what `removal` (its
/// [`Op::RemoveCopy`]s) removes or keeps: any of its operations is about
/// one of those items, or about a folder a copy being removed is in, at
/// any depth (moving it would move the copy off the path shown). Such a
/// round runs none of the removal.
pub fn touched(planned: &[Op], removal: &[Op]) -> bool {
    let mut sids: Vec<Sid> = Vec::new();
    let mut items: Vec<Vacates> = Vec::new();
    for op in removal {
        if let Op::RemoveCopy { sid, remote, remote_up, local, local_up, keep, .. } = op {
            sids.extend(*sid);
            items.extend(remote.as_ref().map(|r| Vacates::Remote(r.id.clone())));
            items.extend(local.as_ref().map(|l| Vacates::Local(l.id)));
            items.extend(remote_up.ids().map(|id| Vacates::Remote(id.clone())));
            items.extend(local_up.ids().map(|id| Vacates::Local(*id)));
            items.push(keep.item());
        }
    }
    planned.iter().any(|op| {
        let (sid, local, remote) = op.subjects();
        sid.is_some_and(|s| sids.contains(&s))
            || local.is_some_and(|l| items.contains(&Vacates::Local(l)))
            || remote.is_some_and(|r| items.contains(&Vacates::Remote(r)))
    })
}

/// Whether the copy that stays is still as chosen, in the store: the cloud
/// file with that eTag and the content, or the file here with that stamp
/// and the content recorded for it.
pub fn keep_stands(o: &Ops<'_>, keep: &Keep, hash: &Hash) -> oneup_store::Result<bool> {
    Ok(match keep {
        Keep::Remote(k) => o.get_item(&k.id.0)?.is_some_and(|r| {
            r.kind == oneup_store::Kind::File && r.etag.as_deref() == Some(k.etag.as_str()) && r.qxh == Some(*hash)
        }),
        Keep::Local(k) => {
            let Some((size, mtime)) = k.stamp else { return Ok(false) };
            match o.local_get(k.id.0)? {
                Some(l) if l.kind == LocalKind::File && l.size == size && l.mtime == mtime => {
                    o.local_only_hash_get(k.id.0, size, mtime)? == Some(*hash)
                        || o.synced_by_local(k.id.0)?.is_some_and(|b| b.hash == Some(*hash) && b.local_mtime == mtime)
                }
                _ => false,
            }
        }
    })
}

/// Where synced item `b` is, if it's still where it was synced on both
/// sides: in its agreed folder, under its agreed name (as Windows and
/// OneDrive compare names), here and in OneDrive. Both must be there.
/// Returns the folder on each side.
pub fn synced_place(o: &Ops<'_>, b: &Base) -> oneup_store::Result<Option<(LocalId, RemoteId)>> {
    let Some(pb) = b.parent.map(|p| o.synced_get(p)).transpose()?.flatten() else { return Ok(None) };
    let (Some(l), Some(r), Some(pl), Some(pr)) = (b.local, &b.remote, pb.local, &pb.remote) else { return Ok(None) };
    let here = o.local_get(l.0)?.is_some_and(|row| row.parent_id == Some(pl.0) && fold(&row.name) == fold(&b.name));
    let there = o
        .get_item(&r.0)?
        .is_some_and(|row| row.parent_id.as_deref() == Some(pr.0.as_str()) && fold(&row.name) == fold(&b.name));
    Ok((here && there).then(|| (pl, pr.clone())))
}

/// The folders OneDrive folder `dir` is (it first) up to the drive's root,
/// in the store; `None` if that chain is broken (a folder missing, not a
/// folder, or no root above).
pub fn remote_up(o: &Ops<'_>, dir: &RemoteId) -> oneup_store::Result<Option<Up<RemoteId>>> {
    let mut up = Vec::new();
    let mut cur = dir.0.clone();
    for _ in 0..MAX_DEPTH {
        let Some(row) = o.get_item(&cur)? else { return Ok(None) };
        match (row.kind, row.parent_id) {
            (oneup_store::Kind::Root, _) => return Ok(Some(Up(up))),
            (oneup_store::Kind::Folder, Some(p)) => {
                up.push((RemoteId(cur), row.name));
                cur = p;
            }
            _ => return Ok(None),
        }
    }
    Ok(None)
}

/// [`remote_up`] here: folder `dir` up to the synced folder.
pub fn local_up(o: &Ops<'_>, dir: LocalId) -> oneup_store::Result<Option<Up<LocalId>>> {
    let mut up = Vec::new();
    let mut cur = dir;
    for _ in 0..MAX_DEPTH {
        let Some(row) = o.local_get(cur.0)? else { return Ok(None) };
        match (row.kind, row.parent_id) {
            (LocalKind::Root, _) => return Ok(Some(Up(up))),
            (LocalKind::Folder, Some(p)) => {
                up.push((cur, row.name));
                cur = LocalId(p);
            }
            _ => return Ok(None),
        }
    }
    Ok(None)
}

/// Deeper than this, a chain of folders is taken as broken.
const MAX_DEPTH: usize = 4096;

/// One copy against the store now.
fn check(o: &Ops<'_>, c: &Chosen) -> oneup_store::Result<Fits> {
    match &c.copy {
        CopyId::Cloud { id, etag } => {
            let Some(row) = o.get_item(&id.0)? else { return Ok(Fits::Kept(Kept::Gone)) };
            let same = row.kind == oneup_store::Kind::File
                && row.etag.as_deref() == Some(etag.as_str())
                && row.size == c.size
                && row.qxh == Some(c.hash);
            if !same {
                return Ok(Fits::Kept(Kept::Changed));
            }
            let remote = Some(RemotePre { id: id.clone(), etag: etag.clone() });
            let Some(b) = o.synced_by_remote(&id.0)? else {
                let remote_dir = row.parent_id.map(RemoteId);
                let Some(remote_up) = remote_dir.as_ref().map(|d| remote_up(o, d)).transpose()?.flatten() else {
                    return Ok(Fits::Kept(Kept::Changed));
                };
                return Ok(Fits::Go(Box::new(Draft {
                    sid: None,
                    remote,
                    remote_dir,
                    remote_up,
                    local: None,
                    local_dir: None,
                    local_up: Up::default(),
                    name: row.name,
                })));
            };
            // Synced: both sides go, and only as and where they last agreed
            // (else the planner has a change to carry first, and the copy
            // here may be other content).
            let place = if b.remote_etag == *etag { synced_place(o, &b)? } else { None };
            let Some((local_dir, remote_dir)) = place else { return Ok(Fits::Kept(Kept::NotInSync)) };
            let here = match b.local {
                Some(l) => o.local_get(l.0)?,
                None => None,
            };
            let local = match here {
                Some(l) if l.kind == LocalKind::File && l.size == b.size && l.mtime == b.local_mtime => {
                    LocalPre { id: LocalId(l.file_id), stamp: Some((l.size, l.mtime)) }
                }
                _ => return Ok(Fits::Kept(Kept::NotInSync)),
            };
            let (Some(remote_up), Some(local_up)) = (remote_up(o, &remote_dir)?, local_up(o, local_dir)?) else {
                return Ok(Fits::Kept(Kept::Changed));
            };
            Ok(Fits::Go(Box::new(Draft {
                sid: Some(b.sid),
                remote,
                remote_dir: Some(remote_dir),
                remote_up,
                local: Some(local),
                local_dir: Some(local_dir),
                local_up,
                name: b.name,
            })))
        }
        CopyId::Local { id, size, mtime } => {
            let Some(l) = o.local_get(id.0)? else { return Ok(Fits::Kept(Kept::Gone)) };
            let same = l.kind == LocalKind::File
                && l.size == *size
                && l.mtime == *mtime
                && *size == c.size
                && o.synced_by_local(id.0)?.is_none()
                && o.local_only_hash_get(id.0, *size, *mtime)? == Some(c.hash);
            if !same {
                return Ok(Fits::Kept(Kept::Changed));
            }
            let local = Some(LocalPre { id: *id, stamp: Some((*size, *mtime)) });
            let local_dir = l.parent_id.map(LocalId);
            let Some(local_up) = local_dir.map(|d| local_up(o, d)).transpose()?.flatten() else {
                return Ok(Fits::Kept(Kept::Changed));
            };
            Ok(Fits::Go(Box::new(Draft {
                sid: None,
                remote: None,
                remote_dir: None,
                remote_up: Up::default(),
                local,
                local_dir,
                local_up,
                name: l.name,
            })))
        }
    }
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used)]
    use super::*;
    use oneup_store::{Kind, LocalRow, RemoteRow, Skip, Store};

    const MIB: i64 = 1 << 20;

    fn cloud(id: &str, parent: &str, h: u8) -> RemoteRow {
        RemoteRow {
            id: id.into(),
            parent_id: Some(parent.into()),
            name: format!("{id}.bin"),
            kind: Kind::File,
            size: 2 * MIB,
            etag: Some(format!("e-{id}")),
            qxh: Some([h; 20]),
            mtime_ms: None,
        }
    }

    fn chosen_cloud(id: &str, h: u8) -> Chosen {
        Chosen {
            copy: CopyId::Cloud { id: RemoteId(id.into()), etag: format!("e-{id}") },
            size: 2 * MIB,
            hash: [h; 20],
        }
    }

    /// `a`, `b` and `c` at the top, `v` in `/Vault` (a folder synced on its own).
    fn store() -> Store {
        let mut s = Store::open_in_memory().unwrap();
        s.write(|o| {
            let folder = |id: &str, parent: Option<&str>, name: &str, kind| RemoteRow {
                id: id.into(),
                parent_id: parent.map(Into::into),
                name: name.into(),
                kind,
                size: 0,
                etag: Some("e".into()),
                qxh: None,
                mtime_ms: None,
            };
            o.upsert_item(&folder("root", None, "", Kind::Root), false)?;
            o.upsert_item(&folder("vault", Some("root"), "Vault", Kind::Folder), false)?;
            for id in ["a", "b", "c"] {
                o.upsert_item(&cloud(id, "root", 1), false)?;
            }
            o.upsert_item(&cloud("v", "vault", 1), false)?;
            o.local_put(&LocalRow {
                file_id: 1,
                parent_id: None,
                name: String::new(),
                name_lossy: false,
                kind: LocalKind::Root,
                size: 0,
                mtime: 0,
                attributes: 0,
                reparse_tag: 0,
                skip: Skip::None,
            })
        })
        .unwrap();
        s
    }

    fn keep_of(op: &Op) -> &Keep {
        match op {
            Op::RemoveCopy { keep, .. } => keep,
            _ => panic!("{op:?}"),
        }
    }

    #[test]
    fn the_last_copy_of_a_content_is_never_removed() {
        let s = store();
        let two = [chosen_cloud("a", 1), chosen_cloud("b", 1)];
        let p = s.read(|o| plan(o, &two, &[])).unwrap();
        assert_eq!(p.removed.iter().map(|(i, _)| *i).collect::<Vec<_>>(), [0, 1]);
        assert!(
            p.ops
                .iter()
                .all(|op| keep_of(op) == &Keep::Remote(RemotePre { id: RemoteId("c".into()), etag: "e-c".into() }))
        );
        let all = [chosen_cloud("a", 1), chosen_cloud("b", 1), chosen_cloud("c", 1)];
        let p = s.read(|o| plan(o, &all, &["/Vault".to_string()])).unwrap();
        assert!(p.ops.is_empty());
        assert_eq!(p.kept, [(0, Kept::LastCopy), (1, Kept::LastCopy), (2, Kept::LastCopy)]);
    }

    /// A copy in a folder synced on its own (a vault, another pair) never
    /// counts as the one that stays: its own agent could remove it.
    #[test]
    fn a_copy_another_pair_syncs_is_never_the_survivor() {
        let s = store();
        let all = [chosen_cloud("a", 1), chosen_cloud("b", 1), chosen_cloud("c", 1)];
        let p = s.read(|o| plan(o, &all, &[])).unwrap();
        assert_eq!(p.removed.len(), 3, "without the vault known, its copy would stay");
        assert_eq!(keep_of(&p.ops[0]), &Keep::Remote(RemotePre { id: RemoteId("v".into()), etag: "e-v".into() }));
        let p = s.read(|o| plan(o, &all, &["/vault".to_string()])).unwrap();
        assert!(p.ops.is_empty(), "the vault's copy doesn't count");
    }

    #[test]
    fn a_copy_that_changed_since_it_was_shown_is_kept() {
        let mut s = store();
        let two = [chosen_cloud("a", 1), chosen_cloud("b", 1)];
        let shown = s.read(|o| plan(o, &two, &[])).unwrap();
        s.write(|o| o.upsert_item(&RemoteRow { etag: Some("e-a2".into()), ..cloud("a", "root", 1) }, false)).unwrap();
        let p = s.read(|o| plan(o, &two, &[])).unwrap();
        assert_eq!(p.removed.iter().map(|(i, _)| *i).collect::<Vec<_>>(), [1]);
        assert_eq!(p.kept, [(0, Kept::Changed)]);
        assert_ne!(p.code(&[]), shown.code(&[]), "another batch, another code");
    }

    /// A round whose plan touches a copy being removed, or the one that
    /// stays, runs none of the removal.
    #[test]
    fn a_round_touching_a_removal_or_its_survivor_defers_it() {
        let s = store();
        let p = s.read(|o| plan(o, &[chosen_cloud("a", 1)], &[])).unwrap();
        let survivor = match keep_of(&p.ops[0]) {
            Keep::Remote(r) => r.clone(),
            Keep::Local(_) => unreachable!(),
        };
        let delete = |r: RemotePre| Op::DeleteRemote { sid: 9, remote: r };
        assert!(touched(&[delete(survivor)], &p.ops), "the survivor deleted by the same round");
        let removed = RemotePre { id: RemoteId("a".into()), etag: "x".into() };
        assert!(touched(&[delete(removed)], &p.ops), "the copy itself changed by the same round");
        let other = RemotePre { id: RemoteId("zzz".into()), etag: "x".into() };
        assert!(!touched(&[delete(other)], &p.ops));
    }

    /// A folder above a copy being removed, moved or deleted by the same
    /// round, would take the copy off the path shown: that round runs none
    /// of the removal.
    #[test]
    fn a_round_touching_a_folder_above_a_removal_defers_it() {
        let s = store();
        let p = s.read(|o| plan(o, &[chosen_cloud("v", 1)], &[])).unwrap();
        let Op::RemoveCopy { remote_up, name, .. } = &p.ops[0] else { unreachable!() };
        assert_eq!(remote_up.path(name), "/Vault/v.bin", "the full path shown");
        let vault = RemotePre { id: RemoteId("vault".into()), etag: "e".into() };
        let moved =
            Op::MoveRemote { sid: 9, remote: vault, to: RemoteId("root".into()), parent: 1, name: "Old".into() };
        assert!(touched(&[moved], &p.ops));
    }
}
