//! [`Cloud`] over Microsoft Graph.
//!
//! Status codes map onto the executor's outcomes: 412 (eTag moved on) and
//! 404 (gone) are [`CloudError::Changed`], 409 (name taken) is
//! [`CloudError::NameTaken`]; the executor skips and re-plans either way.

use std::io::{Read, Write};
use std::sync::Arc;

use oneup_graph::client::SIMPLE_UPLOAD_MAX;
use oneup_graph::fragment::{MAX_UNITS, Sizer, UNIT};
use oneup_graph::{DriveItem, Graph};
use oneup_store::RemoteRow;
use oneup_sync::{RemoteId, RemotePre};

use crate::cloud::{Cloud, CloudError, Meta, MetaDone};

/// The executor's view of a signed-in drive.
#[derive(Debug, Clone)]
pub struct GraphCloud {
    graph: Arc<Graph>,
    /// Upload fragment sizes, learned from the connection and kept from one
    /// file to the next (`oneup_graph::fragment`).
    sizer: Arc<std::sync::Mutex<Sizer>>,
}

/// How much memory an upload may hold at once (its largest fragment), by
/// memory profile.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct UploadMemory {
    pub fragment: usize,
}

impl UploadMemory {
    /// `minimal`: 10 MiB fragments; `balanced`:
    /// up to 40 MiB; `performance`: up to the largest Graph allows
    /// (59.69 MiB). Measured on a fast line, 200 MiB files: 10 MiB 23–25 s,
    /// 40 MiB 11 s, 59.69 MiB 11–13 s.
    pub fn for_profile(name: &str) -> Self {
        match name {
            "minimal" => Self { fragment: 32 * UNIT },
            "performance" => Self { fragment: MAX_UNITS * UNIT },
            _ => Self { fragment: 128 * UNIT },
        }
    }
}

impl GraphCloud {
    pub fn new(graph: Arc<Graph>) -> Self {
        Self::with_memory(graph, UploadMemory::for_profile("balanced"))
    }

    pub fn with_memory(graph: Arc<Graph>, m: UploadMemory) -> Self {
        Self { graph, sizer: Arc::new(std::sync::Mutex::new(Sizer::new(m.fragment))) }
    }

    /// The fragment size the next large upload would start with (learned).
    pub fn fragment_size(&self) -> usize {
        self.sizer().next(u64::MAX)
    }

    fn sizer(&self) -> std::sync::MutexGuard<'_, Sizer> {
        self.sizer.lock().unwrap_or_else(|p| p.into_inner())
    }

    /// The item as the cloud has it now, as a Remote tree row. Graph
    /// computes a new file's QuickXorHash asynchronously, so a response
    /// without one is re-read a few times before giving up.
    async fn row_of(&self, item: DriveItem) -> Result<RemoteRow, CloudError> {
        let mut item = item;
        let mut tries = 0;
        while item.file.is_some() && item.quick_xor_hash().is_none() && tries < 5 {
            tries += 1;
            tokio::time::sleep(std::time::Duration::from_millis(500 * tries)).await;
            item = self.graph.item(&item.id).await.map_err(map)?.ok_or(CloudError::Changed)?;
        }
        oneup_remote::ingest::to_row(&item).ok_or_else(|| CloudError::Other("item without a name".into()))
    }
}

/// A refusal as the executor's outcome. OneDrive's error code decides
/// where there is one: a stale eTag is `resourceModified`, sometimes sent
/// as 409 rather than 412 (measured in a batch), and must not
/// read as a taken name.
fn classify(status: u16, code: Option<String>) -> Option<CloudError> {
    match (code.as_deref(), status) {
        (Some("nameAlreadyExists"), _) => Some(CloudError::NameTaken),
        (Some("resourceModified" | "itemNotFound"), _) => Some(CloudError::Changed),
        (_, 404 | 412) => Some(CloudError::Changed),
        (_, 409) => Some(CloudError::NameTaken),
        _ => None,
    }
}

/// A batched answer's status as the executor's outcome.
fn map_status(a: &oneup_graph::client::BatchResp, what: String) -> CloudError {
    if a.is_too_many_items() {
        return CloudError::TooManyItems;
    }
    classify(a.status, a.code()).unwrap_or_else(|| CloudError::Other(format!("{what}: HTTP {}", a.status)))
}

fn map(e: oneup_graph::Error) -> CloudError {
    if e.is_too_many_items() {
        return CloudError::TooManyItems;
    }
    let classified = e.status().and_then(|s| classify(s, e.code()));
    classified.unwrap_or_else(|| CloudError::Other(e.to_string()))
}

/// Reads until `buf` is full or the source ends; returns the bytes read.
fn fill(src: &mut (dyn Read + Send), buf: &mut [u8]) -> Result<usize, CloudError> {
    let mut n = 0;
    while n < buf.len() {
        match src.read(&mut buf[n..]) {
            Ok(0) => break,
            Ok(k) => n += k,
            Err(e) if e.kind() == std::io::ErrorKind::Interrupted => {}
            Err(e) => return Err(CloudError::Io(e)),
        }
    }
    Ok(n)
}

impl Cloud for GraphCloud {
    async fn download(&self, pre: &RemotePre, out: &mut (dyn Write + Send)) -> Result<Option<[u8; 20]>, CloudError> {
        let info = self.graph.download_info(&pre.id.0).await.map_err(map)?.ok_or(CloudError::Changed)?;
        if info.e_tag.as_deref() != Some(pre.etag.as_str()) {
            return Err(CloudError::Changed);
        }
        if info.folder.is_some() {
            return Err(CloudError::Other("not a file".into()));
        }
        let url = info.download_url.ok_or_else(|| CloudError::Other("no download URL".into()))?;
        // The URL serves the current content. If the file changed after the
        // eTag check, the executor's hash check against the planned version
        // catches it before anything local changes.
        self.graph.download_to(&url, out).await.map_err(|e| match e {
            oneup_graph::Error::Local(io) => CloudError::Io(io),
            e => map(e),
        })?;
        Ok(None)
    }

    async fn upload(
        &self,
        parent: &RemoteId,
        name: &str,
        replace: Option<&RemotePre>,
        size: i64,
        src: &mut (dyn Read + Send),
    ) -> Result<(RemoteRow, Option<[u8; 20]>), CloudError> {
        let total = u64::try_from(size).map_err(|_| CloudError::Other("negative size".into()))?;
        let replace = replace.map(|p| (p.id.0.as_str(), p.etag.as_str()));
        let short = || CloudError::Other("the file ended before its recorded size".into());
        let item = if total as usize <= SIMPLE_UPLOAD_MAX {
            let mut buf = vec![0u8; total as usize];
            if fill(src, &mut buf)? != buf.len() {
                return Err(short());
            }
            self.graph.upload_small(&parent.0, name, replace, &buf).await.map_err(map)?
        } else {
            let url = self.graph.upload_session(&parent.0, name, replace).await.map_err(map)?;
            let mut buf = Vec::new();
            let mut start = 0u64;
            loop {
                let want = self.sizer().next(total - start);
                // Grows to the largest fragment chosen, which the memory
                // profile bounds.
                if buf.len() < want {
                    buf.resize(want, 0);
                }
                let n = match fill(src, &mut buf[..want]) {
                    Ok(n) if n == want => n,
                    Ok(_) => {
                        self.graph.cancel_upload(&url).await;
                        return Err(short());
                    }
                    Err(e) => {
                        self.graph.cancel_upload(&url).await;
                        return Err(e);
                    }
                };
                let sent = std::time::Instant::now();
                let got = self.graph.upload_fragment(&url, start, total, &buf[..n]).await;
                let took = sent.elapsed();
                match got {
                    Ok(f) => {
                        let secs = took.saturating_sub(f.paused + f.capped).as_secs_f64();
                        tracing::debug!(bytes = n, secs, retried = f.retried, "upload fragment sent");
                        self.sizer().observe(n, secs, f.retried);
                        match f.item {
                            Some(item) => break item,
                            None if start + (n as u64) < total => start += n as u64,
                            None => {
                                self.graph.cancel_upload(&url).await;
                                return Err(CloudError::Other("the upload session did not complete".into()));
                            }
                        }
                    }
                    Err(e) => {
                        self.sizer().failed();
                        self.graph.cancel_upload(&url).await;
                        return Err(map(e));
                    }
                }
            }
        };
        Ok((self.row_of(item).await?, None))
    }

    async fn copy(
        &self,
        source: &RemotePre,
        hash: &[u8; 20],
        parent: &RemoteId,
        name: &str,
    ) -> Result<RemoteRow, CloudError> {
        // The listing may be older than the source: check it is still the
        // version with this content right before copying.
        let now = self.graph.item(&source.id.0).await.map_err(map)?.ok_or(CloudError::Changed)?;
        if now.e_tag.as_deref() != Some(source.etag.as_str()) || now.quick_xor_hash() != Some(*hash) {
            return Err(CloudError::Changed);
        }
        let monitor = self.graph.copy_item(&source.id.0, &parent.0, name).await.map_err(map)?;
        // Copies of content OneDrive has are usually quick; give up waiting
        // after about two minutes (the caller then uploads instead, which
        // fails harmlessly as "name taken" if the copy lands after all).
        let mut wait = std::time::Duration::from_millis(250);
        let deadline = std::time::Instant::now() + std::time::Duration::from_secs(120);
        loop {
            tokio::time::sleep(wait).await;
            match self.graph.copy_status(&monitor).await.map_err(map)? {
                oneup_graph::client::CopyStatus::Done(id) => {
                    let item = self.graph.item(&id).await.map_err(map)?.ok_or(CloudError::Changed)?;
                    return self.row_of(item).await;
                }
                oneup_graph::client::CopyStatus::Failed(why) => {
                    return Err(CloudError::Other(format!("OneDrive couldn't copy it: {why}")));
                }
                oneup_graph::client::CopyStatus::Running if std::time::Instant::now() < deadline => {
                    wait = (wait * 2).min(std::time::Duration::from_secs(4));
                }
                oneup_graph::client::CopyStatus::Running => {
                    return Err(CloudError::Other("OneDrive's copy is taking too long".into()));
                }
            }
        }
    }

    async fn mkdir(&self, parent: &RemoteId, name: &str) -> Result<RemoteRow, CloudError> {
        let item = self.graph.create_folder(&parent.0, name).await.map_err(map)?;
        self.row_of(item).await
    }

    async fn move_item(&self, pre: &RemotePre, to: &RemoteId, name: &str) -> Result<RemoteRow, CloudError> {
        let item = self.graph.move_item(&pre.id.0, &pre.etag, &to.0, name).await.map_err(map)?;
        self.row_of(item).await
    }

    async fn delete(&self, pre: &RemotePre) -> Result<(), CloudError> {
        // Graph deletes a folder together with its contents. The planner
        // only deletes folders it has emptied, so contents here mean someone
        // added something meanwhile: refuse, and the next round sees it.
        let item = self.graph.item(&pre.id.0).await.map_err(map)?.ok_or(CloudError::Changed)?;
        if item.e_tag.as_deref() != Some(pre.etag.as_str()) {
            return Err(CloudError::Changed);
        }
        if item.folder.as_ref().is_some_and(|f| f.child_count != Some(0)) {
            return Err(CloudError::NotEmpty);
        }
        self.graph.delete_item(&pre.id.0, &pre.etag).await.map_err(map)
    }

    async fn batch(&self, ops: &[Meta]) -> Vec<Result<MetaDone, CloudError>> {
        use oneup_graph::client::{BATCH_MAX, BatchReq};
        let mut out: Vec<Option<Result<MetaDone, CloudError>>> =
            std::iter::repeat_with(|| None).take(ops.len()).collect();
        for (start, chunk) in ops.chunks(BATCH_MAX).enumerate().map(|(k, c)| (k * BATCH_MAX, c)) {
            // Deletions first re-read their items (in one batch too): Graph
            // deletes a folder with its contents, so only empty folders and
            // unchanged items go on.
            let deletes: Vec<usize> =
                chunk.iter().enumerate().filter(|(_, m)| matches!(m, Meta::Delete { .. })).map(|(i, _)| i).collect();
            let mut refused: Vec<Option<CloudError>> = std::iter::repeat_with(|| None).take(chunk.len()).collect();
            if !deletes.is_empty() {
                let reads: Vec<BatchReq> = deletes
                    .iter()
                    .map(|&i| match &chunk[i] {
                        Meta::Delete { pre } => self.graph.batch_get(&pre.id.0),
                        _ => unreachable!(),
                    })
                    .collect();
                match self.graph.batch(&reads).await {
                    Ok(answers) => {
                        for (&i, a) in deletes.iter().zip(answers) {
                            let Meta::Delete { pre } = &chunk[i] else { continue };
                            refused[i] = match a.item() {
                                Ok(item) if item.e_tag.as_deref() != Some(pre.etag.as_str()) => {
                                    Some(CloudError::Changed)
                                }
                                Ok(item) if item.folder.as_ref().is_some_and(|f| f.child_count != Some(0)) => {
                                    Some(CloudError::NotEmpty)
                                }
                                Ok(_) => None,
                                Err(_) => Some(map_status(&a, "reading an item to delete".into())),
                            };
                        }
                    }
                    Err(e) => {
                        let msg = e.to_string();
                        for &i in &deletes {
                            refused[i] = Some(CloudError::Other(msg.clone()));
                        }
                    }
                }
            }
            let mut sent = Vec::new();
            let mut reqs = Vec::new();
            for (i, m) in chunk.iter().enumerate() {
                if let Some(e) = refused[i].take() {
                    out[start + i] = Some(Err(e));
                    continue;
                }
                reqs.push(match m {
                    Meta::Mkdir { parent, name } => self.graph.batch_create_folder(&parent.0, name),
                    Meta::Move { pre, to, name } => self.graph.batch_move(&pre.id.0, &pre.etag, &to.0, name),
                    Meta::Delete { pre } => self.graph.batch_delete(&pre.id.0, &pre.etag),
                });
                sent.push(i);
            }
            if reqs.is_empty() {
                continue;
            }
            match self.graph.batch(&reqs).await {
                Ok(answers) => {
                    for (&i, a) in sent.iter().zip(answers) {
                        let r = match (&chunk[i], (200..300).contains(&a.status)) {
                            (Meta::Delete { .. }, true) => Ok(MetaDone::Deleted),
                            (_, true) => match a.item() {
                                Ok(item) => self.row_of(item).await.map(MetaDone::Item),
                                Err(e) => Err(CloudError::Other(e.to_string())),
                            },
                            (m, false) => Err({
                                tracing::debug!(status = a.status, "a batched change was refused");
                                map_status(
                                    &a,
                                    match m {
                                        Meta::Mkdir { .. } => "creating a folder",
                                        Meta::Move { .. } => "moving an item",
                                        Meta::Delete { .. } => "deleting an item",
                                    }
                                    .into(),
                                )
                            }),
                        };
                        out[start + i] = Some(r);
                    }
                }
                Err(e) => {
                    for &i in &sent {
                        out[start + i] = Some(Err(CloudError::Other(e.to_string())));
                    }
                }
            }
        }
        out.into_iter().map(|r| r.unwrap_or_else(|| Err(CloudError::Other("not sent".into())))).collect()
    }

    async fn item(&self, id: &RemoteId) -> Result<Option<RemoteRow>, CloudError> {
        match self.graph.item(&id.0).await.map_err(map)? {
            Some(it) => Ok(oneup_remote::ingest::to_row(&it)),
            None => Ok(None),
        }
    }

    async fn children(&self, id: &RemoteId) -> Result<Vec<RemoteRow>, CloudError> {
        let page = self.graph.page(&self.graph.children_url(&id.0)).await.map_err(map)?;
        Ok(page.value.iter().filter(|it| !it.is_deleted()).filter_map(oneup_remote::ingest::to_row).collect())
    }

    async fn prune(&self, pre: &RemotePre) -> Result<(), CloudError> {
        let item = self.graph.item(&pre.id.0).await.map_err(map)?.ok_or(CloudError::Changed)?;
        if item.e_tag.as_deref() != Some(pre.etag.as_str()) {
            return Err(CloudError::Changed);
        }
        self.graph.delete_item(&pre.id.0, &pre.etag).await.map_err(map)
    }
}
