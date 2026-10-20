//! [`Cloud`] over Microsoft Graph.
//!
//! Status codes map onto the executor's outcomes: 412 (eTag moved on) and
//! 404 (gone) are [`CloudError::Changed`], 409 (name taken) is
//! [`CloudError::NameTaken`]; the executor skips and re-plans either way.
//! 507 (no room left) is [`CloudError::Full`].

use std::collections::{HashMap, HashSet, VecDeque};
use std::io::Write;
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use oneup_graph::client::{BATCH_MAX, Fragment, SIMPLE_UPLOAD_MAX};
use oneup_graph::fragment::{MAX_UNITS, Sizer, UNIT};
use oneup_graph::model::DownloadInfo;
use oneup_graph::{DriveItem, FileVersion, Graph};
use oneup_store::RemoteRow;
use oneup_sync::{RemoteId, RemotePre};

use crate::cloud::{Cloud, CloudError, Meta, MetaDone, Source, VersionMark};
use crate::timing::Timings;

/// The executor's view of a signed-in drive.
#[derive(Debug, Clone)]
pub struct GraphCloud {
    graph: Arc<Graph>,
    /// Upload fragment sizes, learned from the connection and kept from one
    /// file to the next (`oneup_graph::fragment`).
    sizer: Arc<std::sync::Mutex<Sizer>>,
    /// How long each kind of request took ([`GraphCloud::timed`]).
    timings: Arc<Timings>,
    /// Download links read ahead ([`Links`]), and a signal for when a
    /// batch of them was read.
    links: Arc<Mutex<Links>>,
    links_read: Arc<tokio::sync::Notify>,
}

/// How long a link read ahead is used: they're pre-authenticated and
/// short-lived, and one used within a minute of reading it works.
const LINK_FRESH: Duration = Duration::from_secs(60);

/// How long a coming download waits to have its link read ahead: one the
/// round didn't get to (skipped, refused, the round stopped) is dropped.
const AHEAD_STALE: Duration = Duration::from_secs(300);

/// How many coming downloads and unused links are kept.
const LINKS_MAX: usize = 1024;

/// How long a finished download waits for the links it started reading.
const REFILL_WAIT: Duration = Duration::from_secs(5);

/// How many links are kept in hand ahead of the downloads that use them.
const LINKS_AHEAD: usize = 3 * BATCH_MAX;

/// Download links read ahead, [`BATCH_MAX`] at a time: a download first
/// reads its item for the eTag and the link, and on its own that request
/// took about four times the content of a small file (measured: 520 ms
/// against 120 ms). The executor says which downloads are coming
/// ([`Cloud::upcoming`]); a download whose link isn't here reads it with
/// the next ones', and one whose link is being read waits for that, so
/// downloads starting together send one batch. Bounded, self-cleaning, and
/// each link is taken out when used.
#[derive(Default)]
struct Links {
    /// Coming downloads whose links aren't read yet, oldest first, with
    /// when they were announced; and the same as a set.
    ahead: VecDeque<(String, Instant)>,
    queued: HashSet<String>,
    /// Links read and not used yet, and when they were read.
    got: HashMap<String, (DownloadInfo, Instant)>,
    /// Links being read now, and in how many batches.
    reading: HashSet<String>,
    batches: usize,
}

impl std::fmt::Debug for Links {
    // The links are credentials: never printed.
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Links").field("ahead", &self.ahead.len()).field("got", &self.got.len()).finish()
    }
}

/// What [`Links::claim`] found.
#[derive(Debug)]
enum Took {
    Link(DownloadInfo),
    /// Being read with others: wait for it.
    Reading,
    /// Not read: these are reserved for reading, the item first.
    Batch(Vec<String>),
}

impl Links {
    /// Drops links gone stale and coming downloads waiting too long.
    fn purge(&mut self, now: Instant) {
        self.got.retain(|_, (_, at)| now.saturating_duration_since(*at) < LINK_FRESH);
        while let Some((id, at)) = self.ahead.front()
            && now.saturating_duration_since(*at) >= AHEAD_STALE
        {
            self.queued.remove(id);
            self.ahead.pop_front();
        }
    }

    fn announce(&mut self, ids: impl IntoIterator<Item = String>, now: Instant) {
        self.purge(now);
        for id in ids {
            if !self.got.contains_key(&id) && !self.reading.contains(&id) && self.queued.insert(id.clone()) {
                self.ahead.push_back((id, now));
            }
        }
        while self.ahead.len() > LINKS_MAX {
            if let Some((id, _)) = self.ahead.pop_front() {
                self.queued.remove(&id);
            }
        }
    }

    /// `id`'s link if one read ahead is still fresh, taken out; else
    /// whether it's being read; else a batch reserved to read it with the
    /// coming ones, as many as the room left under [`LINKS_AHEAD`] (just
    /// it, when two batches are being read or there's no room). One step,
    /// so two callers never both read it.
    fn claim(&mut self, id: &str, now: Instant) -> Took {
        self.purge(now);
        if let Some((info, _)) = self.got.remove(id) {
            return Took::Link(info);
        }
        if self.reading.contains(id) {
            return Took::Reading;
        }
        let room = if self.batches >= 2 { 0 } else { self.room() };
        Took::Batch(self.batch_for(Some(id), room.clamp(1, BATCH_MAX)))
    }

    /// How many more links may be in hand or being read.
    fn room(&self) -> usize {
        LINKS_AHEAD.saturating_sub(self.got.len() + self.reading.len())
    }

    /// The coming downloads to read ahead now, reserved (empty: none):
    /// while more are coming, at most [`LINKS_AHEAD`] links are in hand or
    /// being read, in at most two batches at once. A batch took 600-750 ms,
    /// about as long as small downloads take to use 40 links, so downloads
    /// then rarely wait for one.
    fn refill(&mut self, now: Instant) -> Vec<String> {
        self.purge(now);
        let room = self.room();
        if self.ahead.is_empty() || self.batches >= 2 || room == 0 {
            return Vec::new();
        }
        self.batch_for(None, room.min(BATCH_MAX))
    }

    /// The items to read with `id`'s (none: only coming ones): it first,
    /// then the coming ones not read yet, `max` in all. They count as being
    /// read, in one more batch, until [`Links::finish`].
    fn batch_for(&mut self, id: Option<&str>, max: usize) -> Vec<String> {
        let mut ids: Vec<String> = id.into_iter().map(str::to_string).collect();
        while ids.len() < max
            && let Some((next, _)) = self.ahead.pop_front()
        {
            self.queued.remove(&next);
            if !self.got.contains_key(&next) && !self.reading.contains(&next) && !ids.contains(&next) {
                ids.push(next);
            }
        }
        if let Some(id) = id {
            self.queued.remove(id);
        }
        if !ids.is_empty() {
            self.reading.extend(ids.iter().cloned());
            self.batches += 1;
        }
        ids
    }

    fn finish(&mut self, ids: &[String]) {
        if ids.is_empty() {
            return;
        }
        for id in ids {
            self.reading.remove(id);
        }
        self.batches = self.batches.saturating_sub(1);
    }

    fn keep(&mut self, id: String, info: DownloadInfo, now: Instant) {
        if self.got.len() < LINKS_MAX {
            self.got.insert(id, (info, now));
        }
    }
}

/// Ends a batch of [`Links::claim`] or [`Links::refill`] however it ends, the round
/// stopping too: its links no longer count as being read, and the
/// downloads waiting for them look again.
struct Reading<'a> {
    cloud: &'a GraphCloud,
    ids: Vec<String>,
}

impl Drop for Reading<'_> {
    fn drop(&mut self) {
        self.cloud.links().finish(&self.ids);
        self.cloud.links_read.notify_waiters();
    }
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
        Self {
            graph,
            sizer: Arc::new(std::sync::Mutex::new(Sizer::new(m.fragment))),
            timings: Arc::default(),
            links: Arc::default(),
            links_read: Arc::default(),
        }
    }

    /// Records how long its requests take in `t` (shared with the
    /// executor, [`crate::Executor::timed`]).
    pub fn timed(self, t: Arc<Timings>) -> Self {
        Self { timings: t, ..self }
    }

    /// The fragment size the next large upload would start with (learned).
    pub fn fragment_size(&self) -> usize {
        self.sizer().next(u64::MAX)
    }

    fn links(&self) -> std::sync::MutexGuard<'_, Links> {
        self.links.lock().unwrap_or_else(|p| p.into_inner())
    }

    /// Item `id`'s eTag, kind and download link: read ahead, or read with
    /// the coming downloads' ([`Links`]); `None` if it's gone. Whether it
    /// was read ahead.
    async fn link(&self, id: &str) -> Result<(Option<DownloadInfo>, bool), CloudError> {
        let ids = loop {
            let read = self.links_read.notified();
            tokio::pin!(read);
            read.as_mut().enable();
            let found = self.links().claim(id, Instant::now());
            match found {
                Took::Link(info) => return Ok((Some(info), true)),
                Took::Reading => read.await,
                Took::Batch(ids) => break ids,
            }
        };
        let batch = Reading { cloud: self, ids };
        if batch.ids.len() > 1
            && let Some(mine) = self.read_links(batch, Some(id)).await
        {
            return Ok((mine, false));
        }
        let started = Instant::now();
        let info = self.graph.download_info(id).await.map_err(map)?;
        self.timings.record("download: item and link", started);
        Ok((info, false))
    }

    /// Reads `batch`'s links in one request and keeps them, apart from
    /// `mine`'s, which it returns (`None`: not read, read it on its own).
    async fn read_links(&self, batch: Reading<'_>, mine: Option<&str>) -> Option<Option<DownloadInfo>> {
        let started = Instant::now();
        let refs: Vec<&str> = batch.ids.iter().map(String::as_str).collect();
        let answers = match self.graph.download_infos(&refs).await {
            Ok(a) => a,
            Err(e) => {
                tracing::debug!(error = %e, "download links couldn't be read together");
                return None;
            }
        };
        self.timings.record("download: links, read together", started);
        let now = Instant::now();
        let mut found = None;
        let mut links = self.links();
        for (other, answer) in batch.ids.iter().zip(answers) {
            let is_mine = mine == Some(other.as_str());
            match answer {
                // Only an answer about the item asked for, with a link.
                Ok(Some(info))
                    if info.id.eq_ignore_ascii_case(other)
                        && (info.download_url.is_some() || info.folder.is_some()) =>
                {
                    if is_mine {
                        found = Some(Some(info));
                    } else {
                        links.keep(other.clone(), info, now);
                    }
                }
                Ok(None) if is_mine => found = Some(None),
                // Read on its own when its turn comes.
                _ => {}
            }
        }
        drop(links);
        drop(batch);
        found
    }

    /// Reads the next coming downloads' links, if [`Links::refill`] says so.
    async fn refill(&self) {
        let ids = self.links().refill(Instant::now());
        if !ids.is_empty() {
            self.read_links(Reading { cloud: self, ids }, None).await;
        }
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
            let started = Instant::now();
            tries += 1;
            tokio::time::sleep(std::time::Duration::from_millis(500 * tries)).await;
            item = self.graph.item(&item.id).await.map_err(map)?.ok_or(CloudError::Changed)?;
            self.timings.record("upload: hash not ready, read again", started);
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
        (Some("quotaLimitReached" | "insufficientStorage"), _) | (_, 507) => Some(CloudError::Full),
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

fn short() -> CloudError {
    CloudError::Other("the file ended before its recorded size".into())
}

/// What a large upload needs from the service: Graph's upload session, or
/// a fake in tests, so the protocol is tested without a network.
trait Session: Sync {
    /// Sends `bytes` as bytes `start..` of a `total`-byte upload
    /// ([`Graph::upload_fragment`]).
    fn put(&self, start: u64, total: u64, bytes: &[u8]) -> impl Future<Output = oneup_graph::Result<Fragment>> + Send;
    /// The first byte the session still expects; `None`: it takes no more
    /// ([`Graph::upload_status`]).
    fn status(&self) -> impl Future<Output = oneup_graph::Result<Option<u64>>> + Send;
    fn cancel(&self) -> impl Future<Output = ()> + Send;
    /// Waits before a fragment is sent again, longer the more times it
    /// was sent already (`sends`).
    fn pause(&self, sends: u32) -> impl Future<Output = ()> + Send;
    /// Item `id`'s eTag now; `None`: it's gone.
    fn etag(&self, id: &str) -> impl Future<Output = oneup_graph::Result<Option<String>>> + Send;
}

struct GraphSession<'a> {
    graph: &'a Graph,
    /// Pre-authenticated; never log it.
    url: String,
}

impl Session for GraphSession<'_> {
    async fn put(&self, start: u64, total: u64, bytes: &[u8]) -> oneup_graph::Result<Fragment> {
        self.graph.upload_fragment(&self.url, start, total, bytes).await
    }
    async fn status(&self) -> oneup_graph::Result<Option<u64>> {
        self.graph.upload_status(&self.url).await
    }
    async fn cancel(&self) {
        self.graph.cancel_upload(&self.url).await;
    }
    async fn pause(&self, sends: u32) {
        tokio::time::sleep(std::time::Duration::from_millis(500 << sends.min(6))).await;
    }
    async fn etag(&self, id: &str) -> oneup_graph::Result<Option<String>> {
        Ok(self.graph.item(id).await?.and_then(|it| it.e_tag))
    }
}

/// Most times one fragment is sent, the first included. The client sends a
/// fragment once ([`Graph::upload_fragment`]); its retries are these, each
/// after asking the session where it is. Waiting out throttling
/// (`Retry-After`) isn't a send of its own: the client does that.
const SENDS: u32 = 5;

/// A failed fragment the session may have received anyway: the answer was
/// lost (the connection or the server failed) or a send found the bytes
/// already there (416).
fn maybe_received(e: &oneup_graph::Error) -> bool {
    e.is_transient() || matches!(e, oneup_graph::Error::Exhausted(_)) || e.status() == Some(416)
}

/// Sends `total` bytes from `src` through session `s`, in fragments
/// `sizer` chooses, and returns the item the last one commits.
///
/// A replacement (`replace`: item ID and planned eTag) re-checks the eTag
/// right before the last fragment, because Graph checks `If-Match` only
/// when the session starts. If it moved on (an edit in OneDrive meanwhile),
/// the session is cancelled and the upload is [`CloudError::Changed`], as a
/// one-request upload's 412 is: nothing was replaced, and the next round
/// plans the conflict. That last fragment is at most one unit, so the
/// commit follows the check closely.
///
/// A fragment whose answer was lost goes on from where the session says it
/// is (`nextExpectedRanges`), at most [`SENDS`] sends in all. The session can
/// only be missing bytes of the fragment in flight, which is still in
/// memory; anything else ends the upload (the source can't be read again).
/// A session that is gone (expired, or completed by the lost answer) ends
/// it too, and the next round plans again from what OneDrive then has.
async fn send_session<S: Session>(
    s: &S,
    sizer: &std::sync::Mutex<Sizer>,
    replace: Option<(&str, &str)>,
    total: u64,
    src: &mut dyn Source,
) -> Result<DriveItem, CloudError> {
    let sizer = || sizer.lock().unwrap_or_else(|p| p.into_inner());
    let mut buf = Vec::new();
    let mut start = 0u64;
    loop {
        let left = total - start;
        let mut want = sizer().next(left);
        if replace.is_some() && want as u64 == left && want > UNIT {
            // All but the last unit (fragments are whole units but the last).
            want = (want - 1) / UNIT * UNIT;
        }
        // Grows to the largest fragment chosen, which the memory profile
        // bounds.
        if buf.len() < want {
            buf.resize(want, 0);
        }
        let (filled, read) = src.fill(std::mem::take(&mut buf), want).await;
        buf = filled;
        let n = match read {
            Ok(n) if n == want => n,
            Ok(_) => {
                s.cancel().await;
                return Err(short());
            }
            Err(e) => {
                s.cancel().await;
                return Err(CloudError::Io(e));
            }
        };
        let end = start + n as u64;
        let last = end == total;
        if last && let Some((id, etag)) = replace {
            let now = match s.etag(id).await {
                Ok(now) => now,
                Err(e) => {
                    s.cancel().await;
                    return Err(map(e));
                }
            };
            if now.as_deref() != Some(etag) {
                tracing::debug!("the file changed in OneDrive during its upload; the upload is cancelled");
                s.cancel().await;
                return Err(CloudError::Changed);
            }
        }
        // How much of this fragment the session has, and how many times it
        // was sent.
        let mut have = 0usize;
        let mut sends = 0;
        let item = loop {
            sends += 1;
            let sent = std::time::Instant::now();
            let got = s.put(start + have as u64, total, &buf[have..n]).await;
            let took = sent.elapsed();
            match got {
                Ok(f) => {
                    let secs = took.saturating_sub(f.paused + f.capped).as_secs_f64();
                    tracing::debug!(bytes = n - have, secs, sends, "upload fragment sent");
                    sizer().observe(n - have, secs, sends > 1);
                    break f.item;
                }
                Err(e) if maybe_received(&e) && sends < SENDS => match s.status().await {
                    Ok(Some(next)) if (start..=end).contains(&next) => {
                        tracing::debug!(
                            sends,
                            "an upload fragment's answer was lost; going on from the session's state"
                        );
                        have = (next - start) as usize;
                        if have == n {
                            break None;
                        }
                        s.pause(sends).await;
                    }
                    Ok(Some(_)) => {
                        s.cancel().await;
                        return Err(CloudError::Other("the upload session expects bytes already sent".into()));
                    }
                    Ok(None) if last => return Err(CloudError::Changed),
                    Ok(None) => {
                        return Err(CloudError::Other("the upload session ended before the upload finished".into()));
                    }
                    Err(_) => {
                        sizer().failed();
                        s.cancel().await;
                        return Err(map(e));
                    }
                },
                Err(e) => {
                    sizer().failed();
                    s.cancel().await;
                    return Err(map(e));
                }
            }
        };
        match item {
            Some(item) => return Ok(item),
            None if !last => start = end,
            None => {
                s.cancel().await;
                return Err(CloudError::Other("the upload session did not complete".into()));
            }
        }
    }
}

/// The version a replacement went over, from a history listed newest
/// first (the replacement itself first), if it isn't `base`'s: the entry
/// right after the newest, when `base` is further down. `None` too if
/// `base` isn't listed at all: the history can't tell, and a guess could
/// copy the wrong version. A version is `base`'s by its ID and, where the
/// history gives them, its time and size; or, failing the ID (it may
/// change once a version is no longer the current one), by time and size.
fn replaced_version<'a>(versions: &'a [FileVersion], base: &VersionMark) -> Option<&'a FileVersion> {
    let same = |v: &FileVersion| v.modified_ms.is_some() && v.modified_ms == base.modified_ms && v.size == base.size;
    let at = versions
        .iter()
        .position(|v| v.id == base.id && (base.modified_ms.is_none() || same(v)))
        .or_else(|| versions.iter().skip(1).position(same).map(|i| i + 1));
    match at {
        Some(i) if i >= 2 => versions.get(1),
        Some(_) => None,
        None => {
            tracing::debug!("the version a large upload replaced isn't in the file's history; not checked");
            None
        }
    }
}

impl Cloud for GraphCloud {
    /// Graph's own limiter paces the requests ([`oneup_graph::limiter`]).
    fn concurrent(&self) -> bool {
        true
    }

    fn upcoming(&self, downloads: &[RemotePre]) {
        self.links().announce(downloads.iter().map(|p| p.id.0.clone()), Instant::now());
    }

    async fn download(&self, pre: &RemotePre, out: &mut (dyn Write + Send)) -> Result<Option<[u8; 20]>, CloudError> {
        let (info, read_ahead) = self.link(&pre.id.0).await?;
        let info = info.ok_or(CloudError::Changed)?;
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
        let started = Instant::now();
        // The next links are read while this content arrives. A download
        // that fails, or is stopped, doesn't wait for them; one that's done
        // waits a little (an unfinished batch is dropped, and read again).
        let fetched = {
            let refill = std::pin::pin!(self.refill());
            let content = std::pin::pin!(self.graph.download_to(&url, &mut *out));
            match futures_util::future::select(content, refill).await {
                futures_util::future::Either::Left((fetched, refill)) => {
                    if fetched.is_ok() {
                        let _ = tokio::time::timeout(REFILL_WAIT, refill).await;
                    }
                    fetched
                }
                futures_util::future::Either::Right(((), content)) => content.await,
            }
        };
        let fetched = match fetched {
            // A link read ahead that no longer works (it expired sooner
            // than expected), before any byte arrived: read it again.
            Err(e) if read_ahead && e.status().is_some_and(|s| matches!(s, 401 | 403 | 404 | 410)) => {
                tracing::debug!(error = %e, "a download link read ahead didn't work; reading it again");
                let info = self.graph.download_info(&pre.id.0).await.map_err(map)?.ok_or(CloudError::Changed)?;
                if info.e_tag.as_deref() != Some(pre.etag.as_str()) {
                    return Err(CloudError::Changed);
                }
                let url = info.download_url.ok_or_else(|| CloudError::Other("no download URL".into()))?;
                self.graph.download_to(&url, out).await
            }
            r => r,
        };
        fetched.map_err(|e| match e {
            oneup_graph::Error::Local(io) => CloudError::Io(io),
            e => map(e),
        })?;
        self.timings.record("download: content", started);
        Ok(None)
    }

    async fn upload(
        &self,
        parent: &RemoteId,
        name: &str,
        replace: Option<&RemotePre>,
        size: i64,
        src: &mut dyn Source,
    ) -> Result<(RemoteRow, Option<[u8; 20]>), CloudError> {
        let total = u64::try_from(size).map_err(|_| CloudError::Other("negative size".into()))?;
        let replace = replace.map(|p| (p.id.0.as_str(), p.etag.as_str()));
        let started = Instant::now();
        let item = if total as usize <= SIMPLE_UPLOAD_MAX {
            let (buf, read) = src.fill(vec![0u8; total as usize], total as usize).await;
            if read? != buf.len() {
                return Err(short());
            }
            let item = self.graph.upload_small(&parent.0, name, replace, &buf).await.map_err(map)?;
            self.timings.record("upload: one request", started);
            item
        } else {
            let url = self.graph.upload_session(&parent.0, name, replace).await.map_err(map)?;
            let session = GraphSession { graph: &self.graph, url };
            let item = send_session(&session, &self.sizer, replace, total, src).await?;
            self.timings.record("upload: session", started);
            item
        };
        Ok((self.row_of(item).await?, None))
    }

    async fn version_mark(&self, pre: &RemotePre, size: i64) -> Result<Option<VersionMark>, CloudError> {
        if u64::try_from(size).is_ok_and(|s| s <= SIMPLE_UPLOAD_MAX as u64) {
            return Ok(None);
        }
        let versions = self.graph.versions(&pre.id.0).await.map_err(map)?;
        let now = self.graph.item(&pre.id.0).await.map_err(map)?.ok_or(CloudError::Changed)?;
        if now.e_tag.as_deref() != Some(pre.etag.as_str()) {
            return Err(CloudError::Changed);
        }
        // Read before the eTag check, which shows nothing was saved since:
        // the newest version is the one the upload replaces, unless the
        // history doesn't list it yet (then its size gives it away, and
        // nothing is checked afterwards rather than something wrong).
        Ok(versions.into_iter().next().filter(|v| v.size.is_none_or(|s| Some(s) == now.size)).map(|v| VersionMark {
            id: v.id,
            modified_ms: v.modified_ms,
            size: v.size,
        }))
    }

    async fn replaced_other(&self, id: &RemoteId, mark: &VersionMark) -> Result<Option<VersionMark>, CloudError> {
        let versions = self.graph.versions(&id.0).await.map_err(map)?;
        Ok(replaced_version(&versions, mark).map(|v| VersionMark {
            id: v.id.clone(),
            modified_ms: v.modified_ms,
            size: v.size,
        }))
    }

    async fn download_version(
        &self,
        id: &RemoteId,
        version: &str,
        out: &mut (dyn Write + Send),
    ) -> Result<u64, CloudError> {
        self.graph.download_version(&id.0, version, out).await.map_err(|e| match e {
            oneup_graph::Error::Local(io) => CloudError::Io(io),
            e => map(e),
        })
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

    async fn is_at(&self, id: &RemoteId, parent: &str, name: &str) -> Result<Option<bool>, CloudError> {
        let now = self.item(id).await?;
        Ok(Some(now.is_some_and(|r| r.parent_id.as_deref() == Some(parent) && r.name == name)))
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

#[cfg(test)]
#[allow(clippy::unwrap_used)]
mod tests {
    use std::sync::Mutex;
    use std::time::Duration;

    use super::*;

    /// An upload session as Graph keeps one, for one file in OneDrive.
    #[derive(Default)]
    struct Fake {
        /// The bytes the session has.
        got: Mutex<Vec<u8>>,
        /// The file's eTag in OneDrive.
        etag: Mutex<String>,
        /// After this many fragments the file is edited in OneDrive.
        edit_after: Option<usize>,
        /// Fragments starting here arrive, but their answer is lost (once
        /// each), ending in this status (0: every attempt failed on the
        /// network).
        lose: Mutex<Vec<(u64, u16)>>,
        /// The session expires when an answer is lost.
        expires: bool,
        /// Completed, expired or cancelled: Graph answers 404.
        gone: Mutex<bool>,
        cancelled: Mutex<bool>,
        /// `(start, length)` of every fragment received.
        puts: Mutex<Vec<(u64, usize)>>,
        /// The next this many sends fail with a server error, received or
        /// not as `fail_received` says.
        fail: Mutex<u32>,
        fail_received: bool,
        /// Every send, received or not, and every pause before one.
        sends: Mutex<u32>,
        pauses: Mutex<Vec<u32>>,
    }

    fn refused(status: u16) -> oneup_graph::Error {
        oneup_graph::Error::Status { status, what: "upload session fragment".into(), body: String::new() }
    }

    impl Session for Fake {
        async fn put(&self, start: u64, total: u64, bytes: &[u8]) -> oneup_graph::Result<Fragment> {
            *self.sends.lock().unwrap() += 1;
            if *self.gone.lock().unwrap() {
                return Err(refused(404));
            }
            let failing = {
                let mut fail = self.fail.lock().unwrap();
                let failing = *fail > 0;
                *fail = fail.saturating_sub(1);
                failing
            };
            if failing && !self.fail_received {
                return Err(refused(500));
            }
            let mut got = self.got.lock().unwrap();
            if start != got.len() as u64 {
                return Err(refused(416));
            }
            got.extend_from_slice(bytes);
            let mut puts = self.puts.lock().unwrap();
            puts.push((start, bytes.len()));
            if self.edit_after == Some(puts.len()) {
                *self.etag.lock().unwrap() = "edited on the web".into();
            }
            let done = got.len() as u64 == total;
            if failing {
                *self.gone.lock().unwrap() = done;
                return Err(refused(500));
            }
            let mut lose = self.lose.lock().unwrap();
            if let Some(i) = lose.iter().position(|(at, _)| *at == start) {
                let (_, status) = lose.remove(i);
                *self.gone.lock().unwrap() = done || self.expires;
                return Err(if status == 0 { oneup_graph::Error::Exhausted(8) } else { refused(status) });
            }
            *self.gone.lock().unwrap() = done;
            let item = done.then(|| DriveItem {
                id: "F".into(),
                name: Some("f.bin".into()),
                size: Some(total as i64),
                e_tag: Some("after".into()),
                parent_reference: None,
                file: None,
                folder: None,
                package: None,
                deleted: None,
                root: None,
                file_system_info: None,
                shared: None,
            });
            Ok(Fragment { item, paused: Duration::ZERO, capped: Duration::ZERO })
        }
        async fn status(&self) -> oneup_graph::Result<Option<u64>> {
            Ok((!*self.gone.lock().unwrap()).then(|| self.got.lock().unwrap().len() as u64))
        }
        async fn cancel(&self) {
            *self.cancelled.lock().unwrap() = true;
            *self.gone.lock().unwrap() = true;
        }
        async fn pause(&self, sends: u32) {
            self.pauses.lock().unwrap().push(sends);
        }
        async fn etag(&self, _: &str) -> oneup_graph::Result<Option<String>> {
            Ok(Some(self.etag.lock().unwrap().clone()))
        }
    }

    fn fake() -> Fake {
        Fake { etag: Mutex::new("planned".into()), ..Fake::default() }
    }

    /// Ten units and a bit: two whole fragments of four, then the rest.
    fn content() -> Vec<u8> {
        (0..10 * UNIT + 1000).map(|i| (i % 251) as u8).collect()
    }

    async fn send(s: &Fake, replace: bool, content: &[u8]) -> Result<DriveItem, CloudError> {
        let sizer = std::sync::Mutex::new(Sizer::new(4 * UNIT));
        let replace = replace.then_some(("F", "planned"));
        send_session(s, &sizer, replace, content.len() as u64, &mut &content[..]).await
    }

    const U: u64 = UNIT as u64;

    #[tokio::test]
    async fn a_replacement_edited_in_onedrive_during_its_upload_is_cancelled_before_it_commits() {
        let s = Fake { edit_after: Some(1), ..fake() };
        let c = content();
        let r = send(&s, true, &c).await;
        assert!(matches!(r, Err(CloudError::Changed)), "{r:?}");
        assert!(*s.cancelled.lock().unwrap(), "the session was cancelled");
        assert!(s.got.lock().unwrap().len() < c.len(), "the upload never completed, so nothing was replaced");
    }

    #[tokio::test]
    async fn a_replacements_last_fragment_is_small_and_sent_after_the_check() {
        let s = fake();
        let c = content();
        let item = send(&s, true, &c).await.unwrap();
        assert_eq!(item.size, Some(c.len() as i64));
        assert_eq!(*s.got.lock().unwrap(), c);
        assert_eq!(*s.puts.lock().unwrap(), [(0, 4 * UNIT), (4 * U, 4 * UNIT), (8 * U, 2 * UNIT), (10 * U, 1000)]);
        // A new file has no eTag to check: its fragments stay whole.
        let s = fake();
        send(&s, false, &c).await.unwrap();
        assert_eq!(s.puts.lock().unwrap().len(), 3);
    }

    #[tokio::test]
    async fn a_lost_fragment_answer_goes_on_from_where_the_session_is() {
        // Every attempt failed on the network; or a retry found the bytes
        // already there.
        for status in [0, 416] {
            let s = fake();
            s.lose.lock().unwrap().push((4 * U, status));
            let c = content();
            send(&s, true, &c).await.unwrap();
            assert_eq!(*s.got.lock().unwrap(), c, "every byte sent once, in order");
            let starts: Vec<u64> = s.puts.lock().unwrap().iter().map(|p| p.0).collect();
            assert_eq!(starts, [0, 4 * U, 8 * U, 10 * U], "nothing sent again");
        }
    }

    #[tokio::test]
    async fn a_lost_answer_to_the_last_fragment_plans_again() {
        // The session completed with it, so it's gone: the next round finds
        // the new version in OneDrive.
        let s = fake();
        s.lose.lock().unwrap().push((10 * U, 0));
        let r = send(&s, true, &content()).await;
        assert!(matches!(r, Err(CloudError::Changed)), "{r:?}");
    }

    #[tokio::test]
    async fn an_expired_session_or_a_refused_fragment_ends_the_upload() {
        let s = Fake { expires: true, ..fake() };
        s.lose.lock().unwrap().push((4 * U, 0));
        let r = send(&s, false, &content()).await;
        assert!(matches!(r, Err(CloudError::Other(_))), "{r:?}");

        let s = fake();
        s.lose.lock().unwrap().push((0, 403));
        let r = send(&s, false, &content()).await;
        assert!(matches!(r, Err(CloudError::Other(_))), "{r:?}");
        assert_eq!(s.puts.lock().unwrap().len(), 1, "a refusal is final");
        assert!(*s.cancelled.lock().unwrap());
    }

    /// One budget per fragment: a fragment that keeps failing is sent at
    /// most `SENDS` times in all (the client sends each once), with a
    /// growing pause before each send again, then the upload ends. One that
    /// gets through within the budget goes on, and the next fragment has a
    /// budget of its own.
    #[tokio::test]
    async fn a_fragment_is_sent_at_most_five_times() {
        let c = content();
        let s = fake();
        *s.fail.lock().unwrap() = 100;
        let r = send(&s, false, &c).await;
        assert!(matches!(r, Err(CloudError::Other(_))), "{r:?}");
        assert_eq!(*s.sends.lock().unwrap(), SENDS);
        assert_eq!(*s.pauses.lock().unwrap(), [1, 2, 3, 4]);
        assert!(*s.cancelled.lock().unwrap());

        let s = fake();
        *s.fail.lock().unwrap() = SENDS - 1;
        send(&s, false, &c).await.unwrap();
        assert_eq!(*s.got.lock().unwrap(), c);
        assert_eq!(*s.sends.lock().unwrap(), SENDS + 2, "the first fragment's budget, then two");

        // Received, the answer lost (the first two of three fragments): it
        // goes on from where the session is, so nothing is sent twice.
        let s = Fake { fail_received: true, ..fake() };
        *s.fail.lock().unwrap() = 2;
        send(&s, false, &c).await.unwrap();
        assert_eq!(*s.got.lock().unwrap(), c);
        assert_eq!(*s.sends.lock().unwrap(), 3);
        assert!(s.pauses.lock().unwrap().is_empty(), "nothing was sent again");
    }

    fn version(id: &str, modified_ms: i64, size: i64) -> FileVersion {
        FileVersion { id: id.into(), modified_ms: Some(modified_ms), size: Some(size), by: None }
    }

    #[test]
    fn the_version_an_upload_went_over_is_found_only_when_it_isnt_the_planned_one() {
        let base = VersionMark { id: "2.0".into(), modified_ms: Some(200), size: Some(20) };
        let ours = version("3.0", 300, 30);
        let theirs = version("2.5", 250, 25);
        let planned = version("2.0", 200, 20);
        // Ours went over the planned version.
        assert_eq!(replaced_version(&[ours.clone(), planned.clone()], &base), None);
        // An edit landed in between: that's the one gone over.
        let got = |list: &[FileVersion]| replaced_version(list, &base).map(|v| v.id.clone());
        assert_eq!(got(&[ours.clone(), theirs.clone(), planned.clone()]).as_deref(), Some("2.5"));
        // The planned version under another ID once it's no longer current.
        let renamed = version("x", 200, 20);
        assert_eq!(got(&[ours.clone(), theirs.clone(), renamed.clone()]).as_deref(), Some("2.5"));
        assert_eq!(replaced_version(&[ours.clone(), renamed], &base), None);
        // A history that doesn't show the planned version, or not yet ours:
        // no guess.
        assert_eq!(replaced_version(&[ours, theirs], &base), None);
        assert_eq!(replaced_version(&[planned], &base), None);
    }

    /// A full OneDrive reads as such whichever way OneDrive says it, and
    /// never as a plain failure (which would be retried as one).
    #[test]
    fn a_full_onedrive_is_recognised() {
        let refused = |status, body: &str| oneup_graph::Error::refused(status, "upload", body.into());
        assert!(matches!(map(refused(507, "")), CloudError::Full));
        assert!(matches!(map(refused(507, r#"{"error":{"code":"quotaLimitReached"}}"#)), CloudError::Full));
        assert!(matches!(map(refused(403, r#"{"error":{"code":"quotaLimitReached"}}"#)), CloudError::Full));
        assert!(matches!(map(refused(400, r#"{"error":{"code":"insufficientStorage"}}"#)), CloudError::Full));
        assert!(matches!(map(refused(500, "")), CloudError::Other(_)));
        assert!(matches!(map(refused(412, "")), CloudError::Changed));
    }

    fn info(id: &str) -> DownloadInfo {
        DownloadInfo { id: id.into(), e_tag: Some("e".into()), folder: None, download_url: Some("u".into()) }
    }

    #[test]
    fn a_missing_link_is_read_with_the_coming_ones_and_each_used_once() {
        let mut l = Links::default();
        let now = Instant::now();
        l.announce((0..30).map(|k| format!("i{k}")), now);
        l.announce(["i3".to_string()], now);
        assert_eq!(l.ahead.len(), 30, "announced once");
        let Took::Batch(first) = l.claim("i0", now) else { panic!("not read yet") };
        assert_eq!(first.len(), BATCH_MAX, "a batch's worth");
        assert_eq!(first[0], "i0");
        assert!(first.iter().skip(1).all(|id| id != "i0"), "{first:?}");
        // While they're read, their downloads wait instead of reading again.
        assert!(matches!(l.claim("i1", now), Took::Reading));
        let rest = l.refill(now);
        assert_eq!(rest.len(), 30 - BATCH_MAX, "the rest, and nothing being read");
        l.announce((0..30).map(|k| format!("j{k}")), now);
        assert!(l.refill(now).is_empty(), "two batches are being read");
        l.finish(&rest);
        for id in &first[1..] {
            l.keep(id.clone(), info(id), now);
        }
        l.finish(&first);
        assert_eq!(l.batches, 0);
        // 19 links in hand: room for 41 more, a batch at a time.
        let next = l.refill(now);
        assert_eq!(next.len(), BATCH_MAX);
        l.finish(&next);
        for k in 0..LINKS_AHEAD - 19 - 5 {
            l.keep(format!("h{k}"), info("h"), now);
        }
        assert_eq!(l.refill(now).len(), 5, "only the room left under the limit");
        assert!(matches!(l.claim("i1", now), Took::Link(i) if i.id == "i1"));
        assert!(matches!(l.claim("i1", now), Took::Batch(ids) if ids[0] == "i1"), "taken once: read again");
    }

    #[test]
    fn a_missing_link_keeps_to_the_bounds_too() {
        let mut l = Links::default();
        let now = Instant::now();
        l.announce((0..100).map(|k| format!("i{k}")), now);
        let (a, b) = (l.refill(now), l.refill(now));
        assert_eq!((a.len(), b.len(), l.batches), (BATCH_MAX, BATCH_MAX, 2));
        assert!(l.refill(now).is_empty());
        // Two batches are being read: a download whose link isn't among
        // them reads only its own.
        assert!(matches!(l.claim("x", now), Took::Batch(ids) if ids == ["x"]));
        l.finish(&a);
        l.finish(&b);
        l.finish(&["x".to_string()]);
        // Room for 5 more under the limit: it and 4 coming ones.
        for k in 0..LINKS_AHEAD - 5 {
            l.keep(format!("h{k}"), info("h"), now);
        }
        assert!(matches!(l.claim("y", now), Took::Batch(ids) if ids.len() == 5 && ids[0] == "y"));
    }

    #[test]
    fn links_read_ahead_go_stale_and_stay_bounded() {
        let mut l = Links::default();
        let then = Instant::now();
        l.keep("a".into(), info("a"), then);
        assert!(matches!(l.claim("a", then + LINK_FRESH), Took::Batch(_)), "stale after a minute");
        l.announce(["old".to_string()], then);
        l.announce(["new".to_string()], then + AHEAD_STALE);
        assert_eq!(l.ahead.iter().map(|(id, _)| id.as_str()).collect::<Vec<_>>(), ["new"], "one waiting too long goes");
        l.announce((0..LINKS_MAX + 10).map(|k| format!("i{k}")), then + AHEAD_STALE);
        assert_eq!((l.ahead.len(), l.queued.len()), (LINKS_MAX, LINKS_MAX));
        for k in 0..LINKS_MAX + 10 {
            l.keep(format!("k{k}"), info("k"), then);
        }
        assert_eq!(l.got.len(), LINKS_MAX);
        // Its debug output holds no link.
        assert!(!format!("{l:?}").contains('u'));
    }
}
