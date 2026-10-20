//! Throttling-aware Graph requests and the drive endpoints the mirror uses.

use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering::Relaxed};
use std::time::Duration;

use serde::de::DeserializeOwned;

use crate::model::{DriveItem, ITEM_SELECT, Page};
use crate::{Aimd, Auth, Error, Result};

pub const GRAPH: &str = "https://graph.microsoft.com/v1.0";
/// What OneUpDrive's HTTP clients call themselves.
pub const USER_AGENT: &str = concat!("OneUpDrive/", env!("CARGO_PKG_VERSION"));
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
    /// Request bodies sent, in bytes: every attempt of each, file contents
    /// and JSON alike (what the data budget counts going up).
    pub sent: AtomicU64,
    /// Requests without a body to send that got an answer, and the time
    /// they took until it began (milliseconds, in all): the round trip
    /// plus the service's own time, for judging the link.
    pub timed: AtomicU64,
    pub timed_ms: AtomicU64,
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
    /// Where Graph is: [`GRAPH`], or a loopback server in tests.
    base: String,
    pub stats: NetStats,
    /// Bandwidth caps for file content (`rules.md` § User controls).
    pub upload: crate::throttle::Throttle,
    pub download: crate::throttle::Throttle,
    /// What background transfers may still move on the network's data
    /// budget, taken from as bytes go ([`crate::budget`]).
    pub budget: crate::budget::Budget,
    /// How long transfers of contents were under way, side by side
    /// counted once.
    pub transferring: crate::throttle::Active,
    /// How long a download may receive nothing before it's abandoned.
    stall: Duration,
    /// For [`Graph::download_version`], which must see the redirect.
    no_redirect: std::sync::OnceLock<reqwest::Client>,
    /// The largest upload fragment to send, in bytes (0: as the sizer
    /// chooses); set lower on a poor link.
    fragment_cap: std::sync::atomic::AtomicUsize,
}

/// The settings every OneUpDrive HTTP client starts from: a bounded
/// connect, a total timeout for metadata requests (transfers set their own)
/// and TCP keepalive, so a connection that died without closing (sleep and
/// resume, a NAT dropping it) fails instead of hanging. There's deliberately
/// no client-wide read timeout: reqwest's also bounds the wait for the
/// response headers, which for an upload includes sending its whole body, so
/// it would fail slow-but-moving uploads. Downloads detect a stall per read
/// instead ([`Graph::download_to`]).
pub fn http_builder() -> reqwest::ClientBuilder {
    reqwest::Client::builder()
        .user_agent(USER_AGENT)
        .connect_timeout(Duration::from_secs(20))
        .timeout(Duration::from_secs(120))
        // reqwest's default, stated: probes every 15 s once idle for 15 s.
        .tcp_keepalive(Duration::from_secs(15))
        .tcp_keepalive_interval(Duration::from_secs(15))
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

/// How a search is sent ([`Graph::search`]).
fn search_req(url: &str) -> Req<'_> {
    Req { label: Some("OneDrive search"), private: true, retry: false, ..Req::new(reqwest::Method::GET, url) }
}

/// The URL of a search below `top` (the drive's root or a folder): the
/// text as an OData string (`'` doubled), percent-encoded whole.
fn search_url(top: &str, text: &str, n: u32) -> String {
    let quoted = format!("'{}'", text.replace('\'', "''"));
    let q: String = quoted
        .bytes()
        .map(|b| match b {
            b'A'..=b'Z' | b'a'..=b'z' | b'0'..=b'9' | b'-' | b'.' | b'_' | b'~' => (b as char).to_string(),
            _ => format!("%{b:02X}"),
        })
        .collect();
    format!("{top}/search(q={q})?$top={n}&$select={ITEM_SELECT}")
}

/// Percent-encodes an ID as exactly one path segment: a `/` in it (a
/// permission's ID can have one) is encoded too, never a separator.
fn encode_segment(id: &str) -> String {
    id.bytes()
        .map(|b| match b {
            b'A'..=b'Z' | b'a'..=b'z' | b'0'..=b'9' | b'-' | b'.' | b'_' | b'~' => (b as char).to_string(),
            _ => format!("%{b:02X}"),
        })
        .collect()
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
    /// session token and must never be logged). With a label, network
    /// errors are logged and returned without their URL too.
    label: Option<&'static str>,
    /// The URL holds what the user typed (a search): besides the URL, an
    /// error leaves out the reply's body, which may repeat it.
    private: bool,
    /// Sent again after a network error or a server error. Off for an
    /// upload fragment: its upload session owns those retries, asking the
    /// session where it is first, and counts them per fragment. Throttling
    /// (429, 503) is waited out either way, and isn't a retry.
    retry: bool,
    /// The body is a file's contents: each attempt waits for the upload
    /// cap and must fit in the data budget right before it goes.
    content: bool,
    /// Don't follow a redirect: a 3xx answer is the result (its
    /// `Location` in the measure), for a caller that fetches it itself.
    no_redirect: bool,
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
            private: false,
            retry: true,
            content: false,
            no_redirect: false,
        }
    }

    /// `e` as it may be logged or returned: without the URL when the
    /// request is labelled.
    fn plain(&self, e: reqwest::Error) -> reqwest::Error {
        if self.label.is_some() { e.without_url() } else { e }
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
/// How long a download may go without receiving a byte. Each read restarts
/// it, so a slow line that keeps moving is never cut off.
const STALL_TIMEOUT: Duration = Duration::from_secs(60);

/// What [`Graph::upload_fragment`] reports.
#[derive(Debug)]
pub struct Fragment {
    /// The uploaded item, once the last fragment completed it.
    pub item: Option<DriveItem>,
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
    /// Waiting for the upload cap (file contents).
    capped: Duration,
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

    /// Refused for touching too many items at once ([`crate::too_many_items`]).
    pub fn is_too_many_items(&self) -> bool {
        crate::too_many_items(&self.body)
    }

    pub fn error(&self, what: &str) -> Error {
        Error::refused(self.status, what, self.body.to_string())
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
        // The full item: OneDrive Personal leaves the link out of a
        // `$select`ed answer (measured: every time), which then cost a
        // second request per download.
        let url = format!("{}/items/{}", self.drive_url(), encode_id(id));
        match self.get::<crate::model::DownloadInfo>(&url).await {
            Ok(it) => Ok(Some(it)),
            Err(e) if e.status() == Some(404) => Ok(None),
            Err(e) => Err(e),
        }
    }

    /// [`Graph::download_info`] for up to [`BATCH_MAX`] items in one round
    /// trip, in the order given; each `None` if it no longer exists.
    pub async fn download_infos(&self, ids: &[&str]) -> Result<Vec<Result<Option<crate::model::DownloadInfo>>>> {
        let reqs: Vec<BatchReq> = ids.iter().map(|id| self.batch_get(id)).collect();
        let answers = self.batch(&reqs).await?;
        Ok(answers
            .into_iter()
            .map(|a| match a.status {
                404 => Ok(None),
                200..=299 => Ok(Some(serde_json::from_value(a.body)?)),
                _ => Err(a.error("reading a download link")),
            })
            .collect())
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
        self.download_within(url, out, true).await
    }

    /// [`Graph::download_to`]; `budgeted`: on the data budget, which stops
    /// it once used up ([`crate::budget`]). Not for a file the user opens.
    /// Every byte received counts as it arrives, also of a download that
    /// fails later.
    pub async fn download_within(
        &self,
        url: &str,
        out: &mut (dyn std::io::Write + Send),
        budgeted: bool,
    ) -> Result<u64> {
        if budgeted && self.budget.left() == Some(0) {
            return Err(Error::OverBudget);
        }
        // A file the user opens doesn't wait for the limiter: background
        // transfers hold its permits, and near the data budget's limit
        // they may hold them while they wait their turn to read
        // ([`crate::budget`]). The user's own action comes first.
        // It still waits out any pause the service asked for (429/503 with
        // `Retry-After`): never hammer the service, whoever asks.
        let _permit = if budgeted {
            Some(self.limiter.acquire().await)
        } else {
            self.limiter.unpaused().await;
            None
        };
        if budgeted && self.budget.left() == Some(0) {
            return Err(Error::OverBudget);
        }
        let _busy = self.transferring.begin();
        self.stats.requests.fetch_add(1, Relaxed);
        // A dead connection holds the limiter's permit while it hangs, so
        // waiting for the answer and for each read is bounded.
        let stalled = |_| Error::Stalled("download", self.stall.as_secs());
        let sent = self.http.get(url).timeout(TRANSFER_TIMEOUT).send();
        let mut resp = tokio::time::timeout(self.stall, sent).await.map_err(stalled)??;
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
        let expected = resp.content_length();
        // The budget is used up: the download is complete only if nothing
        // more comes (it was the last chunk), else it stops.
        let mut over = false;
        loop {
            // All of it arrived: done, even with nothing left to spend.
            if expected.is_some_and(|len| n >= len) {
                break;
            }
            if over && expected.is_some() {
                return Err(Error::OverBudget);
            }
            // Near the limit, budgeted downloads read one at a time, and
            // only while something is left ([`crate::budget`]). With
            // nothing left and no length told, one more read tells whether
            // it was all: the end of the body, or one read too many.
            let near = match budgeted && !over {
                false => None,
                true => match self.budget.before_read().await {
                    Ok(g) => g,
                    Err(_) if expected.is_none() => {
                        over = true;
                        None
                    }
                    Err(e) => return Err(e),
                },
            };
            let Some(chunk) = tokio::time::timeout(self.stall, resp.chunk()).await.map_err(stalled)?? else {
                break;
            };
            // Counted as it arrives, before anything else is done with it:
            // a write that fails used the traffic too.
            self.stats.bytes.fetch_add(chunk.len() as u64, Relaxed);
            let fit = self.budget.received(chunk.len() as u64);
            drop(near);
            out.write_all(&chunk).map_err(Error::Local)?;
            n += chunk.len() as u64;
            if budgeted && (over || !fit) {
                if over && expected.is_none() {
                    // More came after the budget was used up.
                    return Err(Error::OverBudget);
                }
                over = true;
            }
            // Not reading meanwhile slows the sender down (TCP flow control).
            self.download.take(chunk.len() as u64).await;
        }
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
        let _busy = self.transferring.begin();
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
            content: true,
            ..Req::new(reqwest::Method::PUT, &url)
        };
        self.json(&req).await
    }

    /// Starts an upload session (see [`Graph::upload_small`] for the
    /// conditions) and returns its pre-authenticated upload URL. Graph
    /// checks `If-Match` here only, not when the last fragment commits the
    /// upload: a replacing upload re-checks the eTag itself before then.
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
    /// The item comes back once the last fragment completes it. Sent once:
    /// after a network or server error the caller asks the session where
    /// it is ([`Graph::upload_status`]) and goes on from there. Throttling
    /// is still waited out here.
    pub async fn upload_fragment(&self, upload_url: &str, start: u64, total: u64, bytes: &[u8]) -> Result<Fragment> {
        let _busy = self.transferring.begin();
        let end = start + bytes.len() as u64 - 1;
        let req = Req {
            auth: false,
            bytes: Some(bytes),
            content_range: Some(format!("bytes {start}-{end}/{total}")),
            timeout: Some(TRANSFER_TIMEOUT),
            label: Some("upload session fragment"),
            retry: false,
            content: true,
            ..Req::new(reqwest::Method::PUT, upload_url)
        };
        let (status, body, m) = self.send_measured(&req).await?;
        let item = if status == 202 { None } else { Some(serde_json::from_slice(&body)?) };
        Ok(Fragment { item, paused: m.paused, capped: m.capped })
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
                let retry = status == 429 || retried(status);
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

    /// Where an upload session stands: the first byte it still expects.
    /// `None` if it takes no more (it expired, was cancelled or completed):
    /// Graph answers 404 for a session that is gone.
    pub async fn upload_status(&self, upload_url: &str) -> Result<Option<u64>> {
        let req = Req { auth: false, label: Some("upload session"), ..Req::new(reqwest::Method::GET, upload_url) };
        match self.send(&req).await {
            Ok((_, body)) => Ok(next_expected(&serde_json::from_slice(&body)?)),
            Err(e) if e.status() == Some(404) => Ok(None),
            Err(e) => Err(e),
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

    /// Removes permission `perm` (a sharing link, or people's access) from
    /// item `id`, if the item's eTag is still `etag` (412 otherwise; Graph
    /// documents `If-Match` for this request). Only a permission that isn't
    /// inherited can be removed. Never touches the item's content. Only for
    /// `oneup_effects::sharing`, behind the gate's token.
    pub async fn delete_permission(&self, id: &str, perm: &str, etag: &str) -> Result<()> {
        let url = format!("{}/items/{}/permissions/{}", self.drive_url(), encode_segment(id), encode_segment(perm));
        let req = Req { if_match: Some(etag), ..Req::new(reqwest::Method::DELETE, &url) };
        self.send(&req).await.map(|_| ())
    }

    /// Sets when permission `perm` of item `id` stops working, if the
    /// item's eTag is still `etag`. **Graph documents only `roles` as
    /// changeable this way**: OneDrive may ignore the date, so the caller
    /// checks the permission it returns. Only for `oneup_effects::sharing`,
    /// behind the gate's token.
    pub async fn set_permission_expiry(
        &self,
        id: &str,
        perm: &str,
        etag: &str,
        expires_ms: i64,
    ) -> Result<crate::model::Permission> {
        let when = time::OffsetDateTime::from_unix_timestamp_nanos(i128::from(expires_ms) * 1_000_000)
            .ok()
            .and_then(|t| t.format(&time::format_description::well_known::Rfc3339).ok())
            .ok_or(Error::Invalid("the expiry date is out of range"))?;
        let url = format!("{}/items/{}/permissions/{}", self.drive_url(), encode_segment(id), encode_segment(perm));
        let body = serde_json::json!({ "expirationDateTime": when });
        let req = Req { json: Some(&body), if_match: Some(etag), ..Req::new(reqwest::Method::PATCH, &url) };
        let v: serde_json::Value = self.json(&req).await?;
        crate::model::Permission::from_json(&v)
            .ok_or_else(|| Error::refused(200, "changing a permission", "no permission in the answer".into()))
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
            budget: crate::budget::Budget::default(),
            transferring: crate::throttle::Active::default(),
            stall: STALL_TIMEOUT,
            no_redirect: std::sync::OnceLock::new(),
            drive: "/me/drive".into(),
            scope: None,
            base: GRAPH.into(),
            fragment_cap: std::sync::atomic::AtomicUsize::new(0),
        }
    }

    /// Caps upload fragments at `bytes` (`None`: no cap), so a dropped
    /// connection on a poor link costs less to send again.
    pub fn set_fragment_cap(&self, bytes: Option<usize>) {
        self.fragment_cap.store(bytes.unwrap_or(0), Relaxed);
    }

    pub fn fragment_cap(&self) -> Option<usize> {
        Some(self.fragment_cap.load(Relaxed)).filter(|n| *n > 0)
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
        format!("{}{}", self.base, self.drive)
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
        // What each attempt sends (the data budget counts every attempt).
        let body_len = match (req.json, req.bytes) {
            (Some(j), _) => serde_json::to_vec(j).map_or(0, |v| v.len()) as u64,
            (None, Some(b)) => b.len() as u64,
            (None, None) => 0,
        };
        loop {
            attempt += 1;
            if attempt > MAX_ATTEMPTS || throttled > MAX_THROTTLES {
                return Err(Error::Exhausted(attempt));
            }
            let http = if req.no_redirect { self.no_redirect_client()? } else { &self.http };
            let mut rb = http.request(req.method.clone(), url);
            let mut token = None;
            if req.auth {
                match self.auth.access_token().await {
                    Ok(t) => rb = rb.bearer_auth(token.insert(t)),
                    // The token endpoint unreachable, busy or failing isn't a
                    // refused sign-in: try again like a failed request.
                    Err(e) if e.is_transient() => {
                        self.stats.retries.fetch_add(1, Relaxed);
                        let wait = backoff(attempt).max(self.auth.retry_after());
                        tracing::warn!(error = %e, attempt, wait_s = wait.as_secs(), "token refresh failed; retrying");
                        tokio::time::sleep(wait).await;
                        continue;
                    }
                    Err(e) => return Err(e),
                }
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
            // File contents: each attempt keeps to the upload cap as it is
            // then (a wait follows a change of cap).
            if req.content {
                measure.capped += self.upload.take(body_len).await;
            }
            let waiting = std::time::Instant::now();
            let permit = self.limiter.acquire().await;
            measure.paused += waiting.elapsed();
            // ...and must fit in the data budget as it is right before it
            // goes, which a change of network may have set meanwhile.
            if req.content && !self.budget.send(body_len) {
                return Err(Error::OverBudget);
            }
            self.stats.sent.fetch_add(body_len, Relaxed);
            measure.attempts += 1;
            self.stats.requests.fetch_add(1, Relaxed);
            let asked = std::time::Instant::now();
            let sent = rb.send().await;
            if sent.is_ok() && req.bytes.is_none() {
                self.stats.timed.fetch_add(1, Relaxed);
                self.stats.timed_ms.fetch_add(u64::try_from(asked.elapsed().as_millis()).unwrap_or(u64::MAX), Relaxed);
            }
            let resp = match sent.map_err(|e| req.plain(e)) {
                Ok(r) => r,
                Err(e) if !req.retry => return Err(e.into()),
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
            if status == 401
                && let Some(t) = &token
                && attempt <= 2
            {
                drop(permit);
                self.auth.invalidate(t).await;
                continue;
            }
            if retried(status) && req.retry {
                drop(permit);
                self.stats.retries.fetch_add(1, Relaxed);
                tokio::time::sleep(backoff(attempt)).await;
                continue;
            }
            // Counted chunk by chunk as they arrive: a body cut off midway
            // used that traffic too.
            // A redirect asked not to be followed is an answer too.
            let answered = (200..300).contains(&status) || (req.no_redirect && (300..400).contains(&status));
            let body = if answered {
                self.read_counted(resp).await
            } else {
                crate::error_body_counted(resp, Some(&self.stats.bytes)).await
            };
            let body = match body.map_err(|e| req.plain(e)) {
                Ok(b) => b,
                Err(e) if !req.retry => return Err(e.into()),
                Err(e) => {
                    drop(permit);
                    self.stats.retries.fetch_add(1, Relaxed);
                    tracing::warn!(error = %e, attempt, "body read failed; retrying");
                    tokio::time::sleep(backoff(attempt)).await;
                    continue;
                }
            };
            drop(permit);
            measure.location = location;
            if !answered {
                let what = match req.label {
                    Some(l) => l.to_string(),
                    None => url.split('?').next().unwrap_or(url).to_string(),
                };
                let body = if req.private { String::new() } else { String::from_utf8_lossy(&body).into_owned() };
                return Err(Error::refused(status, what, body));
            }
            self.limiter.on_success();
            return Ok((status, body, measure));
        }
    }

    /// A response's body, each chunk counted in `NetStats::bytes` as it
    /// arrives.
    async fn read_counted(&self, mut resp: reqwest::Response) -> reqwest::Result<Vec<u8>> {
        let mut body = Vec::new();
        while let Some(chunk) = resp.chunk().await? {
            self.stats.bytes.fetch_add(chunk.len() as u64, Relaxed);
            body.extend_from_slice(&chunk);
        }
        Ok(body)
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

    /// The room left in the drive requests address, in bytes (`None`: not
    /// told, as for a drive someone else owns). One small request.
    pub async fn remaining(&self) -> Result<Option<i64>> {
        let v: serde_json::Value = self.get(&format!("{}?$select=quota", self.drive_url())).await?;
        Ok(v.pointer("/quota/remaining").and_then(serde_json::Value::as_i64))
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
        match self.get::<serde_json::Value>(&self.web_url_request(id)).await {
            Ok(v) => Ok(v.get("webUrl").and_then(|u| u.as_str()).map(str::to_string)),
            Err(e) if e.status() == Some(404) => Ok(None),
            Err(e) => Err(e),
        }
    }

    /// Where [`Graph::web_url`] asks: the item in the client's drive (another
    /// person's, for a scoped client of a shared folder).
    fn web_url_request(&self, id: &str) -> String {
        format!("{}/items/{}?$select=webUrl", self.drive_url(), encode_id(id))
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
        // Only the redirect is read here; the content comes through
        // `download_to`, with its transfer timeout and stall detection.
        let to = self.redirect_of(&url, true).await?;
        self.download_to(&to, out).await
    }

    /// Where `url` redirects to, asked without following it, through the
    /// same throttling, retries and counting as every request (a 429 or
    /// 503 pauses everyone for its `Retry-After`).
    async fn redirect_of(&self, url: &str, auth: bool) -> Result<String> {
        let req =
            Req { auth, no_redirect: true, label: Some("version download"), ..Req::new(reqwest::Method::GET, url) };
        let (status, _, m) = self.send_measured(&req).await?;
        match status {
            301 | 302 | 303 | 307 | 308 => m.location.ok_or_else(|| Error::Status {
                status,
                what: "version download".into(),
                body: "no address".into(),
            }),
            _ => Err(Error::Status { status, what: "version download".into(), body: String::new() }),
        }
    }

    /// The client that doesn't follow redirects.
    fn no_redirect_client(&self) -> Result<&reqwest::Client> {
        if let Some(c) = self.no_redirect.get() {
            return Ok(c);
        }
        let c = http_builder().redirect(reqwest::redirect::Policy::none()).build()?;
        Ok(self.no_redirect.get_or_init(|| c))
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

    /// OneDrive's own search (`search(q=…)`, documented for OneDrive
    /// Personal): items whose name, metadata or **content** match `text`,
    /// as OneDrive ranks them, up to `top` (one page). Read-only, but the
    /// text is sent to Microsoft: only on the user's say-so.
    ///
    /// The text never reaches a log or an error: they name the request
    /// "OneDrive search", without its URL or OneDrive's reply. Not retried
    /// (a network error is logged with its URL otherwise); the person can
    /// search again.
    pub async fn search(&self, text: &str, top: u32) -> Result<Vec<DriveItem>> {
        let url = search_url(&self.top_url(), text, top);
        self.search_at(&url).await
    }

    async fn search_at(&self, url: &str) -> Result<Vec<DriveItem>> {
        let (_, body) = self.send(&search_req(url)).await?;
        let page: Page = serde_json::from_slice(&body)?;
        Ok(page.value.into_iter().filter(|it| !it.is_deleted()).map(|it| self.rooted(it)).collect())
    }

    /// The sharing of items `ids`, in the order given: each one's eTag and
    /// all its permissions (links and people, inherited ones too), or
    /// `None` if it no longer exists. Read-only, for the sharing audit on
    /// demand: ten items per round trip (two batched requests each).
    pub async fn sharing(&self, ids: &[&str]) -> Result<Vec<Result<Option<crate::model::ItemSharing>>>> {
        let mut out = Vec::with_capacity(ids.len());
        for chunk in ids.chunks(BATCH_MAX / 2) {
            let reqs: Vec<BatchReq> = chunk
                .iter()
                .flat_map(|id| {
                    let item = format!("{}/items/{}", self.drive, encode_segment(id));
                    [
                        BatchReq {
                            method: reqwest::Method::GET,
                            url: format!("{item}?$select=id,eTag,root"),
                            if_match: None,
                            body: None,
                        },
                        BatchReq {
                            method: reqwest::Method::GET,
                            url: format!("{item}/permissions"),
                            if_match: None,
                            body: None,
                        },
                    ]
                })
                .collect();
            let answers = self.batch(&reqs).await?;
            for pair in answers.chunks(2) {
                let [item, perms] = pair else { continue };
                out.push(self.item_sharing(item, perms).await);
            }
        }
        Ok(out)
    }

    async fn item_sharing(&self, item: &BatchResp, perms: &BatchResp) -> Result<Option<crate::model::ItemSharing>> {
        // Only the item's own 404 says it's gone. Its permissions not found
        // while the item is there isn't an answer to act on (a check that
        // a permission is gone must know the item still exists).
        if item.status == 404 {
            return Ok(None);
        }
        let it = item.item()?;
        if !(200..300).contains(&perms.status) {
            return Err(perms.error("reading who an item is shared with"));
        }
        let mut permissions = Vec::new();
        let mut page = perms.body.clone();
        loop {
            permissions.extend(
                page.get("value")
                    .and_then(|v| v.as_array())
                    .into_iter()
                    .flatten()
                    .filter_map(crate::model::Permission::from_json),
            );
            match page.get("@odata.nextLink").and_then(|n| n.as_str()) {
                Some(n) => page = self.get(n).await?,
                None => break,
            }
        }
        let etag = it.e_tag.clone().ok_or_else(|| item.error("an item without an eTag"))?;
        Ok(Some(crate::model::ItemSharing { etag, root: it.is_root(), permissions }))
    }

    /// Everything other people shared with the signed-in account, files and
    /// folders (`sharedWithMe`; see [`Graph::shared_folders`] for the
    /// permission it needs). Microsoft deprecated the request: it works in a
    /// degraded state until November 2026 and then returns nothing. At most
    /// `limit` items; the flag says whether there were more.
    pub async fn shared_with_me(&self, limit: usize) -> Result<(Vec<crate::model::SharedItem>, bool)> {
        let mut url = format!("{GRAPH}/me/drive/sharedWithMe");
        let mut out = Vec::new();
        loop {
            let v: serde_json::Value = self.get(&url).await?;
            out.extend(
                v.get("value")
                    .and_then(|v| v.as_array())
                    .into_iter()
                    .flatten()
                    .filter_map(crate::model::SharedItem::from_json),
            );
            let next = v.get("@odata.nextLink").and_then(|n| n.as_str());
            if out.len() >= limit {
                let more = out.len() > limit || next.is_some();
                out.truncate(limit);
                return Ok((out, more));
            }
            match next {
                Some(n) => url = n.to_string(),
                None => return Ok((out, false)),
            }
        }
    }

    /// First page URL of a listing of the whole drive with only what tells
    /// shared items apart (`oneup shared --scan`, for items shared before
    /// the mirror kept the `shared` facet).
    pub fn shared_scan_url(&self) -> String {
        format!("{}/delta?$top={PAGE_SIZE}&$select=id,shared,deleted", self.top_url())
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

/// The first byte an upload session's status still expects: the start of
/// its first `nextExpectedRanges` entry (`"start-end"` or `"start-"`).
/// `None` if it expects nothing more.
fn next_expected(status: &serde_json::Value) -> Option<u64> {
    let ranges = status.get("nextExpectedRanges")?.as_array()?;
    ranges.iter().filter_map(|r| r.as_str()?.split('-').next()?.trim().parse().ok()).min()
}

/// An integer header value (e.g. `Retry-After: 30`, `RateLimit-Remaining: 12`).
fn header_u64(h: &reqwest::header::HeaderMap, name: &str) -> Option<u64> {
    h.get(name)?.to_str().ok()?.trim().parse().ok()
}

/// Server errors worth sending again. Not 507: the drive has no room
/// left, which another try a moment later won't change, and retried, it
/// reached the caller only as "gave up", without its code.
fn retried(status: u16) -> bool {
    (500..600).contains(&status) && status != 507
}

fn backoff(attempt: u32) -> Duration {
    Duration::from_millis(500u64.saturating_mul(1 << attempt.min(6)))
}

#[cfg(test)]
mod tests {
    use super::*;

    /// What's logged, for a test to read back.
    #[derive(Clone, Default)]
    struct Logged(Arc<std::sync::Mutex<Vec<u8>>>);

    impl std::io::Write for Logged {
        fn write(&mut self, b: &[u8]) -> std::io::Result<usize> {
            self.0.lock().unwrap_or_else(|e| e.into_inner()).extend_from_slice(b);
            Ok(b.len())
        }
        fn flush(&mut self) -> std::io::Result<()> {
            Ok(())
        }
    }

    impl<'a> tracing_subscriber::fmt::MakeWriter<'a> for Logged {
        type Writer = Logged;
        fn make_writer(&'a self) -> Logged {
            self.clone()
        }
    }

    /// What someone searches for is sent to OneDrive and nowhere else: a
    /// failed search's error and the log name the request, not its URL,
    /// and leave out a reply that repeats the text.
    #[tokio::test]
    #[allow(clippy::unwrap_used)]
    async fn search_text_never_reaches_errors_or_logs() {
        use std::io::{Read, Write};
        let logged = Logged::default();
        let sub = tracing_subscriber::fmt().with_max_level(tracing::Level::TRACE).with_writer(logged.clone()).finish();
        let _log = tracing::subscriber::set_default(sub);
        let url = |port: u16| format!("http://127.0.0.1:{port}/me/drive/root/search(q='salary%20review')");
        let g = graph();
        // Nobody listening: a network error.
        let closed = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
        let port = closed.local_addr().unwrap().port();
        drop(closed);
        let u = url(port);
        let refused = g.send(&Req { auth: false, ..search_req(&u) }).await.unwrap_err();
        // OneDrive refusing it, its reply repeating the text.
        let server = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
        let port = server.local_addr().unwrap().port();
        let answer = std::thread::spawn(move || {
            let (mut c, _) = server.accept().unwrap();
            let mut buf = [0u8; 4096];
            let _ = c.read(&mut buf).unwrap();
            let body = r#"{"error":{"code":"invalidRequest","message":"bad query: salary review"}}"#;
            write!(c, "HTTP/1.1 400 Bad Request\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}", body.len())
                .unwrap();
        });
        let u = url(port);
        let bad = g.send(&Req { auth: false, ..search_req(&u) }).await.unwrap_err();
        answer.join().unwrap();
        assert!(matches!(&bad, Error::Status { status: 400, what, .. } if what == "OneDrive search"), "{bad:?}");
        let log = String::from_utf8(logged.0.lock().unwrap().clone()).unwrap();
        for text in [format!("{refused} {refused:?}"), format!("{bad} {bad:?}"), log] {
            assert!(!text.contains("salary"), "{text}");
        }
    }

    /// A pair's client searches its own folder, in the drive that holds it
    /// (another person's for a shared folder), and asks that drive for an
    /// item's web page.
    #[test]
    fn a_scoped_client_searches_and_opens_inside_its_folder() {
        let g = graph();
        assert!(search_url(&g.top_url(), "tax", 5).starts_with(&format!("{GRAPH}/me/drive/root/search(q=")));
        assert_eq!(g.web_url_request("A!1"), format!("{GRAPH}/me/drive/items/A%211?$select=webUrl"));
        let g = graph().scoped(Some("abc!1"), "ABC!7");
        assert!(
            search_url(&g.top_url(), "tax", 5).starts_with(&format!("{GRAPH}/drives/abc%211/items/ABC%217/search(q=")),
            "{}",
            search_url(&g.top_url(), "tax", 5)
        );
        assert_eq!(g.web_url_request("B!2"), format!("{GRAPH}/drives/abc%211/items/B%212?$select=webUrl"));
        let own = graph().scoped(None, "F!3");
        assert!(search_url(&own.top_url(), "tax", 5).starts_with(&format!("{GRAPH}/me/drive/items/F%213/search(q=")));
    }

    #[test]
    fn a_search_sends_the_text_as_one_quoted_string() {
        let url = search_url("https://g/me/drive/root", "Bob's tax 2023/€", 50);
        assert_eq!(
            url.split('?').next(),
            Some("https://g/me/drive/root/search(q=%27Bob%27%27s%20tax%202023%2F%E2%82%AC%27)")
        );
        assert!(url.contains("$top=50&$select="), "{url}");
    }

    #[test]
    fn path_encoding_normalises_separators() {
        assert_eq!(encode_drive_path("/Trade/Trader/.venv"), "Trade/Trader/.venv");
        assert_eq!(encode_drive_path(r"\Trade\Trader"), "Trade/Trader");
        assert_eq!(encode_drive_path("/a b/#c%"), "a%20b/%23c%25");
        assert_eq!(encode_drive_path("/naïve"), "na%C3%AFve");
        // An ID is one segment, whatever it holds.
        assert_eq!(encode_segment("aTo/w+I="), "aTo%2Fw%2BI%3D");
        assert_eq!(encode_segment("ABC!12"), "ABC%2112");
    }

    fn graph() -> Graph {
        graph_with(1)
    }

    /// A client for loopback servers, with `n` requests at once.
    fn graph_with(n: usize) -> Graph {
        let store = crate::DpapiFileStore::new(std::env::temp_dir().join("oneup-graph-test-none.dpapi"));
        let auth = Arc::new(Auth::new(reqwest::Client::new(), "x".into(), "s", Box::new(store)));
        Graph::new(reqwest::Client::new(), auth, crate::Aimd::new(n, n))
    }

    /// A loopback server sending `body` bytes of each `size`-byte download,
    /// then dropping the connection (all of it: a complete download).
    async fn serve_body(size: usize, body: usize) -> String {
        use tokio::io::{AsyncReadExt as _, AsyncWriteExt as _};
        let l = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let url = format!("http://{}/f", l.local_addr().unwrap());
        tokio::spawn(async move {
            loop {
                let Ok((mut s, _)) = l.accept().await else { return };
                tokio::spawn(async move {
                    let mut buf = vec![0u8; 16 * 1024];
                    let _ = s.read(&mut buf).await;
                    let head = format!("HTTP/1.1 200 OK\r\ncontent-length: {size}\r\n\r\n");
                    let _ = s.write_all(head.as_bytes()).await;
                    for c in vec![7u8; body].chunks(64 * 1024) {
                        if s.write_all(c).await.is_err() {
                            return;
                        }
                    }
                    let _ = s.flush().await;
                });
            }
        });
        url
    }

    /// A one-connection server answering with `head` then `body` (written
    /// in the pieces given, a moment apart), then closing.
    async fn serve_raw(head: String, pieces: Vec<Vec<u8>>) -> String {
        use tokio::io::{AsyncReadExt as _, AsyncWriteExt as _};
        let l = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let url = format!("http://{}/f", l.local_addr().unwrap());
        tokio::spawn(async move {
            let (mut s, _) = l.accept().await.unwrap();
            let mut buf = vec![0u8; 16 * 1024];
            let _ = s.read(&mut buf).await;
            s.write_all(head.as_bytes()).await.unwrap();
            for p in pieces {
                s.write_all(&p).await.unwrap();
                s.flush().await.unwrap();
                tokio::time::sleep(Duration::from_millis(50)).await;
            }
            let _ = s.shutdown().await;
        });
        url
    }

    /// The last chunk of a download that goes past the budget completes it
    /// (received and counted anyway, so not thrown away); a body of no told
    /// length that uses the budget up exactly is complete too, the end of
    /// it told by one more read.
    #[tokio::test]
    async fn a_download_completed_by_its_last_chunk_is_kept() {
        let head = |len: Option<usize>| match len {
            Some(n) => format!("HTTP/1.1 200 OK\r\ncontent-length: {n}\r\n\r\n"),
            None => "HTTP/1.1 200 OK\r\nconnection: close\r\n\r\n".to_string(),
        };
        let url = serve_raw(head(Some(250_100)), vec![vec![1u8; 250_000], vec![2u8; 100]]).await;
        let g = graph();
        g.budget.set(Some(250_050));
        let mut out = Vec::new();
        let r = tokio::time::timeout(Duration::from_secs(10), g.download_to(&url, &mut out)).await.unwrap();
        assert_eq!(r.unwrap(), 250_100, "complete, not refused");
        assert_eq!(g.budget.left(), Some(0));

        let url = serve_raw(head(None), vec![vec![1u8; 100_000]]).await;
        let g = graph();
        g.budget.set(Some(100_000));
        let r = tokio::time::timeout(Duration::from_secs(10), g.download_to(&url, &mut Vec::new())).await.unwrap();
        assert_eq!(r.unwrap(), 100_000, "an exact fit of no told length");

        // ...but one of no told length that goes on is stopped.
        let url = serve_raw(head(None), vec![vec![1u8; 100_000], vec![2u8; 100_000]]).await;
        let g = graph();
        g.budget.set(Some(100_000));
        let r = tokio::time::timeout(Duration::from_secs(10), g.download_to(&url, &mut Vec::new())).await.unwrap();
        assert!(matches!(r, Err(Error::OverBudget)), "{r:?}");
    }

    /// The redirect to a version's content is asked through the shared
    /// throttling: a 429 pauses everyone for its `Retry-After` and is asked
    /// again, then the address comes back unfollowed.
    #[tokio::test]
    async fn a_versions_redirect_honours_the_services_pause() {
        let throttled =
            "HTTP/1.1 429 Too Many Requests\r\nRetry-After: 1\r\nContent-Length: 0\r\nConnection: close\r\n\r\n";
        let moved = "HTTP/1.1 302 Found\r\nLocation: http://127.0.0.1:9/content\r\nContent-Length: 0\r\nConnection: close\r\n\r\n";
        let (url, count) = serve_each(vec![throttled, moved]).await;
        let g = graph();
        let started = std::time::Instant::now();
        let to = tokio::time::timeout(Duration::from_secs(10), g.redirect_of(&url, false)).await.unwrap().unwrap();
        assert_eq!(to, "http://127.0.0.1:9/content");
        assert_eq!(count.load(Relaxed), 2, "asked again after the pause");
        assert_eq!(g.stats.throttled.load(Relaxed), 1, "the shared limiter paused");
        assert!(started.elapsed() >= Duration::from_millis(900), "{:?}", started.elapsed());
    }

    /// A file the user opens skips the limit on requests at once, but
    /// still waits out a pause the service asked for (`Retry-After`).
    #[tokio::test]
    async fn a_file_the_user_opens_still_waits_out_the_services_pause() {
        let g = graph();
        let _held = g.limiter().acquire().await;
        let url = serve_body(1000, 1000).await;
        g.limiter().on_throttle(Duration::from_millis(800));
        let started = std::time::Instant::now();
        let opened =
            tokio::time::timeout(Duration::from_secs(5), g.download_within(&url, &mut Vec::new(), false)).await;
        assert_eq!(opened.expect("not held up by the permits").unwrap(), 1000);
        assert!(started.elapsed() >= Duration::from_millis(700), "waited {:?}", started.elapsed());
    }

    /// With every request permit held by background transfers, a file the
    /// user opens still downloads; a background one waits its turn.
    #[tokio::test]
    async fn a_file_the_user_opens_doesnt_wait_behind_background_transfers() {
        let g = graph();
        let _held = g.limiter().acquire().await;
        let url = serve_body(1000, 1000).await;
        let opened =
            tokio::time::timeout(Duration::from_secs(5), g.download_within(&url, &mut Vec::new(), false)).await;
        assert_eq!(opened.expect("not held up").unwrap(), 1000);
        let background = tokio::time::timeout(Duration::from_millis(300), g.download_to(&url, &mut Vec::new())).await;
        assert!(background.is_err(), "waits for a permit");
    }

    /// A response cut off midway counts what arrived; the retry counts
    /// again.
    #[tokio::test]
    async fn a_body_cut_off_midway_counts_what_arrived() {
        let partial = "HTTP/1.1 200 OK\r\nContent-Length: 100\r\nConnection: close\r\n\r\n0123456789012345678901234567890123456789";
        let whole =
            "HTTP/1.1 200 OK\r\nContent-Length: 29\r\nConnection: close\r\n\r\n{\"nextExpectedRanges\":[\"5-\"]}";
        let (url, count) = serve_each(vec![partial, whole]).await;
        let g = graph();
        let next = tokio::time::timeout(Duration::from_secs(30), g.upload_status(&url)).await.unwrap().unwrap();
        assert_eq!(next, Some(5));
        assert_eq!(count.load(Relaxed), 2, "read again");
        assert_eq!(g.stats.bytes.load(Relaxed), 40 + 29, "the cut-off part counts");
    }

    /// Downloads that use up exactly what's left are complete, not
    /// refused; one with chunks still to come once the budget is used up is.
    #[tokio::test]
    async fn a_download_that_fits_the_budget_exactly_completes() {
        const SIZE: usize = 300_000;
        let url = serve_body(SIZE, SIZE).await;
        let g = graph();
        g.budget.set(Some(SIZE as u64));
        let mut out = Vec::new();
        let n = tokio::time::timeout(Duration::from_secs(10), g.download_to(&url, &mut out)).await.unwrap();
        assert_eq!(n.unwrap(), SIZE as u64);
        assert_eq!(g.budget.left(), Some(0));
        g.budget.set(Some(SIZE as u64 / 4));
        let r = tokio::time::timeout(Duration::from_secs(10), g.download_to(&url, &mut Vec::new())).await.unwrap();
        assert!(matches!(r, Err(Error::OverBudget)), "{r:?}");
    }

    /// What arrives counts even when it can't be written (a full disk):
    /// the traffic was used.
    #[tokio::test]
    async fn bytes_received_count_even_when_writing_them_fails() {
        struct Full;
        impl std::io::Write for Full {
            fn write(&mut self, _: &[u8]) -> std::io::Result<usize> {
                Err(std::io::Error::other("disk full"))
            }
            fn flush(&mut self) -> std::io::Result<()> {
                Ok(())
            }
        }
        let url = serve_body(200_000, 200_000).await;
        let g = graph();
        g.budget.set(Some(10_000_000));
        let r = tokio::time::timeout(Duration::from_secs(10), g.download_to(&url, &mut Full)).await.unwrap();
        assert!(matches!(r, Err(Error::Local(_))), "{r:?}");
        let got = g.stats.bytes.load(Relaxed);
        assert!(got > 0, "counted");
        assert_eq!(g.budget.left(), Some(10_000_000 - got), "and taken from the budget");
    }

    /// Eight downloads side by side, each dropping at 90%, tried again and
    /// again: together they pass a 3 MB budget by one read at most, not
    /// one read each.
    #[tokio::test(flavor = "multi_thread", worker_threads = 4)]
    async fn downloads_side_by_side_pass_the_budget_by_one_read_at_most() {
        const SIZE: usize = 1_000_000;
        const BUDGET: u64 = 3_000_000;
        let url = serve_body(SIZE, SIZE * 9 / 10).await;
        let g = Arc::new(graph_with(8));
        g.budget.set(Some(BUDGET));
        let tasks: Vec<_> = (0..8)
            .map(|_| {
                let (g, url) = (Arc::clone(&g), url.clone());
                tokio::spawn(async move {
                    for _ in 0..20 {
                        let r = tokio::time::timeout(Duration::from_secs(20), g.download_to(&url, &mut Vec::new()))
                            .await
                            .unwrap();
                        if matches!(r, Err(Error::OverBudget)) {
                            return true;
                        }
                    }
                    false
                })
            })
            .collect();
        for t in tasks {
            assert!(t.await.unwrap(), "each stopped on the budget");
        }
        let got = g.stats.bytes.load(Relaxed);
        assert!(got <= BUDGET + crate::budget::READ_MAX, "{got} bytes on a {BUDGET} budget");
    }

    /// An upload body counts and is checked against the budget at every
    /// attempt, right before it goes: a throttled fragment sent twice
    /// counts twice; a switch to a network whose budget is used up, while
    /// the fragment waits for the cap, stops it before anything is sent.
    #[tokio::test]
    async fn each_attempt_of_an_upload_counts_and_is_checked_as_it_goes() {
        let throttled =
            "HTTP/1.1 429 Too Many Requests\r\nRetry-After: 0\r\nContent-Length: 0\r\nConnection: close\r\n\r\n";
        let accepted = "HTTP/1.1 202 Accepted\r\nContent-Length: 2\r\nConnection: close\r\n\r\n{}";
        let (url, count) = serve_each(vec![throttled, accepted]).await;
        let g = graph();
        g.budget.set(Some(100));
        g.upload_fragment(&url, 0, 10, b"0123456789").await.unwrap();
        assert_eq!(count.load(Relaxed), 2);
        assert_eq!(g.stats.sent.load(Relaxed), 20, "both sends");
        assert_eq!(g.budget.left(), Some(80));

        // Waiting for a 1 KB/s cap; meanwhile another network, no cap, and
        // its budget used up.
        let (url, count) = serve_each(vec![accepted]).await;
        let g = Arc::new(graph());
        g.upload.set(1000);
        g.budget.set(Some(1_000_000));
        let g2 = Arc::clone(&g);
        let up = tokio::spawn(async move { g2.upload_fragment(&url, 0, 2000, &[0u8; 2000]).await });
        tokio::time::sleep(Duration::from_millis(200)).await;
        g.budget.set(Some(0));
        g.upload.set(0);
        let r = tokio::time::timeout(Duration::from_secs(1), up).await.expect("the new cap applied at once").unwrap();
        assert!(matches!(r, Err(Error::OverBudget)), "{r:?}");
        assert_eq!(count.load(Relaxed), 0, "nothing sent");
        assert_eq!(g.stats.sent.load(Relaxed), 0);
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
            shared: None,
        };
        let r = g.rooted(item("ABC!7"));
        assert!(r.is_root() && r.parent_id().is_none(), "the scope's folder stands in for the root");
        let other = g.rooted(item("ABC!8"));
        assert!(!other.is_root() && other.parent_id() == Some("P"));
    }

    /// A one-connection HTTP server on loopback: answers the request with
    /// `head`, then sends each of `drips` `gap` apart, then holds the
    /// connection open without sending more (a connection gone dead).
    async fn serve(head: Vec<u8>, drips: Vec<&'static [u8]>, gap: Duration) -> String {
        use tokio::io::{AsyncReadExt as _, AsyncWriteExt as _};
        let l = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let url = format!("http://{}/f", l.local_addr().unwrap());
        tokio::spawn(async move {
            let (mut s, _) = l.accept().await.unwrap();
            let mut buf = vec![0u8; 16 * 1024];
            let _ = s.read(&mut buf).await;
            s.write_all(&head).await.unwrap();
            for d in drips {
                tokio::time::sleep(gap).await;
                s.write_all(d).await.unwrap();
            }
            tokio::time::sleep(Duration::from_secs(3600)).await;
        });
        url
    }

    /// The roadmap's flaky-link budget test: a 1 MB download that drops at
    /// 90% every time (as rounds try it again) and an upload: what they
    /// use stays within a 3 MB budget (over by one read at most), the
    /// download stops once it's used up and nothing more is sent, while a
    /// file the user opens still comes.
    #[tokio::test]
    async fn repeated_drops_stay_within_the_data_budget() {
        use tokio::io::{AsyncReadExt as _, AsyncWriteExt as _};
        const SIZE: usize = 1_000_000;
        let l = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let url = format!("http://{}/f", l.local_addr().unwrap());
        let served = Arc::new(AtomicU64::new(0));
        let s2 = Arc::clone(&served);
        tokio::spawn(async move {
            loop {
                let Ok((mut s, _)) = l.accept().await else { return };
                let mut buf = vec![0u8; 16 * 1024];
                let _ = s.read(&mut buf).await;
                let head = format!("HTTP/1.1 200 OK\r\ncontent-length: {SIZE}\r\n\r\n");
                let _ = s.write_all(head.as_bytes()).await;
                let body = vec![7u8; SIZE * 9 / 10];
                for c in body.chunks(16 * 1024) {
                    if s.write_all(c).await.is_err() {
                        break;
                    }
                    s2.fetch_add(c.len() as u64, Relaxed);
                }
                // The connection drops.
                drop(s);
            }
        });
        let g = graph();
        const BUDGET: u64 = 3_000_000;
        g.budget.set(Some(BUDGET));
        let mut tries = 0;
        let stopped = loop {
            tries += 1;
            assert!(tries < 20, "never stopped");
            let mut out = Vec::new();
            match tokio::time::timeout(Duration::from_secs(10), g.download_to(&url, &mut out)).await.unwrap() {
                Err(Error::OverBudget) => break tries,
                Err(_) => {}
                Ok(_) => panic!("the server never sends it whole"),
            }
        };
        let got = g.stats.bytes.load(Relaxed);
        // Over by what one read brings at most.
        assert!(got <= BUDGET + 512 * 1024, "{got} bytes received on a {BUDGET} budget");
        assert!(got >= BUDGET - (SIZE as u64), "it used the budget before stopping: {got}");
        assert!(stopped >= 4, "stopped on try {stopped}");
        // Used up: nothing more goes out, and no request is made.
        let before = g.stats.requests.load(Relaxed);
        assert!(matches!(g.download_to(&url, &mut Vec::new()).await, Err(Error::OverBudget)));
        assert!(matches!(g.upload_fragment("http://127.0.0.1:9/u", 0, 10, &[0u8; 10]).await, Err(Error::OverBudget)));
        assert_eq!(g.stats.requests.load(Relaxed), before);
        // A file the user opens comes anyway (and counts).
        let mut out = Vec::new();
        let opened = tokio::time::timeout(Duration::from_secs(10), g.download_within(&url, &mut out, false)).await;
        assert!(!matches!(opened.unwrap(), Err(Error::OverBudget)));
        assert!(out.len() >= SIZE / 2);
    }

    /// A loopback HTTP server answering each request (one per connection,
    /// read whole first) with the next of `answers`, the last one again
    /// after that; returns its URL and how many requests it got.
    async fn serve_each(answers: Vec<&'static str>) -> (String, Arc<AtomicU64>) {
        use tokio::io::{AsyncReadExt as _, AsyncWriteExt as _};
        let l = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let url = format!("http://{}/up", l.local_addr().unwrap());
        let count = Arc::new(AtomicU64::new(0));
        let n = Arc::clone(&count);
        tokio::spawn(async move {
            loop {
                let (mut s, _) = l.accept().await.unwrap();
                let mut req = Vec::new();
                let mut buf = [0u8; 4096];
                loop {
                    let k = s.read(&mut buf).await.unwrap_or(0);
                    if k == 0 {
                        break;
                    }
                    req.extend_from_slice(&buf[..k]);
                    if let Some(end) = req.windows(4).position(|w| w == b"\r\n\r\n") {
                        let head = String::from_utf8_lossy(&req[..end]).to_ascii_lowercase();
                        let len = head
                            .lines()
                            .find_map(|l| l.strip_prefix("content-length:"))
                            .and_then(|v| v.trim().parse::<usize>().ok())
                            .unwrap_or(0);
                        if req.len() >= end + 4 + len {
                            break;
                        }
                    }
                }
                let i = n.fetch_add(1, Relaxed) as usize;
                let _ = s.write_all(answers[i.min(answers.len() - 1)].as_bytes()).await;
                let _ = s.shutdown().await;
            }
        });
        (url, count)
    }

    /// The client sends an upload fragment once: after a server or network
    /// error its upload session decides what to send again (asking the
    /// session first), within one budget per fragment. Throttling is still
    /// waited out here, and isn't a send of its own for that budget.
    #[tokio::test]
    async fn an_upload_fragment_is_sent_once_and_throttling_waited_out() {
        let fragment = |url: String| async move {
            let g = graph();
            let r = tokio::time::timeout(Duration::from_secs(20), g.upload_fragment(&url, 0, 10, b"0123456789")).await;
            r.expect("not retried with back-off")
        };
        let failed = "HTTP/1.1 500 Internal Server Error\r\nContent-Length: 0\r\nConnection: close\r\n\r\n";
        let (url, count) = serve_each(vec![failed]).await;
        let r = fragment(url).await;
        assert!(matches!(r, Err(Error::Status { status: 500, .. })), "{r:?}");
        assert_eq!(count.load(Relaxed), 1);

        // The connection closed without an answer.
        let (url, count) = serve_each(vec![""]).await;
        let r = fragment(url).await;
        assert!(r.as_ref().is_err_and(Error::is_transient), "{r:?}");
        assert_eq!(count.load(Relaxed), 1);

        let throttled =
            "HTTP/1.1 429 Too Many Requests\r\nRetry-After: 0\r\nContent-Length: 0\r\nConnection: close\r\n\r\n";
        let accepted = "HTTP/1.1 202 Accepted\r\nContent-Length: 2\r\nConnection: close\r\n\r\n{}";
        let (url, count) = serve_each(vec![throttled, accepted]).await;
        let f = fragment(url).await.unwrap();
        assert!(f.item.is_none());
        assert_eq!(count.load(Relaxed), 2, "the throttled send was waited out and sent again");
    }

    #[tokio::test]
    async fn a_download_that_stops_receiving_fails_instead_of_hanging() {
        let mut g = graph();
        // Long enough for a loopback request to be answered while the
        // crate's other tests run in parallel (300 ms wasn't).
        g.stall = Duration::from_millis(1500);
        let url = serve(b"HTTP/1.1 200 OK\r\nContent-Length: 100\r\n\r\nabc".to_vec(), vec![], Duration::ZERO).await;
        let mut out = Vec::new();
        let r = tokio::time::timeout(Duration::from_secs(10), g.download_to(&url, &mut out)).await;
        assert!(matches!(r, Ok(Err(Error::Stalled(..)))), "{r:?}");
        assert_eq!(out, b"abc");
    }

    #[tokio::test]
    async fn a_slow_download_that_keeps_moving_completes() {
        let mut g = graph();
        g.stall = Duration::from_millis(1500);
        // Six bytes, 500 ms apart: twice the stall limit in total.
        let head = b"HTTP/1.1 200 OK\r\nContent-Length: 6\r\n\r\n".to_vec();
        let url = serve(head, vec![b"x"; 6], Duration::from_millis(500)).await;
        let mut out = Vec::new();
        let n = tokio::time::timeout(Duration::from_secs(10), g.download_to(&url, &mut out)).await.unwrap().unwrap();
        assert_eq!((n, out.as_slice()), (6, b"xxxxxx".as_slice()));
    }

    #[tokio::test]
    async fn a_huge_refusal_body_is_cut_before_it_becomes_an_error() {
        let page = "<p>".repeat(100_000);
        let head = format!("HTTP/1.1 403 Forbidden\r\nContent-Length: {}\r\n\r\n{page}", page.len()).into_bytes();
        let url = serve(head, vec![], Duration::ZERO).await;
        let req = Req { auth: false, ..Req::new(reqwest::Method::GET, &url) };
        match graph().send(&req).await {
            Err(Error::Status { status: 403, body, .. }) => {
                assert!(body.len() <= crate::MAX_ERROR_BODY + 16, "kept {} bytes", body.len());
            }
            other => panic!("{other:?}"),
        }
    }

    #[tokio::test]
    async fn a_refusal_body_is_not_read_past_what_the_error_keeps() {
        // Declares 50 MB but sends only the start, then nothing: reading the
        // whole body would never finish.
        let head = b"HTTP/1.1 403 Forbidden
Content-Length: 50000000

"
        .to_vec();
        let url = serve(head, vec![&[b'x'; 16 * 1024]], Duration::ZERO).await;
        let req = Req { auth: false, ..Req::new(reqwest::Method::GET, &url) };
        match tokio::time::timeout(Duration::from_secs(10), graph().send(&req)).await {
            Ok(Err(Error::Status { status: 403, body, .. })) => {
                assert!(body.starts_with("xxxx") && body.ends_with("(cut)"), "{body}");
            }
            other => panic!("{other:?}"),
        }
    }

    #[tokio::test]
    async fn a_full_drive_is_answered_at_once_with_its_code() {
        let body = r#"{"error":{"code":"quotaLimitReached","message":"Insufficient Space Available"}}"#;
        let head = format!("HTTP/1.1 507 Insufficient Storage\r\nContent-Length: {}\r\n\r\n{body}", body.len());
        // One connection only: a second try would wait for good.
        let url = serve(head.into_bytes(), vec![], Duration::ZERO).await;
        let req = Req { auth: false, ..Req::new(reqwest::Method::PUT, &url) };
        match tokio::time::timeout(Duration::from_secs(10), graph().send(&req)).await {
            Ok(Err(e @ Error::Status { status: 507, .. })) => {
                assert_eq!(e.code().as_deref(), Some("quotaLimitReached"))
            }
            other => panic!("{other:?}"),
        }
    }

    /// A loopback server answering each request (one per connection, read
    /// whole first) with the next of `answers`, the last one again after
    /// that, and keeping each request as text (request line, headers, body).
    async fn serve_capture(answers: Vec<String>) -> (String, Arc<std::sync::Mutex<Vec<String>>>) {
        use tokio::io::{AsyncReadExt as _, AsyncWriteExt as _};
        let l = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let base = format!("http://{}", l.local_addr().unwrap());
        let got = Arc::new(std::sync::Mutex::new(Vec::new()));
        let kept = Arc::clone(&got);
        tokio::spawn(async move {
            loop {
                let (mut s, _) = l.accept().await.unwrap();
                let mut req = Vec::new();
                let mut buf = [0u8; 4096];
                loop {
                    let k = s.read(&mut buf).await.unwrap_or(0);
                    if k == 0 {
                        break;
                    }
                    req.extend_from_slice(&buf[..k]);
                    if let Some(end) = req.windows(4).position(|w| w == b"\r\n\r\n") {
                        let head = String::from_utf8_lossy(&req[..end]).to_ascii_lowercase();
                        let len = head
                            .lines()
                            .find_map(|l| l.strip_prefix("content-length:"))
                            .and_then(|v| v.trim().parse::<usize>().ok())
                            .unwrap_or(0);
                        if req.len() >= end + 4 + len {
                            break;
                        }
                    }
                }
                let i = {
                    let mut g = kept.lock().unwrap();
                    g.push(String::from_utf8_lossy(&req).into_owned());
                    g.len() - 1
                };
                let _ = s.write_all(answers[i.min(answers.len() - 1)].as_bytes()).await;
                let _ = s.shutdown().await;
            }
        });
        (base, got)
    }

    /// A client signed in (access token `tok`) talking to `base`.
    fn signed_in(base: &str) -> Graph {
        let auth = Arc::new(Auth::with_token("tok"));
        Graph { base: base.into(), ..Graph::new(reqwest::Client::new(), auth, crate::Aimd::new(1, 1)) }
    }

    fn answer(status: &str, body: &str) -> String {
        format!(
            "HTTP/1.1 {status}\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}",
            body.len()
        )
    }

    /// How a batched pair of answers (the item, its permissions) is read:
    /// only the item's own 404 says it's gone.
    #[tokio::test]
    async fn only_the_items_own_not_found_says_it_is_gone() {
        let g = graph();
        let resp = |status: u16, body: serde_json::Value| BatchResp { status, body };
        let item = resp(200, serde_json::json!({ "id": "I!1", "eTag": "\"{E},3\"" }));
        let perms =
            resp(200, serde_json::json!({ "value": [{ "id": "p1", "roles": ["read"], "link": { "type": "view" } }] }));
        let not_found = || resp(404, serde_json::json!({ "error": { "code": "itemNotFound" } }));

        // Both there.
        let s = g.item_sharing(&item, &perms).await.unwrap().unwrap();
        assert_eq!((s.etag.as_str(), s.root, s.permissions.len()), ("\"{E},3\"", false, 1));
        assert_eq!(s.permissions[0].id, "p1");
        // The item gone (its permissions with it).
        assert_eq!(g.item_sharing(&not_found(), &not_found()).await.unwrap(), None);
        assert_eq!(g.item_sharing(&not_found(), &perms).await.unwrap(), None);
        // The item there, its permissions not found: no answer to act on.
        let e = g.item_sharing(&item, &not_found()).await.unwrap_err();
        assert_eq!(e.status(), Some(404));
        // An item without an eTag can't be acted on either.
        let e = g.item_sharing(&resp(200, serde_json::json!({ "id": "I!1" })), &perms).await.unwrap_err();
        assert!(e.status().is_some(), "{e}");
    }

    /// Removing a permission is a DELETE of exactly that permission,
    /// conditional on the item's eTag (`If-Match`), with the token.
    #[tokio::test]
    async fn a_permission_is_removed_only_under_the_items_etag() {
        let (base, got) = serve_capture(vec![answer("204 No Content", "")]).await;
        signed_in(&base).delete_permission("I!1", "p/1", "\"{E},3\"").await.unwrap();
        let reqs = got.lock().unwrap().clone();
        assert_eq!(reqs.len(), 1);
        let r = reqs[0].to_ascii_lowercase();
        assert!(r.starts_with("delete /me/drive/items/i%211/permissions/p%2f1 http/1.1\r\n"), "{r}");
        assert!(r.contains("\r\nif-match: \"{e},3\"\r\n"), "{r}");
        assert!(r.contains("\r\nauthorization: bearer tok\r\n"), "{r}");
    }

    /// An eTag that moved on: 412, refused at once, never sent again.
    #[tokio::test]
    async fn a_removal_under_a_moved_etag_is_refused_and_not_retried() {
        let body = r#"{"error":{"code":"resourceModified","message":"x"}}"#;
        let (base, got) = serve_capture(vec![answer("412 Precondition Failed", body)]).await;
        let e = signed_in(&base).delete_permission("I!1", "p1", "\"{E},3\"").await.unwrap_err();
        assert_eq!((e.status(), e.code().as_deref()), (Some(412), Some("resourceModified")));
        assert_eq!(got.lock().unwrap().len(), 1);
        let (base, got) = serve_capture(vec![answer("412 Precondition Failed", body)]).await;
        let e = signed_in(&base).set_permission_expiry("I!1", "p1", "\"{E},3\"", 0).await.unwrap_err();
        assert_eq!(e.status(), Some(412));
        assert_eq!(got.lock().unwrap().len(), 1);
    }

    /// Setting an expiry is a PATCH of that permission's
    /// `expirationDateTime`, under the item's eTag; OneDrive's answer comes
    /// back as the permission it now has.
    #[tokio::test]
    async fn an_expiry_is_patched_under_the_items_etag() {
        let when = "2030-01-31T23:59:59Z";
        let ms = time::OffsetDateTime::parse(when, &time::format_description::well_known::Rfc3339)
            .map(|t| i64::try_from(t.unix_timestamp_nanos() / 1_000_000).unwrap())
            .unwrap();
        let reply = format!(r#"{{"id":"p1","roles":["read"],"expirationDateTime":"{when}"}}"#);
        let (base, got) = serve_capture(vec![answer("200 OK", &reply)]).await;
        let p = signed_in(&base).set_permission_expiry("I!1", "p1", "\"{E},3\"", ms).await.unwrap();
        assert_eq!((p.id.as_str(), p.expires_ms), ("p1", Some(ms)));
        let reqs = got.lock().unwrap().clone();
        assert_eq!(reqs.len(), 1);
        let r = &reqs[0];
        assert!(r.to_ascii_lowercase().starts_with("patch /me/drive/items/i%211/permissions/p1 http/1.1\r\n"), "{r}");
        assert!(r.to_ascii_lowercase().contains("\r\nif-match: \"{e},3\"\r\n"), "{r}");
        let body = &r[r.find("\r\n\r\n").unwrap() + 4..];
        let v: serde_json::Value = serde_json::from_str(body).unwrap();
        assert_eq!(v, serde_json::json!({ "expirationDateTime": when }));
    }

    #[test]
    fn an_upload_sessions_status_says_where_to_go_on() {
        let s = |v: serde_json::Value| next_expected(&v);
        assert_eq!(s(serde_json::json!({ "nextExpectedRanges": ["655360-"] })), Some(655_360));
        assert_eq!(s(serde_json::json!({ "nextExpectedRanges": ["12345-55232", "77829-99375"] })), Some(12_345));
        assert_eq!(s(serde_json::json!({ "nextExpectedRanges": [] })), None);
        assert_eq!(s(serde_json::json!({ "expirationDateTime": "2026-10-16T00:00:00Z" })), None);
    }

    #[test]
    fn backoff_is_bounded() {
        assert!(backoff(1) < backoff(3));
        assert_eq!(backoff(6), backoff(60));
    }
}
