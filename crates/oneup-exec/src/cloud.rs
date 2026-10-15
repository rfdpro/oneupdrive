//! What the executor needs from the cloud. Implemented over Microsoft Graph
//! (step 5) and by an in-memory fake in tests.
//!
//! Every mutation is conditional, the remote half of `data-safety.md`
//! invariant 5: changes to an existing item carry its planned eTag
//! (`If-Match`), and creations fail rather than replace when the name is
//! taken (`@microsoft.graph.conflictBehavior = fail`).
//!
//! **Content hashes are the cloud's.** A file's hash, in the Remote tree,
//! the Synced tree and a placeholder's identity, is the QuickXorHash of the
//! content as OneDrive stores it. For an ordinary pair that's the file's
//! own content. A cloud that transforms content on the way (a vault, which
//! stores it encrypted) says so by returning, from [`Cloud::download`] and
//! [`Cloud::upload`], the hash of what it actually received or sent, which
//! it verified itself; the executor then uses that hash instead of one of
//! the bytes it saw.

use std::io::{Read, Write};

use oneup_store::RemoteRow;
use oneup_sync::{RemoteId, RemotePre};

#[derive(Debug, thiserror::Error)]
pub enum CloudError {
    /// The item's eTag is no longer the planned one (HTTP 412), or it no
    /// longer exists (404). Nothing was changed; plan again.
    #[error("the cloud item changed since it was planned")]
    Changed,
    /// The name is taken in the destination folder (HTTP 409).
    #[error("the name is already taken in the cloud")]
    NameTaken,
    /// A folder to delete still has contents in the cloud: added since,
    /// or items the rules leave out, which the Remote tree doesn't list.
    #[error("the cloud folder isn't empty")]
    NotEmpty,
    /// Writing the downloaded content locally failed.
    #[error(transparent)]
    Io(#[from] std::io::Error),
    /// Anything else (network, throttling exhausted, server error).
    #[error("{0}")]
    Other(String),
}

/// A change to cloud metadata, alone or in a batch ([`Cloud::batch`]).
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Meta {
    Mkdir { parent: RemoteId, name: String },
    Move { pre: RemotePre, to: RemoteId, name: String },
    Delete { pre: RemotePre },
}

/// What a [`Meta`] change did.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum MetaDone {
    /// The created or moved item, as the cloud now has it.
    Item(RemoteRow),
    Deleted,
}

/// A content hash (QuickXorHash).
pub type Hash = [u8; 20];

pub trait Cloud: Send + Sync {
    /// Streams the content of file `pre.id` into `out`, provided its eTag
    /// is still `pre.etag`. `Some(hash)`: the content was transformed on
    /// the way, and `hash` is that of what OneDrive sent (see the module
    /// docs); `None`: `out` got exactly what OneDrive holds.
    fn download(
        &self,
        pre: &RemotePre,
        out: &mut (dyn Write + Send),
    ) -> impl Future<Output = Result<Option<Hash>, CloudError>> + Send;

    /// Uploads `size` bytes from `src` as `name` in folder `parent`. With
    /// `replace`, as a new version of that item (which keeps its ID and
    /// history) provided its eTag still matches; without, only if the name
    /// is free. Returns the item as the cloud now has it, with its hash,
    /// and, if the content was transformed on the way, the hash of what
    /// was sent (see [`Cloud::download`]).
    fn upload(
        &self,
        parent: &RemoteId,
        name: &str,
        replace: Option<&RemotePre>,
        size: i64,
        src: &mut (dyn Read + Send),
    ) -> impl Future<Output = Result<(RemoteRow, Option<Hash>), CloudError>> + Send;

    /// Whether [`Cloud::copy`] can stand in for an upload of the same
    /// content (not for a vault: equal files encrypt differently).
    fn can_copy(&self) -> bool {
        true
    }

    /// Has OneDrive copy file `source` (provided its eTag still matches and
    /// its content is still `hash`) into `parent` as `name`, only if the
    /// name is free: a server-side copy instead of an upload of the same
    /// content. Returns the new item, with its hash, once the copy is done.
    fn copy(
        &self,
        source: &RemotePre,
        hash: &[u8; 20],
        parent: &RemoteId,
        name: &str,
    ) -> impl Future<Output = Result<RemoteRow, CloudError>> + Send;

    /// Creates folder `name` in `parent`, only if the name is free.
    fn mkdir(&self, parent: &RemoteId, name: &str) -> impl Future<Output = Result<RemoteRow, CloudError>> + Send;

    /// Moves/renames the item, provided its eTag still matches and the
    /// target name is free.
    fn move_item(
        &self,
        pre: &RemotePre,
        to: &RemoteId,
        name: &str,
    ) -> impl Future<Output = Result<RemoteRow, CloudError>> + Send;

    /// Deletes the item into OneDrive's recycle bin, provided its eTag
    /// still matches. Folders only when empty (the planner guarantees it;
    /// implementations should refuse otherwise, because Graph would delete
    /// the contents along with it).
    fn delete(&self, pre: &RemotePre) -> impl Future<Output = Result<(), CloudError>> + Send;
    /// Deletes the item **with everything in it** into OneDrive's recycle
    /// bin, provided its eTag still matches. Only for
    /// [`oneup_sync::Op::PruneRemote`]: an item the user's rules exclude
    /// and the user chose to remove from OneDrive.
    fn prune(&self, pre: &RemotePre) -> impl Future<Output = Result<(), CloudError>> + Send;

    /// The item as the cloud has it now (`None`: gone). Read-only; for a
    /// change that needs an item's current eTag after changing what's
    /// inside it (a folder's eTag moves with its contents).
    fn item(&self, id: &RemoteId) -> impl Future<Output = Result<Option<RemoteRow>, CloudError>> + Send {
        let _ = id;
        async { Err(CloudError::Other("this cloud can't re-read items".into())) }
    }

    /// Several independent metadata changes (the caller makes sure none
    /// depends on another), each with its own precondition and outcome, in
    /// order. By default one after another; over Graph, in one request.
    fn batch(&self, ops: &[Meta]) -> impl Future<Output = Vec<Result<MetaDone, CloudError>>> + Send {
        async move {
            let mut out = Vec::with_capacity(ops.len());
            for m in ops {
                out.push(match m {
                    Meta::Mkdir { parent, name } => self.mkdir(parent, name).await.map(MetaDone::Item),
                    Meta::Move { pre, to, name } => self.move_item(pre, to, name).await.map(MetaDone::Item),
                    Meta::Delete { pre } => self.delete(pre).await.map(|()| MetaDone::Deleted),
                });
            }
            out
        }
    }
}

/// Passes bytes through while computing their QuickXorHash, so content is
/// verified without being read twice.
pub(crate) struct Hashing<T> {
    pub inner: T,
    pub hash: oneup_hash::QuickXorHash,
}

impl<T> Hashing<T> {
    pub fn new(inner: T) -> Self {
        Self { inner, hash: oneup_hash::QuickXorHash::new() }
    }
}

impl<T: Read> Read for Hashing<T> {
    fn read(&mut self, buf: &mut [u8]) -> std::io::Result<usize> {
        let n = self.inner.read(buf)?;
        self.hash.update(&buf[..n]);
        Ok(n)
    }
}

impl<T: Write> Write for Hashing<T> {
    fn write(&mut self, buf: &[u8]) -> std::io::Result<usize> {
        let n = self.inner.write(buf)?;
        self.hash.update(&buf[..n]);
        Ok(n)
    }
    fn flush(&mut self) -> std::io::Result<()> {
        self.inner.flush()
    }
}
