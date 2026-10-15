//! The Remote tree: a metadata mirror of the user's OneDrive.
//!
//! [`Mirror::run`] brings the mirror up to date:
//!
//! 1. **Cursor first.** On a fresh mirror it takes a `token=latest` delta
//!    cursor *before* enumerating anything, so changes made during a long
//!    enumeration are caught afterwards.
//! 2. **Parallel, pruned enumeration.** The top levels are listed
//!    (`children`), excluded folders are never descended into, and each
//!    folder at the split depth becomes a folder-scoped delta *unit*. Units
//!    run concurrently under the shared AIMD limiter with `$top=1000` and a
//!    minimal `$select`. Progress is committed page by page, so an
//!    interrupted run resumes where it stopped.
//! 3. **Orphan resolution.** Entries that arrived before their parent are
//!    placed, fetching unknown parents if needed.
//! 4. **Catch-up.** Delta from the cursor. Folders that appear with content
//!    (moved in from an excluded area, or newly included) get their own units.
//!
//! Where to list and where to use delta is steered by a [`SplitHint`] when one
//! is available (from a local scan or a previous mirror), or by folder size.
//!
//! Everything is read-only with respect to OneDrive.

pub mod hint;
pub mod ingest;
mod mirror;

pub use hint::SplitHint;
pub use mirror::{Config, Mirror, Phase, Progress, RemoteApi, Summary};

#[derive(Debug, thiserror::Error)]
pub enum Error {
    #[error(transparent)]
    Graph(#[from] oneup_graph::Error),
    #[error(transparent)]
    Store(#[from] oneup_store::Error),
    #[error("cancelled; run again to resume")]
    Cancelled,
    #[error("OneDrive asked for a full resync (the delta cursor expired); rebuild the mirror")]
    ResyncRequired,
    #[error("mirror inconsistency: {0}")]
    Inconsistent(String),
    #[error("internal task failed: {0}")]
    Task(String),
}

pub type Result<T> = std::result::Result<T, Error>;
