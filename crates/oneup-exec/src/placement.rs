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

/// Downloads pinned files whose content isn't here (through our own
/// hydration service, which must be connected). Returns their paths
/// relative to the synced folder. Blocking work runs off the async runtime.
pub async fn fetch_pinned(store: &Store, root: &Path) -> oneup_store::Result<Vec<String>> {
    let rows = store.read(|o| {
        let mut v = Vec::new();
        for r in o.local_to_fetch(BATCH)? {
            if let Some(p) = o.local_path_of(r.file_id)? {
                v.push(p);
            }
        }
        Ok(v)
    })?;
    let mut done = Vec::new();
    for rel in rows {
        let path: PathBuf = rel.trim_start_matches('/').split('/').fold(root.to_path_buf(), |p, part| p.join(part));
        let ok = tokio::task::spawn_blocking(move || -> bool {
            oneup_cfapi::provider::open(&path).is_ok_and(|f| oneup_cfapi::provider::hydrate(&f).is_ok())
        })
        .await
        .unwrap_or(false);
        if ok {
            done.push(rel);
        } else {
            tracing::debug!("a pinned file couldn't be downloaded now; tried again next pass");
        }
    }
    Ok(done)
}
