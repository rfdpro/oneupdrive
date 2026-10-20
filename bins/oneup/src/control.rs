//! What the user controls and sees of the running agent, through its control
//! pipe (`oneup_ipc`): pause and resume, answers to what it waits for, and
//! its status. Shared by the agent's loop and the pipe's handler, which
//! only reads and changes this in-memory state; the loop acts on it.
//!
//! The pause is saved to [`PAUSE_FILE`] in the data folder the moment it
//! changes, so a paused agent stays paused when it restarts, however it
//! ended. Answers are not saved: a code answers for one batch the agent is
//! showing right now, and only such codes are accepted.

use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, Ordering::Relaxed};
use std::sync::{Arc, Mutex, MutexGuard};
use std::time::{SystemTime, UNIX_EPOCH};

use oneup_gate::{Breaker, Overflow, OverflowOutcome, Side};
use oneup_ipc::{
    Activity, Answer, Call, Direction, EventKind, Events, INVALID_PARAMS, LastRun, NO_SUCH_BATCH, PauseState, RpcError,
    SafetyStop, Status,
};
use oneup_sync::Op;
use tokio::sync::Notify;

use crate::events::{Feed, New};
use crate::sync::{Answers, Controls, Offer};

/// In the data folder: the agent's pause (JSON), while there is one.
pub const PAUSE_FILE: &str = "agent-pause.json";

pub fn now_ms() -> i64 {
    SystemTime::now().duration_since(UNIX_EPOCH).map(|d| d.as_millis() as i64).unwrap_or(0)
}

/// Which sides are paused right now, for a round already under way: set by
/// [`Control`] the moment the pause changes (the user's or the agent's own),
/// read by the executor before each operation, so it is two flags, not a
/// lock or a file.
#[derive(Debug, Default)]
pub struct LivePause {
    local: AtomicBool,
    remote: AtomicBool,
    /// The network's policy now (`crate::network`), for transfers of file
    /// contents: set the moment Windows reports another network.
    net: Mutex<crate::network::Policy>,
    /// The folder uses Files On-Demand: downloads are placeholders.
    on_demand: AtomicBool,
}

impl LivePause {
    fn set(&self, local: bool, remote: bool) {
        self.local.store(local, Relaxed);
        self.remote.store(remote, Relaxed);
    }

    fn set_net(&self, p: &crate::network::Policy) {
        *self.net.lock().unwrap_or_else(|e| e.into_inner()) = p.clone();
    }

    /// Whether the network lets `op` through now. A download's size isn't
    /// known here: under a size limit, a download after a change of
    /// network waits for the next round, which knows it.
    fn net_allows(&self, op: &Op) -> bool {
        match crate::network::content(op, self.on_demand.load(Relaxed), |_| None) {
            Some((way, bytes)) => self.net.lock().unwrap_or_else(|e| e.into_inner()).allows(way, bytes),
            None => true,
        }
    }

    /// For one round: whether an operation may run now. Not one that
    /// changes a side paused since the round began, nor, after that, any
    /// other changing that side, even if it resumed meanwhile: they may
    /// depend on one that didn't run. Bookkeeping always may.
    ///
    /// The same for a change of network: once its policy differs from the
    /// one the round was planned under, a transfer of file contents it
    /// doesn't let through stops that side for the rest of the round.
    pub fn round_check(&self) -> impl Fn(&Op) -> bool + Sync + '_ {
        let stopped = [AtomicBool::new(false), AtomicBool::new(false)];
        let planned = self.net.lock().unwrap_or_else(|e| e.into_inner()).clone();
        move |op| {
            // A duplicate removed from both sides changes both.
            let changed = *self.net.lock().unwrap_or_else(|e| e.into_inner()) != planned;
            let mut ok = true;
            for (i, side, paused) in [(0, Side::Local, &self.local), (1, Side::Remote, &self.remote)] {
                if !oneup_gate::changes(op, side) {
                    continue;
                }
                if paused.load(Relaxed) || (changed && !self.net_allows(op)) {
                    stopped[i].store(true, Relaxed);
                }
                ok &= !stopped[i].load(Relaxed);
            }
            ok
        }
    }
}

/// The agent's shared control state.
pub struct Control {
    state: Mutex<Shared>,
    /// The pause in effect, for rounds under way (see [`LivePause`]).
    live: Arc<LivePause>,
    /// Where the pause is saved (`None`: nowhere, in tests).
    pause_file: Option<PathBuf>,
    /// Pinged when a call changed something the loop must act on.
    pub wake: Notify,
    /// Whether the push channel is live (set by the push task).
    pub push_up: Arc<AtomicBool>,
    /// Pinged once when a `stop` call arrives.
    pub stop: Notify,
    /// The link to OneDrive is poor (`crate::link`): retries wait longer.
    poor_link: AtomicBool,
    stopping: AtomicBool,
    /// The status revision, for clients that watch it (`Call::Watch`).
    rev: tokio::sync::watch::Sender<u64>,
    /// What happened, for `oneup watch` and hooks (`Call::Events`).
    pub events: Arc<Feed>,
    /// Whether a client's process may use the pipe: the agent sets this so
    /// its own hook processes are turned away (`crate::hooks`), so a hook
    /// can't approve a safety stop or read the status. `None` (tests, `oneup
    /// sync`): everyone may.
    may_serve: std::sync::OnceLock<Box<dyn Fn(u32) -> bool + Send + Sync>>,
}

struct Shared {
    pause: PauseState,
    answers: Answers,
    local: String,
    remote: String,
    activity: Activity,
    checked_ms: Option<i64>,
    last: Option<LastRun>,
    stops: Vec<SafetyStop>,
    overflow: Option<oneup_ipc::Overflow>,
    /// Codes of the stops and the batch answered since: no longer shown as
    /// waiting. The rounds that carry them out can take long (12,711
    /// deletions once ran 40 minutes under Graph's throttling), and until
    /// they end, [`Control::record`] still has them.
    answered: Vec<String>,
    /// Not shown once accepted (it's being carried out).
    prune_offer: Option<oneup_ipc::PruneOffer>,
    /// The problem being retried, and the last one that cleared (until
    /// [`Control::take_resolved`] collects it for the activity log).
    problem: Option<oneup_ipc::Problem>,
    resolved: Option<oneup_ipc::Problem>,
    auto_pause: Option<crate::autopause::Reason>,
    /// Paths to transfer first (`oneup sync-now PATH`), until a round settles.
    first: Vec<String>,
    /// The round being carried out, while one is.
    progress: Option<oneup_ipc::Progress>,
    /// What can't sync until the user acts (`crate::unsynced`).
    unsynced: Option<oneup_ipc::Unsynced>,
    /// Failed items waiting before they're tried again (`crate::backoff`).
    retrying: Option<oneup_ipc::Retrying>,
    databases: Vec<oneup_ipc::DatabaseNote>,
    databases_more: u64,
    /// OneDrive is full: uploads wait, everything else goes on. Shown as
    /// the problem while no other one is.
    cloud_full: Option<oneup_ipc::Problem>,
    /// The network and what it lets sync do (`crate::network`).
    net: oneup_ipc::NetStatus,
    /// What the network's profile lets rounds do.
    policy: crate::network::Policy,
    /// Megabytes the user allowed on top of the data budget, for the agent
    /// to add (`crate::netusage`).
    allowed_mb: u64,
    /// The status as watchers last saw it (its `rev` left at 0).
    shown: Option<Status>,
    /// The sides paused when that was last told as an event, and why.
    paused_told: (bool, bool, Option<String>),
}

fn code(f: u64) -> String {
    format!("{f:016x}")
}

/// Which sides are paused, the user's way or the agent's own (which pauses
/// both).
fn paused_sides(s: &Shared) -> (bool, bool) {
    let auto = s.auto_pause.is_some();
    (s.pause.local || auto, s.pause.remote || auto)
}

/// Whether the user paused sync (either side) in data folder `dir`, as
/// saved, at `now`: what other work started by hand stops for.
pub fn paused_in(dir: &Path, now: i64) -> bool {
    let p = load_pause(&dir.join(PAUSE_FILE));
    (p.local || p.remote) && p.until_ms.is_none_or(|u| u > now)
}

/// Reads a saved pause; anything unreadable counts as "not paused" (it's
/// our own file, and not pausing loses nothing).
fn load_pause(file: &Path) -> PauseState {
    std::fs::read_to_string(file).ok().and_then(|s| serde_json::from_str(&s).ok()).unwrap_or_default()
}

/// Saves the pause (our own file, written through `crate::ownfile`: an
/// empty file after a power cut would read as "not paused"), or removes
/// the file when nothing is paused.
fn save_pause(file: &Path, p: &PauseState) -> std::io::Result<()> {
    if !p.local && !p.remote {
        return match std::fs::remove_file(file) {
            Err(e) if e.kind() != std::io::ErrorKind::NotFound => Err(e),
            _ => Ok(()),
        };
    }
    crate::ownfile::write(file, &serde_json::to_vec(p).map_err(std::io::Error::other)?)
}

impl Control {
    /// `data_dir`: where the pause is kept (`None`: not kept). `answers`:
    /// those given on the command line.
    pub fn new(data_dir: Option<&Path>, local: String, remote: String, answers: Answers) -> Self {
        let pause_file = data_dir.map(|d| d.join(PAUSE_FILE));
        let pause = pause_file.as_deref().map(load_pause).unwrap_or_default();
        let live = LivePause::default();
        live.set(pause.local, pause.remote);
        Self {
            live: Arc::new(live),
            state: Mutex::new(Shared {
                pause,
                answers,
                local,
                remote,
                activity: Activity::Starting,
                checked_ms: None,
                last: None,
                stops: Vec::new(),
                overflow: None,
                answered: Vec::new(),
                prune_offer: None,
                problem: None,
                resolved: None,
                auto_pause: None,
                first: Vec::new(),
                progress: None,
                unsynced: None,
                retrying: None,
                databases: Vec::new(),
                databases_more: 0,
                cloud_full: None,
                net: oneup_ipc::NetStatus::default(),
                policy: crate::network::Policy::default(),
                allowed_mb: 0,
                shown: None,
                paused_told: (pause.local, pause.remote, None),
            }),
            pause_file,
            wake: Notify::new(),
            push_up: Arc::new(AtomicBool::new(false)),
            stop: Notify::new(),
            poor_link: AtomicBool::new(false),
            stopping: AtomicBool::new(false),
            rev: tokio::sync::watch::Sender::new(0),
            events: Arc::new(Feed::new()),
            may_serve: std::sync::OnceLock::new(),
        }
    }

    /// Serve a client process only when `may_serve` returns true (the agent
    /// turns away its own hook processes). Set once, before the pipe serves.
    pub fn gate_clients(&self, may_serve: Box<dyn Fn(u32) -> bool + Send + Sync>) {
        let _ = self.may_serve.set(may_serve);
    }

    /// Moves the revision on if the status changed, and passes the pause
    /// on to a round under way. Called with the state locked, after each
    /// change to it.
    fn publish(&self, s: &mut Shared) {
        let (local, remote) = paused_sides(s);
        self.live.set(local, remote);
        self.tell_pause(s, local, remote);
        let now = Status { rev: 0, ..self.status(s) };
        if s.shown.as_ref() != Some(&now) {
            s.shown = Some(now);
            self.rev.send_modify(|r| *r += 1);
        }
    }

    /// A pause beginning or ending, as an event: which sides, and why if
    /// the agent paused by itself.
    fn tell_pause(&self, s: &mut Shared, local: bool, remote: bool) {
        // Events are English data, like the activity log.
        let english = crate::l10n::Locale::english();
        let now = (local, remote, s.auto_pause.as_ref().map(|r| r.said(&english)));
        if s.paused_told == now {
            return;
        }
        let sides = |l: bool, r: bool| match (l, r) {
            (true, true) => "both directions",
            (true, false) => "changes to this PC",
            (false, true) => "changes to OneDrive",
            (false, false) => "",
        };
        let (was_l, was_r, _) = s.paused_told;
        let mut news = Vec::new();
        let (began_l, began_r) = (local && !was_l, remote && !was_r);
        if began_l || began_r || (now.2.is_some() && now.2 != s.paused_told.2) {
            let detail = match &now.2 {
                Some(why) => format!("{}; by itself: {why}", sides(local, remote)),
                None => sides(local, remote).to_string(),
            };
            news.push(New::about_sync(EventKind::Paused, detail));
        }
        let (ended_l, ended_r) = (was_l && !local, was_r && !remote);
        if ended_l || ended_r {
            news.push(New::about_sync(EventKind::Resumed, sides(ended_l, ended_r)));
        }
        s.paused_told = now;
        self.events.push(news);
    }

    fn lock(&self) -> MutexGuard<'_, Shared> {
        // Nothing in here can be left half-changed by a panic that matters.
        self.state.lock().unwrap_or_else(|e| e.into_inner())
    }

    fn save(&self, p: &PauseState) {
        if let Some(f) = &self.pause_file
            && let Err(e) = save_pause(f, p)
        {
            tracing::warn!(error = %e, "can't save the pause; it lasts until oneupd stops");
        }
    }

    /// Ends a timed pause that is over (at `now`). Returns the pause in
    /// effect.
    pub fn settle_pause(&self, now: i64) -> PauseState {
        let mut s = self.lock();
        if s.pause.until_ms.is_some_and(|u| u <= now) {
            s.pause = PauseState::default();
            self.save(&s.pause);
            tracing::info!("the timed pause is over");
            println!("oneupd: the pause is over; syncing again");
        }
        self.publish(&mut s);
        s.pause
    }

    /// The controls for the next rounds. Pausing by itself pauses both
    /// sides, like the user's pause: it can begin while rounds run.
    pub fn controls(&self) -> Controls {
        let s = self.lock();
        let (pause_local, pause_remote) = paused_sides(&s);
        Controls {
            answers: s.answers.clone(),
            pause_local,
            pause_remote,
            first: s.first.clone(),
            live: Some(Arc::clone(&self.live)),
            net: s.policy.clone(),
        }
    }

    /// Drops approval codes the rounds have used: each approves its list
    /// once (`sync::Engine::spent_answers`).
    pub fn forget_approvals(&self, codes: &[u64]) {
        if codes.is_empty() {
            return;
        }
        self.lock().answers.approve.retain(|c| !codes.contains(c));
    }

    /// The paths asked to go first have been dealt with (a round settled).
    pub fn clear_first(&self) {
        self.lock().first.clear();
    }

    pub fn set_activity(&self, a: Activity) {
        let mut s = self.lock();
        s.activity = a;
        self.publish(&mut s);
    }

    /// How far the round being carried out is (`None`: no round runs).
    /// The engine reports about once a second, which is how often watchers
    /// are woken for it.
    pub fn set_progress(&self, p: Option<oneup_ipc::Progress>) {
        let mut s = self.lock();
        s.progress = p;
        self.publish(&mut s);
    }

    /// What can't sync until the user acts (`crate::unsynced`).
    pub fn set_unsynced(&self, u: Option<oneup_ipc::Unsynced>) {
        let mut s = self.lock();
        s.unsynced = u;
        self.publish(&mut s);
    }

    /// The failed items waiting to be tried again (`crate::backoff`).
    pub fn set_retrying(&self, r: Option<oneup_ipc::Retrying>) {
        let mut s = self.lock();
        s.retrying = r;
        self.publish(&mut s);
    }

    /// The database files held back or warned about after the last rounds
    /// (`crate::databases`).
    pub fn set_databases(&self, d: Vec<oneup_ipc::DatabaseNote>, more: u64) {
        let mut s = self.lock();
        if s.databases != d || s.databases_more != more {
            s.databases = d;
            s.databases_more = more;
            self.publish(&mut s);
        }
    }

    /// OneDrive is full (`crate::problem::cloud_full`), or no longer.
    pub fn set_cloud_full(&self, p: Option<oneup_ipc::Problem>) {
        let mut s = self.lock();
        s.cloud_full = p;
        self.publish(&mut s);
    }

    /// How the agent uses the network now (`crate::network`, `crate::pace`).
    pub fn set_network(&self, n: oneup_ipc::NetStatus) {
        let mut s = self.lock();
        let link = std::mem::take(&mut s.net.link);
        s.net = oneup_ipc::NetStatus { link, ..n };
        self.publish(&mut s);
    }

    /// What the network's profile lets rounds do: the next ones, and the
    /// one under way from its next operation on.
    pub fn set_policy(&self, p: crate::network::Policy) {
        self.live.set_net(&p);
        self.lock().policy = p;
    }

    /// Whether the link to OneDrive is poor (`crate::link`).
    pub fn set_poor_link(&self, poor: bool) {
        if self.poor_link.swap(poor, Relaxed) != poor {
            let mut s = self.lock();
            s.net.link = if poor { "poor".into() } else { String::new() };
            self.publish(&mut s);
        }
    }

    pub fn poor_link(&self) -> bool {
        self.poor_link.load(Relaxed)
    }

    /// The megabytes allowed on top of the data budget since last asked.
    pub fn take_allowed_mb(&self) -> u64 {
        std::mem::take(&mut self.lock().allowed_mb)
    }

    /// The folder uses Files On-Demand (cloud files arrive as placeholders,
    /// which move no contents).
    pub fn set_on_demand(&self, on: bool) {
        self.live.on_demand.store(on, Relaxed);
    }

    /// Why the agent paused by itself, or `None` when it didn't.
    pub fn set_auto_pause(&self, why: Option<crate::autopause::Reason>) {
        let mut s = self.lock();
        s.auto_pause = why;
        self.publish(&mut s);
    }

    /// The problem the agent is retrying (`crate::problem`).
    pub fn set_problem(&self, p: Option<oneup_ipc::Problem>) {
        let mut s = self.lock();
        if p.is_some() {
            s.activity = Activity::Retrying;
        }
        // Once a spell, as the activity log notes it.
        if let Some(new) = &p
            && s.problem.as_ref().is_none_or(|old| old.kind != new.kind)
        {
            self.events.push(vec![New::about_sync(EventKind::Problem, format!("{}: {}", new.kind, new.title))]);
        }
        s.problem = p;
        self.publish(&mut s);
    }

    /// The problem being retried, if any.
    pub fn problem(&self) -> Option<oneup_ipc::Problem> {
        self.lock().problem.clone()
    }

    /// The problem that cleared since this was last asked, if one did.
    pub fn take_resolved(&self) -> Option<oneup_ipc::Problem> {
        self.lock().resolved.take()
    }

    /// Records a sequence of rounds that finished at `at_ms`: what it did
    /// (`None` if nothing; the last changes are kept then), the tripped
    /// safety stops (empty if none), the overflow batch (if any) and the
    /// offer to remove excluded items from OneDrive (if any). Returns
    /// whether the stops, the batch or the offer differ from what was shown
    /// before, so the caller prints them once.
    pub fn record(
        &self,
        at_ms: i64,
        last: Option<LastRun>,
        breakers: &[Breaker],
        overflow: Option<&Overflow>,
        offer: Option<&Offer>,
    ) -> Recorded {
        let stops: Vec<SafetyStop> = breakers
            .iter()
            .map(|b| SafetyStop {
                kind: match b.kind {
                    oneup_gate::BreakerKind::Mass => "mass".into(),
                    oneup_gate::BreakerKind::Suspicious => "content".into(),
                    oneup_gate::BreakerKind::Removal => "removal".into(),
                },
                side: match b.side {
                    Side::Local => "local".into(),
                    Side::Remote => "remote".into(),
                },
                count: b.count,
                at_least: b.at_least,
                recent: b.recent,
                synced_items: b.synced_items,
                code: code(b.fingerprint.0),
            })
            .collect();
        let overflow = overflow.map(|o| oneup_ipc::Overflow {
            count: o.count,
            bytes: o.bytes,
            room: o.room,
            code: code(o.fingerprint.0),
            waiting: o.outcome == OverflowOutcome::Held,
        });
        let offer = offer.filter(|o| !o.accepted).map(|o| oneup_ipc::PruneOffer {
            entries: o.entries,
            items: o.items,
            bytes: o.bytes,
            code: code(o.fingerprint),
        });
        let mut s = self.lock();
        let new_stops = !stops.is_empty() && stops != s.stops;
        let new_offer = offer.is_some() && offer != s.prune_offer;
        // A batch is shown again when its outcome changes (held, then kept
        // or discarded as the user chose).
        let key = |o: &Option<oneup_ipc::Overflow>| o.as_ref().map(|o| (o.code.clone(), o.waiting));
        let new_overflow = overflow.is_some() && key(&overflow) != key(&s.overflow);
        s.checked_ms = Some(at_ms);
        if last.is_some() {
            s.last = last;
        }
        s.stops = stops;
        s.overflow = overflow;
        // Kept while the agent still reports them: a round already under
        // way when the answer came can trip the same stop once more.
        s.prune_offer = offer;
        let Shared { answered, stops, overflow, prune_offer, answers, .. } = &mut *s;
        // An approval answers the stop it was given for while that stands:
        // one the rounds didn't take in (the list changed meanwhile) isn't
        // kept for an identical list some later raising shows.
        answers.approve.retain(|f| stops.iter().any(|st| st.code == code(*f)));
        answered.retain(|c| {
            stops.iter().any(|st| &st.code == c)
                || overflow.as_ref().is_some_and(|o| o.waiting && &o.code == c)
                || prune_offer.as_ref().is_some_and(|o| &o.code == c)
        });
        if let Some(p) = s.problem.take() {
            self.events.push(vec![New::about_sync(EventKind::Resolved, format!("{}: {}", p.kind, p.title))]);
            s.resolved = Some(p);
        }
        if new_stops {
            // What and where, never the code that approves it.
            self.events.push(
                breakers.iter().map(|b| New::about_sync(EventKind::SafetyStop, crate::sync::breaker_what(b))).collect(),
            );
        }
        self.publish(&mut s);
        Recorded { new_stops, new_overflow, new_offer }
    }

    /// The status now (as `oneup status` gets it).
    pub fn snapshot(&self) -> Status {
        let s = self.lock();
        self.status(&s)
    }

    /// Changes whenever the status does (for in-process watchers).
    pub fn subscribe(&self) -> tokio::sync::watch::Receiver<u64> {
        self.rev.subscribe()
    }

    fn status(&self, s: &Shared) -> Status {
        Status {
            version: env!("CARGO_PKG_VERSION").into(),
            rev: *self.rev.borrow(),
            local: s.local.clone(),
            remote: s.remote.clone(),
            activity: if self.stopping.load(Relaxed) {
                Activity::Stopping
            } else if s.pause.local && s.pause.remote {
                Activity::Paused
            } else {
                s.activity
            },
            paused: s.pause,
            push: self.push_up.load(Relaxed),
            checked_ms: s.checked_ms,
            last: s.last.clone(),
            safety_stops: s.stops.iter().filter(|st| !s.answered.contains(&st.code)).cloned().collect(),
            overflow: s.overflow.clone().map(|mut o| {
                o.waiting &= !s.answered.contains(&o.code);
                o
            }),
            problem: s.problem.clone().or_else(|| s.cloud_full.clone()),
            auto_pause: s.auto_pause.as_ref().map(|r| r.key.to_string()),
            auto_pause_hours: s.auto_pause.as_ref().and_then(|r| r.hours.clone()),
            prune_offer: s.prune_offer.clone().filter(|o| !s.answered.contains(&o.code)),
            progress: s.progress.clone(),
            unsynced: s.unsynced.clone(),
            retrying: s.retrying.clone(),
            databases: s.databases.clone(),
            databases_more: s.databases_more,
            network: (s.net != oneup_ipc::NetStatus::default()).then(|| s.net.clone()),
        }
    }

    fn call(&self, call: Call, now: i64) -> Result<Status, RpcError> {
        let mut s = self.lock();
        let mut wake = true;
        match call {
            // `serve` answers a watch itself; here it is a status.
            Call::Status | Call::Watch { .. } | Call::Events { .. } => wake = false,
            Call::SyncNow => {}
            Call::AllowData { mb } => {
                if !(1..=1_000_000).contains(&mb) {
                    return Err(RpcError::new(INVALID_PARAMS, "allow from 1 MB to 1,000,000 MB"));
                }
                s.allowed_mb = s.allowed_mb.saturating_add(mb);
            }
            Call::SyncFirst { path } => {
                // Bounded: a client can't grow the agent's memory.
                if !s.first.contains(&path) && s.first.len() < 32 {
                    s.first.push(path);
                }
            }
            Call::Stop => {
                wake = false;
                if !self.stopping.swap(true, Relaxed) {
                    tracing::info!("stop requested");
                    self.stop.notify_one();
                }
            }
            Call::Pause { direction, minutes } => {
                if minutes == Some(0) {
                    return Err(RpcError::new(INVALID_PARAMS, "a timed pause needs at least one minute"));
                }
                let (l, r) = sides(direction);
                s.pause.local |= l;
                s.pause.remote |= r;
                // One timer for the whole pause: the latest call sets it.
                s.pause.until_ms = minutes
                    .map(|m| now.saturating_add(i64::try_from(m).unwrap_or(i64::MAX / 2).saturating_mul(60_000)));
                self.save(&s.pause);
            }
            Call::Resume { direction } => {
                let (l, r) = sides(direction);
                s.pause.local &= !l;
                s.pause.remote &= !r;
                if !s.pause.local && !s.pause.remote {
                    s.pause.until_ms = None;
                }
                if s.activity == Activity::Paused {
                    // The loop wakes and runs now.
                    s.activity = Activity::Syncing;
                }
                self.save(&s.pause);
            }
            Call::Answer { code: given, answer } => {
                let given = given.trim().to_ascii_lowercase();
                let Ok(f) = u64::from_str_radix(&given, 16) else {
                    return Err(RpcError::new(INVALID_PARAMS, "expected the 16-character code oneupd showed"));
                };
                let matches = match answer {
                    Answer::Approve => s.stops.iter().any(|st| st.code == code(f)),
                    Answer::Prune | Answer::KeepInCloud => s.prune_offer.as_ref().is_some_and(|o| o.code == code(f)),
                    Answer::Keep | Answer::Discard => {
                        s.overflow.as_ref().is_some_and(|o| o.waiting && o.code == code(f))
                    }
                };
                if !matches {
                    return Err(RpcError::new(
                        NO_SUCH_BATCH,
                        "oneupd isn't waiting for that code (it may have changed meanwhile; see `oneup status`)",
                    ));
                }
                let list = match answer {
                    Answer::Approve => &mut s.answers.approve,
                    Answer::Keep => &mut s.answers.keep,
                    Answer::Discard => &mut s.answers.discard,
                    Answer::Prune => &mut s.answers.prune,
                    Answer::KeepInCloud => &mut s.answers.keep_in_cloud,
                };
                if !list.contains(&f) {
                    list.push(f);
                }
                if answer != Answer::Prune && !s.answered.contains(&code(f)) {
                    s.answered.push(code(f));
                }
                if s.activity == Activity::WaitingForYou {
                    // The loop wakes and carries it out now.
                    s.activity = Activity::Syncing;
                }
                tracing::info!(?answer, "answer received");
            }
        }
        self.publish(&mut s);
        let status = self.status(&s);
        drop(s);
        if wake {
            self.wake.notify_one();
        }
        Ok(status)
    }
}

/// What [`Control::record`] found new, to print once.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Recorded {
    pub new_stops: bool,
    pub new_overflow: bool,
    pub new_offer: bool,
}

fn sides(d: Direction) -> (bool, bool) {
    match d {
        Direction::Both => (true, true),
        Direction::Local => (true, false),
        Direction::Remote => (false, true),
    }
}

impl oneup_ipc::Handler for Control {
    fn handle(&self, call: Call) -> Result<Status, RpcError> {
        self.call(call, now_ms())
    }

    fn changes(&self) -> Option<tokio::sync::watch::Receiver<u64>> {
        Some(self.rev.subscribe())
    }

    fn events(&self, since: Option<u64>) -> Result<Events, RpcError> {
        Ok(self.events.page(since))
    }

    fn event_changes(&self) -> Option<tokio::sync::watch::Receiver<u64>> {
        Some(self.events.subscribe())
    }

    fn permit(&self, pid: u32) -> bool {
        self.may_serve.get().is_none_or(|allow| allow(pid))
    }
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used)]
    use super::*;
    use oneup_gate::Fingerprint;

    fn control(dir: Option<&Path>) -> Control {
        Control::new(dir, "L".into(), "/R".into(), Answers::default())
    }

    fn rec(new_stops: bool, new_overflow: bool) -> Recorded {
        Recorded { new_stops, new_overflow, new_offer: false }
    }

    #[test]
    fn an_offer_to_prune_is_accepted_by_its_code_and_hidden_once_accepted() {
        let c = control(None);
        let offer = Offer { fingerprint: 0xef, entries: 2, items: 30, bytes: 5, accepted: false, covered: Vec::new() };
        assert!(c.record(1, None, &[], None, Some(&offer)).new_offer);
        assert!(!c.record(2, None, &[], None, Some(&offer)).new_offer);
        let answer = |code: &str, answer| c.call(Call::Answer { code: code.into(), answer }, 0);
        assert_eq!(answer("00000000000000ef", Answer::Approve).unwrap_err().code, NO_SUCH_BATCH);
        assert_eq!(answer("00000000000000ee", Answer::Prune).unwrap_err().code, NO_SUCH_BATCH);
        let s = answer("00000000000000ef", Answer::Prune).unwrap();
        assert_eq!(s.prune_offer.unwrap().items, 30);
        assert_eq!(c.controls().answers.prune, vec![0xef]);
        // Being carried out: no longer offered.
        let s = c.call(Call::Status, 0).unwrap();
        assert!(s.prune_offer.is_some());
        c.record(3, None, &[], None, Some(&Offer { accepted: true, ..offer }));
        assert!(c.call(Call::Status, 0).unwrap().prune_offer.is_none());
    }

    #[test]
    fn keeping_offered_items_in_onedrive_settles_the_offer_at_once() {
        let c = control(None);
        let offer = Offer { fingerprint: 0xef, entries: 2, items: 30, bytes: 5, accepted: false, covered: Vec::new() };
        c.record(1, None, &[], None, Some(&offer));
        let s = c.call(Call::Answer { code: "00000000000000ef".into(), answer: Answer::KeepInCloud }, 0).unwrap();
        assert!(s.prune_offer.is_none());
        assert_eq!(c.controls().answers.keep_in_cloud, vec![0xef]);
        // Not shown again while the agent still has it, before a round
        // saves the answer.
        assert!(!c.record(2, None, &[], None, Some(&offer)).new_offer);
        assert!(c.snapshot().prune_offer.is_none());
    }

    fn last() -> Option<LastRun> {
        Some(LastRun { at_ms: 1, done: 3, skipped: 0, failed: 0, held: 0 })
    }

    #[test]
    fn pauses_are_kept_timed_and_saved_at_once() {
        let dir = std::env::temp_dir().join(format!("oneup-control-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        let c = control(Some(&dir));
        let s = c.call(Call::Pause { direction: Direction::Local, minutes: None }, 0).unwrap();
        assert_eq!((s.paused.local, s.paused.remote, s.activity), (true, false, Activity::Starting));
        // Saved by the call itself: an agent started now is paused too.
        assert!(control(Some(&dir)).controls().pause_local);
        let s = c.call(Call::Pause { direction: Direction::Remote, minutes: Some(2) }, 1000).unwrap();
        assert_eq!((s.activity, s.paused.until_ms), (Activity::Paused, Some(121_000)));
        assert!(c.controls().pause_local && c.controls().pause_remote);
        assert!(c.call(Call::Pause { direction: Direction::Both, minutes: Some(0) }, 0).is_err());

        // The timer ends it, for both directions, and that is saved too.
        assert!(c.settle_pause(2000).remote);
        assert_eq!(c.settle_pause(121_000), PauseState::default());
        let again = control(Some(&dir));
        assert!(!again.controls().pause_local && !again.controls().pause_remote);
        assert!(!dir.join(PAUSE_FILE).exists());

        // Resuming one direction leaves the other paused.
        c.call(Call::Pause { direction: Direction::Both, minutes: None }, 0).unwrap();
        let s = c.call(Call::Resume { direction: Direction::Remote }, 0).unwrap();
        assert_eq!((s.paused.local, s.paused.remote), (true, false));
        // A damaged file reads as "not paused".
        std::fs::write(dir.join(PAUSE_FILE), b"{garbage").unwrap();
        assert!(!control(Some(&dir)).controls().pause_local);
        std::fs::remove_dir_all(&dir).unwrap();
    }

    #[test]
    fn a_pause_mid_round_stops_that_side_for_the_rest_of_the_round() {
        use oneup_sync::{LocalId, LocalPre, RemoteId, RemotePre};
        let c = control(None);
        let live = c.controls().live.unwrap();
        let here = Op::RecycleLocal { sid: 1, local: LocalPre { id: LocalId(1), stamp: None } };
        let there = Op::DeleteRemote { sid: 2, remote: RemotePre { id: RemoteId("r".into()), etag: "e".into() } };
        let check = live.round_check();
        assert!(check(&here) && check(&there));
        c.call(Call::Pause { direction: Direction::Remote, minutes: None }, 0).unwrap();
        assert!(check(&here) && !check(&there) && check(&Op::Forget(3)));
        // Not again before the round ends: what's left may depend on what
        // didn't run. The next round plans it again.
        c.call(Call::Resume { direction: Direction::Remote }, 0).unwrap();
        assert!(!check(&there));
        assert!(live.round_check()(&there));

        // Pausing by itself pauses both sides, for the next rounds too.
        c.set_auto_pause(Some(crate::autopause::Reason { key: "on a metered connection", hours: None }));
        let check = live.round_check();
        assert!(!check(&here) && !check(&there));
        assert!(c.controls().pause_local && c.controls().pause_remote);
        assert!(!c.snapshot().paused.local, "the user's pause is the user's");
        c.set_auto_pause(None);
        assert!(live.round_check()(&here));
    }

    /// Removing a synced duplicate changes both sides: a pause of either
    /// stops it.
    #[test]
    fn a_pause_of_either_side_stops_removing_a_synced_duplicate() {
        use oneup_sync::{Keep, LocalId, LocalPre, RemoteId, RemotePre};
        let remote = RemotePre { id: RemoteId("r".into()), etag: "e".into() };
        let op = Op::RemoveCopy {
            sid: Some(1),
            remote: Some(remote.clone()),
            remote_dir: None,
            remote_up: Default::default(),
            local: Some(LocalPre { id: LocalId(1), stamp: Some((1, 1)) }),
            local_dir: None,
            local_up: Default::default(),
            name: "a".into(),
            hash: [0; 20],
            keep: Keep::Remote(remote),
        };
        let c = control(None);
        let live = c.controls().live.unwrap();
        assert!(live.round_check()(&op));
        c.call(Call::Pause { direction: Direction::Local, minutes: None }, 0).unwrap();
        assert!(!live.round_check()(&op), "only this PC is paused");
    }

    /// Windows reporting a metered network mid-round (the agent sets the
    /// new policy at once): the next upload doesn't start, nor anything
    /// else changing OneDrive for the rest of that round; renames on this
    /// PC still go. The next round plans what waits under the new policy.
    #[test]
    fn a_change_of_network_mid_round_stops_its_transfers_at_the_next_operation() {
        use crate::network::{Policy, Transfer};
        use oneup_sync::{Link, LocalId, LocalPre, RemoteId, RemotePre};
        let c = control(None);
        let live = c.controls().live.unwrap();
        let upload = Op::Upload {
            local: LocalPre { id: LocalId(1), stamp: Some((5_000_000, 0)) },
            to: RemoteId("p".into()),
            parent: 1,
            name: "a".into(),
            replace: None,
            link: Link::New,
        };
        let rename = Op::MoveLocal {
            sid: 2,
            local: LocalPre { id: LocalId(2), stamp: None },
            to: LocalId(9),
            parent: 1,
            name: "b".into(),
        };
        let delete = Op::DeleteRemote { sid: 3, remote: RemotePre { id: RemoteId("r".into()), etag: "e".into() } };
        let check = live.round_check();
        assert!(check(&upload), "planned under 'everything'");
        c.set_policy(Policy { transfer: Transfer::Metadata, ..Policy::default() });
        assert!(!check(&upload) && !check(&delete), "that side stops for the rest of the round");
        assert!(check(&rename) && check(&Op::Forget(4)));
        // The next round is planned under the new policy, which already
        // left the upload out; what it planned goes.
        assert!(live.round_check()(&delete));
        // Back to a network that lets everything through: from the next
        // operation on.
        let check = live.round_check();
        c.set_policy(Policy::default());
        assert!(check(&upload));
    }

    #[test]
    fn more_data_allowed_is_handed_to_the_agent_once() {
        let c = control(None);
        assert!(c.call(Call::AllowData { mb: 0 }, 0).is_err());
        c.call(Call::AllowData { mb: 500 }, 0).unwrap();
        c.call(Call::AllowData { mb: 200 }, 0).unwrap();
        assert_eq!(c.take_allowed_mb(), 700);
        assert_eq!(c.take_allowed_mb(), 0);
    }

    #[test]
    fn the_revision_moves_only_when_the_status_changes() {
        let c = control(None);
        let rev = |c: &Control| c.call(Call::Status, 0).unwrap().rev;
        let r0 = rev(&c);
        assert_eq!(rev(&c), r0);
        c.set_activity(Activity::Idle);
        let r1 = rev(&c);
        assert!(r1 > r0);
        c.set_activity(Activity::Idle);
        c.call(Call::SyncNow, 0).unwrap();
        assert_eq!(rev(&c), r1);
        c.call(Call::Pause { direction: Direction::Remote, minutes: None }, 0).unwrap();
        assert!(rev(&c) > r1);
    }

    /// Pauses, problems and safety stops reach the event feed (for `oneup
    /// watch` and hooks) once each, and a stop's event never carries the
    /// code that approves it.
    #[test]
    fn what_happens_to_sync_itself_becomes_events_once() {
        let c = control(None);
        let kinds = |c: &Control| -> Vec<(EventKind, String)> {
            c.events.page(Some(0)).items.into_iter().map(|e| (e.kind, e.detail)).collect()
        };
        c.call(Call::Pause { direction: Direction::Remote, minutes: None }, 0).unwrap();
        c.call(Call::Pause { direction: Direction::Remote, minutes: None }, 0).unwrap();
        c.set_auto_pause(Some(crate::autopause::Reason { key: "on a metered connection", hours: None }));
        c.set_auto_pause(None);
        c.call(Call::Resume { direction: Direction::Both }, 0).unwrap();
        let p = |kind: &str| oneup_ipc::Problem {
            kind: kind.into(),
            title: "Offline".into(),
            detail: String::new(),
            action: String::new(),
            technical: String::new(),
            since_ms: 0,
            attempts: 1,
            retry_ms: None,
        };
        c.set_problem(Some(p("offline")));
        c.set_problem(Some(p("offline")));
        let breaker = Breaker {
            side: Side::Local,
            count: 300,
            recent: 0,
            synced_items: 1000,
            fingerprint: oneup_gate::Fingerprint(0x1234_5678_9abc_def0),
            kind: oneup_gate::BreakerKind::Mass,
            at_least: false,
            items: Vec::new(),
            later: Vec::new(),
            sids: Vec::new(),
        };
        c.record(1, None, std::slice::from_ref(&breaker), None, None);
        c.record(2, None, std::slice::from_ref(&breaker), None, None);
        let got = kinds(&c);
        let names: Vec<EventKind> = got.iter().map(|(k, _)| *k).collect();
        assert_eq!(
            names,
            [
                EventKind::Paused,
                EventKind::Paused,
                EventKind::Resumed,
                EventKind::Resumed,
                EventKind::Problem,
                EventKind::Resolved,
                EventKind::SafetyStop,
            ],
            "{got:?}"
        );
        assert_eq!(got[0].1, "changes to OneDrive");
        assert!(got[1].1.contains("by itself: on a metered connection"), "{got:?}");
        // Its own pause over, this PC's side goes again; the user's pause
        // of the other side stands until resumed.
        assert_eq!((got[2].1.as_str(), got[3].1.as_str()), ("changes to this PC", "changes to OneDrive"));
        assert!(got[6].1.contains("300 items on this PC"), "{got:?}");
        assert!(got.iter().all(|(_, d)| !d.contains("123456789abcdef0")), "no approval code");
    }

    #[test]
    fn only_codes_the_agent_is_waiting_for_are_accepted() {
        let c = control(None);
        let answer = |code: &str, answer| c.call(Call::Answer { code: code.into(), answer }, 0);
        // Nothing waiting: refused.
        let e = answer("00000000000000ab", Answer::Approve).unwrap_err();
        assert_eq!(e.code, NO_SUCH_BATCH);
        assert_eq!(answer("not hex", Answer::Approve).unwrap_err().code, INVALID_PARAMS);

        let breaker = Breaker {
            side: Side::Local,
            count: 30,
            recent: 0,
            synced_items: 100,
            fingerprint: Fingerprint(0xab),
            kind: oneup_gate::BreakerKind::Mass,
            items: Vec::new(),
            later: Vec::new(),
            sids: Vec::new(),
            at_least: false,
        };
        let overflow = Overflow {
            count: 2,
            bytes: 200,
            room: 100,
            fingerprint: Fingerprint(0xcd),
            outcome: OverflowOutcome::Held,
        };
        assert_eq!(c.record(5, last(), std::slice::from_ref(&breaker), Some(&overflow), None), rec(true, true));
        // The same again: nothing new to print; a quiet check keeps the
        // last changes.
        assert_eq!(c.record(9, None, std::slice::from_ref(&breaker), Some(&overflow), None), rec(false, false));
        c.set_activity(Activity::WaitingForYou);
        let s = answer("00000000000000AB", Answer::Approve).unwrap();
        assert_eq!((s.checked_ms, s.last.unwrap().done), (Some(9), 3));
        assert_eq!(c.controls().answers.approve, vec![0xab]);
        // Once the rounds used it, it approves nothing more.
        c.forget_approvals(&[0xab]);
        assert!(c.controls().answers.approve.is_empty());
        // Answered: no longer waiting, though the rounds carrying it out
        // haven't ended (nor when one under way trips it once more).
        assert!(s.safety_stops.is_empty());
        assert_eq!(s.activity, Activity::Syncing);
        c.record(9, None, std::slice::from_ref(&breaker), Some(&overflow), None);
        assert!(c.snapshot().safety_stops.is_empty());
        // A stop's code doesn't answer the overflow, nor the reverse.
        assert_eq!(answer("00000000000000ab", Answer::Discard).unwrap_err().code, NO_SUCH_BATCH);
        assert_eq!(answer("00000000000000cd", Answer::Approve).unwrap_err().code, NO_SUCH_BATCH);
        let s = answer("00000000000000cd", Answer::Discard).unwrap();
        assert_eq!(c.controls().answers.discard, vec![0xcd]);
        assert!(!s.overflow.unwrap().waiting);

        // Handled: shown once more (the outcome changed), and its code is
        // no longer accepted.
        let done = Overflow { outcome: OverflowOutcome::Decided(oneup_gate::Choice::Discard), ..overflow };
        assert_eq!(c.record(10, last(), &[], Some(&done), None), rec(false, true));
        assert_eq!(answer("00000000000000cd", Answer::Keep).unwrap_err().code, NO_SUCH_BATCH);
        // The same stop tripping again later is a new question.
        assert_eq!(c.record(11, None, std::slice::from_ref(&breaker), None, None), rec(true, false));
        assert_eq!(c.snapshot().safety_stops.len(), 1);
        // An approval the rounds never took in goes once its stop no longer
        // stands: it doesn't approve an identical list raised later.
        answer("00000000000000AB", Answer::Approve).unwrap();
        c.record(12, None, std::slice::from_ref(&breaker), None, None);
        assert_eq!(c.controls().answers.approve, vec![0xab], "kept while it stands");
        c.record(13, None, &[], None, None);
        assert!(c.controls().answers.approve.is_empty(), "dropped once it no longer stands");
    }
}
