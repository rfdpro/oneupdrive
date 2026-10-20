//! `oneup sync`: two-way sync of one local folder with one OneDrive folder.
//!
//! While [`crate::setup::PRE_RELEASE`] is on, this command refuses any data
//! folder that isn't a test-account folder (`oneup login --test-account`).
//!
//! One run is a sequence of **rounds** until nothing is left to do:
//! 1. the change journal brings the Local tree up to date (the folder is
//!    scanned in full only the first time);
//! 2. OneDrive's delta feed brings the Remote tree up to date;
//! 3. the reconciler plans, over a [`Parked`] view (names OneDrive refuses
//!    and local-by-nature files never travel);
//! 4. the safety gate reviews the plan: a tripped mass-change breaker stops
//!    the run before anything happens; removed files that don't fit in the
//!    version store wait for the user's decision (by default) while
//!    everything else goes ahead;
//! 5. the executor carries out what was approved, re-checking every
//!    precondition right before acting. What goes into the folders it
//!    makes is planned and reviewed as each one is made, and runs in the
//!    same round ([`Follower`]).
//!
//! The pairing (local folder ↔ OneDrive folder) is recorded in the data
//! folder's database on the first run, together with the token written
//! into the local folder's `.oneup\root.marker`. Later runs must name the
//! same pair; a data folder syncs exactly one pair.

use std::hash::{BuildHasher, Hasher};
use std::path::{Path, PathBuf};
use std::sync::Arc;

use anyhow::{Context, Result, bail};
use oneup_effects::LocalFs;
use oneup_exec::{Cloud, Executor, GraphCloud, Report, Skip, link_roots};
use oneup_gate::{
    Approval, Approved, Breaker, BreakerKind, Choice, Context as GateContext, Decision, Fingerprint, ItemKey, OldCopy,
    Overflow, OverflowOutcome, Recent, Side, Withheld, is_mass_sensitive, review_in, store_bytes,
};
use oneup_graph::Graph;
use oneup_ipc::Progress;
use oneup_plan::Parked;
use oneup_plan::park::Why;
use oneup_remote::{Config as MirrorConfig, Mirror};
use oneup_rules::{ExcludeSet, RuleSet};

use crate::prune::Scope as PruneScope;
use oneup_store::{ActivityEntry, ActivityOutcome, Kind, Ops, Store};
use oneup_sync::{HoldReason, LocalId, Op, RemoteId, all_folders, plan_flow};

/// Database keys of the pairing (`meta` table).
pub(crate) const KEY_LOCAL: &str = "sync.local_path";
/// The fingerprint of the user's rules the last clean round planned with.
const KEY_RULES: &str = "rules.fingerprint";
/// The program version that last finished a round planning every folder.
const KEY_FULL_PASS: &str = "plan.full_pass_version";

/// Which folders the agent's first round plans. Every folder only when the
/// marks may not cover everything: no round planned every folder yet with
/// this version (a database from before schema v5 has no marks; a new
/// version may plan differently). Changes while nothing ran are marked as
/// they're read (the change journal, OneDrive's changes), and changed rules
/// make the round plan every folder anyway. Planning every folder took about
/// 9 minutes for 300k files at every start before this.
pub fn start_scope(store: &Store) -> Result<Scope> {
    let done = store.read(|o| o.meta_get(KEY_FULL_PASS))?;
    Ok(if done.as_deref() == Some(env!("CARGO_PKG_VERSION")) { Scope::Dirty } else { Scope::All })
}
pub(crate) const KEY_REMOTE: &str = "sync.remote_path";
pub(crate) const KEY_TOKEN: &str = "sync.token";
/// Set once `.oneup\root.marker` has been written. Before that, a missing
/// marker is expected; afterwards it means the folder was replaced.
pub(crate) const KEY_MARKER: &str = "sync.marker_written";

pub struct Options {
    /// The data folder (its `config.toml` holds the rules).
    pub data_dir: PathBuf,
    pub local: PathBuf,
    /// OneDrive folder, as a path from the drive's top (`/Sync/Test`).
    pub remote: String,
    pub dry_run: bool,
    /// A dry run lists at most this many changes (and always the totals).
    pub preview_limit: Option<usize>,
    /// Record the pair and open the folder, and leave the syncing to the
    /// background agent (the settings window's setup).
    pub pair_only: bool,
    /// Make the folder OneUpDrive's Files On-Demand folder before the first
    /// round, so OneDrive's files arrive online-only.
    pub on_demand: bool,
    pub max_rounds: usize,
    pub answers: Answers,
}

/// What the user controls in a run: their answers, and which directions
/// are paused (the agent's `oneup pause`; a one-shot `oneup sync` never is).
#[derive(Debug, Clone, Default)]
pub struct Controls {
    pub answers: Answers,
    pub pause_local: bool,
    pub pause_remote: bool,
    /// Paths (relative to the synced folder) to transfer before anything
    /// else (`oneup sync-now PATH`).
    pub first: Vec<String>,
    /// The pause as it changes while a round runs (the agent's; `None`:
    /// nothing pauses mid-round).
    pub live: Option<Arc<crate::control::LivePause>>,
}

/// Who the messages are for: `oneup sync` takes answers on its next run,
/// the agent through `oneup approve/keep/discard` while it keeps running.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Asker {
    Sync,
    Agent,
}

/// The user's answers, as codes the run printed: approvals for tripped
/// safety stops ([`print_breakers`]) and decisions on version-store
/// overflow batches ([`print_overflow`]).
#[derive(Debug, Clone, Default)]
pub struct Answers {
    pub approve: Vec<u64>,
    pub keep: Vec<u64>,
    pub discard: Vec<u64>,
    /// Accepted offers to remove excluded items from OneDrive ([`Offer`]).
    pub prune: Vec<u64>,
    /// Offers declined: those items stay in OneDrive ([`crate::prune::KEY_KEPT`]).
    pub keep_in_cloud: Vec<u64>,
}

/// Unless `cloud_copies = "prune"`: the OneDrive items the rules exclude,
/// which the agent offers to remove (to OneDrive's recycle bin) once they're
/// here. Worked out when the agent starts and when the rules change;
/// accepting it by its code removes exactly these items (a changed list has
/// a new code; one that only shrinks, as they go, stays accepted).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Offer {
    pub fingerprint: u64,
    pub entries: usize,
    /// Items in all, at least (unlisted folders count once until OneDrive
    /// is asked, which only happens when removing).
    pub items: u64,
    pub bytes: u64,
    /// Accepted: being brought here and removed, round by round (through
    /// the safety gate, which may stop to ask), until nothing is left.
    pub accepted: bool,
    /// The items (ID and eTag) it's for.
    pub covered: Vec<(String, String)>,
}

impl Answers {
    fn approvals(&self) -> Vec<Approval> {
        self.approve
            .iter()
            .flat_map(|&f| [Side::Local, Side::Remote].map(|side| Approval { side, fingerprint: Fingerprint(f) }))
            .collect()
    }

    fn decisions(&self) -> Vec<Decision> {
        let d = |codes: &[u64], choice| {
            codes.iter().map(move |&f| Decision { fingerprint: Fingerprint(f), choice }).collect::<Vec<_>>()
        };
        let mut out = d(&self.keep, Choice::Keep);
        out.extend(d(&self.discard, Choice::Discard));
        out
    }
}

/// `oneup sync`: rounds until nothing is left to do, then trims the
/// version store.
pub async fn run(mut store: Store, graph: Arc<Graph>, rules: ExcludeSet, opts: &Options) -> Result<()> {
    // A new pairing of the OneDrive app's old folder keeps the choices made
    // there (`crate::carryover`), before anything is listed or planned.
    let mut carried = false;
    let new_pair = !opts.dry_run && store.read(|o| o.meta_get(KEY_LOCAL))?.is_none();
    if new_pair && let Some(said) = crate::carryover::apply(&opts.data_dir, &mut store, &opts.local, &opts.remote)? {
        println!("{said}");
        carried = true;
    }
    let Some((store, engine)) = prepare(store, graph, rules, opts).await? else { return Ok(()) };
    if carried {
        // Paired now: the app's choices are used.
        crate::carryover::mark_applied(&opts.data_dir)?;
    }
    if new_pair && let Some(said) = crate::appleftovers::after_move_over(&opts.data_dir, &opts.local) {
        // The OneDrive app's folder is OneUpDrive's now: what the app left
        // in the registry goes.
        println!("{said}");
    }
    if opts.pair_only {
        println!("Ready: the background agent does the syncing from here.");
        return Ok(());
    }
    let controls = Controls { answers: opts.answers.clone(), ..Controls::default() };
    let (mut store, out) = engine
        .rounds(
            store,
            Scope::All,
            &|| controls.clone(),
            Recent::default(),
            &mut Recent::default(),
            opts.max_rounds,
            &|_| {},
            None,
        )
        .await?;
    if let Some(o) = &out.overflow {
        print_overflow(o, &store, Asker::Sync)?;
    }
    if out.end != End::Stopped {
        // Removing chosen duplicates waits without stopping the rest.
        print_breakers(&out.breakers, Asker::Sync);
    }
    match out.end {
        End::Stopped => {
            print_breakers(&out.breakers, Asker::Sync);
            bail!("stopped before changing anything: a safety stop needs your answer (see above)")
        }
        End::Settled => out.totals.print(out.rounds),
        End::Retry => {
            out.totals.print(out.rounds);
            println!("Some items were skipped (see above); run `oneup sync` again to retry them.");
        }
        End::Unsettled => {
            out.totals.print(out.rounds);
            println!("Not settled after {} rounds; run `oneup sync` again to continue.", out.rounds);
        }
    }
    engine.trim(&mut store)
}

/// Checks (or, on the first run, records) the pairing, brings both trees
/// up to date, and opens the sync folder. `None` after a dry run, which
/// prints what the first round would do instead.
async fn prepare(
    mut store: Store,
    graph: Arc<Graph>,
    rules: ExcludeSet,
    opts: &Options,
) -> Result<Option<(Store, Engine)>> {
    let remote_path = normalize_remote(&opts.remote)?;
    let local = std::fs::canonicalize(&opts.local)
        .with_context(|| format!("{} doesn't exist (create the folder first)", opts.local.display()))?;
    if local.parent().is_none() || local.components().count() < 3 {
        bail!("refusing to sync a drive's top folder; pick a folder inside it");
    }

    // ---- the pairing: recorded once, then enforced
    let paired_local = store.read(|o| o.meta_get(KEY_LOCAL))?;
    let paired_remote = store.read(|o| o.meta_get(KEY_REMOTE))?;
    let first = match (&paired_local, &paired_remote) {
        (Some(l), Some(r)) => {
            if !crate::test_account::same_dir(Path::new(l), &local)
                || oneup_sync::fold(r) != oneup_sync::fold(&remote_path)
            {
                bail!(
                    "this data folder already syncs {l} with OneDrive {r}; \
                     use another --data-dir for another pair"
                );
            }
            false
        }
        _ => true,
    };
    // Once paired, listing follows the rules as they are now (config.toml),
    // not the choice made at the first sync.
    let rules = if first {
        rules
    } else {
        let (set, _) = crate::rulefiles::load(&opts.data_dir, &store, &mut crate::rulefiles::Cache::default())?;
        set.pruning()
    };

    // ---- local: a full scan the first time only; the journal afterwards
    if first {
        println!("Scanning {} …", local.display());
        let cfg = oneup_local::Config::new(rules.clone());
        let mut scan = oneup_local::Scan::open(store, &local, cfg)?;
        if let Some(other) = scan.other_engine() {
            bail!("{} is managed by {}; OneUpDrive won't sync it", local.display(), other.label());
        }
        let summary = tokio::task::spawn_blocking(move || scan.run().map(|s| (s, scan))).await??;
        if !summary.0.finished {
            bail!("the scan was interrupted; run the same command again");
        }
        store = summary.1.into_store();
        // Another sync app's placeholders with no sync root (an unlinking
        // that skipped a folder some program had open): their content isn't
        // here, and OneUpDrive could neither serve nor replace them.
        let left = store.read(|o| o.local_stats())?.placeholders;
        if left > 0 && oneup_cfapi::provider::registration(&local).is_none() {
            bail!(
                "{} still has {left} placeholders another sync app left behind (an unlinking that didn't finish); \
                 run `oneup migrate finish-unlink` (or Set up's Finish unlinking) first",
                local.display()
            );
        }
    }

    // ---- remote: bring the mirror up to date, find (or create) the folder
    store = mirror(&graph, store, &rules).await?;
    if first {
        // The OneDrive app's old folder goes only with the account it synced.
        crate::moveover::refuse_other_account(&store, &local, &opts.data_dir)?;
    }
    let remote_id = match find_remote(&store, &remote_path)? {
        Some(id) => id,
        None if opts.dry_run => bail!("OneDrive folder {remote_path} doesn't exist yet; a real run creates it"),
        None => {
            create_remote(&graph, &store, &remote_path).await?;
            store = mirror(&graph, store, &rules).await?;
            find_remote(&store, &remote_path)?.context("the new OneDrive folder didn't appear in the delta feed")?
        }
    };

    let root = store.read(|o| o.local_root())?.context("no local scan")?;
    if !store.write(|o| link_roots(o, root.root_file_id, &remote_id))? {
        bail!("this data folder's sync tree belongs to another folder pair; use another --data-dir");
    }
    if first {
        let token = random_token();
        store.write(|o| {
            o.meta_set(KEY_LOCAL, &local.to_string_lossy())?;
            o.meta_set(KEY_REMOTE, &remote_path)?;
            crate::moveover::note_app_folder(o, &local)?;
            o.meta_set(KEY_TOKEN, &token)
        })?;
    }

    if opts.dry_run {
        dry_run(store, &rules, &opts.answers, first, &opts.data_dir, opts.preview_limit).await?;
        return Ok(None);
    }

    // ---- the effects layer: opening it checks the folder's identity
    let token = store.read(|o| o.meta_get(KEY_TOKEN))?.context("pairing token missing")?;
    let first_use = store.read(|o| o.meta_get(KEY_MARKER))?.is_none();
    let mut fs = LocalFs::open(Path::new(&root.path), root.volume_serial, root.root_file_id, &token, first_use)
        .context("opening the sync folder")?;
    locate_from(&mut fs, &store)?;
    store.write(|o| o.meta_set(KEY_MARKER, "1"))?;
    if opts.on_demand && !fs.on_demand() {
        // Before any round, so nothing is downloaded in full on the way.
        drop(fs);
        let on = crate::ondemand::turn_on(&store)?;
        println!("Files On-Demand is on ({} folders, {} files already here kept as they are).", on.folders, on.files);
        fs = open_paired(&store)?;
    }
    Ok(Some((store, Engine::new(fs, graph, rules, &opts.data_dir))))
}

/// For the background agent: the pair a first `oneup sync` recorded, opened
/// with the same identity checks. Never records a new pairing.
pub fn resume(store: &Store, graph: Arc<Graph>, rules: ExcludeSet, data_dir: &Path) -> Result<(Pair, Engine)> {
    let pair = pairing(store)?;
    let fs = open_paired(store)?;
    Ok((pair, Engine::new(fs, graph, rules, data_dir)))
}

/// For a vault pair's agent: as [`resume`], with the vault unlocked by
/// `key` (`crate::vaultpair`).
pub fn resume_vault(
    store: &Store,
    graph: Arc<Graph>,
    rules: ExcludeSet,
    data_dir: &Path,
    key: oneup_vault::key::Masterkey,
) -> Result<(Pair, Engine)> {
    let pair = pairing(store)?;
    let fs = open_paired(store)?;
    let vault = crate::vaultpair::VaultRemote::open(data_dir, store, Arc::clone(&graph), key)?;
    Ok((pair, Engine::with_vault(fs, graph, rules, data_dir, Some(Arc::new(vault)))))
}

/// The recorded folder pair.
#[derive(Debug, Clone)]
pub struct Pair {
    pub local: PathBuf,
    pub remote: String,
}

pub fn pairing(store: &Store) -> Result<Pair> {
    let local = store.read(|o| o.meta_get(KEY_LOCAL))?;
    let remote = store.read(|o| o.meta_get(KEY_REMOTE))?;
    match (local, remote) {
        (Some(l), Some(r)) => Ok(Pair { local: PathBuf::from(l), remote: r }),
        _ => Err(NotPaired.into()),
    }
}

/// The data folder syncs nothing yet (a typed error, so the agent can say
/// "set up a folder" rather than report a fault, `crate::problem`).
#[derive(Debug)]
pub struct NotPaired;

impl std::fmt::Display for NotPaired {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("this data folder doesn't sync anything yet; run `oneup sync <folder> --remote <path>` once first")
    }
}

impl std::error::Error for NotPaired {}

/// Opens the paired sync folder for the version-store commands, with the
/// same identity check as a sync run.
pub fn open_paired(store: &Store) -> Result<LocalFs> {
    if store.read(|o| o.meta_get(KEY_MARKER))?.is_none() {
        bail!("this data folder has no synced folder yet (run `oneup sync` first)");
    }
    let token = store.read(|o| o.meta_get(KEY_TOKEN))?.context("pairing token missing")?;
    let root = store.read(|o| o.local_root())?.context("no local scan")?;
    let mut fs = LocalFs::open(Path::new(&root.path), root.volume_serial, root.root_file_id, &token, false)
        .context("opening the sync folder")?;
    locate_from(&mut fs, store)?;
    Ok(fs)
}

/// Lets the effects find items where the Local tree has them
/// (`LocalFs::set_locator`), through a database connection of their own:
/// they run on the executor's tasks, which don't hold the engine's.
fn locate_from(fs: &mut LocalFs, store: &Store) -> Result<()> {
    if let Some(reader) = store.reopen(256)? {
        let reader = std::sync::Mutex::new(reader);
        fs.set_locator(Box::new(move |id| {
            let reader = reader.lock().unwrap_or_else(std::sync::PoisonError::into_inner);
            reader.read(|o| o.local_path_of(id.0)).ok().flatten()
        }));
    }
    Ok(())
}

/// The executor stopped a round because something is fundamentally wrong
/// (the sync folder failed its identity check, or the database failed).
/// Retrying can't help; the agent stops on it.
#[derive(Debug)]
pub struct Halted(pub String);

impl std::fmt::Display for Halted {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "stopped: {}", self.0)
    }
}

impl std::error::Error for Halted {}

/// Which folders a round plans.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Scope {
    /// Every Synced folder: cost grows with the tree. For a one-shot run,
    /// changed rules, and the agent's start-up when the marks may not cover
    /// everything ([`start_scope`]).
    All,
    /// Only the folders the store marked dirty (`oneup_store::dirty`):
    /// cost grows with the changes.
    Dirty,
}

/// How a sequence of rounds ended.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum End {
    /// A round found nothing left to do.
    Settled,
    /// Nothing more could be done now, but some operations were skipped
    /// (a file open in another program, a change in the meantime) or
    /// failed; they need another try later.
    Retry,
    /// A safety stop tripped: that round changed nothing.
    Stopped,
    /// Still busy after the maximum number of rounds.
    Unsettled,
}

pub struct Outcome {
    pub end: End,
    pub rounds: usize,
    pub totals: Totals,
    /// The last version-store overflow batch the rounds saw, if any (see
    /// [`print_overflow`]).
    pub overflow: Option<Overflow>,
    /// The tripped safety stops, when `end` is [`End::Stopped`] (see
    /// [`print_breakers`]).
    pub breakers: Vec<Breaker>,
    /// When the earliest operation the last round held may go: an upload
    /// of a file that keeps changing ([`crate::busy`]), or one that failed
    /// ([`crate::backoff`]). The caller runs a round then.
    pub held_until: Option<std::time::Instant>,
}

/// Everything a round needs besides the database: the executor (with the
/// opened sync folder), OneDrive, and the exclusion rules.
pub struct Engine {
    exec: Executor<crate::vaultpair::PairCloud>,
    /// How long each kind of operation and request took, logged after each
    /// round (`oneup_exec::timing`).
    timings: Arc<oneup_exec::Timings>,
    graph: Arc<Graph>,
    /// A vault pair: its Remote tree is the vault's decrypted view.
    vault: Option<Arc<crate::vaultpair::VaultRemote>>,
    /// What listing skips by folder name: the name-only part of the user's
    /// rules (`RuleSet::pruning`), worked out again every round.
    rules: std::sync::Mutex<ExcludeSet>,
    /// The data folder, for `config.toml`; the user's rules are loaded
    /// from it and the `.oneupignore` files every round.
    data_dir: PathBuf,
    rule_files: std::sync::Mutex<crate::rulefiles::Cache>,
    /// Notices already logged (safety stops, overflow batches), so one that
    /// stands over many rounds is logged once.
    logged: std::sync::Mutex<Vec<String>>,
    offer: std::sync::Mutex<Option<Offer>>,
    /// Converting synced files already here into placeholders, in the
    /// background ([`Engine::convert_leftovers`]).
    converting: std::sync::Mutex<Converting>,
    /// Files that keep changing, whose uploads wait (the agent's; none for
    /// `oneup sync`).
    busy: std::sync::OnceLock<Arc<crate::busy::Busy>>,
    /// Failed operations wait their turn (`crate::backoff`; the agent's).
    backoff: std::sync::atomic::AtomicBool,
    /// Set while OneDrive is full: uploads wait ([`Engine::cloud_full`]).
    full: std::sync::Mutex<Option<Full>>,
    /// The store's invariants were checked once this run ([`Engine::remote`]).
    verified: std::sync::atomic::AtomicBool,
    /// Safety stops raised lately, and the lists the user approved.
    stops: std::sync::Mutex<Stops>,
    /// Where what the rounds did goes as events (the agent's; none for
    /// `oneup sync`).
    events: std::sync::OnceLock<Arc<crate::events::Feed>>,
    /// Databases the last round held back (`crate::databases`).
    db_waits: std::sync::Mutex<(Vec<oneup_ipc::DatabaseNote>, u64)>,
    /// Files that shouldn't sync while in use, seen since the agent
    /// started, until excluded or kept syncing (at most
    /// [`crate::databases::MAX_LIVE`]).
    db_live: std::sync::Mutex<Vec<oneup_ipc::DatabaseNote>>,
    /// Whether applying pin rules or downloading what they keep has work
    /// left ([`Engine::pins_pending`]; the walk's own state is in the store).
    pin_more: std::sync::atomic::AtomicBool,
}

/// What stop `i` of `review` lists this round: exactly the operations its
/// items key (with its folders still to come, `Breaker::later`), not every
/// operation held for its side.
fn stop_ops(review: &oneup_gate::Review, i: usize) -> Vec<&Op> {
    review
        .lists
        .get(i)
        .map(|l| l.iter().filter_map(|&w| review.withheld.get(w)).map(|(op, _)| op).collect())
        .unwrap_or_default()
}

/// Records the agent's safety window with a call's counts added
/// ([`Engine::rounds`]).
pub type Keep = dyn Fn(&mut Store, Recent) -> Result<()> + Send + Sync;

/// `executed` plus what `approved` may carry out that the safety window
/// counts: recorded before a round runs.
fn reserved(executed: Recent, approved: &[Approved], suspicious: &[LocalId]) -> Recent {
    let mut r = executed;
    for op in approved.iter().map(Approved::op) {
        r.add_op(op, suspicious);
    }
    r
}

/// Safety stops an engine raised, and what the user approved of them.
///
/// Approving a stop approves its list, item by item (`oneup_gate::ItemKey`):
/// the rounds after the answer run whatever of it they plan, and the
/// folders it lists to empty level by level. The list's folders were marked
/// for planning when it was raised, so the round after the answer plans all
/// of it, even for a stop a full pass raised.
#[derive(Debug, Default)]
struct Stops {
    /// The stops raised lately, newest last (at most [`STOPS_KEPT`]): an
    /// answer can come for one a later round already replaced.
    raised: Vec<Breaker>,
    /// Items on the lists approved, sorted, until a sequence of rounds
    /// settles (everything on them done, or no longer planned).
    approved: Vec<ItemKey>,
    /// Of those, the ones whose operations the last round planned but
    /// didn't run (held as a file that keeps changing, paused, waiting for
    /// a decision or a retry, skipped): kept when the rounds settle, until
    /// they run or are no longer planned.
    waiting: Vec<ItemKey>,
    /// Approval codes taken in by this call of the rounds: an approval
    /// answers one raising of a stop, so a code isn't taken again in the
    /// call; the caller drops it from its answers afterwards
    /// ([`Engine::spent_answers`]), and a later raising of the same stop
    /// needs (and takes) a new approval.
    spent: Vec<u64>,
    /// Of those, the ones the caller hasn't dropped yet.
    to_drop: Vec<u64>,
}

/// Stops remembered for an answer at most.
const STOPS_KEPT: usize = 8;

impl Stops {
    /// The user's approval codes not used yet.
    fn live(&self, codes: &[u64]) -> Vec<u64> {
        codes.iter().copied().filter(|c| !self.spent.contains(c)).collect()
    }

    /// Takes in the user's approvals (`codes`): the lists of stops raised
    /// here become approved, and their codes are spent.
    fn answer(&mut self, codes: &[u64]) {
        let codes = self.live(codes);
        let (taken, kept): (Vec<Breaker>, Vec<Breaker>) =
            std::mem::take(&mut self.raised).into_iter().partition(|b| codes.contains(&b.fingerprint.0));
        self.raised = kept;
        for b in taken {
            self.approve(&b);
        }
    }

    /// A list approved (by a code this engine raised, or one the gate
    /// matched): its items are approved and its code is spent.
    fn approve(&mut self, b: &Breaker) {
        self.approved.extend(&b.items);
        self.approved.sort_unstable();
        self.approved.dedup();
        let code = b.fingerprint.0;
        if !self.spent.contains(&code) {
            self.spent.push(code);
            self.to_drop.push(code);
        }
    }

    /// A call of the rounds begins: the caller has dropped the codes the
    /// last one took in.
    fn begin(&mut self) {
        self.spent.clear();
    }

    /// A round ran: of the operations on approved lists (`listed`, with
    /// the keys they matched), those in `done` ran, and their keys go; the
    /// rest wait.
    fn round_ended(&mut self, listed: Vec<(Op, Vec<ItemKey>)>, done: &[Op]) {
        if listed.is_empty() {
            self.waiting.clear();
            return;
        }
        let mut by_op: std::collections::HashMap<String, Vec<ItemKey>> = std::collections::HashMap::new();
        for (op, keys) in listed {
            by_op.entry(format!("{op:?}")).or_default().extend(keys);
        }
        let mut ran: Vec<ItemKey> = Vec::new();
        for op in done {
            if let Some(keys) = by_op.remove(&format!("{op:?}")) {
                ran.extend(keys);
            }
        }
        ran.sort_unstable();
        self.approved.retain(|k| ran.binary_search(k).is_err());
        self.waiting = by_op.into_values().flatten().collect();
        self.waiting.sort_unstable();
        self.waiting.dedup();
    }

    /// A sequence of rounds settled: no stop stands, and of what was
    /// approved, only what still waits stays approved.
    fn settled(&mut self) {
        let waiting = &self.waiting;
        self.approved.retain(|k| waiting.binary_search(k).is_ok());
        self.raised.clear();
    }

    /// Remembers stops a round raised. Returns the Synced items of those
    /// not seen before, whose folders the caller marks for planning.
    fn raise(&mut self, breakers: &[Breaker]) -> Vec<oneup_sync::Sid> {
        let mut mark = Vec::new();
        for b in breakers {
            if self.raised.iter().any(|r| r.fingerprint == b.fingerprint) {
                continue;
            }
            mark.extend(&b.sids);
            self.raised.push(b.clone());
        }
        let over = self.raised.len().saturating_sub(STOPS_KEPT);
        self.raised.drain(..over);
        mark
    }
}

/// OneDrive refused an upload for want of room: uploads wait until it has
/// more. Read again at most every [`ROOM_CHECK`], at a round (a wake-up of
/// the agent's: a change, a notification, its timed check), never on a timer
/// of its own.
#[derive(Debug, Clone, Copy)]
struct Full {
    /// When it was found full (Unix ms).
    since_ms: i64,
    /// The room it had left then, in bytes (`None`: not told).
    remaining: Option<i64>,
    /// When an upload was last refused, and the room last read.
    tried: std::time::Instant,
    checked: std::time::Instant,
}

/// The least time between two reads of the room left while OneDrive is full.
const ROOM_CHECK: std::time::Duration = std::time::Duration::from_secs(60);
/// How long uploads wait while the room left reads the same (or isn't told,
/// as for a folder someone else shares) before one is tried again anyway.
const FULL_RETRY: std::time::Duration = std::time::Duration::from_secs(3600);

impl Full {
    /// A new reading of the room left (`None`: not told, or unreadable) at
    /// `now`: whether uploads go again. They do once it grew, or
    /// [`FULL_RETRY`] after the last refusal, whatever it reads.
    fn room_again(&mut self, remaining: Option<i64>, now: std::time::Instant) -> bool {
        self.checked = now;
        match (self.remaining, remaining) {
            (Some(before), Some(n)) if n > before => return true,
            (None, Some(n)) => self.remaining = Some(n),
            _ => {}
        }
        now.saturating_duration_since(self.tried) >= FULL_RETRY
    }
}

/// OneDrive is full ([`Engine::cloud_full`]).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct CloudFull {
    /// Since when (Unix ms).
    pub since_ms: i64,
    /// The room it said it had left, in bytes.
    pub remaining: Option<i64>,
}

/// At most this long applying pin rules after a settled round; the rest
/// waits for the next one ([`Engine::pins_pending`]).
const PIN_SLICE: std::time::Duration = std::time::Duration::from_secs(20);

/// Drops from `ops` (freeing operations) the files a pin rule keeps, and
/// sets their pin back. Returns how many.
fn keep_pinned(
    store: &mut Store,
    pinner: &mut dyn crate::pinrules::Pinner,
    pins: &oneup_rules::PinSet,
    ops: &mut Vec<Op>,
) -> Result<usize> {
    let mut kept = 0;
    let mut out = Vec::with_capacity(ops.len());
    for op in std::mem::take(ops) {
        let path = match &op {
            Op::Dehydrate { local, .. } => store.read(|o| o.local_path_of(local.id.0))?.map(|p| (local.id.0, p)),
            _ => None,
        };
        match path {
            Some((id, p)) if pins.pins(&p, false) => {
                kept += 1;
                if !crate::pinrules::pin_back(store, pinner, id, &crate::filetree::clean(&p))? {
                    tracing::debug!("a file a pin rule keeps couldn't be pinned again now");
                }
            }
            _ => out.push(op),
        }
    }
    *ops = out;
    Ok(kept)
}

/// Excluded folders looked at per pass ([`Engine::clear_excluded_marks`]);
/// more wait for the next settled round.
const MARK_LIMIT: usize = 5_000;

/// Where [`Engine::convert_leftovers`] is.
#[derive(Debug)]
struct Converting {
    /// The next page starts after this sid; `None` once done for this run.
    after: Option<oneup_sync::Sid>,
    pass: usize,
    /// This pass: converted, and left as they were.
    converted: u64,
    left: u64,
    /// Converted since the agent started (logged once it's done).
    total: u64,
}

impl Default for Converting {
    fn default() -> Self {
        Self { after: Some(oneup_sync::ROOT), pass: 1, converted: 0, left: 0, total: 0 }
    }
}

impl Engine {
    fn new(fs: LocalFs, graph: Arc<Graph>, rules: ExcludeSet, data_dir: &Path) -> Self {
        Self::with_vault(fs, graph, rules, data_dir, None)
    }

    fn with_vault(
        fs: LocalFs,
        graph: Arc<Graph>,
        rules: ExcludeSet,
        data_dir: &Path,
        vault: Option<Arc<crate::vaultpair::VaultRemote>>,
    ) -> Self {
        // Uploads hold up to a fragment in memory; the profile bounds it,
        // unless [agent] sets it.
        let agent = crate::config::load_agent(data_dir).unwrap_or_default();
        let timings = Arc::new(oneup_exec::Timings::default());
        let plain = GraphCloud::with_memory(Arc::clone(&graph), agent.upload_memory()).timed(Arc::clone(&timings));
        let cloud = match &vault {
            Some(v) => crate::vaultpair::PairCloud::Vault(v.cloud(plain)),
            None => crate::vaultpair::PairCloud::Plain(plain),
        };
        let exec = Executor::new(fs, cloud, rules.clone()).timed(Arc::clone(&timings));
        exec.set_parallel(agent.parallel());
        Self {
            exec,
            timings,
            graph,
            vault,
            rules: std::sync::Mutex::new(rules),
            data_dir: data_dir.to_path_buf(),
            rule_files: std::sync::Mutex::default(),
            logged: std::sync::Mutex::new(Vec::new()),
            offer: std::sync::Mutex::new(None),
            converting: std::sync::Mutex::new(Converting::default()),
            busy: std::sync::OnceLock::new(),
            backoff: std::sync::atomic::AtomicBool::new(false),
            full: std::sync::Mutex::new(None),
            verified: std::sync::atomic::AtomicBool::new(false),
            stops: std::sync::Mutex::default(),
            events: std::sync::OnceLock::new(),
            db_waits: std::sync::Mutex::default(),
            db_live: std::sync::Mutex::default(),
            pin_more: std::sync::atomic::AtomicBool::new(false),
        }
    }

    /// The databases held back by the last round, and the files warned
    /// about (`crate::databases`), for the status.
    /// The notes, and how many more databases were held back than they
    /// list (at most [`crate::databases::MAX_WAITS`] are).
    pub fn database_notes(&self) -> (Vec<oneup_ipc::DatabaseNote>, u64) {
        let (waits, more) = self.db_waits.lock().unwrap_or_else(|p| p.into_inner()).clone();
        let live = self.db_live.lock().unwrap_or_else(|p| p.into_inner()).clone();
        ([waits, live].concat(), more)
    }

    /// After a round: notes the databases it held back, and warns (once)
    /// about each file it sent or fetched that shouldn't sync while in use,
    /// unless the user keeps it syncing. A warning goes once the rules
    /// exclude the file or the user keeps it.
    fn note_databases(
        &self,
        store: &mut Store,
        planned: &[(Op, Line)],
        report: &Report,
        keep: &[String],
        rules: &RuleSet,
    ) -> Result<()> {
        // Looked up by operation once: a large round has many.
        let paths: std::collections::HashMap<&Op, &str> = planned.iter().map(|(o, l)| (o, l.path.as_str())).collect();
        let path_of = |op: &Op| paths.get(op).map(|p| (*p).to_string());
        let found = crate::databases::Found::of(report);
        *self.db_waits.lock().unwrap_or_else(|p| p.into_inner()) = crate::databases::held_notes(report, &paths);
        let mut new = Vec::new();
        {
            let mut live = self.db_live.lock().unwrap_or_else(|p| p.into_inner());
            live.retain(|n| {
                !crate::databases::kept_syncing(keep, &n.path) && !rules.excludes(n.path.trim_start_matches('/'), false)
            });
            let ops = report.done.iter().chain(report.skipped.iter().map(|(op, _)| op));
            for op in ops {
                let (Some(format), Some(path)) = (crate::databases::live_format(&found, op), path_of(op)) else {
                    continue;
                };
                if crate::databases::kept_syncing(keep, &path)
                    || live.iter().any(|n| oneup_sync::same_name(&n.path, &path))
                    || live.len() >= crate::databases::MAX_LIVE
                {
                    continue;
                }
                let n = crate::databases::live_note(path, format);
                new.push(n.clone());
                live.push(n);
            }
        }
        for n in new {
            let e = ActivityEntry {
                path: n.path.clone(),
                ..notice(ActivityOutcome::Note, "database", crate::databases::detail(&n))
            };
            self.log_once(store, format!("live {}", n.path), e)?;
        }
        Ok(())
    }

    fn stops_lock(&self) -> std::sync::MutexGuard<'_, Stops> {
        self.stops.lock().unwrap_or_else(|p| p.into_inner())
    }

    /// Approval codes this engine has used since this was last asked: the
    /// caller drops them from the answers it passes in (`oneupd`'s control
    /// state), so they can't approve anything again.
    pub fn spent_answers(&self) -> Vec<u64> {
        std::mem::take(&mut self.stops_lock().to_drop)
    }

    /// What the rounds do goes to `feed` as events, as it goes to the
    /// activity log.
    pub fn publish_events(&self, feed: Arc<crate::events::Feed>) {
        let _ = self.events.set(feed);
    }

    /// Holds the uploads of the files `busy` says keep changing.
    pub fn hold_busy(&self, busy: Arc<crate::busy::Busy>) {
        let _ = self.busy.set(busy);
    }

    /// Until when `op` waits because its file keeps changing, if it does.
    fn held_until(&self, op: &Op, now: std::time::Instant) -> Option<std::time::Instant> {
        self.busy.get()?.held_until(sent_file(op)?, now)
    }

    /// Failed operations wait before they're tried again (`crate::backoff`).
    pub fn back_off(&self) {
        self.backoff.store(true, std::sync::atomic::Ordering::Relaxed);
    }

    fn backs_off(&self) -> bool {
        self.backoff.load(std::sync::atomic::Ordering::Relaxed)
    }

    /// Leaves out the operations that failed and whose time hasn't come
    /// (`crate::backoff`), keeping the order, with when the first may go.
    /// Lookups by key, and none while nothing is on record.
    #[allow(clippy::type_complexity)]
    fn hold_failed(
        &self,
        o: &Ops<'_>,
        planned: Vec<(Approved, Line)>,
    ) -> oneup_store::Result<(Vec<(Approved, Line)>, Vec<(Approved, Line)>, Option<std::time::Instant>)> {
        if !self.backs_off() || !o.retry_any()? {
            return Ok((planned, Vec::new(), None));
        }
        let at = now_ms();
        let waits = planned
            .iter()
            .map(|(a, _)| crate::backoff::until(o, a.op(), at))
            .collect::<oneup_store::Result<Vec<_>>>()?;
        let (now, mut waits) = (std::time::Instant::now(), waits.into_iter());
        Ok(hold_back(planned, |_| waits.next().flatten().map(|u| after_ms(now, u - at))))
    }

    /// Whether OneDrive is full, so uploads wait, and since when.
    pub fn cloud_full(&self) -> Option<CloudFull> {
        self.full_lock().map(|f| CloudFull { since_ms: f.since_ms, remaining: f.remaining })
    }

    fn full_lock(&self) -> std::sync::MutexGuard<'_, Option<Full>> {
        self.full.lock().unwrap_or_else(|p| p.into_inner())
    }

    /// While OneDrive is full: reads the room it has left (at most every
    /// [`ROOM_CHECK`]), and lets uploads go again once it grew.
    async fn check_room(&self, store: &mut Store) -> Result<()> {
        if !self.full_lock().is_some_and(|f| f.checked.elapsed() >= ROOM_CHECK) {
            return Ok(());
        }
        let remaining = match self.graph.remaining().await {
            Ok(r) => r,
            Err(e) => {
                tracing::debug!(error = %e, "couldn't read the room left in OneDrive");
                None
            }
        };
        let again = {
            let mut g = self.full_lock();
            let again = g.as_mut().is_some_and(|f| f.room_again(remaining, std::time::Instant::now()));
            if again {
                *g = None;
            }
            again
        };
        if again {
            println!("Uploads resume.");
            log_activity(store, vec![notice(ActivityOutcome::Note, "uploads", "uploads resume".into())])?;
        }
        Ok(())
    }

    /// OneDrive refused an upload for want of room: uploads wait from now
    /// on, until it has more ([`Engine::check_room`]).
    async fn found_full(&self, store: &mut Store) -> Result<()> {
        let remaining = self.graph.remaining().await.ok().flatten();
        let now = std::time::Instant::now();
        let was = {
            let mut g = self.full_lock();
            let since_ms = g.map_or_else(now_ms, |f| f.since_ms);
            g.replace(Full { since_ms, remaining, tried: now, checked: now }).is_some()
        };
        if !was {
            println!("OneDrive is full: uploads wait until there's room. Downloads go on.");
            let detail = "OneDrive is full; uploads wait until there's room".to_string();
            log_activity(store, vec![notice(ActivityOutcome::Waiting, "uploads", detail)])?;
        }
        Ok(())
    }

    /// The cloud for serving Files On-Demand downloads: the same view of
    /// OneDrive as the executor's (decrypting, for a vault pair).
    pub fn hydration_cloud(&self) -> crate::vaultpair::PairCloud {
        let plain = GraphCloud::new(Arc::clone(&self.graph));
        match &self.vault {
            Some(v) => crate::vaultpair::PairCloud::Vault(v.cloud(plain)),
            None => crate::vaultpair::PairCloud::Plain(plain),
        }
    }

    /// The cloud side brought up to date: the mirror, or for a vault pair
    /// its decrypted view. The store's invariants are checked in the first
    /// round only: the check reads every row of all three trees, so at every
    /// round it made each push notification cost hundreds of megabytes.
    async fn remote(&self, store: Store, pruning: &ExcludeSet) -> Result<Store> {
        use std::sync::atomic::Ordering::Relaxed;
        match &self.vault {
            Some(v) => v.refresh(store).await,
            None => {
                let verify = !self.verified.load(Relaxed);
                let store = mirror_with(&self.graph, store, pruning, verify).await?;
                self.verified.store(true, Relaxed);
                Ok(store)
            }
        }
    }

    /// The current offer to remove excluded items from OneDrive, if any.
    pub fn offer(&self) -> Option<Offer> {
        self.offer.lock().unwrap_or_else(|p| p.into_inner()).clone()
    }

    fn set_offer(&self, o: Option<Offer>) {
        *self.offer.lock().unwrap_or_else(|p| p.into_inner()) = o;
    }

    /// Unless `cloud_copies = "prune"`: works out the [`Offer`] when `look`
    /// (the agent started, or the rules changed) or while one is accepted,
    /// and once it is, returns the removals of what's here and brings the
    /// rest here ([`Engine::ready_ops`]). `brings`: see
    /// [`crate::prune::candidates`].
    async fn offered(
        &self,
        store: &mut Store,
        rules: &RuleSet,
        look: bool,
        answers: &Answers,
        brings: bool,
    ) -> Result<Vec<Op>> {
        // Kept in OneDrive: settled for good (saved, so a restart doesn't
        // offer them again).
        if let Some(o) = self.offer().filter(|o| !o.accepted && answers.keep_in_cloud.contains(&o.fingerprint)) {
            crate::prune::keep(store, o.covered.into_iter().map(|(id, _)| id))?;
            self.set_offer(None);
            log_activity(
                store,
                vec![notice(
                    ActivityOutcome::Note,
                    "keep in OneDrive",
                    format!("{} excluded items stay in OneDrive, as you chose; changes here aren't sent", o.items),
                )],
            )?;
            return Ok(Vec::new());
        }
        let before = self.offer().filter(|o| o.accepted || answers.prune.contains(&o.fingerprint));
        if !look && before.is_none() {
            return Ok(Vec::new());
        }
        let found = crate::prune::candidates(store, rules, PruneScope::All, brings)?;
        let found = crate::prune::without_kept(store, found)?;
        if found.is_empty() {
            self.set_offer(None);
            return Ok(Vec::new());
        }
        let fingerprint = crate::prune::fingerprint(&found);
        let covered: Vec<(String, String)> = found.iter().map(|c| (c.id.clone(), c.etag.clone())).collect();
        // Accepted for this list, or for one it's part of (some are gone).
        let accepted = answers.prune.contains(&fingerprint)
            || before.is_some_and(|o| covered.iter().all(|x| o.covered.contains(x)));
        self.set_offer(Some(Offer {
            fingerprint,
            entries: found.len(),
            items: found.iter().map(|c| c.items.max(1)).sum(),
            bytes: found.iter().map(|c| u64::try_from(c.size).unwrap_or(0)).sum(),
            accepted,
            covered,
        }));
        if !accepted {
            return Ok(Vec::new());
        }
        self.ready_ops(store, found).await
    }

    /// The removals of the candidates that are all here. The online-only
    /// files of the others are set to always keep on this device, so the
    /// placement pass downloads them first (`crate::prune`, "only once
    /// it's here").
    async fn ready_ops(&self, store: &Store, found: Vec<crate::prune::Candidate>) -> Result<Vec<Op>> {
        let fetch: Vec<String> = found.iter().flat_map(|c| c.fetch.iter().cloned()).collect();
        if !fetch.is_empty() && self.exec.local().on_demand() {
            let root = PathBuf::from(store.read(|o| o.local_root())?.context("no local scan")?.path);
            tokio::task::spawn_blocking(move || {
                use oneup_cfapi::provider;
                for rel in fetch {
                    let full = rel.trim_start_matches('/').split('/').fold(root.clone(), |p, x| p.join(x));
                    if let Err(e) = provider::open(&full).and_then(|f| provider::set_pin(&f, provider::Pin::Pinned)) {
                        tracing::debug!(error = %e, "an excluded file to bring here couldn't be set to keep");
                    }
                }
            })
            .await?;
        }
        let ready: Vec<_> = found.into_iter().filter(crate::prune::Candidate::ready).collect();
        Ok(crate::prune::counted(self.graph.as_ref(), ready).await?.iter().map(crate::prune::Candidate::op).collect())
    }

    /// Files On-Demand placement after a round (`oneup_exec::placement`):
    /// pinned files are downloaded, files set to free up space are freed
    /// through the gate. Nothing while changes to this PC are paused.
    pub async fn placement(&self, mut store: Store, controls: &(dyn Fn() -> Controls + Send + Sync)) -> Result<Store> {
        if !self.exec.local().on_demand() || controls().pause_local {
            return Ok(store);
        }
        let root = store.read(|o| o.local_root())?.context("no local scan")?;
        // Pins kept from the OneDrive app, as their items become ours.
        let pinned = crate::carryover::pin_pending(&mut store, Path::new(&root.path))?;
        if pinned > 0 {
            log_activity(
                &mut store,
                vec![notice(
                    ActivityOutcome::Note,
                    "keep",
                    format!("{pinned} item(s) kept on this device, as they were with the OneDrive app"),
                )],
            )?;
        }
        // Pin rules ([rules] pin): set on what they match, before anything
        // is fetched or freed, so this pass already downloads them.
        let pins = crate::config::load_rules(&self.data_dir, &store)?.pin_set();
        let mut pinner = crate::pinrules::FsPinner(PathBuf::from(&root.path));
        let pass = tokio::task::block_in_place(|| {
            crate::pinrules::apply(&mut store, &mut pinner, &pins, PIN_SLICE, now_ms())
        })?;
        let mut notes = Vec::new();
        if pass.pinned > 0 {
            notes.push(notice(
                ActivityOutcome::Note,
                "keep",
                format!("{} item(s) kept on this PC by pin rules", pass.pinned),
            ));
        }
        if pass.unpinned > 0 {
            let detail = format!("{} item(s) no longer kept on this PC: their pin rule was removed", pass.unpinned);
            notes.push(notice(ActivityOutcome::Note, "keep", detail));
        }
        log_activity(&mut store, notes)?;
        // Downloads keep to a pause that comes meanwhile, leave free space
        // alone, and take a share per pass; the rest follows soon after.
        let space = crate::config::kept(&self.data_dir, crate::config::load_space(&self.data_dir))?;
        let total = oneup_sys::disk::total_bytes(Path::new(&root.path)).ok();
        let reserve = crate::pinrules::reserve(total, space.low_disk_percent);
        let limit = std::sync::Mutex::new(crate::pinrules::FetchLimit::new(reserve, std::time::Instant::now()));
        let stopped = std::sync::atomic::AtomicBool::new(false);
        let root_path = PathBuf::from(&root.path);
        let may = |size: u64| {
            let paused = controls().pause_local;
            let free = crate::pinrules::free_space(&root_path);
            let mut l = limit.lock().unwrap_or_else(|p| p.into_inner());
            let ok = l.may(size, paused, free, std::time::Instant::now());
            if !ok {
                stopped.store(true, std::sync::atomic::Ordering::Relaxed);
            }
            ok
        };
        let fetched = oneup_exec::placement::fetch_pinned(&store, Path::new(&root.path), &may).await?;
        // More to download (stopped by its share, not by a pause or space,
        // which a later wake-up finds anyway).
        let more_fetch = stopped.load(std::sync::atomic::Ordering::Relaxed) && !controls().pause_local;
        self.pin_more.store(pass.more || more_fetch, std::sync::atomic::Ordering::Relaxed);
        if !fetched.is_empty() {
            let at_ms = now_ms();
            let log: Vec<ActivityEntry> = fetched
                .into_iter()
                .map(|path| ActivityEntry {
                    seq: 0,
                    at_ms,
                    outcome: ActivityOutcome::Done,
                    action: "keep".into(),
                    path,
                    detail: "always kept on this device: downloaded".into(),
                })
                .collect();
            println!("Downloaded {} file(s) set to always keep on this device.", log.len());
            log_activity(&mut store, log)?;
        }
        let mut ops = oneup_exec::placement::free_ops(&store)?;
        // A pin rule wins over "Free up space" chosen in File Explorer: the
        // file isn't freed and gets its pin back.
        if !pins.is_empty() {
            let kept = keep_pinned(&mut store, &mut pinner, &pins, &mut ops)?;
            if kept > 0 {
                let detail = format!("{kept} item(s) kept on this PC by pin rules, though set to free up space");
                log_activity(&mut store, vec![notice(ActivityOutcome::Note, "keep", detail)])?;
            }
        }
        let note = self.space_policy(&mut store, Path::new(&root.path), &pins, &mut ops)?;
        if ops.is_empty() {
            return Ok(store);
        }
        if let Some(note) = note {
            println!("Freeing up space: {note}.");
            log_activity(&mut store, vec![notice(ActivityOutcome::Note, "space", note)])?;
        }
        let policy = crate::versions::policy(&store)?;
        let review = review_in(&policy, &GateContext::default(), ops);
        let planned: Vec<(Op, Line)> =
            store.read(|o| Ok(review.approved.iter().map(|a| (a.op().clone(), describe_approved(o, a))).collect()))?;
        let report = self.exec.run(&mut store, review.approved).await;
        let log = print_round(1, &planned, &report);
        log_activity(&mut store, log)?;
        if let Some(why) = report.halted {
            return Err(Halted(why).into());
        }
        Ok(store)
    }

    /// Whether applying pin rules has work left for another pass soon (a
    /// walk after the rules changed, or many changes).
    pub fn pins_pending(&self) -> bool {
        self.pin_more.load(std::sync::atomic::Ordering::Relaxed)
    }

    /// Files On-Demand: turns synced files and folders that are already
    /// here but still ordinary into placeholders in sync, keeping their
    /// content (`crate::ondemand::convert`), for up to `budget` after a
    /// settled round. Turning it on does this all at once with the agent
    /// stopped; an interrupted run (ended by an install, say) left
    /// thousands showing "Sync pending" in
    /// Explorer. Pages through the Synced tree once per start, skipping
    /// what's recorded as a placeholder; returns whether there's more.
    pub fn convert_leftovers(&self, store: &mut Store, budget: std::time::Duration) -> Result<bool> {
        if !self.exec.local().on_demand() || self.vault.is_some() {
            return Ok(false);
        }
        let started = std::time::Instant::now();
        let mut c = self.converting.lock().unwrap_or_else(|p| p.into_inner());
        while let Some(after) = c.after {
            if started.elapsed() >= budget {
                return Ok(true);
            }
            let page = store.read(|o| o.synced_not_placeholders(after, 200))?;
            let Some(last) = page.last() else {
                // A pass over everything; again if it converted something
                // and left some (a parent converted late).
                let again = c.left > 0 && c.converted > 0 && c.pass < 3;
                c.after = again.then_some(oneup_sync::ROOT);
                c.pass += 1;
                (c.converted, c.left) = (0, 0);
                continue;
            };
            c.after = Some(last.sid);
            for b in &page {
                match crate::ondemand::convert(self.exec.local(), store, b)? {
                    crate::ondemand::Converted::Left => c.left += 1,
                    _ => {
                        c.converted += 1;
                        c.total += 1;
                    }
                }
            }
        }
        if c.total > 0 {
            let n = std::mem::take(&mut c.total);
            tracing::info!(n, "files already here became placeholders");
            let detail = format!("{n} item(s) already on this PC became Files On-Demand placeholders (content kept)");
            log_activity(store, vec![notice(ActivityOutcome::Note, "convert", detail)])?;
        }
        Ok(false)
    }

    /// Clears the marks a pre-release build put on excluded folders for
    /// File Explorer, a concept since shelved: each one's
    /// *excluded* pin goes back to unspecified, the folder and its contents
    /// otherwise as they are (never reverted: see
    /// `LocalFs::unmark_excluded_folder`). Only folders whose recorded
    /// attributes still show the pin are opened, so once cleared this costs
    /// a look at the excluded folders' rows (a partial index). Returns how
    /// many were cleared.
    pub fn clear_excluded_marks(&self, store: &mut Store) -> Result<usize> {
        if !self.exec.local().on_demand() || self.vault.is_some() {
            return Ok(0);
        }
        // Both pin bits: pinned *excluded* (measured).
        const EXCLUDED_PIN: u32 = 0x0018_0000;
        let (excluded, synced) =
            store.read(|o| Ok((o.local_excluded_folders(MARK_LIMIT)?, o.local_marked_but_synced(MARK_LIMIT)?)))?;
        let fs = self.exec.local();
        let mut cleared = 0usize;
        for r in excluded.iter().chain(&synced).filter(|r| r.attributes & EXCLUDED_PIN == EXCLUDED_PIN) {
            match fs.unmark_excluded_folder(r.file_id) {
                Ok(true) => cleared += 1,
                Ok(false) => {}
                Err(e) => tracing::debug!(error = %e, "an excluded folder's mark wasn't cleared; tried again later"),
            }
        }
        if cleared > 0 {
            tracing::info!(cleared, "marks of excluded folders cleared");
            let detail = format!("{cleared} excluded folder(s) marked by a test build are now plain folders again");
            log_activity(store, vec![notice(ActivityOutcome::Note, "excluded folders", detail)])?;
        }
        Ok(cleared)
    }

    /// The automatic space policies (`crate::space`): adds what they free
    /// now to `ops`, and says why. Files not used for a while are looked
    /// for at most hourly; a low disk, at every pass.
    fn space_policy(
        &self,
        store: &mut Store,
        root: &Path,
        pins: &oneup_rules::PinSet,
        ops: &mut Vec<Op>,
    ) -> Result<Option<String>> {
        const KEY_LAST: &str = "space.last_ms";
        let space = crate::config::kept(&self.data_dir, crate::config::load_space(&self.data_dir))?;
        if space.free_unused_days == 0 && !space.free_when_low {
            return Ok(None);
        }
        let disk = match (oneup_sys::disk::free_bytes(root), oneup_sys::disk::total_bytes(root)) {
            (Ok(free), Ok(total)) => Some(crate::space::Disk { free, total }),
            _ => None,
        };
        let low =
            space.free_when_low && disk.is_some_and(|d| d.free < d.total / 100 * u64::from(space.low_disk_percent));
        let now = now_ms();
        let last: i64 = store.read(|o| o.meta_get(KEY_LAST))?.and_then(|v| v.parse().ok()).unwrap_or(0);
        if !low && now - last < 3_600_000 {
            return Ok(None);
        }
        let kept = |rel: &str| pins.pins(rel, false);
        let sel = crate::space::select(store, &space, disk, now, &|rel| crate::space::last_access(root, rel), &kept)?;
        store.write(|o| o.meta_set(KEY_LAST, &now.to_string()))?;
        if sel.ops.is_empty() {
            return Ok(None);
        }
        let note = crate::space::describe(&sel);
        ops.extend(sel.ops);
        Ok(Some(note))
    }

    /// Brings both trees up to date, as a round does first.
    pub async fn refresh(&self, store: Store) -> Result<Store> {
        let pruning = self.update_pruning(&store)?;
        let store = observe(store, &pruning).await?;
        self.remote(store, &pruning).await
    }

    /// `oneup cloud prune --yes`: removes `found` from OneDrive through the
    /// safety gate and the executor. The user confirmed exactly this list,
    /// which approves a safety stop it trips (and only for this list).
    pub async fn prune(&self, mut store: Store, found: &[crate::prune::Candidate]) -> Result<(Store, Report)> {
        let ops: Vec<Op> = found.iter().map(crate::prune::Candidate::op).collect();
        let policy = crate::versions::policy(&store)?;
        let synced_items = store.read(|o| o.synced_stats())?.items.max(0) as u64;
        let mut review = review_in(&policy, &GateContext { synced_items, ..GateContext::default() }, ops.clone());
        if !review.breakers.is_empty() {
            let approvals: Vec<Approval> =
                review.breakers.iter().map(|b| Approval { side: b.side, fingerprint: b.fingerprint }).collect();
            let cx = GateContext { synced_items, approvals: &approvals, ..GateContext::default() };
            review = review_in(&policy, &cx, ops);
        }
        let planned: Vec<(Op, Line)> =
            store.read(|o| Ok(review.approved.iter().map(|a| (a.op().clone(), describe_approved(o, a))).collect()))?;
        let report = self.exec.run(&mut store, review.approved).await;
        let log = print_round(1, &planned, &report);
        log_activity(&mut store, log)?;
        if let Some(why) = &report.halted {
            return Err(Halted(why.clone()).into());
        }
        Ok((store, report))
    }

    /// What listing skips by folder name now (see [`Engine::pruning`]).
    pub fn pruning(&self) -> ExcludeSet {
        self.rules.lock().unwrap_or_else(|p| p.into_inner()).clone()
    }

    /// Works out what listing skips from the rules as they stand (the rule
    /// files as last seen) and hands it to the executor.
    fn update_pruning(&self, store: &Store) -> Result<ExcludeSet> {
        let pruning = {
            let mut cache = self.rule_files.lock().unwrap_or_else(|p| p.into_inner());
            pruning_for(&self.data_dir, store, &mut cache)?
        };
        *self.rules.lock().unwrap_or_else(|p| p.into_inner()) = pruning.clone();
        self.exec.set_rules(pruning.clone());
        Ok(pruning)
    }

    /// The user's rules for this round, telling the user once about a
    /// rule file that was ignored.
    fn load_rules(&self, store: &mut Store) -> Result<RuleSet> {
        let (set, problems) = {
            let mut cache = self.rule_files.lock().unwrap_or_else(|p| p.into_inner());
            crate::rulefiles::load(&self.data_dir, store, &mut cache)?
        };
        for (path, why) in problems {
            println!("Rule file {path} ignored: {why}");
            let e =
                ActivityEntry { path: path.clone(), ..notice(ActivityOutcome::Skipped, "rule file", why.to_string()) };
            self.log_once(store, format!("rules {path} {why}"), e)?;
        }
        Ok(set)
    }

    pub fn local(&self) -> &LocalFs {
        self.exec.local()
    }

    /// Saves what each tripped stop holds back ([`crate::stoplist`]), once
    /// per stop: the agent plans again at every wake-up while one stands.
    fn save_stop_lists(&self, store: &Store, review: &oneup_gate::Review) {
        let codes: Vec<String> = review.breakers.iter().map(|b| format!("{:016x}", b.fingerprint.0)).collect();
        crate::stoplist::keep_only(&self.data_dir, &codes);
        for (i, (b, code)) in review.breakers.iter().zip(&codes).enumerate() {
            if crate::stoplist::path(&self.data_dir, code).exists() {
                continue;
            }
            let held = stop_ops(review, i);
            let lines = store.read(|o| {
                Ok(held
                    .iter()
                    .map(|op| if b.kind == BreakerKind::Removal { removal_line(o, op) } else { describe(o, op) })
                    .chain(b.later.iter().map(|op| later_line(o, op)))
                    .take(crate::stoplist::MAX_LINES)
                    .map(|l| l.to_string())
                    .collect::<Vec<_>>())
            });
            let header = format!("OneUpDrive safety stop {code}: {}. Nothing has been changed yet.", breaker_what(b));
            let total = held.len() + b.later.len();
            let saved = lines
                .map_err(anyhow::Error::from)
                .and_then(|l| Ok(crate::stoplist::write(&self.data_dir, code, &header, &l, total)?));
            if let Err(e) = saved {
                tracing::warn!(error = %e, "can't save the list of what a safety stop holds back");
            }
        }
    }

    /// Logs a notice unless the same one was logged last time round.
    fn log_once(&self, store: &mut Store, key: String, e: ActivityEntry) -> Result<()> {
        {
            let mut seen = self.logged.lock().unwrap_or_else(|p| p.into_inner());
            if seen.contains(&key) {
                return Ok(());
            }
            if seen.len() >= 16 {
                seen.remove(0);
            }
            seen.push(key);
        }
        log_activity(store, vec![e])
    }

    /// Rounds until nothing is left to do (see the module docs). The first
    /// round plans `first`; later ones plan only what the previous rounds
    /// marked dirty, which is everything they could have left behind.
    ///
    /// `recent`: mass-sensitive operations already carried out in the
    /// caller's safety window. This call's own (per side, see
    /// [`oneup_gate::Recent`]) are added to `executed` as each round runs
    /// them, so they reach the caller's window even when a later round
    /// ends the call in an error.
    /// `controls` is asked again before every round, so a pause or a
    /// `sync-now PATH` that arrives meanwhile counts from the next round.
    /// `keep` (the agent's) records the safety window durably with this
    /// call's counts added: before a round changes anything, with what it
    /// may carry out reserved, and again with what it did. A round whose
    /// reservation can't be recorded doesn't run (the call fails), so a
    /// crash never forgets what ran.
    #[allow(clippy::too_many_arguments)]
    pub async fn rounds(
        &self,
        mut store: Store,
        first: Scope,
        controls: &(dyn Fn() -> Controls + Send + Sync),
        recent: Recent,
        executed: &mut Recent,
        max_rounds: usize,
        progress: &(dyn Fn(Option<Progress>) + Send + Sync),
        keep: Option<&Keep>,
    ) -> Result<(Store, Outcome)> {
        let mut out = Outcome {
            end: End::Unsettled,
            rounds: 0,
            totals: Totals::default(),
            overflow: None,
            breakers: Vec::new(),
            held_until: None,
        };
        let mut scope = first;
        let mut changed_rounds = 0;
        self.check_room(&mut store).await?;
        self.stops_lock().begin();
        for n in 1..=max_rounds.max(1) {
            out.rounds = n;
            out.held_until = None;
            let controls = &controls();
            if controls.pause_local && controls.pause_remote && n > 1 {
                // Paused meanwhile: nothing more now; the caller sees why.
                out.end = End::Settled;
                break;
            }
            // Bandwidth caps (config.toml [agent]) can change any time, and
            // may apply only in some hours.
            let settings = crate::config::kept(&self.data_dir, crate::config::load_agent(&self.data_dir))?;
            let capped = crate::autopause::caps_apply(&settings, crate::autopause::local_minute_of_day());
            let cap = |kib: u64| if capped { kib.saturating_mul(1024) } else { 0 };
            self.graph.upload.set(cap(settings.upload_kib));
            self.graph.download.set(cap(settings.download_kib));
            let pruning = self.update_pruning(&store)?;
            store = observe(store, &pruning).await?;
            store = self.remote(store, &pruning).await?;
            // A vault move a crash cut short, recorded where it went before
            // the plan reads its old ID's removal as a deletion.
            let settled = self.exec.settle_moves(&mut store)?;
            if settled != oneup_exec::Settled::default() {
                let oneup_exec::Settled { relinked, held, waiting, .. } = settled;
                tracing::info!(relinked, held, waiting, "settled moves an earlier round cut short");
            }
            let user_rules = self.load_rules(&mut store)?;
            // Databases: what counts as one, and whether they wait
            // (`[databases]`, read every round like the rest).
            let db_settings = crate::config::kept(&self.data_dir, crate::config::load_databases(&self.data_dir))?;
            let databases = db_settings.databases()?;
            // Changed rules, or database patterns (they decide which working
            // files are parked): every folder is planned again, from the
            // database (rules.md § Changing rules without a rescan).
            let fingerprint = planning_fingerprint(&user_rules, &databases);
            self.exec.set_databases(databases);
            let rules_changed = store.read(|o| o.meta_get(KEY_RULES))?.as_deref() != Some(fingerprint.as_str());
            if rules_changed {
                scope = Scope::All;
            }
            let window = recent.plus(*executed);
            // The user's answers for what can't sync (`crate::resolve`):
            // renames join this round's plan, for the gate like the rest.
            let backup = crate::flow::get(&store)? == crate::flow::Flow::Backup;
            let fs = self.exec.local();
            // "Can't tell" counts as here: nothing is noted gone on a doubt.
            let mut chosen = crate::resolve::apply_queued(&mut store, backup, &|id| fs.presence(id) != Some(false))?;
            log_activity(&mut store, std::mem::take(&mut chosen.log))?;
            // Duplicates the user chose to remove (`crate::duplicates`): the
            // gate holds them as a stop of their own until approved.
            let paused = controls.pause_local || controls.pause_remote;
            let (removal, log) = crate::duplicates::for_round(&self.data_dir, &mut store, backup, paused)?;
            log_activity(&mut store, log)?;
            // What happens to OneDrive's copies of excluded items.
            let mode = crate::config::load_rules(&self.data_dir, &store)?.cloud;
            let mut extra = match mode {
                _ if controls.pause_remote => Vec::new(),
                crate::config::CloudCopies::Prune => {
                    self.set_offer(None);
                    let dirty = store.read(|o| o.dirty_scope())?.folders;
                    let where_ = if rules_changed || scope == Scope::All {
                        PruneScope::All
                    } else {
                        PruneScope::Children(&dirty)
                    };
                    let found = crate::prune::candidates(&store, &user_rules, where_, true)?;
                    self.ready_ops(&store, found).await?
                }
                crate::config::CloudCopies::Keep | crate::config::CloudCopies::Pull => {
                    let look = rules_changed || scope == Scope::All;
                    let brings = mode == crate::config::CloudCopies::Pull;
                    let t = std::time::Instant::now();
                    let extra = self.offered(&mut store, &user_rules, look, &controls.answers, brings).await?;
                    if look {
                        tracing::info!(s = t.elapsed().as_secs(), "looked for excluded items still in OneDrive");
                    }
                    extra
                }
            };
            extra.extend(chosen.ops.iter().map(|(op, _)| op.clone()));
            let removal_ops = removal.as_ref().map(|r| r.plan.ops.clone()).unwrap_or_default();
            let modes = Modes { pull: mode != crate::config::CloudCopies::Keep, extra, removal: removal_ops };
            // Stops approved since the last round: their lists are approved.
            // Codes already used approve nothing again.
            let (listed, controls) = {
                let mut stops = self.stops_lock();
                stops.answer(&controls.answers.approve);
                let mut c = controls.clone();
                c.answers.approve = stops.live(&c.answers.approve);
                (stops.approved.clone(), c)
            };
            let controls = &controls;
            let t = std::time::Instant::now();
            let Planned { mut review, problems, upto, suspicious, policy, synced, planning } =
                plan_round(&store, Some(self.exec.local()), scope, controls, window, &user_rules, modes, &listed)?;
            // Approved by their codes (`oneup sync --approve`): their lists
            // stay approved for the folders they empty in later rounds.
            for b in &review.answered {
                self.stops_lock().approve(b);
            }
            // Said, so a slow start can be told from a slow round (one
            // can take minutes on a large folder).
            if scope == Scope::All || t.elapsed().as_secs() >= 10 {
                tracing::info!(round = n, all = scope == Scope::All, s = t.elapsed().as_secs(), "planned");
            }
            let planned_all = scope == Scope::All;
            scope = Scope::Dirty;
            out.totals.problems = problems;
            if !review.breakers.is_empty() {
                for b in &review.breakers {
                    let detail = format!("{}; nothing was changed (code {:016x})", breaker_what(b), b.fingerprint.0);
                    let e = notice(ActivityOutcome::Waiting, "safety stop", detail);
                    self.log_once(&mut store, format!("stop {:x}", b.fingerprint.0), e)?;
                }
                // Every round while it stands, and the one after the answer,
                // plans all of its list: a full pass's stop would otherwise
                // shrink to what the marks cover (a new code, or none).
                let mark = self.stops_lock().raise(&review.breakers);
                if !mark.is_empty() {
                    store.write(|o| o.dirty_mark_synced(&mark))?;
                }
                self.save_stop_lists(&store, &review);
            }
            if review.breakers.iter().any(|b| b.kind != BreakerKind::Removal) {
                // Nothing at all runs in a tripped round, not even the side
                // that didn't trip: the user should see the whole picture
                // first. The marks stay, so the next try plans it all again.
                out.breakers = review.breakers;
                out.end = End::Stopped;
                return Ok((store, out));
            }
            // Removing chosen duplicates waits for its own approval and
            // holds nothing else back. Otherwise no stop stands: their
            // lists go.
            if review.breakers.is_empty() {
                crate::stoplist::keep_only(&self.data_dir, &[]);
            }
            out.breakers.clone_from(&review.breakers);
            out.totals.held = review.withheld.iter().filter(|(_, w)| *w == Withheld::Held).count();
            // Held only for want of evidence, but in the user's Recycle Bin:
            // deleted by the user (`crate::recyclebin`); planned again.
            let recycled = self.note_recycled(&mut store, &review.withheld)?;
            // Named for `oneup unsynced`: a count alone told nobody what.
            let held: Vec<(String, crate::unsynced::Why)> = store.read(|o| {
                let mut v = Vec::new();
                for (op, w) in &review.withheld {
                    if let (Withheld::Held, Op::Hold { reason, local, .. }) = (w, op) {
                        // Which side lost it: this PC, if its local item is gone.
                        let here = match local {
                            Some(l) => o.local_get(l.0)?.is_some(),
                            None => false,
                        };
                        v.push((describe(o, op).path, crate::unsynced::Why::of_hold(*reason, !here)));
                    }
                }
                Ok(v)
            })?;
            // Work the gate put off (a paused direction, a batch waiting for
            // the user's answer) is planned again from its folders' marks,
            // so resuming or answering finds it.
            let deferred = review
                .withheld
                .iter()
                .any(|(_, w)| matches!(w, Withheld::Paused(_) | Withheld::Panic | Withheld::Overflow));
            if let Some(o) = &review.overflow {
                out.overflow = Some(o.clone());
                let size = crate::versions::size;
                let (outcome, what) = match o.outcome {
                    OverflowOutcome::Held => (ActivityOutcome::Waiting, "waiting for your decision".to_string()),
                    OverflowOutcome::KeptByPolicy => (ActivityOutcome::Note, "kept anyway (your setting)".into()),
                    OverflowOutcome::Decided(Choice::Keep) => (ActivityOutcome::Note, "kept, as you chose".into()),
                    OverflowOutcome::Decided(Choice::Discard) => {
                        (ActivityOutcome::Note, "removed without a local copy, as you chose".into())
                    }
                };
                let detail = format!(
                    "{} files ({}) don't fit (room for {}): {what} (code {:016x})",
                    o.count,
                    size(o.bytes),
                    size(o.room),
                    o.fingerprint.0
                );
                let key = format!("overflow {:x} {:?}", o.fingerprint.0, o.outcome);
                self.log_once(&mut store, key, notice(outcome, "version store full", detail))?;
            }
            let trim = crate::versions::trim_settings(&store)?;
            if trim.rotate && review.store_bytes > 0 {
                let removed = crate::versions::make_room(self.exec.local(), &trim, review.store_bytes)?;
                if let Some(d) = crate::versions::removed_detail(&removed) {
                    log_activity(&mut store, vec![notice(ActivityOutcome::Note, "rotated", d)])?;
                }
            }
            // What the user wants first goes first ([priority]; transfers only).
            let (mut first, last) = crate::config::kept(&self.data_dir, crate::config::load_priority(&self.data_dir))?;
            for p in &controls.first {
                first.extend(oneup_rules::literal_path_rules(p));
            }
            let order = crate::priority::Priority::new(&first, &last);
            let on_demand = self.exec.local().on_demand();
            let planned: Vec<(Approved, Line)> = store.read(|o| {
                Ok(review
                    .approved
                    .into_iter()
                    .map(|a| {
                        let mut l = describe_approved(o, &a);
                        if chosen.ops.iter().any(|(op, _)| op == a.op()) {
                            l.note = "as you chose".into();
                        }
                        if oneup_gate::is_removal(a.op()) {
                            l = removal_line(o, a.op());
                        }
                        let l = if on_demand { on_demand_line(a.op(), l) } else { l };
                        (a, l)
                    })
                    .collect())
            })?;
            let planned = crate::priority::apply(&order, planned, |(a, l)| (a.op(), l.path.as_str()));
            // Files that keep changing wait (`crate::busy`); their folders
            // stay marked, so a later round plans them again.
            let now = std::time::Instant::now();
            let (planned, busy, until) = hold_back(planned, |(a, _)| self.held_until(a.op(), now));
            let waiting: Vec<(Op, Line)> = busy.into_iter().map(|(a, l)| (a.op().clone(), l)).collect();
            // While OneDrive is full, nothing new goes there (uploads, and
            // new folders, which it may refuse as well); failed operations
            // wait until their time (`crate::backoff`). Their folders stay
            // marked too, and everything else goes.
            let (planned, no_room): (Vec<_>, Vec<_>) = if self.full_lock().is_some() {
                planned.into_iter().partition(|(a, _)| !matches!(a.op(), Op::Upload { .. } | Op::MkdirRemote { .. }))
            } else {
                (planned, Vec::new())
            };
            let (planned, failing, retry_at) = store.read(|o| self.hold_failed(o, planned))?;
            out.held_until = earliest(until, retry_at);
            let mut later: Vec<Op> = waiting.iter().map(|(op, _)| op.clone()).collect();
            later.extend(no_room.iter().chain(&failing).map(|(a, _)| a.op().clone()));
            let wait = waiting.iter().filter(|(op, _)| matches!(op, Op::Upload { .. })).map(|(_, l)| {
                let detail = "changing often; sent once it's left alone for a moment".to_string();
                ActivityEntry {
                    seq: 0,
                    at_ms: now_ms(),
                    outcome: ActivityOutcome::Waiting,
                    action: l.action.into(),
                    path: l.path.clone(),
                    detail,
                }
            });
            log_activity(&mut store, wait.collect())?;
            let (approved, planned): (Vec<Approved>, Vec<(Op, Line)>) = planned
                .into_iter()
                .map(|(a, l)| {
                    let op = a.op().clone();
                    (a, (op, l))
                })
                .unzip();
            let listed_ops = std::mem::take(&mut review.listed);
            if let Some(keep) = keep {
                // Counted before anything runs: a crash mid-round must not
                // forget what it may have done.
                keep(&mut store, reserved(*executed, &approved, &suspicious))
                    .context("can't record the safety window, so nothing was changed")?;
            }
            let bytes_total = store.read(|o| transfer_bytes(o, &approved, on_demand))?;
            let (approvals, decisions) = (controls.answers.approvals(), controls.answers.decisions());
            let follower = Follower {
                engine: self,
                planning: &planning,
                policy: &policy,
                cx: GateContext {
                    synced_items: synced,
                    recent: window,
                    approvals: &approvals,
                    approved: &listed,
                    pending: &[],
                    paths: &[],
                    pending_paths: &[],
                    decisions: &decisions,
                    room: None,
                    suspicious: &[],
                },
                order: &order,
                on_demand,
                taken: std::sync::Mutex::default(),
                items: std::sync::atomic::AtomicU64::new(0),
                bytes: std::sync::atomic::AtomicU64::new(0),
            };
            let report = self
                .run_reported(&mut store, approved, n, bytes_total, progress, controls.live.as_deref(), &follower)
                .await;
            // What went into the new folders ran after the rest: reported last.
            let planned = [planned, follower.taken.into_inner().unwrap_or_else(|p| p.into_inner())].concat();
            // Paused meanwhile: what didn't run is planned again from the
            // marks, which stay, as for work the gate put off.
            let deferred = deferred || !report.not_run.is_empty();
            // Counted before anything below can fail: what ran must reach
            // the caller's safety window even if this call ends in an error.
            for op in &report.done {
                executed.add_op(op, &suspicious);
            }
            // Approved changes that ran are done with; those that wait stay.
            self.stops_lock().round_ended(listed_ops, &report.done);
            crate::resolve::ran(&mut store, &chosen.ops, &report)?;
            if let Some(r) = &removal {
                let log = crate::duplicates::ran(&mut store, r, &report)?;
                log_activity(&mut store, log)?;
            }
            let mut log = print_round(n, &planned, &report);
            self.note_databases(&mut store, &planned, &report, &db_settings.keep_syncing, &user_rules)?;
            if !report.not_run.is_empty() {
                let detail = format!("{} operations wait until sync resumes", report.not_run.len());
                println!("  {detail}");
                log.push(notice(ActivityOutcome::Waiting, "paused", detail));
            }
            if let Some(why) = &report.halted {
                log.push(notice(ActivityOutcome::Failed, "stopped", why.clone()));
            }
            if let Some(feed) = self.events.get() {
                feed.push(crate::events::from_activity(&log));
            }
            // A database that fails can't take the log either; the halt
            // below still reports it.
            let logged = log_activity(&mut store, log);
            if let Some(why) = report.halted {
                return Err(Halted(why).into());
            }
            logged?;
            if let Some(keep) = keep {
                keep(&mut store, *executed).context("can't record the safety window")?;
            }
            // Failures wait their turn; what was done is through with them.
            let failures = if self.backs_off() {
                store.write(|o| crate::backoff::record(o, &report, now_ms()))?
            } else {
                crate::backoff::Recorded::default()
            };
            if let Some(first) = failures.first_ms {
                out.held_until = earliest(out.held_until, Some(after_ms(std::time::Instant::now(), first - now_ms())));
            }
            if report.skipped.iter().any(|(_, s)| *s == Skip::CloudFull) {
                self.found_full(&mut store).await?;
            }
            out.totals.add(&report);
            let (clean, mut end) = settle(&report, failures.waiting, &mut changed_rounds);
            if recycled > 0 && matches!(end, Some(End::Settled)) {
                end = None; // another round, to carry the deletions out
            }
            self.note_unsynced(&mut store, &user_rules, &out.totals.problems, held, &planned, &report, planned_all)?;
            // The round ran to its end (a halt returned above). Unless the
            // gate put work off (planned again from the marks, which stay),
            // the marks are used up, except for the folders of what's
            // retried; and the rules have been applied everywhere, so the
            // next round needn't plan every folder. Keeping every mark for
            // one skipped file made every later round plan the whole tree.
            if !deferred {
                // Kept for what's tried again later: skips unless only a
                // change can end them, failures, uploads OneDrive had no
                // room for, and what waited.
                let mut retried = Touched::default();
                let skipped = report.skipped.iter().filter(|(_, s)| !clean || *s == Skip::CloudFull);
                for op in skipped.map(|(op, _)| op).chain(report.failed.iter().map(|(op, _)| op)).chain(&later) {
                    retried.add(op);
                }
                store.write(|o| {
                    o.dirty_clear_except(upto, &retried.local, &retried.remote, &retried.synced)?;
                    o.meta_set(KEY_RULES, &fingerprint)?;
                    if planned_all {
                        o.meta_set(KEY_FULL_PASS, env!("CARGO_PKG_VERSION"))?;
                    }
                    Ok(())
                })?;
            }
            if let Some(end) = end {
                if end == End::Settled {
                    // Everything approved ran, or no longer applies.
                    self.stops_lock().settled();
                }
                out.end = end;
                return Ok((store, out));
            }
        }
        Ok((store, out))
    }

    /// Items held as missing without evidence whose path (or a folder
    /// above it) the user sent to this PC's Recycle Bin: their deletion is
    /// noted as evidence, and their folders marked for planning. Returns
    /// how many. Reads the bin's index only when there are such holds.
    fn note_recycled(&self, store: &mut Store, withheld: &[(Op, Withheld)]) -> Result<usize> {
        let missing: Vec<(&Op, LocalId, Option<oneup_sync::Sid>)> = withheld
            .iter()
            .filter_map(|(op, w)| match (w, op) {
                (Withheld::Held, Op::Hold { sid, local: Some(l), reason: HoldReason::MissingWithoutEvidence, .. }) => {
                    Some((op, *l, *sid))
                }
                _ => None,
            })
            .collect();
        if missing.is_empty() {
            return Ok(0);
        }
        let Some(root) = store.read(|o| o.local_root())? else { return Ok(0) };
        let root = PathBuf::from(&root.path);
        let bin = crate::recyclebin::originals(&root);
        if bin.is_empty() {
            return Ok(0);
        }
        // Only items no longer here: their path is the synced one.
        let found: Vec<(LocalId, Option<oneup_sync::Sid>, String)> = store.read(|o| {
            let mut v = Vec::new();
            for (op, l, sid) in &missing {
                if o.local_get(l.0)?.is_some() {
                    continue;
                }
                let rel = describe(o, op).path;
                let full = rel.trim_start_matches('/').split('/').fold(root.clone(), |p, x| p.join(x));
                if crate::recyclebin::sent(&full, &bin) {
                    v.push((*l, *sid, rel));
                }
            }
            Ok(v)
        })?;
        if found.is_empty() {
            return Ok(0);
        }
        let sids: Vec<oneup_sync::Sid> = found.iter().filter_map(|(_, s, _)| *s).collect();
        store.write(|o| {
            for (l, _, _) in &found {
                o.note_local_deleted(l.0)?;
            }
            o.dirty_mark_synced(&sids)
        })?;
        let n = found.len();
        tracing::info!(n, "items in the Recycle Bin count as deleted");
        let detail = format!(
            "{n} item(s) you deleted on this PC (they're in its Recycle Bin) will be deleted in OneDrive too, into its recycle bin"
        );
        log_activity(store, vec![notice(ActivityOutcome::Note, "recycle bin", detail)])?;
        Ok(n)
    }

    /// Keeps the record of what can't sync (`crate::unsynced`): the
    /// planner's problem items, what the gate held and the skips only a
    /// change can end are added, what was done leaves it. `full`: every
    /// folder was planned.
    #[allow(clippy::too_many_arguments)]
    fn note_unsynced(
        &self,
        store: &mut Store,
        rules: &RuleSet,
        problems: &[(String, Why)],
        held: Vec<(String, crate::unsynced::Why)>,
        planned: &[(Op, Line)],
        report: &Report,
        full: bool,
    ) -> Result<()> {
        use crate::unsynced::Why as U;
        let mut found: Vec<(String, U)> = problems.iter().map(|(p, w)| (p.clone(), U::from(*w))).collect();
        // A synced item that can't sync any more (a refused name, too
        // large) is held as missing on one side; why is the problem's.
        found.extend(held.into_iter().filter(|(p, _)| !problems.iter().any(|(q, _)| oneup_sync::same_name(q, p))));
        let mut done: Vec<String> = Vec::new();
        // The report keeps the planned order (see `print_round`): one pass.
        let (mut d, mut s) = (report.done.iter().peekable(), report.skipped.iter().peekable());
        for (op, line) in planned {
            if d.next_if(|x| *x == op).is_some() {
                done.push(line.path.clone());
            } else if let Some((_, why)) = s.next_if(|(o, _)| o == op) {
                match why {
                    Skip::Refused => found.push((line.path.clone(), U::Special)),
                    Skip::NameTaken => found.push((line.path.clone(), U::NameTaken)),
                    Skip::NotEmpty => found.push((line.path.clone(), U::NotEmpty)),
                    _ => {}
                }
            }
        }
        let had = store.read(|o| o.meta_get(crate::unsynced::KEY))?.is_some();
        if found.is_empty() && !had {
            return Ok(());
        }
        let Some(root) = store.read(|o| o.local_root())? else { return Ok(()) };
        crate::unsynced::update(store, Path::new(&root.path), rules, found, done.iter().map(String::as_str), full)
    }

    /// Carries out round `round`'s operations, telling `progress` how far
    /// it is about once a second (and `None` once it's over). Bytes are
    /// counted where every transfer passes, the bandwidth caps, so a large
    /// file moves them while it's on its way. With `live`, a pause that
    /// comes meanwhile stops the paused side's operations from then on.
    #[allow(clippy::too_many_arguments)]
    async fn run_reported(
        &self,
        store: &mut Store,
        approved: Vec<Approved>,
        round: usize,
        bytes_total: u64,
        progress: &(dyn Fn(Option<Progress>) + Send + Sync),
        live: Option<&crate::control::LivePause>,
        follower: &Follower<'_>,
    ) -> Report {
        use std::sync::atomic::{AtomicU64, Ordering::Relaxed};
        let items_total = approved.len() as u64;
        let items_done = AtomicU64::new(0);
        let moved = || self.graph.upload.moved() + self.graph.download.moved();
        let moved_before = moved();
        let started = std::time::Instant::now();
        let tell = || {
            // What went into new folders counts as it's taken in.
            let bytes_total = bytes_total + follower.bytes.load(Relaxed);
            progress(Some(Progress {
                round,
                items_done: items_done.load(Relaxed),
                items_total: items_total + follower.items.load(Relaxed),
                bytes_done: moved().saturating_sub(moved_before).min(bytes_total),
                bytes_total,
                elapsed_ms: u64::try_from(started.elapsed().as_millis()).unwrap_or(u64::MAX),
            }));
        };
        let count = |n: usize| {
            items_done.fetch_add(n as u64, Relaxed);
        };
        let paused = live.map(crate::control::LivePause::round_check);
        let may_run = |op: &Op| paused.as_ref().is_none_or(|p| p(op));
        let follow = |o: &Ops<'_>, made: &[oneup_sync::Sid], room: usize| follower.follow(o, made, room);
        let run = self.exec.run_following(store, approved, &count, &may_run, &follow, oneup_exec::FOLLOW_MAX);
        tokio::pin!(run);
        let mut tick = tokio::time::interval(std::time::Duration::from_secs(1));
        tick.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);
        let report = loop {
            tokio::select! {
                r = &mut run => break r,
                _ = tick.tick() => tell(),
            }
        };
        progress(None);
        self.timings.log();
        report
    }

    /// Applies the automatic version-store trimming rule (after rounds,
    /// never during one).
    pub fn trim(&self, store: &mut Store) -> Result<()> {
        let removed = crate::versions::auto_trim(self.exec.local(), store)?;
        if let Some(d) = crate::versions::removed_detail(&removed) {
            log_activity(store, vec![notice(ActivityOutcome::Note, "trimmed", d)])?;
        }
        Ok(())
    }
}

/// Plans one round and has the gate review it. `answers`: codes the user
/// gave. A code is a batch's fingerprint, so it answers for exactly the
/// batch that was shown and nothing else (one more deletion makes a
/// different code). `fs`: the sync folder, to see how much room its version
/// store has (only listed when the plan would add to it; without it, the
/// store is taken to have no limit, which only a first dry run does).
/// `approved`: items on lists the user approved (sorted).
/// The fingerprint a round plans with: the rules', and the database
/// patterns' (`oneup_plan::park` parks SQLite's working files by them), so
/// a change to either plans every folder again. Without database patterns,
/// the rules' alone, as before there were any.
fn planning_fingerprint(rules: &RuleSet, databases: &oneup_rules::database::Databases) -> String {
    let d = databases.fingerprint();
    if d.is_empty() {
        return rules.fingerprint();
    }
    // FNV-1a: short enough to keep in `meta`.
    let h = d.bytes().fold(0xcbf2_9ce4_8422_2325u64, |h, b| (h ^ u64::from(b)).wrapping_mul(0x0100_0000_01b3));
    format!("{} databases {h:016x}", rules.fingerprint())
}

#[allow(clippy::too_many_arguments)]
fn plan_round<'a>(
    store: &Store,
    fs: Option<&LocalFs>,
    scope: Scope,
    controls: &Controls,
    recent: Recent,
    rules: &'a RuleSet,
    modes: Modes,
    approved: &[ItemKey],
) -> Result<Planned<'a>> {
    let approvals = controls.answers.approvals();
    let decisions = controls.answers.decisions();
    let policy = oneup_gate::Policy {
        pause_local: controls.pause_local,
        pause_remote: controls.pause_remote,
        ..crate::versions::policy(store)?
    };
    let synced = store.read(|o| o.synced_stats())?.items.max(0) as u64;
    // Databases as the executor has them (`[databases]`): their SQLite
    // working files are parked beside them.
    let databases = fs.map(LocalFs::databases).unwrap_or_default();
    let planning = Planning::new(rules, modes.pull, crate::flow::get(store)?, databases);
    let (ops, problems, upto, pending) = store.read(|o| {
        let dirty = o.dirty_scope()?;
        let folders = match scope {
            Scope::All => all_folders(&Parked::with_rules(o, &planning.hide))?,
            Scope::Dirty => dirty.folders,
        };
        let (mut ops, problems) = planning.plan(o, folders)?;
        // The folders its deletions empty, for a stop to list with them.
        // What a folder holds is read unfiltered: an excluded or parked
        // item keeps it from being emptied.
        let parked = Parked::with_rules(o, &planning.hide).with_databases(&planning.databases);
        let pending = oneup_sync::pending_deletes(&parked, o, &ops, planning.flow)?;
        add_extra(&mut ops, modes.extra.clone());
        // Removing chosen duplicates waits for a round whose plan doesn't
        // touch what it removes or keeps (`oneup_exec::dupes::touched`):
        // the plan's own deletion of the copy that stays, or a move of one
        // that goes, could otherwise run in the same round.
        if !oneup_exec::dupes::touched(&ops, &modes.removal) {
            ops.extend(modes.removal.iter().cloned());
        }
        let problems = problems
            .into_iter()
            .map(|(id, name, why)| Ok((o.local_path_of(id.0)?.unwrap_or(name), why)))
            .collect::<oneup_store::Result<Vec<_>>>()?;
        Ok((ops, problems, dirty.upto, pending))
    })?;
    let room = match fs {
        Some(fs) if store_bytes(&policy, &ops) > 0 => fs.room(&crate::versions::trim_settings(store)?)?,
        _ => None,
    };
    let suspicious = match fs {
        Some(fs) if policy.max_suspicious > 0 => judge_content(fs, &ops),
        _ => Vec::new(),
    };
    // The paths a stop's list shows are part of what an approval covers.
    // Read only when something could trip or a list is approved.
    // A removal the user chose always is (`oneup_gate::always_stops`).
    let (paths, pending_paths) =
        if oneup_gate::may_stop(&policy, &ops, &pending, recent, !approved.is_empty(), !suspicious.is_empty()) {
            store.read(|o| Ok((stop_paths(o, &ops), stop_paths(o, &pending))))?
        } else {
            (Vec::new(), Vec::new())
        };
    let cx = GateContext {
        synced_items: synced,
        recent,
        approvals: &approvals,
        approved,
        pending: &pending,
        paths: &paths,
        pending_paths: &pending_paths,
        decisions: &decisions,
        room,
        suspicious: &suspicious,
    };
    let review = review_in(&policy, &cx, ops);
    Ok(Planned { review, problems, upto, suspicious, policy, synced, planning })
}

/// Adds `extra` (prunes, the renames the user chose) to a round's plan,
/// after the planner's own resolve pass. A chosen rename that would meet
/// what the plan does (the same item, or a name the plan needs in that
/// folder, another chosen rename's included) waits for a later round: it
/// stays queued (`crate::resolve::ran`) instead of being skipped there.
fn add_extra(ops: &mut Vec<Op>, extra: Vec<Op>) {
    if !extra.iter().any(|e| matches!(e, Op::RenameLocal { .. })) {
        ops.extend(extra);
        return;
    }
    let mut taken: std::collections::HashSet<(oneup_sync::Dir, String)> =
        ops.iter().filter_map(|o| o.target().map(|(d, n)| (d, oneup_sync::fold(n)))).collect();
    let busy: std::collections::HashSet<LocalId> =
        ops.iter().filter(|o| o.is_action()).filter_map(|o| o.subjects().1).collect();
    for e in extra {
        if let Op::RenameLocal { local, dir, name } = &e {
            let target = (oneup_sync::Dir::Local(*dir), oneup_sync::fold(name));
            if busy.contains(&local.id) || !taken.insert(target) {
                continue;
            }
        }
        ops.push(e);
    }
}

/// How a round plans: over the [`Parked`] view of the trees, with what
/// pull mode never sends dropped.
struct Planning<'a> {
    /// Exclusions only stop uploads (`pull`, `prune`): excluded items stay
    /// visible to planning (only paused folders, and those neither side
    /// lists, are hidden), and what would change OneDrive for them is
    /// dropped (`judge`).
    hide: RuleSet,
    judge: Option<&'a RuleSet>,
    flow: oneup_sync::Flow,
    /// What counts as a database (`oneup_plan::park`: SQLite's working
    /// files beside one are parked).
    databases: oneup_rules::database::Databases,
}

impl<'a> Planning<'a> {
    fn new(
        rules: &'a RuleSet,
        pull: bool,
        flow: oneup_sync::Flow,
        databases: oneup_rules::database::Databases,
    ) -> Self {
        let hide = if pull { rules.upload_only() } else { rules.clone() };
        Self { hide, judge: pull.then_some(rules), flow, databases }
    }

    /// The plan for `folders`, and the items parking kept out of it.
    #[allow(clippy::type_complexity)]
    fn plan(
        &self,
        o: &Ops<'_>,
        folders: Vec<oneup_sync::Sid>,
    ) -> oneup_store::Result<(Vec<Op>, Vec<(LocalId, String, Why)>)> {
        let p = Parked::with_rules(o, &self.hide).with_databases(&self.databases);
        let mut ops = plan_flow(&p, folders, self.flow)?;
        if let Some(rules) = self.judge {
            let judge = Parked::with_rules(o, rules);
            let mut kept = Vec::with_capacity(ops.len());
            for op in ops {
                if !judge.sends_excluded(&op)? {
                    kept.push(op);
                }
            }
            ops = kept;
        }
        Ok((ops, p.problems()))
    }
}

/// What goes into the folders a round makes, planned and reviewed while it
/// runs (`Executor::run_following`): planned as the round was, reviewed by
/// the gate with the round's policy, answers and window, and held back as
/// the round's own operations are (files that keep changing, uploads while
/// OneDrive is full, failures waiting their turn). What it leaves out is in
/// a new folder, whose mark brings it to the next round.
struct Follower<'a> {
    engine: &'a Engine,
    planning: &'a Planning<'a>,
    policy: &'a oneup_gate::Policy,
    cx: GateContext<'a>,
    order: &'a crate::priority::Priority,
    on_demand: bool,
    /// What it took in, described, in the order it was taken in.
    taken: std::sync::Mutex<Vec<(Op, Line)>>,
    /// How many operations, and the bytes their transfers add up to.
    items: std::sync::atomic::AtomicU64,
    bytes: std::sync::atomic::AtomicU64,
}

impl Follower<'_> {
    /// At most `room` operations: the rest stays in the new folders, whose
    /// marks bring them to the next round.
    fn follow(&self, o: &Ops<'_>, made: &[oneup_sync::Sid], room: usize) -> Vec<Approved> {
        self.try_follow(o, made, room).unwrap_or_else(|e| {
            tracing::debug!(error = %e, "what goes into new folders is left to the next round");
            Vec::new()
        })
    }

    fn try_follow(&self, o: &Ops<'_>, made: &[oneup_sync::Sid], room: usize) -> oneup_store::Result<Vec<Approved>> {
        use std::sync::atomic::Ordering::Relaxed;
        let (ops, _) = self.planning.plan(o, made.to_vec())?;
        // Without mass-sensitive operations no breaker can trip, so the gate
        // here only withholds what a pause, a panic or a hold does.
        if !oneup_gate::may_follow(&ops) {
            return Ok(Vec::new());
        }
        let review = review_in(self.policy, &self.cx, ops);
        let planned: Vec<(Approved, Line)> = review
            .approved
            .into_iter()
            .map(|a| {
                let l = describe_approved(o, &a);
                let l = if self.on_demand { on_demand_line(a.op(), l) } else { l };
                (a, l)
            })
            .collect();
        let planned = crate::priority::apply(self.order, planned, |(a, l)| (a.op(), l.path.as_str()));
        let now = std::time::Instant::now();
        let (planned, _, _) = hold_back(planned, |(a, _)| self.engine.held_until(a.op(), now));
        let full = self.engine.full_lock().is_some();
        let planned = planned
            .into_iter()
            .filter(|(a, _)| !(full && matches!(a.op(), Op::Upload { .. } | Op::MkdirRemote { .. })));
        let (planned, _, _) = self.engine.hold_failed(o, planned.collect())?;
        let (approved, lines): (Vec<Approved>, Vec<(Op, Line)>) = planned
            .into_iter()
            .take(room)
            .map(|(a, l)| {
                let op = a.op().clone();
                (a, (op, l))
            })
            .unzip();
        self.items.fetch_add(approved.len() as u64, Relaxed);
        self.bytes.fetch_add(transfer_bytes(o, &approved, self.on_demand)?, Relaxed);
        self.taken.lock().unwrap_or_else(|p| p.into_inner()).extend(lines);
        Ok(approved)
    }
}

/// At most this many files a round reads the beginning of for the content
/// guard (a mass encryption trips it long before).
const JUDGE_MAX: usize = 2_000;

/// The files a round would send over their OneDrive copy whose new content
/// doesn't look like their type (`oneup_gate::content`), sorted. Only
/// edits of existing files are judged: that's what encrypting files in
/// place looks like, and a new file has no earlier version to protect.
fn judge_content(fs: &LocalFs, ops: &[Op]) -> Vec<LocalId> {
    let mut out: Vec<LocalId> = ops
        .iter()
        .filter_map(|op| match op {
            Op::Upload { local, name, replace: Some(_), .. } => Some((local, name)),
            _ => None,
        })
        .take(JUDGE_MAX)
        .filter(|(local, name)| {
            fs.sample(local, oneup_gate::content::HEAD)
                .is_some_and(|head| oneup_gate::content::looks_wrong(name, &head))
        })
        .map(|(local, _)| local.id)
        .collect();
    out.sort();
    out
}

/// What happens to OneDrive's copies of excluded items in a round.
#[derive(Default)]
struct Modes {
    /// `pull` or `prune`: bring them here, never send them
    /// (`Parked::sends_excluded`).
    pull: bool,
    /// `prune`: their removal, added to the plan for the gate; and the
    /// renames and removals the user chose.
    extra: Vec<Op>,
    /// The removal of duplicates the user chose, added unless the plan
    /// touches what it removes or keeps.
    removal: Vec<Op>,
}

/// One round's reviewed plan.
struct Planned<'a> {
    review: oneup_gate::Review,
    /// Items that can't be synced, for the report.
    problems: Vec<(String, Why)>,
    /// The dirty marks' high-water number, read *before* planning: once the
    /// round's operations all ran, everything marked up to it is done.
    upto: i64,
    /// Files judged by the content guard (sorted).
    suspicious: Vec<LocalId>,
    /// What it was reviewed with and planned by, for what goes into the
    /// folders it makes ([`Follower`]).
    policy: oneup_gate::Policy,
    synced: u64,
    planning: Planning<'a>,
}

async fn dry_run(
    store: Store,
    rules: &ExcludeSet,
    answers: &Answers,
    first: bool,
    data_dir: &Path,
    limit: Option<usize>,
) -> Result<()> {
    let store = observe(store, rules).await?;
    let (user_rules, ignored) = crate::rulefiles::load(data_dir, &store, &mut crate::rulefiles::Cache::default())?;
    for (path, why) in ignored {
        println!("Rule file {path} ignored: {why}");
    }
    // Read-only here: only the version store's size is looked at.
    let fs = if first { None } else { open_paired(&store).ok() };
    let controls = Controls { answers: answers.clone(), ..Controls::default() };
    let pull = crate::config::load_rules(data_dir, &store)?.cloud != crate::config::CloudCopies::Keep;
    let modes = Modes { pull, ..Modes::default() };
    let Planned { review, problems, .. } =
        plan_round(&store, fs.as_ref(), Scope::All, &controls, Recent::default(), &user_rules, modes, &[])?;
    println!("Dry run: nothing is changed. The first round would do:");
    let limit = limit.unwrap_or(usize::MAX);
    let mut totals = std::collections::BTreeMap::<&str, u64>::new();
    let mut shown = 0usize;
    store.read(|o| {
        for a in &review.approved {
            let line = describe_approved(o, a);
            *totals.entry(line.action).or_default() += 1;
            if shown < limit {
                println!("  {line}");
                shown += 1;
            }
        }
        for (op, why) in &review.withheld {
            *totals.entry(withheld_label(*why)).or_default() += 1;
            if shown < limit {
                println!("  [{}] {}", withheld_label(*why), describe(o, op));
                shown += 1;
            }
        }
        // A stop's folders to delete once emptied, in later rounds.
        let needs = withheld_label(Withheld::Breaker(Side::Remote));
        for op in review.breakers.iter().flat_map(|b| &b.later) {
            *totals.entry(needs).or_default() += 1;
            if shown < limit {
                println!("  [{needs}] {}", later_line(o, op));
                shown += 1;
            }
        }
        Ok(())
    })?;
    let later: usize = review.breakers.iter().map(|b| b.later.len()).sum();
    let all = review.approved.len() + review.withheld.len() + later;
    if all == 0 {
        println!("  nothing: both sides agree");
    } else {
        if all > shown {
            println!("  … and {} more", all - shown);
        }
        let parts: Vec<String> = totals.iter().map(|(a, n)| format!("{a} {n}")).collect();
        println!("Totals: {}", parts.join(", "));
    }
    print_problems(&problems);
    print_breakers(&review.breakers, Asker::Sync);
    if let Some(o) = &review.overflow {
        print_overflow(o, &store, Asker::Sync)?;
    }
    println!("Not listed: what goes into new folders, planned as each is made, and what later rounds find.");
    Ok(())
}

/// What listing skips by folder name in the data folder `data_dir`: the
/// name-only part of its rules, whatever happens to OneDrive's copies
/// (nothing in such a folder is brought here: `RuleSet::upload_only`).
pub(crate) fn pruning_for(data_dir: &Path, store: &Store, cache: &mut crate::rulefiles::Cache) -> Result<ExcludeSet> {
    Ok(crate::rulefiles::load(data_dir, store, cache)?.0.pruning())
}

pub(crate) async fn mirror(graph: &Arc<Graph>, store: Store, rules: &ExcludeSet) -> Result<Store> {
    mirror_with(graph, store, rules, true).await
}

/// [`mirror`], checking the store's invariants only if `verify` (see
/// `Mirror::update`). A listing from scratch is always checked.
pub(crate) async fn mirror_with(graph: &Arc<Graph>, store: Store, rules: &ExcludeSet, verify: bool) -> Result<Store> {
    let cfg = MirrorConfig {
        list_depth: 2,
        max_list_depth: 6,
        split_bytes: 256 << 20,
        rules: rules.clone(),
        max_tasks: 8,
        hint: None,
    };
    let m = Mirror::new(Arc::clone(graph), store, cfg.clone());
    let done = if verify { m.run().await.map(drop) } else { m.update().await.map(drop) };
    match done {
        Ok(()) => Ok(m.into_store()?),
        // The change position expired (OneDrive keeps them for a limited
        // time): list OneDrive again, as `oneup mirror --rebuild` does. An
        // item missing from the new listing is "missing without evidence",
        // held and never deleted anywhere on that basis (data-safety.md invariant 3).
        Err(oneup_remote::Error::ResyncRequired) => {
            tracing::warn!("OneDrive's change position expired; listing OneDrive again");
            let mut store = m.into_store()?;
            store.write(|o| o.clear_remote())?;
            let m = Mirror::new(Arc::clone(graph), store, cfg);
            match m.run().await {
                Ok(_) => Ok(m.into_store()?),
                Err(e) => Err(e.into()),
            }
        }
        Err(e) => Err(e.into()),
    }
}

/// Brings the Local tree up to date from the change journal (cheap when
/// nothing in the synced folder changed: the agent calls it on every
/// journal wake to see whether anything needs a round).
pub async fn observe(store: Store, rules: &ExcludeSet) -> Result<Store> {
    observe_with(store, rules, oneup_local::observe::seen_deletes()).await
}

/// [`observe`], with the delete records read ahead in `seen`.
async fn observe_with(
    store: Store,
    rules: &ExcludeSet,
    seen: &'static oneup_local::observe::SeenDeletes,
) -> Result<Store> {
    use oneup_local::observe::{Error, Observer, Rescan};
    let cfg = oneup_local::Config::new(rules.clone());
    tokio::task::spawn_blocking(move || -> Result<Store> {
        let mut store = store;
        // Changed listing rules: adjust the stored tree instead of rescanning.
        let rules = cfg.rules.clone();
        if let Some(r) = store.write(|o| oneup_local::observe::reapply_rules(o, &rules))? {
            let detail = format!(
                "folders skipped while listing changed: {} now skipped, {} to be looked through",
                r.excluded, r.included
            );
            println!("Rules: {detail}.");
            log_activity(&mut store, vec![notice(ActivityOutcome::Note, "rules", detail)])?;
        }
        let mut obs = Observer::open(store, cfg.clone())?;
        match obs.catch_up() {
            Ok(_) => Ok(caught_up(obs, seen)),
            Err(Error::NeedsRescan(why @ (Rescan::JournalWrapped | Rescan::JournalReplaced))) => {
                rescan(obs.into_store(), cfg, why, seen)
            }
            Err(e) => Err(e.into()),
        }
    })
    .await?
}

/// The store of an observer that caught up, forgetting the delete records
/// the watcher read ahead that it has now read itself.
fn caught_up(obs: oneup_local::observe::Observer, seen: &oneup_local::observe::SeenDeletes) -> Store {
    let (journal_id, cursor) = obs.position();
    seen.trim(journal_id, cursor);
    obs.into_store()
}

/// The change journal lost track of the synced folder: expected after the
/// PC (or the agent) was off for a while, because the journal keeps only so
/// much history (`sync-engine.md` § Journal loss). The folder is looked through
/// again. Nothing is inferred from what the scan doesn't find: a synced item
/// missing afterwards is held and reported, never deleted (`data-safety.md`
/// invariant 3). Only the same folder (same volume, same folder ID) is
/// rescanned; anything else stops sync, as before.
fn rescan(
    store: Store,
    cfg: oneup_local::Config,
    why: oneup_local::observe::Rescan,
    seen: &oneup_local::observe::SeenDeletes,
) -> Result<Store> {
    use oneup_local::scan::{Replaced, Start};
    let root = store.read(|o| o.local_root())?.context("no local scan")?;
    println!("The PC's change record can't be followed any more ({why}); looking the synced folder through again …");
    tracing::info!(%why, "rescanning the synced folder");
    let mut scan = oneup_local::Scan::open(store, Path::new(&root.path), cfg.clone())?;
    if scan.start() != Start::Replaced(Replaced::Finished) {
        bail!(Halted(format!("the synced folder can't be rescanned as it was ({:?})", scan.start())));
    }
    let summary = scan.run()?;
    if !summary.finished {
        bail!("the rescan was interrupted");
    }
    let mut store = scan.into_store();
    // Deletions the agent's journal watcher read before the journal wrapped
    // are evidence still (`SeenDeletes`).
    let deleted = oneup_local::observe::note_seen_deletes(&mut store, seen)?;
    let mut detail = format!(
        "{why}; looked through again: {} files and {} folders in {:.1?}",
        summary.stats.files, summary.stats.folders, summary.elapsed
    );
    if deleted > 0 {
        detail.push_str(&format!("; deletions found in the journal: {deleted}"));
    }
    println!("  {detail}");
    log_activity(&mut store, vec![notice(ActivityOutcome::Note, "rescan", detail)])?;
    // Changes made while the scan ran are in the journal from its start.
    let mut obs = oneup_local::observe::Observer::open(store, cfg)?;
    match obs.catch_up() {
        Ok(_) => Ok(caught_up(obs, seen)),
        // Lost again while the scan ran (a busy disk): the tree is the
        // scan's, and the next look at the journal rescans again.
        Err(oneup_local::observe::Error::NeedsRescan(
            why @ (oneup_local::observe::Rescan::JournalWrapped | oneup_local::observe::Rescan::JournalReplaced),
        )) => {
            tracing::info!(%why, "the change journal lost track again during the rescan");
            Ok(obs.into_store())
        }
        Err(e) => Err(e.into()),
    }
}

/// Whether a pair may be the whole drive: once sync is released
/// ([`crate::setup::PRE_RELEASE`]). The setup page then suggests it.
pub const WHOLE_DRIVE_ALLOWED: bool = !crate::setup::PRE_RELEASE;

/// `/A/B` with single slashes, no trailing slash; the drive's top is `/`,
/// or refused before [`WHOLE_DRIVE_ALLOWED`].
pub(crate) fn normalize_remote(p: &str) -> Result<String> {
    let parts: Vec<&str> = p.split(['/', '\\']).filter(|s| !s.is_empty()).collect();
    if parts.is_empty() && WHOLE_DRIVE_ALLOWED {
        return Ok("/".into());
    }
    if parts.is_empty() {
        bail!("pick a OneDrive folder (e.g. --remote /Sync-Test), not the whole drive");
    }
    if parts.iter().any(|s| *s == "." || *s == "..") {
        bail!("the OneDrive folder path can't contain . or ..");
    }
    Ok(format!("/{}", parts.join("/")))
}

/// The ID of the OneDrive folder at `path`, from the mirror, each name
/// compared as Windows and OneDrive compare names (`oneup_sync::fold`).
/// Refused if a part matches more than one folder (names that fold alike
/// can coexist in OneDrive): picking one could pair the wrong folder.
pub fn find_remote(store: &Store, path: &str) -> Result<Option<String>> {
    let Some(root) = store.read(|o| o.remote_root())? else { return Ok(None) };
    let mut cur = root.id;
    let mut so_far = String::new();
    for part in path.split('/').filter(|s| !s.is_empty()) {
        so_far = format!("{so_far}/{part}");
        let mut found = store.read(|o| o.remote_named(&cur, part))?.into_iter().filter(|r| r.kind == Kind::Folder);
        let Some(row) = found.next() else { return Ok(None) };
        if let Some(other) = found.next() {
            bail!(
                "{so_far} matches more than one OneDrive folder ({} and {}); rename one of them",
                row.name,
                other.name
            );
        }
        cur = row.id;
    }
    Ok(Some(cur))
}

/// Creates the missing folders of `path` (with `conflictBehavior=fail`, so
/// an existing folder is never replaced).
async fn create_remote(graph: &Arc<Graph>, store: &Store, path: &str) -> Result<()> {
    let cloud = GraphCloud::new(Arc::clone(graph));
    let root = store.read(|o| o.remote_root())?.context("the mirror has no drive root")?;
    let mut parent = RemoteId(root.id);
    let mut so_far = String::new();
    for part in path.split('/').filter(|s| !s.is_empty()) {
        so_far = format!("{so_far}/{part}");
        parent = match find_remote(store, &so_far)? {
            Some(id) => RemoteId(id),
            None => {
                println!("Creating OneDrive folder {so_far}");
                RemoteId(cloud.mkdir(&parent, part).await.map_err(|e| anyhow::anyhow!("creating {so_far}: {e}"))?.id)
            }
        };
    }
    Ok(())
}

/// The marker token: 128 bits from the OS-seeded hasher keys (`RandomState`
/// draws them from the system's random source). It identifies the folder;
/// it isn't a secret.
pub(crate) fn random_token() -> String {
    let h = || std::collections::hash_map::RandomState::new().build_hasher().finish();
    format!("{:016x}{:016x}", h(), h())
}

/// What an operation does to which item, for the run's output and the
/// activity log.
#[derive(Debug, Clone, PartialEq, Eq)]
struct Line {
    action: &'static str,
    /// Relative to the synced folders (`/docs/a.txt`).
    path: String,
    folder: bool,
    /// Where it goes, for moves and renames.
    to: Option<String>,
    note: String,
}

/// The activity log's action for a conflict copy (`Op::ConflictCopy`: this
/// PC's version renamed, OneDrive's taking its name), and its note. The
/// problems list (`crate::problems`) finds the copies by these.
pub const CONFLICT_ACTION: &str = "keep both";
const CONFLICT_NOTE: &str = "changed on both sides";

/// The name a conflict copy got, in the folder of the entry's path, from
/// its activity-log detail.
pub fn conflict_copy_name(detail: &str) -> Option<&str> {
    detail.strip_prefix("-> ")?.strip_suffix(CONFLICT_NOTE)?.strip_suffix("; ")
}

impl Line {
    fn new(action: &'static str, path: String, note: &str) -> Self {
        Self { action, path, folder: false, to: None, note: note.into() }
    }

    /// The activity-log detail: destination and note.
    fn detail(&self) -> String {
        match (&self.to, self.note.is_empty()) {
            (Some(t), true) => format!("-> {t}"),
            (Some(t), false) => format!("-> {t}; {}", self.note),
            (None, _) => self.note.clone(),
        }
    }
}

impl std::fmt::Display for Line {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "{:<12}{}{}", self.action, self.path, if self.folder { "/" } else { "" })?;
        if let Some(t) = &self.to {
            write!(f, " -> {t}")?;
        }
        if !self.note.is_empty() {
            write!(f, "  ({})", self.note)?;
        }
        Ok(())
    }
}

/// One line per operation, with paths relative to the synced folders.
fn describe(o: &Ops<'_>, op: &Op) -> Line {
    describe_as(o, op, OldCopy::Keep)
}

/// [`describe`], given what happens to a removed or replaced file's old copy.
fn describe_as(o: &Ops<'_>, op: &Op, old: OldCopy) -> Line {
    let dropped = old == OldCopy::DropIfSynced;
    let lp = |id: u128| o.local_path_of(id).ok().flatten().unwrap_or_else(|| "(unknown local item)".into());
    // Cloud paths relative to the synced OneDrive folder, like local ones.
    let top = o
        .synced_get(oneup_sync::ROOT)
        .ok()
        .flatten()
        .and_then(|b| b.remote)
        .and_then(|r| o.path_of(&r.0).ok().flatten());
    let rp = |id: &RemoteId| {
        let full = o.path_of(&id.0).ok().flatten().unwrap_or_else(|| "(unknown OneDrive item)".into());
        match top.as_deref().and_then(|t| full.strip_prefix(t)) {
            Some("") => "/".into(),
            Some(rest) if rest.starts_with('/') => rest.into(),
            _ => full,
        }
    };
    let moved = |action, path: String, to: String, note: &str| Line { to: Some(to), ..Line::new(action, path, note) };
    match op {
        Op::Upload { local, replace, .. } => {
            Line::new("upload", lp(local.id.0), if replace.is_some() { "new version" } else { "" })
        }
        Op::Download { remote, replace, .. } => Line::new(
            "download",
            rp(&remote.id),
            match (replace, dropped) {
                (None, _) => "",
                (Some(_), false) => "old version kept in .oneup\\versions",
                (Some(_), true) => "new version; old one in OneDrive's history",
            },
        ),
        Op::MkdirRemote { local, .. } => Line { folder: true, ..Line::new("create", lp(local.id.0), "in OneDrive") },
        Op::MkdirLocal { remote, .. } => Line { folder: true, ..Line::new("create", rp(&remote.id), "on this PC") },
        Op::MoveRemote { remote, to, name, .. } => moved("move", rp(&remote.id), join(&rp(to), name), "in OneDrive"),
        Op::RenameRemote { remote, dir, name } => {
            moved("rename", rp(&remote.id), join(&rp(dir), name), "in OneDrive, freeing the name")
        }
        Op::MoveLocal { local, to, name, .. } => moved("move", lp(local.id.0), join(&lp(to.0), name), "on this PC"),
        Op::RenameLocal { local, name, .. } => moved("rename", lp(local.id.0), name.clone(), "freeing the name"),
        Op::ConflictCopy { local, name, .. } => moved(CONFLICT_ACTION, lp(local.id.0), name.clone(), CONFLICT_NOTE),
        Op::DeleteRemote { remote, .. } => Line::new("delete", rp(&remote.id), "to OneDrive's recycle bin"),
        Op::Dehydrate { local, .. } => Line::new("free up", lp(local.id.0), ""),
        Op::PruneRemote { remote, items, exact, .. } => {
            let n = if *exact { format!("{items} items") } else { format!("at least {items} items") };
            Line {
                folder: *items > 1 || !*exact,
                ..Line::new("prune", rp(&remote.id), &format!("excluded, {n}; to OneDrive's recycle bin, kept here"))
            }
        }
        Op::RecycleLocal { local, .. } if dropped => {
            Line::new("remove", lp(local.id.0), "no copy kept, as you chose; in OneDrive's recycle bin")
        }
        Op::RecycleLocal { local, .. } => Line::new("recycle", lp(local.id.0), "to .oneup\\versions"),
        Op::RemoveCopy { .. } => removal_line(o, op),
        Op::Pair { local, .. } => Line::new("in sync", lp(local.id.0), ""),
        Op::Hash(p) => Line::new("check", lp(p.id.0), "comparing contents"),
        Op::Record(_) | Op::Forget(_) => Line::new("bookkeeping", String::new(), ""),
        Op::Hold { local, remote, reason, .. } => {
            // Held for being missing on one side: the side that still has
            // it names it (the local row is often the one gone).
            let here = local.and_then(|l| o.local_path_of(l.0).ok().flatten());
            let what = match (here, remote) {
                (Some(p), _) => p,
                (None, Some(r)) => rp(r),
                (None, None) => local.map_or_else(|| "(item)".into(), |l| lp(l.0)),
            };
            let why = match reason {
                HoldReason::MissingWithoutEvidence => "missing on one side, no recorded deletion",
                HoldReason::NameClash => "names differ only in case",
                HoldReason::OneWay => "a backup never changes this PC",
            };
            Line::new("held", what, why)
        }
    }
}

/// [`describe`] for an approved operation, which also says what happens to
/// the old copy of a file it removes or replaces.
/// What `approved`'s transfers add up to, in bytes: uploads by their file's
/// size when planned, downloads by the cloud's (none with Files On-Demand,
/// where cloud files arrive as placeholders).
fn transfer_bytes(o: &Ops<'_>, approved: &[Approved], on_demand: bool) -> oneup_store::Result<u64> {
    let mut total = 0u64;
    for a in approved {
        let size = match a.op() {
            Op::Upload { local, .. } => local.stamp.map_or(0, |(size, _)| size),
            Op::Download { remote, .. } if !on_demand => o.get_item(&remote.id.0)?.map_or(0, |r| r.size),
            _ => 0,
        };
        total += u64::try_from(size).unwrap_or(0);
    }
    Ok(total)
}

/// The path a stop's list shows for each of `ops` (what [`describe`] says),
/// for the gate's item keys; empty for what no stop lists.
fn stop_paths(o: &Ops<'_>, ops: &[Op]) -> Vec<String> {
    ops.iter()
        .map(|op| {
            if oneup_gate::is_removal(op) {
                removal_line(o, op).path
            } else if is_mass_sensitive(op) {
                describe(o, op).path
            } else {
                String::new()
            }
        })
        .collect()
}

/// The path a removal stop's list shows for `op` ([`removal_line`]), part
/// of its key: a preview works out the stop's code with it. It's the path
/// the operation carries ([`oneup_sync::Up`]), the one the executor checks
/// the copy is still at, so an approval names exactly where it acts.
pub(crate) fn removal_path(o: &Ops<'_>, op: &Op) -> oneup_store::Result<String> {
    Ok(match op {
        Op::RemoveCopy { remote: Some(_), remote_up, name, .. } => remote_up.path(name),
        Op::RemoveCopy { local: Some(_), local_up, name, .. } => local_up.path(name),
        _ => describe(o, op).path,
    })
}

/// [`describe`] for removing a duplicate the user chose, as its stop's list
/// shows it (and its key holds the path): a cloud copy by its full OneDrive
/// path, since it may be outside the synced folder.
fn removal_line(o: &Ops<'_>, op: &Op) -> Line {
    match op {
        Op::RemoveCopy { remote: Some(_), local, .. } => {
            let path = removal_path(o, op).unwrap_or_else(|_| "(OneDrive item)".into());
            let note = if local.is_some() {
                "duplicate, as you chose; to OneDrive's recycle bin, the copy here to .oneup\\versions"
            } else {
                "duplicate, as you chose; to OneDrive's recycle bin"
            };
            Line::new("remove", path, note)
        }
        Op::RemoveCopy { local: Some(_), .. } => {
            let path = removal_path(o, op).unwrap_or_else(|_| "(local item)".into());
            Line::new("remove", path, "duplicate, as you chose; to .oneup\\versions")
        }
        _ => describe(o, op),
    }
}

/// A folder a stop lists to delete once the deletions on its list emptied
/// it (`oneup_gate::Breaker::later`).
fn later_line(o: &Ops<'_>, op: &Op) -> Line {
    let l = describe(o, op);
    Line { folder: true, note: format!("{}, once empty", l.note), ..l }
}

fn describe_approved(o: &Ops<'_>, a: &Approved) -> Line {
    describe_as(o, a.op(), a.old_copy())
}

/// In Files On-Demand mode, cloud files arrive as placeholders: say so,
/// rather than "download".
fn on_demand_line(op: &Op, line: Line) -> Line {
    match op {
        Op::Download { replace: None, .. } => Line::new("online-only", line.path, ""),
        Op::Download { replace: Some(_), .. } => {
            Line::new("update", line.path, "new version, online-only; old one in OneDrive's history")
        }
        _ => line,
    }
}

fn join(folder: &str, name: &str) -> String {
    format!("{}/{name}", folder.trim_end_matches('/'))
}

fn withheld_label(w: Withheld) -> &'static str {
    match w {
        Withheld::Panic => "stopped",
        Withheld::Paused(_) => "paused",
        Withheld::Breaker(_) => "needs approval",
        Withheld::Malformed => "refused",
        Withheld::Held => "held",
        Withheld::Overflow => "waiting for your decision",
    }
}

/// The items an operation names, whose folders are planned again when it's
/// retried ([`Ops::dirty_clear_except`] marks each one's folder, and the
/// item itself if it's a folder).
#[derive(Default)]
struct Touched {
    local: Vec<LocalId>,
    remote: Vec<RemoteId>,
    synced: Vec<oneup_sync::Sid>,
}

impl Touched {
    fn add(&mut self, op: &Op) {
        let (local, remote, synced) = (&mut self.local, &mut self.remote, &mut self.synced);
        match op {
            Op::Pair { local: l, remote: r, parent, .. } => {
                local.push(l.id);
                remote.push(r.id.clone());
                synced.push(*parent);
            }
            Op::Record(b) => {
                synced.push(b.sid);
                local.extend(b.local);
                remote.extend(b.remote.clone());
            }
            Op::Forget(sid) => synced.push(*sid),
            Op::Hash(l) | Op::Dehydrate { local: l, .. } => local.push(l.id),
            Op::RemoveCopy { sid, local: l, remote: r, .. } => {
                synced.extend(*sid);
                local.extend(l.as_ref().map(|l| l.id));
                remote.extend(r.as_ref().map(|r| r.id.clone()));
            }
            Op::Upload { local: l, to, parent, replace, .. } => {
                local.push(l.id);
                remote.push(to.clone());
                remote.extend(replace.as_ref().map(|r| r.id.clone()));
                synced.push(*parent);
            }
            Op::Download { remote: r, to, parent, replace, .. } => {
                remote.push(r.id.clone());
                local.push(*to);
                local.extend(replace.as_ref().map(|l| l.id));
                synced.push(*parent);
            }
            Op::MkdirRemote { local: l, to, parent, .. } => {
                local.push(l.id);
                remote.push(to.clone());
                synced.push(*parent);
            }
            Op::MkdirLocal { remote: r, to, parent, .. } => {
                remote.push(r.id.clone());
                local.push(*to);
                synced.push(*parent);
            }
            Op::MoveRemote { sid, remote: r, to, parent, .. } => {
                synced.extend([*sid, *parent]);
                remote.extend([r.id.clone(), to.clone()]);
            }
            Op::MoveLocal { sid, local: l, to, parent, .. } => {
                synced.extend([*sid, *parent]);
                local.extend([l.id, *to]);
            }
            Op::RenameLocal { local: l, dir, .. } => local.extend([l.id, *dir]),
            Op::RenameRemote { remote: r, dir, .. } => remote.extend([r.id.clone(), dir.clone()]),
            Op::ConflictCopy { sid, local: l, dir, .. } => {
                synced.push(*sid);
                local.extend([l.id, *dir]);
            }
            Op::DeleteRemote { sid, remote: r } => {
                synced.push(*sid);
                remote.push(r.id.clone());
            }
            Op::RecycleLocal { sid, local: l } => {
                synced.push(*sid);
                local.push(l.id);
            }
            Op::PruneRemote { remote: r, keep, .. } => {
                remote.push(r.id.clone());
                local.extend(keep.iter().map(|l| l.id));
            }
            Op::Hold { sid, local: l, remote: r, .. } => {
                synced.extend(*sid);
                local.extend(*l);
                remote.extend(r.clone());
            }
        }
    }
}

/// What a round's report means: whether the marks it was planned from are
/// used up, and how the rounds end if that was the last thing to do
/// (`None`: another round, at once). `waiting`: of the failures, how many
/// wait their turn (`crate::backoff`). `changed_rounds`: rounds in a row, up
/// to this one, that did nothing but skip items changed meanwhile.
fn settle(report: &Report, waiting: usize, changed_rounds: &mut usize) -> (bool, Option<End>) {
    // Every operation planned from the marks ran: they're used up. A
    // skipped or failed one may have nothing that would mark its folder
    // again (a file open elsewhere changes nothing), so then the marks stay
    // for the retry. A skip that only a change to the item can end counts as
    // done: the change marks it again. Failures that wait their turn, and
    // uploads OneDrive had no room for, are left to later rounds, which hold
    // them (the caller keeps their marks): nothing to retry now.
    let failed = report.failed.len() > waiting;
    let clean = report.skipped.iter().all(|(_, s)| waits_for_a_change(*s) || *s == Skip::CloudFull) && !failed;
    if !report.is_idle() {
        *changed_rounds = 0;
        return (clean, None);
    }
    if clean {
        return (true, Some(End::Settled));
    }
    // A skip because something changed meanwhile is worth an immediate new
    // round (the change is seen now), a few times; anything else, or one
    // that keeps coming back (a stale row a new round doesn't clear), waits
    // for the caller's retry, which backs off.
    let only_changed = !failed
        && report.skipped.iter().all(|(_, s)| matches!(s, Skip::Changed | Skip::CloudFull) || waits_for_a_change(*s));
    if only_changed {
        *changed_rounds += 1;
        if *changed_rounds < CHANGED_ROUNDS {
            return (false, None);
        }
    }
    (false, Some(End::Retry))
}

/// Idle rounds in a row that only skipped items changed meanwhile, before
/// the rounds end in a retry ([`settle`]).
const CHANGED_ROUNDS: usize = 3;

/// Skips that trying again can't end, only a change to the item (which
/// marks its folder, so it's planned again then): an item that can't be
/// synced as it is (a hard link, a name Windows can't use), or a name held
/// by something that isn't synced (a symbolic link, say). Retrying them
/// kept the agent from ever being up to date (2,759 hard-linked files of
/// a conda environment, re-checked every pass).
fn waits_for_a_change(s: Skip) -> bool {
    // A folder to delete that isn't empty: something new in it marks it
    // again; items the rules leave out never would. Retried at once, a
    // OneDrive folder holding only those spun the agent through 20 rounds
    // at a time.
    matches!(s, Skip::Refused | Skip::NameTaken | Skip::NotEmpty)
}

fn skip_label(s: Skip) -> &'static str {
    match s {
        Skip::Changed => "changed meanwhile; retrying",
        Skip::Busy => "open in another program; retrying",
        Skip::NameTaken => "name taken; retried when it changes",
        Skip::NotEmpty => "folder not empty (may hold excluded items)",
        Skip::ContentMismatch => "content didn't match; retrying",
        Skip::Refused => "special item or invalid name; retried when it changes",
        Skip::CloudFull => "OneDrive is full; sent once there's room",
        Skip::Database => "a database not ready to sync; retrying",
    }
}

/// How an upload done as a server-side copy is described.
const COPIED: &str = "OneDrive already had this content, so it copied it there: nothing was sent";

/// Prints each operation with its outcome, and returns the activity-log
/// entries for them. `planned` is the approved list in order, described
/// before it ran (afterwards a recycled item has no path). The executor
/// puts each operation in exactly one of the report's lists, in that order
/// whatever order they ran in, so one merge pass pairs them up.
fn print_round(n: usize, planned: &[(Op, Line)], r: &Report) -> Vec<ActivityEntry> {
    let mut log = Vec::new();
    if r.done.is_empty() && r.skipped.is_empty() && r.failed.is_empty() {
        return log;
    }
    let at_ms = now_ms();
    let mut entry = |line: &Line, outcome, why: Option<String>| {
        let detail = match why {
            Some(w) if line.detail().is_empty() => w,
            Some(w) => format!("{}; {w}", line.detail()),
            None => line.detail(),
        };
        log.push(ActivityEntry { seq: 0, at_ms, outcome, action: line.action.into(), path: line.path.clone(), detail });
    };
    println!("Round {n}:");
    let found = crate::databases::Found::of(r);
    let mut done = r.done.iter().peekable();
    let mut skipped = r.skipped.iter().peekable();
    let mut failed = r.failed.iter().peekable();
    // Bookkeeping isn't news, and first-sync pairing can be millions of lines.
    let quiet = |op: &Op| matches!(op, Op::Record(_) | Op::Forget(_) | Op::Pair { .. } | Op::Hash(_));
    for (op, line) in planned {
        if done.next_if(|d| *d == op).is_some() {
            if r.kept_instead.contains(op) {
                println!(
                    "  ok    {line}\n        (its old copy was kept in .oneup\\versions after all: \
                     it differed from the version OneDrive has)"
                );
                entry(line, ActivityOutcome::Done, Some(KEPT_INSTEAD.into()));
            } else if r.copied.contains(op) {
                println!("  ok    {line}  ({COPIED})");
                entry(line, ActivityOutcome::Done, Some(COPIED.into()));
            } else if let Some((_, copy)) = r.kept_theirs.iter().find(|(o, _)| o == op) {
                let why = format!("{KEPT_THEIRS}; that version is kept as {copy}");
                println!("  ok    {line}  ({why})");
                entry(line, ActivityOutcome::Done, Some(why));
            } else if let Some((_, why)) = r.theirs_not_kept.iter().find(|(o, _)| o == op) {
                // Done, and a failure of its own: the edit it went over
                // wasn't kept next to it.
                println!("  ok    {line}");
                println!("  FAIL  {line}  ({why})");
                entry(line, ActivityOutcome::Done, None);
                entry(line, ActivityOutcome::Failed, Some(why.clone()));
            } else if !matches!(op, Op::Record(_) | Op::Forget(_)) {
                println!("  ok    {line}");
                if !quiet(op) {
                    entry(line, ActivityOutcome::Done, None);
                }
            }
        } else if let Some((_, s)) = skipped.next_if(|(o, _)| o == op) {
            match crate::databases::skip_detail(&found, op).filter(|_| *s == Skip::Database) {
                // Waiting, not failing: it goes once the database is ready.
                Some(why) => {
                    println!("  wait  {line}  ({why})");
                    entry(line, ActivityOutcome::Waiting, Some(why));
                }
                None => {
                    println!("  skip  {line}  ({})", skip_label(*s));
                    entry(line, ActivityOutcome::Skipped, Some(skip_label(*s).into()));
                }
            }
        } else if let Some((_, m)) = failed.next_if(|(o, _)| o == op) {
            println!("  FAIL  {line}  ({m})");
            entry(line, ActivityOutcome::Failed, Some(m.clone()));
        } else {
            println!("  --    {line}  (not run: the round stopped)");
        }
    }
    log
}

/// An upload went over an edit made in OneDrive meanwhile, which was kept
/// next to it: a conflict (`crate::events`).
pub const KEPT_THEIRS: &str = "it changed in OneDrive during the upload";

const KEPT_INSTEAD: &str =
    "its old copy was kept in .oneup\\versions after all: it differed from the version OneDrive has";

pub fn now_ms() -> i64 {
    std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).map(|d| d.as_millis() as i64).unwrap_or(0)
}

/// Splits `items` into those that go now and those `until` holds back,
/// keeping their order, with the earliest time a held one may go.
fn hold_back<T>(
    items: Vec<T>,
    mut until: impl FnMut(&T) -> Option<std::time::Instant>,
) -> (Vec<T>, Vec<T>, Option<std::time::Instant>) {
    let (mut now, mut held, mut first) = (Vec::new(), Vec::new(), None::<std::time::Instant>);
    for item in items {
        match until(&item) {
            Some(u) => {
                first = Some(first.map_or(u, |f| f.min(u)));
                held.push(item);
            }
            None => now.push(item),
        }
    }
    (now, held, first)
}

/// The earlier of two times, if either.
fn earliest(a: Option<std::time::Instant>, b: Option<std::time::Instant>) -> Option<std::time::Instant> {
    match (a, b) {
        (Some(a), Some(b)) => Some(a.min(b)),
        (a, b) => a.or(b),
    }
}

/// `ms` milliseconds after `now` (`now` itself if not after).
fn after_ms(now: std::time::Instant, ms: i64) -> std::time::Instant {
    now + std::time::Duration::from_millis(u64::try_from(ms).unwrap_or(0))
}

/// The local file `op` reads to send it: an upload, or the hashing that
/// comes before one. What waits while the file keeps changing.
fn sent_file(op: &Op) -> Option<u128> {
    match op {
        Op::Upload { local, .. } | Op::Hash(local) => Some(local.id.0),
        _ => None,
    }
}

/// Compacts the database at the agent's start (`[agent] compact_database`)
/// if free pages are most of it (`Space::worth_compacting`) and the disk
/// has room for the copy, saying so in the activity log. Before the agent's
/// other connections open. Best effort: a failure leaves the database as
/// it was, and the next start tries again.
pub fn compact(store: &mut Store, data_dir: &Path) {
    let space = match store.space() {
        Ok(s) if s.worth_compacting() => s,
        Ok(_) => return,
        Err(e) => return tracing::warn!(error = %e, "couldn't read the database's size"),
    };
    // SQLite documents that VACUUM may need up to twice the database's size
    // (a temporary copy, then the rewrite in the WAL).
    let room = oneup_sys::disk::free_bytes(data_dir).unwrap_or(0);
    let need = store.files_size().max(space.total).saturating_mul(2);
    if room < need {
        return tracing::info!(room, need, "not enough free disk to compact the database");
    }
    let size = crate::versions::size;
    let detail = match store.compact() {
        Ok(c) if c.after < c.before => format!("database compacted from {} to {}", size(c.before), size(c.after)),
        Ok(c) if c.pending => {
            format!("database compacted; its file ({}) shrinks once another program stops reading it", size(c.before))
        }
        Ok(c) => format!(
            "database compacted, but its file stayed at {} (another program has it open); the space is reused",
            size(c.before)
        ),
        Err(e) => return tracing::warn!(error = %e, "couldn't compact the database; tried again at the next start"),
    };
    println!("oneupd: {detail}");
    if let Err(e) = log_activity(store, vec![notice(ActivityOutcome::Note, "compacted", detail)]) {
        tracing::warn!(error = %e, "the compaction wasn't logged");
    }
}

/// An activity-log entry about sync itself (no item).
fn notice(outcome: ActivityOutcome, action: &str, detail: String) -> ActivityEntry {
    ActivityEntry { seq: 0, at_ms: now_ms(), outcome, action: action.into(), path: String::new(), detail }
}

/// Adds entries to the activity log. A skip or failure that repeats what
/// the item's last entry says (a file open for an hour, retried every few
/// minutes) isn't added again.
/// Notes something in the activity log.
pub fn note(store: &mut Store, action: &str, detail: String) -> Result<()> {
    log_activity(store, vec![notice(ActivityOutcome::Note, action, detail)])
}

fn log_activity(store: &mut Store, entries: Vec<ActivityEntry>) -> Result<()> {
    if entries.is_empty() {
        return Ok(());
    }
    store.write(|o| {
        let mut keep = Vec::with_capacity(entries.len());
        for e in entries {
            if matches!(e.outcome, ActivityOutcome::Skipped | ActivityOutcome::Failed | ActivityOutcome::Waiting)
                && !e.path.is_empty()
            {
                let last = o.activity_recent(Some(&e.path), 1)?;
                if last.first().is_some_and(|l| l.path == e.path && l.action == e.action && l.detail == e.detail) {
                    continue;
                }
            }
            keep.push(e);
        }
        o.activity_add(&keep)
    })?;
    Ok(())
}

fn print_problems(problems: &[(String, Why)]) {
    for (path, why) in problems {
        let why = match why {
            Why::InvalidName => "OneDrive doesn't allow this name; rename it",
            Why::TooLarge => "over OneDrive's 250 GB limit",
        };
        println!("  not synced  {path}  ({why})");
    }
}

/// What a tripped breaker is about, in a few words.
pub fn breaker_what(b: &Breaker) -> String {
    let side = match b.side {
        Side::Local => "on this PC",
        Side::Remote => "in OneDrive",
    };
    match b.kind {
        BreakerKind::Mass => format!(
            "{}{} items {side} would be deleted, overwritten or moved",
            if b.at_least { "at least " } else { "" },
            b.count
        ),
        BreakerKind::Removal => format!("removing the duplicates you chose: {} changes", b.count),
        BreakerKind::Suspicious => format!(
            "{} files would replace their OneDrive copy with content that doesn't look like their type \
             (as when files are encrypted by ransomware)",
            b.count
        ),
    }
}

/// Tells the user about tripped safety stops and how to approve them.
pub fn print_breakers(breakers: &[Breaker], asker: Asker) {
    for b in breakers {
        // The agent's window: say why a small batch tripped.
        let earlier = match b.recent {
            0 => String::new(),
            n => format!(", on top of {n} done in the last few minutes"),
        };
        let code = format!("{:016x}", b.fingerprint.0);
        if b.kind == BreakerKind::Removal {
            // Sync goes on meanwhile: only the removal waits.
            let how = match asker {
                Asker::Sync => format!("run `oneup sync` again with --approve {code}"),
                Asker::Agent => format!("run `oneup approve {code}`"),
            };
            println!(
                "  Safety stop: {}.\n  Nothing was removed yet. If the list is what you chose, {how}",
                breaker_what(b)
            );
            continue;
        }
        let how = match asker {
            Asker::Sync => format!(
                "See the list with `oneup sync --dry-run`. If it's what you meant, run `oneup sync` again with \
                 --approve {code}"
            ),
            Asker::Agent => format!(
                "oneupd waits and changes nothing until you answer. If it's what you meant, run \
                 `oneup approve {code}`"
            ),
        };
        println!(
            "  Safety stop: {}{earlier} ({} items are synced).\n  Nothing was changed. {how}",
            breaker_what(b),
            b.synced_items
        );
    }
}

/// Tells the user about a version-store overflow batch (see `oneup_gate`):
/// what it is, and for a held one, the two answers.
pub fn print_overflow(o: &Overflow, store: &Store, asker: Asker) -> Result<()> {
    use crate::versions::size;
    let days = crate::versions::trim_settings(store)?.min_age_days;
    let (n, bytes, room) = (o.count, size(o.bytes), size(o.room));
    let code = format!("{:016x}", o.fingerprint.0);
    let (how, keep, discard) = match asker {
        Asker::Sync => ("running `oneup sync` again with", format!("--keep {code}   "), format!("--discard {code}")),
        Asker::Agent => ("running", format!("oneup keep {code}   "), format!("oneup discard {code}")),
    };
    match o.outcome {
        OverflowOutcome::Held => println!(
            "  Version store full: {n} files ({bytes}) deleted or replaced in OneDrive would go into the version \
             store, which has room for {room}.\n  \
             They are still on this PC, untouched; everything else keeps syncing. Decide once for all of them, \
             by {how}:\n    \
             {keep}  keep them anyway; the store stays over its limit until they are {days} days old\n    \
             {discard}  remove them from this PC without a local copy. OneDrive's recycle bin usually \
             still has them for up to 30 days, but empties sooner when it is full. (A file that differs from \
             the version OneDrive has is kept anyway.)\n  \
             To decide this automatically from now on: `oneup versions --set-overflow keep`, or \
             `--set-rotate on` to make room by removing the oldest kept items."
        ),
        OverflowOutcome::KeptByPolicy => println!(
            "  Version store over its limit: {n} files ({bytes}) deleted or replaced in OneDrive were kept \
             although it had room for {room} (your setting: keep). It shrinks once they are {days} days old; \
             see `oneup versions` to free space sooner."
        ),
        OverflowOutcome::Decided(Choice::Keep) => println!(
            "  Kept as you chose: {n} files ({bytes}) in the version store, which had room for {room}. \
             It stays over its limit until they are {days} days old."
        ),
        OverflowOutcome::Decided(Choice::Discard) => println!(
            "  As you chose, {n} files ({bytes}) that didn't fit in the version store were removed from this PC \
             without a local copy (any that differed from the version OneDrive has were kept)."
        ),
    }
    Ok(())
}

#[derive(Default)]
pub struct Totals {
    done: usize,
    skipped: usize,
    failed: usize,
    held: usize,
    problems: Vec<(String, Why)>,
}

impl Totals {
    fn add(&mut self, r: &Report) {
        self.done += r.done.iter().filter(|op| !matches!(op, Op::Record(_) | Op::Forget(_))).count();
        self.skipped += r.skipped.len();
        self.failed += r.failed.len();
    }

    pub fn done(&self) -> usize {
        self.done
    }

    pub fn skipped(&self) -> usize {
        self.skipped
    }

    pub fn failed(&self) -> usize {
        self.failed
    }

    pub fn held(&self) -> usize {
        self.held
    }

    /// Whether nothing at all happened (the agent then prints nothing).
    pub fn is_quiet(&self) -> bool {
        self.done + self.skipped + self.failed == 0
    }

    pub fn print(&self, rounds: usize) {
        println!(
            "Sync finished after {rounds} round(s): {} operations done, {} skipped, {} failed, {} held.",
            self.done, self.skipped, self.failed, self.held
        );
        print_problems(&self.problems);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The problems list reads conflict copies' names back from the log.
    #[test]
    fn a_conflict_copys_name_is_read_back_from_its_log_entry() {
        let name = "a (conflict; this PC).txt";
        let line = Line { to: Some(name.into()), ..Line::new(CONFLICT_ACTION, "/docs/a.txt".into(), CONFLICT_NOTE) };
        assert_eq!(conflict_copy_name(&line.detail()), Some(name));
        assert_eq!(conflict_copy_name("-> b.txt; renamed"), None);
    }

    /// After the journal lost track (simulated: the saved position moved
    /// before its oldest record), the same folder is looked through again
    /// and the rescan is logged. Temp folder only.
    #[cfg(windows)]
    #[tokio::test]
    async fn a_wrapped_journal_rescans_the_folder() {
        #![allow(clippy::unwrap_used)]
        let dir = std::env::temp_dir().join(format!("oneup-rescan-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        std::fs::write(dir.join("a.txt"), b"a").unwrap();
        let rules = ExcludeSet::none();
        let mut scan =
            oneup_local::Scan::open(Store::open_in_memory().unwrap(), &dir, oneup_local::Config::new(rules.clone()))
                .unwrap();
        assert!(scan.run().unwrap().finished);
        let mut store = scan.into_store();
        store.write(|o| o.local_set_journal_cursor(0)).unwrap();
        std::fs::write(dir.join("b.txt"), b"made while nothing watched").unwrap();

        let store = observe(store, &rules).await.unwrap();
        assert_eq!(store.read(|o| o.local_stats()).unwrap().files, 2);
        let log = store.read(|o| o.activity_recent(None, 5)).unwrap();
        assert!(log.iter().any(|e| e.action == "rescan"), "{log:?}");
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// The agent's journal watcher read a synced file's delete record, then
    /// the journal lost track before the observer read it: after the
    /// rescan the file is deleted, with evidence, instead of held. The
    /// volume's real records, read the way the watcher reads them. Temp
    /// folder only.
    #[cfg(windows)]
    #[tokio::test]
    async fn deletions_read_before_a_journal_loss_are_still_evidence() {
        #![allow(clippy::unwrap_used)]
        use oneup_local::observe::{SeenDeletes, remember_deletes};
        use oneup_sync::{Base, LocalId, NodeKind, ROOT, RemoteId, Seen, Trees};
        let dir = std::env::temp_dir().join(format!("oneup-seen-deletes-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        std::fs::write(dir.join("a.txt"), b"a").unwrap();
        std::fs::write(dir.join("b.txt"), b"b").unwrap();
        let rules = ExcludeSet::none();
        let mut scan =
            oneup_local::Scan::open(Store::open_in_memory().unwrap(), &dir, oneup_local::Config::new(rules.clone()))
                .unwrap();
        assert!(scan.run().unwrap().finished);
        let mut store = scan.into_store();
        let top = store.read(|o| o.local_root()).unwrap().unwrap().root_file_id;
        let id_of = |s: &Store, name: &str| s.read(|o| o.local_named(top, name)).unwrap()[0].file_id;
        let (a, b) = (id_of(&store, "a.txt"), id_of(&store, "b.txt"));
        store
            .write(|o| {
                let base = |sid, name: &str, local, kind| Base {
                    sid,
                    parent: (sid != ROOT).then_some(ROOT),
                    name: name.into(),
                    kind,
                    size: 1,
                    hash: None,
                    local: Some(LocalId(local)),
                    local_mtime: 0,
                    remote: Some(RemoteId(format!("r{sid}"))),
                    remote_etag: String::new(),
                };
                o.synced_put(&base(ROOT, "", top, NodeKind::Folder))?;
                o.synced_put(&base(1, "a.txt", a, NodeKind::File))?;
                o.synced_put(&base(2, "b.txt", b, NodeKind::File))
            })
            .unwrap();

        let volume = oneup_ntfs::win::Volume::open_containing(&dir).unwrap();
        let j = volume.query_journal().unwrap();
        std::fs::remove_file(dir.join("b.txt")).unwrap();
        let seen: &'static SeenDeletes = Box::leak(Box::new(SeenDeletes::new()));
        let (mut cursor, end) = (j.next_usn, volume.query_journal().unwrap().next_usn);
        let mut buf = vec![0u8; 64 * 1024];
        while cursor < end {
            let (next, records) = volume.read_journal(j.journal_id, cursor, false, &mut buf).unwrap();
            let records: Vec<_> = records.flatten().collect();
            remember_deletes(&store, seen, j.journal_id, &records).unwrap();
            if next <= cursor {
                break;
            }
            cursor = next;
        }
        assert_eq!(seen.ids(j.journal_id), [b], "only the synced item's delete record is kept");
        store.write(|o| o.local_set_journal_cursor(0)).unwrap();

        let store = observe_with(store, &rules, seen).await.unwrap();
        store
            .read(|o| {
                assert_eq!(o.local(LocalId(b))?, Seen::Deleted);
                assert!(o.local(LocalId(a))?.present().is_some());
                assert!(o.check_invariants()?.is_empty());
                Ok(())
            })
            .unwrap();
        let log = store.read(|o| o.activity_recent(None, 5)).unwrap();
        assert!(
            log.iter().any(|e| e.action == "rescan" && e.detail.contains("deletions found in the journal: 1")),
            "{log:?}"
        );
        assert!(seen.is_empty(), "forgotten once the observer read past it");
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// A stop's list folders are marked once while it stands; approving its
    /// code approves its items, for the rounds until they settle.
    #[test]
    fn an_approved_stop_approves_its_list_and_its_folders_are_marked_once() {
        let ops: Vec<Op> = (0..30u64)
            .map(|i| Op::RecycleLocal {
                sid: i,
                local: oneup_sync::LocalPre { id: LocalId(u128::from(i)), stamp: Some((1, 1)) },
            })
            .collect();
        let breakers = oneup_gate::review(&oneup_gate::Policy::default(), 100, &[], ops).breakers;
        let code = breakers[0].fingerprint.0;
        let mut s = Stops::default();
        assert_eq!(s.raise(&breakers), (0..30).collect::<Vec<_>>());
        assert!(s.raise(&breakers).is_empty(), "marked once while it stands");
        s.answer(&[code ^ 1]);
        assert!(s.approved.is_empty(), "another code approves nothing");
        s.answer(&[code]);
        assert_eq!(s.approved, breakers[0].items);
        assert!(s.raised.is_empty());
        // At most a few stops are remembered for an answer.
        for i in 0..20u64 {
            let mut b = breakers[0].clone();
            b.fingerprint = oneup_gate::Fingerprint(i);
            s.raise(&[b]);
        }
        assert_eq!(s.raised.len(), STOPS_KEPT);
    }

    /// An approval code is used once: after its list ran and the rounds
    /// settled, the same items coming back (a file restored, then
    /// rewritten suspiciously again: an upload's key has no content stamp)
    /// trip the stop again instead of riding on the old answer.
    #[test]
    fn an_approval_code_is_used_once() {
        let uploads: Vec<Op> = (1..=25u64)
            .map(|i| Op::Upload {
                local: oneup_sync::LocalPre { id: LocalId(u128::from(i)), stamp: Some((1, 1)) },
                to: RemoteId("root".into()),
                parent: 0,
                name: format!("f{i}.docx"),
                replace: Some(oneup_sync::RemotePre { id: RemoteId(format!("r{i}")), etag: "e".into() }),
                link: oneup_sync::Link::Existing(i),
            })
            .collect();
        let bad: Vec<LocalId> = (1..=25).map(LocalId).collect();
        let p = oneup_gate::Policy::default();
        let review = |stops: &Stops, given: &[u64]| {
            let approvals: Vec<Approval> = stops
                .live(given)
                .into_iter()
                .flat_map(|f| [Side::Local, Side::Remote].map(|side| Approval { side, fingerprint: Fingerprint(f) }))
                .collect();
            let cx = GateContext {
                synced_items: 100_000,
                suspicious: &bad,
                approvals: &approvals,
                approved: &stops.approved,
                ..GateContext::default()
            };
            review_in(&p, &cx, uploads.clone())
        };
        let mut s = Stops::default();
        let r = review(&s, &[]);
        let code = r.breakers[0].fingerprint.0;
        s.raise(&r.breakers);
        // The user approves; the agent's answers keep the code meanwhile.
        let given = [code];
        s.answer(&given);
        assert!(review(&s, &given).breakers.is_empty(), "approved: they run");
        assert_eq!(std::mem::take(&mut s.to_drop), [code], "the caller drops the code");
        s.settled();
        assert!(s.approved.is_empty() && s.raised.is_empty());
        let r = review(&s, &given);
        assert_eq!(r.breakers.len(), 1, "back again: a new stop");
        assert!(r.answered.is_empty());
        // That new raising takes a new approval (the caller dropped the code
        // after the call; the user gives it again).
        s.begin();
        assert_eq!(review(&s, &[]).breakers.len(), 1);
        s.raise(&r.breakers);
        s.answer(&given);
        assert!(review(&s, &[]).breakers.is_empty(), "approved again");

        // The same when the gate matched the code (`oneup sync --approve`).
        let mut s = Stops::default();
        let r = review(&s, &given);
        assert_eq!(r.answered.len(), 1);
        s.approve(&r.answered[0]);
        s.settled();
        assert_eq!(review(&s, &given).breakers.len(), 1);
    }

    /// An approved change that waits (a file that keeps changing, a pause)
    /// stays approved when the rounds settle, until it runs; then it's done
    /// with. One that's no longer planned goes when they settle.
    #[test]
    fn an_approved_change_that_waits_stays_approved_until_it_runs() {
        let uploads: Vec<Op> = (1..=25u64)
            .map(|i| Op::Upload {
                local: oneup_sync::LocalPre { id: LocalId(u128::from(i)), stamp: Some((1, 1)) },
                to: RemoteId("root".into()),
                parent: 0,
                name: format!("f{i}.docx"),
                replace: Some(oneup_sync::RemotePre { id: RemoteId(format!("r{i}")), etag: "e".into() }),
                link: oneup_sync::Link::Existing(i),
            })
            .collect();
        let bad: Vec<LocalId> = (1..=25).map(LocalId).collect();
        for paused in [false, true] {
            let p = oneup_gate::Policy { pause_remote: paused, ..oneup_gate::Policy::default() };
            let review = |stops: &Stops| {
                let cx = GateContext {
                    synced_items: 100_000,
                    suspicious: &bad,
                    approved: &stops.approved,
                    ..GateContext::default()
                };
                review_in(&p, &cx, uploads.clone())
            };
            let mut s = Stops::default();
            let r = review(&s);
            s.raise(&r.breakers);
            s.answer(&[r.breakers[0].fingerprint.0]);
            // Approved; held back (busy: after the gate; paused: by it):
            // nothing ran, and the rounds settle.
            let r = review(&s);
            assert!(r.breakers.is_empty(), "paused {paused}");
            assert_eq!(r.listed.len(), 25);
            s.round_ended(r.listed, &[]);
            s.settled();
            // A later call: still approved, though no code is given.
            s.begin();
            let r = review(&s);
            assert!(r.breakers.is_empty(), "paused {paused}: still approved");
            // It runs (the pause over); then it's done with.
            s.round_ended(r.listed, &uploads);
            s.settled();
            assert!(s.approved.is_empty());
            assert_eq!(review(&s).breakers.len(), 1, "back again: a new stop");
        }
        // Approved, then no longer planned (the file restored): gone at settling.
        let p = oneup_gate::Policy::default();
        let cx = GateContext { synced_items: 100_000, suspicious: &bad, ..GateContext::default() };
        let r = review_in(&p, &cx, uploads.clone());
        let mut s = Stops::default();
        s.raise(&r.breakers);
        s.answer(&[r.breakers[0].fingerprint.0]);
        s.round_ended(Vec::new(), &[]);
        s.settled();
        assert!(s.approved.is_empty());
    }

    /// With a mass stop and a content-guard stop together, and parts of each
    /// approved earlier, the list saved for each stop is exactly what its
    /// code approves.
    #[test]
    fn each_saved_stop_list_is_what_its_code_approves() {
        let mut ops: Vec<Op> = (1..=25u64)
            .map(|i| Op::Upload {
                local: oneup_sync::LocalPre { id: LocalId(u128::from(i)), stamp: Some((1, 1)) },
                to: RemoteId("root".into()),
                parent: 0,
                name: format!("f{i}.docx"),
                replace: Some(oneup_sync::RemotePre { id: RemoteId(format!("r{i}")), etag: "e".into() }),
                link: oneup_sync::Link::Existing(i),
            })
            .collect();
        ops.extend((100..130u64).map(|i| Op::DeleteRemote {
            sid: i,
            remote: oneup_sync::RemotePre { id: RemoteId(format!("r{i}")), etag: "e".into() },
        }));
        let bad: Vec<LocalId> = (1..=25).map(LocalId).collect();
        let mut approved: Vec<ItemKey> =
            ops[..12].iter().map(|o| oneup_gate::item_key(BreakerKind::Suspicious, o, "")).collect();
        approved.extend(ops[12..25].iter().map(|o| oneup_gate::item_key(BreakerKind::Mass, o, "")));
        approved.sort_unstable();
        let cx = GateContext { synced_items: 300, suspicious: &bad, approved: &approved, ..GateContext::default() };
        let r = review_in(&oneup_gate::Policy::default(), &cx, ops);
        assert_eq!(r.breakers.len(), 2);
        for (i, b) in r.breakers.iter().enumerate() {
            let mut shown: Vec<ItemKey> =
                stop_ops(&r, i).into_iter().map(|op| oneup_gate::item_key(b.kind, op, "")).collect();
            shown.sort_unstable();
            assert_eq!(shown, b.items, "{:?}", b.kind);
        }
    }

    /// A pruned folder counts as everything in it wherever the safety
    /// window and the stops count: it makes the round read the paths its
    /// list shows, its approval runs it, and the window records all of it.
    #[test]
    fn a_pruned_folder_counts_as_everything_in_it() {
        let p = oneup_gate::Policy::default();
        let prune = Op::PruneRemote {
            remote: oneup_sync::RemotePre { id: RemoteId("big".into()), etag: "e".into() },
            forget: vec![7],
            items: 1000,
            exact: true,
            keep: Vec::new(),
        };
        assert!(oneup_gate::may_stop(&p, std::slice::from_ref(&prune), &[], Recent::default(), false, false));
        let shown = vec!["/node_modules/".to_string()];
        let cx = GateContext { synced_items: 100_000, paths: &shown, ..GateContext::default() };
        let b = review_in(&p, &cx, vec![prune.clone()]).breakers.remove(0);
        assert_eq!(b.count, 1000);
        let cx = GateContext { approved: &b.items, ..cx };
        let r = review_in(&p, &cx, vec![prune.clone()]);
        assert!(r.breakers.is_empty(), "approved where it was shown");
        let reserve = reserved(Recent::default(), &r.approved, &[]);
        assert_eq!(reserve.remote, 1000);
        let mut store = Store::open_in_memory().unwrap();
        crate::safetywindow::SafetyWindow::default().save_with(&mut store, reserve, 0).unwrap();
        let (mut w, _) = crate::safetywindow::SafetyWindow::load(&store, 1).unwrap();
        assert_eq!(w.recent(1).remote, 1000);
        let mut done = Recent::default();
        done.add_op(&prune, &[]);
        assert_eq!(done.remote, 1000);
    }

    /// A removal of unknown size (a folder never listed in full, of which
    /// 10 items are known) goes through a round as the engine plans it:
    /// it raises a stop whose list is keyed with the path it shows, so
    /// approving it works the first time (from the agent's list, or by its
    /// code as `oneup sync --approve` does), and a move of a folder above it
    /// between `status` and `approve` makes it a stop of its own again.
    /// A round whose only change is removing one chosen duplicate, far
    /// below every limit, still stops for it, keyed with the path its list
    /// shows (the full OneDrive path), and the preview's code is the stop's.
    /// Moved meanwhile, the same copy is another list.
    #[test]
    fn a_chosen_removal_is_keyed_where_it_is_shown_in_a_round() {
        use oneup_store::RemoteRow;
        let row = |id: &str, parent: &str, name: &str, kind: Kind| RemoteRow {
            id: id.into(),
            parent_id: Some(parent.into()),
            name: name.into(),
            kind,
            size: if kind == Kind::File { 2 << 20 } else { 0 },
            etag: Some(format!("e-{id}")),
            qxh: (kind == Kind::File).then_some([7; 20]),
            mtime_ms: None,
        };
        let mut store = Store::open_in_memory().unwrap();
        store
            .write(|o| {
                o.upsert_item(&RemoteRow { parent_id: None, ..row("root", "", "", Kind::Root) }, false)?;
                o.upsert_item(&row("top", "root", "Sync", Kind::Folder), false)?;
                o.upsert_item(&row("old", "root", "Old", Kind::Folder), false)?;
                o.upsert_item(&row("keep", "top", "a.jpg", Kind::File), false)?;
                o.upsert_item(&row("dup", "old", "a.jpg", Kind::File), false)?;
                oneup_exec::link_roots(o, 1, "top").map(|_| ())
            })
            .unwrap();
        let chosen = oneup_exec::dupes::Chosen {
            copy: oneup_exec::dupes::CopyId::Cloud { id: RemoteId("dup".into()), etag: "e-dup".into() },
            size: 2 << 20,
            hash: [7; 20],
        };
        let plan = store.read(|o| oneup_exec::dupes::plan(o, std::slice::from_ref(&chosen), &[])).unwrap();
        assert_eq!(plan.ops.len(), 1);
        let rules = RuleSet::default();
        let round = |store: &Store, removal: Vec<Op>| {
            let modes = Modes { removal, ..Modes::default() };
            plan_round(store, None, Scope::All, &Controls::default(), Recent::default(), &rules, modes, &[])
                .unwrap()
                .review
        };
        let r = round(&store, plan.ops.clone());
        assert_eq!(r.breakers.len(), 1);
        let b = &r.breakers[0];
        assert_eq!(b.kind, BreakerKind::Removal);
        let at = |path: &str| oneup_gate::item_key(BreakerKind::Removal, &plan.ops[0], path);
        assert_eq!(b.items, [at("/Old/a.jpg")], "keyed with the path the list shows");
        let preview = store.read(|o| crate::duplicates::code(o, &plan)).unwrap();
        assert_eq!(format!("{:016x}", b.fingerprint.0), preview);
        // `Old` renamed: the copy is shown elsewhere, another list.
        store
            .write(|o| {
                o.upsert_item(&RemoteRow { name: "Older".into(), ..row("old", "root", "", Kind::Folder) }, false)
                    .map(|_| ())
            })
            .unwrap();
        // Each round works the choice out again: the path is the one the
        // operation carries, the one the executor checks it's still at.
        let again = store.read(|o| oneup_exec::dupes::plan(o, std::slice::from_ref(&chosen), &[])).unwrap();
        let r = round(&store, again.ops.clone());
        assert_eq!(r.breakers[0].items, [oneup_gate::item_key(BreakerKind::Removal, &again.ops[0], "/Older/a.jpg")]);
        assert_ne!(r.breakers[0].fingerprint, b.fingerprint);
    }

    #[test]
    fn a_removal_of_unknown_size_is_approved_where_it_was_shown() {
        use oneup_store::RemoteRow;
        let row = |id: &str, parent: &str, name: &str, kind: Kind| RemoteRow {
            id: id.into(),
            parent_id: Some(parent.into()),
            name: name.into(),
            kind,
            size: 1,
            etag: Some("e".into()),
            qxh: None,
            mtime_ms: None,
        };
        let mut store = Store::open_in_memory().unwrap();
        store
            .write(|o| {
                o.upsert_item(
                    &RemoteRow { parent_id: None, kind: Kind::Root, ..row("root", "", "", Kind::Root) },
                    false,
                )?;
                o.upsert_item(&row("top", "root", "Sync", Kind::Folder), false)?;
                o.upsert_item(&row("other", "top", "Other", Kind::Folder), false)?;
                o.upsert_item(&row("app", "top", "App", Kind::Folder), false)?;
                // Skipped by name when listing: nothing inside was ever seen.
                o.upsert_item(&row("nm", "app", "node_modules", Kind::Folder), true)?;
                oneup_exec::link_roots(o, 1, "top").map(|_| ())
            })
            .unwrap();
        // As found and counted: 10 direct children, all OneDrive tells.
        let prune = Op::PruneRemote {
            remote: oneup_sync::RemotePre { id: RemoteId("nm".into()), etag: "e".into() },
            forget: Vec::new(),
            items: 10,
            exact: false,
            keep: Vec::new(),
        };
        let rules = RuleSet::new(&["dev"], &[] as &[&str]).unwrap();
        let round = |store: &Store, approved: &[ItemKey], codes: Vec<u64>| {
            let controls =
                Controls { answers: Answers { approve: codes, ..Answers::default() }, ..Controls::default() };
            let modes = Modes { pull: true, extra: vec![prune.clone()], ..Modes::default() };
            plan_round(store, None, Scope::All, &controls, Recent::default(), &rules, modes, approved).unwrap().review
        };

        let r = round(&store, &[], Vec::new());
        assert_eq!(r.breakers.len(), 1, "10 known items, but it may hold any number");
        let b = r.breakers[0].clone();
        assert!(b.at_least);
        let shown = store.read(|o| Ok(stop_ops(&r, 0).into_iter().map(|op| describe(o, op).path).collect::<Vec<_>>()));
        assert_eq!(shown.unwrap(), ["/App/node_modules"]);
        let at = |path: &str| oneup_gate::item_key(BreakerKind::Mass, &prune, path);
        assert_eq!(b.items, [at("/App/node_modules")], "keyed with the path the list shows");

        // Approved the first time, from the agent's list or by the code.
        let r = round(&store, &b.items, Vec::new());
        assert!(r.breakers.is_empty() && r.approved.len() == 1, "{:?}", r.breakers);
        let r = round(&store, &[], vec![b.fingerprint.0]);
        assert!(r.breakers.is_empty() && r.approved.len() == 1);
        assert_eq!(r.answered.len(), 1);

        // `App` moved into `Other` between `status` and `approve`.
        store.write(|o| o.upsert_item(&row("app", "other", "App", Kind::Folder), false).map(|_| ())).unwrap();
        let r = round(&store, &b.items, Vec::new());
        assert_eq!(r.breakers.len(), 1, "the same folder elsewhere isn't what was approved");
        assert!(r.approved.is_empty());
        assert_eq!(r.breakers[0].items, [at("/Other/App/node_modules")]);
        let r = round(&store, &[], vec![b.fingerprint.0]);
        assert_eq!(r.breakers.len(), 1, "nor does the old code approve it");
    }

    /// What a round may carry out is counted for the safety window before
    /// it runs, and recorded (`Keep`).
    #[test]
    fn a_round_reserves_what_it_may_carry_out() {
        let ops: Vec<Op> = (0..30u64)
            .map(|i| Op::RecycleLocal {
                sid: i,
                local: oneup_sync::LocalPre { id: LocalId(u128::from(i)), stamp: Some((1, 1)) },
            })
            .collect();
        let approved = oneup_gate::review(&oneup_gate::Policy::default(), 1_000_000, &[], ops).approved;
        let before = Recent { local: 5, remote: 1, suspicious: 0 };
        assert_eq!(reserved(before, &approved, &[]), Recent { local: 35, remote: 1, suspicious: 0 });
    }

    #[test]
    fn remote_paths_are_normalized_and_the_drive_top_follows_the_release() {
        assert_eq!(normalize_remote("Sync//Test/").unwrap(), "/Sync/Test");
        assert_eq!(normalize_remote(r"\a\b").unwrap(), "/a/b");
        assert_eq!(normalize_remote("/").ok().as_deref(), WHOLE_DRIVE_ALLOWED.then_some("/"));
        assert_eq!(normalize_remote(r"\").ok().as_deref(), WHOLE_DRIVE_ALLOWED.then_some("/"));
        assert!(normalize_remote("/a/../b").is_err());
    }

    #[test]
    fn joined_paths_have_single_slashes() {
        assert_eq!(join("/", "x"), "/x");
        assert_eq!(join("/a", "x"), "/a/x");
    }

    #[test]
    fn lines_print_as_before_and_log_their_details() {
        let create = Line { folder: true, ..Line::new("create", "/x".into(), "in OneDrive") };
        assert_eq!(create.to_string(), "create      /x/  (in OneDrive)");
        let mv = Line { to: Some("/b".into()), ..Line::new("move", "/a".into(), "on this PC") };
        assert_eq!(mv.to_string(), "move        /a -> /b  (on this PC)");
        assert_eq!(mv.detail(), "-> /b; on this PC");
        assert_eq!(Line::new("in sync", "/f".into(), "").to_string(), "in sync     /f");
    }

    #[test]
    fn the_agent_plans_every_folder_at_start_only_until_this_version_has() {
        let mut s = Store::open_in_memory().unwrap();
        assert_eq!(start_scope(&s).unwrap(), Scope::All, "a new or pre-marks database");
        s.write(|o| o.meta_set(KEY_FULL_PASS, "0.0.1")).unwrap();
        assert_eq!(start_scope(&s).unwrap(), Scope::All, "another version may plan differently");
        s.write(|o| o.meta_set(KEY_FULL_PASS, env!("CARGO_PKG_VERSION"))).unwrap();
        assert_eq!(start_scope(&s).unwrap(), Scope::Dirty);
    }

    #[test]
    fn skips_only_a_change_can_end_settle_the_rounds_and_others_are_retried() {
        let op = || Op::Forget(1);
        let skipped =
            |skips: &[Skip]| Report { skipped: skips.iter().map(|s| (op(), *s)).collect(), ..Report::default() };
        // Hard links refused, a name held by a
        // symbolic link. Up to date; their folders wait for a change.
        assert_eq!(settle(&skipped(&[Skip::Refused, Skip::NameTaken]), 0, &mut 0), (true, Some(End::Settled)));
        // A OneDrive folder holding only excluded items: retried at once,
        // it spun through 20 rounds at a time.
        assert_eq!(settle(&skipped(&[Skip::NotEmpty]), 0, &mut 0), (true, Some(End::Settled)));
        // A file open elsewhere: nothing would mark it again, so retried.
        assert_eq!(settle(&skipped(&[Skip::Refused, Skip::Busy]), 0, &mut 0), (false, Some(End::Retry)));
        assert_eq!(settle(&skipped(&[Skip::ContentMismatch]), 0, &mut 0), (false, Some(End::Retry)));
        // Changed meanwhile: another round at once.
        assert_eq!(settle(&skipped(&[Skip::Changed, Skip::Refused]), 0, &mut 0), (false, None));
        let failed = Report { failed: vec![(op(), "411".into())], ..skipped(&[Skip::Refused]) };
        assert_eq!(settle(&failed, 0, &mut 0), (false, Some(End::Retry)));
        // Work done: another round, marks used up unless something waits.
        let busy = Report { done: vec![op()], ..skipped(&[Skip::NameTaken]) };
        assert_eq!(settle(&busy, 0, &mut 0), (true, None));
        assert_eq!(settle(&Report::default(), 0, &mut 0), (true, Some(End::Settled)));
        // Not run (paused meanwhile): neither a failure nor a retry, which
        // would back off; the marks stay all the same (`rounds`).
        let paused = Report { not_run: vec![op()], ..Report::default() };
        assert_eq!(settle(&paused, 0, &mut 0), (true, Some(End::Settled)));
    }

    /// A pair's OneDrive folder is found with names compared as Windows
    /// compares them, part by part, and two folders that match one part
    /// are refused rather than one picked.
    #[test]
    fn a_pairs_onedrive_folder_is_found_by_name_as_windows_compares_names() {
        let mut s = Store::open_in_memory().unwrap();
        let row = |id: &str, parent: Option<&str>, name: &str, kind| oneup_store::RemoteRow {
            id: id.into(),
            parent_id: parent.map(Into::into),
            name: name.into(),
            kind,
            size: 0,
            etag: Some("e".into()),
            qxh: None,
            mtime_ms: None,
        };
        s.write(|o| {
            o.upsert_item(&row("root", None, "root", Kind::Root), false)?;
            o.upsert_item(&row("g", Some("root"), "ΟΔΟΣ", Kind::Folder), false)?;
            o.upsert_item(&row("d", Some("g"), "Docs", Kind::Folder), false)?;
            o.upsert_item(&row("k", Some("root"), "\u{212A}", Kind::Folder), false)?;
            o.upsert_item(&row("f", Some("root"), "file", Kind::File), false)?;
            // Two names OneDrive tells apart that fold alike (final ς and σ).
            o.upsert_item(&row("l1", Some("root"), "λογος", Kind::Folder), false)?;
            o.upsert_item(&row("l2", Some("root"), "λογοσ", Kind::Folder), false)?;
            Ok(())
        })
        .unwrap();
        let find = |p: &str| find_remote(&s, p).unwrap();
        assert_eq!(find("/οδοσ/DOCS").as_deref(), Some("d"));
        assert_eq!(find("/\u{212A}").as_deref(), Some("k"));
        assert_eq!(find("/k"), None, "the Kelvin sign isn't k");
        assert_eq!(find("/file"), None, "not a folder");
        assert_eq!(find("/").as_deref(), Some("root"));
        let err = find_remote(&s, "/ΛΟΓΟΣ/x").unwrap_err().to_string();
        assert!(err.contains("/ΛΟΓΟΣ") && err.contains("λογος") && err.contains("λογοσ"), "{err}");
    }

    /// An upload done over an edit it couldn't keep is logged as done, and
    /// the edit not kept as a failure of its own, naming the file; it isn't
    /// a failed operation (which would be retried and backed off).
    #[test]
    fn an_edit_not_kept_is_logged_as_a_failure_next_to_its_done_upload() {
        use oneup_sync::{Link, LocalId, LocalPre, RemoteId, RemotePre};
        let op = Op::Upload {
            local: LocalPre { id: LocalId(4), stamp: Some((1, 2)) },
            to: RemoteId("d".into()),
            parent: 1,
            name: "a.txt".into(),
            replace: Some(RemotePre { id: RemoteId("r".into()), etag: "e".into() }),
            link: Link::Existing(2),
        };
        let planned = [(op.clone(), Line::new("upload", "/docs/a.txt".into(), ""))];
        let why = "uploaded, over an edit made in OneDrive meanwhile; the file's version history in OneDrive has that edit: HTTP 500";
        let report = Report { done: vec![op.clone()], theirs_not_kept: vec![(op, why.into())], ..Report::default() };
        let log = print_round(1, &planned, &report);
        let got: Vec<_> = log.iter().map(|e| (e.outcome, e.path.as_str(), e.detail.as_str())).collect();
        assert_eq!(got, [(ActivityOutcome::Done, "/docs/a.txt", ""), (ActivityOutcome::Failed, "/docs/a.txt", why)]);
        assert_eq!(settle(&report, 0, &mut 0), (true, None), "not a failure to retry");
    }

    /// A failure that waits its turn (`crate::backoff`) and uploads OneDrive
    /// had no room for are left to later rounds, which hold them: the rounds
    /// settle instead of retrying them at the next wake-up, whatever woke it.
    #[test]
    fn failures_that_wait_their_turn_and_full_uploads_settle_the_rounds() {
        let failed = Report { failed: vec![(Op::Forget(1), "HTTP 500".into())], ..Report::default() };
        assert_eq!(settle(&failed, 1, &mut 0), (true, Some(End::Settled)));
        assert_eq!(settle(&failed, 0, &mut 0), (false, Some(End::Retry)), "one that doesn't wait is retried");
        let full = Report { skipped: vec![(Op::Forget(1), Skip::CloudFull)], ..Report::default() };
        assert_eq!(settle(&full, 0, &mut 0), (true, Some(End::Settled)));
        let both = Report { skipped: vec![(Op::Forget(2), Skip::Busy)], ..full };
        assert_eq!(settle(&both, 0, &mut 0), (false, Some(End::Retry)));
    }

    /// While OneDrive is full, uploads go again once it has more room, or
    /// after an hour whatever it says (it may never say).
    #[test]
    fn uploads_go_again_once_onedrive_has_more_room() {
        let t = std::time::Instant::now();
        let full = |remaining| Full { since_ms: 0, remaining, tried: t, checked: t };
        let at = |s| t + std::time::Duration::from_secs(s);
        let mut f = full(Some(100));
        assert!(!f.room_again(Some(100), at(60)), "the same");
        assert!(!f.room_again(Some(40), at(120)), "less (another device uploaded)");
        assert!(!f.room_again(None, at(180)), "unreadable");
        assert!(f.room_again(Some(101), at(240)), "more");
        let mut f = full(None);
        assert!(!f.room_again(Some(5), at(60)), "the first reading is the baseline");
        assert!(f.room_again(Some(6), at(120)));
        let mut f = full(None);
        assert!(!f.room_again(None, at(3599)));
        assert!(f.room_again(None, at(3600)), "never told: tried again after an hour");
    }

    /// A failed operation waits (`crate::backoff`) while the rest of its
    /// folder goes; its folder stays marked like a held upload's (see
    /// `a_held_upload_is_planned_again_in_the_next_round`).
    #[test]
    fn a_failed_operation_waits_while_the_rest_go() {
        use oneup_sync::{Link, LocalPre};
        let op = |id: u128| Op::Upload {
            local: LocalPre { id: LocalId(id), stamp: Some((1, 2)) },
            to: RemoteId("d".into()),
            parent: 1,
            name: format!("{id}.txt"),
            replace: None,
            link: Link::New,
        };
        let mut s = Store::open_in_memory().unwrap();
        let report = Report { failed: vec![(op(1), "HTTP 500".into())], done: vec![op(2)], ..Report::default() };
        let r = s.write(|o| crate::backoff::record(o, &report, now_ms())).unwrap();
        assert_eq!(r.waiting, 1);
        let at = now_ms();
        let waits: Vec<Option<i64>> =
            s.read(|o| [op(1), op(2), op(3)].iter().map(|op| crate::backoff::until(o, op, at)).collect()).unwrap();
        let mut waits = waits.into_iter();
        let now = std::time::Instant::now();
        let (go, held, first) = hold_back(vec![1, 2, 3], |_| waits.next().flatten().map(|u| after_ms(now, u - at)));
        assert_eq!((go, held), (vec![2, 3], vec![1]));
        let first = first.unwrap().saturating_duration_since(now).as_secs();
        assert!((58..=60).contains(&first), "a minute: {first} s");
    }

    /// A "changed meanwhile" that a new round doesn't clear (a stale row)
    /// gets a few immediate rounds, then the ordinary retry pause: it spun
    /// through every round of a sequence, a OneDrive request each, and
    /// started over 5 s later, for as long as the row stayed.
    #[test]
    fn a_changed_skip_that_doesnt_clear_ends_in_a_retry() {
        let changed = Report { skipped: vec![(Op::Forget(1), Skip::Changed)], ..Report::default() };
        let mut streak = 0;
        for _ in 1..CHANGED_ROUNDS {
            assert_eq!(settle(&changed, 0, &mut streak), (false, None), "another round at once");
        }
        assert_eq!(settle(&changed, 0, &mut streak), (false, Some(End::Retry)));
        // Work done in between starts the count again.
        let mut streak = 0;
        assert_eq!(settle(&changed, 0, &mut streak), (false, None));
        let busy =
            Report { done: vec![Op::Forget(2)], skipped: vec![(Op::Forget(1), Skip::Changed)], ..Report::default() };
        assert_eq!(settle(&busy, 0, &mut streak), (false, None));
        for _ in 1..CHANGED_ROUNDS {
            assert_eq!(settle(&changed, 0, &mut streak), (false, None));
        }
    }

    /// A held upload's folder keeps its mark through the end of the round
    /// (`Touched`, `dirty_clear_except`), so the next round plans it again.
    #[test]
    fn a_held_upload_is_planned_again_in_the_next_round() {
        use oneup_store::{Kind, LocalKind, LocalRow, RemoteRow, Skip};
        use oneup_sync::{Base, Link, LocalPre, NodeKind, ROOT, RemoteId};
        let local = |id: u128, parent: Option<u128>, name: &str, kind: LocalKind, size: i64| LocalRow {
            file_id: id,
            parent_id: parent,
            name: name.into(),
            name_lossy: false,
            kind,
            size,
            mtime: 9,
            attributes: 0,
            reparse_tag: 0,
            skip: Skip::None,
        };
        let remote = |id: &str, parent: Option<&str>, kind: Kind| RemoteRow {
            id: id.into(),
            parent_id: parent.map(Into::into),
            name: id.into(),
            kind,
            size: 0,
            etag: Some("e".into()),
            qxh: None,
            mtime_ms: None,
        };
        let base = |sid, parent, name: &str, local: u128, remote: &str| Base {
            sid,
            parent,
            name: name.into(),
            kind: NodeKind::Folder,
            size: 0,
            hash: None,
            local: Some(LocalId(local)),
            local_mtime: 0,
            remote: Some(RemoteId(remote.into())),
            remote_etag: "e".into(),
        };
        let mut s = Store::open_in_memory().unwrap();
        let d = s
            .write(|o| {
                o.local_put(&local(1, None, "", LocalKind::Root, 0))?;
                o.local_put(&local(3, Some(1), "d", LocalKind::Folder, 0))?;
                o.upsert_item(&remote("root", None, Kind::Root), false)?;
                o.upsert_item(&remote("d", Some("root"), Kind::Folder), false)?;
                o.synced_put(&base(ROOT, None, "", 1, "root"))?;
                o.synced_insert(&base(0, Some(ROOT), "d", 3, "d"))
            })
            .unwrap();
        s.write(|o| o.dirty_clear(i64::MAX).map(drop)).unwrap();
        // A program saves a new file in d, again and again.
        s.write(|o| o.local_put(&local(4, Some(3), "a.db", LocalKind::File, 7)).map(drop)).unwrap();
        let round = |s: &mut Store, held: &[Op]| {
            s.write(|o| {
                let scope = o.dirty_scope()?;
                let mut retried = Touched::default();
                for op in held {
                    retried.add(op);
                }
                o.dirty_clear_except(scope.upto, &retried.local, &retried.remote, &retried.synced)?;
                Ok(scope.folders)
            })
            .unwrap()
        };
        let upload = Op::Upload {
            local: LocalPre { id: LocalId(4), stamp: Some((7, 9)) },
            to: RemoteId("d".into()),
            parent: d,
            name: "a.db".into(),
            replace: None,
            link: Link::New,
        };
        assert!(round(&mut s, &[upload]).contains(&d), "the round plans d and holds the upload");
        assert!(round(&mut s, &[]).contains(&d), "so the next round plans d again, and sends it");
        assert!(!round(&mut s, &[]).contains(&d), "after which d is done");
    }

    #[test]
    fn held_items_wait_in_order_and_the_earliest_time_is_kept() {
        let t = std::time::Instant::now();
        let at = |s: u64| Some(t + std::time::Duration::from_secs(s));
        let (now, held, first) = hold_back(vec![1, 2, 3, 4, 5], |n| match n {
            2 => at(30),
            4 => at(15),
            _ => None,
        });
        assert_eq!((now, held, first), (vec![1, 3, 5], vec![2, 4], at(15)));
        assert_eq!(hold_back(vec![1], |_| None), (vec![1], vec![], None));
    }

    #[test]
    fn only_what_reads_a_local_file_to_send_it_waits_for_it() {
        use oneup_sync::{Link, LocalPre, RemoteId};
        let local = LocalPre { id: LocalId(7), stamp: Some((1, 2)) };
        let upload = Op::Upload {
            local: local.clone(),
            to: RemoteId("p".into()),
            parent: 1,
            name: "a.db".into(),
            replace: None,
            link: Link::New,
        };
        assert_eq!(sent_file(&upload), Some(7));
        assert_eq!(sent_file(&Op::Hash(local)), Some(7));
        assert_eq!(sent_file(&Op::Forget(3)), None);
    }

    #[test]
    fn a_chosen_rename_that_meets_the_plan_waits_for_a_later_round() {
        use oneup_sync::{Link, LocalPre, RemotePre};
        let pre = |id| LocalPre { id: LocalId(id), stamp: Some((1, 1)) };
        let rename = |id, name: &str| Op::RenameLocal { local: pre(id), dir: LocalId(1), name: name.into() };
        let download = Op::Download {
            remote: RemotePre { id: RemoteId("r".into()), etag: "e".into() },
            to: LocalId(1),
            parent: 0,
            name: "Notes.txt".into(),
            replace: None,
            link: Link::New,
        };
        let upload = Op::Upload {
            local: pre(5),
            to: RemoteId("root".into()),
            parent: 0,
            name: "five.txt".into(),
            replace: None,
            link: Link::New,
        };
        let mut ops = vec![download, upload];
        let extra = vec![
            rename(4, "notes.txt"), // the plan downloads into that name
            rename(5, "other.txt"), // the plan uploads that item
            rename(6, "free.txt"),
            rename(7, "FREE.txt"), // another chosen rename takes it first
        ];
        add_extra(&mut ops, extra);
        assert_eq!(&ops[2..], [rename(6, "free.txt")]);
    }

    /// A change to the database patterns alone (no other event) plans
    /// every folder again: they decide which working files are parked.
    #[test]
    fn database_patterns_are_part_of_the_planning_fingerprint() {
        use oneup_rules::database::Databases;
        let rules = RuleSet::default();
        let none = Databases::default();
        let ledger = Databases::new(true, &[], &["*.ledger"]).unwrap();
        let finance = Databases::new(true, &["finance"], &[]).unwrap();
        assert_eq!(planning_fingerprint(&rules, &none), rules.fingerprint(), "unchanged without patterns");
        let all = [&none, &ledger, &finance].map(|d| planning_fingerprint(&rules, d));
        assert!(all[0] != all[1] && all[1] != all[2] && all[0] != all[2], "{all:?}");
        assert_eq!(planning_fingerprint(&rules, &ledger), planning_fingerprint(&rules, &ledger.clone()));
    }

    #[test]
    fn tokens_differ() {
        assert_ne!(random_token(), random_token());
        assert_eq!(random_token().len(), 32);
    }
}
