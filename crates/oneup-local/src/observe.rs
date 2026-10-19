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
//! * Any other record means "look again". Its parent comes from the
//!   record, but unprivileged records carry no names (measured), so the
//!   item is read from its parent's listing without being opened ([`Lookup`]:
//!   while a file is open, some programs can't save it by renaming another
//!   file over it, see `win::Volume`). It's looked up there under its
//!   stored name, then under names other items of that folder gave up in
//!   the same batch (a save by rename leaves the new file with the old
//!   one's name), each one lookup in the folder's index; failing those, a
//!   folder of at most [`LIST_WHOLE_MAX`] entries is listed whole, once per
//!   batch (stopping past that many: the folder may have filled since). Only
//!   an item with a new name in a larger folder is opened, by file ID
//!   ([`Volume::stat_in`]), and only if still in the record's folder. Not
//!   in the record's folder any more: it moved or went since, it's left as
//!   it was (never taken for deleted), and its later record says so. Then:
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
//! records of items in the trees ([`remember_deletes`]), and after a
//! rescan they're noted as evidence for the items the rescan didn't find
//! ([`note_seen_deletes`]). They are the same USN delete records the
//! observer would have applied; nothing is inferred from absence.
//!
//! ## Cost
//! Proportional to the journal's records, not the tree: a few indexed
//! lookups per mentioned ID, and per item inside the tree a lookup or two
//! in its folder's index (or a share of one whole listing of a folder of at
//! most [`LIST_WHOLE_MAX`] entries per batch). Folders are listed only when
//! they enter the tree or for such a lookup. The journal is volume-wide, so
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
    /// Items that had to be opened by ID to be read: a new name in a
    /// folder too large to list whole (see the module docs).
    pub opened_by_id: u64,
    /// Folders whose children changed (old and new parents, and folders
    /// that entered the tree). The sync engine plans exactly these.
    pub dirty: BTreeSet<u128>,
}

/// What reading an item a record mentions found.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Stat {
    Found(DirEntry),
    /// No such item (any more), or not in the record's folder now. If it
    /// was deleted, its delete record follows; if it moved, its record of
    /// the move does.
    Gone,
    /// It exists but couldn't be read.
    Unreadable,
}

/// An item to read, with what the store knows that lets it be read from
/// its folder's listing, without opening it (see the module docs).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Lookup {
    pub id: FileId,
    /// The folder the record places it in (a listed folder of the tree).
    pub parent: FileId,
    /// Names to look up in `parent`, most likely first: its stored name,
    /// then names that items of `parent` gave up in the same batch (a save
    /// that renames a new file over the old one, or moves the old one
    /// aside first, leaves the new file with the old one's name).
    pub names: Vec<String>,
    /// Whether `parent` is small enough to list whole when no name finds
    /// the item ([`LIST_WHOLE_MAX`]).
    pub list_whole: bool,
}

/// The most entries a folder may hold to be listed whole for an item none
/// of whose [`Lookup::names`] finds it (a new name). Listed once per batch,
/// for all of its items that need it. The live listing stops past this many
/// entries as well (the store's count can be behind). A larger folder's item is opened by
/// ID instead: listing it per batch would cost more than the folder's
/// changes, and that open is brief.
pub const LIST_WHOLE_MAX: usize = 4096;

/// At most this many [`Lookup::names`] are tried: each is one lookup in the
/// folder's index.
const NAMES_MAX: usize = 8;

/// Whether `id` is a stored folder whose contents are listed, i.e. a place
/// in the tree that children can occupy.
fn listed_folder(o: &Ops<'_>, id: u128) -> Result<bool> {
    Ok(o.local_get(id)?.is_some_and(|r| matches!(r.kind, LocalKind::Folder | LocalKind::Root) && r.skip == Skip::None))
}

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

/// Applies one batch of journal records to the Local tree (see the module
/// docs). Pure apart from `stat`, which reads an item's current metadata,
/// so it can be tested with made-up records.
pub fn apply_records(
    o: &Ops<'_>,
    root_id: u128,
    rules: &ExcludeSet,
    records: &[UsnRecord],
    stat: &mut dyn FnMut(&Lookup) -> Stat,
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
    let freed = freed_names(o, records)?;
    let mut small: HashMap<u128, bool> = HashMap::new();

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
        let mut names: Vec<String> = known.iter().map(|k| k.name.clone()).collect();
        for n in freed.get(&parent).into_iter().flatten().rev() {
            if names.len() >= NAMES_MAX {
                break;
            }
            if !names.contains(n) {
                names.push(n.clone());
            }
        }
        // Counted once per folder and batch, and never past the bound.
        let list_whole = match small.get(&parent) {
            Some(&s) => s,
            None => {
                let s = o.local_child_count_upto(parent, LIST_WHOLE_MAX + 1)? <= LIST_WHOLE_MAX;
                small.insert(parent, s);
                s
            }
        };
        let entry = match stat(&Lookup { id: r.file_id, parent: r.parent_id, names, list_whole }) {
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

/// The stored names that items deleted or renamed in `records` gave up, by
/// folder, in record order: as the store had them before the batch. One
/// indexed lookup per such item.
fn freed_names(o: &Ops<'_>, records: &[UsnRecord]) -> Result<HashMap<u128, Vec<String>>> {
    let mut freed: HashMap<u128, Vec<String>> = HashMap::new();
    let mut seen = std::collections::HashSet::new();
    for r in records.iter().filter(|r| r.has(reason::FILE_DELETE) || r.has(reason::RENAME_OLD_NAME)) {
        if !seen.insert(r.file_id.0) {
            continue;
        }
        if let Some(row) = o.local_get(r.file_id.0)?
            && let Some(parent) = row.parent_id
        {
            freed.entry(parent).or_default().push(row.name);
        }
    }
    Ok(freed)
}

/// The most delete records [`SeenDeletes`] keeps (32 bytes each, so at most
/// 8 MiB). Only records of items in the trees are kept, and only until the
/// observer has read past them; past this, the oldest go, and their items
/// are held after a rescan as they would be without the log.
const SEEN_MAX: usize = 1 << 18;

/// Delete records of items in the trees, read by the journal watcher ahead
/// of the observer (see the module docs). One per process
/// ([`seen_deletes`]); a process without a watcher leaves it empty. File IDs
/// belong to the volume, not the journal, so records from a journal since
/// replaced still count: a rescan uses them all ([`note_seen_deletes`]).
#[derive(Debug, Default)]
pub struct SeenDeletes {
    inner: Mutex<VecDeque<Segment>>,
}

/// One journal's records, oldest journal first.
#[derive(Debug)]
struct Segment {
    journal_id: u64,
    /// Where the observer has read this journal up to: older records are
    /// its already.
    floor: i64,
    /// (USN of the record, file ID), oldest first.
    ids: VecDeque<(i64, u128)>,
}

impl SeenDeletes {
    pub const fn new() -> Self {
        Self { inner: Mutex::new(VecDeque::new()) }
    }

    fn lock(&self) -> std::sync::MutexGuard<'_, VecDeque<Segment>> {
        self.inner.lock().unwrap_or_else(std::sync::PoisonError::into_inner)
    }

    /// Records that journal `journal_id` holds a delete record for `id` at
    /// `usn`. A record the observer has read past already (a watcher's read
    /// that lost the race with the observer) is refused.
    pub fn saw(&self, journal_id: u64, usn: i64, id: u128) {
        let mut s = self.lock();
        let seg = segment(&mut s, journal_id);
        if usn < seg.floor {
            return;
        }
        seg.ids.push_back((usn, id));
        // Past the bound, the oldest go first.
        let mut total: usize = s.iter().map(|g| g.ids.len()).sum();
        while total > SEEN_MAX {
            let Some(oldest) = s.front_mut() else { break };
            if oldest.ids.pop_front().is_none() && s.len() > 1 {
                s.pop_front();
                continue;
            }
            total -= 1;
        }
    }

    /// Forgets journal `journal_id`'s records before `cursor`, and refuses
    /// them from now on: the observer has read them.
    pub fn trim(&self, journal_id: u64, cursor: i64) {
        let mut s = self.lock();
        let seg = segment(&mut s, journal_id);
        seg.floor = seg.floor.max(cursor);
        while seg.ids.front().is_some_and(|&(usn, _)| usn < seg.floor) {
            seg.ids.pop_front();
        }
    }

    /// Every record held, of any journal, oldest first: (journal, USN, ID).
    pub fn pending(&self) -> Vec<(u64, i64, u128)> {
        let s = self.lock();
        s.iter().flat_map(|g| g.ids.iter().map(|&(usn, id)| (g.journal_id, usn, id))).collect()
    }

    /// Forgets `done` (from [`SeenDeletes::pending`]) once acted on, and
    /// whatever came before them in their journals; what the watcher added
    /// since stays. The floors stay too, so a late read of the same records
    /// is still refused.
    pub fn acknowledge(&self, done: &[(u64, i64, u128)]) {
        let mut s = self.lock();
        for g in s.iter_mut() {
            let Some(last) = done.iter().filter(|d| d.0 == g.journal_id).map(|d| d.1).max() else { continue };
            while g.ids.front().is_some_and(|&(usn, _)| usn <= last) {
                g.ids.pop_front();
            }
        }
    }

    /// The IDs held from journal `journal_id`.
    pub fn ids(&self, journal_id: u64) -> Vec<u128> {
        let s = self.lock();
        s.iter().filter(|g| g.journal_id == journal_id).flat_map(|g| g.ids.iter().map(|&(_, id)| id)).collect()
    }

    pub fn len(&self) -> usize {
        self.lock().iter().map(|g| g.ids.len()).sum()
    }

    pub fn is_empty(&self) -> bool {
        self.len() == 0
    }
}

/// Journal `journal_id`'s segment, made (as the newest) if there's none.
/// Journals are replaced rarely; a handful of segments at most.
fn segment(s: &mut VecDeque<Segment>, journal_id: u64) -> &mut Segment {
    let i = match s.iter().position(|g| g.journal_id == journal_id) {
        Some(i) => i,
        None => {
            s.push_back(Segment { journal_id, floor: 0, ids: VecDeque::new() });
            s.len() - 1
        }
    };
    &mut s[i]
}

static SEEN: SeenDeletes = SeenDeletes::new();

/// This process's [`SeenDeletes`].
pub fn seen_deletes() -> &'static SeenDeletes {
    &SEEN
}

/// Keeps, in `seen`, the delete records among `records` (journal
/// `journal_id`) of items in the Local or the Synced tree: evidence is only
/// ever recorded for Synced items, but an item just uploaded is in the
/// Local tree before its Synced row is written. One indexed lookup or two
/// per delete record, in one read. Returns how many were kept.
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
    let known = store.read(|o| {
        let mut keep = Vec::new();
        for r in deletes {
            let id = r.file_id.0;
            if o.synced_by_local(id)?.is_some() || o.local_get(id)?.is_some() {
                keep.push((r.usn, id));
            }
        }
        Ok(keep)
    })?;
    for &(usn, id) in &known {
        seen.saw(journal_id, usn, id);
    }
    Ok(known.len())
}

/// After a rescan: notes deletion evidence for the items `seen` holds delete
/// records of and that the rescan didn't find, then forgets those records
/// (kept if the write fails, for the next rescan). An item it found is left
/// alone: it's there. Only Synced items get evidence
/// (`Ops::note_local_deleted`). Returns how many did.
pub fn note_seen_deletes(store: &mut Store, seen: &SeenDeletes) -> oneup_store::Result<u64> {
    let Some(root) = store.read(|o| o.local_root())? else { return Ok(0) };
    let pending = seen.pending();
    if pending.is_empty() {
        return Ok(0);
    }
    let noted = store.write(|o| {
        let mut noted = 0;
        for &(_, _, id) in &pending {
            // The root's own delete record isn't evidence for anything
            // (a rescan wouldn't have opened without it anyway).
            if id != root.root_file_id && o.local_get(id)?.is_none() && o.note_local_deleted(id)? {
                noted += 1;
            }
        }
        Ok(noted)
    })?;
    seen.acknowledge(&pending);
    Ok(noted)
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

    use super::{Changes, Error, LIST_WHOLE_MAX, Lookup, Rescan, Result, Stat, apply_records};
    use crate::classify;
    use crate::scan::{Config, Progress, list_queued};
    use std::collections::HashMap;

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
                let by_id = volume.opened_by_id();
                // Whole listings, per folder, for this batch only.
                let mut listings: HashMap<u128, Option<Vec<DirEntry>>> = HashMap::new();
                let buf = &mut self.buf;
                let mut stat = |l: &Lookup| read_item(volume, l, &mut listings, buf);
                self.store.try_write(|o| {
                    apply_records(o, root_id, rules, &records, &mut stat, &mut ch)?;
                    o.local_set_journal_cursor(next)?;
                    Ok::<_, Error>(())
                })?;
                ch.opened_by_id += self.volume.opened_by_id() - by_id;
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
        /// Opens the journal of the volume holding `path` at `from` (a
        /// journal ID and position, e.g. the observer's), or at the oldest
        /// record kept if the journal no longer reaches back that far or
        /// was replaced since. Without `from`, at its end.
        pub fn open(path: &Path, from: Option<(u64, i64)>) -> win::Result<Self> {
            let volume = Volume::open_containing(path)?;
            let j = volume.query_journal()?;
            let cursor = match from {
                Some((id, usn)) if id == j.journal_id => usn.clamp(oldest(&j), j.next_usn),
                // The journal was replaced since: all of the new one.
                Some(_) => oldest(&j),
                None => j.next_usn,
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
                    // A record that doesn't parse stops the read (and the
                    // position) there, as it does the observer's.
                    Ok((next, records)) => {
                        let records = records.collect::<std::result::Result<Vec<_>, _>>()?;
                        break (next, records);
                    }
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

    /// Reads the item `l` names from its folder's listing, so the item
    /// itself is never opened: while a file is open, some programs can't
    /// save it by renaming another file over it (`win::Volume`'s docs).
    /// First its likely names, each one lookup in the folder's index
    /// (`Volume::listed_in`), then the whole listing of a small folder
    /// (once per batch, in `listings`). Only an item with a new name in a
    /// large folder is opened, by ID: nothing else tells its name.
    fn read_item(
        volume: &Volume,
        l: &Lookup,
        listings: &mut HashMap<u128, Option<Vec<DirEntry>>>,
        buf: &mut [u8],
    ) -> Stat {
        for name in &l.names {
            match volume.listed_in(l.parent, name, l.id) {
                Ok(Some(e)) => return Stat::Found(e),
                Ok(None) => {}
                // The folder itself went: so did the item, from there.
                Err(e) if e.is_not_found() => return Stat::Gone,
                Err(e) => {
                    tracing::debug!(error = %e, "a changed item's folder could not be read");
                    return Stat::Unreadable;
                }
            }
        }
        if l.list_whole {
            // The store's count can be behind the folder (filled since):
            // the listing stops past the bound too, and the folder then
            // counts as large for the rest of the batch.
            let listed = listings.entry(l.parent.0).or_insert_with(|| {
                let mut out = Vec::new();
                match volume
                    .open_dir_by_id(l.parent)
                    .and_then(|h| win::list_dir_upto(&h, buf, &mut out, LIST_WHOLE_MAX))
                {
                    Ok(true) => Some(out),
                    Ok(false) => None,
                    Err(e) => {
                        tracing::debug!(error = %e, "a changed item's folder could not be listed");
                        None
                    }
                }
            });
            if let Some(entries) = listed {
                return entries.iter().find(|e| e.file_id == l.id).map_or(Stat::Gone, |e| Stat::Found(e.clone()));
            }
        }
        // Only if it's still in the record's folder: one that moved since
        // has a later record, which places it; stored here, it would be
        // planned in the wrong folder meanwhile.
        match volume.stat_in(l.id, l.parent) {
            Ok(Some(e)) => as_listed(volume, l.parent, e),
            Ok(None) => Stat::Gone,
            Err(e) => {
                tracing::debug!(error = %e, "changed item could not be read");
                Stat::Unreadable
            }
        }
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

        /// An item opened by ID (a new name in a large folder) that moved
        /// to another folder after its record was written is not taken to
        /// be in the record's folder: its later record places it.
        #[test]
        fn an_item_read_by_id_counts_only_in_the_records_folder() {
            let dir = temp("moved");
            std::fs::create_dir_all(dir.join("a")).unwrap();
            std::fs::create_dir_all(dir.join("b")).unwrap();
            std::fs::write(dir.join("b").join("new.txt"), b"x").unwrap();
            let volume = Volume::open_containing(&dir).unwrap();
            let (a, b) = (FileId(file_id(&dir.join("a"))), FileId(file_id(&dir.join("b"))));
            let id = FileId(file_id(&dir.join("b").join("new.txt")));
            let read = |parent| {
                let l = Lookup { id, parent, names: Vec::new(), list_whole: false };
                read_item(&volume, &l, &mut HashMap::new(), &mut vec![0u8; 64 * 1024])
            };
            // The record said `a`; it's in `b` now. Opened by ID, it's
            // found, but not in `a`: not stored there, and not deleted.
            assert!(volume.stat_by_id(id).unwrap().is_some());
            assert_eq!(read(a), Stat::Gone);
            assert!(matches!(read(b), Stat::Found(e) if e.name == "new.txt"));
            assert_eq!(volume.opened_by_id(), 3);
            let _ = std::fs::remove_dir_all(&dir);
        }

        #[test]
        fn a_replaced_journal_is_read_from_its_oldest_record() {
            let dir = temp("other");
            // Deleted before the tail opens, after the position it's given.
            let f = dir.join("gone.txt");
            std::fs::write(&f, b"x").unwrap();
            let id = file_id(&f);
            std::fs::remove_file(&f).unwrap();
            let now = JournalTail::open(&dir, None).unwrap().position();
            let mut tail = JournalTail::open(&dir, Some((now.0.wrapping_add(1), now.1))).unwrap();
            assert_eq!(tail.position().0, now.0);
            assert!(tail.position().1 < now.1, "{:?} {now:?}", tail.position());
            let (deleted, _) = drain(&mut tail);
            assert!(deleted.contains(&id), "the new journal's records are read");
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
        apply_seen(s, records, live).0
    }

    /// [`apply`], also returning what each read was told.
    fn apply_seen(s: &mut Store, records: &[UsnRecord], live: &[DirEntry]) -> (Changes, Vec<Lookup>) {
        let mut ch = Changes::default();
        let mut asked = Vec::new();
        let mut stat = |l: &Lookup| {
            asked.push(l.clone());
            match live.iter().find(|e| e.file_id == l.id) {
                Some(e) => Stat::Found(e.clone()),
                None => Stat::Gone,
            }
        };
        s.try_write(|o| apply_records(o, ROOT_ID, &ExcludeSet::dev(), records, &mut stat, &mut ch)).unwrap();
        assert!(s.read(|o| o.check_invariants()).unwrap().is_empty());
        (ch, asked)
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
    fn items_are_looked_up_under_their_stored_and_freed_names() {
        let mut s = tree();
        // An edit; a save that renamed new file 6 over b.txt (4); a save
        // that moved a.txt (3) aside to `~old` first, then renamed 7 in.
        let (_, asked) = apply_seen(
            &mut s,
            &[
                rec(2, ROOT_ID, reason::BASIC_INFO_CHANGE),
                rec(6, ROOT_ID, reason::FILE_CREATE),
                rec(4, ROOT_ID, reason::FILE_DELETE),
                rec(6, ROOT_ID, reason::RENAME_NEW_NAME),
                rec(3, 2, reason::RENAME_OLD_NAME),
                rec(3, 2, reason::RENAME_NEW_NAME),
                rec(7, 2, reason::RENAME_NEW_NAME),
                rec(3, 2, reason::FILE_DELETE),
                rec(8, ROOT_ID, reason::FILE_CREATE),
            ],
            &[entry(2, "docs", DIR, 0), entry(6, "b.txt", 0, 2), entry(7, "a.txt", 0, 3), entry(8, "new.txt", 0, 1)],
        );
        let names = |id: u128| asked.iter().find(|l| l.id.0 == id).map(|l| l.names.clone()).unwrap();
        assert_eq!(names(2)[0], "docs", "its stored name first");
        assert_eq!(names(6), ["b.txt"], "the name b.txt gave up");
        assert_eq!(names(7), ["a.txt"]);
        assert!(names(8).contains(&"b.txt".to_string()) && asked.iter().all(|l| l.list_whole));
        assert_eq!(path(&s, 6).as_deref(), Some("/b.txt"));
        assert_eq!(path(&s, 7).as_deref(), Some("/docs/a.txt"));
    }

    #[test]
    fn a_large_folder_is_not_listed_whole() {
        let mut s = tree();
        s.write(|o| {
            for i in 0..=LIST_WHOLE_MAX as u128 {
                o.local_update(&row(1000 + i, 2, &format!("f{i}"), LocalKind::File))?;
            }
            Ok(())
        })
        .unwrap();
        let (_, asked) = apply_seen(
            &mut s,
            &[rec(9, 2, reason::FILE_CREATE), rec(10, ROOT_ID, reason::FILE_CREATE)],
            &[entry(9, "n9", 0, 1), entry(10, "n10", 0, 1)],
        );
        assert_eq!(asked.iter().map(|l| (l.id.0, l.list_whole)).collect::<Vec<_>>(), [(9, false), (10, true)]);
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
            // a.txt: deleted, never synced, and gone from the trees.
            rec(3, 2, reason::FILE_DELETE),
            // Elsewhere on the disk.
            rec(50, 49, reason::FILE_DELETE),
            // Not a delete.
            rec(4, ROOT_ID, reason::DATA_EXTEND | reason::CLOSE),
            // docs: in the Local tree (kept), but not synced (no evidence).
            rec(2, ROOT_ID, reason::FILE_DELETE),
        ];
        assert_eq!(remember_deletes(&s, &log, JOURNAL, &records).unwrap(), 1);
        assert_eq!(log.ids(JOURNAL), [2]);
        // Even if a record of a present item got in, the item is there.
        log.saw(JOURNAL, 10, 4);
        log.saw(JOURNAL, 11, ROOT_ID);
        assert_eq!(note_seen_deletes(&mut s, &log).unwrap(), 0);
        assert!(seen(&s, 4).present().is_some());
        assert!(s.read(|o| o.check_invariants()).unwrap().is_empty());
    }

    #[test]
    fn deletions_recorded_by_a_journal_since_replaced_are_evidence() {
        // Read from the old journal; the rescan continues a new one. File
        // IDs belong to the volume.
        let mut s = rescanned(&[4]);
        let log = SeenDeletes::new();
        log.saw(JOURNAL - 1, 10, 4);
        assert_eq!(note_seen_deletes(&mut s, &log).unwrap(), 1);
        assert_eq!(seen(&s, 4), Seen::Deleted);
        assert!(log.is_empty(), "acted on");
    }

    #[test]
    fn a_file_deleted_while_its_upload_is_recorded_gets_evidence() {
        // new.txt was uploaded; the user deleted it before the executor
        // wrote its Synced row, and the journal wrapped before the observer
        // read the delete.
        let mut s = tree();
        s.write(|o| o.local_update(&row(5, ROOT_ID, "new.txt", LocalKind::File))).unwrap();
        let log = SeenDeletes::new();
        assert_eq!(remember_deletes(&s, &log, JOURNAL, &[rec(5, ROOT_ID, reason::FILE_DELETE)]).unwrap(), 1);
        // Then the Synced row, and a rescan that doesn't find it.
        let mut s = rescanned(&[]);
        let base = Base {
            sid: 9,
            parent: Some(ROOT),
            name: "new.txt".into(),
            kind: NodeKind::File,
            size: 0,
            hash: None,
            local: Some(LocalId(5)),
            local_mtime: 5,
            remote: Some(RemoteId("r5".into())),
            remote_etag: String::new(),
        };
        s.write(|o| o.synced_put(&base)).unwrap();
        assert_eq!(seen(&s, 5), Seen::Missing);
        assert_eq!(note_seen_deletes(&mut s, &log).unwrap(), 1);
        assert_eq!(seen(&s, 5), Seen::Deleted);
    }

    #[test]
    fn the_log_forgets_what_the_observer_read_and_stays_bounded() {
        let log = SeenDeletes::new();
        for usn in 0..10 {
            log.saw(JOURNAL, usn, usn as u128);
        }
        log.trim(JOURNAL, 6);
        assert_eq!(log.ids(JOURNAL), [6, 7, 8, 9]);
        // A watcher's read that lost the race with the observer.
        log.saw(JOURNAL, 5, 99);
        assert_eq!(log.ids(JOURNAL), [6, 7, 8, 9], "already read by the observer");
        for usn in 10..(SEEN_MAX as i64 + 10) {
            log.saw(JOURNAL, usn, 1);
        }
        assert_eq!(log.len(), SEEN_MAX);
        assert_eq!(log.ids(JOURNAL)[0], 1, "the oldest went first");
        // Another journal keeps its own position; the bound is over all.
        log.trim(JOURNAL + 1, 100);
        log.saw(JOURNAL + 1, 50, 7);
        assert!(log.ids(JOURNAL + 1).is_empty(), "before the observer's position there");
        log.saw(JOURNAL + 1, 150, 8);
        assert_eq!(log.len(), SEEN_MAX);
        assert_eq!(log.ids(JOURNAL + 1), [8]);
        let pending = log.pending();
        assert_eq!(pending.len(), SEEN_MAX);
        log.saw(JOURNAL + 1, 160, 9);
        log.acknowledge(&pending);
        assert_eq!(log.ids(JOURNAL + 1), [9], "added after the snapshot: kept");
        log.acknowledge(&log.pending());
        assert!(log.is_empty());
        log.saw(JOURNAL, 5, 99);
        assert!(log.is_empty(), "the positions outlive a take");
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
