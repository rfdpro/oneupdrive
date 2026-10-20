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
//! * the old item is gone and an item of the same kind, linked to nothing,
//!   is listed where the move took it: the Synced row is relinked to it,
//!   at the place the move recorded, as the executor would have done;
//! * the old item is gone and nothing is found there yet: its removal is no
//!   evidence of a deletion (`data-safety.md` invariant 3), so the evidence
//!   is withdrawn (the item reads as missing, which holds it) and the
//!   record stays for the next round.

use oneup_store::{Kind, Ops, Result};
use oneup_sync::{NodeKind, RemoteId, Sid, same_name};

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
#[derive(Debug, Default, Clone, Copy, PartialEq, Eq)]
pub struct Settled {
    /// Synced rows relinked to the item their move made.
    pub relinked: usize,
    /// Moves whose item can't be found yet: held, tried again next round.
    pub waiting: usize,
}

/// Settles the moves a round cut short left (see the module docs). Call it
/// once the Remote tree is current and before planning. Costs one indexed
/// lookup per record left over: none in a round that wasn't cut short.
pub fn settle_moves(o: &Ops<'_>) -> Result<Settled> {
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
        let mut found = None;
        for (r, _) in o.remote_children(&m.to)? {
            if r.id != m.old && r.kind == kind && same_name(&r.name, &m.name) {
                found = Some(r);
                break;
            }
        }
        match found {
            Some(r) if o.synced_by_remote(&r.id)?.is_none() => {
                tracing::debug!(sid = m.sid, "a move cut short is recorded where it went");
                // Its eTag stays the one last agreed: the planner compares
                // content by hash, and without one an eTag that differs
                // costs a download recognised as identical.
                let parent = Some(m.parent);
                o.synced_put(&oneup_sync::Base { remote: Some(RemoteId(r.id)), parent, name: m.name, ..b })?;
                o.meta_delete(&key)?;
                out.relinked += 1;
            }
            _ => {
                tracing::debug!(sid = m.sid, "a move cut short isn't found where it went yet");
                o.unnote_remote_deleted(&m.old)?;
                out.waiting += 1;
            }
        }
    }
    Ok(out)
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used)]
    use super::*;
    use oneup_store::{RemoteRow, Store};
    use oneup_sync::{Base, LocalId, ROOT};

    fn row(id: &str, parent: &str, name: &str, kind: Kind) -> RemoteRow {
        RemoteRow {
            id: id.into(),
            parent_id: Some(parent.into()),
            name: name.into(),
            kind,
            size: 0,
            etag: Some("e".into()),
            qxh: None,
            mtime_ms: None,
        }
    }

    fn base(sid: Sid, parent: Option<Sid>, name: &str, kind: NodeKind, remote: &str) -> Base {
        Base {
            sid,
            parent,
            name: name.into(),
            kind,
            size: 0,
            hash: None,
            local: Some(LocalId(u128::from(sid) + 100)),
            local_mtime: 0,
            remote: Some(RemoteId(remote.into())),
            remote_etag: "e".into(),
        }
    }

    /// The root `root`, folders `a` and `b`, and in `a` item 3, `x.txt`.
    fn store(kind: NodeKind) -> Store {
        let mut s = Store::open_in_memory().unwrap();
        let k = if kind == NodeKind::File { Kind::File } else { Kind::Folder };
        s.write(|o| {
            o.upsert_item(&RemoteRow { parent_id: None, kind: Kind::Root, ..row("root", "", "", Kind::Root) }, false)?;
            o.upsert_item(&row("a", "root", "a", Kind::Folder), false)?;
            o.upsert_item(&row("b", "root", "b", Kind::Folder), false)?;
            o.upsert_item(&row("old", "a", "x.txt", k), false)?;
            o.synced_put(&base(ROOT, None, "", NodeKind::Folder, "root"))?;
            o.synced_put(&base(1, Some(ROOT), "a", NodeKind::Folder, "a"))?;
            o.synced_put(&base(2, Some(ROOT), "b", NodeKind::Folder, "b"))?;
            o.synced_put(&base(3, Some(1), "x.txt", kind, "old"))?;
            Ok(())
        })
        .unwrap();
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
            let k = if kind == NodeKind::File { Kind::File } else { Kind::Folder };
            s.write(|o| {
                o.meta_set(&moving_of(3), &intent().encode())?;
                // The listing after the crash: the old entry is gone (with
                // evidence), the rebuilt one is in `b`.
                o.delete_subtree("old")?;
                o.upsert_item(&row("new", "b", "A Long Name.txt", k), false)
            })
            .unwrap();
            let done = s.write(settle_moves).unwrap();
            assert_eq!(done, Settled { relinked: 1, waiting: 0 });
            let b = s.read(|o| o.synced_get(3)).unwrap().unwrap();
            assert_eq!(b.remote, Some(RemoteId("new".into())));
            assert_eq!((b.parent, b.name.as_str()), (Some(2), "a long name.txt"));
            assert_eq!(b.remote_etag, "e");
            assert!(s.read(|o| o.check_invariants()).unwrap().is_empty());
            assert!(left(&s).is_empty());
        }
    }

    /// The move didn't happen: the record goes and the Synced row stays,
    /// so the next plan makes the move again.
    #[test]
    fn a_move_not_made_is_dropped() {
        let mut s = store(NodeKind::File);
        s.write(|o| o.meta_set(&moving_of(3), &intent().encode())).unwrap();
        assert_eq!(s.write(settle_moves).unwrap(), Settled::default());
        assert_eq!(s.read(|o| o.synced_get(3)).unwrap().unwrap().remote, Some(RemoteId("old".into())));
        assert!(left(&s).is_empty());
    }

    /// Recorded already (the row follows another ID), or forgotten: the
    /// record just goes.
    #[test]
    fn a_move_recorded_or_forgotten_is_dropped() {
        let mut s = store(NodeKind::File);
        s.write(|o| {
            o.meta_set(&moving_of(3), &intent().encode())?;
            o.meta_set(&moving_of(9), &MoveIntent { sid: 9, ..intent() }.encode())?;
            o.meta_set(&format!("{MOVING}.x"), "nonsense")?;
            o.delete_subtree("old")?;
            o.upsert_item(&row("new", "b", "a long name.txt", Kind::File), false)?;
            let b = o.synced_get(3)?.unwrap();
            o.synced_put(&Base { remote: Some(RemoteId("new".into())), ..b })
        })
        .unwrap();
        assert_eq!(s.write(settle_moves).unwrap(), Settled::default());
        assert!(left(&s).is_empty());
    }

    /// Gone, and nothing (of its kind, linked to nothing) where it went:
    /// held without evidence, and tried again.
    #[test]
    fn a_move_whose_item_isnt_found_yet_holds_it() {
        let mut s = store(NodeKind::File);
        s.write(|o| {
            o.meta_set(&moving_of(3), &intent().encode())?;
            o.delete_subtree("old")?;
            // A folder of that name isn't it.
            o.upsert_item(&row("other", "b", "a long name.txt", Kind::Folder), false)
        })
        .unwrap();
        assert!(noted_gone(&s, "old"), "the listing noted it deleted");
        assert_eq!(s.write(settle_moves).unwrap(), Settled { relinked: 0, waiting: 1 });
        assert!(!noted_gone(&s, "old"), "no longer evidence");
        assert_eq!(s.read(|o| o.synced_get(3)).unwrap().unwrap().remote, Some(RemoteId("old".into())));
        assert_eq!(left(&s).len(), 1, "tried again next round");
    }
}
