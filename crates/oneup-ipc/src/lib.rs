//! The agent's control API (`docs/architecture/control-api.md`): JSON-RPC 2.0 over a named
//! pipe, one JSON object per line. The CLI and, later, the tray use it to
//! see the agent's status, pause and resume it, and answer what it waits for
//! (safety stops, version-store overflow), all without restarting it.
//!
//! **Who can connect.** Only the current Windows user, on this PC:
//! * the pipe's DACL has a single entry, for the user's SID, and remote
//!   clients are rejected (`oneup_sys::pipe`);
//! * its name ends in 128 random bits, written to [`PIPE_FILE`] in the data
//!   folder (which only the user can read), so no other process can guess
//!   it and create it first to listen in the agent's place. The agent also
//!   creates it as the *first* instance, so it can't join an impostor's pipe;
//! * the client keeps the default impersonation level ("identification"),
//!   so the agent can't act as the client.
//!
//! **Bounded.** Requests are at most [`MAX_LINE`] bytes; at most
//! [`MAX_CLIENTS`] connections are served at once; idle ones are closed
//! after [`IDLE`]. Handlers only read and change in-memory state, so a call
//! never waits for a sync round.

mod protocol;

use std::path::Path;
use std::time::Duration;

use tokio::io::{AsyncBufRead, AsyncBufReadExt, AsyncRead, AsyncReadExt, AsyncWrite, AsyncWriteExt, BufReader};

pub use protocol::{
    Activity, Answer, Call, DatabaseNote, Direction, Event, EventKind, Events, INVALID_PARAMS, INVALID_REQUEST,
    LastRun, METHOD_NOT_FOUND, NO_SUCH_BATCH, Overflow, PARSE_ERROR, PauseState, Problem, Progress, PruneOffer,
    Retrying, RpcError, SafetyStop, Status, Unsynced,
};

/// In the data folder: the name of the running agent's pipe.
pub const PIPE_FILE: &str = "agent.pipe";
/// The longest request (or reply) line accepted.
pub const MAX_LINE: u64 = 64 * 1024;
/// Connections served at once; more are closed straight away.
pub const MAX_CLIENTS: usize = 8;
/// A connection with no request for this long is closed.
pub const IDLE: Duration = Duration::from_secs(60);
/// How long a client waits for a reply.
pub const REPLY_TIMEOUT: Duration = Duration::from_secs(30);
/// The longest a [`Call::Watch`] waits before answering anyway.
pub const WATCH_MAX: Duration = Duration::from_secs(50);

const PIPE_PREFIX: &str = r"\\.\pipe\oneupdrive-";

/// The agent's side: answers calls from its in-memory state.
pub trait Handler: Send + Sync + 'static {
    /// Answers every call but [`Call::Watch`], which [`serve`] turns into a
    /// wait on [`Handler::changes`] and then a [`Call::Status`].
    fn handle(&self, call: Call) -> Result<Status, RpcError>;

    /// The status revision, updated whenever it changes. `None`: watching
    /// isn't supported.
    fn changes(&self) -> Option<tokio::sync::watch::Receiver<u64>> {
        None
    }

    /// Answers [`Call::Events`] at once (after [`serve`] waited for one);
    /// the page must fit in a [`MAX_LINE`] reply. By default the agent has
    /// no events.
    fn events(&self, since: Option<u64>) -> Result<Events, RpcError> {
        let _ = since;
        Err(RpcError::new(METHOD_NOT_FOUND, "this agent has no events"))
    }

    /// The newest event's [`Event::seq`], updated with each new one.
    fn event_changes(&self) -> Option<tokio::sync::watch::Receiver<u64>> {
        None
    }

    /// Whether the client process `pid` may use the control pipe. The
    /// agent refuses its own hook processes (`oneup_app::hooks`), so a hook
    /// can't approve a safety stop or read the status over the pipe. By
    /// default everyone may.
    fn permit(&self, _pid: u32) -> bool {
        true
    }
}

/// Answers a [`Call::Events`]: once there is an event after `since`, or
/// after [`WATCH_MAX`]; at once for `None`.
async fn events(handler: &dyn Handler, since: Option<u64>) -> Result<Events, RpcError> {
    if let (Some(since), Some(mut rx)) = (since, handler.event_changes()) {
        // A closed channel (the agent stopping) answers at once.
        let _ = tokio::time::timeout(WATCH_MAX, rx.wait_for(|n| *n != since)).await;
    }
    handler.events(since)
}

/// Answers a [`Call::Watch`]: once the revision isn't `since` any more, or
/// after [`WATCH_MAX`].
async fn watch(handler: &dyn Handler, since: u64) -> Result<Status, RpcError> {
    let Some(mut rx) = handler.changes() else {
        return Err(RpcError::new(METHOD_NOT_FOUND, "this agent can't be watched"));
    };
    // A closed channel (the agent stopping) answers at once.
    let _ = tokio::time::timeout(WATCH_MAX, rx.wait_for(|r| *r != since)).await;
    handler.handle(Call::Status)
}

/// Reads one line of at most [`MAX_LINE`] bytes into `buf`. `false` at the
/// end of the stream.
async fn read_line<R: AsyncBufRead + Unpin>(r: &mut R, buf: &mut Vec<u8>) -> std::io::Result<bool> {
    buf.clear();
    let n = (&mut *r).take(MAX_LINE + 1).read_until(b'\n', buf).await?;
    if n == 0 {
        return Ok(false);
    }
    if buf.last() != Some(&b'\n') && n as u64 > MAX_LINE {
        return Err(std::io::Error::new(std::io::ErrorKind::InvalidData, "line too long"));
    }
    Ok(true)
}

/// Serves one connection until the client closes it, stays idle for
/// [`IDLE`], or sends a line that's too long.
pub async fn serve<S: AsyncRead + AsyncWrite + Unpin>(stream: S, handler: &dyn Handler) -> std::io::Result<()> {
    let (r, mut w) = tokio::io::split(stream);
    let mut r = BufReader::new(r);
    let mut buf = Vec::new();
    loop {
        match tokio::time::timeout(IDLE, read_line(&mut r, &mut buf)).await {
            Err(_) | Ok(Ok(false)) => return Ok(()),
            Ok(Err(e)) => return Err(e),
            Ok(Ok(true)) => {}
        }
        let line = String::from_utf8_lossy(&buf);
        if line.trim().is_empty() {
            continue;
        }
        let (id, call) = protocol::decode(line.trim());
        if let Ok(c) = &call {
            tracing::debug!(method = c.method(), "control call");
        }
        let out = match call {
            Ok(Call::Events { since }) => serde_json::to_vec(&protocol::reply(id, events(handler, since).await)),
            Ok(Call::Watch { since }) => serde_json::to_vec(&protocol::reply(id, watch(handler, since).await)),
            other => serde_json::to_vec(&protocol::reply(id, other.and_then(|c| handler.handle(c)))),
        };
        let mut out = out.map_err(std::io::Error::other)?;
        out.push(b'\n');
        w.write_all(&out).await?;
        w.flush().await?;
    }
}

#[derive(Debug, thiserror::Error)]
pub enum ClientError {
    /// No agent is running for this data folder.
    #[error("oneupd isn't running for this data folder")]
    NotRunning,
    /// The agent refused the call.
    #[error("{0}")]
    Rpc(RpcError),
    #[error("no reply from oneupd")]
    Timeout,
    #[error("unexpected reply from oneupd: {0}")]
    Protocol(String),
    #[error(transparent)]
    Io(#[from] std::io::Error),
}

/// A connection to the agent, over any stream (a pipe in practice).
pub struct Client<S> {
    r: BufReader<tokio::io::ReadHalf<S>>,
    w: tokio::io::WriteHalf<S>,
    next: u64,
}

impl<S: AsyncRead + AsyncWrite> Client<S> {
    pub fn new(stream: S) -> Self {
        let (r, w) = tokio::io::split(stream);
        Self { r: BufReader::new(r), w, next: 1 }
    }

    /// Makes a call and returns the agent's status after it. Not for
    /// [`Call::Events`] ([`Client::events`]).
    pub async fn call(&mut self, call: &Call) -> Result<Status, ClientError> {
        if matches!(call, Call::Events { .. }) {
            return Err(ClientError::Protocol("events are read with Client::events".into()));
        }
        self.request(call).await
    }

    /// The events after `since` (waiting for one, up to [`WATCH_MAX`]);
    /// `None`: where the feed is now, without waiting.
    pub async fn events(&mut self, since: Option<u64>) -> Result<Events, ClientError> {
        self.request(&Call::Events { since }).await
    }

    async fn request<T: serde::de::DeserializeOwned>(&mut self, call: &Call) -> Result<T, ClientError> {
        let id = self.next;
        self.next += 1;
        let mut line = serde_json::to_vec(&protocol::request(id, call)).map_err(std::io::Error::other)?;
        line.push(b'\n');
        self.w.write_all(&line).await?;
        self.w.flush().await?;
        let mut buf = Vec::new();
        let wait = match call {
            Call::Watch { .. } | Call::Events { .. } => WATCH_MAX + REPLY_TIMEOUT,
            _ => REPLY_TIMEOUT,
        };
        match tokio::time::timeout(wait, read_line(&mut self.r, &mut buf)).await {
            Err(_) => return Err(ClientError::Timeout),
            Ok(Ok(false)) => return Err(ClientError::Protocol("the connection closed".into())),
            Ok(r) => {
                r?;
            }
        }
        let resp: protocol::Response<T> =
            serde_json::from_slice(&buf).map_err(|e| ClientError::Protocol(e.to_string()))?;
        if resp.id != id {
            return Err(ClientError::Protocol("reply to another call".into()));
        }
        match (resp.result, resp.error) {
            (_, Some(e)) => Err(ClientError::Rpc(e)),
            (Some(s), None) => Ok(s),
            (None, None) => Err(ClientError::Protocol("empty reply".into())),
        }
    }
}

/// A random pipe name (see the crate docs).
fn random_name() -> std::io::Result<String> {
    let mut b = [0u8; 16];
    getrandom::fill(&mut b).map_err(|e| std::io::Error::other(format!("random: {e}")))?;
    Ok(format!("{PIPE_PREFIX}{}", b.iter().map(|x| format!("{x:02x}")).collect::<String>()))
}

/// The running agent's pipe name, from the data folder.
fn read_name(data_dir: &Path) -> Result<String, ClientError> {
    match std::fs::read_to_string(data_dir.join(PIPE_FILE)) {
        Ok(s) if s.trim().starts_with(PIPE_PREFIX) => Ok(s.trim().to_string()),
        Ok(_) => Err(ClientError::Protocol(format!("{PIPE_FILE} doesn't hold a OneUpDrive pipe name"))),
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => Err(ClientError::NotRunning),
        Err(e) => Err(e.into()),
    }
}

#[cfg(windows)]
pub use win::{Server, connect};

#[cfg(windows)]
mod win {
    use std::path::{Path, PathBuf};
    use std::sync::Arc;
    use std::time::Duration;

    use tokio::net::windows::named_pipe::{ClientOptions, NamedPipeClient, NamedPipeServer};
    use tokio::sync::Semaphore;

    use super::{Client, ClientError, Handler, MAX_CLIENTS, PIPE_FILE, random_name, read_name, serve};

    /// `ERROR_PIPE_BUSY`: every instance is taken; try again shortly.
    const PIPE_BUSY: i32 = 231;

    /// The agent's pipe, served for as long as this lives. Dropping it stops
    /// serving and removes [`PIPE_FILE`].
    pub struct Server {
        file: PathBuf,
        name: String,
        task: tokio::task::JoinHandle<()>,
    }

    impl Server {
        /// Creates the pipe, records its name in `data_dir`, and serves calls
        /// with `handler`. The caller must hold the data folder's lock, so
        /// only one agent records a name there.
        pub fn start(data_dir: &Path, handler: Arc<dyn Handler>) -> std::io::Result<Self> {
            let name = random_name()?;
            let first = oneup_sys::pipe::create(&name, true)?;
            let file = data_dir.join(PIPE_FILE);
            // Our own file: written whole, then renamed into place.
            let tmp = data_dir.join(format!("{PIPE_FILE}.tmp"));
            std::fs::write(&tmp, &name)?;
            std::fs::rename(&tmp, &file)?;
            let task = tokio::spawn(accept(first, name.clone(), handler));
            Ok(Self { file, name, task })
        }
    }

    impl Drop for Server {
        fn drop(&mut self) {
            self.task.abort();
            if std::fs::read_to_string(&self.file).is_ok_and(|s| s.trim() == self.name) {
                let _ = std::fs::remove_file(&self.file);
            }
        }
    }

    async fn accept(mut next: NamedPipeServer, name: String, handler: Arc<dyn Handler>) {
        let slots = Arc::new(Semaphore::new(MAX_CLIENTS));
        loop {
            if let Err(e) = next.connect().await {
                tracing::warn!(error = %e, "control pipe: a connection failed");
            } else {
                let conn = next;
                // Who's calling: the agent refuses its own hook processes,
                // so a hook can't reach the control calls (approving a
                // safety stop) by any path other programs don't have.
                use std::os::windows::io::AsRawHandle as _;
                let pid = oneup_sys::pipe::client_process_id(conn.as_raw_handle() as isize);
                // A client whose process id can't be read, or which the
                // handler won't vouch for, isn't served: a hook that fired a
                // request and exited before this ran can't slip through.
                let permitted = pid.is_some_and(|p| handler.permit(p));
                match Arc::clone(&slots).try_acquire_owned() {
                    Ok(permit) if permitted => {
                        let handler = Arc::clone(&handler);
                        tokio::spawn(async move {
                            if let Err(e) = serve(conn, handler.as_ref()).await {
                                tracing::debug!(error = %e, "control connection ended");
                            }
                            drop(permit);
                        });
                    }
                    _ => drop(conn),
                }
            }
            // The next instance, for the next client.
            next = loop {
                match oneup_sys::pipe::create(&name, false) {
                    Ok(p) => break p,
                    Err(e) => {
                        tracing::warn!(error = %e, "control pipe: can't create an instance; retrying in 5 s");
                        tokio::time::sleep(Duration::from_secs(5)).await;
                    }
                }
            };
        }
    }

    /// Connects to the agent running for `data_dir`.
    pub async fn connect(data_dir: &Path) -> Result<Client<NamedPipeClient>, ClientError> {
        let name = read_name(data_dir)?;
        for _ in 0..40 {
            match ClientOptions::new().open(&name) {
                Ok(c) => return Ok(Client::new(c)),
                Err(e) if e.raw_os_error() == Some(PIPE_BUSY) => tokio::time::sleep(Duration::from_millis(50)).await,
                // A name left behind by an agent that didn't exit cleanly.
                Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Err(ClientError::NotRunning),
                Err(e) => return Err(e.into()),
            }
        }
        Err(ClientError::Timeout)
    }
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used)]
    use super::*;
    use std::sync::Mutex;

    /// A handler that pauses and reports, like the agent's.
    struct Fake(Mutex<PauseState>);

    fn status(paused: PauseState) -> Status {
        Status {
            version: "test".into(),
            rev: 0,
            local: "L".into(),
            remote: "/R".into(),
            activity: Activity::Idle,
            paused,
            push: false,
            checked_ms: None,
            last: None,
            safety_stops: Vec::new(),
            overflow: None,
            problem: None,
            auto_pause: None,
            auto_pause_hours: None,
            prune_offer: None,
            progress: None,
            unsynced: None,
            retrying: None,
            databases: Vec::new(),
            databases_more: 0,
        }
    }

    impl Handler for Fake {
        fn handle(&self, call: Call) -> Result<Status, RpcError> {
            let mut p = self.0.lock().unwrap();
            match call {
                Call::Pause { .. } => p.local = true,
                Call::Answer { .. } => return Err(RpcError::new(NO_SUCH_BATCH, "nothing waits for that code")),
                _ => {}
            }
            Ok(status(*p))
        }
    }

    #[tokio::test]
    async fn calls_are_answered_in_order_and_errors_dont_end_the_connection() {
        let (a, b) = tokio::io::duplex(1 << 16);
        let fake = Fake(Mutex::new(PauseState::default()));
        let server = tokio::spawn(async move { serve(b, &fake).await });
        let mut c = Client::new(a);
        assert!(!c.call(&Call::Status).await.unwrap().paused.local);
        let s = c.call(&Call::Pause { direction: Direction::Local, minutes: None }).await.unwrap();
        assert!(s.paused.local);
        let e = c.call(&Call::Answer { code: "00".into(), answer: Answer::Keep }).await.unwrap_err();
        assert!(matches!(e, ClientError::Rpc(RpcError { code: NO_SUCH_BATCH, .. })), "{e}");
        assert!(c.call(&Call::Status).await.unwrap().paused.local);
        drop(c);
        server.await.unwrap().unwrap();
    }

    /// A handler whose revision the test moves.
    struct Watched(tokio::sync::watch::Sender<u64>);

    impl Handler for Watched {
        fn handle(&self, _: Call) -> Result<Status, RpcError> {
            Ok(Status { rev: *self.0.borrow(), ..status(PauseState::default()) })
        }
        fn changes(&self) -> Option<tokio::sync::watch::Receiver<u64>> {
            Some(self.0.subscribe())
        }
    }

    #[tokio::test(start_paused = true)]
    async fn a_watch_answers_when_the_status_changes_or_after_a_while() {
        let (a, b) = tokio::io::duplex(1 << 16);
        let tx = tokio::sync::watch::Sender::new(3u64);
        let h = std::sync::Arc::new(Watched(tx));
        let server = {
            let h = std::sync::Arc::clone(&h);
            tokio::spawn(async move { serve(b, h.as_ref()).await })
        };
        let mut c = Client::new(a);
        // Already past what the client saw: at once.
        assert_eq!(c.call(&Call::Watch { since: 2 }).await.unwrap().rev, 3);
        // Not yet: answered when it changes.
        let bump = {
            let h = std::sync::Arc::clone(&h);
            tokio::spawn(async move {
                tokio::time::sleep(Duration::from_secs(5)).await;
                h.0.send_replace(4);
            })
        };
        let t = tokio::time::Instant::now();
        assert_eq!(c.call(&Call::Watch { since: 3 }).await.unwrap().rev, 4);
        assert_eq!(t.elapsed().as_secs(), 5);
        bump.await.unwrap();
        // Nothing changes: answered anyway, after WATCH_MAX.
        let t = tokio::time::Instant::now();
        assert_eq!(c.call(&Call::Watch { since: 4 }).await.unwrap().rev, 4);
        assert_eq!(t.elapsed(), WATCH_MAX);
        // A handler that can't be watched says so.
        drop(c);
        server.await.unwrap().unwrap();
        let (a, b) = tokio::io::duplex(1 << 16);
        let fake = Fake(Mutex::new(PauseState::default()));
        let server = tokio::spawn(async move { serve(b, &fake).await });
        let mut c = Client::new(a);
        let e = c.call(&Call::Watch { since: 0 }).await.unwrap_err();
        assert!(matches!(e, ClientError::Rpc(RpcError { code: METHOD_NOT_FOUND, .. })), "{e}");
        drop(c);
        server.await.unwrap().unwrap();
    }

    /// A handler with an event feed the test adds to.
    struct Feed(Mutex<Vec<Event>>, tokio::sync::watch::Sender<u64>);

    impl Handler for Feed {
        fn handle(&self, _: Call) -> Result<Status, RpcError> {
            Ok(status(PauseState::default()))
        }
        fn events(&self, since: Option<u64>) -> Result<Events, RpcError> {
            let all = self.0.lock().unwrap();
            let next = all.last().map_or(0, |e| e.seq);
            let items = match since {
                None => Vec::new(),
                Some(s) => all.iter().filter(|e| e.seq > s).cloned().collect(),
            };
            Ok(Events { next, missed: false, items })
        }
        fn event_changes(&self) -> Option<tokio::sync::watch::Receiver<u64>> {
            Some(self.1.subscribe())
        }
    }

    #[tokio::test(start_paused = true)]
    async fn events_are_read_from_where_the_client_was_and_waited_for() {
        let (a, b) = tokio::io::duplex(1 << 16);
        let h = std::sync::Arc::new(Feed(Mutex::new(Vec::new()), tokio::sync::watch::Sender::new(0)));
        let server = {
            let h = std::sync::Arc::clone(&h);
            tokio::spawn(async move { serve(b, h.as_ref()).await })
        };
        let mut c = Client::new(a);
        // From now: at once, and nothing.
        let first = c.events(None).await.unwrap();
        assert_eq!((first.next, first.items.len()), (0, 0));
        let add = {
            let h = std::sync::Arc::clone(&h);
            tokio::spawn(async move {
                tokio::time::sleep(Duration::from_secs(3)).await;
                h.0.lock().unwrap().push(Event {
                    seq: 1,
                    at_ms: 5,
                    kind: EventKind::Synced,
                    path: "/a.txt".into(),
                    action: "upload".into(),
                    detail: String::new(),
                });
                h.1.send_replace(1);
            })
        };
        let t = tokio::time::Instant::now();
        let page = c.events(Some(first.next)).await.unwrap();
        assert_eq!(t.elapsed().as_secs(), 3, "waited for the event");
        assert_eq!((page.next, page.items[0].path.as_str()), (1, "/a.txt"));
        add.await.unwrap();
        // The status call can't be used for events, nor the reverse.
        assert!(matches!(c.call(&Call::Events { since: None }).await, Err(ClientError::Protocol(_))));
        assert_eq!(c.call(&Call::Status).await.unwrap().version, "test");
        drop(c);
        server.await.unwrap().unwrap();
        // An agent without events says so.
        let (a, b) = tokio::io::duplex(1 << 16);
        let fake = Fake(Mutex::new(PauseState::default()));
        let server = tokio::spawn(async move { serve(b, &fake).await });
        let mut c = Client::new(a);
        let e = c.events(Some(0)).await.unwrap_err();
        assert!(matches!(e, ClientError::Rpc(RpcError { code: METHOD_NOT_FOUND, .. })), "{e}");
        drop(c);
        server.await.unwrap().unwrap();
    }

    #[test]
    fn event_kinds_have_one_name_each() {
        for k in EventKind::ALL {
            assert_eq!(EventKind::from_name(k.name()), Some(k));
            assert_eq!(serde_json::to_value(k).unwrap(), serde_json::Value::from(k.name()));
        }
        assert_eq!(EventKind::from_name(" Safety_Stop "), Some(EventKind::SafetyStop));
        assert_eq!(EventKind::from_name("deleted"), None);
    }

    #[tokio::test]
    async fn an_oversized_request_closes_the_connection() {
        let (mut a, b) = tokio::io::duplex(1 << 20);
        let fake = Fake(Mutex::new(PauseState::default()));
        let server = tokio::spawn(async move { serve(b, &fake).await });
        a.write_all(&vec![b'x'; MAX_LINE as usize + 10]).await.unwrap();
        a.write_all(b"\n").await.unwrap();
        let err = server.await.unwrap().unwrap_err();
        assert_eq!(err.kind(), std::io::ErrorKind::InvalidData);
    }

    #[test]
    fn pipe_names_are_random_and_recognised() {
        let (a, b) = (random_name().unwrap(), random_name().unwrap());
        assert_ne!(a, b);
        assert!(a.starts_with(PIPE_PREFIX) && a.len() == PIPE_PREFIX.len() + 32);
        let dir = std::env::temp_dir().join(format!("oneup-ipc-name-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        assert!(matches!(read_name(&dir), Err(ClientError::NotRunning)));
        std::fs::write(dir.join(PIPE_FILE), r"\\.\pipe\something-else").unwrap();
        assert!(matches!(read_name(&dir), Err(ClientError::Protocol(_))));
        std::fs::write(dir.join(PIPE_FILE), &a).unwrap();
        assert_eq!(read_name(&dir).unwrap(), a);
        std::fs::remove_dir_all(&dir).unwrap();
    }
}
