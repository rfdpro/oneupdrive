//! The resumable, read-only scanner.
//!
//! ## How it works
//! 1. The root folder is opened by path once, to learn its identity (volume
//!    serial + 128-bit file ID) and normalized path.
//! 2. Every folder to list sits in `local_scan_queue`. A *step* takes up to
//!    [`Config::batch_dirs`] of them (deepest first), opens each **by file
//!    ID**, lists it in large batched reads, and writes the children, the
//!    newly found subfolders and the dequeue of the listed folders in **one
//!    transaction**. A crash or Ctrl-C therefore loses at most one step, and a
//!    re-listed folder just upserts the same rows again.
//! 3. Opening by ID (rather than by path) makes the scan immune to the
//!    260-character path limit and to folders being renamed while it runs.
//! 4. Excluded folders, links, unpopulated cloud folders and unreadable
//!    folders are stored as single rows that say why they weren't listed;
//!    nothing below them is visited.
//!
//! ## Consistency
//! A scan of a live folder is not an atomic snapshot: things can change
//! while it runs. The change-journal position is recorded *before* the scan
//! starts, so the observer can replay everything that happened during it.
//!
//! ## Cost
//! Memory is bounded by [`Config::flush_entries`] (plus the single largest
//! folder, which is listed in one go) and the store's page cache. Spike 0001
//! measured this listing method at ~13k entries/s cold and ~48k warm.

use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering::Relaxed};
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

use oneup_ntfs::FileId;
use oneup_ntfs::dirent::DirEntry;
use oneup_ntfs::win::{self, Volume};
use oneup_rules::ExcludeSet;
use oneup_store::{LocalKind, LocalRoot, LocalRow, LocalStats, PutOutcome, Skip, Store};

use crate::classify;
use crate::coexist::{self, OtherEngine};

#[derive(Debug, thiserror::Error)]
pub enum Error {
    #[error(transparent)]
    Store(#[from] oneup_store::Error),
    #[error("{0}")]
    Io(#[from] win::Error),
    #[error("{} is not a folder", .0.display())]
    NotAFolder(PathBuf),
    #[error("{} is a link to another location; scan the folder it points to instead", .0.display())]
    RootIsLink(PathBuf),
    #[error(
        "can't scan {}: only folders on local NTFS or ReFS drives are supported for now ({source})",
        path.display()
    )]
    Unsupported { path: PathBuf, source: win::Error },
    #[error(transparent)]
    Placeholders(#[from] oneup_cfapi::PlaceholderModeError),
}

pub type Result<T> = std::result::Result<T, Error>;

/// Tuning knobs. The defaults suit every memory profile: the scan's memory
/// use is dominated by the store's page cache, which the caller sizes.
#[derive(Debug, Clone)]
pub struct Config {
    /// Folders the scan does not descend into (stored as one row each).
    pub rules: ExcludeSet,
    /// Folders listed per step (and so per transaction).
    pub batch_dirs: usize,
    /// A step ends early once this many entries are waiting to be written.
    pub flush_entries: usize,
    /// Listing buffer. 256 KiB is what the enumeration speed was measured with.
    pub buffer_bytes: usize,
}

impl Config {
    /// Defaults from measurement:
    /// the store's cost is dominated by per-commit WAL work and checkpoints,
    /// so steps are large. 4,096 folders per transaction wrote 600k rows 6.5×
    /// faster than 256. A step still takes well under a second, which is all
    /// an interruption can lose.
    pub fn new(rules: ExcludeSet) -> Self {
        Self { rules, batch_dirs: 4096, flush_entries: 32_768, buffer_bytes: 256 * 1024 }
    }
}

/// Live counters for progress display (this run only, not cumulative across resumes).
#[derive(Debug, Default)]
pub struct Progress {
    pub folders_listed: AtomicU64,
    pub entries: AtomicU64,
    pub excluded: AtomicU64,
    pub unpopulated: AtomicU64,
    pub unreadable: AtomicU64,
    pub links: AtomicU64,
    /// File IDs seen under a second name: hard links, or entries moved mid-scan.
    pub duplicates: AtomicU64,
    /// Time spent opening and listing folders vs. writing to the store, in
    /// microseconds. Shows which side limits a scan's speed.
    pub list_us: AtomicU64,
    pub store_us: AtomicU64,
}

/// What happened to a previously stored scan when this one was opened.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Replaced {
    /// It was complete; a scan always starts over once finished.
    Finished,
    /// It was of a different folder (or the folder was replaced: same path,
    /// different identity, e.g. deleted and recreated or another disk).
    OtherFolder,
    /// The exclusion rules changed; mixing two rule sets in one tree would
    /// make it wrong, so it starts over.
    RulesChanged,
    /// It was made by an older scanner whose rows can't be trusted (see
    /// `oneup_store::LOCAL_SCAN_FORMAT`), so it starts over.
    OlderScanner,
    /// Another sync app held the folder when the unfinished scan started and
    /// doesn't now, or the other way round (an unlinking happened between):
    /// the half already listed doesn't describe the folder, so it starts over.
    ManagerChanged,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Start {
    /// No earlier scan was stored.
    Fresh,
    /// Continues an unfinished scan of the same folder with the same rules.
    Resumed,
    /// An earlier scan was discarded.
    Replaced(Replaced),
}

#[derive(Debug, Clone)]
pub struct Summary {
    /// False if cancelled before the queue emptied (run again to resume).
    pub finished: bool,
    pub start: Start,
    pub root: LocalRoot,
    pub stats: LocalStats,
    pub duplicates: u64,
    pub elapsed: Duration,
}

/// A scan in progress. Created by [`Scan::open`], driven by [`Scan::run`]
/// (or [`Scan::step`] for fine-grained control).
pub struct Scan {
    store: Store,
    volume: Volume,
    cfg: Config,
    root: LocalRoot,
    start: Start,
    other_engine: Option<OtherEngine>,
    buf: Vec<u8>,
    progress: Arc<Progress>,
    cancel: Arc<AtomicBool>,
}

fn now_ms() -> i64 {
    SystemTime::now().duration_since(UNIX_EPOCH).map(|d| d.as_millis() as i64).unwrap_or(0)
}

impl Scan {
    /// Opens `path` for scanning: resumes a matching unfinished scan, or
    /// starts a new one (discarding whatever was stored before).
    pub fn open(mut store: Store, path: &Path, cfg: Config) -> Result<Self> {
        // Before anything is looked at: otherwise Windows may disguise cloud
        // placeholders as ordinary files, in listings and on handles alike.
        oneup_cfapi::expose_placeholders()?;
        let handle = win::open_dir(path)?;
        let (attributes, reparse_tag) = win::attribute_tag(&handle)?;
        let as_entry = DirEntry {
            file_id: FileId::default(),
            name: String::new(),
            name_lossy: false,
            size: 0,
            attributes,
            reparse_tag,
            created: 0,
            modified: 0,
            changed: 0,
        };
        if !as_entry.is_dir() {
            return Err(Error::NotAFolder(path.to_path_buf()));
        }
        if classify::kind_of(&as_entry) == LocalKind::Link {
            return Err(Error::RootIsLink(path.to_path_buf()));
        }
        let (volume_serial, root_id) = win::file_id(&handle)?;
        let normalized = win::final_path(&handle)?;
        drop(handle);
        let volume = Volume::open_containing(&normalized)
            .map_err(|source| Error::Unsupported { path: normalized.clone(), source })?;
        let other_engine = coexist::other_engine(&normalized);

        let rules = cfg.rules.fingerprint();
        let previous = store.read(|o| o.local_root())?;
        let start = match &previous {
            None => Start::Fresh,
            Some(p) if p.volume_serial != volume_serial || p.root_file_id != root_id.0 => {
                Start::Replaced(Replaced::OtherFolder)
            }
            Some(p) if p.rules != rules => Start::Replaced(Replaced::RulesChanged),
            Some(_) if !store.read(|o| o.local_scan_is_current())? => Start::Replaced(Replaced::OlderScanner),
            Some(p)
                if p.finished_at_ms.is_none()
                    && p.sync_root_provider != other_engine.as_ref().map(OtherEngine::label) =>
            {
                Start::Replaced(Replaced::ManagerChanged)
            }
            Some(p) if p.finished_at_ms.is_some() => Start::Replaced(Replaced::Finished),
            Some(_) => Start::Resumed,
        };

        let root = if let (Start::Resumed, Some(p)) = (start, previous) {
            p
        } else {
            // Take the journal position *first*, so every change made during
            // the scan is still in the journal afterwards (if it doesn't wrap).
            let journal = volume.query_journal().ok().map(|j| (j.journal_id, j.next_usn));
            let root = LocalRoot {
                path: normalized.to_string_lossy().into_owned(),
                volume_serial,
                root_file_id: root_id.0,
                rules,
                journal,
                started_at_ms: now_ms(),
                finished_at_ms: None,
                sync_root_provider: other_engine.as_ref().map(OtherEngine::label),
            };
            let root_row = LocalRow {
                file_id: root_id.0,
                parent_id: None,
                name: String::new(),
                name_lossy: false,
                kind: LocalKind::Root,
                size: 0,
                mtime: 0,
                attributes,
                reparse_tag,
                skip: Skip::None,
            };
            store.write(|o| {
                o.local_begin(&root, &root_row)?;
                // Scanning an online-only folder would make its provider
                // download the folder's listing, so the root stays unlisted.
                if classify::is_unpopulated(attributes, reparse_tag) {
                    o.local_set_skip(root_id.0, Skip::Unpopulated)?;
                    o.local_dequeue(root_id.0)?;
                }
                Ok(())
            })?;
            root
        };
        tracing::info!(?start, "local scan opened");
        tracing::debug!(path = %root.path, "local scan root");

        Ok(Self {
            store,
            volume,
            buf: vec![0u8; cfg.buffer_bytes.max(4096)],
            cfg,
            root,
            start,
            other_engine,
            progress: Arc::default(),
            cancel: Arc::default(),
        })
    }

    pub fn start(&self) -> Start {
        self.start
    }

    pub fn root(&self) -> &LocalRoot {
        &self.root
    }

    /// Another sync engine that manages (part of) this folder, if any.
    pub fn other_engine(&self) -> Option<&OtherEngine> {
        self.other_engine.as_ref()
    }

    pub fn progress(&self) -> Arc<Progress> {
        Arc::clone(&self.progress)
    }

    /// Setting this flag stops [`Scan::run`] after the current step.
    pub fn cancel_flag(&self) -> Arc<AtomicBool> {
        Arc::clone(&self.cancel)
    }

    /// Lists one batch of queued folders and commits the result.
    /// Returns `false` once the queue is empty and the scan is marked finished.
    pub fn step(&mut self) -> Result<bool> {
        if list_queued(&mut self.store, &self.volume, &self.cfg, &mut self.buf, &self.progress)? == 0 {
            self.store.write(|o| o.local_finish(now_ms()))?;
            self.root.finished_at_ms = self.store.read(|o| o.local_root())?.and_then(|r| r.finished_at_ms);
            return Ok(false);
        }
        Ok(true)
    }

    /// Runs steps until the scan finishes or the cancel flag is set.
    pub fn run(&mut self) -> Result<Summary> {
        let started = Instant::now();
        let mut finished = false;
        while !self.cancel.load(Relaxed) {
            if !self.step()? {
                finished = true;
                break;
            }
        }
        let stats = self.store.read(|o| o.local_stats())?;
        Ok(Summary {
            finished,
            start: self.start,
            root: self.root.clone(),
            stats,
            duplicates: self.progress.duplicates.load(Relaxed),
            elapsed: started.elapsed(),
        })
    }

    /// Gives the store back (e.g. to inspect it after a run in tests).
    pub fn into_store(self) -> Store {
        self.store
    }
}

/// Lists one batch of queued folders and commits the result. Returns how
/// many folders were taken off the queue (0 = the queue is empty). Shared
/// by the scan and by the change-journal observer, which queues folders
/// that appear in the tree (created, or moved in from elsewhere).
pub(crate) fn list_queued(
    store: &mut Store,
    volume: &Volume,
    cfg: &Config,
    buf: &mut [u8],
    p: &Progress,
) -> Result<usize> {
    let batch = store.read(|o| o.local_next_dirs(cfg.batch_dirs))?;
    if batch.is_empty() {
        return Ok(0);
    }

    // List first (I/O, no transaction open), then write everything at once.
    let t_list = Instant::now();
    let mut listed: Vec<(u128, u32, Option<Vec<DirEntry>>)> = Vec::new();
    let mut pending = 0usize;
    for (id, depth) in batch {
        let mut entries = Vec::new();
        let result = volume.open_dir_by_id(FileId(id)).and_then(|h| win::list_dir(&h, buf, &mut entries));
        match result {
            Ok(()) => {
                pending += entries.len();
                listed.push((id, depth, Some(entries)));
            }
            Err(e) => {
                // Usually access denied; also a folder deleted since it was
                // queued. Either way: record it, count it, move on.
                tracing::debug!(error = %e, access_denied = e.is_access_denied(), "folder could not be listed");
                listed.push((id, depth, None));
            }
        }
        if pending >= cfg.flush_entries {
            break;
        }
    }

    let rules = &cfg.rules;
    let taken = listed.len();
    p.list_us.fetch_add(t_list.elapsed().as_micros() as u64, Relaxed);
    let t_store = Instant::now();
    store.write(|o| {
        for (id, depth, entries) in &listed {
            match entries {
                None => {
                    o.local_set_skip(*id, Skip::Unreadable)?;
                    p.unreadable.fetch_add(1, Relaxed);
                }
                Some(entries) => {
                    for e in entries {
                        // Only the scanned folder itself is queued at depth 0.
                        let row = classify::to_row(e, *id, *depth == 0, rules);
                        if o.local_put(&row)? == PutOutcome::Duplicate {
                            p.duplicates.fetch_add(1, Relaxed);
                            continue;
                        }
                        match (row.kind, row.skip) {
                            (LocalKind::Folder, Skip::None) => o.local_enqueue(row.file_id, depth + 1)?,
                            (LocalKind::Folder, Skip::Excluded) => _ = p.excluded.fetch_add(1, Relaxed),
                            (LocalKind::Folder, Skip::Unpopulated) => _ = p.unpopulated.fetch_add(1, Relaxed),
                            (LocalKind::Link, _) => _ = p.links.fetch_add(1, Relaxed),
                            _ => {}
                        }
                    }
                    p.entries.fetch_add(entries.len() as u64, Relaxed);
                    p.folders_listed.fetch_add(1, Relaxed);
                }
            }
            o.local_dequeue(*id)?;
        }
        Ok(())
    })?;
    p.store_us.fetch_add(t_store.elapsed().as_micros() as u64, Relaxed);
    Ok(taken)
}
