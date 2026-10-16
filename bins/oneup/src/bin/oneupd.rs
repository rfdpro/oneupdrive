//! `oneupd`: OneUpDrive's background agent.
//!
//! Keeps the folder pair that `oneup sync` recorded in a data folder in sync
//! for as long as it runs, doing work only when something changed:
//!
//! * **This PC:** a thread blocks in the kernel on the volume's change
//!   journal (zero CPU while nothing happens). Any change on the
//!   volume wakes it; the agent then reads the journal, which marks the
//!   synced folders that actually changed (`oneup_store::dirty`), and only
//!   if some did does it run a round. A busy disk elsewhere costs a journal
//!   read, never a OneDrive request.
//! * **OneDrive:** a push channel (`oneup_graph::push`) reports that
//!   something changed, about 16 s after it did; the agent
//!   then reads the delta feed. The feed is also read on a timer: every
//!   `--poll-seconds` while push is down or off, every 10 minutes while it
//!   works (a safety net for a missed notification).
//!
//! Rounds are the same as `oneup sync`'s (shared code), except that after
//! the start-up pass they plan only the folders marked dirty, so their cost
//! grows with the changes, not with the tree.
//!
//! **Control** (`oneup_app::control`, over `oneup_ipc`'s pipe): `oneup
//! status`, `pause`/`resume` (both directions or one, optionally timed; kept
//! across restarts), `approve`/`keep`/`discard`, and `sync-now`, all while
//! the agent keeps running. While both directions are paused, no rounds run
//! at all; the journal is still read, so nothing is missed.
//!
//! **Safety.** Everything `oneup sync` guarantees holds (same gate, same
//! effects layer). In addition, the safety stops count what the agent did
//! in the last [`WINDOW`], so a mass deletion trickling in as many small
//! rounds still trips them ([`oneup_gate::Recent`]). A tripped stop changes
//! nothing and waits for the user's `oneup approve CODE`; later rounds
//! change nothing either while it stands. Removed files that don't fit in
//! the version store are not a stop: they wait for `oneup keep`/`discard`
//! while everything else syncs. Each is shown once. A sync folder that
//! fails its identity check ends the agent. Network and service errors are
//! retried with a growing pause.
//!
//! While [`oneup::setup::PRE_RELEASE`] is on, test-account data folders
//! only, like `oneup sync`. One process per data folder (`sync.lock`). Ctrl+C stops after the current
//! round; a second Ctrl+C stops at once.

use std::collections::VecDeque;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering::Relaxed};
use std::time::{Duration, Instant};

use anyhow::{Context, Result};
use clap::Parser;
use oneup_app::autopause;
use oneup_app::busy::Busy;
use oneup_app::control::{Control, now_ms};
use oneup_app::problem;
use oneup_app::setup::{self, DirLock, KEY_PRESET};
use oneup_app::sync::{self, Answers, Asker, End, Halted, Scope};
use oneup_gate::Recent;
use oneup_graph::{Aimd, Graph};
use oneup_ipc::{Activity, LastRun};
use oneup_store::Store;
use tokio::sync::Notify;

/// How far back the safety stops look (see the module docs).
const WINDOW: Duration = Duration::from_secs(15 * 60);
/// Rounds per wake-up before the agent pauses briefly and starts over.
const MAX_ROUNDS: usize = 20;
/// After a journal wake-up, how long to let a burst of changes (a program
/// saving many files) finish before reading it.
const SETTLE: Duration = Duration::from_secs(1);
/// The least time between two journal wake-ups. Bounds the cost on a busy
/// disk; changes are never lost meanwhile (the journal keeps them).
const MIN_WAKE_GAP: Duration = Duration::from_millis(500);
/// With the program tracker on, the least time between two reads of the
/// journal: the tracker must ask who wrote a file before the writer closes
/// it, so it can't wait for the next wake-up.
const TRACK_GAP: Duration = Duration::from_millis(50);
/// The journal watcher skips ahead when this many bytes of records are
/// waiting (see `watch_journal`).
const SKIP_AHEAD: i64 = 4 << 20;
/// Pauses before retrying skipped or failed operations, and after errors.
const RETRY_FIRST: Duration = Duration::from_secs(30);
/// Converting files already here into placeholders (`Engine::convert_leftovers`):
/// at most this long at a time, with this pause between slices.
const CONVERT_SLICE: Duration = Duration::from_secs(20);
const CONVERT_GAP: Duration = Duration::from_secs(5);
const RETRY_MAX: Duration = Duration::from_secs(15 * 60);
const ERROR_FIRST: Duration = Duration::from_secs(60);
const ERROR_MAX: Duration = Duration::from_secs(30 * 60);
/// After the database was busy (another program held it too long):
/// nothing went wrong, so the agent tries again soon, without backing off.
const BUSY_PAUSE: Duration = Duration::from_secs(10);
/// With the push channel live, the timed OneDrive check is only a safety
/// net against a missed notification.
const PUSH_POLL: Duration = Duration::from_secs(10 * 60);
/// After a notification, how long to let related ones arrive.
const PUSH_SETTLE: Duration = Duration::from_secs(2);
/// Pauses before reconnecting the push channel.
const PUSH_RETRY_FIRST: Duration = Duration::from_secs(5);
const PUSH_RETRY_MAX: Duration = Duration::from_secs(10 * 60);
/// While fully paused: how long a wait may be at most (a timed pause ends
/// earlier). Nothing is checked meanwhile; this only bounds the wait.
const PAUSED_WAIT: Duration = Duration::from_secs(60 * 60);
/// SQLite page cache (the "balanced" memory profile).

#[derive(Parser)]
#[command(name = "oneupd", version, about = "OneUpDrive background agent")]
struct Args {
    /// The data folder whose folder pair to keep in sync (a test account's;
    /// pair it once with `oneup sync <folder> --remote <path>` first)
    #[arg(long)]
    data_dir: PathBuf,
    /// App registration client ID [default: the one the test account was signed in with]
    #[arg(long, env = "ONEUP_CLIENT_ID", hide_env_values = true)]
    client_id: Option<String>,
    /// How often to check OneDrive for changes, in seconds (at least 15).
    /// While push notifications work, OneDrive is checked when it reports a
    /// change, and on this timer at most every 10 minutes
    #[arg(long, default_value_t = 60)]
    poll_seconds: u64,
    /// Don't use push notifications; only check OneDrive on the timer
    #[arg(long)]
    no_push: bool,
    /// Go ahead with a batch that triggered a safety stop (the code it
    /// printed; `oneup approve CODE` does the same while oneupd runs)
    #[arg(long, value_name = "CODE", value_parser = parse_code)]
    approve: Vec<u64>,
    /// Keep a batch of removed files in the version store although it
    /// doesn't fit (`oneup keep CODE` while oneupd runs)
    #[arg(long, value_name = "CODE", value_parser = parse_code)]
    keep: Vec<u64>,
    /// Remove a batch of files that doesn't fit in the version store from
    /// this PC without a local copy (`oneup discard CODE` while oneupd runs)
    #[arg(long, value_name = "CODE", value_parser = parse_code)]
    discard: Vec<u64>,
    /// More log output (-v, -vv)
    #[arg(short, long, action = clap::ArgAction::Count)]
    verbose: u8,
    /// A vault pair's data folder: read the vault's keys from standard
    /// input (how `oneup vault unlock` hands them over; never from disk)
    #[arg(long, hide = true)]
    key_stdin: bool,
}

fn parse_code(s: &str) -> Result<u64, String> {
    u64::from_str_radix(s.trim(), 16).map_err(|_| "expected the 16-character code the safety stop printed".into())
}

/// Set by Ctrl+C; the agent checks it between rounds.
struct Stop {
    flag: AtomicBool,
    notify: Notify,
}

impl Stop {
    fn requested(&self) -> bool {
        self.flag.load(Relaxed)
    }

    /// Sleeps for `d`, or less if a stop is requested meanwhile.
    async fn sleep(&self, d: Duration) {
        if !self.requested() {
            tokio::select! {
                _ = tokio::time::sleep(d) => {}
                _ = self.notify.notified() => {}
            }
        }
    }
}

// Two workers, not one per core (often 16 or more): one round runs
// at a time and it's I/O-bound, but rounds do blocking work inline, so the
// second worker keeps control calls and the push channel answering while a
// round runs. Blocking-pool threads (rescans, opening the store) end after
// 10 s idle.
#[tokio::main(flavor = "multi_thread", worker_threads = 2)]
async fn main() -> Result<()> {
    oneup_app::setup::tls();
    let args = Args::parse();
    let level = match args.verbose {
        0 => "info",
        _ => "debug",
    };
    tracing_subscriber::fmt()
        .with_env_filter(tracing_subscriber::EnvFilter::try_from_default_env().unwrap_or_else(|_| level.into()))
        .with_writer(std::io::stderr)
        .init();
    let dir = args.data_dir.clone();
    let result = run(args).await;
    // Why it stopped, for the tray and the settings window (not when
    // another agent holds the folder: that one is running).
    if let Err(e) = &result
        && LOCKED.load(Relaxed)
    {
        let p = problem::describe(e, None, now_ms(), None);
        if let Err(w) = problem::save_stopped(&dir, &p) {
            tracing::warn!(error = %w, "couldn't save why oneupd stopped");
        }
    }
    result
}

/// Set once this agent holds its data folder's lock.
static LOCKED: AtomicBool = AtomicBool::new(false);

async fn run(args: Args) -> Result<()> {
    // ---- checks that no retry can fix
    let dir = &args.data_dir;
    setup::require_test_account(dir)?;
    let _lock = DirLock::acquire(dir)?;
    LOCKED.store(true, Relaxed);
    problem::clear_stopped(dir);
    // So Explorer's commands can find this data folder from a file's path.
    oneup_app::datafolders::record(dir);
    // A vault pair (oneup_app::vaultpair) syncs only while unlocked.
    let key = if oneup_app::vaultpair::is_vault_dir(dir) {
        Some(if args.key_stdin {
            oneup_app::vaultpair::key_from_stdin()?
        } else {
            oneup_app::vaultpair::load_key(dir)?
                .context("this vault is locked; `oneup vault unlock` starts it with its password")?
        })
    } else {
        None
    };
    let http = setup::http()?;
    let auth = setup::auth(dir, args.client_id.as_deref(), &http)?;
    if !auth.is_signed_in()? {
        return Err(oneup_graph::Error::NotSignedIn.into());
    }
    // An extra folder pair lists only its own folder (oneup_app::pairs).
    let graph = Arc::new(oneup_app::pairs::scoped(dir, Graph::new(http, auth, Aimd::new(4, 8)))?);
    // How it uses the PC (config.toml [agent]): memory and priority are
    // read once here; the bandwidth caps every round.
    let settings = oneup_app::config::kept(dir, oneup_app::config::load_agent(dir))?;
    let priority_set = match oneup_sys::priority::set_background(settings.background) {
        Ok(()) => true,
        Err(e) => {
            tracing::warn!(error = %e, "couldn't set the agent's priority");
            false
        }
    };
    let cache_kib = settings.cache_kib();
    let memory = Memory { cache_kib, map_bytes: Memory::map_bytes(cache_kib, !settings.background && priority_set) };
    // Before the hydration service and the program tracker open their own
    // connections.
    if settings.compact_database {
        sync::compact(&mut setup::open_store(dir, cache_kib)?, dir);
    }
    let control = {
        let store = setup::open_store(dir, cache_kib)?;
        let pair = sync::pairing(&store)?;
        let local = pair.local.display().to_string();
        let local = local.strip_prefix(r"\\?\").unwrap_or(&local).to_string();
        println!("oneupd: keeping {local} in sync with OneDrive {} (Ctrl+C to stop)", pair.remote);
        let answers = Answers {
            approve: args.approve.clone(),
            keep: args.keep.clone(),
            discard: args.discard.clone(),
            ..Answers::default()
        };
        Arc::new(Control::new(Some(dir), local, pair.remote, answers))
    };
    let p = control.controls();
    if p.pause_local || p.pause_remote {
        println!("oneupd: {} (`oneup resume` to continue)", describe_pause(p.pause_local, p.pause_remote));
    }
    // Control from `oneup status/pause/resume/approve/...`. Without it the
    // agent still syncs; answers then need a restart with the flags.
    let _ipc = match oneup_ipc::Server::start(dir, Arc::clone(&control) as Arc<dyn oneup_ipc::Handler>) {
        Ok(s) => Some(s),
        Err(e) => {
            tracing::warn!(error = %e, "can't open the control pipe; `oneup status/pause/approve` won't reach this agent");
            None
        }
    };

    let stop = Arc::new(Stop { flag: AtomicBool::new(false), notify: Notify::new() });
    {
        // `oneup stop` works like Ctrl+C: after the current round.
        let (stop, control) = (Arc::clone(&stop), Arc::clone(&control));
        tokio::spawn(async move {
            control.stop.notified().await;
            println!("oneupd: stopping after the current round (asked by `oneup stop`)");
            stop.flag.store(true, Relaxed);
            stop.notify.notify_one();
        });
    }
    // Windows closing the agent (its console closed, the user signing out,
    // shutdown, Ctrl+Break): said in the log, and a clean stop where there's
    // time for one. Without this, Windows ended it without a word, and a
    // vanished agent left nothing to go on.
    {
        use tokio::signal::windows as sig;
        let stop = Arc::clone(&stop);
        tokio::spawn(async move {
            let (Ok(mut brk), Ok(mut close), Ok(mut logoff), Ok(mut shutdown)) =
                (sig::ctrl_break(), sig::ctrl_close(), sig::ctrl_logoff(), sig::ctrl_shutdown())
            else {
                return;
            };
            let why = tokio::select! {
                _ = brk.recv() => "Ctrl+Break",
                _ = close.recv() => "its console was closed",
                _ = logoff.recv() => "the user is signing out",
                _ = shutdown.recv() => "Windows is shutting down",
            };
            tracing::warn!("Windows asked oneupd to stop: {why}");
            println!("oneupd: stopping: {why}");
            stop.flag.store(true, Relaxed);
            stop.notify.notify_one();
        });
    }
    {
        let stop = Arc::clone(&stop);
        tokio::spawn(async move {
            if tokio::signal::ctrl_c().await.is_ok() {
                println!("oneupd: stopping after the current round (Ctrl+C again to stop at once)");
                stop.flag.store(true, Relaxed);
                stop.notify.notify_one();
                if tokio::signal::ctrl_c().await.is_ok() {
                    std::process::exit(130);
                }
            }
        });
    }

    // Explorer's status flyout, for a folder registered with the shell.
    let flyout = {
        let token = setup::open_store(dir, 1024)?.read(|o| o.meta_get("sync.token"))?;
        token.map(|t| tokio::spawn(publish_flyout(dir.clone(), t, Arc::clone(&control), Arc::clone(&graph))))
    };

    // ---- run; on an error, start over after a growing pause
    let mut pause = ERROR_FIRST;
    // Outlives each attempt: a mass deletion trickling in must still trip
    // the stops when an error restarts the rounds in between.
    let mut window = Window::default();
    loop {
        let started = Instant::now();
        match agent(dir, &graph, &args, &stop, &control, memory, key.as_ref(), &mut window).await {
            Ok(()) => {
                println!("oneupd: stopped.");
                end_flyout(dir, flyout);
                return Ok(());
            }
            Err(e) if is_permanent(&e) => {
                end_flyout(dir, flyout);
                return Err(e);
            }
            Err(e) if is_busy(&e) => {
                tracing::warn!("{e:#}");
                println!(
                    "oneupd: the database was busy (another OneUpDrive program held it); trying again in {} s",
                    BUSY_PAUSE.as_secs()
                );
                stop.sleep(BUSY_PAUSE).await;
                if stop.requested() {
                    println!("oneupd: stopped.");
                    return Ok(());
                }
            }
            Err(e) => {
                tracing::error!("{e:#}");
                let before = control.problem();
                pause = error_pause(pause, before.is_none(), started.elapsed());
                let retry_ms = now_ms() + i64::try_from(pause.as_millis()).unwrap_or(i64::MAX);
                let p = problem::describe(&e, before.as_ref(), now_ms(), Some(retry_ms));
                // Once per spell, not at every retry.
                if before.is_none_or(|b| b.kind != p.kind) {
                    note_problem(dir, &p, false);
                }
                println!("oneupd: {}: {e:#}\n        trying again in {} s", p.title, pause.as_secs());
                control.set_problem(Some(p));
                // Retry now (the settings window, `oneup sync-now`) cuts the
                // pause short; the next one still grows.
                tokio::select! {
                    () = stop.sleep(pause) => {}
                    () = control.wake.notified() => println!("oneupd: trying again now, as asked"),
                }
                if stop.requested() {
                    println!("oneupd: stopped.");
                    return Ok(());
                }
                pause = (pause * 2).min(ERROR_MAX);
            }
        }
    }
}

/// The pause before retrying after an error, given the last one (`pause`):
/// back to the first when the failed attempt got going first (a sequence of
/// rounds finished, which clears the standing problem, so `recovered`), or
/// ran longer than the longest pause. Otherwise one blip after days of
/// syncing would cost the longest pause left over from earlier ones.
fn error_pause(pause: Duration, recovered: bool, ran: Duration) -> Duration {
    if recovered || ran >= ERROR_MAX { ERROR_FIRST } else { pause }
}

/// Notes a problem beginning or clearing in the activity log; best effort.
fn note_problem(dir: &Path, p: &oneup_ipc::Problem, resolved: bool) {
    let logged = setup::open_store(dir, 1024).and_then(|mut s| Ok(problem::log(&mut s, p, resolved)?));
    if let Err(e) = logged {
        tracing::warn!(error = %format!("{e:#}"), "couldn't note the problem in the activity log");
    }
}

fn describe_pause(local: bool, remote: bool) -> &'static str {
    match (local, remote) {
        (true, true) => "paused",
        (true, false) => "changes to this PC are paused",
        (false, true) => "changes to OneDrive are paused",
        (false, false) => "not paused",
    }
}

/// The database was busy for longer than the store waits: retried soon.
fn is_busy(e: &anyhow::Error) -> bool {
    e.chain().any(|c| c.downcast_ref::<oneup_store::Error>().is_some_and(oneup_store::Error::is_busy))
}

/// Errors that retrying can't fix: the agent stops and says why.
fn is_permanent(e: &anyhow::Error) -> bool {
    e.chain().any(|c| {
        c.is::<Halted>()
            || matches!(
                c.downcast_ref::<oneup_local::observe::Error>(),
                Some(oneup_local::observe::Error::NeedsRescan(_))
            )
    })
}

/// How much memory the agent's database connection uses: its page cache,
/// and how much of the file it maps (0: none).
#[derive(Clone, Copy)]
struct Memory {
    cache_kib: u32,
    map_bytes: u64,
}

impl Memory {
    /// How much of the database the agent maps (`Store::map_memory`): four
    /// times its cache, and only when it's known to run at normal priority.
    /// Background priority keeps the agent's working set small, so mapped
    /// pages were trimmed and faulted back in one by one: a full pass of a
    /// 340k-item tree took 372 s instead of 47 s, with 220 million page
    /// faults.
    fn map_bytes(cache_kib: u32, normal_priority: bool) -> u64 {
        if normal_priority { u64::from(cache_kib) * 4 * 1024 } else { 0 }
    }
}

/// Syncs until stopped (`Ok`) or an error. Everything is reopened from the
/// database each time, so a retry starts from a clean state, except the
/// safety window (`window`), which the caller keeps across retries.
#[allow(clippy::too_many_arguments)]
async fn agent(
    dir: &Path,
    graph: &Arc<Graph>,
    args: &Args,
    stop: &Stop,
    control: &Control,
    memory: Memory,
    key: Option<&oneup_vault::key::Masterkey>,
    window: &mut Window,
) -> Result<()> {
    let mut store = setup::open_store(dir, memory.cache_kib)?;
    if memory.map_bytes > 0 {
        store.map_memory(memory.map_bytes)?;
    }
    if store.read(|o| o.meta_get(KEY_PRESET))?.is_none() {
        anyhow::bail!("no exclusion choice recorded; run `oneup sync` once first");
    }
    prepare_registration(dir, &store).await;
    // What listing skips; each round works it out again from the rules.
    let rules = oneup_app::config::load_rules(dir, &store)?.rule_set()?.pruning();
    let (_, engine) = match key {
        Some(k) => sync::resume_vault(&store, Arc::clone(graph), rules, dir, k.clone())?,
        None => sync::resume(&store, Arc::clone(graph), rules, dir)?,
    };
    let root = store.read(|o| o.local_root())?.context("no local scan")?;
    let journal = Arc::new(Notify::new());
    let tracker = oneup_app::apps::Tracker::start(dir, Path::new(&root.path));
    let busy = Arc::new(Busy::new(oneup_app::config::kept(dir, oneup_app::config::load_agent(dir))?.busy));
    engine.hold_busy(Arc::clone(&busy));
    watch_journal(PathBuf::from(&root.path), &journal, tracker, Arc::clone(&busy));
    let remote = Arc::new(Notify::new());
    // Files On-Demand: serve the placeholders' content while this attempt
    // runs (files-on-demand.md § Hydration).
    let _provider = if engine.local().on_demand() {
        // Explorer's entry was brought up to date before connecting
        // (`prepare_registration`).
        let stale = Arc::clone(&remote);
        let h = oneup_exec::hydrate::Hydrator::new(
            // Decrypting, for a vault pair.
            engine.hydration_cloud(),
            setup::open_store(dir, 4 * 1024)?,
            dir.join("hydration"),
            Path::new(&root.path),
            tokio::runtime::Handle::current(),
            Box::new(move || stale.notify_one()),
        )?;
        let serve = Arc::new(oneup_exec::hydrate::Serve(h));
        Some(oneup_cfapi::provider::connect(Path::new(&root.path), serve)?)
    } else {
        None
    };
    let push_up = Arc::clone(&control.push_up);
    // Aborted when this attempt ends, so a restart never runs two channels.
    // A folder in someone else's drive has no push channel for us: the
    // timer covers it.
    let _push = (!args.no_push && !graph.is_other_drive())
        .then(|| AbortOnDrop(tokio::spawn(push(Arc::clone(graph), Arc::clone(&remote), Arc::clone(&push_up)))));

    let poll = Duration::from_secs(args.poll_seconds.max(15));
    // Every folder only when the marks may not cover everything (see
    // `start_scope`); otherwise what changed, including while nothing ran.
    let mut scope = sync::start_scope(&store)?;
    let mut retry = RETRY_FIRST;
    let mut settings = oneup_app::config::kept(dir, oneup_app::config::load_agent(dir))?;
    let mut auto_was: Option<String> = None;
    loop {
        // ---- automatic pauses (config.toml [agent]), checked at every
        // wake-up; a broken config.toml keeps the last good settings
        match oneup_app::config::load_agent(dir) {
            Ok(s) => settings = s,
            Err(e) => tracing::warn!(error = %format!("{e:#}"), "config.toml: keeping the previous settings"),
        }
        busy.set_settings(settings.busy);
        let minute = autopause::local_minute_of_day();
        let conditions = autopause::Conditions {
            metered: oneup_sys::conditions::metered(),
            battery_saver: oneup_sys::conditions::battery_saver(),
            full_screen: oneup_sys::conditions::full_screen(),
            minute_of_day: minute,
        };
        let auto = autopause::reason(&settings, &conditions);
        if auto != auto_was {
            match &auto {
                Some(why) => println!("oneupd: pausing by itself: {why}"),
                None => println!("oneupd: resuming ({} no longer applies)", auto_was.as_deref().unwrap_or("")),
            }
            auto_was.clone_from(&auto);
        }
        control.set_auto_pause(auto.clone());

        // ---- a timed pause may be over; a changed pause is saved
        let paused = control.settle_pause(now_ms());
        let mut held_until = None;
        let next = if auto.is_some() {
            control.set_activity(Activity::Paused);
            // Looked at again at the next wake-up or check, or when the
            // quiet hours end.
            let check = if push_up.load(Relaxed) { PUSH_POLL.max(poll) } else { poll };
            autopause::quiet_until(&settings, minute)
                .map_or(check, |m| check.min(Duration::from_secs(u64::from(m) * 60 + 5)))
        } else if paused.local && paused.remote {
            control.set_activity(Activity::Paused);
            PAUSED_WAIT
        } else {
            control.set_activity(if scope == Scope::All { Activity::Starting } else { Activity::Syncing });
            let started = Instant::now();
            let mut executed = Recent::default();
            let recent = window.recent();
            let rounds = engine
                .rounds(store, scope, &|| control.controls(), recent, &mut executed, MAX_ROUNDS, &|p| {
                    control.set_progress(p)
                })
                .await;
            // Before the error, if any: the retry must still count them.
            window.add(executed);
            let (s, out) = rounds?;
            store = s;
            held_until = out.held_until;
            control.set_unsynced(oneup_app::unsynced::count(&oneup_app::unsynced::load(&store)?));
            if !out.totals.is_quiet() {
                out.totals.print(out.rounds);
            }
            let now = now_ms();
            let last = (!out.totals.is_quiet()).then(|| LastRun {
                at_ms: now,
                done: out.totals.done(),
                skipped: out.totals.skipped(),
                failed: out.totals.failed(),
                held: out.totals.held(),
            });
            let offer = engine.offer();
            let new = control.record(now, last, &out.breakers, out.overflow.as_ref(), offer.as_ref());
            if let Some(p) = control.take_resolved() {
                println!("oneupd: resolved: {}", p.title);
                note_problem(dir, &p, true);
            }
            if new.new_stops {
                sync::print_breakers(&out.breakers, Asker::Agent);
            }
            if new.new_overflow
                && let Some(o) = &out.overflow
            {
                sync::print_overflow(o, &store, Asker::Agent)?;
            }
            if new.new_offer
                && let Some(o) = &offer
            {
                println!(
                    "oneupd: {} items excluded by your rules are still in OneDrive; `oneup cloud prune` lists them, \
                     `oneup cloud prune --yes` removes them (code {:016x})",
                    o.items, o.fingerprint
                );
            }
            tracing::debug!(rounds = out.rounds, ms = started.elapsed().as_millis() as u64, "rounds done");
            scope = Scope::Dirty;
            // With push live, the timed check is only a safety net. A
            // dropped channel pings `remote` itself, so a check follows at
            // once, and later waits use `poll` again.
            let check = if push_up.load(Relaxed) { PUSH_POLL.max(poll) } else { poll };
            match out.end {
                End::Settled => {
                    control.clear_first();
                    store = engine.placement(store, &control.controls()).await?;
                    // Removes kept copies on this PC: not while changes
                    // to it are paused.
                    if !control.controls().pause_local {
                        engine.trim(&mut store)?;
                    }
                    retry = RETRY_FIRST;
                    control.set_activity(Activity::Idle);
                    // Files already here becoming placeholders, a slice at a
                    // time: changes that come meanwhile wait at most one.
                    // Blocking file work: off the runtime's other tasks
                    // (the hydration service, the control pipe).
                    let more = !control.controls().pause_local
                        && tokio::task::block_in_place(|| engine.convert_leftovers(&mut store, CONVERT_SLICE))?;
                    // Marks a test build put on excluded folders (shelved).
                    if !control.controls().pause_local {
                        tokio::task::block_in_place(|| engine.clear_excluded_marks(&mut store))?;
                    }
                    if more { CONVERT_GAP } else { check }
                }
                End::Retry => {
                    let wait = retry.min(check);
                    retry = (retry * 2).min(RETRY_MAX);
                    control.set_activity(Activity::Retrying);
                    println!("oneupd: retrying the skipped items in {} s", wait.as_secs());
                    wait
                }
                End::Unsettled => Duration::from_secs(5),
                // Nothing ran. The next wake-up (a change, a check, or the
                // user's answer) plans it again; the stop is shown once.
                End::Stopped => {
                    control.set_activity(Activity::WaitingForYou);
                    check
                }
            }
        };
        // A held upload goes once its file has been left alone.
        let next = held_until.map_or(next, |u| next.min(u.saturating_duration_since(Instant::now())));
        // A timed pause ends the wait when it's over.
        let next = match paused.until_ms {
            Some(u) if paused.local || paused.remote => {
                next.min(Duration::from_millis(u64::try_from(u - now_ms()).unwrap_or(0)))
            }
            _ => next,
        };

        // ---- wait for something to do
        store = wait(store, &engine.pruning(), &journal, &remote, stop, &control.wake, next).await?;
        if stop.requested() {
            return Ok(());
        }
    }
}

/// Waits until a round is worth running: a new change in the synced folder,
/// a push notification, the OneDrive check is due (`timeout`), a control
/// call (`control`), or a stop. Wake-ups for changes elsewhere on the disk
/// read the journal and go back to waiting.
///
/// "New" because marks left by a round with skipped items stay until their
/// retry (`timeout`); only marks made after the wait began cut it short.
async fn wait(
    mut store: Store,
    rules: &oneup_rules::ExcludeSet,
    journal: &Notify,
    remote: &Notify,
    stop: &Stop,
    control: &Notify,
    timeout: Duration,
) -> Result<Store> {
    let deadline = tokio::time::Instant::now() + timeout;
    let seen = store.read(|o| o.dirty_last())?;
    loop {
        tokio::select! {
            _ = tokio::time::sleep_until(deadline) => return Ok(store),
            _ = stop.notify.notified() => return Ok(store),
            _ = control.notified() => {
                tracing::debug!("control call");
                return Ok(store);
            }
            _ = remote.notified() => {
                // Something changed in OneDrive (or the channel dropped). A
                // change often comes as a few notifications in a row.
                tracing::debug!("push notification");
                stop.sleep(PUSH_SETTLE).await;
                return Ok(store);
            }
            _ = journal.notified() => {}
        }
        // A control call doesn't wait for the burst to settle.
        tokio::select! {
            _ = stop.sleep(SETTLE) => {}
            _ = control.notified() => return Ok(store),
        }
        if stop.requested() {
            return Ok(store);
        }
        store = sync::observe(store, rules).await?;
        if store.read(|o| o.dirty_last())? > seen {
            tracing::debug!("change in the synced folder");
            return Ok(store);
        }
    }
}

/// Starts the thread that blocks on the volume's change journal and pings
/// `wake` whenever anything on the volume changed. If the journal can't be
/// watched, the agent still works: local changes are then picked up at the
/// next OneDrive check, which reads the journal too.
///
/// It also hands every written file to the program tracker
/// (`oneup_app::apps`), if there is one. When it falls far behind (a burst
/// of activity on the disk), it skips ahead: the observer reads everything
/// anyway; only those writes go uncounted.
fn watch_journal(root: PathBuf, wake: &Arc<Notify>, tracker: Option<oneup_app::apps::Tracker>, busy: Arc<Busy>) {
    // The thread can't be interrupted while blocked in the kernel. It holds
    // only a weak reference, so once the agent is done with `wake` (a stop
    // or a restart) the thread ends at its next wake-up.
    let alive = Arc::downgrade(wake);
    // Wakes the agent at most every `MIN_WAKE_GAP`, so the watcher can read
    // more often for the tracker. A full channel is a wake-up already due.
    let (ping, pinged) = std::sync::mpsc::sync_channel::<()>(1);
    let waker = alive.clone();
    let woke = std::thread::Builder::new().name("journal-wake".into()).spawn(move || {
        while pinged.recv().is_ok() {
            let Some(wake) = waker.upgrade() else { return };
            // A stored permit: wake-ups while the agent is busy collapse
            // into one.
            wake.notify_one();
            drop(wake);
            std::thread::sleep(MIN_WAKE_GAP);
        }
    });
    if let Err(e) = woke {
        tracing::warn!(error = %e, "can't start the change-journal watcher");
        return;
    }
    let spawned = std::thread::Builder::new().name("journal-watch".into()).spawn(move || {
        // Placeholders' attributes as they are (see below); process-wide.
        if let Err(e) = oneup_cfapi::expose_placeholders() {
            tracing::warn!(error = %e, "online-only files may look ordinary to the app tracker");
        }
        let run = || -> oneup_ntfs::win::Result<()> {
            let volume = oneup_ntfs::win::Volume::open_containing(&root)?;
            let j = volume.query_journal()?;
            let mut cursor = j.next_usn;
            // The observer reads the records properly; here they only wake
            // the agent and name written files for the tracker.
            let mut buf = vec![0u8; 64 * 1024];
            loop {
                let next = {
                    let (next, records) = volume.read_journal(j.journal_id, cursor, true, &mut buf)?;
                    let (now, wall) = (Instant::now(), std::time::SystemTime::now());
                    for r in records.flatten() {
                        // An online-only file wasn't written by a program:
                        // that's OneUpDrive making or freeing a
                        // placeholder. Asking who has it open downloaded
                        // it (thousands of files on a large folder).
                        if r.has(oneup_ntfs::record::reason::CONTENT)
                            && !r.is_directory()
                            && !oneup_ntfs::dirent::recalls(r.attributes)
                        {
                            if let Some(t) = &tracker {
                                t.saw(r.file_id.0);
                            }
                            // Our own downloads don't make a file busy.
                            if !oneup_cfapi::ours::wrote(r.file_id.0) {
                                busy.changed(r.file_id.0, oneup_app::busy::instant_of(r.timestamp, now, wall));
                            }
                        }
                    }
                    next
                };
                if alive.strong_count() == 0 {
                    return Ok(());
                }
                let _ = ping.try_send(());
                let end = volume.query_journal()?.next_usn;
                // Every record is read (for the program tracker and for
                // files that keep changing) unless far behind.
                cursor = if end - next > SKIP_AHEAD { end } else { next };
                std::thread::sleep(if tracker.is_some() { TRACK_GAP } else { MIN_WAKE_GAP });
            }
        };
        if let Err(e) = run() {
            tracing::warn!(error = %e, "can't watch the change journal; local changes are picked up at each OneDrive check");
        }
    });
    if let Err(e) = spawned {
        tracing::warn!(error = %e, "can't start the change-journal watcher");
    }
}

/// Keeps OneDrive's push channel open for as long as the agent runs: asks
/// for an endpoint, listens, and reconnects when it ends. `remote` is pinged
/// for every notification, and also whenever the channel drops, because a
/// notification may have been missed meanwhile. `up` says whether the
/// channel is live right now.
async fn push(graph: Arc<Graph>, remote: Arc<Notify>, up: Arc<AtomicBool>) {
    let mut pause = PUSH_RETRY_FIRST;
    loop {
        let started = Instant::now();
        let result = match graph.push_endpoint().await {
            Ok(ep) => {
                oneup_graph::push::listen(
                    &ep,
                    || {
                        up.store(true, Relaxed);
                        tracing::info!("push channel live");
                    },
                    || remote.notify_one(),
                )
                .await
            }
            Err(e) => Err(e),
        };
        if up.swap(false, Relaxed) {
            remote.notify_one();
        }
        match result {
            Ok(()) => tracing::info!("push channel closed by OneDrive; reconnecting"),
            Err(e) => tracing::warn!("push channel: {e}; checking OneDrive every poll interval meanwhile"),
        }
        // A channel that lasted a while was healthy: reconnect quickly.
        if started.elapsed() > Duration::from_secs(5 * 60) {
            pause = PUSH_RETRY_FIRST;
        }
        tokio::time::sleep(pause).await;
        pause = (pause * 2).min(PUSH_RETRY_MAX);
    }
}

/// Before the agent connects as the folder's provider: has `oneup
/// refresh-explorer-entry --repair` (which has the package identity this
/// program runs without) register the folder again if Windows lost its
/// registration and that's safe, then bring Explorer's entry up to date
/// with this version, and waits for it.
///
/// Before connecting, not after: re-registering the folder through the
/// shell while connected left a registration that Windows dropped once the
/// agent disconnected. Every new version did that at its first start (its
/// entry names the version), so the next stop or install lost the folder's
/// Files On-Demand registration (reproduced: intact through a reinstall, a
/// force-kill, the install folder moved and the hook; lost after a new
/// version's first run, at its stop).
/// If it can't be repaired, opening the folder fails its identity check,
/// which says what to do.
async fn prepare_registration(dir: &Path, store: &oneup_store::Store) {
    use std::os::windows::process::CommandExt as _;
    const CREATE_NO_WINDOW: u32 = 0x0800_0000;
    let Ok(pair) = sync::pairing(store) else { return };
    if !oneup_effects::LocalFs::was_on_demand(&pair.local) {
        return; // not a Files On-Demand folder: nothing registered
    }
    let Ok(exe) = oneup_app::launch::sibling("oneup.exe") else { return };
    let dir = dir.to_path_buf();
    let run = tokio::task::spawn_blocking(move || {
        std::process::Command::new(exe)
            .arg("--data-dir")
            .arg(&dir)
            .args(["refresh-explorer-entry", "--repair"])
            .stdin(std::process::Stdio::null())
            .creation_flags(CREATE_NO_WINDOW)
            .output()
    });
    match run.await {
        Ok(Ok(out)) if out.status.success() => {
            if String::from_utf8_lossy(&out.stdout).contains("registered for Files On-Demand again") {
                println!("oneupd: Windows had lost the folder's Files On-Demand registration; it was registered again");
            }
        }
        Ok(Ok(out)) => println!("oneupd: {}", String::from_utf8_lossy(&out.stderr).trim()),
        Ok(Err(e)) => tracing::warn!(error = %e, "the folder's registration couldn't be checked"),
        Err(e) => tracing::warn!(error = %e, "the folder's registration couldn't be checked"),
    }
}

/// Keeps Explorer's status flyout current (`oneup_app::shellstatus`):
/// whenever the status changes, and the storage figure every hour.
async fn publish_flyout(dir: PathBuf, token: String, control: Arc<Control>, graph: Arc<Graph>) {
    use oneup_app::shellstatus::{ShellStatus, publish};
    let mut changes = control.subscribe();
    let mut quota: Option<(i64, i64)> = None;
    let mut quota_at: Option<Instant> = None;
    // Written only when it changes: the status also changes every second
    // while a round runs (its progress), which the flyout doesn't show.
    let mut shown: Option<ShellStatus> = None;
    loop {
        // Someone else's drive (a shared folder) has no quota of ours.
        if !graph.is_other_drive() && quota_at.is_none_or(|t| t.elapsed() > Duration::from_secs(3600)) {
            if let Ok(q) = graph.quota().await {
                quota = q;
            }
            quota_at = Some(Instant::now());
        }
        let now = ShellStatus::from_status(&control.snapshot(), &dir, quota);
        if shown.as_ref() != Some(&now) {
            publish(&token, &now);
            shown = Some(now);
        }
        tokio::select! {
            r = changes.changed() => if r.is_err() { return },
            _ = tokio::time::sleep(Duration::from_secs(3600)) => {}
        }
    }
}

/// The agent is ending: Explorer shows no status for its folder.
fn end_flyout(dir: &Path, task: Option<tokio::task::JoinHandle<()>>) {
    if let Some(t) = task {
        t.abort();
    }
    if let Ok(Some(token)) = setup::open_store(dir, 1024).and_then(|s| Ok(s.read(|o| o.meta_get("sync.token"))?)) {
        oneup_app::shellstatus::clear(&token);
    }
}

/// Aborts a task when dropped.
struct AbortOnDrop(tokio::task::JoinHandle<()>);

impl Drop for AbortOnDrop {
    fn drop(&mut self) {
        self.0.abort();
    }
}

/// Mass-sensitive operations carried out in the last [`WINDOW`].
#[derive(Default)]
struct Window {
    done: VecDeque<(Instant, Recent)>,
}

impl Window {
    fn add(&mut self, r: Recent) {
        if r.local + r.remote + r.suspicious > 0 {
            self.done.push_back((Instant::now(), r));
        }
    }

    fn recent(&mut self) -> Recent {
        while self.done.front().is_some_and(|(t, _)| t.elapsed() > WINDOW) {
            self.done.pop_front();
        }
        self.done.iter().fold(Recent::default(), |a, (_, r)| a.plus(*r))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_error_pause_starts_over_once_the_agent_got_going() {
        let mut pause = ERROR_FIRST;
        // Errors in a row: it grows (doubled after each wait) to the most.
        for _ in 0..10 {
            pause = (error_pause(pause, false, Duration::from_secs(1)) * 2).min(ERROR_MAX);
        }
        assert_eq!(pause, ERROR_MAX);
        assert_eq!(error_pause(pause, false, Duration::from_secs(1)), ERROR_MAX, "still failing at once");
        assert_eq!(error_pause(pause, true, Duration::from_secs(1)), ERROR_FIRST, "rounds finished meanwhile");
        assert_eq!(error_pause(pause, false, ERROR_MAX), ERROR_FIRST, "ran a long while");
    }

    /// What the stops count adds up across calls of the rounds (the agent
    /// keeps one window across retries, `run`).
    #[test]
    fn the_safety_window_adds_up_what_ran() {
        let mut w = Window::default();
        w.add(Recent { local: 190, remote: 0, suspicious: 0 });
        w.add(Recent::default());
        w.add(Recent { local: 5, remote: 2, suspicious: 1 });
        assert_eq!(w.recent(), Recent { local: 195, remote: 2, suspicious: 1 });
        assert_eq!(w.done.len(), 2, "nothing done, nothing kept");
    }

    #[test]
    fn the_database_is_mapped_only_at_normal_priority() {
        assert_eq!(Memory::map_bytes(64 * 1024, true), 256 << 20, "four times the cache");
        assert_eq!(Memory::map_bytes(64 * 1024, false), 0, "background priority, or not known to be normal");
    }
}
