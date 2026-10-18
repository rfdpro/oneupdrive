//! [`VaultCloud`]: the cloud as a vault pair sees it (see the crate docs).
//!
//! Every operation becomes the encrypted items' operations, each with the
//! same preconditions as for an ordinary pair (`If-Match`, names that must
//! be free), and the encrypted items it changes are written into the cloud
//! listing straight away. Content is encrypted while it's uploaded and
//! decrypted while it's downloaded; the hash returned is the encrypted
//! file's, checked against what OneDrive reports (`oneup_exec::cloud`).
//!
//! Folders are made as Cryptomator makes them: the storage folder first,
//! then the entry's folder, then its `dir.c9r`, so an entry never points to
//! nothing.
//!
//! An entry whose stored name changes shape in a move (a long name, kept in
//! a `.c9s` folder with its `name.c9s`, or a folder's `.c9r` folder) is
//! moved as Cryptomator would rebuild it: the new entry is made in the
//! destination, the part that carries the content (`contents.c9r`, the
//! `.c9r` file, or `dir.c9r`) is moved into it under its If-Match, and the
//! old entry, by then holding nothing but its name, is removed. The
//! decrypted item then has a new ID (the new entry's); the executor records
//! it. An interruption leaves either the old entry whole and a new one
//! without content (shown as a problem; the retry uses it), or the new
//! entry whole and the old one without content (removed by the retry or
//! reported).

use std::io::{Read, Write};
use std::sync::{Arc, Mutex, RwLock};

use oneup_exec::{Cloud, CloudError, VersionMark};
use oneup_store::{Kind, RemoteRow, Store};
use oneup_sync::{RemoteId, RemotePre};
use oneup_vault::content::{Decryptor, Encryptor, encrypted_size};
use oneup_vault::names::{self, CONTENTS_FILE, DIR_FILE, NAME_FILE, Stored};

use crate::{Node, NodeKind, Nodes, Vault};

type Hash = [u8; 20];

/// A cloud that keeps a vault (see the module docs).
pub struct VaultCloud<C> {
    inner: C,
    vault: Vault,
    /// The decrypted items, from the last projection plus what this cloud
    /// made since.
    nodes: Arc<RwLock<Nodes>>,
    /// The cloud listing the encrypted items are recorded in.
    listing: Arc<Mutex<Store>>,
}

fn other(e: impl std::fmt::Display) -> CloudError {
    CloudError::Other(e.to_string())
}

/// Counts and hashes what passes through.
struct Hashed<T> {
    inner: T,
    hash: oneup_hash::QuickXorHash,
}

impl<T: Read> Read for Hashed<T> {
    fn read(&mut self, buf: &mut [u8]) -> std::io::Result<usize> {
        let n = self.inner.read(buf)?;
        self.hash.update(&buf[..n]);
        Ok(n)
    }
}

impl<T: Write> Write for Hashed<T> {
    fn write(&mut self, buf: &[u8]) -> std::io::Result<usize> {
        let n = self.inner.write(buf)?;
        self.hash.update(&buf[..n]);
        Ok(n)
    }
    fn flush(&mut self) -> std::io::Result<()> {
        self.inner.flush()
    }
}

fn pre_of(r: &RemoteRow) -> RemotePre {
    RemotePre { id: RemoteId(r.id.clone()), etag: r.etag.clone().unwrap_or_default() }
}

impl<C: Cloud> VaultCloud<C> {
    pub fn new(inner: C, vault: Vault, nodes: Arc<RwLock<Nodes>>, listing: Arc<Mutex<Store>>) -> Self {
        Self { inner, vault, nodes, listing }
    }

    pub fn inner(&self) -> &C {
        &self.inner
    }

    fn node(&self, id: &str) -> Result<Node, CloudError> {
        self.nodes.read().unwrap_or_else(|p| p.into_inner()).get(id).cloned().ok_or(CloudError::Changed)
    }

    fn set_node(&self, id: &str, n: Node) {
        self.nodes.write().unwrap_or_else(|p| p.into_inner()).insert(id.to_string(), n);
    }

    /// Records encrypted items OneDrive now has.
    fn record(&self, rows: &[&RemoteRow]) -> Result<(), CloudError> {
        let mut s = self.listing.lock().unwrap_or_else(|p| p.into_inner());
        s.write(|o| {
            for r in rows {
                o.upsert_item(r, false)?;
            }
            Ok(())
        })
        .map_err(other)
    }

    /// Records the removal of encrypted items (and what was inside them).
    fn record_gone(&self, id: &str) -> Result<(), CloudError> {
        let mut s = self.listing.lock().unwrap_or_else(|p| p.into_inner());
        s.write(|o| o.delete_subtree(id).map(|_| ())).map_err(other)
    }

    fn listed_children(&self, id: &str) -> Result<Vec<RemoteRow>, CloudError> {
        let s = self.listing.lock().unwrap_or_else(|p| p.into_inner());
        Ok(s.read(|o| o.remote_children(id)).map_err(other)?.into_iter().map(|(r, _)| r).collect())
    }

    fn listed(&self, id: &str) -> Result<Option<RemoteRow>, CloudError> {
        let s = self.listing.lock().unwrap_or_else(|p| p.into_inner());
        s.read(|o| o.get_item(id)).map_err(other)
    }

    /// A folder's ID and the cloud folder for its entries, made if missing.
    async fn storage_of(&self, dir: &str) -> Result<(String, String), CloudError> {
        let n = self.node(dir)?;
        let dir_id = n.dir_id.clone().ok_or(CloudError::Changed)?;
        if let Some(s) = n.storage {
            return Ok((dir_id, s));
        }
        let s = self.make_storage(&dir_id).await?;
        self.set_node(dir, Node { storage: Some(s.clone()), ..n });
        Ok((dir_id, s))
    }

    /// Finds or makes `d/XX/YYYY…` for `dir_id`; returns its ID.
    async fn make_storage(&self, dir_id: &str) -> Result<String, CloudError> {
        let path = names::dir_path(&self.vault.key, dir_id).map_err(other)?;
        let mut cur = self.vault.root.clone();
        for part in path.split('/') {
            let found = self
                .listed_children(&cur)?
                .into_iter()
                .find(|r| r.kind == Kind::Folder && r.name.eq_ignore_ascii_case(part));
            cur = match found {
                Some(r) => r.id,
                None => {
                    let r = self.inner.mkdir(&RemoteId(cur.clone()), part).await?;
                    self.record(&[&r])?;
                    r.id
                }
            };
        }
        Ok(cur)
    }

    /// The decrypted row for an entry just made or moved.
    fn row(
        &self,
        node_row: &RemoteRow,
        content: Option<&RemoteRow>,
        parent: &str,
        name: &str,
        plain: i64,
    ) -> RemoteRow {
        match content {
            Some(c) => RemoteRow {
                id: node_row.id.clone(),
                parent_id: Some(parent.to_string()),
                name: name.to_string(),
                kind: Kind::File,
                size: plain,
                etag: c.etag.clone(),
                qxh: c.qxh,
                mtime_ms: c.mtime_ms,
            },
            None => RemoteRow {
                id: node_row.id.clone(),
                parent_id: Some(parent.to_string()),
                name: name.to_string(),
                kind: Kind::Folder,
                size: 0,
                etag: node_row.etag.clone(),
                qxh: None,
                mtime_ms: node_row.mtime_ms,
            },
        }
    }

    /// Uploads `data` (already in its stored form) as a new small file.
    async fn put_small(&self, parent: &str, name: &str, data: &[u8]) -> Result<RemoteRow, CloudError> {
        let (r, _) =
            self.inner.upload(&RemoteId(parent.to_string()), name, None, data.len() as i64, &mut &data[..]).await?;
        self.record(&[&r])?;
        Ok(r)
    }

    /// Encrypts `src` (`size` bytes) into the content file `name` in
    /// `parent`, or as a new version of `replace`. Returns OneDrive's item
    /// and the hash of what was sent, after checking they agree.
    async fn put_content(
        &self,
        parent: &str,
        name: &str,
        replace: Option<&RemotePre>,
        size: i64,
        src: &mut (dyn Read + Send),
    ) -> Result<(RemoteRow, Hash), CloudError> {
        let plain = u64::try_from(size).map_err(other)?;
        let enc_size = i64::try_from(encrypted_size(plain)).map_err(other)?;
        let mut sent = Hashed {
            inner: Encryptor::new(&self.vault.key, src).map_err(other)?,
            hash: oneup_hash::QuickXorHash::new(),
        };
        let (r, _) = self.inner.upload(&RemoteId(parent.to_string()), name, replace, enc_size, &mut sent).await?;
        let content_len = sent.inner.content_len;
        let hash = sent.hash.finalize();
        self.record(&[&r])?;
        if content_len != plain || r.size != enc_size || r.qxh != Some(hash) {
            return Err(CloudError::Other("OneDrive's copy of the encrypted file doesn't match what was sent".into()));
        }
        Ok((r, hash))
    }
}

impl<C: Cloud> Cloud for VaultCloud<C> {
    async fn download(&self, pre: &RemotePre, out: &mut (dyn Write + Send)) -> Result<Option<Hash>, CloudError> {
        let n = self.node(&pre.id.0)?;
        let content = n.content.clone().ok_or_else(|| CloudError::Other("not a file".into()))?;
        let cpre = RemotePre { id: RemoteId(content), etag: pre.etag.clone() };
        let mut got = Hashed { inner: Decryptor::new(&self.vault.key, out), hash: oneup_hash::QuickXorHash::new() };
        self.inner.download(&cpre, &mut got).await?;
        let hash = got.hash.finalize();
        // Checks the last chunk; nothing unauthenticated was passed on.
        got.inner.finish().map_err(|e| CloudError::Io(std::io::Error::other(e.to_string())))?;
        Ok(Some(hash))
    }

    async fn upload(
        &self,
        parent: &RemoteId,
        name: &str,
        replace: Option<&RemotePre>,
        size: i64,
        src: &mut (dyn Read + Send),
    ) -> Result<(RemoteRow, Option<Hash>), CloudError> {
        if let Some(old) = replace {
            // A new version of the file's encrypted contents.
            let n = self.node(&old.id.0)?;
            let content = n.content.clone().ok_or(CloudError::Changed)?;
            let listed = self.listed(&content)?.ok_or(CloudError::Changed)?;
            let parent_id = listed.parent_id.clone().unwrap_or_default();
            let cpre = RemotePre { id: RemoteId(content), etag: old.etag.clone() };
            let (r, hash) = self.put_content(&parent_id, &listed.name, Some(&cpre), size, src).await?;
            let node_row = RemoteRow { id: old.id.0.clone(), ..r.clone() };
            let dparent = n.parent.clone().unwrap_or_default();
            return Ok((self.row(&node_row, Some(&r), &dparent, name, size), Some(hash)));
        }
        let (dir_id, storage) = self.storage_of(&parent.0).await?;
        let enc = names::encrypt_name(&self.vault.key, name, &dir_id).map_err(other)?;
        match Stored::of(enc, self.vault.threshold) {
            Stored::Full(stored) => {
                let (r, hash) = self.put_content(&storage, &stored, None, size, src).await?;
                self.set_node(
                    &r.id,
                    Node {
                        kind: NodeKind::File,
                        parent: Some(parent.0.clone()),
                        long: false,
                        content: Some(r.id.clone()),
                        dir_id: None,
                        storage: None,
                    },
                );
                Ok((self.row(&r, Some(&r), &parent.0, name, size), Some(hash)))
            }
            Stored::Short { dir, name: full } => {
                // One left without content by an upload cut short (the file
                // was saved while read) is used again.
                let holder = self.shell(&storage, &dir, Some(&full)).await?;
                let (c, hash) = self.put_content(&holder.id, CONTENTS_FILE, None, size, src).await?;
                self.set_node(
                    &holder.id,
                    Node {
                        kind: NodeKind::File,
                        parent: Some(parent.0.clone()),
                        long: true,
                        content: Some(c.id.clone()),
                        dir_id: None,
                        storage: None,
                    },
                );
                Ok((self.row(&holder, Some(&c), &parent.0, name, size), Some(hash)))
            }
        }
    }

    /// One operation at a time: a new file's storage folder is found or
    /// made in several requests, which two uploads into one folder could
    /// interleave.
    fn concurrent(&self) -> bool {
        false
    }

    fn can_copy(&self) -> bool {
        false
    }

    async fn copy(&self, _: &RemotePre, _: &Hash, _: &RemoteId, _: &str) -> Result<RemoteRow, CloudError> {
        Err(CloudError::Other("no server-side copies in a vault".into()))
    }

    async fn mkdir(&self, parent: &RemoteId, name: &str) -> Result<RemoteRow, CloudError> {
        let (dir_id, storage) = self.storage_of(&parent.0).await?;
        let enc = names::encrypt_name(&self.vault.key, name, &dir_id).map_err(other)?;
        let stored = Stored::of(enc, self.vault.threshold);
        let new_id = oneup_vault::new_uuid().map_err(other)?;
        // Its storage first: a dir.c9r never points to nothing.
        let new_storage = self.make_storage(&new_id).await?;
        let holder = self.inner.mkdir(&RemoteId(storage), stored.stored_name()).await?;
        self.record(&[&holder])?;
        if let Stored::Short { name: full, .. } = &stored {
            self.put_small(&holder.id, NAME_FILE, full.as_bytes()).await?;
        }
        self.put_small(&holder.id, DIR_FILE, new_id.as_bytes()).await?;
        // The folder's eTag changes as files go into it: the one to plan
        // with comes with the next listing.
        let holder = self.latest(&holder);
        self.set_node(
            &holder.id,
            Node {
                kind: NodeKind::Dir,
                parent: Some(parent.0.clone()),
                long: matches!(stored, Stored::Short { .. }),
                content: None,
                dir_id: Some(new_id),
                storage: Some(new_storage),
            },
        );
        Ok(self.row(&holder, None, &parent.0, name, 0))
    }

    async fn move_item(&self, pre: &RemotePre, to: &RemoteId, name: &str) -> Result<RemoteRow, CloudError> {
        let n = self.node(&pre.id.0)?;
        let (dir_id, storage) = self.storage_of(&to.0).await?;
        let enc = names::encrypt_name(&self.vault.key, name, &dir_id).map_err(other)?;
        let stored = match Stored::of(enc, self.vault.threshold) {
            Stored::Full(s) if !n.long => s,
            reshaped => return self.move_reshaped(pre, &n, to, name, &storage, reshaped).await,
        };
        let r = self.inner.move_item(pre, &RemoteId(storage), &stored).await?;
        self.record(&[&r])?;
        self.set_node(&r.id, Node { parent: Some(to.0.clone()), ..n.clone() });
        Ok(match n.kind {
            NodeKind::File => {
                let plain = oneup_vault::content::content_size(u64::try_from(r.size).unwrap_or(0))
                    .and_then(|s| i64::try_from(s).ok())
                    .unwrap_or(0);
                self.row(&r, Some(&r), &to.0, name, plain)
            }
            _ => self.row(&r, None, &to.0, name, 0),
        })
    }

    async fn delete(&self, pre: &RemotePre) -> Result<(), CloudError> {
        let n = self.node(&pre.id.0)?;
        match n.kind {
            NodeKind::Root => Err(CloudError::Other("the vault itself isn't deleted".into())),
            NodeKind::File if !n.long => {
                self.inner.delete(pre).await?;
                self.record_gone(&pre.id.0)
            }
            NodeKind::File => {
                // The `.c9s` folder with its name and contents; its own eTag
                // from the listing, the contents' version from the plan.
                let holder = self.listed(&pre.id.0)?.ok_or(CloudError::Changed)?;
                let content = n.content.clone().ok_or(CloudError::Changed)?;
                if self.listed(&content)?.and_then(|c| c.etag) != Some(pre.etag.clone()) {
                    return Err(CloudError::Changed);
                }
                self.inner.prune(&pre_of(&holder)).await?;
                self.record_gone(&holder.id)
            }
            NodeKind::Dir => {
                // Only an empty folder (the planner empties it first): its
                // storage may hold nothing but the ID backup.
                if let Some(s) = &n.storage {
                    let inside = self.listed_children(s)?;
                    if inside.iter().any(|r| !r.name.eq_ignore_ascii_case(names::DIR_ID_BACKUP)) {
                        return Err(CloudError::Changed);
                    }
                }
                // The entry first: then the folder is gone at once, and an
                // interruption leaves only an empty storage folder behind.
                self.inner.prune(pre).await?;
                self.record_gone(&pre.id.0)?;
                if let Some(s) = &n.storage
                    && let Some(row) = self.listed(s)?
                {
                    match self.inner.prune(&pre_of(&row)).await {
                        Ok(()) => self.record_gone(s)?,
                        Err(e) => tracing::debug!(error = %e, "an emptied vault folder's storage stayed"),
                    }
                }
                self.nodes.write().unwrap_or_else(|p| p.into_inner()).remove(&pre.id.0);
                Ok(())
            }
        }
    }

    async fn prune(&self, _: &RemotePre) -> Result<(), CloudError> {
        Err(CloudError::Other("rules don't remove items from a vault".into()))
    }

    /// Not offered: an encrypted folder's entries aren't the vault's items,
    /// so a folder too large for OneDrive to move at once can't be moved in
    /// parts here (the move is reported as failed).
    async fn children(&self, _: &RemoteId) -> Result<Vec<RemoteRow>, CloudError> {
        Err(CloudError::Other("a vault folder can't be moved in parts".into()))
    }

    // OneDrive versions the encrypted contents; these address them.

    async fn version_mark(&self, pre: &RemotePre, size: i64) -> Result<Option<VersionMark>, CloudError> {
        let content = self.node(&pre.id.0)?.content.ok_or(CloudError::Changed)?;
        let enc = i64::try_from(encrypted_size(u64::try_from(size).map_err(other)?)).map_err(other)?;
        self.inner.version_mark(&RemotePre { id: RemoteId(content), etag: pre.etag.clone() }, enc).await
    }

    async fn replaced_other(&self, id: &RemoteId, mark: &VersionMark) -> Result<Option<VersionMark>, CloudError> {
        let content = self.node(&id.0)?.content.ok_or(CloudError::Changed)?;
        // Its size is the encrypted file's; decrypting checks the content.
        Ok(self.inner.replaced_other(&RemoteId(content), mark).await?.map(|v| VersionMark { size: None, ..v }))
    }

    async fn download_version(
        &self,
        id: &RemoteId,
        version: &str,
        out: &mut (dyn Write + Send),
    ) -> Result<u64, CloudError> {
        let content = self.node(&id.0)?.content.ok_or(CloudError::Changed)?;
        let mut got = Decryptor::new(&self.vault.key, out);
        self.inner.download_version(&RemoteId(content), version, &mut got).await?;
        // Checks the last chunk; nothing unauthenticated was passed on.
        let (n, _) = got.finish().map_err(|e| CloudError::Io(std::io::Error::other(e.to_string())))?;
        Ok(n)
    }
}

impl<C: Cloud> VaultCloud<C> {
    /// A move whose entry changes shape (see the module docs): a long name
    /// on either side, or a folder's `.c9r` folder.
    async fn move_reshaped(
        &self,
        pre: &RemotePre,
        n: &Node,
        to: &RemoteId,
        name: &str,
        storage: &str,
        stored: Stored,
    ) -> Result<RemoteRow, CloudError> {
        // What carries the entry: a file's encrypted contents, a folder's ID.
        let (payload, payload_pre) = match n.kind {
            NodeKind::File => {
                let c = n.content.clone().ok_or(CloudError::Changed)?;
                let row = self.listed(&c)?.ok_or(CloudError::Changed)?;
                (row, RemotePre { id: RemoteId(c), etag: pre.etag.clone() })
            }
            NodeKind::Dir => {
                let dir = self
                    .listed_children(&pre.id.0)?
                    .into_iter()
                    .find(|r| r.kind == Kind::File && r.name.eq_ignore_ascii_case(DIR_FILE))
                    .ok_or(CloudError::Changed)?;
                let p = pre_of(&dir);
                (dir, p)
            }
            NodeKind::Root => return Err(CloudError::Other("the vault itself doesn't move".into())),
        };
        // The old entry's own folder, if it has one (it goes last).
        let old_shell = (n.long || n.kind == NodeKind::Dir).then(|| pre.id.0.clone());
        let (entry, moved) = match (&stored, n.kind) {
            (Stored::Full(s), NodeKind::File) => {
                let r = self.inner.move_item(&payload_pre, &RemoteId(storage.to_string()), s).await?;
                self.record(&[&r])?;
                (r.clone(), r)
            }
            (Stored::Full(s), _) => {
                let shell = self.shell(storage, s, None).await?;
                let r = self.inner.move_item(&payload_pre, &RemoteId(shell.id.clone()), DIR_FILE).await?;
                self.record(&[&r])?;
                (shell, r)
            }
            (Stored::Short { dir, name: full }, kind) => {
                let shell = self.shell(storage, dir, Some(full)).await?;
                let inside = if kind == NodeKind::File { CONTENTS_FILE } else { DIR_FILE };
                let r = self.inner.move_item(&payload_pre, &RemoteId(shell.id.clone()), inside).await?;
                self.record(&[&r])?;
                (shell, r)
            }
        };
        drop(payload);
        if let Some(old) = old_shell {
            // Holds nothing but its name now; its eTag moved with that, so
            // it's read again first.
            match self.inner.item(&RemoteId(old.clone())).await {
                Ok(Some(row)) => match self.inner.prune(&pre_of(&row)).await {
                    Ok(()) => self.record_gone(&old)?,
                    Err(e) => tracing::debug!(error = %e, "a moved vault entry's old folder stayed"),
                },
                Ok(None) => self.record_gone(&old)?,
                Err(e) => tracing::debug!(error = %e, "a moved vault entry's old folder stayed"),
            }
        }
        let entry = self.latest(&entry);
        let node = Node {
            parent: Some(to.0.clone()),
            long: matches!(stored, Stored::Short { .. }),
            content: (n.kind == NodeKind::File).then(|| moved.id.clone()),
            ..n.clone()
        };
        {
            let mut nodes = self.nodes.write().unwrap_or_else(|p| p.into_inner());
            nodes.remove(&pre.id.0);
            nodes.insert(entry.id.clone(), node);
        }
        Ok(match n.kind {
            NodeKind::File => {
                let plain = oneup_vault::content::content_size(u64::try_from(moved.size).unwrap_or(0))
                    .and_then(|s| i64::try_from(s).ok())
                    .unwrap_or(0);
                self.row(&entry, Some(&moved), &to.0, name, plain)
            }
            _ => self.row(&entry, None, &to.0, name, 0),
        })
    }

    /// The new entry's folder `stored` in `storage` (with its `name.c9s`
    /// for a long name). One left without content by an interrupted move is
    /// used again.
    async fn shell(&self, storage: &str, stored: &str, long_name: Option<&str>) -> Result<RemoteRow, CloudError> {
        let existing = self.listed_children(storage)?.into_iter().find(|r| r.name.eq_ignore_ascii_case(stored));
        if let Some(r) = existing {
            let inside = self.listed_children(&r.id)?;
            let empty = r.kind == Kind::Folder
                && inside.iter().all(|c| c.name.eq_ignore_ascii_case(NAME_FILE) && long_name.is_some());
            if !empty {
                return Err(CloudError::NameTaken);
            }
            if long_name.is_some() && inside.is_empty() {
                self.put_small(&r.id, NAME_FILE, long_name.unwrap_or_default().as_bytes()).await?;
            }
            return Ok(r);
        }
        let shell = self.inner.mkdir(&RemoteId(storage.to_string()), stored).await?;
        self.record(&[&shell])?;
        if let Some(full) = long_name {
            self.put_small(&shell.id, NAME_FILE, full.as_bytes()).await?;
        }
        Ok(shell)
    }

    /// The item as listed after this cloud's own changes to it.
    fn latest(&self, r: &RemoteRow) -> RemoteRow {
        self.listed(&r.id).ok().flatten().unwrap_or_else(|| r.clone())
    }
}
