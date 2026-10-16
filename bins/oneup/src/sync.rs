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
//!    precondition right before acting.
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
    Approval, Approved, Breaker, BreakerKind, Choice, Context as GateContext, Decision, Fingerprint, OldCopy, Overflow,
    OverflowOutcome, Recent, Side, Withheld, is_mass_sensitive, review_in, store_bytes,
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
    if !opts.dry_run
        && store.read(|o| o.meta_get(KEY_LOCAL))?.is_none()
        && let Some(said) = crate::carryover::apply(&opts.data_dir, &mut store, &opts.local, &opts.remote)?
    {
        println!("{said}");
        carried = true;
    }
    let Some((store, engine)) = prepare(store, graph, rules, opts).await? else { return Ok(()) };
    if carried {
        // Paired now: the app's choices are used.
        crate::carryover::mark_applied(&opts.data_dir)?;
    }
    if opts.pair_only {
        println!("Ready: the background agent does the syncing from here.");
        return Ok(());
    }
    let controls = Controls { answers: opts.answers.clone(), ..Controls::default() };
    let (mut store, out) =
        engine.rounds(store, Scope::All, &|| controls.clone(), Recent::default(), opts.max_rounds, &|_| {}).await?;
    if let Some(o) = &out.overflow {
        print_overflow(o, &store, Asker::Sync)?;
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
            if !crate::test_account::same_dir(Path::new(l), &local) || !r.eq_ignore_ascii_case(&remote_path) {
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
    LocalFs::open(Path::new(&root.path), root.volume_serial, root.root_file_id, &token, false)
        .context("opening the sync folder")
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
    /// Mass-sensitive operations carried out, per side (for the agent's
    /// safety-stop window, see [`oneup_gate::Recent`]).
    pub executed: Recent,
    /// The last version-store overflow batch the rounds saw, if any (see
    /// [`print_overflow`]).
    pub overflow: Option<Overflow>,
    /// The tripped safety stops, when `end` is [`End::Stopped`] (see
    /// [`print_breakers`]).
    pub breakers: Vec<Breaker>,
    /// When the earliest upload the last round held for a file that keeps
    /// changing may go ([`crate::busy`]): the caller runs a round then.
    pub held_until: Option<std::time::Instant>,
}

/// Everything a round needs besides the database: the executor (with the
/// opened sync folder), OneDrive, and the exclusion rules.
pub struct Engine {
    exec: Executor<crate::vaultpair::PairCloud>,
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
    /// The store's invariants were checked once this run ([`Engine::remote`]).
    verified: std::sync::atomic::AtomicBool,
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
        // Uploads hold up to a fragment in memory; the profile bounds it.
        let memory = crate::config::load_agent(data_dir).map(|a| a.memory).unwrap_or_default();
        let plain = GraphCloud::with_memory(Arc::clone(&graph), oneup_exec::UploadMemory::for_profile(&memory));
        let cloud = match &vault {
            Some(v) => crate::vaultpair::PairCloud::Vault(v.cloud(plain)),
            None => crate::vaultpair::PairCloud::Plain(plain),
        };
        let exec = Executor::new(fs, cloud, rules.clone());
        Self {
            exec,
            graph,
            vault,
            rules: std::sync::Mutex::new(rules),
            data_dir: data_dir.to_path_buf(),
            rule_files: std::sync::Mutex::default(),
            logged: std::sync::Mutex::new(Vec::new()),
            offer: std::sync::Mutex::new(None),
            converting: std::sync::Mutex::new(Converting::default()),
            busy: std::sync::OnceLock::new(),
            verified: std::sync::atomic::AtomicBool::new(false),
        }
    }

    /// Holds the uploads of the files `busy` says keep changing.
    pub fn hold_busy(&self, busy: Arc<crate::busy::Busy>) {
        let _ = self.busy.set(busy);
    }

    /// Until when `op` waits because its file keeps changing, if it does.
    fn held_until(&self, op: &Op, now: std::time::Instant) -> Option<std::time::Instant> {
        self.busy.get()?.held_until(sent_file(op)?, now)
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
        Ok(crate::prune::counted(&self.graph, ready).await?.iter().map(crate::prune::Candidate::op).collect())
    }

    /// Files On-Demand placement after a round (`oneup_exec::placement`):
    /// pinned files are downloaded, files set to free up space are freed
    /// through the gate. Nothing while changes to this PC are paused.
    pub async fn placement(&self, mut store: Store, controls: &Controls) -> Result<Store> {
        if !self.exec.local().on_demand() || controls.pause_local {
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
        let fetched = oneup_exec::placement::fetch_pinned(&store, Path::new(&root.path)).await?;
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
        let note = self.space_policy(&mut store, Path::new(&root.path), &mut ops)?;
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
    fn space_policy(&self, store: &mut Store, root: &Path, ops: &mut Vec<Op>) -> Result<Option<String>> {
        const KEY_LAST: &str = "space.last_ms";
        let space = crate::config::load_space(&self.data_dir)?;
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
        let sel = crate::space::select(store, &space, disk, now, &|rel| crate::space::last_access(root, rel))?;
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
    fn save_stop_lists(&self, store: &Store, review: &oneup_gate::Review, suspicious: &[LocalId]) {
        let codes: Vec<String> = review.breakers.iter().map(|b| format!("{:016x}", b.fingerprint.0)).collect();
        crate::stoplist::keep_only(&self.data_dir, &codes);
        for (b, code) in review.breakers.iter().zip(&codes) {
            if crate::stoplist::path(&self.data_dir, code).exists() {
                continue;
            }
            let held: Vec<&Op> = review
                .withheld
                .iter()
                .filter(|(op, w)| {
                    *w == Withheld::Breaker(b.side)
                        && match b.kind {
                            BreakerKind::Mass => is_mass_sensitive(op),
                            BreakerKind::Suspicious => oneup_gate::is_suspicious(op, suspicious),
                        }
                })
                .map(|(op, _)| op)
                .collect();
            let lines = store.read(|o| {
                Ok(held
                    .iter()
                    .take(crate::stoplist::MAX_LINES)
                    .map(|op| describe(o, op).to_string())
                    .collect::<Vec<_>>())
            });
            let header = format!("OneUpDrive safety stop {code}: {}. Nothing has been changed yet.", breaker_what(b));
            let saved = lines
                .map_err(anyhow::Error::from)
                .and_then(|l| Ok(crate::stoplist::write(&self.data_dir, code, &header, &l, held.len())?));
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
    /// caller's safety window; this call's own are added as it goes.
    /// `controls` is asked again before every round, so a pause or a
    /// `sync-now PATH` that arrives meanwhile counts from the next round.
    pub async fn rounds(
        &self,
        mut store: Store,
        first: Scope,
        controls: &(dyn Fn() -> Controls + Send + Sync),
        recent: Recent,
        max_rounds: usize,
        progress: &(dyn Fn(Option<Progress>) + Send + Sync),
    ) -> Result<(Store, Outcome)> {
        let mut out = Outcome {
            end: End::Unsettled,
            rounds: 0,
            totals: Totals::default(),
            executed: Recent::default(),
            overflow: None,
            breakers: Vec::new(),
            held_until: None,
        };
        let mut scope = first;
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
            let settings = crate::config::load_agent(&self.data_dir)?;
            let capped = crate::autopause::caps_apply(&settings, crate::autopause::local_minute_of_day());
            let cap = |kib: u64| if capped { kib.saturating_mul(1024) } else { 0 };
            self.graph.upload.set(cap(settings.upload_kib));
            self.graph.download.set(cap(settings.download_kib));
            let pruning = self.update_pruning(&store)?;
            store = observe(store, &pruning).await?;
            store = self.remote(store, &pruning).await?;
            let user_rules = self.load_rules(&mut store)?;
            // Changed rules: every folder is planned again, from the
            // database (rules.md § Changing rules without a rescan).
            let fingerprint = user_rules.fingerprint();
            let rules_changed = store.read(|o| o.meta_get(KEY_RULES))?.as_deref() != Some(fingerprint.as_str());
            if rules_changed {
                scope = Scope::All;
            }
            let window = recent.plus(out.executed);
            // What happens to OneDrive's copies of excluded items.
            let mode = crate::config::load_rules(&self.data_dir, &store)?.cloud;
            let extra = match mode {
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
            let modes = Modes { pull: mode != crate::config::CloudCopies::Keep, extra };
            let t = std::time::Instant::now();
            let Planned { review, problems, upto, suspicious } =
                plan_round(&store, Some(self.exec.local()), scope, controls, window, &user_rules, modes)?;
            // Said, so a slow start can be told from a slow round (one
            // can take minutes on a large folder).
            if scope == Scope::All || t.elapsed().as_secs() >= 10 {
                tracing::info!(round = n, all = scope == Scope::All, s = t.elapsed().as_secs(), "planned");
            }
            let planned_all = scope == Scope::All;
            scope = Scope::Dirty;
            out.totals.problems = problems;
            if !review.breakers.is_empty() {
                // Nothing at all runs in a tripped round, not even the side
                // that didn't trip: the user should see the whole picture
                // first. The marks stay, so the next try plans it all again.
                for b in &review.breakers {
                    let detail = format!("{}; nothing was changed (code {:016x})", breaker_what(b), b.fingerprint.0);
                    let e = notice(ActivityOutcome::Waiting, "safety stop", detail);
                    self.log_once(&mut store, format!("stop {:x}", b.fingerprint.0), e)?;
                }
                self.save_stop_lists(&store, &review, &suspicious);
                out.breakers = review.breakers;
                out.end = End::Stopped;
                return Ok((store, out));
            }
            // No stop stands: their lists go.
            crate::stoplist::keep_only(&self.data_dir, &[]);
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
            let (mut first, last) = crate::config::load_priority(&self.data_dir)?;
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
                        let l = describe_approved(o, &a);
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
            out.held_until = until;
            let waiting: Vec<(Op, Line)> = busy.into_iter().map(|(a, l)| (a.op().clone(), l)).collect();
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
            let bytes_total = store.read(|o| transfer_bytes(o, &approved, on_demand))?;
            let report = self.run_reported(&mut store, approved, n, bytes_total, progress).await;
            let mut log = print_round(n, &planned, &report);
            if let Some(why) = &report.halted {
                log.push(notice(ActivityOutcome::Failed, "stopped", why.clone()));
            }
            // A database that fails can't take the log either; the halt
            // below still reports it.
            let logged = log_activity(&mut store, log);
            if let Some(why) = report.halted {
                return Err(Halted(why).into());
            }
            logged?;
            out.totals.add(&report);
            out.executed.suspicious +=
                report.done.iter().filter(|op| oneup_gate::is_suspicious(op, &suspicious)).count();
            for op in report.done.iter().filter(|op| is_mass_sensitive(op)) {
                match Side::changed_by(op) {
                    Some(Side::Local) => out.executed.local += 1,
                    Some(Side::Remote) => out.executed.remote += 1,
                    None => {}
                }
            }
            let (clean, mut end) = settle(&report);
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
                let mut retried = Touched::default();
                if !clean {
                    for op in report.skipped.iter().map(|(op, _)| op).chain(report.failed.iter().map(|(op, _)| op)) {
                        retried.add(op);
                    }
                }
                for (op, _) in &waiting {
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
        found.extend(held);
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
    /// file moves them while it's on its way.
    async fn run_reported(
        &self,
        store: &mut Store,
        approved: Vec<Approved>,
        round: usize,
        bytes_total: u64,
        progress: &(dyn Fn(Option<Progress>) + Send + Sync),
    ) -> Report {
        use std::sync::atomic::{AtomicU64, Ordering::Relaxed};
        let items_total = approved.len() as u64;
        let items_done = AtomicU64::new(0);
        let moved = || self.graph.upload.moved() + self.graph.download.moved();
        let moved_before = moved();
        let started = std::time::Instant::now();
        let tell = || {
            progress(Some(Progress {
                round,
                items_done: items_done.load(Relaxed),
                items_total,
                bytes_done: moved().saturating_sub(moved_before).min(bytes_total),
                bytes_total,
                elapsed_ms: u64::try_from(started.elapsed().as_millis()).unwrap_or(u64::MAX),
            }));
        };
        let count = |n: usize| {
            items_done.fetch_add(n as u64, Relaxed);
        };
        let run = self.exec.run_counted(store, approved, &count);
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
fn plan_round(
    store: &Store,
    fs: Option<&LocalFs>,
    scope: Scope,
    controls: &Controls,
    recent: Recent,
    rules: &RuleSet,
    modes: Modes,
) -> Result<Planned> {
    let approvals = controls.answers.approvals();
    let decisions = controls.answers.decisions();
    let policy = oneup_gate::Policy {
        pause_local: controls.pause_local,
        pause_remote: controls.pause_remote,
        ..crate::versions::policy(store)?
    };
    let synced = store.read(|o| o.synced_stats())?.items.max(0) as u64;
    // Exclusions only stop uploads (`pull`, `prune`): excluded items stay
    // visible to planning (only paused folders, and those neither side
    // lists, are hidden), and what would change OneDrive for them is
    // dropped below.
    let hide = if modes.pull { rules.upload_only() } else { rules.clone() };
    let flow = crate::flow::get(store)?;
    let (ops, problems, upto) = store.read(|o| {
        let p = Parked::with_rules(o, &hide);
        let dirty = o.dirty_scope()?;
        let folders = match scope {
            Scope::All => all_folders(&p)?,
            Scope::Dirty => dirty.folders,
        };
        let mut ops = plan_flow(&p, folders, flow)?;
        if modes.pull {
            let judge = Parked::with_rules(o, rules);
            let mut kept = Vec::with_capacity(ops.len());
            for op in ops {
                if !judge.sends_excluded(&op)? {
                    kept.push(op);
                }
            }
            ops = kept;
        }
        ops.extend(modes.extra);
        let problems = p
            .problems()
            .into_iter()
            .map(|(id, name, why)| Ok((o.local_path_of(id.0)?.unwrap_or(name), why)))
            .collect::<oneup_store::Result<Vec<_>>>()?;
        Ok((ops, problems, dirty.upto))
    })?;
    let room = match fs {
        Some(fs) if store_bytes(&policy, &ops) > 0 => fs.room(&crate::versions::trim_settings(store)?)?,
        _ => None,
    };
    let suspicious = match fs {
        Some(fs) if policy.max_suspicious > 0 => judge_content(fs, &ops),
        _ => Vec::new(),
    };
    let cx = GateContext {
        synced_items: synced,
        recent,
        approvals: &approvals,
        decisions: &decisions,
        room,
        suspicious: &suspicious,
    };
    let review = review_in(&policy, &cx, ops);
    Ok(Planned { review, problems, upto, suspicious })
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
    /// `prune`: their removal, added to the plan for the gate.
    extra: Vec<Op>,
}

/// One round's reviewed plan.
struct Planned {
    review: oneup_gate::Review,
    /// Items that can't be synced, for the report.
    problems: Vec<(String, Why)>,
    /// The dirty marks' high-water number, read *before* planning: once the
    /// round's operations all ran, everything marked up to it is done.
    upto: i64,
    /// Files judged by the content guard (sorted).
    suspicious: Vec<LocalId>,
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
    let modes = Modes { pull, extra: Vec::new() };
    let Planned { review, problems, .. } =
        plan_round(&store, fs.as_ref(), Scope::All, &controls, Recent::default(), &user_rules, modes)?;
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
        Ok(())
    })?;
    let all = review.approved.len() + review.withheld.len();
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
    println!("Later rounds continue from there (e.g. a folder's contents after the folder).");
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
            Ok(_) => Ok(obs.into_store()),
            Err(Error::NeedsRescan(why @ (Rescan::JournalWrapped | Rescan::JournalReplaced))) => {
                rescan(obs.into_store(), cfg, why)
            }
            Err(e) => Err(e.into()),
        }
    })
    .await?
}

/// The change journal lost track of the synced folder: expected after the
/// PC (or the agent) was off for a while, because the journal keeps only so
/// much history (`sync-engine.md` § Journal loss). The folder is looked through
/// again. Nothing is inferred from what the scan doesn't find: a synced item
/// missing afterwards is held and reported, never deleted (`data-safety.md`
/// invariant 3). Only the same folder (same volume, same folder ID) is
/// rescanned; anything else stops sync, as before.
fn rescan(store: Store, cfg: oneup_local::Config, why: oneup_local::observe::Rescan) -> Result<Store> {
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
    let detail = format!(
        "{why}; looked through again: {} files and {} folders in {:.1?}",
        summary.stats.files, summary.stats.folders, summary.elapsed
    );
    println!("  {detail}");
    log_activity(&mut store, vec![notice(ActivityOutcome::Note, "rescan", detail)])?;
    // Changes made while the scan ran are in the journal from its start.
    let mut obs = oneup_local::observe::Observer::open(store, cfg)?;
    obs.catch_up()?;
    Ok(obs.into_store())
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

/// The ID of the OneDrive folder at `path`, from the mirror (names compared
/// case-insensitively, as OneDrive does).
pub fn find_remote(store: &Store, path: &str) -> Result<Option<String>> {
    Ok(store.read(|o| {
        let Some(mut cur) = o.remote_root()? else { return Ok(None) };
        for part in path.split('/').filter(|s| !s.is_empty()) {
            let next = o
                .remote_children(&cur.id)?
                .into_iter()
                .map(|(row, _)| row)
                .find(|row| row.kind == Kind::Folder && row.name.to_lowercase() == part.to_lowercase());
            match next {
                Some(row) => cur = row,
                None => return Ok(None),
            }
        }
        Ok(Some(cur.id))
    })?)
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
        Op::ConflictCopy { local, name, .. } => {
            moved("keep both", lp(local.id.0), name.clone(), "changed on both sides")
        }
        Op::DeleteRemote { remote, .. } => Line::new("delete", rp(&remote.id), "to OneDrive's recycle bin"),
        Op::Dehydrate { local, .. } => Line::new("free up", lp(local.id.0), ""),
        Op::PruneRemote { remote, .. } => {
            Line::new("prune", rp(&remote.id), "excluded; to OneDrive's recycle bin, kept here")
        }
        Op::RecycleLocal { local, .. } if dropped => {
            Line::new("remove", lp(local.id.0), "no copy kept, as you chose; in OneDrive's recycle bin")
        }
        Op::RecycleLocal { local, .. } => Line::new("recycle", lp(local.id.0), "to .oneup\\versions"),
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
/// (`None`: another round, at once).
fn settle(report: &Report) -> (bool, Option<End>) {
    // Every operation planned from the marks ran: they're used up. A
    // skipped or failed one may have nothing that would mark its folder
    // again (a file open elsewhere changes nothing), so then the marks stay
    // for the retry. A skip that only a change to the item can end counts as
    // done: the change marks it again.
    let clean = report.skipped.iter().all(|(_, s)| waits_for_a_change(*s)) && report.failed.is_empty();
    if !report.is_idle() {
        return (clean, None);
    }
    if clean {
        return (true, Some(End::Settled));
    }
    // A skip because something changed meanwhile is worth an immediate new
    // round (the change is seen now); anything else waits for the caller's
    // retry.
    let only_changed =
        report.failed.is_empty() && report.skipped.iter().all(|(_, s)| *s == Skip::Changed || waits_for_a_change(*s));
    (false, (!only_changed).then_some(End::Retry))
}

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
    }
}

/// Prints each operation with its outcome, and returns the activity-log
/// entries for them. `planned` is the approved list in order, described
/// before it ran (afterwards a recycled item has no path). The executor
/// runs operations in that order and puts each in exactly one of the
/// report's lists, keeping the order, so one merge pass pairs them up.
/// How an upload done as a server-side copy is described.
const COPIED: &str = "OneDrive already had this content, so it copied it there: nothing was sent";

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
            } else if !matches!(op, Op::Record(_) | Op::Forget(_)) {
                println!("  ok    {line}");
                if !quiet(op) {
                    entry(line, ActivityOutcome::Done, None);
                }
            }
        } else if let Some((_, s)) = skipped.next_if(|(o, _)| o == op) {
            println!("  skip  {line}  ({})", skip_label(*s));
            entry(line, ActivityOutcome::Skipped, Some(skip_label(*s).into()));
        } else if let Some((_, m)) = failed.next_if(|(o, _)| o == op) {
            println!("  FAIL  {line}  ({m})");
            entry(line, ActivityOutcome::Failed, Some(m.clone()));
        } else {
            println!("  --    {line}  (not run: the round stopped)");
        }
    }
    log
}

const KEPT_INSTEAD: &str =
    "its old copy was kept in .oneup\\versions after all: it differed from the version OneDrive has";

pub fn now_ms() -> i64 {
    std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).map(|d| d.as_millis() as i64).unwrap_or(0)
}

/// Splits `items` into those that go now and those `until` holds back,
/// keeping their order, with the earliest time a held one may go.
fn hold_back<T>(
    items: Vec<T>,
    until: impl Fn(&T) -> Option<std::time::Instant>,
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
        BreakerKind::Mass => format!("{} items {side} would be deleted, overwritten or moved", b.count),
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
        assert_eq!(settle(&skipped(&[Skip::Refused, Skip::NameTaken])), (true, Some(End::Settled)));
        // A OneDrive folder holding only excluded items: retried at once,
        // it spun through 20 rounds at a time.
        assert_eq!(settle(&skipped(&[Skip::NotEmpty])), (true, Some(End::Settled)));
        // A file open elsewhere: nothing would mark it again, so retried.
        assert_eq!(settle(&skipped(&[Skip::Refused, Skip::Busy])), (false, Some(End::Retry)));
        assert_eq!(settle(&skipped(&[Skip::ContentMismatch])), (false, Some(End::Retry)));
        // Changed meanwhile: another round at once.
        assert_eq!(settle(&skipped(&[Skip::Changed, Skip::Refused])), (false, None));
        let failed = Report { failed: vec![(op(), "411".into())], ..skipped(&[Skip::Refused]) };
        assert_eq!(settle(&failed), (false, Some(End::Retry)));
        // Work done: another round, marks used up unless something waits.
        let busy = Report { done: vec![op()], ..skipped(&[Skip::NameTaken]) };
        assert_eq!(settle(&busy), (true, None));
        assert_eq!(settle(&Report::default()), (true, Some(End::Settled)));
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
    fn tokens_differ() {
        assert_ne!(random_token(), random_token());
        assert_eq!(random_token().len(), 32);
    }
}
