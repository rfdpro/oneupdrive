//! Throttling-aware Graph requests and the drive endpoints the mirror uses.

use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering::Relaxed};
use std::time::Duration;

use serde::de::DeserializeOwned;

use crate::model::{DriveItem, ITEM_SELECT, Page};
use crate::{Aimd, Auth, Error, Result};

pub const GRAPH: &str = "https://graph.microsoft.com/v1.0";
/// Largest page size Graph honours for drive collections (measured 2.6× faster than the default).
pub const PAGE_SIZE: u32 = 1000;
const MAX_ATTEMPTS: u32 = 8;
/// Throttle responses tolerated for one request before giving up.
const MAX_THROTTLES: u32 = 50;

/// Counters for progress reporting and diagnostics.
#[derive(Debug, Default)]
pub struct NetStats {
    pub requests: AtomicU64,
    pub retries: AtomicU64,
    pub throttled: AtomicU64,
    pub bytes: AtomicU64,
    /// Longest `Retry-After` seen, in seconds.
    pub retry_after_max_s: AtomicU64,
    /// Responses carrying `RateLimit-*` early-warning headers.
    pub rate_limit_warnings: AtomicU64,
}

#[derive(Debug)]
pub struct Graph {
    http: reqwest::Client,
    auth: Arc<Auth>,
    limiter: Arc<Aimd>,
    /// Which drive requests address (`/me/drive`, or `/drives/{id}` for a
    /// folder someone shared), and the folder that stands in for its root
    /// if only a folder is synced ([`Graph::scoped`]).
    drive: String,
    scope: Option<String>,
    pub stats: NetStats,
    /// Bandwidth caps for file content (`rules.md` § User controls).
    pub upload: crate::throttle::Throttle,
    pub download: crate::throttle::Throttle,
}

/// Percent-encodes a drive path for `root:/path:` addressing. Always uses
/// `/` separators: Graph happens to accept `\`, but we never rely on it.
pub fn encode_drive_path(path: &str) -> String {
    path.split(['/', '\\'])
        .filter(|s| !s.is_empty())
        .map(|seg| {
            seg.bytes()
                .map(|b| match b {
                    b'A'..=b'Z' | b'a'..=b'z' | b'0'..=b'9' | b'-' | b'.' | b'_' | b'~' => (b as char).to_string(),
                    _ => format!("%{b:02X}"),
                })
                .collect::<String>()
        })
        .collect::<Vec<_>>()
        .join("/")
}

fn encode_id(id: &str) -> String {
    encode_drive_path(id)
}

/// One request, described so it can be rebuilt for each attempt.
struct Req<'a> {
    method: reqwest::Method,
    url: &'a str,
    /// Send the bearer token. Off for pre-authenticated upload/download
    /// URLs, which reject it (and must not receive it).
    auth: bool,
    if_match: Option<&'a str>,
    json: Option<&'a serde_json::Value>,
    bytes: Option<&'a [u8]>,
    content_range: Option<String>,
    timeout: Option<Duration>,
    /// Shown in errors instead of the URL (pre-authenticated URLs carry a
    /// session token and must never be logged).
    label: Option<&'static str>,
}

impl<'a> Req<'a> {
    fn new(method: reqwest::Method, url: &'a str) -> Self {
        Self {
            method,
            url,
            auth: true,
            if_match: None,
            json: None,
            bytes: None,
            content_range: None,
            timeout: None,
            label: None,
        }
    }
}

/// Simple uploads are allowed up to 250 MB, but Microsoft recommends upload
/// sessions above 4 MiB, and measurement agrees: a one-request upload goes
/// through Graph itself and is slower per byte than a session's fragments,
/// which go straight to storage, so above 4 MiB the session's extra request
/// is already paid back.
pub const SIMPLE_UPLOAD_MAX: usize = 4 * 1024 * 1024;
/// The fragment size used before fragments were sized adaptively
/// (`crate::fragment`), still the sizer's starting point: 10 MiB.
pub const FRAGMENT: usize = crate::fragment::START_UNITS * crate::fragment::UNIT;
/// Upper bound for one transfer request. The client's default timeout
/// (120 s) is for metadata; a fragment or a large download legitimately
/// takes longer on a slow line.
const TRANSFER_TIMEOUT: Duration = Duration::from_secs(6 * 3600);

/// What [`Graph::upload_fragment`] reports.
#[derive(Debug)]
pub struct Fragment {
    /// The uploaded item, once the last fragment completed it.
    pub item: Option<DriveItem>,
    /// It took more than one attempt (a network error or a server error).
    pub retried: bool,
    /// Time spent waiting because the service asked for it (throttling).
    pub paused: Duration,
    /// Time spent waiting for the user's bandwidth cap.
    pub capped: Duration,
}

/// How one request went, for [`Graph::send_measured`].
#[derive(Debug, Default)]
struct Measure {
    attempts: u32,
    /// Waiting for the limiter: throttling pauses all requests there.
    paused: Duration,
    /// The response's `Location` header (asynchronous jobs).
    location: Option<String>,
}

/// Most requests one JSON batch may carry.
pub const BATCH_MAX: usize = 20;

/// One request inside a JSON batch ([`Graph::batch`]).
#[derive(Debug, Clone)]
pub struct BatchReq {
    pub method: reqwest::Method,
    /// Relative to the API version, e.g. `/me/drive/items/{id}`.
    pub url: String,
    pub if_match: Option<String>,
    pub body: Option<serde_json::Value>,
}

/// Its answer.
#[derive(Debug, Clone)]
pub struct BatchResp {
    pub status: u16,
    pub body: serde_json::Value,
}

impl BatchResp {
    /// An item in a 2xx answer.
    pub fn item(&self) -> Result<DriveItem> {
        if !(200..300).contains(&self.status) {
            return Err(self.error("batched request"));
        }
        Ok(serde_json::from_value(self.body.clone())?)
    }

    /// OneDrive's error code, for a refused request.
    pub fn code(&self) -> Option<String> {
        self.body.pointer("/error/code")?.as_str().map(str::to_string)
    }

    pub fn error(&self, what: &str) -> Error {
        Error::Status { status: self.status, what: what.into(), body: self.body.to_string() }
    }
}

/// Where a server-side copy stands ([`Graph::copy_status`]).
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum CopyStatus {
    Running,
    /// Done: the new item's ID.
    Done(String),
    Failed(String),
}

/// Writes to OneDrive. Every change to an existing item is conditional on
/// its eTag (`If-Match`), and every creation fails rather than replaces if
/// the name is taken (`data-safety.md` invariant 5). Only usable with a token
/// that has `Files.ReadWrite` (see the crate docs).
impl Graph {
    /// An item's eTag, kind and pre-authenticated download URL. `None` if
    /// it no longer exists.
    pub async fn download_info(&self, id: &str) -> Result<Option<crate::model::DownloadInfo>> {
        // `$select` keeps the response small, but whether it returns the
        // annotation varies by service version; fall back to the full item.
        let narrow =
            format!("{}/items/{}?$select=id,eTag,folder,@microsoft.graph.downloadUrl", self.drive_url(), encode_id(id));
        let full = format!("{}/items/{}", self.drive_url(), encode_id(id));
        for url in [narrow, full] {
            match self.get::<crate::model::DownloadInfo>(&url).await {
                Ok(it) if it.download_url.is_some() || it.folder.is_some() => return Ok(Some(it)),
                Ok(_) => continue,
                Err(e) if e.status() == Some(404) => return Ok(None),
                Err(e) => return Err(e),
            }
        }
        Ok(Some(crate::model::DownloadInfo { id: id.to_string(), e_tag: None, folder: None, download_url: None }))
    }

    /// The item at drive path `path` ("/a/b.txt"), or `None`.
    pub async fn item_by_path(&self, path: &str) -> Result<Option<DriveItem>> {
        let url = format!("{}:/{}", self.top_url(), encode_drive_path(path.trim_start_matches('/')));
        match self.get(&url).await {
            Ok(it) => Ok(Some(it)),
            Err(e) if e.status() == Some(404) => Ok(None),
            Err(e) => Err(e),
        }
    }

    /// Streams a pre-authenticated download URL into `out`. Not retried
    /// once bytes have been written: the caller verifies the whole file by
    /// hash and starts over on failure.
    pub async fn download_to(&self, url: &str, out: &mut (dyn std::io::Write + Send)) -> Result<u64> {
        let _permit = self.limiter.acquire().await;
        self.stats.requests.fetch_add(1, Relaxed);
        let mut resp = self.http.get(url).timeout(TRANSFER_TIMEOUT).send().await?;
        let status = resp.status().as_u16();
        if !(200..300).contains(&status) {
            if status == 429 || status == 503 {
                let wait = header_u64(resp.headers(), reqwest::header::RETRY_AFTER.as_str())
                    .map_or(backoff(1), Duration::from_secs);
                self.limiter.on_throttle(wait);
            }
            return Err(Error::Status { status, what: "download".into(), body: String::new() });
        }
        let mut n = 0u64;
        while let Some(chunk) = resp.chunk().await? {
            out.write_all(&chunk).map_err(Error::Local)?;
            n += chunk.len() as u64;
            // Not reading meanwhile slows the sender down (TCP flow control).
            self.download.take(chunk.len() as u64).await;
        }
        self.stats.bytes.fetch_add(n, Relaxed);
        Ok(n)
    }

    /// Creates folder `name` in `parent`; 409 if the name is taken.
    pub async fn create_folder(&self, parent: &str, name: &str) -> Result<DriveItem> {
        let url = format!("{}/items/{}/children", self.drive_url(), encode_id(parent));
        let body = serde_json::json!({ "name": name, "folder": {}, "@microsoft.graph.conflictBehavior": "fail" });
        let req = Req { json: Some(&body), ..Req::new(reqwest::Method::POST, &url) };
        self.json(&req).await
    }

    /// Uploads a small file (at most [`SIMPLE_UPLOAD_MAX`]) in one request:
    /// as a new item `name` in `parent` (409 if taken), or as a new version
    /// of `replace = (id, eTag)` (412 if the eTag moved on).
    pub async fn upload_small(
        &self,
        parent: &str,
        name: &str,
        replace: Option<(&str, &str)>,
        content: &[u8],
    ) -> Result<DriveItem> {
        self.upload.take(content.len() as u64).await;
        let url = match replace {
            Some((id, _)) => format!("{}/items/{}/content", self.drive_url(), encode_id(id)),
            None => format!(
                "{}/items/{}:/{}:/content?@microsoft.graph.conflictBehavior=fail",
                self.drive_url(),
                encode_id(parent),
                encode_drive_path(name)
            ),
        };
        let req = Req {
            bytes: Some(content),
            if_match: replace.map(|(_, tag)| tag),
            timeout: Some(TRANSFER_TIMEOUT),
            ..Req::new(reqwest::Method::PUT, &url)
        };
        self.json(&req).await
    }

    /// Starts an upload session (see [`Graph::upload_small`] for the
    /// conditions) and returns its pre-authenticated upload URL.
    pub async fn upload_session(&self, parent: &str, name: &str, replace: Option<(&str, &str)>) -> Result<String> {
        let (url, behavior) = match replace {
            Some((id, _)) => (format!("{}/items/{}/createUploadSession", self.drive_url(), encode_id(id)), "replace"),
            None => (
                format!(
                    "{}/items/{}:/{}:/createUploadSession",
                    self.drive_url(),
                    encode_id(parent),
                    encode_drive_path(name)
                ),
                "fail",
            ),
        };
        let body = serde_json::json!({ "item": { "@microsoft.graph.conflictBehavior": behavior } });
        let req =
            Req { json: Some(&body), if_match: replace.map(|(_, tag)| tag), ..Req::new(reqwest::Method::POST, &url) };
        let s: crate::model::UploadSession = self.json(&req).await?;
        Ok(s.upload_url)
    }

    /// Sends bytes `start..start + bytes.len()` of a `total`-byte upload.
    /// The item comes back once the last fragment completes it.
    pub async fn upload_fragment(&self, upload_url: &str, start: u64, total: u64, bytes: &[u8]) -> Result<Fragment> {
        let capped = std::time::Instant::now();
        self.upload.take(bytes.len() as u64).await;
        let capped = capped.elapsed();
        let end = start + bytes.len() as u64 - 1;
        let req = Req {
            auth: false,
            bytes: Some(bytes),
            content_range: Some(format!("bytes {start}-{end}/{total}")),
            timeout: Some(TRANSFER_TIMEOUT),
            label: Some("upload session fragment"),
            ..Req::new(reqwest::Method::PUT, upload_url)
        };
        let (status, body, m) = self.send_measured(&req).await?;
        let item = if status == 202 { None } else { Some(serde_json::from_slice(&body)?) };
        Ok(Fragment { item, retried: m.attempts > 1, paused: m.paused, capped })
    }

    /// Asks OneDrive to copy file `source` into folder `parent` as `name`
    /// (409 if the name is taken: never replaces). The copy runs on the
    /// server; returns the URL to follow it with [`Graph::copy_status`].
    pub async fn copy_item(&self, source: &str, parent: &str, name: &str) -> Result<String> {
        let url =
            format!("{}/items/{}/copy?@microsoft.graph.conflictBehavior=fail", self.drive_url(), encode_id(source));
        let body = serde_json::json!({ "parentReference": { "id": parent }, "name": name });
        let req = Req { json: Some(&body), ..Req::new(reqwest::Method::POST, &url) };
        let (_, _, m) = self.send_measured(&req).await?;
        m.location.ok_or_else(|| Error::Status { status: 202, what: "copy".into(), body: "no monitor URL".into() })
    }

    /// Where a copy started by [`Graph::copy_item`] stands. The monitor URL
    /// is pre-authenticated (no token is sent, and it is never logged).
    pub async fn copy_status(&self, monitor: &str) -> Result<CopyStatus> {
        let req = Req { auth: false, label: Some("copy monitor"), ..Req::new(reqwest::Method::GET, monitor) };
        let (_, body) = self.send(&req).await?;
        let v: serde_json::Value = serde_json::from_slice(&body)?;
        // Finished copies answer either with the job's status or (after a
        // redirect) with the new item itself.
        if v.get("file").is_some()
            && let Some(id) = v.get("id").and_then(|i| i.as_str())
        {
            return Ok(CopyStatus::Done(id.to_string()));
        }
        let status = v.get("status").and_then(|s| s.as_str()).unwrap_or_default();
        Ok(match status {
            "completed" => match v.get("resourceId").and_then(|i| i.as_str()) {
                Some(id) => CopyStatus::Done(id.to_string()),
                None => CopyStatus::Failed("completed without the new item's ID".into()),
            },
            "failed" | "cancelled" | "deleteFailed" => {
                CopyStatus::Failed(v.get("error").map(|e| e.to_string()).unwrap_or_else(|| status.to_string()))
            }
            _ => CopyStatus::Running,
        })
    }

    /// Sends up to [`BATCH_MAX`] requests in one round trip (JSON batching).
    /// Each keeps its own headers (`If-Match`) and gets its own answer, in
    /// the order given. Requests the service throttled or failed with a
    /// server error are sent again (after `Retry-After`, which also pauses
    /// every other request), a bounded number of times; what is still
    /// failing then is answered with its last status.
    pub async fn batch(&self, reqs: &[BatchReq]) -> Result<Vec<BatchResp>> {
        assert!(reqs.len() <= BATCH_MAX, "a batch holds at most {BATCH_MAX} requests");
        let mut out: Vec<Option<BatchResp>> = vec![None; reqs.len()];
        let mut todo: Vec<usize> = (0..reqs.len()).collect();
        let url = format!("{GRAPH}/$batch");
        for attempt in 1..=MAX_ATTEMPTS {
            if todo.is_empty() {
                break;
            }
            let requests: Vec<serde_json::Value> = todo
                .iter()
                .map(|&i| {
                    let r = &reqs[i];
                    let mut headers = serde_json::Map::new();
                    if let Some(tag) = &r.if_match {
                        headers.insert("If-Match".into(), tag.clone().into());
                    }
                    let mut v = serde_json::json!({ "id": i.to_string(), "method": r.method.as_str(), "url": r.url });
                    if let Some(b) = &r.body {
                        headers.insert("Content-Type".into(), "application/json".into());
                        v["body"] = b.clone();
                    }
                    v["headers"] = serde_json::Value::Object(headers);
                    v
                })
                .collect();
            let body = serde_json::json!({ "requests": requests });
            let req = Req { json: Some(&body), ..Req::new(reqwest::Method::POST, &url) };
            let (_, answer) = self.send(&req).await?;
            let answer: serde_json::Value = serde_json::from_slice(&answer)?;
            let mut again = Vec::new();
            let mut wait: Option<Duration> = None;
            for r in answer.get("responses").and_then(|r| r.as_array()).into_iter().flatten() {
                let Some(i) = r.get("id").and_then(|i| i.as_str()).and_then(|i| i.parse::<usize>().ok()) else {
                    continue;
                };
                if i >= reqs.len() {
                    continue;
                }
                let status = r.get("status").and_then(serde_json::Value::as_u64).unwrap_or(0) as u16;
                let resp = BatchResp { status, body: r.get("body").cloned().unwrap_or(serde_json::Value::Null) };
                let retry = status == 429 || (500..600).contains(&status);
                if retry && attempt < MAX_ATTEMPTS {
                    if status == 429 || status == 503 {
                        let after = r
                            .get("headers")
                            .and_then(|h| h.get("Retry-After").or_else(|| h.get("retry-after")))
                            .and_then(|v| v.as_str().map(str::to_string).or_else(|| v.as_u64().map(|n| n.to_string())))
                            .and_then(|s| s.parse::<u64>().ok())
                            .map(Duration::from_secs)
                            .unwrap_or_else(|| backoff(attempt));
                        wait = Some(wait.map_or(after, |w| w.max(after)));
                    }
                    again.push(i);
                }
                out[i] = Some(resp);
            }
            // Requests the answer left out are sent again too.
            again.extend(todo.iter().copied().filter(|&i| out[i].is_none()));
            again.sort_unstable();
            again.dedup();
            if again.is_empty() {
                break;
            }
            match wait {
                Some(w) => {
                    self.limiter.on_throttle(w);
                    self.stats.throttled.fetch_add(1, Relaxed);
                    tracing::info!(wait_s = w.as_secs(), "batched requests throttled by Graph; pausing all requests");
                }
                None => {
                    self.stats.retries.fetch_add(1, Relaxed);
                    tokio::time::sleep(backoff(attempt)).await;
                }
            }
            todo = again;
        }
        Ok(out.into_iter().map(|r| r.unwrap_or(BatchResp { status: 503, body: serde_json::Value::Null })).collect())
    }

    /// The batch request that creates folder `name` in `parent` (409 if taken).
    pub fn batch_create_folder(&self, parent: &str, name: &str) -> BatchReq {
        BatchReq {
            method: reqwest::Method::POST,
            url: format!("{}/items/{}/children", self.drive, encode_id(parent)),
            if_match: None,
            body: Some(serde_json::json!({ "name": name, "folder": {}, "@microsoft.graph.conflictBehavior": "fail" })),
        }
    }

    /// The batch request that moves/renames `id` if its eTag is still `etag`.
    pub fn batch_move(&self, id: &str, etag: &str, to: &str, name: &str) -> BatchReq {
        BatchReq {
            method: reqwest::Method::PATCH,
            url: format!("{}/items/{}", self.drive, encode_id(id)),
            if_match: Some(etag.to_string()),
            body: Some(serde_json::json!({ "parentReference": { "id": to }, "name": name })),
        }
    }

    /// The batch request that reads item `id` (with its child count).
    pub fn batch_get(&self, id: &str) -> BatchReq {
        BatchReq {
            method: reqwest::Method::GET,
            url: format!("{}/items/{}", self.drive, encode_id(id)),
            if_match: None,
            body: None,
        }
    }

    /// The batch request that deletes `id` if its eTag is still `etag`
    /// (**with** its contents: callers check folders are empty first).
    pub fn batch_delete(&self, id: &str, etag: &str) -> BatchReq {
        BatchReq {
            method: reqwest::Method::DELETE,
            url: format!("{}/items/{}", self.drive, encode_id(id)),
            if_match: Some(etag.to_string()),
            body: None,
        }
    }

    /// Abandons an upload session (best effort; sessions also expire).
    pub async fn cancel_upload(&self, upload_url: &str) {
        let req = Req { auth: false, label: Some("upload session"), ..Req::new(reqwest::Method::DELETE, upload_url) };
        let _ = self.send(&req).await;
    }

    /// Moves/renames an item, if its eTag is still `etag` (412 otherwise)
    /// and the name is free in `to` (409 otherwise).
    pub async fn move_item(&self, id: &str, etag: &str, to: &str, name: &str) -> Result<DriveItem> {
        let url = format!("{}/items/{}", self.drive_url(), encode_id(id));
        let body = serde_json::json!({ "parentReference": { "id": to }, "name": name });
        let req = Req { json: Some(&body), if_match: Some(etag), ..Req::new(reqwest::Method::PATCH, &url) };
        self.json(&req).await
    }

    /// Deletes an item into the OneDrive recycle bin, if its eTag is still
    /// `etag`. **Graph deletes a folder with everything in it**: callers
    /// must check that a folder is empty first.
    pub async fn delete_item(&self, id: &str, etag: &str) -> Result<()> {
        let url = format!("{}/items/{}", self.drive_url(), encode_id(id));
        let req = Req { if_match: Some(etag), ..Req::new(reqwest::Method::DELETE, &url) };
        self.send(&req).await.map(|_| ())
    }

    async fn json<T: DeserializeOwned>(&self, req: &Req<'_>) -> Result<T> {
        let (_, body) = self.send(req).await?;
        Ok(serde_json::from_slice(&body)?)
    }
}

impl Graph {
    pub fn new(http: reqwest::Client, auth: Arc<Auth>, limiter: Arc<Aimd>) -> Self {
        Self {
            http,
            auth,
            limiter,
            stats: NetStats::default(),
            upload: crate::throttle::Throttle::default(),
            download: crate::throttle::Throttle::default(),
            drive: "/me/drive".into(),
            scope: None,
        }
    }

    /// Addresses only folder `root` (by ID) of drive `drive` (another
    /// person's drive, for a folder they shared; `None`: the signed-in
    /// account's own): listing starts there, the folder takes the place of
    /// the drive's root (its items come back with the `root` facet and no
    /// parent), and the change feed covers only it (folder-scoped delta,
    /// which OneDrive Personal allows on any folder).
    #[must_use]
    pub fn scoped(mut self, drive: Option<&str>, root: &str) -> Self {
        if let Some(d) = drive {
            self.drive = format!("/drives/{}", encode_id(d));
        }
        self.scope = Some(root.to_string());
        self
    }

    /// The scope's root folder, if the client is [`Graph::scoped`].
    pub fn scope_root(&self) -> Option<&str> {
        self.scope.as_deref()
    }

    /// Whether requests address another person's drive.
    pub fn is_other_drive(&self) -> bool {
        self.drive != "/me/drive"
    }

    fn drive_url(&self) -> String {
        format!("{GRAPH}{}", self.drive)
    }

    /// Makes the scope's root folder look like a drive root.
    fn rooted(&self, mut item: DriveItem) -> DriveItem {
        if self.scope.as_deref() == Some(item.id.as_str()) && !item.is_deleted() {
            item.root = Some(crate::model::Facet {});
            item.parent_reference = None;
        }
        item
    }

    pub fn limiter(&self) -> &Arc<Aimd> {
        &self.limiter
    }

    /// GET a URL (absolute, e.g. a nextLink) and decode JSON, handling
    /// throttling (429/503 + Retry-After), transient failures and token expiry.
    pub async fn get<T: DeserializeOwned>(&self, url: &str) -> Result<T> {
        let (_, body) = self.send(&Req::new(reqwest::Method::GET, url)).await?;
        Ok(serde_json::from_slice(&body)?)
    }

    /// Sends one request (rebuilt on every attempt), with the same
    /// throttling, retry and token handling for every method. Returns the
    /// status and body of a 2xx response; anything else is
    /// [`Error::Status`].
    ///
    /// Retrying writes is safe here because every write OneUpDrive sends is
    /// conditional: a change to an existing item carries `If-Match` (a
    /// retry after a lost success then fails with 412 instead of applying
    /// twice), and a creation uses `conflictBehavior=fail` (409 instead of
    /// a duplicate). The executor treats both as "re-plan", and the next
    /// round finds the change already made.
    async fn send(&self, req: &Req<'_>) -> Result<(u16, Vec<u8>)> {
        self.send_measured(req).await.map(|(status, body, _)| (status, body))
    }

    /// [`Graph::send`], also saying how many attempts it took and how long
    /// it waited because the service asked it to (throttling).
    async fn send_measured(&self, req: &Req<'_>) -> Result<(u16, Vec<u8>, Measure)> {
        let url = req.url;
        let mut attempt = 0u32;
        let mut measure = Measure::default();
        // Throttling is the service pacing us, not a failure: honouring
        // Retry-After doesn't use up attempts (but is bounded, to never hang forever).
        let mut throttled = 0u32;
        loop {
            attempt += 1;
            if attempt > MAX_ATTEMPTS || throttled > MAX_THROTTLES {
                return Err(Error::Exhausted(attempt));
            }
            let mut rb = self.http.request(req.method.clone(), url);
            if req.auth {
                rb = rb.bearer_auth(self.auth.access_token().await?);
            }
            if let Some(tag) = req.if_match {
                rb = rb.header(reqwest::header::IF_MATCH, tag);
            }
            if let Some(range) = &req.content_range {
                rb = rb.header(reqwest::header::CONTENT_RANGE, range.as_str());
            }
            if let Some(t) = req.timeout {
                rb = rb.timeout(t);
            }
            rb = match (req.json, req.bytes) {
                (Some(j), _) => rb.json(j),
                // An empty body goes out without a length, which Graph
                // refuses with 411 (seen uploading empty files).
                (None, Some([])) => rb.header(reqwest::header::CONTENT_LENGTH, 0).body(Vec::new()),
                (None, Some(b)) => rb.body(b.to_vec()),
                // Graph wants an explicit zero length on bodiless writes.
                (None, None) if req.method != reqwest::Method::GET => rb.header(reqwest::header::CONTENT_LENGTH, 0),
                (None, None) => rb,
            };
            let waiting = std::time::Instant::now();
            let permit = self.limiter.acquire().await;
            measure.paused += waiting.elapsed();
            measure.attempts += 1;
            self.stats.requests.fetch_add(1, Relaxed);
            let sent = rb.send().await;
            let resp = match sent {
                Ok(r) => r,
                Err(e) => {
                    drop(permit);
                    self.stats.retries.fetch_add(1, Relaxed);
                    tracing::warn!(error = %e, attempt, "network error; retrying");
                    tokio::time::sleep(backoff(attempt)).await;
                    continue;
                }
            };
            let status = resp.status().as_u16();
            self.observe_rate_limit_headers(resp.headers());
            let location =
                resp.headers().get(reqwest::header::LOCATION).and_then(|l| l.to_str().ok()).map(str::to_string);
            if status == 429 || status == 503 {
                let wait = header_u64(resp.headers(), reqwest::header::RETRY_AFTER.as_str())
                    .map(Duration::from_secs)
                    .unwrap_or_else(|| backoff(throttled + 1));
                drop(permit);
                // Pauses *all* requests: throttled calls still count against the quota.
                self.limiter.on_throttle(wait);
                self.stats.throttled.fetch_add(1, Relaxed);
                self.stats.retry_after_max_s.fetch_max(wait.as_secs(), Relaxed);
                tracing::info!(status, wait_s = wait.as_secs(), "throttled by Graph; pausing all requests");
                throttled += 1;
                attempt -= 1;
                // The service pacing us isn't a retry (the wait counts as paused).
                measure.attempts -= 1;
                continue;
            }
            if status == 401 && req.auth && attempt <= 2 {
                drop(permit);
                self.auth.invalidate().await;
                continue;
            }
            if (500..600).contains(&status) {
                drop(permit);
                self.stats.retries.fetch_add(1, Relaxed);
                tokio::time::sleep(backoff(attempt)).await;
                continue;
            }
            let body = match resp.bytes().await {
                Ok(b) => b,
                Err(e) => {
                    drop(permit);
                    self.stats.retries.fetch_add(1, Relaxed);
                    tracing::warn!(error = %e, attempt, "body read failed; retrying");
                    tokio::time::sleep(backoff(attempt)).await;
                    continue;
                }
            };
            drop(permit);
            self.stats.bytes.fetch_add(body.len() as u64, Relaxed);
            measure.location = location;
            if !(200..300).contains(&status) {
                let what = match req.label {
                    Some(l) => l.to_string(),
                    None => url.split('?').next().unwrap_or(url).to_string(),
                };
                return Err(Error::Status { status, what, body: String::from_utf8_lossy(&body).into_owned() });
            }
            self.limiter.on_success();
            return Ok((status, body.to_vec(), measure));
        }
    }

    /// OneDrive sends `RateLimit-*` headers (best effort) once usage passes
    /// ~80% of a window; use them to slow down before being throttled.
    fn observe_rate_limit_headers(&self, h: &reqwest::header::HeaderMap) {
        if let (Some(remaining), Some(limit)) = (header_u64(h, "ratelimit-remaining"), header_u64(h, "ratelimit-limit"))
        {
            let reset = Duration::from_secs(header_u64(h, "ratelimit-reset").unwrap_or(60));
            self.stats.rate_limit_warnings.fetch_add(1, Relaxed);
            tracing::debug!(remaining, limit, reset_s = reset.as_secs(), "RateLimit headers");
            self.limiter.on_pressure(remaining, limit, reset);
        }
    }

    pub async fn page(&self, url: &str) -> Result<Page> {
        let mut page: Page = self.get(url).await?;
        if self.scope.is_some() {
            page.value = page.value.into_iter().map(|it| self.rooted(it)).collect();
        }
        Ok(page)
    }

    /// One page of a folder's children (`next`: the previous page's
    /// `nextLink`). Read-only.
    pub async fn children_page(&self, id: &str, next: Option<&str>) -> Result<Page> {
        let url = match next {
            Some(n) => n.to_string(),
            None => {
                format!("{}/items/{}/children?$top=200&$select=id,name,size,folder", self.drive_url(), encode_id(id))
            }
        };
        self.page(&url).await
    }

    /// The drive's root folder, or the scope's ([`Graph::scoped`]).
    pub async fn root(&self) -> Result<DriveItem> {
        match &self.scope {
            Some(id) => {
                let it =
                    self.get(&format!("{}/items/{}?$select={ITEM_SELECT}", self.drive_url(), encode_id(id))).await?;
                Ok(self.rooted(it))
            }
            None => self.get(&format!("{}/root?$select={ITEM_SELECT}", self.drive_url())).await,
        }
    }

    /// The signed-in account's drive: identifies *which* OneDrive a token
    /// reaches (used to keep write access confined to a test account).
    pub async fn drive(&self) -> Result<crate::model::Drive> {
        self.get(&format!("{}?$select=id,driveType,owner", self.drive_url())).await
    }

    /// The account's storage: (used, total) bytes.
    pub async fn quota(&self) -> Result<Option<(i64, i64)>> {
        let v: serde_json::Value = self.get(&format!("{GRAPH}/me/drive?$select=quota")).await?;
        let n = |k: &str| v.pointer(&format!("/quota/{k}")).and_then(serde_json::Value::as_i64);
        Ok(n("used").zip(n("total")))
    }

    /// `None` if the item no longer exists.
    pub async fn item(&self, id: &str) -> Result<Option<DriveItem>> {
        match self.get(&format!("{}/items/{}?$select={ITEM_SELECT}", self.drive_url(), encode_id(id))).await {
            Ok(it) => Ok(Some(self.rooted(it))),
            Err(e) if e.status() == Some(404) => Ok(None),
            Err(e) => Err(e),
        }
    }

    /// The item's WebDAV address (`webDavUrl`; for OneDrive Personal,
    /// `https://d.docs.live.net/…`): what Office opens to edit a file in
    /// OneDrive itself, with AutoSave. `None` if it no longer exists.
    pub async fn web_dav_url(&self, id: &str) -> Result<Option<String>> {
        match self
            .get::<serde_json::Value>(&format!("{}/items/{}?$select=webDavUrl", self.drive_url(), encode_id(id)))
            .await
        {
            Ok(v) => Ok(v.get("webDavUrl").and_then(|u| u.as_str()).map(str::to_string)),
            Err(e) if e.status() == Some(404) => Ok(None),
            Err(e) => Err(e),
        }
    }

    /// The item's page on OneDrive's website; `None` if it no longer exists.
    pub async fn web_url(&self, id: &str) -> Result<Option<String>> {
        match self
            .get::<serde_json::Value>(&format!("{}/items/{}?$select=webUrl", self.drive_url(), encode_id(id)))
            .await
        {
            Ok(v) => Ok(v.get("webUrl").and_then(|u| u.as_str()).map(str::to_string)),
            Err(e) if e.status() == Some(404) => Ok(None),
            Err(e) => Err(e),
        }
    }

    /// A file's earlier versions in OneDrive, newest first (the current one
    /// included, first). Read-only.
    pub async fn versions(&self, id: &str) -> Result<Vec<crate::model::FileVersion>> {
        let mut url = format!("{}/items/{}/versions", self.drive_url(), encode_id(id));
        let mut out = Vec::new();
        loop {
            let v: serde_json::Value = self.get(&url).await?;
            out.extend(
                v.get("value")
                    .and_then(|v| v.as_array())
                    .into_iter()
                    .flatten()
                    .filter_map(crate::model::FileVersion::from_json),
            );
            match v.get("@odata.nextLink").and_then(|n| n.as_str()) {
                Some(n) => url = n.to_string(),
                None => return Ok(out),
            }
        }
    }

    /// Streams version `version` of file `id` into `out`. Its content is
    /// served from a pre-authenticated address OneDrive redirects to; the
    /// token is sent only to Graph, never there.
    pub async fn download_version(
        &self,
        id: &str,
        version: &str,
        out: &mut (dyn std::io::Write + Send),
    ) -> Result<u64> {
        let url = format!("{}/items/{}/versions/{}/content", self.drive_url(), encode_id(id), encode_id(version));
        let http =
            reqwest::Client::builder().redirect(reqwest::redirect::Policy::none()).timeout(TRANSFER_TIMEOUT).build()?;
        let _permit = self.limiter.acquire().await;
        let resp = http.get(&url).bearer_auth(self.auth.access_token().await?).send().await?;
        let status = resp.status().as_u16();
        let to = match status {
            301 | 302 | 303 | 307 | 308 => resp
                .headers()
                .get(reqwest::header::LOCATION)
                .and_then(|l| l.to_str().ok())
                .map(str::to_string)
                .ok_or_else(|| Error::Status { status, what: "version download".into(), body: "no address".into() })?,
            _ => return Err(Error::Status { status, what: "version download".into(), body: String::new() }),
        };
        drop(_permit);
        self.download_to(&to, out).await
    }

    /// Graph's `restoreVersion`, with `If-Match: etag`. **OneDrive Personal
    /// ignores the `If-Match`** (measured): OneUpDrive restores by
    /// downloading the version and uploading it conditionally instead. Kept
    /// for `oneup dev-remote restore-check`, which measures it.
    pub async fn restore_version(&self, id: &str, version: &str, etag: &str) -> Result<()> {
        let url =
            format!("{}/items/{}/versions/{}/restoreVersion", self.drive_url(), encode_id(id), encode_id(version));
        let req = Req { if_match: Some(etag), ..Req::new(reqwest::Method::POST, &url) };
        self.send(&req).await.map(|_| ())
    }

    /// A sharing link for item `id`: anyone with it can view it (`edit`:
    /// also change it). Returns the link.
    pub async fn share_link(&self, id: &str, edit: bool) -> Result<String> {
        let url = format!("{}/items/{}/createLink", self.drive_url(), encode_id(id));
        let body = serde_json::json!({ "type": if edit { "edit" } else { "view" }, "scope": "anonymous" });
        let req = Req { json: Some(&body), ..Req::new(reqwest::Method::POST, &url) };
        let v: serde_json::Value = self.json(&req).await?;
        v.pointer("/link/webUrl").and_then(|u| u.as_str()).map(str::to_string).ok_or_else(|| Error::Status {
            status: 200,
            what: "sharing link".into(),
            body: "no link".into(),
        })
    }

    /// The folders other people shared with the signed-in account
    /// (`sharedWithMe`; needs the `Files.Read.All` or `Files.ReadWrite.All`
    /// permission). Files shared on their own are left out: only folders can
    /// be synced as a pair.
    pub async fn shared_folders(&self) -> Result<Vec<crate::model::SharedFolder>> {
        let mut url = format!("{GRAPH}/me/drive/sharedWithMe");
        let mut out = Vec::new();
        loop {
            let v: serde_json::Value = self.get(&url).await?;
            for it in v.get("value").and_then(|v| v.as_array()).into_iter().flatten() {
                if let Some(f) = crate::model::SharedFolder::from_json(it) {
                    out.push(f);
                }
            }
            match v.get("@odata.nextLink").and_then(|n| n.as_str()) {
                Some(n) => url = n.to_string(),
                None => return Ok(out),
            }
        }
    }

    /// First page URL for a folder's direct children.
    pub fn children_url(&self, folder_id: &str) -> String {
        format!("{}/items/{}/children?$top={PAGE_SIZE}&$select={ITEM_SELECT}", self.drive_url(), encode_id(folder_id))
    }

    /// First page URL for a folder-scoped delta, addressed by id.
    pub fn delta_by_id_url(&self, folder_id: &str) -> String {
        format!("{}/items/{}/delta?$top={PAGE_SIZE}&$select={ITEM_SELECT}", self.drive_url(), encode_id(folder_id))
    }

    /// First page URL for a folder-scoped delta, addressed by path
    /// (the form verified against OneDrive Personal).
    pub fn delta_by_path_url(&self, path: &str) -> String {
        format!("{}:/{}:/delta?$top={PAGE_SIZE}&$select={ITEM_SELECT}", self.top_url(), encode_drive_path(path))
    }

    /// The drive's root, or the scope's root folder, for path addressing.
    fn top_url(&self) -> String {
        match &self.scope {
            Some(id) => format!("{}/items/{}", self.drive_url(), encode_id(id)),
            None => format!("{}/root", self.drive_url()),
        }
    }

    /// A deltaLink for "now" without enumerating anything.
    pub async fn latest_cursor(&self) -> Result<String> {
        let page = self.page(&format!("{}/delta?token=latest", self.top_url())).await?;
        page.delta_link.ok_or_else(|| Error::Auth("token=latest returned no deltaLink".into()))
    }
}

/// An integer header value (e.g. `Retry-After: 30`, `RateLimit-Remaining: 12`).
fn header_u64(h: &reqwest::header::HeaderMap, name: &str) -> Option<u64> {
    h.get(name)?.to_str().ok()?.trim().parse().ok()
}

fn backoff(attempt: u32) -> Duration {
    Duration::from_millis(500u64.saturating_mul(1 << attempt.min(6)))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn path_encoding_normalises_separators() {
        assert_eq!(encode_drive_path("/Trade/Trader/.venv"), "Trade/Trader/.venv");
        assert_eq!(encode_drive_path(r"\Trade\Trader"), "Trade/Trader");
        assert_eq!(encode_drive_path("/a b/#c%"), "a%20b/%23c%25");
        assert_eq!(encode_drive_path("/naïve"), "na%C3%AFve");
    }

    fn graph() -> Graph {
        let store = crate::DpapiFileStore::new(std::env::temp_dir().join("oneup-graph-test-none.dpapi"));
        let auth = Arc::new(Auth::new(reqwest::Client::new(), "x".into(), "s", Box::new(store)));
        Graph::new(reqwest::Client::new(), auth, crate::Aimd::new(1, 1))
    }

    #[test]
    fn a_scoped_client_addresses_one_folder_as_its_root() {
        let g = graph();
        assert_eq!(g.delta_by_path_url("/a").split('?').next(), Some(&*format!("{GRAPH}/me/drive/root:/a:/delta")));
        let g = graph().scoped(Some("abc!1"), "ABC!7");
        assert!(g.is_other_drive());
        assert_eq!(
            g.delta_by_path_url("/a b").split('?').next(),
            Some(&*format!("{GRAPH}/drives/abc%211/items/ABC%217:/a%20b:/delta"))
        );
        assert!(g.children_url("X!2").starts_with(&format!("{GRAPH}/drives/abc%211/items/X%212/children")));
        assert_eq!(g.batch_get("X!2").url, "/drives/abc%211/items/X%212");
        let item = |id: &str| DriveItem {
            id: id.into(),
            name: Some("n".into()),
            size: None,
            e_tag: None,
            parent_reference: Some(crate::model::ParentReference { id: Some("P".into()), path: None }),
            file: None,
            folder: Some(crate::model::FolderFacet::default()),
            package: None,
            deleted: None,
            root: None,
            file_system_info: None,
        };
        let r = g.rooted(item("ABC!7"));
        assert!(r.is_root() && r.parent_id().is_none(), "the scope's folder stands in for the root");
        let other = g.rooted(item("ABC!8"));
        assert!(!other.is_root() && other.parent_id() == Some("P"));
    }

    #[test]
    fn backoff_is_bounded() {
        assert!(backoff(1) < backoff(3));
        assert_eq!(backoff(6), backoff(60));
    }
}
