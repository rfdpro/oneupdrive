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

use futures_util::future::BoxFuture;
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
    /// OneDrive won't move or rename this folder in one request: too many
    /// items below it (its "list view threshold"). Nothing was changed; the
    /// executor moves it in parts instead.
    #[error("OneDrive won't move a folder with this many items in one go")]
    TooManyItems,
    /// The OneDrive has no room left (HTTP 507, `quotaLimitReached`,
    /// `insufficientStorage`). Nothing was changed; uploads wait for room.
    #[error("OneDrive is full")]
    Full,
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

/// One version of a file in OneDrive's version history, as the history
/// lists it ([`Cloud::version_mark`]).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct VersionMark {
    pub id: String,
    /// When it was saved (Unix milliseconds).
    pub modified_ms: Option<i64>,
    pub size: Option<i64>,
}

/// What a replacement went over ([`Cloud::replaced_other`]).
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Replaced {
    /// The version it was meant to replace: nothing else to keep.
    Planned,
    /// Another one: an edit that reached OneDrive while it committed.
    Other(VersionMark),
    /// The history can't tell: it doesn't list the planned version, or
    /// nothing after it (not yet, or the replacement never committed).
    Unknown,
}

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

    /// Uploads `size` bytes from `src` as `name` in folder `parent`, read
    /// through [`Source::fill`] where the cloud can (see [`Source`]). With
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
        src: &mut dyn Source,
    ) -> impl Future<Output = Result<(RemoteRow, Option<Hash>), CloudError>> + Send;

    /// Before replacing file `pre` with `size` bytes: the version it holds
    /// now, if the replacement can commit without its precondition being
    /// checked then. Graph checks an upload session's `If-Match` only when
    /// the session starts; [`Cloud::upload`] re-checks the eTag before the
    /// last fragment, which leaves a moment for an edit in OneDrive to land
    /// first. `None`: nothing to check afterwards (a one-request upload
    /// commits under its `If-Match`). No default, like
    /// [`Cloud::children`]: a wrapper that didn't pass it on would lose the
    /// check without any test noticing.
    fn version_mark(
        &self,
        pre: &RemotePre,
        size: i64,
    ) -> impl Future<Output = Result<Option<VersionMark>, CloudError>> + Send;

    /// Right after an upload replaced file `id`, leaving eTag `etag`: the
    /// version its history lists the upload as, by its ID where it can
    /// (from the commit's answer; or read from the file, only while it
    /// still has `etag`). `None`: it can't be told. Read-only.
    fn uploaded_version(
        &self,
        id: &RemoteId,
        etag: Option<&str>,
    ) -> impl Future<Output = Result<Option<VersionMark>, CloudError>> + Send;

    /// After replacing file `id`, which held `mark`'s version, with the
    /// version `ours` ([`Cloud::uploaded_version`]): what the replacement
    /// went over. The history may not list `ours` yet: then it can't tell.
    /// Read-only.
    fn replaced_other(
        &self,
        id: &RemoteId,
        mark: &VersionMark,
        ours: &VersionMark,
    ) -> impl Future<Output = Result<Replaced, CloudError>> + Send;

    /// Streams the content of version `version` of file `id` into `out`;
    /// returns its size. Read-only.
    fn download_version(
        &self,
        id: &RemoteId,
        version: &str,
        out: &mut (dyn Write + Send),
    ) -> impl Future<Output = Result<u64, CloudError>> + Send;

    /// The file's versions in OneDrive's history, newest first (its current
    /// one included). Read-only. No default, like [`Cloud::children`].
    fn versions(&self, id: &RemoteId) -> impl Future<Output = Result<Vec<VersionMark>, CloudError>> + Send;

    /// Brings deleted item `id` back from OneDrive's recycle bin into folder
    /// `parent` as `name`, only if the name is free there; returns it as the
    /// cloud now has it. [`CloudError::Changed`]: it's no longer in the
    /// recycle bin. No default, like [`Cloud::children`].
    fn restore(
        &self,
        id: &RemoteId,
        parent: &RemoteId,
        name: &str,
    ) -> impl Future<Output = Result<RemoteRow, CloudError>> + Send;

    /// Whether operations may run several at a time. `false`: the executor
    /// runs one at a time. No default, like [`Cloud::children`]: a wrapper
    /// that didn't pass a `false` on would run its cloud concurrently.
    fn concurrent(&self) -> bool;

    /// Whether a move keeps the item's ID, as OneDrive's does. `false` for
    /// a vault, where a move can rebuild the entry under a new ID: the
    /// executor then writes the move ahead (see `crate::moving`). No
    /// default, like [`Cloud::concurrent`]: a wrapper that didn't pass a
    /// `false` on would lose that record.
    fn moves_keep_ids(&self) -> bool;

    /// Downloads coming up in the round, in the planner's order: a hint, so
    /// a cloud that reads each download's link first may read the coming
    /// ones together. By default nothing; a wrapper that doesn't pass it
    /// on only loses that.
    fn upcoming(&self, downloads: &[RemotePre]) {
        let _ = downloads;
    }

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

    /// Whether folder `parent` has an item named `name` in the cloud now
    /// (`None`: this cloud can't tell). Read-only; asked right before a
    /// restore from the recycle bin, which takes the name from whatever
    /// holds it.
    fn name_taken(
        &self,
        parent: &RemoteId,
        name: &str,
    ) -> impl Future<Output = Result<Option<bool>, CloudError>> + Send {
        let _ = (parent, name);
        async { Ok(None) }
    }

    /// Whether item `id` is in folder `parent` under `name` in the cloud now
    /// (`Some(false)` if it's gone; `None`: this cloud can't tell).
    /// Read-only; for a move here that follows one in the cloud, which a
    /// listing behind the cloud can show after it was undone.
    fn is_at(
        &self,
        id: &RemoteId,
        parent: &str,
        name: &str,
    ) -> impl Future<Output = Result<Option<bool>, CloudError>> + Send {
        let _ = (id, parent, name);
        async { Ok(None) }
    }

    /// Some of folder `id`'s children as the cloud has them now (one page
    /// of a listing; empty: it has none). Read-only; for moving a folder
    /// in parts ([`CloudError::TooManyItems`]), which moves what it gets
    /// and asks again. No default: a wrapper that didn't pass it on left
    /// such moves failing in the agent while every test passed.
    fn children(&self, id: &RemoteId) -> impl Future<Output = Result<Vec<RemoteRow>, CloudError>> + Send;

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

/// What an upload reads its content from ([`Cloud::upload`]).
///
/// A file's content is read where it doesn't hold anything else up: the
/// executor's sources ([`Source::fill`]) read on a blocking thread, so a
/// slow disk, or a large fragment, doesn't stall the operations under way,
/// which share one task. Through `Read`, they read in place: a cloud that
/// transforms the content on the way (a vault, which encrypts it) reads
/// that way, and runs one operation at a time ([`Cloud::concurrent`]).
pub trait Source: Read + Send {
    /// Reads into `buf[..want]` until it's full or the source ends, and
    /// returns `buf` with how many bytes were read. By default in place.
    ///
    /// # Panics
    /// If `want` is more than `buf.len()`.
    fn fill(&mut self, mut buf: Vec<u8>, want: usize) -> BoxFuture<'_, (Vec<u8>, std::io::Result<usize>)> {
        let read = read_full(self, &mut buf[..want]);
        Box::pin(std::future::ready((buf, read)))
    }

    /// Whether reading it again gives the same bytes while the file is
    /// unchanged, so an upload cut short may go on from where it ended
    /// (`crate::graph`, resumable sessions). Not for content transformed
    /// on the way (a vault encrypts with fresh randomness each time). Off
    /// by default.
    fn resumable(&self) -> bool {
        false
    }
}

impl Source for &[u8] {}

impl Source for std::fs::File {}

/// Reads until `buf` is full or `src` ends; returns the bytes read.
pub fn read_full<R: Read + ?Sized>(src: &mut R, buf: &mut [u8]) -> std::io::Result<usize> {
    let mut n = 0;
    while n < buf.len() {
        match src.read(&mut buf[n..]) {
            Ok(0) => break,
            Ok(k) => n += k,
            Err(e) if e.kind() == std::io::ErrorKind::Interrupted => {}
            Err(e) => return Err(e),
        }
    }
    Ok(n)
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
