//! Files On-Demand placement (`docs/architecture/files-on-demand.md`
//! § Pinning and freeing space): after a round, the agent downloads files
//! pinned "Always keep on this device" whose content isn't here, and frees
//! files set to "Free up space" whose content still is. Windows does
//! neither by itself (measured, `oneup-cfapi` tests).
//!
//! * Fetching is a hydration: content arrives only through our own
//!   `FETCH_DATA`, verified (`hydrate.rs`).
//! * Freeing removes local content, so it goes through the safety gate and
//!   the executor as [`Op::Dehydrate`] operations (a paused "changes to this
//!   PC" withholds them), and only for the cloud's current version.
//!
//! Each pass handles a bounded batch; the next pass continues.

use std::path::{Path, PathBuf};

use oneup_store::Store;
use oneup_sync::{LocalId, LocalPre, Op};

/// At most this many files per pass, each way.
pub const BATCH: usize = 32;

/// The freeing work as operations for the gate: placeholders set to free
/// up space whose content is here and whose hash is on record.
pub fn free_ops(store: &Store) -> oneup_store::Result<Vec<Op>> {
    store.read(|o| {
        let mut ops = Vec::new();
        for r in o.local_to_free(BATCH)? {
            if let Some(hash) = o.local_hash_get(r.file_id, r.size, r.mtime)? {
                ops.push(Op::Dehydrate {
                    local: LocalPre { id: LocalId(r.file_id), stamp: Some((r.size, r.mtime)) },
                    hash,
                });
            }
        }
        Ok(ops)
    })
}

/// Why a pass of downloads stopped before its batch was done.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Stop {
    /// Changes to this PC were paused meanwhile.
    Paused,
    /// The disk would keep too little free space.
    LowDisk,
    /// The free space couldn't be read: nothing is downloaded blind.
    SpaceUnknown,
    /// The pass used its share of bytes or time; more follows soon.
    Share,
    /// Downloading more needs the person's go-ahead.
    Approval,
}

/// What a pass of downloads did.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Fetched {
    /// Downloaded: path relative to the synced folder, file ID and size.
    pub done: Vec<(String, u128, u64)>,
    /// Tried but not downloaded (failed or cut short): file IDs. Some of
    /// their bytes may have come.
    pub attempted: Vec<u128>,
    /// Why it stopped early, if it did.
    pub stopped: Option<Stop>,
    /// More may be waiting: the batch was full, or it stopped early.
    pub more: bool,
}

/// Downloads pinned files whose content isn't here (through our own
/// hydration service, which must be connected). Blocking work runs off the
/// async runtime. `may(file ID, size)` is asked before each file: an
/// error stops the pass there (a pause that came meanwhile, too little or
/// unknown free space, the pass's share used, a download that needs the
/// person's go-ahead), and says why.
/// Downloads one pinned file, `rel` relative to the synced folder (as
/// [`fetch_pinned`] does each). Whether it's here now.
pub async fn fetch_one(root: &Path, rel: &str) -> bool {
    let path: PathBuf = rel.trim_start_matches('/').split('/').fold(root.to_path_buf(), |p, part| p.join(part));
    tokio::task::spawn_blocking(move || -> bool {
        oneup_cfapi::provider::open(&path).is_ok_and(|f| oneup_cfapi::provider::hydrate(&f).is_ok())
    })
    .await
    .unwrap_or(false)
}

pub async fn fetch_pinned(
    store: &Store,
    root: &Path,
    may: &(dyn Fn(u128, u64) -> Result<(), Stop> + Send + Sync),
) -> oneup_store::Result<Fetched> {
    let rows = store.read(|o| {
        let mut v = Vec::new();
        for r in o.local_to_fetch(BATCH)? {
            if let Some(p) = o.local_path_of(r.file_id)? {
                v.push((p, r.file_id, u64::try_from(r.size).unwrap_or(0)));
            }
        }
        Ok(v)
    })?;
    let mut out = Fetched { more: rows.len() >= BATCH, ..Fetched::default() };
    for (rel, id, size) in rows {
        if let Err(why) = may(id, size) {
            out.stopped = Some(why);
            out.more = true;
            break;
        }
        if fetch_one(root, &rel).await {
            out.done.push((rel, id, size));
        } else {
            out.attempted.push(id);
            tracing::debug!("a pinned file couldn't be downloaded now; tried again next pass");
        }
    }
    Ok(out)
}
