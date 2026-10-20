//! The messages: JSON-RPC 2.0, one JSON object per line.
//!
//! Every call answers with the agent's [`Status`] after it took effect, so a
//! client never needs a second round trip to show the result.

use serde::{Deserialize, Serialize};
use serde_json::Value;

/// Which direction of sync a pause or resume applies to.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Direction {
    /// Both: nothing syncs.
    Both,
    /// Changes to this PC (downloads, local moves, recycling).
    Local,
    /// Changes to OneDrive (uploads, cloud moves, deletions).
    Remote,
}

/// The user's answer to something the agent is waiting for, by the code it
/// showed.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Answer {
    /// Go ahead with a batch that tripped a safety stop.
    Approve,
    /// Keep a batch that doesn't fit in the version store anyway.
    Keep,
    /// Remove such a batch without a local copy.
    Discard,
    /// Remove the excluded items the agent offers to remove from OneDrive
    /// ([`Status::prune_offer`]).
    Prune,
    /// Keep them in OneDrive instead: the offer for these items is settled
    /// and not made again, also after a restart.
    KeepInCloud,
}

/// A call to the agent.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Call {
    Status,
    /// Pause `direction`; for `minutes`, or until resumed.
    Pause {
        direction: Direction,
        minutes: Option<u64>,
    },
    Resume {
        direction: Direction,
    },
    /// Answer the waiting batch with this code (16 hex digits).
    Answer {
        code: String,
        answer: Answer,
    },
    /// Check both sides for changes now.
    SyncNow,
    /// Check now, and transfer what's at `path` (relative to the synced
    /// folder) before anything else.
    SyncFirst {
        path: String,
    },
    /// Stop the agent after the current round.
    Stop,
    /// Allow `mb` more megabytes this month on the network's data budget
    /// (`NetStatus::data`); the agent goes on if it had paused for it.
    AllowData {
        mb: u64,
    },
    /// Wait until the status differs from revision `since` (or up to
    /// [`crate::WATCH_MAX`]), then return it: lets a client follow the
    /// agent without polling.
    Watch {
        since: u64,
    },
    /// The events after `since` (an [`Event::seq`]), waiting for one (up to
    /// [`crate::WATCH_MAX`]) if there are none yet; `None`: none, only where
    /// the feed is now. Answered with [`Events`], not a status
    /// ([`crate::Client::events`]).
    Events {
        since: Option<u64>,
    },
}

#[derive(Serialize, Deserialize)]
struct WatchParams {
    since: u64,
}

#[derive(Serialize, Deserialize)]
struct EventsParams {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    since: Option<u64>,
}

#[derive(Serialize, Deserialize)]
struct AllowParams {
    mb: u64,
}

#[derive(Serialize, Deserialize)]
struct PauseParams {
    direction: Direction,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    minutes: Option<u64>,
}

#[derive(Serialize, Deserialize)]
struct ResumeParams {
    direction: Direction,
}

#[derive(Serialize, Deserialize)]
struct PathParams {
    path: String,
}

#[derive(Serialize, Deserialize)]
struct AnswerParams {
    code: String,
    answer: Answer,
}

impl Call {
    pub fn method(&self) -> &'static str {
        match self {
            Call::Status => "status",
            Call::Pause { .. } => "pause",
            Call::Resume { .. } => "resume",
            Call::Answer { .. } => "answer",
            Call::SyncNow => "sync_now",
            Call::SyncFirst { .. } => "sync_first",
            Call::Stop => "stop",
            Call::Watch { .. } => "watch",
            Call::Events { .. } => "events",
            Call::AllowData { .. } => "allow_data",
        }
    }

    fn params(&self) -> Value {
        let v = match self {
            Call::Status | Call::SyncNow | Call::Stop => return Value::Null,
            Call::Pause { direction, minutes } => {
                serde_json::to_value(PauseParams { direction: *direction, minutes: *minutes })
            }
            Call::Resume { direction } => serde_json::to_value(ResumeParams { direction: *direction }),
            Call::Answer { code, answer } => serde_json::to_value(AnswerParams { code: code.clone(), answer: *answer }),
            Call::SyncFirst { path } => serde_json::to_value(PathParams { path: path.clone() }),
            Call::Watch { since } => serde_json::to_value(WatchParams { since: *since }),
            Call::Events { since } => serde_json::to_value(EventsParams { since: *since }),
            Call::AllowData { mb } => serde_json::to_value(AllowParams { mb: *mb }),
        };
        v.unwrap_or(Value::Null)
    }

    fn parse(method: &str, params: Value) -> Result<Call, RpcError> {
        fn p<T: serde::de::DeserializeOwned>(v: Value) -> Result<T, RpcError> {
            serde_json::from_value(v).map_err(|e| RpcError::new(INVALID_PARAMS, format!("invalid params: {e}")))
        }
        Ok(match method {
            "status" => Call::Status,
            "sync_now" => Call::SyncNow,
            "sync_first" => {
                let PathParams { path } = p(params)?;
                Call::SyncFirst { path }
            }
            "stop" => Call::Stop,
            "watch" => {
                let WatchParams { since } = p(params)?;
                Call::Watch { since }
            }
            "events" => {
                // No params at all reads as "from now".
                let EventsParams { since } = if params.is_null() { EventsParams { since: None } } else { p(params)? };
                Call::Events { since }
            }
            "allow_data" => {
                let AllowParams { mb } = p(params)?;
                Call::AllowData { mb }
            }
            "pause" => {
                let PauseParams { direction, minutes } = p(params)?;
                Call::Pause { direction, minutes }
            }
            "resume" => {
                let ResumeParams { direction } = p(params)?;
                Call::Resume { direction }
            }
            "answer" => {
                let AnswerParams { code, answer } = p(params)?;
                Call::Answer { code, answer }
            }
            other => return Err(RpcError::new(METHOD_NOT_FOUND, format!("no method {other:?}"))),
        })
    }
}

/// What the agent is doing.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Activity {
    /// Starting up (the first pass looks at every folder).
    Starting,
    /// Running rounds.
    Syncing,
    /// Up to date, waiting for changes.
    Idle,
    /// Both directions are paused.
    Paused,
    /// A safety stop is waiting for the user; nothing runs until then.
    WaitingForYou,
    /// Some items, or the connection, will be retried.
    Retrying,
    /// Stopping after the current round.
    Stopping,
}

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct PauseState {
    pub local: bool,
    pub remote: bool,
    /// When the pause ends by itself (Unix ms); `None`: until resumed.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub until_ms: Option<i64>,
}

/// How the agent's last sequence of rounds went.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct LastRun {
    pub at_ms: i64,
    pub done: usize,
    pub skipped: usize,
    pub failed: usize,
    pub held: usize,
}

/// A tripped safety stop.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct SafetyStop {
    /// `mass` (deletions, overwrites, moves) or `content` (files whose new
    /// content doesn't look like their type).
    #[serde(default)]
    pub kind: String,
    /// `local` (this PC) or `remote` (OneDrive).
    pub side: String,
    pub count: usize,
    /// `count` is only a lower bound (a folder of unknown size is on the
    /// list).
    #[serde(default)]
    pub at_least: bool,
    /// Of the limit, how much the last 15 minutes had used.
    pub recent: usize,
    pub synced_items: u64,
    /// The approval code.
    pub code: String,
}

impl SafetyStop {
    /// How many items, as the user is told: `12`, or `at least 12`.
    pub fn items(&self) -> String {
        if self.at_least { format!("at least {}", self.count) } else { self.count.to_string() }
    }
}

/// A version-store overflow batch.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Overflow {
    pub count: usize,
    pub bytes: u64,
    pub room: u64,
    pub code: String,
    /// Waiting for the user's `keep` or `discard`.
    pub waiting: bool,
}

/// OneDrive items the rules exclude, which the agent offers to bring here
/// (if they aren't all here yet) and then remove (to OneDrive's recycle bin;
/// the copies on this PC stay). Not with `cloud_copies = "prune"`, which
/// does that without asking.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct PruneOffer {
    /// Outermost items (a folder counts once).
    pub entries: usize,
    /// Items in all, as far as known (a lower bound).
    pub items: u64,
    pub bytes: u64,
    /// The code that accepts exactly these ([`Answer::Prune`]).
    pub code: String,
}

/// Items that can't sync until the user does something (a hard link, a
/// name OneDrive refuses, a file over its size limit). Trying again
/// doesn't help, so sync is up to date without them; `oneup unsynced`
/// lists them. Excluded items aren't counted: they're left out on purpose.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Unsynced {
    pub count: usize,
    /// There are more than `count` (the agent keeps a bounded list).
    #[serde(default)]
    pub more: bool,
}

/// How far the round being carried out is (published about once a second
/// while it runs). A sync can take several rounds; each is planned whole
/// before it runs, so the totals are the round's, not the whole sync's.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Progress {
    /// The round's number in this sequence of rounds, from 1.
    pub round: usize,
    pub items_done: u64,
    pub items_total: u64,
    /// Content sent or received, and what the round's transfers add up to
    /// (0 when nothing is transferred: online-only files, renames).
    pub bytes_done: u64,
    pub bytes_total: u64,
    /// How long the round has run, in ms.
    pub elapsed_ms: u64,
}

impl Progress {
    /// The time left, in ms, once enough has happened to tell: the slower
    /// of the item rate and the byte rate so far, as a mix of small and
    /// large files can make either one misleading alone.
    pub fn eta_ms(&self) -> Option<u64> {
        const SETTLE_MS: u64 = 10_000;
        if self.elapsed_ms < SETTLE_MS || self.items_done == 0 || self.items_done >= self.items_total {
            return None;
        }
        let t = self.elapsed_ms as f64;
        let items = t * (self.items_total - self.items_done) as f64 / self.items_done as f64;
        let bytes = match (self.bytes_done, self.bytes_total) {
            (done, total) if done > 0 && total > done => t * (total - done) as f64 / done as f64,
            _ => 0.0,
        };
        Some(items.max(bytes) as u64)
    }
}

/// A problem the agent ran into, told for people (`oneup_app::problem`
/// says which errors make which kind). Shown by the tray, the settings
/// window, Explorer's flyout and `oneup status`; saved to the data folder
/// when it stops the agent, so they can say why it isn't running.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Problem {
    /// What kind: `setup`, `offline`, `signin`, `cloud-full`, `disk-full`, `folder`,
    /// `records`, `onedrive` or `other`; also the user guide's anchor.
    pub kind: String,
    /// A few words, for a menu or a status line.
    pub title: String,
    /// What happened, and what OneUpDrive does about it.
    pub detail: String,
    /// What the user can do.
    pub action: String,
    /// The error as the program saw it, for a report. Never shown first.
    pub technical: String,
    /// When this kind of problem began (Unix ms).
    pub since_ms: i64,
    /// How many tries in a row have met it.
    pub attempts: u32,
    /// When the agent tries again (Unix ms); `None` once it stopped.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub retry_ms: Option<i64>,
}

/// The agent's state, as every call returns it.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Status {
    pub version: String,
    /// Goes up whenever anything else here changes (see [`Call::Watch`]).
    #[serde(default)]
    pub rev: u64,
    /// The synced folder on this PC, and the OneDrive folder.
    pub local: String,
    pub remote: String,
    pub activity: Activity,
    pub paused: PauseState,
    /// Whether OneDrive's push notifications are live.
    pub push: bool,
    /// When the agent last looked at both sides (Unix ms).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub checked_ms: Option<i64>,
    /// The last time it changed anything, and how that went.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub last: Option<LastRun>,
    #[serde(default)]
    pub safety_stops: Vec<SafetyStop>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub overflow: Option<Overflow>,
    /// The problem being retried, if any.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub problem: Option<Problem>,
    /// Why the agent paused by itself (a metered connection, battery
    /// saver, quiet hours), if it did: a fixed English message, the key
    /// readers translate it by (`oneup_app::autopause::Reason`).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub auto_pause: Option<String>,
    /// The quiet hours `auto_pause` names, as its `{hours}`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub auto_pause_hours: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub prune_offer: Option<PruneOffer>,
    /// The round being carried out, while one is.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub progress: Option<Progress>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub unsynced: Option<Unsynced>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub retrying: Option<Retrying>,
    /// Database files held back, or warned about
    /// (`docs/architecture/databases.md`): at most 100.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub databases: Vec<DatabaseNote>,
    /// How many more were held back than `databases` lists.
    #[serde(default, skip_serializing_if = "is_zero")]
    pub databases_more: u64,
    /// The network and what it lets sync do (`docs/user-guide/slow-and-metered-networks.md`).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub network: Option<NetStatus>,
}

fn is_zero(n: &u64) -> bool {
    *n == 0
}

/// A database file the agent holds back until it's in a state to sync, or
/// one that shouldn't sync while in use.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct DatabaseNote {
    /// Relative to the synced folder (`/Mail/archive.pst`).
    pub path: String,
    /// `sqlite`, `keepass`, `access`, `thunderbird`, `outlook`,
    /// `virtual-disk` or `other` (a `[databases]` pattern).
    pub format: String,
    /// Why it waits: `wal` (SQLite changes not yet in the file), `journal`
    /// (a SQLite transaction under way), `open` (open in another program),
    /// `beside` (a database's working file, or a file going into such a
    /// name: they never travel).
    /// `live`: a format that shouldn't sync while in use (Outlook data
    /// files, virtual disks), whether it waits or not.
    pub why: String,
    /// The programs that have it open, as Windows names them
    /// (`KeePass.exe`).
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub programs: Vec<String>,
    /// What waits: `up` (sending it), `down` (OneDrive's newer version).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub direction: Option<String>,
    /// All of it in a sentence, as the activity log says it.
    #[serde(default)]
    pub detail: String,
}

/// How the agent uses the network now.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct NetStatus {
    /// The network's name as Windows shows it (empty: not told).
    #[serde(default)]
    pub name: String,
    #[serde(default)]
    pub metered: bool,
    /// Which profile applies: `default`, `metered` or `own`.
    #[serde(default)]
    pub profile: String,
    /// What transfers, for people ("names and folders only").
    #[serde(default)]
    pub transfer: String,
    /// `internet`, `constrained` (a network to sign in to), `local`,
    /// `none` or `unknown`.
    #[serde(default)]
    pub connectivity: String,
    /// Sync every this many minutes; 0: within seconds of a change.
    #[serde(default)]
    pub interval_min: u32,
    /// When the next pass runs (Unix ms), with an interval.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub next_ms: Option<i64>,
    /// File contents waiting for a network that lets them through.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub waiting: Option<NetWaiting>,
    /// Data used this month on this network's profile.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub data: Option<DataUse>,
}

/// Data used this month on the profile in effect, by direction.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct DataUse {
    /// `YYYY-MM`.
    pub month: String,
    pub up: u64,
    pub down: u64,
    /// The monthly budget with what the user allowed on top, in bytes; 0:
    /// none.
    pub budget: u64,
    /// The budget is used up: sync pauses on this network.
    pub reached: bool,
}

/// What waits for another network (`oneup network waiting` lists it, the
/// largest first).
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct NetWaiting {
    pub count: u64,
    pub bytes: u64,
    /// Why, for people.
    pub why: String,
}

/// Items whose operations failed and wait before they're tried again (a
/// minute after the first failure, doubling to an hour; `oneup explain`
/// tells one item's).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Retrying {
    pub count: u64,
    /// When the first is tried again (Unix ms).
    pub next_ms: i64,
}

/// What happened, for `oneup watch`, hooks and scripts
/// (`docs/architecture/hooks.md`).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum EventKind {
    /// An item was uploaded, downloaded, made, moved, renamed or removed.
    Synced,
    /// Changed on both sides: both versions were kept.
    Conflict,
    /// An operation on an item failed.
    Failure,
    /// A safety stop tripped and waits for the user.
    SafetyStop,
    /// Sync paused (the user's pause or the agent's own), or resumed.
    Paused,
    Resumed,
    /// A problem began, or cleared.
    Problem,
    Resolved,
}

impl EventKind {
    pub const ALL: [EventKind; 8] = [
        EventKind::Synced,
        EventKind::Conflict,
        EventKind::Failure,
        EventKind::SafetyStop,
        EventKind::Paused,
        EventKind::Resumed,
        EventKind::Problem,
        EventKind::Resolved,
    ];

    /// As written in `config.toml` and shown (`safety_stop`).
    pub fn name(self) -> &'static str {
        match self {
            EventKind::Synced => "synced",
            EventKind::Conflict => "conflict",
            EventKind::Failure => "failure",
            EventKind::SafetyStop => "safety_stop",
            EventKind::Paused => "paused",
            EventKind::Resumed => "resumed",
            EventKind::Problem => "problem",
            EventKind::Resolved => "resolved",
        }
    }

    pub fn from_name(s: &str) -> Option<Self> {
        Self::ALL.into_iter().find(|k| k.name().eq_ignore_ascii_case(s.trim()))
    }

    /// Whether events of this kind are about one item (and have a path).
    pub fn has_path(self) -> bool {
        matches!(self, EventKind::Synced | EventKind::Conflict | EventKind::Failure)
    }
}

/// One thing that happened. Never file contents, tokens or approval codes.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Event {
    /// Goes up by one per event while the agent runs (from 1; a restarted
    /// agent starts again).
    pub seq: u64,
    pub at_ms: i64,
    pub kind: EventKind,
    /// The item, relative to the synced folder (`/docs/a.txt`); empty for
    /// events about sync itself.
    #[serde(default)]
    pub path: String,
    /// What was done (`upload`, `download`, `move`, ...), for items.
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub action: String,
    /// Why, or what else to know, in a few words.
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub detail: String,
}

/// A page of events ([`Call::Events`]).
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct Events {
    /// Pass as `since` for the next page.
    pub next: u64,
    /// Events after `since` were dropped before they were read (the agent
    /// keeps the latest few thousand), or the agent restarted.
    #[serde(default)]
    pub missed: bool,
    pub items: Vec<Event>,
}

// ------------------------------------------------------------- JSON-RPC

pub const PARSE_ERROR: i64 = -32700;
pub const INVALID_REQUEST: i64 = -32600;
pub const METHOD_NOT_FOUND: i64 = -32601;
pub const INVALID_PARAMS: i64 = -32602;
/// The code doesn't match anything the agent is waiting for (it may have
/// changed meanwhile).
pub const NO_SUCH_BATCH: i64 = 1;

/// An error the agent returns for a call.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, thiserror::Error)]
#[error("{message}")]
pub struct RpcError {
    pub code: i64,
    pub message: String,
}

impl RpcError {
    pub fn new(code: i64, message: impl Into<String>) -> Self {
        Self { code, message: message.into() }
    }
}

#[derive(Serialize, Deserialize)]
pub(crate) struct Request {
    pub jsonrpc: String,
    pub id: Value,
    pub method: String,
    #[serde(default)]
    pub params: Value,
}

#[derive(Serialize, Deserialize)]
#[serde(bound(serialize = "T: Serialize", deserialize = "T: Deserialize<'de>"))]
pub(crate) struct Response<T = Status> {
    pub jsonrpc: String,
    pub id: Value,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub result: Option<T>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub error: Option<RpcError>,
}

pub(crate) fn request(id: u64, call: &Call) -> Request {
    Request { jsonrpc: "2.0".into(), id: Value::from(id), method: call.method().into(), params: call.params() }
}

/// Decodes a request line: the call, or the error to answer with (and the
/// request's ID, when it could be read).
pub(crate) fn decode(line: &str) -> (Value, Result<Call, RpcError>) {
    let req: Request = match serde_json::from_str(line) {
        Ok(r) => r,
        Err(e) => {
            let code = if serde_json::from_str::<Value>(line).is_ok() { INVALID_REQUEST } else { PARSE_ERROR };
            return (Value::Null, Err(RpcError::new(code, format!("bad request: {e}"))));
        }
    };
    if req.jsonrpc != "2.0" {
        return (req.id, Err(RpcError::new(INVALID_REQUEST, "expected jsonrpc 2.0")));
    }
    let call = Call::parse(&req.method, req.params);
    (req.id, call)
}

pub(crate) fn reply<T>(id: Value, outcome: Result<T, RpcError>) -> Response<T> {
    let (result, error) = match outcome {
        Ok(s) => (Some(s), None),
        Err(e) => (None, Some(e)),
    };
    Response { jsonrpc: "2.0".into(), id, result, error }
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used)]
    use super::*;

    #[test]
    fn the_time_left_follows_the_slower_rate_once_settled() {
        let p = |items_done, bytes_done, bytes_total, elapsed_ms| Progress {
            round: 1,
            items_done,
            items_total: 100,
            bytes_done,
            bytes_total,
            elapsed_ms,
        };
        assert_eq!(p(10, 0, 0, 5_000).eta_ms(), None, "too early to tell");
        assert_eq!(p(0, 0, 0, 60_000).eta_ms(), None, "nothing done yet");
        assert_eq!(p(100, 0, 0, 60_000).eta_ms(), None, "done");
        // A quarter of the items in a minute: three more minutes.
        assert_eq!(p(25, 0, 0, 60_000).eta_ms(), Some(180_000));
        // ...but only a tenth of the bytes: nine more minutes.
        assert_eq!(p(25, 10, 100, 60_000).eta_ms(), Some(540_000));
        // Old agents send no progress; new ones leave it out when idle.
        let s: Status = serde_json::from_str(
            r#"{"version":"0","local":"","remote":"","activity":"idle","paused":{"local":false,"remote":false},"push":false}"#,
        )
        .unwrap();
        assert_eq!(s.progress, None);
    }

    #[test]
    fn calls_round_trip_through_json() {
        let calls = [
            Call::Status,
            Call::SyncNow,
            Call::SyncFirst { path: "docs/a b.txt".into() },
            Call::Stop,
            Call::Watch { since: 42 },
            Call::Events { since: Some(7) },
            Call::Events { since: None },
            Call::Pause { direction: Direction::Both, minutes: Some(120) },
            Call::Pause { direction: Direction::Local, minutes: None },
            Call::Resume { direction: Direction::Remote },
            Call::Answer { code: "308bb40746eae0b2".into(), answer: Answer::Discard },
        ];
        for c in calls {
            let line = serde_json::to_string(&request(7, &c)).unwrap();
            let (id, back) = decode(&line);
            assert_eq!((id, back.unwrap()), (Value::from(7), c));
        }
    }

    #[test]
    fn bad_requests_get_json_rpc_errors() {
        let code = |line: &str| decode(line).1.unwrap_err().code;
        assert_eq!(code("not json"), PARSE_ERROR);
        assert_eq!(code(r#"{"id":1}"#), INVALID_REQUEST);
        assert_eq!(code(r#"{"jsonrpc":"1.0","id":1,"method":"status"}"#), INVALID_REQUEST);
        assert_eq!(code(r#"{"jsonrpc":"2.0","id":1,"method":"format_disk"}"#), METHOD_NOT_FOUND);
        assert_eq!(
            code(r#"{"jsonrpc":"2.0","id":1,"method":"pause","params":{"direction":"sideways"}}"#),
            INVALID_PARAMS
        );
        // The ID comes back even when the method is unknown.
        assert_eq!(decode(r#"{"jsonrpc":"2.0","id":"x","method":"nope"}"#).0, Value::from("x"));
    }
}
