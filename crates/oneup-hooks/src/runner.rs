//! The hooks' queue, and the threads that run them.
//!
//! Events are offered as they happen ([`Runner::offer`], called on the
//! agent's round, so it only matches and queues: no I/O). Each hook keeps
//! one pending run per item: more events for the same item replace it and
//! put it off by the hook's coalescing time (at most [`MAX_DEFER`] beyond
//! its first event), so a file saved ten times in a second starts the hook
//! once. Runs start no more often than the hook's rate allows, one at a
//! time per hook and [`WORKERS`] at a time in all, on threads in Windows'
//! background mode, as contained processes (below-normal priority, EcoQoS,
//! ended with everything they started when their time is up).
//!
//! **Bounded.** At most [`MAX_PENDING`] runs wait, whatever the burst: more
//! are dropped and counted ([`Note::Dropped`]), so a storm of events can't
//! grow memory without limit. Nothing here waits on a run: the round that
//! offered the events goes on at once.

use std::cmp::Reverse;
use std::collections::{BinaryHeap, HashMap};
use std::ffi::OsString;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Condvar, Mutex, MutexGuard};
use std::time::{Duration, Instant};

use oneup_ipc::Event;
use oneup_rules::RuleSet;

use crate::program::{self, Places};
use crate::{Fill, HookSpec, MAX_HOOKS};

/// Runs waiting, at most (across hooks).
pub const MAX_PENDING: usize = 16_384;
/// Runs at once, at most (across hooks; one per hook).
pub const WORKERS: usize = 2;
/// How long further events for an item can put its run off, beyond the
/// hook's coalescing time after the first.
pub const MAX_DEFER: Duration = Duration::from_secs(60);
/// A run's output kept for the log.
pub const KEEP_OUTPUT: usize = 8 * 1024;
/// An event's detail, at most (characters).
const MAX_DETAIL: usize = 300;

/// What the runner works with; given again whenever it changes
/// ([`Runner::configure`]).
#[derive(Debug, Clone, Default)]
pub struct Settings {
    pub hooks: Vec<HookSpec>,
    /// Nothing starts, and nothing is queued, while paused.
    pub paused: bool,
    /// The synced folder on this PC, for `{path}`.
    pub root: PathBuf,
    pub places: Places,
}

/// How a program ended.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Finished {
    /// `None` if it was ended (its time was up) or couldn't be read.
    pub exit_code: Option<i32>,
    pub timed_out: bool,
    pub output: Vec<u8>,
    pub truncated: bool,
}

/// Starts a hook's program and waits for it (or its timeout).
pub trait Launch: Send + Sync {
    fn launch(&self, program: &Path, args: &[OsString], timeout: Duration) -> std::io::Result<Finished>;
}

/// Starts programs contained (`oneup_sys::contained`): in their own
/// folder, without OneUpDrive's environment variables (`ONEUP_*`, which
/// can hold a vault's recovery key), output kept up to [`KEEP_OUTPUT`].
pub struct Contained;

impl Launch for Contained {
    #[cfg(windows)]
    fn launch(&self, program: &Path, args: &[OsString], timeout: Duration) -> std::io::Result<Finished> {
        let cwd = program.parent().unwrap_or(program);
        let drop_env = |n: &std::ffi::OsStr| n.to_string_lossy().to_ascii_uppercase().starts_with("ONEUP_");
        let f = oneup_sys::contained::run(&oneup_sys::contained::Spec {
            program,
            args,
            cwd,
            drop_env: &drop_env,
            timeout,
            keep_output: KEEP_OUTPUT,
        })?;
        Ok(Finished { exit_code: f.exit_code, timed_out: f.timed_out, output: f.output, truncated: f.truncated })
    }

    #[cfg(not(windows))]
    fn launch(&self, _: &Path, _: &[OsString], _: Duration) -> std::io::Result<Finished> {
        Err(std::io::Error::other("hooks run on Windows only"))
    }
}

/// A run that happened.
#[derive(Debug, Clone)]
pub struct Ran {
    pub hook: String,
    pub event: Event,
    /// Events for the item folded into this run.
    pub merged: u32,
    pub took: Duration,
    /// How it ended, or why it couldn't start.
    pub result: Result<Finished, String>,
}

/// What the runner reports, for the log and the activity log.
#[derive(Debug, Clone)]
pub enum Note {
    /// A hook won't run: its settings, or where its program is. Said once
    /// per reason.
    Refused {
        hook: String,
        why: String,
    },
    /// Runs dropped because [`MAX_PENDING`] were waiting already.
    Dropped {
        count: u64,
    },
    Ran(Ran),
}

/// The hooks' queue and its threads. Dropping it stops starting runs;
/// runs under way end by themselves (or at their timeout).
pub struct Runner {
    shared: Arc<Shared>,
}

struct Shared {
    state: Mutex<State>,
    wake: Condvar,
    launch: Arc<dyn Launch>,
    notes: Box<dyn Fn(Note) + Send + Sync>,
}

#[derive(Default)]
struct State {
    hooks: Vec<Slot>,
    paused: bool,
    root: PathBuf,
    places: Arc<Places>,
    /// Runs waiting, across hooks.
    pending: usize,
    /// Orders the queue's entries (newer replaces older).
    ver: u64,
    /// The hook looked at first next time, so one busy hook can't starve
    /// the others.
    turn: usize,
    running: usize,
    stopping: bool,
}

struct Slot {
    spec: HookSpec,
    rules: Option<RuleSet>,
    /// The program to start, or why the hook can't run.
    ok: Result<PathBuf, String>,
    /// The reason last reported, so each is said once.
    said: Option<String>,
    /// One pending run per item (by folded path), and when each is due.
    queue: HashMap<String, Pending>,
    due: BinaryHeap<Reverse<(Instant, u64, String)>>,
    running: bool,
    tokens: f64,
    filled: Instant,
}

struct Pending {
    event: Event,
    first: Instant,
    due: Instant,
    ver: u64,
    merged: u32,
}

/// A run taken off the queue.
struct Job {
    spec: HookSpec,
    event: Event,
    merged: u32,
    root: PathBuf,
    places: Arc<Places>,
}

impl Slot {
    fn new(spec: HookSpec, ok: Result<PathBuf, String>, now: Instant) -> Self {
        let rules = (!spec.paths.is_empty()).then(|| RuleSet::new(&[] as &[String], &spec.paths).unwrap_or_default());
        let tokens = Self::capacity(&spec);
        Self {
            spec,
            rules,
            ok,
            said: None,
            queue: HashMap::new(),
            due: BinaryHeap::new(),
            running: false,
            tokens,
            filled: now,
        }
    }

    /// A tenth of a minute's worth may start at once.
    fn capacity(spec: &HookSpec) -> f64 {
        (f64::from(spec.per_minute) / 6.0).max(1.0)
    }

    fn live(&self) -> bool {
        self.spec.enabled && self.ok.is_ok()
    }

    fn refill(&mut self, now: Instant) {
        let rate = f64::from(self.spec.per_minute) / 60.0;
        let elapsed = now.saturating_duration_since(self.filled).as_secs_f64();
        self.tokens = (self.tokens + elapsed * rate).min(Self::capacity(&self.spec));
        self.filled = now;
    }

    /// When the next run may start, as far as the rate goes.
    fn allowed_at(&self, now: Instant) -> Instant {
        if self.tokens >= 1.0 {
            return now;
        }
        let rate = f64::from(self.spec.per_minute) / 60.0;
        now + Duration::from_secs_f64((1.0 - self.tokens) / rate)
    }

    /// The earliest pending run, dropping replaced entries on the way.
    fn next_due(&mut self) -> Option<Instant> {
        while let Some(Reverse((due, ver, key))) = self.due.peek() {
            if self.queue.get(key).is_some_and(|p| p.ver == *ver) {
                return Some(*due);
            }
            self.due.pop();
        }
        None
    }

    /// Rebuilds the due list from the queue once replaced entries are most
    /// of it (bounds it at a few times the queue).
    fn compact(&mut self) {
        if self.due.len() > 4 * self.queue.len() + 64 {
            self.due = self.queue.iter().map(|(k, p)| Reverse((p.due, p.ver, k.clone()))).collect();
        }
    }

    fn clear(&mut self) -> usize {
        let n = self.queue.len();
        self.queue.clear();
        self.due.clear();
        n
    }
}

/// Whether a hook may run: its settings, its program (where it is) and the
/// files its arguments name. The program to start if so.
pub fn check(spec: &HookSpec, places: &Places) -> Result<PathBuf, String> {
    spec.check()?;
    let program = program::check(&spec.program, places).map_err(|r| r.0)?;
    program::check_args(&spec.args, places).map_err(|r| r.0)?;
    Ok(program)
}

/// What runs are coalesced by: the item, or for events about sync itself
/// their kind (a pause and a problem a moment apart are two runs).
fn key(e: &Event) -> String {
    if e.path.is_empty() { format!("#{}", e.kind.name()) } else { oneup_sync::fold(&e.path) }
}

fn trimmed(e: &Event) -> Event {
    let mut e = e.clone();
    if e.detail.chars().count() > MAX_DETAIL {
        e.detail = e.detail.chars().take(MAX_DETAIL).collect::<String>() + "…";
    }
    e
}

impl Shared {
    fn lock(&self) -> MutexGuard<'_, State> {
        // Nothing in here is left half-changed by a panic that matters.
        self.state.lock().unwrap_or_else(std::sync::PoisonError::into_inner)
    }
}

impl State {
    /// The next run that may start now, or when one may (`None`: none
    /// waits).
    fn pick(&mut self, now: Instant) -> Result<Job, Option<Instant>> {
        let n = self.hooks.len();
        let mut next: Option<Instant> = None;
        for k in 0..n {
            let i = (self.turn + k) % n;
            let slot = &mut self.hooks[i];
            if slot.running || !slot.live() {
                continue;
            }
            let Some(due) = slot.next_due() else { continue };
            slot.refill(now);
            let at = due.max(slot.allowed_at(now));
            if at > now {
                next = Some(next.map_or(at, |x| x.min(at)));
                continue;
            }
            let Some(Reverse((_, _, key))) = slot.due.pop() else { continue };
            let Some(p) = slot.queue.remove(&key) else { continue };
            slot.tokens -= 1.0;
            slot.running = true;
            let job = Job {
                spec: slot.spec.clone(),
                event: p.event,
                merged: p.merged,
                root: self.root.clone(),
                places: Arc::clone(&self.places),
            };
            self.pending -= 1;
            self.running += 1;
            self.turn = i + 1;
            return Ok(job);
        }
        Err(next)
    }
}

impl Runner {
    /// Starts the threads, with `settings`. `notes` hears of every run,
    /// refusal and drop (on the runner's threads, or the caller's for
    /// [`Runner::configure`] and [`Runner::offer`]); it must not call back
    /// into the runner.
    pub fn start(settings: Settings, launch: Arc<dyn Launch>, notes: Box<dyn Fn(Note) + Send + Sync>) -> Self {
        let shared = Arc::new(Shared { state: Mutex::new(State::default()), wake: Condvar::new(), launch, notes });
        let runner = Self { shared };
        runner.configure(settings);
        for n in 0..WORKERS {
            let shared = Arc::clone(&runner.shared);
            let spawned = std::thread::Builder::new().name(format!("hooks-{n}")).spawn(move || worker(&shared));
            if let Err(e) = spawned {
                tracing::warn!(error = %e, "can't start a thread for hooks");
            }
        }
        runner
    }

    /// New settings (`config.toml` changed, or the synced folders did).
    /// A hook whose settings are unchanged keeps its queue and rate; the
    /// others start afresh. Each hook's program is checked again.
    pub fn configure(&self, s: Settings) {
        let now = Instant::now();
        let places = Arc::new(s.places);
        let mut notes = Vec::new();
        {
            let mut st = self.shared.lock();
            let mut old = std::mem::take(&mut st.hooks);
            let mut specs = s.hooks;
            for extra in specs.drain(MAX_HOOKS.min(specs.len())..) {
                let why = format!("more than {MAX_HOOKS} hooks; this one and those after it don't run");
                notes.push(Note::Refused { hook: extra.name, why });
            }
            for spec in specs {
                let ok = check(&spec, &places);
                let mut slot = match old.iter().position(|o| o.spec == spec) {
                    Some(i) => {
                        let mut o = old.swap_remove(i);
                        o.ok = ok;
                        o
                    }
                    None => Slot::new(spec, ok, now),
                };
                match &slot.ok {
                    Err(why) if slot.said.as_ref() != Some(why) => {
                        notes.push(Note::Refused { hook: slot.spec.name.clone(), why: why.clone() });
                        slot.said = Some(why.clone());
                    }
                    Err(_) => {}
                    Ok(_) => slot.said = None,
                }
                if !slot.live() || s.paused {
                    slot.clear();
                }
                st.hooks.push(slot);
            }
            st.pending = st.hooks.iter().map(|h| h.queue.len()).sum();
            st.paused = s.paused;
            st.root = s.root;
            st.places = places;
        }
        self.shared.wake.notify_all();
        for n in notes {
            (self.shared.notes)(n);
        }
    }

    /// Queues the runs `events` start. Never waits on a run.
    pub fn offer(&self, events: &[Event]) {
        if events.is_empty() {
            return;
        }
        let now = Instant::now();
        let mut dropped = 0u64;
        let mut added = false;
        {
            let mut st = self.shared.lock();
            if st.paused || st.stopping {
                return;
            }
            let State { hooks, pending, ver, .. } = &mut *st;
            for e in events {
                for slot in hooks.iter_mut() {
                    if !slot.live() || !slot.spec.matches(slot.rules.as_ref(), e) {
                        continue;
                    }
                    let key = key(e);
                    *ver += 1;
                    let wait = slot.spec.coalesce;
                    match slot.queue.get_mut(&key) {
                        Some(p) => {
                            p.event = trimmed(e);
                            p.merged += 1;
                            p.ver = *ver;
                            p.due = (now + wait).min(p.first + wait + MAX_DEFER);
                            slot.due.push(Reverse((p.due, *ver, key)));
                            slot.compact();
                        }
                        None if *pending >= MAX_PENDING => {
                            dropped += 1;
                            continue;
                        }
                        None => {
                            let due = now + wait;
                            slot.queue.insert(
                                key.clone(),
                                Pending { event: trimmed(e), first: now, due, ver: *ver, merged: 0 },
                            );
                            slot.due.push(Reverse((due, *ver, key)));
                            *pending += 1;
                        }
                    }
                    added = true;
                }
            }
        }
        if added {
            self.shared.wake.notify_all();
        }
        if dropped > 0 {
            (self.shared.notes)(Note::Dropped { count: dropped });
        }
    }

    /// Runs waiting, and under way.
    pub fn backlog(&self) -> (usize, usize) {
        let st = self.shared.lock();
        (st.pending, st.running)
    }

    /// Waits until no run waits or is under way, or `timeout` passes;
    /// whether it got there.
    pub fn wait_idle(&self, timeout: Duration) -> bool {
        let end = Instant::now() + timeout;
        let mut st = self.shared.lock();
        while st.pending > 0 || st.running > 0 {
            let now = Instant::now();
            if now >= end {
                return false;
            }
            st = self.shared.wake.wait_timeout(st, end - now).unwrap_or_else(std::sync::PoisonError::into_inner).0;
        }
        true
    }
}

impl Drop for Runner {
    fn drop(&mut self) {
        self.shared.lock().stopping = true;
        self.shared.wake.notify_all();
    }
}

fn worker(shared: &Shared) {
    #[cfg(windows)]
    if let Err(e) = oneup_sys::priority::set_thread_background() {
        tracing::debug!(error = %e, "hooks: background mode for the thread");
    }
    loop {
        let job = {
            let mut st = shared.lock();
            loop {
                if st.stopping {
                    return;
                }
                let now = Instant::now();
                match st.pick(now) {
                    Ok(job) => break job,
                    Err(Some(at)) => {
                        st =
                            shared.wake.wait_timeout(st, at - now).unwrap_or_else(std::sync::PoisonError::into_inner).0;
                    }
                    Err(None) => st = shared.wake.wait(st).unwrap_or_else(std::sync::PoisonError::into_inner),
                }
            }
        };
        let name = job.spec.name.clone();
        let started = Instant::now();
        // Checked again right before the start: the program, or a folder on
        // its way, may have moved into a synced folder since it was loaded.
        let checked = check(&job.spec, &job.places);
        let result = match &checked {
            Ok(program) => {
                let args = Fill { event: &job.event, root: &job.root }.args(&job.spec.args);
                shared.launch.launch(program, &args, job.spec.timeout).map_err(|e| format!("couldn't start: {e}"))
            }
            Err(why) => Err(why.clone()),
        };
        let took = started.elapsed();
        let refused = checked.err();
        {
            let mut st = shared.lock();
            let State { hooks, pending, running, .. } = &mut *st;
            *running -= 1;
            if let Some(slot) = hooks.iter_mut().find(|s| s.spec.name == name) {
                slot.running = false;
                if let Some(why) = &refused {
                    slot.ok = Err(why.clone());
                    slot.said = Some(why.clone());
                    *pending -= slot.clear();
                }
            }
        }
        shared.wake.notify_all();
        match refused {
            Some(why) => (shared.notes)(Note::Refused { hook: name, why }),
            None => (shared.notes)(Note::Ran(Ran { hook: name, event: job.event, merged: job.merged, took, result })),
        }
    }
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used)]
    use super::*;
    use oneup_ipc::EventKind;

    /// Records each start; takes `delay` to "run".
    struct Fake {
        started: Mutex<Vec<(PathBuf, Vec<OsString>, Instant)>>,
        delay: Duration,
    }

    impl Launch for Fake {
        fn launch(&self, program: &Path, args: &[OsString], _: Duration) -> std::io::Result<Finished> {
            self.started.lock().unwrap().push((program.to_path_buf(), args.to_vec(), Instant::now()));
            std::thread::sleep(self.delay);
            Ok(Finished { exit_code: Some(0), timed_out: false, output: Vec::new(), truncated: false })
        }
    }

    fn fake(delay: Duration) -> Arc<Fake> {
        Arc::new(Fake { started: Mutex::new(Vec::new()), delay })
    }

    fn cmd() -> String {
        std::path::PathBuf::from(std::env::var_os("SystemRoot").unwrap())
            .join("System32")
            .join("where.exe")
            .to_string_lossy()
            .into_owned()
    }

    fn hook(name: &str, coalesce_ms: u64, per_minute: u32) -> HookSpec {
        HookSpec {
            coalesce: Duration::from_millis(coalesce_ms),
            per_minute,
            ..HookSpec::new(name, &[EventKind::Synced], &cmd(), &["{relpath}"])
        }
    }

    fn synced(path: &str) -> Event {
        Event {
            seq: 0,
            at_ms: 0,
            kind: EventKind::Synced,
            path: path.into(),
            action: "upload".into(),
            detail: String::new(),
        }
    }

    type Notes = Arc<Mutex<Vec<Note>>>;

    fn runner(hooks: Vec<HookSpec>, launch: Arc<Fake>) -> (Runner, Notes) {
        let notes: Notes = Arc::default();
        let sink = Arc::clone(&notes);
        let s = Settings { hooks, root: PathBuf::from(r"C:\Sync"), ..Settings::default() };
        (Runner::start(s, launch, Box::new(move |n| sink.lock().unwrap().push(n))), notes)
    }

    fn started_args(f: &Fake) -> Vec<String> {
        let mut v: Vec<String> =
            f.started.lock().unwrap().iter().map(|(_, a, _)| a[0].to_string_lossy().into_owned()).collect();
        v.sort();
        v
    }

    #[test]
    fn a_burst_of_changes_to_one_item_starts_the_hook_once_with_the_latest() {
        let f = fake(Duration::ZERO);
        let (r, _) = runner(vec![hook("h", 200, 6000)], Arc::clone(&f));
        for _ in 0..50 {
            r.offer(&[synced("/a.txt"), synced("/B.txt")]);
        }
        r.offer(&[synced("/b.TXT")]);
        assert!(r.wait_idle(Duration::from_secs(10)));
        // Names compare as Windows compares them; the latest event's path.
        assert_eq!(started_args(&f), ["a.txt", "b.TXT"]);
    }

    #[test]
    fn events_about_sync_itself_coalesce_by_kind() {
        let f = fake(Duration::ZERO);
        let spec = HookSpec { on: vec![EventKind::Paused, EventKind::Problem], ..hook("h", 200, 6000) };
        let (r, _) = runner(vec![spec], Arc::clone(&f));
        let about = |kind| Event { kind, path: String::new(), ..synced("") };
        r.offer(&[about(EventKind::Paused), about(EventKind::Problem), about(EventKind::Paused)]);
        assert!(r.wait_idle(Duration::from_secs(10)));
        assert_eq!(f.started.lock().unwrap().len(), 2);
    }

    #[test]
    fn runs_keep_to_the_hooks_rate() {
        let f = fake(Duration::ZERO);
        // 120 a minute: 20 at once, then one every half second.
        let (r, _) = runner(vec![hook("h", 0, 120)], Arc::clone(&f));
        let events: Vec<Event> = (0..23).map(|i| synced(&format!("/{i}.txt"))).collect();
        let t = Instant::now();
        r.offer(&events);
        assert!(r.wait_idle(Duration::from_secs(10)));
        let took = t.elapsed();
        assert_eq!(f.started.lock().unwrap().len(), 23);
        assert!(took >= Duration::from_millis(1400) && took < Duration::from_secs(5), "{took:?}");
    }

    #[test]
    fn the_queue_is_bounded_and_drops_are_reported() {
        let f = fake(Duration::ZERO);
        let (r, notes) = runner(vec![hook("h", 60_000, 1)], Arc::clone(&f));
        let events: Vec<Event> = (0..MAX_PENDING + 10).map(|i| synced(&format!("/{i}.txt"))).collect();
        r.offer(&events);
        assert_eq!(r.backlog().0, MAX_PENDING);
        let dropped: u64 =
            notes.lock().unwrap().iter().map(|n| if let Note::Dropped { count } = n { *count } else { 0 }).sum();
        assert_eq!(dropped, 10);
        // More events for items already waiting take no room.
        r.offer(&events[..5]);
        assert_eq!(r.backlog().0, MAX_PENDING);
    }

    #[test]
    fn paused_or_disabled_hooks_queue_nothing_and_pausing_clears_the_queue() {
        let f = fake(Duration::ZERO);
        let (r, _) = runner(vec![hook("h", 60_000, 60)], Arc::clone(&f));
        r.offer(&[synced("/a.txt")]);
        assert_eq!(r.backlog().0, 1);
        let mut s = Settings { hooks: vec![hook("h", 60_000, 60)], paused: true, ..Settings::default() };
        r.configure(s.clone());
        assert_eq!(r.backlog().0, 0);
        r.offer(&[synced("/a.txt")]);
        assert_eq!(r.backlog().0, 0, "paused");
        s.paused = false;
        s.hooks[0].enabled = false;
        r.configure(s);
        r.offer(&[synced("/a.txt")]);
        assert_eq!(r.backlog().0, 0, "disabled");
        assert!(f.started.lock().unwrap().is_empty());
    }

    /// The program is checked when the hook is loaded and again right
    /// before each start: one that a synced folder holds by then never
    /// starts, and the hook says why once.
    #[test]
    fn a_program_that_moved_into_a_synced_folder_is_refused_at_its_next_start() {
        let dir = std::env::temp_dir().join(format!("oneup-hooks-moved-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        let tools = dir.join("Tools");
        std::fs::create_dir_all(&tools).unwrap();
        let exe = tools.join("run.exe");
        std::fs::write(&exe, b"MZ").unwrap();
        let f = fake(Duration::ZERO);
        let spec = HookSpec { coalesce: Duration::from_millis(300), ..hook("h", 0, 60) };
        let spec = HookSpec { program: exe.to_string_lossy().into_owned(), ..spec };
        let (r, notes) = runner(vec![spec.clone()], Arc::clone(&f));
        r.offer(&[synced("/a.txt")]);
        // Before it's due, its folder becomes a synced one (the places
        // the runner knows are the ones the agent found at load).
        let synced_places = Places::new(vec![(tools.clone(), "the folder pair Tools".into())]);
        {
            let mut st = r.shared.lock();
            st.places = Arc::new(synced_places);
        }
        assert!(r.wait_idle(Duration::from_secs(10)));
        assert!(f.started.lock().unwrap().is_empty(), "never started");
        let refused: Vec<String> = notes
            .lock()
            .unwrap()
            .iter()
            .filter_map(|n| if let Note::Refused { why, .. } = n { Some(why.clone()) } else { None })
            .collect();
        assert_eq!(refused.len(), 1, "{refused:?}");
        assert!(refused[0].contains("the folder pair Tools") && refused[0].contains("never runs as a hook"));
        // And at load, with the same places: refused at once, said once.
        r.configure(Settings {
            hooks: vec![spec],
            places: Places::new(vec![(tools, "the folder pair Tools".into())]),
            ..Settings::default()
        });
        r.offer(&[synced("/b.txt")]);
        assert_eq!(r.backlog().0, 0);
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn hooks_run_one_at_a_time_each_and_in_turn() {
        let f = fake(Duration::from_millis(30));
        let (r, _) = runner(vec![hook("a", 0, 6000), hook("b", 0, 6000)], Arc::clone(&f));
        let events: Vec<Event> = (0..10).map(|i| synced(&format!("/{i}.txt"))).collect();
        let t = Instant::now();
        r.offer(&events);
        assert!(r.wait_idle(Duration::from_secs(10)));
        assert_eq!(f.started.lock().unwrap().len(), 20);
        // Two at once (one per hook): about 10 runs' time, not 20.
        assert!(t.elapsed() < Duration::from_millis(550), "{:?}", t.elapsed());
    }
}
