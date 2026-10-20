//! Moves written ahead, for clouds whose moves can change an item's ID
//! ([`crate::Cloud::moves_keep_ids`]: a vault, which rebuilds an entry whose
//! stored name changes shape under a new ID).
//!
//! The executor records the new ID once the move is done. A crash, or a
//! power cut, between the move and that record would leave the Synced row
//! on the old ID, which the next listing reports gone: the next plan would
//! read the user's move as one side's deletion, and could recycle the item
//! here (a folder whose move it undoes) or duplicate it. So the move is
//! written ahead, durably, under `exec.moving.<sid>` ([`MoveIntent`]), and
//! the record that follows the move clears it. One left over is settled
//! before the next plan ([`settle_moves`]), once the listing is current:
//!
//! * the Synced row already follows another ID, or is gone: nothing to do;
//! * the old item is still listed: the move didn't happen, and the next
//!   plan makes it again;
//! * the old item is gone and exactly one item where the move took it is
//!   provably the one moved: the Synced row is relinked to it, at the place
//!   the move recorded, as the executor would have done (a file's
//!   placeholder identity included). For a file, the one item there of that
//!   name (as names compare) linked to nothing with the Synced row's size
//!   and hash, and none whose hash isn't listed; for a folder, the one folder there of
//!   that name, none linked. A vault folder can hold names that differ
//!   only in case, so a name alone proves nothing;
//! * otherwise the old ID's removal is no evidence of a deletion
//!   (`data-safety.md` invariant 3): the evidence is withdrawn, so the item
//!   reads as missing, which holds it. With no candidate at all the record
//!   goes (the hold stays, for the user to settle); with several, or a file
//!   whose hash isn't listed, it stays for the next round, whose listing
//!   may tell them apart.

use oneup_store::{Kind, LocalKind, Ops, RemoteRow, Result};
use oneup_sync::{LocalId, LocalPre, NodeKind, RemoteId, Sid};

/// Where moves written ahead are kept: `exec.moving.<sid>`.
pub(crate) const MOVING: &str = "exec.moving";

/// The key of Synced item `sid`'s move written ahead.
pub(crate) fn moving_of(sid: Sid) -> String {
    format!("{MOVING}.{sid}")
}

/// A move of Synced item `sid`, cloud item `old`, into cloud folder `to`
/// (Synced folder `parent`) as `name`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct MoveIntent {
    pub sid: Sid,
    pub old: String,
    pub to: String,
    pub parent: Sid,
    pub name: String,
}

impl MoveIntent {
    /// Cloud IDs have no spaces; the name, which may, goes last.
    pub fn encode(&self) -> String {
        format!("{} {} {} {} {}", self.sid, self.old, self.to, self.parent, self.name)
    }

    pub fn decode(s: &str) -> Option<Self> {
        let mut it = s.splitn(5, ' ');
        Some(Self {
            sid: it.next()?.parse().ok()?,
            old: it.next()?.to_string(),
            to: it.next()?.to_string(),
            parent: it.next()?.parse().ok()?,
            name: it.next()?.to_string(),
        })
    }
}

/// What [`settle_moves`] did.
#[derive(Debug, Default, Clone, PartialEq, Eq)]
pub struct Settled {
    /// Synced rows relinked to the item their move made.
    pub relinked: usize,
    /// Of those, the files here, with the item each now names: an online-only
    /// file's placeholder identity follows it, as after a move recorded at
    /// once (`Executor::settle_moves`).
    pub files: Vec<(LocalPre, RemoteRow)>,
    /// Moves with nothing where they went that could be their item: held
    /// for the user, the record dropped.
    pub held: usize,
    /// Moves with more than one item that could be theirs: held, and tried
    /// again next round.
    pub waiting: usize,
}

/// Settles the moves a round cut short left (see the module docs), in the
/// store only: callers go through `Executor::settle_moves`, which also
/// renews the placeholders of the files relinked. Call it once the Remote
/// tree is current and before planning. Costs a few indexed lookups per
/// record left over (the destination's items of that name only): none in a
/// round that wasn't cut short.
pub(crate) fn settle_moves(o: &Ops<'_>) -> Result<Settled> {
    let mut out = Settled::default();
    for (key, value) in o.meta_under(MOVING)? {
        let Some(m) = MoveIntent::decode(&value) else {
            o.meta_delete(&key)?;
            continue;
        };
        let Some(b) = o.synced_get(m.sid)?.filter(|b| b.remote.as_ref().is_some_and(|r| r.0 == m.old)) else {
            o.meta_delete(&key)?;
            continue;
        };
        if o.get_item(&m.old)?.is_some() {
            o.meta_delete(&key)?;
            continue;
        }
        let kind = match b.kind {
            NodeKind::File => Kind::File,
            NodeKind::Folder => Kind::Folder,
        };
        let (mut unlinked, mut linked) = (Vec::new(), 0usize);
        for r in o.remote_named(&m.to, &m.name)? {
            if r.id == m.old || r.kind != kind {
                continue;
            }
            if o.synced_by_remote(&r.id)?.is_some() {
                linked += 1;
            } else {
                unlinked.push(r);
            }
        }
        // What could be the item moved, and whether it's proved to be. A
        // file without a hash on either side can't be told apart (Graph
        // sometimes omits one): it stays a candidate until a listing has it.
        let (candidates, proved) = match b.kind {
            NodeKind::File => {
                let (unproven, hashed): (Vec<_>, Vec<_>) =
                    unlinked.into_iter().partition(|r| b.hash.is_none() || r.qxh.is_none());
                let mut same: Vec<_> = hashed.into_iter().filter(|r| r.qxh == b.hash && r.size == b.size).collect();
                let proved = same.len() == 1 && unproven.is_empty();
                same.extend(unproven);
                (same, proved)
            }
            NodeKind::Folder => {
                let proved = unlinked.len() == 1 && linked == 0;
                (unlinked, proved)
            }
        };
        if proved && let Some(r) = candidates.first().cloned() {
            tracing::debug!(sid = m.sid, "a move cut short is recorded where it went");
            // Its eTag stays the one last agreed: the planner compares
            // content by hash, and without one an eTag that differs costs
            // a download recognised as identical.
            if let Some(l) = b.local
                && let Some(row) = o.local_get(l.0)?
                && row.kind == LocalKind::File
            {
                out.files.push((LocalPre { id: LocalId(row.file_id), stamp: Some((row.size, row.mtime)) }, r.clone()));
            }
            let parent = Some(m.parent);
            o.synced_put(&oneup_sync::Base { remote: Some(RemoteId(r.id)), parent, name: m.name, ..b })?;
            o.meta_delete(&key)?;
            out.relinked += 1;
            continue;
        }
        o.unnote_remote_deleted(&m.old)?;
        if candidates.is_empty() {
            tracing::debug!(sid = m.sid, "a move cut short isn't found where it went: held");
            o.meta_delete(&key)?;
            out.held += 1;
        } else {
            tracing::debug!(sid = m.sid, "a move cut short can't be told apart where it went: held for now");
            out.waiting += 1;
        }
    }
    Ok(out)
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used)]
    use super::*;
    use oneup_store::Store;
    use oneup_sync::{Base, ROOT};

    const H: [u8; 20] = [7; 20];

    fn row(id: &str, parent: &str, name: &str, kind: Kind) -> RemoteRow {
        let file = kind == Kind::File;
        RemoteRow {
            id: id.into(),
            parent_id: Some(parent.into()),
            name: name.into(),
            kind,
            size: if file { 5 } else { 0 },
            etag: Some("e".into()),
            qxh: file.then_some(H),
            mtime_ms: None,
        }
    }

    fn base(sid: Sid, parent: Option<Sid>, name: &str, kind: NodeKind, remote: &str) -> Base {
        let file = kind == NodeKind::File;
        Base {
            sid,
            parent,
            name: name.into(),
            kind,
            size: if file { 5 } else { 0 },
            hash: file.then_some(H),
            local: Some(LocalId(u128::from(sid) + 100)),
            local_mtime: 0,
            remote: Some(RemoteId(remote.into())),
            remote_etag: "e".into(),
        }
    }

    fn kind_of(kind: NodeKind) -> Kind {
        if kind == NodeKind::File { Kind::File } else { Kind::Folder }
    }

    /// The root `root`, folders `a` and `b`, and in `a` item 3, `x.txt`,
    /// with the move of item 3 into `b` as `a long name.txt` cut short: the
    /// listing since has the old entry gone (with evidence).
    fn store(kind: NodeKind) -> Store {
        let mut s = Store::open_in_memory().unwrap();
        s.write(|o| {
            o.upsert_item(&RemoteRow { parent_id: None, kind: Kind::Root, ..row("root", "", "", Kind::Root) }, false)?;
            o.upsert_item(&row("a", "root", "a", Kind::Folder), false)?;
            o.upsert_item(&row("b", "root", "b", Kind::Folder), false)?;
            o.upsert_item(&row("old", "a", "x.txt", kind_of(kind)), false)?;
            o.synced_put(&base(ROOT, None, "", NodeKind::Folder, "root"))?;
            o.synced_put(&base(1, Some(ROOT), "a", NodeKind::Folder, "a"))?;
            o.synced_put(&base(2, Some(ROOT), "b", NodeKind::Folder, "b"))?;
            o.synced_put(&base(3, Some(1), "x.txt", kind, "old"))?;
            o.meta_set(&moving_of(3), &intent().encode())?;
            o.delete_subtree("old")?;
            Ok(())
        })
        .unwrap();
        assert!(noted_gone(&s, "old"), "the listing noted it deleted");
        s
    }

    fn intent() -> MoveIntent {
        MoveIntent { sid: 3, old: "old".into(), to: "b".into(), parent: 2, name: "a long name.txt".into() }
    }

    /// Whether the store holds deletion evidence for cloud item `id`.
    fn noted_gone(s: &Store, id: &str) -> bool {
        use oneup_sync::{Seen, Trees};
        matches!(s.read(|o| o.remote(&RemoteId(id.into()))).unwrap(), Seen::Deleted)
    }

    fn left(s: &Store) -> Vec<(String, String)> {
        s.read(|o| o.meta_under(MOVING)).unwrap()
    }

    fn remote_of(s: &Store, sid: Sid) -> String {
        s.read(|o| o.synced_get(sid)).unwrap().unwrap().remote.unwrap().0
    }

    #[test]
    fn an_intent_round_trips_with_spaces_in_the_name() {
        let m = intent();
        assert_eq!(MoveIntent::decode(&m.encode()), Some(m));
        assert_eq!(MoveIntent::decode("3 old"), None);
    }

    /// The move was made, the record wasn't: the Synced row follows the
    /// new ID, at the new place, and the old ID's removal is no deletion.
    #[test]
    fn a_move_made_but_not_recorded_is_relinked() {
        for kind in [NodeKind::File, NodeKind::Folder] {
            let mut s = store(kind);
            s.write(|o| o.upsert_item(&row("new", "b", "A Long Name.txt", kind_of(kind)), false)).unwrap();
            assert_eq!(s.write(settle_moves).unwrap(), Settled { relinked: 1, ..Settled::default() });
            let b = s.read(|o| o.synced_get(3)).unwrap().unwrap();
            assert_eq!(b.remote, Some(RemoteId("new".into())));
            assert_eq!((b.parent, b.name.as_str()), (Some(2), "a long name.txt"));
            assert_eq!(b.remote_etag, "e");
            assert!(!noted_gone(&s, "old"));
            assert!(s.read(|o| o.check_invariants()).unwrap().is_empty());
            assert!(left(&s).is_empty());
        }
    }

    /// Two names there that differ only in case, one already linked: the
    /// file is the one with its content, whichever is listed first; a
    /// folder can't be told apart, so it's held and tried again.
    #[test]
    fn names_that_differ_in_case_are_told_apart_by_content_or_not_at_all() {
        for kind in [NodeKind::File, NodeKind::Folder] {
            let mut s = store(kind);
            s.write(|o| {
                // Linked already: another item of that name.
                let other = RemoteRow { qxh: Some([8; 20]), ..row("linked", "b", "a LONG name.txt", kind_of(kind)) };
                let other = if kind == NodeKind::File { other } else { RemoteRow { qxh: None, ..other } };
                o.upsert_item(&other, false)?;
                o.synced_put(&base(4, Some(2), "a LONG name.txt", kind, "linked"))?;
                o.upsert_item(&row("new", "b", "A Long Name.txt", kind_of(kind)), false)
            })
            .unwrap();
            let done = s.write(settle_moves).unwrap();
            if kind == NodeKind::File {
                assert_eq!(done, Settled { relinked: 1, ..Settled::default() });
                assert_eq!(remote_of(&s, 3), "new");
            } else {
                assert_eq!(done, Settled { waiting: 1, ..Settled::default() });
                assert_eq!(remote_of(&s, 3), "old");
                assert!(!noted_gone(&s, "old"), "held, not deleted");
                assert_eq!(left(&s).len(), 1, "tried again next round");
            }
            assert_eq!(remote_of(&s, 4), "linked");
        }
    }

    /// Two unlinked files of that name with its content: not told apart.
    #[test]
    fn two_files_with_its_content_are_not_told_apart() {
        let mut s = store(NodeKind::File);
        s.write(|o| {
            o.upsert_item(&row("new", "b", "A Long Name.txt", Kind::File), false)?;
            o.upsert_item(&row("twin", "b", "a long name.TXT", Kind::File), false)
        })
        .unwrap();
        assert_eq!(s.write(settle_moves).unwrap(), Settled { waiting: 1, ..Settled::default() });
        assert_eq!(remote_of(&s, 3), "old");
        assert!(!noted_gone(&s, "old"));
    }

    /// A file of that name with other content isn't it (nor is a folder):
    /// held for the user, and the record goes.
    #[test]
    fn a_file_of_that_name_with_other_content_is_not_it() {
        let mut s = store(NodeKind::File);
        s.write(|o| {
            o.upsert_item(
                &RemoteRow { qxh: Some([8; 20]), ..row("other", "b", "a long name.txt", Kind::File) },
                false,
            )?;
            o.upsert_item(&row("folder", "b", "A long name.txt", Kind::Folder), false)
        })
        .unwrap();
        assert_eq!(s.write(settle_moves).unwrap(), Settled { held: 1, ..Settled::default() });
        assert_eq!(remote_of(&s, 3), "old");
        assert!(!noted_gone(&s, "old"), "held, not deleted");
        assert!(left(&s).is_empty(), "not looked for again");
    }

    /// A same-name file whose listing has no hash may be it: held, and
    /// tried again, until a listing has its hash.
    #[test]
    fn a_file_without_a_hash_waits_for_one() {
        let mut s = store(NodeKind::File);
        s.write(|o| o.upsert_item(&RemoteRow { qxh: None, ..row("new", "b", "a long name.txt", Kind::File) }, false))
            .unwrap();
        assert_eq!(s.write(settle_moves).unwrap(), Settled { waiting: 1, ..Settled::default() });
        assert_eq!(remote_of(&s, 3), "old");
        assert_eq!(left(&s).len(), 1);
        // Also next to one that is proved: two that could be it.
        s.write(|o| o.upsert_item(&row("twin", "b", "A long name.txt", Kind::File), false)).unwrap();
        assert_eq!(s.write(settle_moves).unwrap(), Settled { waiting: 1, ..Settled::default() });
        // Its hash listed (other content): the proved one is it.
        s.write(|o| {
            o.upsert_item(&RemoteRow { qxh: Some([8; 20]), ..row("new", "b", "a long name.txt", Kind::File) }, false)
        })
        .unwrap();
        assert_eq!(s.write(settle_moves).unwrap(), Settled { relinked: 1, ..Settled::default() });
        assert_eq!(remote_of(&s, 3), "twin");
    }

    /// Nothing there at all: held, and the record goes.
    #[test]
    fn a_move_whose_item_isnt_there_holds_it() {
        let mut s = store(NodeKind::Folder);
        assert_eq!(s.write(settle_moves).unwrap(), Settled { held: 1, ..Settled::default() });
        assert!(!noted_gone(&s, "old"));
        assert!(left(&s).is_empty());
    }

    /// The move didn't happen: the record goes and the Synced row stays,
    /// so the next plan makes the move again.
    #[test]
    fn a_move_not_made_is_dropped() {
        let mut s = store(NodeKind::File);
        s.write(|o| o.upsert_item(&row("old", "a", "x.txt", Kind::File), false)).unwrap();
        assert_eq!(s.write(settle_moves).unwrap(), Settled::default());
        assert_eq!(remote_of(&s, 3), "old");
        assert!(left(&s).is_empty());
    }

    /// Recorded already (the row follows another ID), or forgotten: the
    /// record just goes.
    #[test]
    fn a_move_recorded_or_forgotten_is_dropped() {
        let mut s = store(NodeKind::File);
        s.write(|o| {
            o.meta_set(&moving_of(9), &MoveIntent { sid: 9, ..intent() }.encode())?;
            o.meta_set(&format!("{MOVING}.x"), "nonsense")?;
            o.upsert_item(&row("new", "b", "a long name.txt", Kind::File), false)?;
            let b = o.synced_get(3)?.unwrap();
            o.synced_put(&Base { remote: Some(RemoteId("new".into())), ..b })
        })
        .unwrap();
        assert_eq!(s.write(settle_moves).unwrap(), Settled::default());
        assert!(left(&s).is_empty());
    }
}
