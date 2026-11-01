//! A rewind's cost follows the changes since its moment, not the size of
//! the folder: planning one change in a large folder reads a handful of
//! records (`oneup_sync::rewind::plan` over the store).
#![allow(clippy::unwrap_used)]

use std::cell::Cell;

use oneup_store::{Ops, Store};
use oneup_sync::rewind::{History, Was};
use oneup_sync::{Base, LocalId, LocalNode, NodeKind, ROOT, RemoteId, RemoteNode, Seen, Sid, Trees};

/// The store, counting the records each read returns.
struct Counting<'a, 'b> {
    o: &'a Ops<'b>,
    reads: Cell<usize>,
}

impl Counting<'_, '_> {
    fn n(&self, k: usize) {
        self.reads.set(self.reads.get() + k.max(1));
    }
}

type R<T> = Result<T, oneup_store::Error>;

impl Trees for Counting<'_, '_> {
    type Error = oneup_store::Error;

    fn base(&self, sid: Sid) -> R<Option<Base>> {
        self.n(1);
        Trees::base(self.o, sid)
    }
    fn base_children(&self, sid: Sid) -> R<Vec<Base>> {
        let v = Trees::base_children(self.o, sid)?;
        self.n(v.len());
        Ok(v)
    }
    fn base_named(&self, parent: Sid, name: &str) -> R<Vec<Base>> {
        let v = Trees::base_named(self.o, parent, name)?;
        self.n(v.len());
        Ok(v)
    }
    fn base_by_local(&self, id: LocalId) -> R<Option<Base>> {
        self.n(1);
        Trees::base_by_local(self.o, id)
    }
    fn base_by_remote(&self, id: &RemoteId) -> R<Option<Base>> {
        self.n(1);
        Trees::base_by_remote(self.o, id)
    }
    fn local(&self, id: LocalId) -> R<Seen<LocalNode>> {
        self.n(1);
        Trees::local(self.o, id)
    }
    fn remote(&self, id: &RemoteId) -> R<Seen<RemoteNode>> {
        self.n(1);
        Trees::remote(self.o, id)
    }
    fn local_children(&self, id: LocalId) -> R<Vec<LocalNode>> {
        let v = Trees::local_children(self.o, id)?;
        self.n(v.len());
        Ok(v)
    }
    fn remote_children(&self, id: &RemoteId) -> R<Vec<RemoteNode>> {
        let v = Trees::remote_children(self.o, id)?;
        self.n(v.len());
        Ok(v)
    }
}

impl History for Counting<'_, '_> {
    type Error = oneup_store::Error;

    fn change_after(&self, sid: Sid, at: i64) -> R<Option<Option<Was>>> {
        self.n(1);
        History::change_after(self.o, sid, at)
    }
    fn children_after(&self, parent: Sid, at: i64) -> R<Vec<Sid>> {
        let v = History::children_after(self.o, parent, at)?;
        self.n(v.len());
        Ok(v)
    }
    fn last_seen(&self, id: &RemoteId) -> R<Option<Was>> {
        self.n(1);
        History::last_seen(self.o, id)
    }
    fn deleted_from(&self, id: &RemoteId) -> R<Option<(RemoteId, String)>> {
        self.n(1);
        History::deleted_from(self.o, id)
    }
    fn changed_after(&self, at: i64, limit: usize) -> R<Option<Vec<Sid>>> {
        let v = History::changed_after(self.o, at, limit)?;
        self.n(v.as_ref().map_or(0, Vec::len));
        Ok(v)
    }
    fn unsettled(&self, limit: usize) -> R<Option<Vec<Sid>>> {
        let v = History::unsettled(self.o, limit)?;
        self.n(v.as_ref().map_or(0, Vec::len));
        Ok(v)
    }
}

fn base(sid: Sid, parent: Option<Sid>, name: &str, kind: NodeKind, hash: u8) -> Base {
    Base {
        sid,
        parent,
        name: name.into(),
        kind,
        size: 1,
        hash: (kind == NodeKind::File).then_some([hash; 20]),
        local: Some(LocalId(u128::from(sid) + 1_000)),
        local_mtime: 0,
        remote: Some(RemoteId(format!("synthetic-drive!{sid}"))),
        remote_etag: format!("e{sid}"),
    }
}

#[test]
fn one_change_in_a_large_folder_reads_a_handful_of_records() {
    const FILES: Sid = 50_000;
    let mut s = Store::open_in_memory().unwrap();
    s.write(|o| {
        o.synced_put(&base(ROOT, None, "", NodeKind::Folder, 0))?;
        o.synced_put(&base(2, Some(ROOT), "big", NodeKind::Folder, 0))?;
        for i in 0..FILES {
            o.synced_put(&base(10 + i, Some(2), &format!("f{i}.txt"), NodeKind::File, 1))?;
        }
        o.history_start(1)
    })
    .unwrap();
    // One file edited since the moment, and synced (the history records it).
    let at = 1_000;
    s.write(|o| o.synced_put(&base(10 + FILES / 2, Some(2), &format!("f{}.txt", FILES / 2), NodeKind::File, 2)))
        .unwrap();
    // Synced: the rounds that did it planned what they marked.
    s.write(|o| o.dirty_clear(o.dirty_scope()?.upto).map(|_| ())).unwrap();
    let reads = s
        .read(|o| {
            let c = Counting { o, reads: Cell::new(0) };
            let r = oneup_sync::rewind::plan(&c, &c, 2, at, 100_000).unwrap();
            assert!(
                r.changes.iter().any(|x| x.sid == 10 + FILES / 2)
                    || r.skipped.iter().any(|(sid, _, _)| *sid == 10 + FILES / 2),
                "the changed file is planned: {r:?}"
            );
            Ok(c.reads.get())
        })
        .unwrap();
    assert!(reads < 100, "{reads} records read for one change among {FILES}");
}

/// A synced record, and its item on both sides exactly as recorded.
fn synced_both(o: &Ops<'_>, b: &Base) -> oneup_store::Result<()> {
    use oneup_store::{Kind, LocalKind, LocalRow, RemoteRow, Skip};
    o.synced_put(b)?;
    let parent = b.parent.and_then(|p| o.synced_get(p).ok().flatten());
    let folder = b.kind == NodeKind::Folder;
    let root = b.sid == ROOT;
    o.local_put(&LocalRow {
        file_id: b.local.unwrap().0,
        parent_id: parent.as_ref().and_then(|p| p.local).map(|l| l.0),
        name: b.name.clone(),
        name_lossy: false,
        kind: if root {
            LocalKind::Root
        } else if folder {
            LocalKind::Folder
        } else {
            LocalKind::File
        },
        size: b.size,
        mtime: b.local_mtime,
        attributes: 0,
        reparse_tag: 0,
        skip: Skip::None,
    })?;
    o.upsert_item(
        &RemoteRow {
            id: b.remote.clone().unwrap().0,
            parent_id: parent.and_then(|p| p.remote).map(|r| r.0),
            name: b.name.clone(),
            kind: if root {
                Kind::Root
            } else if folder {
                Kind::Folder
            } else {
                Kind::File
            },
            size: b.size,
            etag: Some(b.remote_etag.clone()),
            qxh: b.hash,
            mtime_ms: None,
        },
        false,
    )?;
    Ok(())
}

#[test]
fn one_unsynced_edit_among_a_large_folder_reads_a_handful_of_records() {
    // The root holds 50,000 files and the folder rewound; one file in the
    // root was edited here and isn't synced yet, so the root is marked.
    // Finding the folder by name and reading what isn't synced yet read
    // only the matches, not the root's other children.
    const FILES: Sid = 50_000;
    let mut s = Store::open_in_memory().unwrap();
    s.write(|o| {
        synced_both(o, &base(ROOT, None, "", NodeKind::Folder, 0))?;
        synced_both(o, &base(2, Some(ROOT), "Docs", NodeKind::Folder, 0))?;
        synced_both(o, &base(3, Some(2), "in.txt", NodeKind::File, 1))?;
        for i in 0..FILES {
            synced_both(o, &base(10 + i, Some(ROOT), &format!("f{i}.txt"), NodeKind::File, 1))?;
        }
        o.history_start(1)?;
        o.dirty_clear(o.dirty_scope()?.upto).map(|_| ())
    })
    .unwrap();
    // Edited here: a new size, not synced.
    s.write(|o| {
        let mut row = o.local_get(u128::from(10 + FILES / 2) + 1_000)?.unwrap();
        row.size = 99;
        row.mtime = 5;
        o.local_update(&row)
    })
    .unwrap();
    assert!(!s.read(|o| o.dirty_scope()).unwrap().folders.is_empty(), "the root is marked");
    #[cfg(feature = "statement-log")]
    {
        s.log_statements();
        Store::take_statements();
    }
    let reads = s
        .read(|o| {
            let c = Counting { o, reads: Cell::new(0) };
            let unsettled = History::unsettled(o, 1000)?.unwrap();
            assert!(unsettled.contains(&(10 + FILES / 2)), "the edit is found: {unsettled:?}");
            assert!(unsettled.len() < 10, "only what differs: {}", unsettled.len());
            let sid = oneup_sync::rewind::folder_at(&c, &c, &["docs"], 1_000)?.unwrap();
            assert_eq!(sid, 2, "found by name, as Windows compares names");
            oneup_sync::rewind::plan(&c, &c, sid, 1_000, 100_000).unwrap();
            Ok(c.reads.get())
        })
        .unwrap();
    assert!(reads < 100, "{reads} records read for one edit among {FILES}");
    // Put back as recorded: its mark goes when the rounds tidy up.
    s.write(|o| {
        let mut row = o.local_get(u128::from(10 + FILES / 2) + 1_000)?.unwrap();
        row.size = 1;
        row.mtime = 0;
        o.local_update(&row)?;
        assert_eq!(o.history_settle_marks(100)?, 1);
        assert_eq!(History::unsettled(o, 10)?, Some(vec![]));
        Ok(())
    })
    .unwrap();
    // And no statement it ran reads a whole table that grows with the tree.
    #[cfg(feature = "statement-log")]
    {
        let scans = s.tree_scans(&Store::take_statements()).unwrap();
        assert!(scans.is_empty(), "{scans:#?}");
    }
}
