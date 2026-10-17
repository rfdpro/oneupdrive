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
//! ## Delete records read ahead ([`SeenDeletes`])
//! The agent's journal watcher reads every record as it's written, while
//! the observer only runs between rounds. A long round on a busy disk (a
//! build tree or a dependency folder deleted: hundreds of thousands of
//! records) can wrap a small journal before the observer reads it, losing
//! the delete records of synced items deleted meanwhile, so they would be
//! held instead of deleted in OneDrive. The watcher keeps the delete
//! records of items a Synced row links ([`remember_deletes`]), and after a
//! rescan they're noted as evidence for the items the rescan didn't find
//! ([`note_seen_deletes`]). They are the same USN delete records the
//! observer would have applied; nothing is inferred from absence.
//!
//! ## Cost
//! Proportional to the journal's records, not the tree: one indexed lookup
//! per mentioned ID, and one open-by-ID per item inside the tree. Folders
//! are listed only when they enter the tree. The journal is volume-wide, so
//! activity elsewhere on the disk costs a lookup per record and nothing more.

use std::collections::{BTreeSet, HashMap, VecDeque};
use std::sync::Mutex;

use oneup_ntfs::FileId;
use oneup_ntfs::dirent::DirEntry;
use oneup_ntfs::record::{UsnRecord, reason};
use oneup_rules::ExcludeSet;
use oneup_store::{LocalKind, Ops, Skip, Store};

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
    // (item, its parent per the record)
    stat: &mut dyn FnMut(FileId, FileId) -> Stat,
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
        let entry = match stat(r.file_id, r.parent_id) {
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

/// The most delete records [`SeenDeletes`] keeps (32 bytes each, so at most
/// 8 MiB). Only synced items' records are kept, and only until the
/// observer has read past them; past this, the oldest go, and their items
/// are held after a rescan as they would be without the log.
const SEEN_MAX: usize = 1 << 18;

/// Delete records of synced items, read by the journal watcher ahead of the
/// observer (see the module docs). One per process ([`seen_deletes`]); a
/// process without a watcher leaves it empty.
#[derive(Debug, Default)]
pub struct SeenDeletes {
    inner: Mutex<Seen>,
}

#[derive(Debug, Default)]
struct Seen {
    journal_id: u64,
    /// (USN of the record, file ID), oldest first.
    ids: VecDeque<(i64, u128)>,
}

impl SeenDeletes {
    pub const fn new() -> Self {
        Self { inner: Mutex::new(Seen { journal_id: 0, ids: VecDeque::new() }) }
    }

    fn lock(&self) -> std::sync::MutexGuard<'_, Seen> {
        self.inner.lock().unwrap_or_else(std::sync::PoisonError::into_inner)
    }

    /// Records that journal `journal_id` holds a delete record for `id` at
    /// `usn`. Records of another journal are dropped: a new journal's
    /// positions start over.
    pub fn saw(&self, journal_id: u64, usn: i64, id: u128) {
        let mut s = self.lock();
        if s.journal_id != journal_id {
            s.ids.clear();
            s.journal_id = journal_id;
        }
        if s.ids.len() >= SEEN_MAX {
            s.ids.pop_front();
        }
        s.ids.push_back((usn, id));
    }

    /// Forgets the records before `cursor`: the observer has read them.
    pub fn trim(&self, journal_id: u64, cursor: i64) {
        let mut s = self.lock();
        if s.journal_id == journal_id {
            while s.ids.front().is_some_and(|&(usn, _)| usn < cursor) {
                s.ids.pop_front();
            }
        }
    }

    /// The IDs recorded from journal `journal_id`.
    pub fn ids(&self, journal_id: u64) -> Vec<u128> {
        let s = self.lock();
        if s.journal_id != journal_id {
            return Vec::new();
        }
        s.ids.iter().map(|&(_, id)| id).collect()
    }

    pub fn len(&self) -> usize {
        self.lock().ids.len()
    }

    pub fn is_empty(&self) -> bool {
        self.len() == 0
    }
}

static SEEN: SeenDeletes = SeenDeletes::new();

/// This process's [`SeenDeletes`].
pub fn seen_deletes() -> &'static SeenDeletes {
    &SEEN
}

/// Keeps, in `seen`, the delete records among `records` (journal
/// `journal_id`) whose items a Synced row links: the only ones evidence is
/// ever recorded for. One indexed lookup per delete record, in one read.
/// Returns how many were kept.
pub fn remember_deletes(
    store: &Store,
    seen: &SeenDeletes,
    journal_id: u64,
    records: &[UsnRecord],
) -> oneup_store::Result<usize> {
    let deletes: Vec<&UsnRecord> = records.iter().filter(|r| r.has(reason::FILE_DELETE)).collect();
    if deletes.is_empty() {
        return Ok(0);
    }
    let synced = store.read(|o| {
        let mut keep = Vec::new();
        for r in deletes {
            if o.synced_by_local(r.file_id.0)?.is_some() {
                keep.push((r.usn, r.file_id.0));
            }
        }
        Ok(keep)
    })?;
    for &(usn, id) in &synced {
        seen.saw(journal_id, usn, id);
    }
    Ok(synced.len())
}

/// After a rescan: notes deletion evidence for the items `seen` holds delete
/// records of (from the journal the rescan continues) and that the rescan
/// didn't find. An item it found is left alone: it's there. Returns how
/// many Synced items got evidence.
pub fn note_seen_deletes(store: &mut Store, seen: &SeenDeletes) -> oneup_store::Result<u64> {
    let Some(root) = store.read(|o| o.local_root())? else { return Ok(0) };
    let Some((journal_id, _)) = root.journal else { return Ok(0) };
    let ids = seen.ids(journal_id);
    if ids.is_empty() {
        return Ok(0);
    }
    store.write(|o| {
        let mut noted = 0;
        for id in ids {
            // The root's own delete record isn't evidence for anything
            // (a rescan wouldn't have opened without it anyway).
            if id != root.root_file_id && o.local_get(id)?.is_none() && o.note_local_deleted(id)? {
                noted += 1;
            }
        }
        Ok(noted)
    })
}

#[cfg(windows)]
pub use driver::Observer;
#[cfg(windows)]
pub use driver::{JournalTail, Tailed};

#[cfg(windows)]
mod driver {
    use std::path::Path;

    use oneup_ntfs::FileId;
    use oneup_ntfs::dirent::DirEntry;
    use oneup_ntfs::record::UsnRecord;
    use oneup_ntfs::win::{self, Volume};
    use oneup_store::{LocalRoot, Store};

    use super::{Changes, Error, Rescan, Result, Stat, apply_records};
    use crate::classify;
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
                let mut stat = |id: FileId, parent: FileId| match volume.stat_by_id(id) {
                    Ok(Some(e)) => as_listed(volume, parent, e),
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

        /// The journal (id, USN) the tree is current up to.
        pub fn position(&self) -> (u64, i64) {
            (self.journal_id, self.cursor)
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

    /// Reads a volume's change journal record by record from a position
    /// on, for the agent's journal watcher. It never skips ahead by choice.
    /// When the journal wrapped past its position anyway (the disk wrote
    /// faster than it read), it goes on from the oldest record still kept;
    /// when the journal was replaced, from the new journal's start. Either
    /// way it says so ([`Tailed::lost`]): what was lost is held, as after
    /// any journal loss.
    #[derive(Debug)]
    pub struct JournalTail {
        volume: Volume,
        journal_id: u64,
        cursor: i64,
        /// 64 KiB holds several hundred records.
        buf: Vec<u8>,
    }

    /// One read of a [`JournalTail`].
    #[derive(Debug, Default)]
    pub struct Tailed {
        pub records: Vec<UsnRecord>,
        /// Records were lost before these (the journal wrapped past the
        /// position, or was replaced).
        pub lost: bool,
    }

    impl JournalTail {
        /// Opens the journal of the volume holding `path`, at `from` (a
        /// journal ID and position, e.g. the observer's) if that journal is
        /// still the volume's; at its oldest kept record if it no longer
        /// reaches back that far; else at its end.
        pub fn open(path: &Path, from: Option<(u64, i64)>) -> win::Result<Self> {
            let volume = Volume::open_containing(path)?;
            let j = volume.query_journal()?;
            let cursor = match from {
                Some((id, usn)) if id == j.journal_id => usn.clamp(oldest(&j), j.next_usn),
                _ => j.next_usn,
            };
            Ok(Self { volume, journal_id: j.journal_id, cursor, buf: vec![0u8; 64 * 1024] })
        }

        /// The journal and position the next read starts from.
        pub fn position(&self) -> (u64, i64) {
            (self.journal_id, self.cursor)
        }

        /// How many bytes of records are waiting to be read.
        pub fn behind(&self) -> win::Result<i64> {
            Ok(self.volume.query_journal()?.next_usn.saturating_sub(self.cursor))
        }

        /// The next records, up to a buffer's worth. With `wait`, blocks in
        /// the kernel until there's at least one.
        pub fn next(&mut self, wait: bool) -> win::Result<Tailed> {
            // Checked first as well: a read from before the oldest record
            // fails, but one from position 0 reads from the start silently.
            let j = self.volume.query_journal()?;
            let mut lost = false;
            if j.journal_id != self.journal_id || self.cursor < oldest(&j) {
                (self.journal_id, self.cursor, lost) = (j.journal_id, oldest(&j), true);
            }
            let mut retried = false;
            let (next, records) = loop {
                match self.volume.read_journal(self.journal_id, self.cursor, wait, &mut self.buf) {
                    Ok((next, records)) => break (next, records.flatten().collect::<Vec<_>>()),
                    // Wrapped or replaced meanwhile (a wait can be long):
                    // once more from the oldest record. Anything else, or a
                    // second failure, is the caller's.
                    Err(e) => {
                        let j = self.volume.query_journal()?;
                        if retried || (j.journal_id == self.journal_id && self.cursor >= oldest(&j)) {
                            return Err(e);
                        }
                        (self.journal_id, self.cursor, lost, retried) = (j.journal_id, oldest(&j), true, true);
                    }
                }
            };
            self.cursor = self.cursor.max(next);
            Ok(Tailed { records, lost })
        }
    }

    fn oldest(j: &win::JournalInfo) -> i64 {
        j.first_usn.max(j.lowest_valid_usn)
    }

    /// A cloud placeholder folder read by ID, with the attributes its
    /// parent's listing gives it. The handle may lack the recall flags that
    /// mark it unpopulated (Microsoft documents `FILE_ATTRIBUTE_RECALL_ON_OPEN`
    /// only for listings), and a folder judged populated is listed, which
    /// would make its provider fetch its contents. Only the parent is asked,
    /// for this one name. Unconfirmed (moved again, or unreadable), it's left
    /// unread: its next record, or a rescan, reads it.
    fn as_listed(volume: &Volume, parent: FileId, e: DirEntry) -> Stat {
        if !e.is_dir()
            || !classify::is_cloud_tag(e.reparse_tag)
            || classify::is_unpopulated(e.attributes, e.reparse_tag)
        {
            return Stat::Found(e);
        }
        match volume.listed_in(parent, &e.name, e.file_id) {
            Ok(Some(l)) => Stat::Found(DirEntry { attributes: l.attributes, reparse_tag: l.reparse_tag, ..e }),
            Ok(None) => Stat::Unreadable,
            Err(err) => {
                tracing::debug!(error = %err, "a cloud folder's listing entry could not be read");
                Stat::Unreadable
            }
        }
    }

    /// The temp folder's volume journal; temp folders only.
    #[cfg(test)]
    mod tests {
        #![allow(clippy::unwrap_used)]
        use super::*;
        use oneup_ntfs::record::reason;

        fn temp(name: &str) -> std::path::PathBuf {
            let dir = std::env::temp_dir().join(format!("oneup-tail-{name}-{}", std::process::id()));
            let _ = std::fs::remove_dir_all(&dir);
            std::fs::create_dir_all(&dir).unwrap();
            dir
        }

        /// Reads until caught up; the deleted IDs, and whether any read lost records.
        fn drain(tail: &mut JournalTail) -> (Vec<u128>, bool) {
            let (mut deleted, mut lost) = (Vec::new(), false);
            while tail.behind().unwrap() > 0 {
                let read = tail.next(false).unwrap();
                lost |= read.lost;
                deleted.extend(read.records.iter().filter(|r| r.has(reason::FILE_DELETE)).map(|r| r.file_id.0));
                if read.records.is_empty() {
                    break;
                }
            }
            (deleted, lost)
        }

        fn file_id(path: &Path) -> u128 {
            let dir = win::open_dir(path.parent().unwrap()).unwrap();
            let (mut buf, mut out) = (vec![0u8; 64 * 1024], Vec::new());
            win::list_dir(&dir, &mut buf, &mut out).unwrap();
            let name = path.file_name().unwrap().to_string_lossy();
            out.iter().find(|e| e.name == name).unwrap().file_id.0
        }

        #[test]
        fn a_backlog_larger_than_a_buffer_is_read_whole() {
            let dir = temp("backlog");
            let mut tail = JournalTail::open(&dir, None).unwrap();
            // Hundreds of KiB of records ahead of the one that matters.
            for i in 0..3000 {
                let p = dir.join(format!("f{i}"));
                std::fs::write(&p, b"x").unwrap();
                std::fs::remove_file(&p).unwrap();
            }
            let last = dir.join("last.txt");
            std::fs::write(&last, b"x").unwrap();
            let id = file_id(&last);
            std::fs::remove_file(&last).unwrap();
            assert!(tail.behind().unwrap() > 64 * 1024, "more than one read waiting");
            let (deleted, lost) = drain(&mut tail);
            assert!(!lost);
            assert!(deleted.contains(&id));
            let _ = std::fs::remove_dir_all(&dir);
        }

        #[test]
        fn a_position_the_journal_wrapped_past_goes_on_from_the_oldest_record() {
            let dir = temp("wrapped");
            let mut tail = JournalTail::open(&dir, None).unwrap();
            let (journal_id, _) = tail.position();
            // As if the disk outran the reads: a position before the oldest
            // record kept.
            tail.cursor = 8;
            let read = tail.next(false).unwrap();
            assert!(read.lost);
            assert!(tail.position().1 > 8);
            assert_eq!(tail.position().0, journal_id);
            // Opening at such a position starts at the oldest record instead.
            let tail = JournalTail::open(&dir, Some((journal_id, 8))).unwrap();
            assert!(tail.position().1 > 8);
            let _ = std::fs::remove_dir_all(&dir);
        }

        #[test]
        fn another_journals_position_starts_at_the_end() {
            let dir = temp("other");
            let now = JournalTail::open(&dir, None).unwrap().position();
            let tail = JournalTail::open(&dir, Some((now.0.wrapping_add(1), 0))).unwrap();
            assert_eq!(tail.position().0, now.0);
            assert!(tail.position().1 >= now.1);
            let _ = std::fs::remove_dir_all(&dir);
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
        let mut stat = |id: FileId, _| match live.iter().find(|e| e.file_id == id) {
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
        let r =
            s.try_write(|o| apply_records(o, ROOT_ID, &ExcludeSet::dev(), &records, &mut |_, _| Stat::Gone, &mut ch));
        assert!(matches!(r, Err(Error::NeedsRescan(Rescan::RootGone))));
        assert!(s.read(|o| o.local_get(4)).unwrap().is_some(), "the batch was rolled back");
    }

    const JOURNAL: u64 = 7;

    /// [`tree`] as a rescan of journal [`JOURNAL`] leaves it after `gone`
    /// were deleted while nothing read the journal: their rows dropped,
    /// without evidence.
    fn rescanned(gone: &[u128]) -> Store {
        let mut s = tree();
        s.write(|o| {
            let root = oneup_store::LocalRoot {
                path: r"C:\t".into(),
                volume_serial: 1,
                root_file_id: ROOT_ID,
                rules: String::new(),
                journal: Some((JOURNAL, 1000)),
                started_at_ms: 0,
                finished_at_ms: Some(1),
                sync_root_provider: None,
            };
            // `local_begin` starts the Local tree over; what's left goes back.
            o.local_begin(&root, &row(ROOT_ID, 0, "", LocalKind::Root))?;
            let rows = [
                row(2, ROOT_ID, "docs", LocalKind::Folder),
                row(3, 2, "a.txt", LocalKind::File),
                row(4, ROOT_ID, "b.txt", LocalKind::File),
            ];
            for r in rows.iter().filter(|r| !gone.contains(&r.file_id)) {
                o.local_update(r)?;
            }
            Ok(())
        })
        .unwrap();
        s
    }

    fn seen(s: &Store, id: u128) -> Seen<oneup_sync::LocalNode> {
        s.read(|o| o.local(LocalId(id))).unwrap()
    }

    #[test]
    fn delete_records_read_ahead_are_evidence_after_a_rescan() {
        // b.txt (synced) was deleted; the journal wrapped before the
        // observer read it, but the watcher had.
        let mut s = rescanned(&[4]);
        assert_eq!(seen(&s, 4), Seen::Missing, "without the record: held");
        let log = SeenDeletes::new();
        let kept = remember_deletes(&s, &log, JOURNAL, &[rec(4, ROOT_ID, reason::FILE_DELETE | reason::CLOSE)]);
        assert_eq!(kept.unwrap(), 1);
        assert_eq!(note_seen_deletes(&mut s, &log).unwrap(), 1);
        assert_eq!(seen(&s, 4), Seen::Deleted);
        assert!(s.read(|o| o.check_invariants()).unwrap().is_empty());
    }

    #[test]
    fn only_synced_items_absent_after_the_rescan_get_evidence() {
        let mut s = rescanned(&[3]);
        let log = SeenDeletes::new();
        let records = [
            // a.txt: deleted, but never synced, so nothing asks about it.
            rec(3, 2, reason::FILE_DELETE),
            // Elsewhere on the disk.
            rec(50, 49, reason::FILE_DELETE),
            // Not a delete.
            rec(4, ROOT_ID, reason::DATA_EXTEND | reason::CLOSE),
        ];
        assert_eq!(remember_deletes(&s, &log, JOURNAL, &records).unwrap(), 0);
        assert!(log.is_empty());
        // Even if a record of a present item got in, the item is there.
        log.saw(JOURNAL, 10, 4);
        log.saw(JOURNAL, 11, ROOT_ID);
        assert_eq!(note_seen_deletes(&mut s, &log).unwrap(), 0);
        assert!(seen(&s, 4).present().is_some());
        assert!(s.read(|o| o.check_invariants()).unwrap().is_empty());
    }

    #[test]
    fn records_of_another_journal_are_not_evidence() {
        let mut s = rescanned(&[4]);
        let log = SeenDeletes::new();
        log.saw(JOURNAL + 1, 10, 4);
        assert_eq!(note_seen_deletes(&mut s, &log).unwrap(), 0);
        assert_eq!(seen(&s, 4), Seen::Missing);
        // A new journal drops what an old one held.
        log.saw(JOURNAL, 5, 9);
        assert_eq!(log.ids(JOURNAL), [9]);
        assert!(log.ids(JOURNAL + 1).is_empty());
    }

    #[test]
    fn the_log_forgets_what_the_observer_read_and_stays_bounded() {
        let log = SeenDeletes::new();
        for usn in 0..10 {
            log.saw(JOURNAL, usn, usn as u128);
        }
        log.trim(JOURNAL + 1, 100);
        assert_eq!(log.len(), 10, "another journal's position says nothing");
        log.trim(JOURNAL, 6);
        assert_eq!(log.ids(JOURNAL), [6, 7, 8, 9]);
        for usn in 10..(SEEN_MAX as i64 + 10) {
            log.saw(JOURNAL, usn, 1);
        }
        assert_eq!(log.len(), SEEN_MAX);
        assert_eq!(log.ids(JOURNAL)[0], 1, "the oldest went first");
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
                &mut |_, _| Stat::Unreadable,
                &mut ch,
            )
        })
        .unwrap();
        assert_eq!(ch.unreadable, 1);
        assert_eq!(path(&s, 3).as_deref(), Some("/docs/a.txt"), "left as it was");
    }
}
