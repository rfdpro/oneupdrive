//! The change-journal observer: keeps the Local tree current after a scan,
//! and records **deletion evidence** for the reconciler.
//!
//! ## How it works
//! The scan stored the journal position taken *before* it started. The
//! observer reads the volume's USN journal from there, in batches, and for
//! each batch applies the **last** record of every file ID it mentions:
//!
//! * A `FILE_DELETE` record removes the item. For an item the Synced tree
//!   links, that's recorded as evidence (`note_local_deleted`); it is the
//!   only thing that ever is. The item's own record is required: a folder
//!   delete is evidence for that folder, not for rows still stored below it.
//! * Any other record means "look again": the item is read *by file ID*
//!   ([`Volume::stat_by_id`]), because unprivileged records carry no names
//!   (measured). Its parent comes from the record. Then:
//!   - parent in the tree (a listed folder): the item is stored or updated,
//!     which covers creates, edits, renames and moves within the tree;
//!     a folder that is new to the tree is queued and listed with the
//!     scanner's code, so a folder moved in from elsewhere arrives whole;
//!   - parent not in the tree: the item left it (moved out, or into an
//!     excluded folder), and its rows are dropped **without** evidence.
//!     The reconciler sees that as Missing and holds it.
//!
//! The journal position advances in the same transaction as the batch's
//! changes, so a crash replays at most one batch, and replaying is
//! harmless: every step reads the current state rather than trusting the
//! record's history. Folders queued for listing survive a crash in
//! `local_scan_queue` and are listed first on the next run.
//!
//! ## When a rescan is required
//! The journal was deleted and re-created (new journal ID), or it wrapped
//! past our position, or the scan is unfinished, older, or used other
//! rules. Then changes may have been missed, and only a new scan can
//! rebuild the tree. A rescan records **no** evidence: whatever disappeared
//! meanwhile reads as Missing (`data-safety.md` invariant 3).
//!
//! ## Cost
//! Proportional to the journal's records, not the tree: one indexed lookup
//! per mentioned ID, and one open-by-ID per item inside the tree. Folders
//! are listed only when they enter the tree. The journal is volume-wide, so
//! activity elsewhere on the disk costs a lookup per record and nothing more.

use std::collections::{BTreeSet, HashMap};

use oneup_ntfs::FileId;
use oneup_ntfs::dirent::DirEntry;
use oneup_ntfs::record::{UsnRecord, reason};
use oneup_rules::ExcludeSet;
use oneup_store::{LocalKind, Ops, Skip};

use crate::classify;

/// Why the stored Local tree can't be brought up to date from the journal.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Rescan {
    NoScan,
    ScanUnfinished,
    /// Made by an older scanner whose rows can't be trusted.
    OlderScanner,
    /// The exclusion rules differ from the ones the scan used.
    RulesChanged,
    /// The file system has no change journal (e.g. FAT, network shares).
    NoJournal,
    /// The journal was deleted and re-created since the scan.
    JournalReplaced,
    /// The journal no longer reaches back to our position.
    JournalWrapped,
    /// The scanned folder itself is gone, or another folder or disk is now
    /// at its place.
    RootGone,
}

impl std::fmt::Display for Rescan {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(match self {
            Rescan::NoScan => "no folder has been scanned yet",
            Rescan::ScanUnfinished => "the scan hasn't finished",
            Rescan::OlderScanner => "the scan was made by an older version",
            Rescan::RulesChanged => "the exclusion rules changed since the scan",
            Rescan::NoJournal => "the drive has no change journal",
            Rescan::JournalReplaced => "the drive's change journal was reset since the scan",
            Rescan::JournalWrapped => "the drive's change journal no longer reaches back to the last update",
            Rescan::RootGone => "the scanned folder is gone or was replaced",
        })
    }
}

#[derive(Debug, thiserror::Error)]
pub enum Error {
    #[error(transparent)]
    Store(#[from] oneup_store::Error),
    #[cfg(windows)]
    #[error(transparent)]
    Io(#[from] oneup_ntfs::win::Error),
    #[error(transparent)]
    Parse(#[from] oneup_ntfs::record::ParseError),
    #[cfg(windows)]
    #[error(transparent)]
    Scan(#[from] crate::scan::Error),
    #[error("scan again ({0}): `oneup scan <folder>`")]
    NeedsRescan(Rescan),
}

pub type Result<T> = std::result::Result<T, Error>;

/// What one catch-up did.
#[derive(Debug, Default, Clone, PartialEq, Eq)]
pub struct Changes {
    /// Journal records read (whole volume).
    pub records: u64,
    /// Items new to the tree.
    pub created: u64,
    /// Items in the tree that changed: content, attributes, name or place.
    pub updated: u64,
    /// Rows dropped because their items left the tree (no evidence).
    pub left: u64,
    /// Items deleted, per a USN delete record.
    pub deleted: u64,
    /// Of those, Synced items whose deletion was recorded as evidence.
    pub evidence: u64,
    /// Items that couldn't be read (usually access denied); left as they were.
    pub unreadable: u64,
    /// Folders queued for listing because they entered the tree.
    pub folders_queued: u64,
    /// Folders whose children changed (old and new parents, and folders
    /// that entered the tree). The sync engine plans exactly these.
    pub dirty: BTreeSet<u128>,
}

/// What reading an item by file ID found.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Stat {
    Found(DirEntry),
    /// No such item (any more). If it was deleted, its delete record follows.
    Gone,
    /// It exists but couldn't be read.
    Unreadable,
}

/// Whether `id` is a stored folder whose contents are listed, i.e. a place
/// in the tree that children can occupy.
fn listed_folder(o: &Ops<'_>, id: u128) -> Result<bool> {
    Ok(o.local_get(id)?.is_some_and(|r| matches!(r.kind, LocalKind::Folder | LocalKind::Root) && r.skip == Skip::None))
}

/// Applies one batch of journal records to the Local tree (see the module
/// docs). Pure apart from `stat`, which reads an item's current metadata,
/// so it can be tested with made-up records.
/// What [`reapply_rules`] changed.
#[derive(Debug, Default, Clone, Copy, PartialEq, Eq)]
pub struct Reapplied {
    /// Folders newly excluded: their stored contents were dropped.
    pub excluded: usize,
    /// Folders no longer excluded: queued to be listed.
    pub included: usize,
}

/// Brings a finished scan in line with changed name-only folder rules,
/// without a rescan (`docs/architecture/rules.md`): a folder that is now
/// excluded keeps its own row, marked excluded, and loses what was stored
/// below it; one that no longer is gets unmarked and queued, and the
/// observer lists it at its next catch-up. Database only. Dropping rows is
/// not deletion evidence (synced items there are hidden by planning, whose
/// rules include these). One pass over the stored folders: rule changes are
/// rare. Returns `None` when nothing needed doing (the rules are the ones
/// stored, or there's no finished current scan to adjust).
pub fn reapply_rules(o: &Ops<'_>, rules: &ExcludeSet) -> oneup_store::Result<Option<Reapplied>> {
    let Some(root) = o.local_root()? else { return Ok(None) };
    let fp = rules.fingerprint();
    if root.rules == fp || root.finished_at_ms.is_none() || !o.local_scan_is_current()? {
        return Ok(None);
    }
    let top = root.root_file_id;
    let mut flip = Vec::new();
    o.for_each_local_folder(|id, parent, name, skip| {
        let want = (parent == Some(top) && oneup_rules::is_engine_dir(name)) || rules.excludes_folder(name);
        match (skip, want) {
            (Skip::None, true) => flip.push((id, true)),
            (Skip::Excluded, false) => flip.push((id, false)),
            _ => {}
        }
    })?;
    let mut done = Reapplied::default();
    for (id, exclude) in flip {
        // Gone already: inside a folder excluded earlier in this loop.
        let Some(row) = o.local_get(id)? else { continue };
        if exclude {
            o.local_remove_children(id)?;
            o.local_set_skip(id, Skip::Excluded)?;
            o.local_dequeue(id)?;
            done.excluded += 1;
        } else if row.skip == Skip::Excluded {
            o.local_set_skip(id, Skip::None)?;
            let depth = o.local_path_of(id)?.map_or(1, |p| p.matches('/').count());
            o.local_enqueue(id, u32::try_from(depth).unwrap_or(u32::MAX))?;
            done.included += 1;
        }
    }
    o.local_set_rules(&fp)?;
    Ok(Some(done))
}

pub fn apply_records(
    o: &Ops<'_>,
    root_id: u128,
    rules: &ExcludeSet,
    records: &[UsnRecord],
    stat: &mut dyn FnMut(FileId) -> Stat,
    ch: &mut Changes,
) -> Result<()> {
    // Last record per ID, in the order those last records were written:
    // that's each item's most recent known parent, and keeps parents that
    // settled earlier ahead of children that settled later.
    let mut last: HashMap<u128, usize> = HashMap::new();
    for (i, r) in records.iter().enumerate() {
        last.insert(r.file_id.0, i);
    }
    let mut order: Vec<usize> = last.into_values().collect();
    order.sort_unstable();

    for r in order.into_iter().map(|i| &records[i]) {
        let id = r.file_id.0;
        if id == root_id {
            // Renaming or moving the scanned folder doesn't change its
            // contents (it's tracked by ID). Deleting it is not evidence for
            // anything inside: stop and require a rescan.
            if r.has(reason::FILE_DELETE) {
                return Err(Error::NeedsRescan(Rescan::RootGone));
            }
            continue;
        }
        let known = o.local_get(id)?;

        if r.has(reason::FILE_DELETE) {
            if let Some(old) = &known {
                ch.dirty.extend(old.parent_id);
                o.local_remove_subtree(id)?;
                ch.deleted += 1;
            }
            // Recorded even if the item already left the tree: with POSIX
            // delete semantics (the default since Windows 10 1809), a file
            // that another program still has open is first moved to the
            // hidden `$Extend\$Deleted` folder, which looks like a move out,
            // and its delete record only follows when the last handle
            // closes. Synced rows outlive Local rows, so the evidence still
            // reaches the item. (`note_local_deleted` ignores IDs no Synced
            // row links.)
            if o.note_local_deleted(id)? {
                ch.evidence += 1;
            }
            continue;
        }

        let parent = r.parent_id.0;
        if !listed_folder(o, parent)? {
            if let Some(old) = known {
                ch.dirty.extend(old.parent_id);
                ch.left += o.local_remove_subtree(id)? as u64;
            }
            continue;
        }
        let entry = match stat(r.file_id) {
            Stat::Found(e) if !e.name.is_empty() => e,
            Stat::Found(_) | Stat::Unreadable => {
                ch.unreadable += 1;
                continue;
            }
            Stat::Gone => continue,
        };
        let row = classify::to_row(&entry, parent, parent == root_id, rules);
        ch.dirty.insert(parent);
        let was_listed = match &known {
            None => {
                ch.created += 1;
                false
            }
            Some(old) => {
                if old.parent_id != row.parent_id {
                    ch.dirty.extend(old.parent_id);
                }
                if *old != row {
                    ch.updated += 1;
                }
                let listed = old.kind == LocalKind::Folder && old.skip == Skip::None;
                if listed && row.skip != Skip::None {
                    // Renamed to an excluded name (or otherwise no longer
                    // listable): its contents leave the tree, no evidence.
                    ch.left += o.local_remove_children(id)? as u64;
                }
                listed
            }
        };
        o.local_update(&row)?;
        if row.kind == LocalKind::Folder && row.skip == Skip::None && !was_listed {
            o.local_enqueue(id, 1)?;
            ch.folders_queued += 1;
            ch.dirty.insert(id);
        }
    }
    Ok(())
}

#[cfg(windows)]
pub use driver::Observer;

#[cfg(windows)]
mod driver {
    use std::path::Path;

    use oneup_ntfs::FileId;
    use oneup_ntfs::record::UsnRecord;
    use oneup_ntfs::win::{self, Volume};
    use oneup_store::{LocalRoot, Store};

    use super::{Changes, Error, Rescan, Result, Stat, apply_records};
    use crate::scan::{Config, Progress, list_queued};

    /// Brings a finished scan up to date from the change journal.
    pub struct Observer {
        store: Store,
        volume: Volume,
        cfg: Config,
        root: LocalRoot,
        journal_id: u64,
        cursor: i64,
        /// Directory-listing buffer (for folders entering the tree).
        buf: Vec<u8>,
        /// Journal read buffer. 64 KiB holds several hundred records.
        jbuf: Vec<u8>,
        progress: Progress,
    }

    impl Observer {
        /// Checks that the stored scan can be continued from the journal:
        /// finished, current, made with `cfg`'s rules, and of the same
        /// folder (identity: volume serial + root file ID).
        pub fn open(store: Store, cfg: Config) -> Result<Self> {
            // As for the scan: otherwise Windows disguises placeholders.
            oneup_cfapi::expose_placeholders().map_err(crate::scan::Error::from)?;
            let root = store.read(|o| o.local_root())?.ok_or(Error::NeedsRescan(Rescan::NoScan))?;
            if root.finished_at_ms.is_none() {
                return Err(Error::NeedsRescan(Rescan::ScanUnfinished));
            }
            if !store.read(|o| o.local_scan_is_current())? {
                return Err(Error::NeedsRescan(Rescan::OlderScanner));
            }
            if root.rules != cfg.rules.fingerprint() {
                return Err(Error::NeedsRescan(Rescan::RulesChanged));
            }
            let (journal_id, cursor) = root.journal.ok_or(Error::NeedsRescan(Rescan::NoJournal))?;
            // The folder may have been renamed since (it's tracked by ID), but
            // its volume is found through the stored path.
            let volume =
                Volume::open_containing(Path::new(&root.path)).map_err(|_| Error::NeedsRescan(Rescan::RootGone))?;
            let same = volume
                .open_dir_by_id(FileId(root.root_file_id))
                .and_then(|h| win::file_id(&h))
                .is_ok_and(|(serial, id)| serial == root.volume_serial && id.0 == root.root_file_id);
            if !same {
                return Err(Error::NeedsRescan(Rescan::RootGone));
            }
            Ok(Self {
                store,
                volume,
                buf: vec![0u8; cfg.buffer_bytes.max(4096)],
                cfg,
                root,
                journal_id,
                cursor,
                jbuf: vec![0u8; 64 * 1024],
                progress: Progress::default(),
            })
        }

        /// Applies everything the journal recorded up to now, then lists
        /// the folders that entered the tree. Returns what changed.
        pub fn catch_up(&mut self) -> Result<Changes> {
            let mut ch = Changes::default();
            self.drain_queue()?;
            let j = self.volume.query_journal()?;
            if j.journal_id != self.journal_id {
                return Err(Error::NeedsRescan(Rescan::JournalReplaced));
            }
            let oldest = j.first_usn.max(j.lowest_valid_usn);
            if self.cursor < oldest {
                return Err(Error::NeedsRescan(Rescan::JournalWrapped));
            }
            let target = j.next_usn;
            while self.cursor < target {
                let read = self.volume.read_journal(self.journal_id, self.cursor, false, &mut self.jbuf);
                let (next, records) = match read {
                    Ok((next, recs)) => (next, recs.collect::<std::result::Result<Vec<UsnRecord>, _>>()?),
                    Err(e) => {
                        // A read can fail because the journal wrapped or was
                        // reset meanwhile; say so rather than just failing.
                        let j = self.volume.query_journal()?;
                        if j.journal_id != self.journal_id {
                            return Err(Error::NeedsRescan(Rescan::JournalReplaced));
                        }
                        if self.cursor < j.first_usn.max(j.lowest_valid_usn) {
                            return Err(Error::NeedsRescan(Rescan::JournalWrapped));
                        }
                        return Err(e.into());
                    }
                };
                if next <= self.cursor {
                    break;
                }
                ch.records += records.len() as u64;
                let (volume, rules, root_id) = (&self.volume, &self.cfg.rules, self.root.root_file_id);
                let mut stat = |id: FileId| match volume.stat_by_id(id) {
                    Ok(Some(e)) => Stat::Found(e),
                    Ok(None) => Stat::Gone,
                    Err(e) => {
                        tracing::debug!(error = %e, "changed item could not be read");
                        Stat::Unreadable
                    }
                };
                self.store.try_write(|o| {
                    apply_records(o, root_id, rules, &records, &mut stat, &mut ch)?;
                    o.local_set_journal_cursor(next)?;
                    Ok::<_, Error>(())
                })?;
                self.cursor = next;
                self.drain_queue()?;
            }
            tracing::debug!(records = ch.records, dirty = ch.dirty.len(), "local tree caught up");
            Ok(ch)
        }

        fn drain_queue(&mut self) -> Result<()> {
            while list_queued(&mut self.store, &self.volume, &self.cfg, &mut self.buf, &self.progress)? > 0 {}
            Ok(())
        }

        pub fn store(&self) -> &Store {
            &self.store
        }

        /// For the executor, which records what it did in the same store.
        pub fn store_mut(&mut self) -> &mut Store {
            &mut self.store
        }

        pub fn into_store(self) -> Store {
            self.store
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use oneup_store::{LocalRow, Store};
    use oneup_sync::{Base, LocalId, NodeKind, ROOT, RemoteId, Seen, Trees};

    const ROOT_ID: u128 = 1;
    const DIR: u32 = 0x10;

    fn rec(id: u128, parent: u128, reason: u32) -> UsnRecord {
        UsnRecord {
            major_version: 3,
            file_id: FileId(id),
            parent_id: FileId(parent),
            usn: 0,
            timestamp: 0,
            reason,
            source_info: 0,
            attributes: 0,
            name: None,
            name_lossy: false,
        }
    }

    fn entry(id: u128, name: &str, attributes: u32, size: u64) -> DirEntry {
        DirEntry {
            file_id: FileId(id),
            name: name.into(),
            name_lossy: false,
            size,
            attributes,
            reparse_tag: 0,
            created: 0,
            modified: 5,
            changed: 0,
        }
    }

    fn row(id: u128, parent: u128, name: &str, kind: LocalKind) -> LocalRow {
        LocalRow {
            file_id: id,
            parent_id: (kind != LocalKind::Root).then_some(parent),
            name: name.into(),
            name_lossy: false,
            kind,
            size: 0,
            mtime: 5,
            attributes: if kind == LocalKind::File { 0 } else { DIR },
            reparse_tag: 0,
            skip: Skip::None,
        }
    }

    /// root(1) / { docs(2) / a.txt(3), b.txt(4) }, with b.txt synced.
    fn tree() -> Store {
        let mut s = Store::open_in_memory().unwrap();
        s.write(|o| {
            o.local_update(&row(ROOT_ID, 0, "", LocalKind::Root))?;
            o.local_update(&row(2, ROOT_ID, "docs", LocalKind::Folder))?;
            o.local_update(&row(3, 2, "a.txt", LocalKind::File))?;
            o.local_update(&row(4, ROOT_ID, "b.txt", LocalKind::File))?;
            let base = |sid, parent, name: &str, local| Base {
                sid,
                parent,
                name: name.into(),
                kind: NodeKind::File,
                size: 0,
                hash: None,
                local: Some(LocalId(local)),
                local_mtime: 5,
                remote: Some(RemoteId(format!("r{local}"))),
                remote_etag: String::new(),
            };
            o.synced_put(&Base { kind: NodeKind::Folder, ..base(ROOT, None, "", ROOT_ID) })?;
            o.synced_put(&base(1, Some(ROOT), "b.txt", 4))?;
            Ok(())
        })
        .unwrap();
        s
    }

    /// Applies `records` with a fake file system holding `live` entries.
    fn apply(s: &mut Store, records: &[UsnRecord], live: &[DirEntry]) -> Changes {
        let mut ch = Changes::default();
        let mut stat = |id: FileId| match live.iter().find(|e| e.file_id == id) {
            Some(e) => Stat::Found(e.clone()),
            None => Stat::Gone,
        };
        s.try_write(|o| apply_records(o, ROOT_ID, &ExcludeSet::dev(), records, &mut stat, &mut ch)).unwrap();
        assert!(s.read(|o| o.check_invariants()).unwrap().is_empty());
        ch
    }

    fn path(s: &Store, id: u128) -> Option<String> {
        s.read(|o| o.local_path_of(id)).unwrap()
    }

    #[test]
    fn creates_edits_and_moves_follow_the_last_record() {
        let mut s = tree();
        let ch = apply(
            &mut s,
            &[
                rec(5, 2, reason::FILE_CREATE),
                rec(3, 2, reason::RENAME_OLD_NAME),
                rec(3, ROOT_ID, reason::RENAME_NEW_NAME),
                rec(4, ROOT_ID, reason::DATA_EXTEND | reason::CLOSE),
            ],
            &[entry(5, "new.txt", 0, 1), entry(3, "moved.txt", 0, 0), entry(4, "b.txt", 0, 9)],
        );
        assert_eq!(path(&s, 5).as_deref(), Some("/docs/new.txt"));
        assert_eq!(path(&s, 3).as_deref(), Some("/moved.txt"));
        assert_eq!(s.read(|o| o.local_get(4)).unwrap().map(|r| r.size), Some(9));
        assert_eq!((ch.created, ch.updated, ch.deleted), (1, 2, 0));
        assert_eq!(ch.dirty, [ROOT_ID, 2].into());
    }

    #[test]
    fn only_a_delete_record_is_evidence() {
        let mut s = tree();
        // b.txt deleted (atomic save: a new file takes its name); a.txt's
        // folder moved out of the tree: gone too, but without evidence.
        let ch = apply(
            &mut s,
            &[
                rec(4, ROOT_ID, reason::FILE_DELETE | reason::CLOSE),
                rec(6, ROOT_ID, reason::RENAME_NEW_NAME),
                rec(2, 999, reason::RENAME_NEW_NAME),
            ],
            &[entry(6, "b.txt", 0, 3)],
        );
        assert_eq!((ch.deleted, ch.evidence, ch.left), (1, 1, 2));
        s.read(|o| {
            assert_eq!(o.local(LocalId(4))?, Seen::Deleted);
            assert_eq!(o.local(LocalId(3))?, Seen::Missing, "moved out is not deleted");
            assert!(o.local(LocalId(6))?.present().is_some());
            Ok(())
        })
        .unwrap();
    }

    #[test]
    fn a_delete_after_leaving_the_tree_is_still_evidence() {
        let mut s = tree();
        // POSIX delete of an open file: first a move into `$Extend\$Deleted`
        // (outside the tree), then, in a later batch, the delete itself.
        let ch = apply(&mut s, &[rec(4, 999, reason::RENAME_NEW_NAME)], &[]);
        assert_eq!((ch.left, ch.evidence), (1, 0));
        assert_eq!(s.read(|o| o.local(LocalId(4))).unwrap(), Seen::Missing);
        let ch = apply(&mut s, &[rec(4, 999, reason::FILE_DELETE | reason::CLOSE)], &[]);
        assert_eq!((ch.deleted, ch.evidence), (0, 1));
        assert_eq!(s.read(|o| o.local(LocalId(4))).unwrap(), Seen::Deleted);
    }

    #[test]
    fn folders_entering_the_tree_are_queued_and_excluded_ones_emptied() {
        let mut s = tree();
        // A folder moved in from elsewhere; its child's record came first
        // (parent not in the tree yet), so the listing must pick it up.
        let ch = apply(
            &mut s,
            &[
                rec(8, 7, reason::RENAME_NEW_NAME),
                rec(7, ROOT_ID, reason::RENAME_NEW_NAME),
                rec(2, ROOT_ID, reason::RENAME_NEW_NAME),
            ],
            &[entry(7, "incoming", DIR, 0), entry(8, "inner.txt", 0, 1), entry(2, "node_modules", DIR, 0)],
        );
        assert_eq!(ch.folders_queued, 1);
        assert_eq!(path(&s, 8), None, "listed later, with the folder");
        s.read(|o| {
            assert_eq!(o.local_next_dirs(10)?, vec![(7, 1)]);
            // docs was renamed to an excluded name: its contents leave the tree.
            assert_eq!(o.local_get(2)?.map(|r| r.skip), Some(Skip::Excluded));
            assert!(o.local_get(3)?.is_none());
            Ok(())
        })
        .unwrap();
        assert_eq!(ch.left, 1);
    }

    #[test]
    fn deleting_the_root_requires_a_rescan_and_changes_nothing() {
        let mut s = tree();
        let mut ch = Changes::default();
        let records = [rec(4, ROOT_ID, reason::FILE_DELETE), rec(ROOT_ID, 0, reason::FILE_DELETE)];
        let r = s.try_write(|o| apply_records(o, ROOT_ID, &ExcludeSet::dev(), &records, &mut |_| Stat::Gone, &mut ch));
        assert!(matches!(r, Err(Error::NeedsRescan(Rescan::RootGone))));
        assert!(s.read(|o| o.local_get(4)).unwrap().is_some(), "the batch was rolled back");
    }

    #[test]
    fn records_outside_the_tree_and_unreadable_items_are_ignored() {
        let mut s = tree();
        let ch = apply(&mut s, &[rec(50, 49, reason::FILE_CREATE), rec(51, 2, reason::FILE_CREATE)], &[]);
        assert_eq!((ch.created, ch.dirty.len()), (0, 0));
        let mut ch = Changes::default();
        s.try_write(|o| {
            apply_records(
                o,
                ROOT_ID,
                &ExcludeSet::dev(),
                &[rec(3, 2, reason::CLOSE)],
                &mut |_| Stat::Unreadable,
                &mut ch,
            )
        })
        .unwrap();
        assert_eq!(ch.unreadable, 1);
        assert_eq!(path(&s, 3).as_deref(), Some("/docs/a.txt"), "left as it was");
    }
}
