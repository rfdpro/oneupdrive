//! The executor proper (see the crate docs).

use std::collections::VecDeque;
use std::io::Seek as _;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering::Relaxed};
use std::time::{Duration, Instant};

use futures_util::StreamExt as _;
use futures_util::stream::FuturesUnordered;
use oneup_cfapi::identity::Identity;
use oneup_effects::{Done, Error as FxError, LocalFs, Version};
use oneup_gate::{Approved, OldCopy};
use oneup_graph::client::SIMPLE_UPLOAD_MAX;
use oneup_rules::ExcludeSet;
use oneup_store::{Kind, LocalKind, LocalRow, Ops, RemoteRow, Store};
use oneup_sync::{
    Base, Dir, Hash, Link, LocalId, LocalPre, NodeKind, Op, ROOT, RemoteId, RemotePre, Sid, Vacates,
    conflict_candidate, fold,
};

use crate::blocking::{Jobs, OffTask, Settle};
use crate::cloud::{Cloud, CloudError, Hashing, Meta, MetaDone, VersionMark};
use crate::schedule::{Claims, Held, Node, Parallel};
use crate::timing::Timings;

/// The store, shared by the operations under way. Each use locks it for
/// one synchronous read or transaction, never across an `.await`.
///
/// Writes don't wait inside SQLite for another connection's write (the
/// hydration service, the settings window, a command), which would block
/// the task every operation under way runs on, for up to
/// [`oneup_store::BUSY_WAIT`]: they wait here instead ([`Db::write`]).
/// Reads don't wait for writers (WAL).
struct Db<'a>(std::sync::Mutex<&'a mut Store>);

impl<'a> Db<'a> {
    fn new(store: &'a mut Store) -> Self {
        Self(std::sync::Mutex::new(store))
    }

    fn lock(&self) -> std::sync::MutexGuard<'_, &'a mut Store> {
        self.0.lock().unwrap_or_else(std::sync::PoisonError::into_inner)
    }

    fn read<T>(&self, f: impl FnOnce(&Ops<'_>) -> oneup_store::Result<T>) -> oneup_store::Result<T> {
        self.lock().read(f)
    }

    /// Runs `f` in one write transaction, once no other connection is
    /// writing: tries at once and, while one is, again after a pause that
    /// grows to [`BUSY_PAUSE_MAX`], the task going on with the operations
    /// under way meanwhile. Gives up, as SQLite would, after
    /// [`oneup_store::BUSY_WAIT`].
    async fn write<T, E: From<oneup_store::Error>>(&self, f: impl FnOnce(&Ops<'_>) -> Result<T, E>) -> Result<T, E> {
        self.write_as(false, f).await
    }

    /// [`Db::write`] for a transaction that must outlive the effect after
    /// it: a record written ahead of the effect ([`PENDING`]), or state
    /// whose rollback would make the next round undo the effect. Committed
    /// durably (`Store::try_write_now_durable`), so it's on disk, with
    /// every commit before it, before the effect starts and outlives a
    /// power cut (`data-safety.md` invariant 7). Such a transaction says so
    /// (`Ops::meta_set_ahead`, `Ops::require_durable`), which refuses
    /// anywhere else.
    async fn write_ahead<T, E: From<oneup_store::Error>>(
        &self,
        f: impl FnOnce(&Ops<'_>) -> Result<T, E>,
    ) -> Result<T, E> {
        self.write_as(true, f).await
    }

    async fn write_as<T, E: From<oneup_store::Error>>(
        &self,
        durable: bool,
        f: impl FnOnce(&Ops<'_>) -> Result<T, E>,
    ) -> Result<T, E> {
        let started = Instant::now();
        let mut pause = Duration::from_millis(1);
        let mut f = f;
        loop {
            let tried = {
                let mut store = self.lock();
                if durable { store.try_write_now_durable(f) } else { store.try_write_now(f) }
            };
            match tried {
                Ok(done) => return done,
                Err((_, busy)) if started.elapsed() >= oneup_store::BUSY_WAIT => return Err(busy.into()),
                Err((back, _)) => f = back,
            }
            tokio::time::sleep(pause).await;
            pause = (pause * 2).min(BUSY_PAUSE_MAX);
        }
    }
}

/// The longest pause between tries of a write while another connection
/// writes ([`Db::write`]), as SQLite's own wait does.
const BUSY_PAUSE_MAX: Duration = Duration::from_millis(100);

/// How many operations, from the oldest not yet finished, the executor
/// takes in to look for ones it can start. Behind one slow operation, the
/// rest go at most this far ahead (plus one batch), which bounds what is
/// held for the report.
const WINDOW: usize = 256;

/// At most this many operations a round takes in for the folders it
/// creates ([`Executor::run_following`]); what's in folders created after
/// that waits for the next round, which their marks bring.
pub const FOLLOW_MAX: usize = 20_000;

/// Plans and reviews what goes into folders a round has just created: given
/// the store, those folders (Synced rows the round linked) and how many
/// operations may still be taken in, the approved operations for their
/// contents, in the planner's order, at most that many. The caller's
/// planner and gate, as for a round ([`Executor::run_following`]).
pub type Follow<'a> = dyn Fn(&Ops<'_>, &[Sid], usize) -> Vec<Approved> + Sync + 'a;

/// Why an operation was skipped. Nothing was changed; the next round plans
/// again from fresh state.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Skip {
    /// An item (local or cloud) changed since it was planned.
    Changed,
    /// A local item is open in another program.
    Busy,
    /// The destination name is taken.
    NameTaken,
    /// A folder to recycle has gained contents.
    NotEmpty,
    /// The cloud's content didn't match what was expected or sent.
    ContentMismatch,
    /// Can't be done as planned: a name Windows can't use, a special item
    /// (placeholder, link), or something outside the sync folder.
    Refused,
    /// The OneDrive has no room left: an upload or a new folder refused
    /// for it, or one not tried after OneDrive said so in the same round.
    CloudFull,
}

/// What one round did.
#[derive(Debug, Default)]
pub struct Report {
    pub done: Vec<Op>,
    pub skipped: Vec<(Op, Skip)>,
    pub failed: Vec<(Op, String)>,
    /// Set if the round stopped early: the sync folder failed its identity
    /// check, or the state database failed.
    pub halted: Option<String>,
    /// Done, but an old copy the gate allowed to drop was kept in the
    /// version store instead: it wasn't provably the version OneDrive has
    /// (see [`OldCopy::DropIfSynced`]).
    pub kept_instead: Vec<Op>,
    /// Uploads done as a copy, in OneDrive, of a file with the same content
    /// (nothing was sent).
    pub copied: Vec<Op>,
    /// Not run: the caller's check refused them when their turn came
    /// ([`Executor::run_checked`]), or stopped a download of theirs under
    /// way. Nothing was changed, and they're neither skipped nor failed:
    /// the next round plans them again.
    pub not_run: Vec<Op>,
    /// Uploads that went over an edit made in OneDrive while they were
    /// committing (see [`Cloud::version_mark`]), and the name of the new
    /// file in OneDrive that keeps the edited version.
    pub kept_theirs: Vec<(Op, String)>,
    /// Uploads that went over such an edit whose version couldn't be kept
    /// as a new file, and why. The upload itself is done (they're in
    /// [`Report::done`] too), and the edit is in the file's version
    /// history in OneDrive.
    pub theirs_not_kept: Vec<(Op, String)>,
}

impl Report {
    /// Whether the round changed nothing at all.
    pub fn is_idle(&self) -> bool {
        self.done.is_empty()
    }
}

enum Fail {
    Skip(Skip),
    Failed(String),
    Halt(String),
    /// The caller's check refused it (see [`Report::not_run`]).
    NotNow,
}

impl From<FxError> for Fail {
    fn from(e: FxError) -> Self {
        #[cfg(feature = "crash-test")]
        if oneup_effects::Crashed::is(&e) {
            return Fail::Halt(CRASHED.into());
        }
        match e {
            FxError::Changed => Fail::Skip(Skip::Changed),
            FxError::Busy => Fail::Skip(Skip::Busy),
            FxError::NameTaken => Fail::Skip(Skip::NameTaken),
            FxError::NotEmpty => Fail::Skip(Skip::NotEmpty),
            FxError::BadName | FxError::NotOrdinary | FxError::Outside | FxError::WrongOp => Fail::Skip(Skip::Refused),
            FxError::RootChanged(_) | FxError::OtherEngine(_) => Fail::Halt(e.to_string()),
            FxError::KeptAside { .. } | FxError::Win(_) | FxError::Io(_) => Fail::Failed(e.to_string()),
        }
    }
}

impl From<oneup_store::Error> for Fail {
    fn from(e: oneup_store::Error) -> Self {
        Fail::Halt(format!("state database: {e}"))
    }
}

impl From<CloudError> for Fail {
    fn from(e: CloudError) -> Self {
        match e {
            CloudError::Changed => Fail::Skip(Skip::Changed),
            CloudError::NameTaken => Fail::Skip(Skip::NameTaken),
            CloudError::NotEmpty => Fail::Skip(Skip::NotEmpty),
            CloudError::Full => Fail::Skip(Skip::CloudFull),
            CloudError::Io(_) | CloudError::Other(_) | CloudError::TooManyItems => Fail::Failed(e.to_string()),
        }
    }
}

type Res<T = ()> = Result<T, Fail>;

/// The halt an injected crash reports (see `Executor::crash_after`).
#[cfg(feature = "crash-test")]
pub const CRASHED: &str = "injected crash";

/// What a failed copy of a version an upload went over reports.
const THEIRS_NOT_KEPT: &str =
    "uploaded, over an edit made in OneDrive meanwhile; the file's version history in OneDrive has that edit";

/// Names tried for a kept version before giving up.
const NAME_TRIES: usize = 5;

/// What a halt reports for operations of a batch not recorded after it.
const NOT_RUN: &str = "not run: the round stopped";

/// What an operation that was done reports besides being done (see
/// [`Executor::one`]).
#[derive(Default)]
struct Notes {
    /// [`Report::kept_instead`].
    kept_instead: bool,
    /// [`Report::copied`].
    copied: bool,
    /// [`Report::kept_theirs`].
    theirs: Option<String>,
    /// [`Report::theirs_not_kept`].
    theirs_not_kept: Option<String>,
    /// Done, but the round stops after it (the store failed, or the sync
    /// folder its identity check, while keeping such a version).
    halt: Option<String>,
}

/// Whether an operation's outcome halts the round.
fn halts(outcome: &Res, notes: &Notes) -> bool {
    matches!(outcome, Err(Fail::Halt(_))) || notes.halt.is_some()
}

/// Adds one operation's outcome to the report.
fn record(report: &mut Report, op: Op, outcome: Res, notes: Notes) {
    match outcome {
        Ok(()) => {
            if notes.kept_instead {
                report.kept_instead.push(op.clone());
            }
            if notes.copied {
                report.copied.push(op.clone());
            }
            if let Some(name) = notes.theirs {
                report.kept_theirs.push((op.clone(), name));
            }
            if let Some(why) = notes.theirs_not_kept {
                tracing::warn!(error = %why, "an edit an upload went over wasn't kept as a new file");
                report.theirs_not_kept.push((op.clone(), why));
            }
            report.done.push(op);
            if let Some(m) = notes.halt
                && report.halted.is_none()
            {
                tracing::warn!(reason = %m, "round halted");
                report.halted = Some(m);
            }
        }
        Err(Fail::Skip(s)) => {
            tracing::debug!(reason = ?s, "operation skipped");
            report.skipped.push((op, s));
        }
        Err(Fail::Failed(m)) => {
            tracing::warn!(error = %m, "operation failed");
            report.failed.push((op, m));
        }
        Err(Fail::NotNow) => report.not_run.push(op),
        Err(Fail::Halt(m)) => {
            if m != NOT_RUN && report.halted.is_none() {
                tracing::warn!(reason = %m, "round halted");
                report.halted = Some(m);
            }
        }
    }
}

/// How many of the operations at the start of `ops` go to the cloud as one
/// batch, and what they claim together: consecutive cloud metadata changes
/// (create folder, move or rename, delete), at most a batch's worth, none
/// conflicting with another ([`crate::schedule`]): the group ends before an
/// operation that touches an item another one touches, changes a folder
/// another one works in (Graph judges a batch against the state before any
/// of it, so a folder deleted after something left it would still hold
/// that item), or takes or frees a name another one takes or frees. Also
/// before one whose item the store doesn't have. Anything else is a group
/// of its own; below 2 the caller runs them one by one.
fn batch_of(store: &Db<'_>, ops: &[Approved]) -> (usize, Claims) {
    let mut group = Held::default();
    let mut all = Claims::default();
    let mut n = 0;
    for a in ops.iter().take(oneup_graph::client::BATCH_MAX) {
        let meta = matches!(
            a.op(),
            Op::MkdirRemote { .. } | Op::MoveRemote { .. } | Op::RenameRemote { .. } | Op::DeleteRemote { .. }
        );
        let c = claims(store, a.op());
        if n > 0 && (!meta || c.unknown || group.conflicts(&c)) {
            break;
        }
        group.add(&c);
        all.merge(c);
        n += 1;
        if !meta {
            break;
        }
    }
    (n, all)
}

/// The [`Timings`] stage an operation's time is counted in.
fn stage(op: &Op) -> &'static str {
    match op {
        Op::Upload { .. } => "op: upload",
        Op::Download { .. } => "op: download",
        Op::MkdirRemote { .. } | Op::MoveRemote { .. } | Op::RenameRemote { .. } | Op::DeleteRemote { .. } => {
            "op: cloud change"
        }
        Op::PruneRemote { .. } => "op: cloud prune",
        Op::MkdirLocal { .. }
        | Op::MoveLocal { .. }
        | Op::RenameLocal { .. }
        | Op::ConflictCopy { .. }
        | Op::RecycleLocal { .. }
        | Op::Dehydrate { .. } => "op: local change",
        Op::Hash(_) => "op: hash",
        Op::Pair { .. } | Op::Record(_) | Op::Forget(_) | Op::Hold { .. } => "op: bookkeeping",
    }
}

/// The downloads among `ops` whose links may be read ahead
/// ([`Cloud::upcoming`]): those whose cloud file has OneDrive's hash. A
/// link serves whatever the file holds when it's used, and only the hash
/// tells another content of the same size from the planned one; without
/// it, the link is read right before the download, as always.
fn coming_downloads<'a>(store: &Db<'_>, ops: impl IntoIterator<Item = &'a Approved>) -> Vec<RemotePre> {
    ops.into_iter()
        .filter_map(|a| match a.op() {
            Op::Download { remote, .. } => Some(remote),
            _ => None,
        })
        .filter(|r| store.read(|o| o.get_item(&r.id.0)).ok().flatten().is_some_and(|row| row.qxh.is_some()))
        .cloned()
        .collect()
}

/// Whether a full OneDrive refuses `op`: new content and new folders. The
/// agent holds the same while it stays full, in later rounds.
fn needs_room(op: &Op) -> bool {
    matches!(op, Op::Upload { .. } | Op::MkdirRemote { .. })
}

/// Whether an upload may be a copy in OneDrive instead
/// ([`Executor::copy_instead`]), which first reads the whole file if
/// OneDrive has one of its size.
fn may_copy(can_copy: bool, new: bool, size: i64) -> bool {
    can_copy && new && size >= oneup_store::COPY_MIN
}

/// Consecutive operations of the round that run as one: a single
/// operation, or a batch.
struct Unit {
    /// Where in the round they start.
    start: usize,
    /// Shared with the blocking work an operation hands its effects to
    /// ([`Executor::off_task`]).
    group: Vec<Arc<Approved>>,
    claims: Claims,
}

/// The Synced folder `op` linked, if it made one: a new folder on either
/// side, or two new ones paired. Its contents weren't planned with the
/// round: the folder had no Synced row then.
fn made_folder(o: &Ops<'_>, op: &Op) -> oneup_store::Result<Option<Sid>> {
    let b = match op {
        Op::MkdirRemote { local, link: Link::New, .. } | Op::Pair { local, kind: NodeKind::Folder, .. } => {
            o.synced_by_local(local.id.0)?
        }
        Op::MkdirLocal { remote, link: Link::New, .. } => o.synced_by_remote(&remote.id.0)?,
        _ => None,
    };
    Ok(b.filter(|b| b.kind == NodeKind::Folder).map(|b| b.sid))
}

/// Where the store has an item that an operation moves or removes.
struct Place {
    folder: Option<Node>,
    name: String,
    file: bool,
}

/// What `op` touches ([`crate::schedule`]): what the operation names, and
/// for an item it moves, renames or removes, where the store has it now.
/// One whose item can't be read runs alone.
fn claims(store: &Db<'_>, op: &Op) -> Claims {
    store.read(|o| claims_in(o, op)).unwrap_or_else(|e| {
        tracing::debug!(error = %e, "what an operation touches couldn't be read; it runs alone");
        Claims::unknown()
    })
}

/// Reading a file, however large, doesn't make an operation run alone: it
/// reads on a blocking thread ([`crate::blocking`]), so the operations
/// under way go on meanwhile.
fn claims_in(o: &Ops<'_>, op: &Op) -> oneup_store::Result<Claims> {
    use Node::{Local as L, Remote as R, Synced as S};
    let dir = |d: Dir| match d {
        Dir::Local(id) => L(id),
        Dir::Remote(id) => R(id),
    };
    let mut c = Claims::default();
    // The items it acts on.
    let (sid, local, remote) = op.subjects();
    sid.into_iter().for_each(|s| c.item(&S(s)));
    local.into_iter().for_each(|l| c.item(&L(l)));
    remote.into_iter().for_each(|r| c.item(&R(r)));
    // Where it puts something, and the name it takes there.
    if let Some((d, name)) = op.target() {
        let d = dir(d);
        c.within(&d);
        c.name(&d, name);
    }
    match op {
        Op::Pair { parent, .. }
        | Op::MkdirRemote { parent, .. }
        | Op::MkdirLocal { parent, .. }
        | Op::MoveRemote { parent, .. }
        | Op::MoveLocal { parent, .. } => c.within(&S(*parent)),
        Op::Upload { parent, to, .. } => {
            c.within(&S(*parent));
            c.within(&R(to.clone()));
        }
        Op::Download { parent, to, .. } => {
            c.within(&S(*parent));
            c.within(&L(*to));
        }
        Op::Record(b) => b.parent.into_iter().for_each(|p| c.within(&S(p))),
        _ => {}
    }
    // What it moves or removes: the folder it leaves, and the name it frees.
    match op.vacates() {
        Some(Vacates::Local(id)) => {
            let at = o.local_get(id.0)?.map(|r| Place {
                folder: r.parent_id.map(|p| L(LocalId(p))),
                name: r.name,
                file: r.kind == LocalKind::File,
            });
            leaves(&mut c, at);
        }
        Some(Vacates::Remote(id)) => {
            let at = o.get_item(&id.0)?.map(|r| Place {
                folder: r.parent_id.map(|p| R(RemoteId(p))),
                name: r.name,
                file: r.kind == Kind::File,
            });
            leaves(&mut c, at);
        }
        None => {}
    }
    // A Synced row it moves or removes: the folder it leaves.
    if let Some(sid) = sid
        && (op.vacates().is_some() || matches!(op, Op::Forget(_)))
    {
        match o.synced_get(sid)? {
            Some(b) => {
                b.parent.into_iter().for_each(|p| c.within(&S(p)));
                c.alone |= b.kind != NodeKind::File;
            }
            None => {
                c.alone = true;
                c.unknown = true;
            }
        }
    }
    match op {
        // It removes a whole subtree in OneDrive.
        Op::PruneRemote { .. } => c.alone = true,
        Op::Upload { local, .. } => {
            c.large = local.stamp.is_some_and(|(size, _)| usize::try_from(size).is_ok_and(|s| s > SIMPLE_UPLOAD_MAX));
        }
        // Reads a whole file as fast as the disk allows: how many at once
        // has a limit of its own (`Parallel::hashes`).
        Op::Hash(_) => c.hash = true,
        _ => {}
    }
    Ok(c)
}

/// Claims the folder an item leaves and the name it frees there, from where
/// the store has it. Moving or removing a folder reaches everything below
/// it, so that runs alone, as does an item the store doesn't have.
fn leaves(c: &mut Claims, at: Option<Place>) {
    match at {
        Some(at) => {
            if let Some(d) = &at.folder {
                c.within(d);
                c.name(d, &at.name);
            }
            c.alone |= !at.file;
        }
        None => {
            c.alone = true;
            c.unknown = true;
        }
    }
}

/// Skip unless `ok`.
fn need(ok: bool) -> Res {
    if ok { Ok(()) } else { Err(Fail::Skip(Skip::Changed)) }
}

/// Links the Synced tree's root to the sync folder and the drive root, the
/// first time; afterwards checks that they are still the ones linked.
pub fn link_roots(o: &Ops<'_>, local_root: u128, remote_root: &str) -> oneup_store::Result<bool> {
    let want = Base {
        sid: ROOT,
        parent: None,
        name: String::new(),
        kind: NodeKind::Folder,
        size: 0,
        hash: None,
        local: Some(LocalId(local_root)),
        local_mtime: 0,
        remote: Some(RemoteId(remote_root.to_string())),
        remote_etag: String::new(),
    };
    match o.synced_get(ROOT)? {
        None => {
            o.synced_put(&want)?;
            Ok(true)
        }
        Some(b) => Ok(b.local == want.local && b.remote == want.remote),
    }
}

pub struct Executor<C> {
    /// Shared with the blocking work effects run in ([`Executor::off_task`]).
    fs: Arc<LocalFs>,
    cloud: C,
    /// The name-only folder rules the Local tree follows; they can change
    /// between rounds ([`Executor::set_rules`]).
    rules: std::sync::RwLock<ExcludeSet>,
    /// How many operations run at once ([`Executor::set_parallel`]).
    parallel: std::sync::Mutex<Parallel>,
    /// How long each kind of operation took ([`Executor::timed`]).
    timings: std::sync::Arc<Timings>,
    /// Work under way on blocking threads ([`crate::blocking`]).
    jobs: Arc<Jobs>,
    /// See [`Executor::on_blocking`].
    #[cfg(feature = "crash-test")]
    on_blocking: std::sync::Mutex<Option<BlockingHook>>,
}

/// See [`Executor::on_blocking`].
#[cfg(feature = "crash-test")]
pub type BlockingHook = Arc<dyn Fn(&Op) + Send + Sync>;

impl<C: Cloud> Executor<C> {
    /// `rules`: the exclusion rules the Local tree was scanned with, so
    /// rows the executor writes match what the observer would write.
    pub fn new(fs: LocalFs, cloud: C, rules: ExcludeSet) -> Self {
        Self {
            fs: Arc::new(fs),
            cloud,
            rules: std::sync::RwLock::new(rules),
            parallel: std::sync::Mutex::new(Parallel::for_profile("balanced")),
            timings: std::sync::Arc::default(),
            jobs: Arc::default(),
            #[cfg(feature = "crash-test")]
            on_blocking: std::sync::Mutex::default(),
        }
    }

    /// The rules changed: rows written from now on follow these.
    pub fn set_rules(&self, rules: ExcludeSet) {
        *self.rules.write().unwrap_or_else(|p| p.into_inner()) = rules;
    }

    /// Records how long its operations take in `t`.
    pub fn timed(self, t: std::sync::Arc<Timings>) -> Self {
        Self { timings: t, ..self }
    }

    /// How many operations later rounds run at once (the memory profile's,
    /// [`Parallel::for_profile`]; `balanced` until set).
    pub fn set_parallel(&self, p: Parallel) {
        *self.parallel.lock().unwrap_or_else(|p| p.into_inner()) = p;
    }

    /// Test-only: the round stops at the crash point after `n` others have
    /// passed, as if the process had been killed there, and reports
    /// [`CRASHED`] as its halt. Crash points are right after each effect
    /// (before its record), right after each write-ahead record (before its
    /// effect), and between the steps of effects that take several (one
    /// count, kept by the effects layer: `LocalFs::crash_after`). `None`
    /// turns it off.
    #[cfg(feature = "crash-test")]
    pub fn crash_after(&self, n: Option<u32>) {
        self.fs.crash_after(n);
    }

    /// Test-only: every effect run on a blocking thread
    /// ([`Executor::off_task`]) first calls `f` with its operation, on that
    /// thread, so a test can hold it there as a slow disk would. `None`
    /// turns it off.
    #[cfg(feature = "crash-test")]
    pub fn on_blocking(&self, f: Option<BlockingHook>) {
        *self.on_blocking.lock().unwrap_or_else(std::sync::PoisonError::into_inner) = f;
    }

    /// Runs effect `f` for `a` on a blocking thread ([`crate::blocking`]):
    /// those that may read a whole file (hashing it, checking an old copy
    /// before it's dropped or replaced, checking content before it's
    /// freed), so the operations under way don't wait for the disk. It runs
    /// to its end even if the round stops meanwhile, and the round waits
    /// for it ([`Jobs::settled`]). A panic in it goes on as if it had
    /// happened here.
    async fn off_task<T: Send + 'static>(
        &self,
        a: &Arc<Approved>,
        f: impl FnOnce(&LocalFs, &Approved) -> T + Send + 'static,
    ) -> Res<T> {
        let (fs, a) = (Arc::clone(&self.fs), Arc::clone(a));
        #[cfg(feature = "crash-test")]
        let hook = self.on_blocking.lock().unwrap_or_else(std::sync::PoisonError::into_inner).clone();
        // An armed crash runs it here instead, unless a test holds it on its
        // thread: the effect's own crash points (`LocalFs::crash_after`) then
        // count in the same order every run, between the round's others, so
        // crash point n is always the same place.
        #[cfg(feature = "crash-test")]
        if self.fs.crash_armed() && hook.is_none() {
            return Ok(f(&fs, &a));
        }
        let job = self.jobs.run(move || {
            #[cfg(feature = "crash-test")]
            if let Some(hook) = hook {
                hook(a.op());
            }
            f(&fs, &a)
        });
        match job.await {
            Ok(out) => Ok(out),
            Err(e) => match e.try_into_panic() {
                Ok(panic) => std::panic::resume_unwind(panic),
                // Only while the runtime shuts down.
                Err(_) => Err(Fail::Halt("a file operation didn't run: shutting down".into())),
            },
        }
    }

    /// A place where a killed process would leave the effects layer and the
    /// store out of step. Nothing runs after an injected crash: no record,
    /// no withdrawal.
    fn crash_point(&self) -> Res {
        #[cfg(feature = "crash-test")]
        if self.fs.crash_now() {
            return Err(Fail::Halt(CRASHED.into()));
        }
        Ok(())
    }

    pub fn local(&self) -> &LocalFs {
        &self.fs
    }

    /// After an upload replaced file `id` in OneDrive folder `to` (Synced
    /// folder `parent`): if it went over a version other than `mark`'s (an
    /// edit that reached OneDrive while the upload committed), keeps that
    /// version as a new file next to it, named as conflict copies are, and
    /// returns its name. Only a free name is taken; nothing is replaced. The
    /// next round brings it here like any new file in OneDrive.
    async fn keep_replaced(
        &self,
        store: &Db<'_>,
        id: &RemoteId,
        parent: Sid,
        to: &RemoteId,
        name: &str,
        mark: &VersionMark,
    ) -> Res<Option<String>> {
        let theirs = match self.cloud.replaced_other(id, mark).await {
            Ok(Some(v)) => v,
            Ok(None) => return Ok(None),
            // The upload is done; an edit it may have gone over is in the
            // file's version history either way.
            Err(e) => {
                tracing::warn!(error = %e, "an upload's replaced version couldn't be checked");
                return Ok(None);
            }
        };
        let lost = |e: String| Fail::Failed(format!("{THEIRS_NOT_KEPT}: {e}"));
        let mut staged = self.fs.stage()?;
        let Some(file) = staged.file() else { return Err(odd()) };
        let len = self.cloud.download_version(id, &theirs.id, &mut *file).await.map_err(|e| lost(e.to_string()))?;
        if theirs.size.is_some_and(|s| u64::try_from(s).ok() != Some(len)) {
            return Err(lost("its content didn't match its size".into()));
        }
        let size = i64::try_from(len).map_err(|e| lost(e.to_string()))?;
        // Free on both sides, so the copy comes here under the same name.
        // Each name is looked up on its own: listing the folder would load
        // every item in it, and a folder can hold millions.
        let local = store.read(|o| Ok(o.synced_get(parent)?.and_then(|b| b.local)))?;
        let free = |o: &Ops<'_>, copy: &str| -> oneup_store::Result<bool> {
            if !o.remote_named(&to.0, copy)?.is_empty() {
                return Ok(false);
            }
            Ok(match local {
                Some(l) => o.local_named(l.0, copy)?.is_empty(),
                None => true,
            })
        };
        let mut n = 0;
        let mut tries = 0;
        // A name OneDrive has that the Remote tree doesn't yet: another.
        while tries < NAME_TRIES {
            n += 1;
            let copy = conflict_candidate(name, NodeKind::File, n);
            if !store.read(|o| free(o, &copy))? {
                continue;
            }
            tries += 1;
            file.rewind().map_err(|e| lost(e.to_string()))?;
            // Read off the task, through a handle of its own (sharing the
            // position just rewound).
            let mut reader = OffTask::new(file.try_clone().map_err(|e| lost(e.to_string()))?, &self.jobs);
            match self.cloud.upload(to, &copy, None, size, &mut reader).await {
                Ok((row, sent)) => {
                    let sent = sent.unwrap_or_else(|| reader.hash.finalize());
                    let read = reader.hash.len();
                    store.write(|o| Ok::<_, Fail>(o.upsert_item(&row, false)?)).await?;
                    if row.qxh != Some(sent) || read != len || row.size != size {
                        return Err(lost("OneDrive's copy didn't match what was sent".into()));
                    }
                    tracing::info!("a file changed in OneDrive while its upload committed; that version was kept");
                    return Ok(Some(copy));
                }
                Err(CloudError::NameTaken) => continue,
                Err(e) => return Err(lost(e.to_string())),
            }
        }
        Err(lost("no free name".into()))
    }

    /// For a new file (`new`) of at least `COPY_MIN` bytes, `size` bytes
    /// long: if OneDrive already has a file with the same size and content,
    /// has OneDrive copy it to `to/name` instead of uploading (server-side
    /// copy). Reads the open file to hash it (off the task, like the
    /// upload's reads) only when a cloud file of exactly its size exists,
    /// and leaves it rewound for an upload otherwise. `None`: upload it.
    /// The copy is verified: one whose content turned out different (the
    /// source changed in between) is removed again, into OneDrive's recycle
    /// bin, before uploading instead.
    async fn copy_instead(
        &self,
        store: &Db<'_>,
        src: &mut OffTask<oneup_effects::UploadSource>,
        size: i64,
        to: &RemoteId,
        name: &str,
        new: bool,
    ) -> Res<Option<(RemoteRow, Hash)>> {
        if !may_copy(self.cloud.can_copy(), new, size) {
            return Ok(None);
        }
        let candidates = store.read(|o| o.remote_files_of_size(size, 16))?;
        if candidates.is_empty() {
            return Ok(None);
        }
        let read = src
            .with(|file| {
                let mut buf = vec![0u8; 1 << 20];
                let h = oneup_hash::hash_reader(&mut *file, &mut buf)?;
                file.rewind()?;
                Ok::<_, std::io::Error>(h)
            })
            .await;
        let h = match read.and_then(|r| r) {
            Ok(h) => h,
            Err(e) if oneup_effects::ChangedMeanwhile::is(&e) => return Err(Fail::Skip(Skip::Changed)),
            Err(e) => return Err(Fail::Failed(e.to_string())),
        };
        let hash = h.finalize();
        if i64::try_from(h.len()).ok() != Some(size) {
            return Err(Fail::Skip(Skip::Changed));
        }
        let Some(source) = candidates.into_iter().find(|c| c.qxh == Some(hash)) else { return Ok(None) };
        let pre = RemotePre { id: RemoteId(source.id.clone()), etag: source.etag.clone().unwrap_or_default() };
        match self.cloud.copy(&pre, &hash, to, name).await {
            Ok(row) if row.qxh == Some(hash) && row.size == size => Ok(Some((row, hash))),
            Ok(row) => {
                // Not the content we have: remove what was just made.
                tracing::warn!("a copy made in OneDrive had other content than this file; removing it and uploading");
                let made = RemotePre { id: RemoteId(row.id.clone()), etag: row.etag.clone().unwrap_or_default() };
                self.cloud.delete(&made).await?;
                Ok(None)
            }
            Err(CloudError::NameTaken) => Err(Fail::Skip(Skip::NameTaken)),
            Err(e) => {
                tracing::debug!(error = %e, "copying in OneDrive didn't work; uploading instead");
                Ok(None)
            }
        }
    }

    pub fn cloud(&self) -> &C {
        &self.cloud
    }

    /// Runs `approved`: in the planner's order wherever one operation
    /// depends on another, side by side otherwise ([`crate::schedule`]).
    /// Stops at the first halt.
    pub async fn run(&self, store: &mut Store, approved: Vec<Approved>) -> Report {
        self.run_counted(store, approved, &|_| {}).await
    }

    /// [`Self::run`], calling `done(n)` each time `n` more operations have
    /// finished, however they ended (for progress).
    pub async fn run_counted(
        &self,
        store: &mut Store,
        approved: Vec<Approved>,
        done: &(dyn Fn(usize) + Sync),
    ) -> Report {
        self.run_checked(store, approved, done, &|_| true).await
    }

    /// [`Self::run_counted`], asking `may_run` as each operation starts
    /// whether it may run now (a pause that came meanwhile, say). One it
    /// refuses isn't started ([`Report::not_run`]). A download under way is
    /// asked again as its data arrives, and stopped once refused: nothing
    /// was placed yet, so it's simply done again later. Anything else that
    /// started runs to its end (an upload commits, a batch is answered).
    /// `may_run` is called often: it must be cheap.
    ///
    /// Operations finish in any order; the report lists them in the
    /// planner's order all the same, which callers rely on. A halt stops
    /// the round at once: operations still under way are dropped where
    /// they wait, as a crash would leave them, and aren't reported. Work
    /// they handed to a blocking thread runs to its end first
    /// ([`crate::blocking`]).
    pub async fn run_checked(
        &self,
        store: &mut Store,
        approved: Vec<Approved>,
        done: &(dyn Fn(usize) + Sync),
        may_run: &(dyn Fn(&Op) -> bool + Sync),
    ) -> Report {
        self.run_following(store, approved, done, may_run, &|_, _, _| Vec::new(), 0).await
    }

    /// [`Self::run_checked`], taking in what goes into the folders the
    /// round makes as it goes: once an operation that made a folder is
    /// recorded, `follow` plans and reviews the folder's contents, and they
    /// run after everything taken in before them, as if planned last. A new
    /// tree then goes in one round instead of one level a round, without a
    /// round's start (the change journal, the cloud listing) for each level,
    /// and a level's folders are made while the level above is still being
    /// sent. At most `follow_max` ([`FOLLOW_MAX`]) operations are taken in this way; the
    /// rest wait for the next round, which the new folders' marks bring, as
    /// does whatever `follow` leaves out.
    pub async fn run_following(
        &self,
        store: &mut Store,
        approved: Vec<Approved>,
        done: &(dyn Fn(usize) + Sync),
        may_run: &(dyn Fn(&Op) -> bool + Sync),
        follow: &Follow<'_>,
        follow_max: usize,
    ) -> Report {
        let mut report = Report::default();
        if let Err(e) = self.fs.check_root() {
            report.halted = Some(e.to_string());
            return report;
        }
        // However the round ends, nothing it started is still running.
        let _settle = Settle(&self.jobs);
        let db = Db::new(store);
        // Records left by a round that was cut short.
        if let Err(Fail::Halt(m) | Fail::Failed(m)) = self.recover_all(&db).await {
            report.halted = Some(m);
            return report;
        }
        let limits = self.parallel();
        // Once OneDrive says it's full (to one operation or in a batch),
        // the round's other uploads and new folders aren't sent only to be
        // refused; everything else goes on.
        let full = AtomicBool::new(false);
        // Not taken in yet: the round's operations from the `next`th on.
        let mut pending: VecDeque<Approved> = approved.into();
        let mut followed = 0;
        let mut waiting: VecDeque<Unit> = VecDeque::new();
        let mut held = Held::default();
        let mut running = FuturesUnordered::new();
        // Outcomes from the first not yet reported on: they're reported in
        // the planner's order, as soon as everything before has finished.
        let mut reported = 0;
        let mut outcomes: VecDeque<Option<(Op, Res, Notes)>> = VecDeque::new();
        let mut next = 0;
        loop {
            // A run of independent cloud metadata changes goes in one batch;
            // each keeps its own precondition and outcome.
            let first_new = waiting.len();
            while next - reported < WINDOW && !pending.is_empty() {
                let (n, claims) = batch_of(&db, pending.make_contiguous());
                waiting.push_back(Unit { start: next, group: pending.drain(..n).map(Arc::new).collect(), claims });
                next += n;
            }
            // On demand, a download is a placeholder: nothing is fetched.
            if waiting.len() > first_new && !self.fs.on_demand() {
                let coming = coming_downloads(&db, waiting.range(first_new..).flat_map(|u| &u.group).map(|a| &**a));
                if !coming.is_empty() {
                    self.cloud.upcoming(&coming);
                }
            }
            // Each in the planner's order, once nothing under way and
            // nothing before it still waiting conflicts with it.
            let mut earlier = Held::default();
            let mut k = 0;
            while k < waiting.len() && held.units < limits.ops && held.alone == 0 {
                let c = &waiting[k].claims;
                let ready = if c.alone {
                    held.units == 0 && k == 0
                } else {
                    !held.conflicts(c) && !earlier.conflicts(c) && held.has_room(c, limits)
                };
                if !ready {
                    if c.alone {
                        // Everything after it waits for it.
                        break;
                    }
                    earlier.add(c);
                    k += 1;
                    continue;
                }
                let Some(u) = waiting.remove(k) else { break };
                held.add(&u.claims);
                running.push(self.unit(&db, u, &full, may_run));
            }
            let Some((u, finished)) = running.next().await else { break };
            held.remove(&u.claims);
            let len = u.group.len();
            done(len);
            let halted = finished.iter().any(|(r, notes)| halts(r, notes));
            if !halted && followed < follow_max {
                let mut more = self.follow(&db, &u.group, &finished, follow, follow_max - followed);
                more.truncate(follow_max - followed);
                followed += more.len();
                pending.extend(more);
            }
            let from = u.start - reported;
            if outcomes.len() < from + len {
                outcomes.resize_with(from + len, || None);
            }
            for (k, (a, (r, notes))) in u.group.into_iter().zip(finished).enumerate() {
                outcomes[from + k] = Some((a.op().clone(), r, notes));
            }
            if halted {
                break;
            }
            while let Some(Some(_)) = outcomes.front() {
                if let Some(Some((op, outcome, notes))) = outcomes.pop_front() {
                    record(&mut report, op, outcome, notes);
                }
                reported += 1;
            }
        }
        drop(running);
        // What the dropped operations handed to blocking threads ends
        // before the round does.
        self.jobs.settled().await;
        // After a halt: what finished, still in the planner's order.
        for (op, outcome, notes) in outcomes.into_iter().flatten() {
            record(&mut report, op, outcome, notes);
        }
        report
    }

    /// What goes into the folders `group` made ([`Self::run_following`]),
    /// read from the store once they're recorded. A store that fails here
    /// leaves them to the next round.
    fn follow(
        &self,
        store: &Db<'_>,
        group: &[Arc<Approved>],
        finished: &[(Res, Notes)],
        follow: &Follow<'_>,
        room: usize,
    ) -> Vec<Approved> {
        let made = store.read(|o| {
            let mut made = Vec::new();
            for (a, _) in group.iter().zip(finished).filter(|(_, (r, _))| r.is_ok()) {
                made.extend(made_folder(o, a.op())?);
            }
            Ok(if made.is_empty() { Vec::new() } else { follow(o, &made, room) })
        });
        made.unwrap_or_else(|e| {
            tracing::debug!(error = %e, "what goes into a new folder is left to the next round");
            Vec::new()
        })
    }

    /// The limits this round runs under: one at a time for a cloud that
    /// can't take more ([`Cloud::concurrent`]).
    fn parallel(&self) -> Parallel {
        if !self.cloud.concurrent() {
            return Parallel::ONE;
        }
        self.parallel.lock().unwrap_or_else(std::sync::PoisonError::into_inner).at_least_one()
    }

    /// Runs one operation, or a batch. Whether each may run is asked as it
    /// starts (a pause, a full OneDrive): one refused then isn't sent, and
    /// the batch goes without it.
    async fn unit(
        &self,
        store: &Db<'_>,
        u: Unit,
        full: &AtomicBool,
        may_run: &(dyn Fn(&Op) -> bool + Sync),
    ) -> (Unit, Vec<(Res, Notes)>) {
        let group = &u.group;
        let mut out: Vec<Option<(Res, Notes)>> = std::iter::repeat_with(|| None).take(group.len()).collect();
        let mut go = Vec::new();
        for (k, a) in group.iter().enumerate() {
            if !may_run(a.op()) {
                out[k] = Some((Err(Fail::NotNow), Notes::default()));
            } else if full.load(Relaxed) && needs_room(a.op()) {
                out[k] = Some((Err(Fail::Skip(Skip::CloudFull)), Notes::default()));
            } else {
                go.push(k);
            }
        }
        match go.as_slice() {
            [] => {}
            &[k] => out[k] = Some(self.single(store, &group[k], may_run).await),
            _ => {
                tracing::debug!(n = go.len(), "cloud changes sent in one batch");
                let members: Vec<&Approved> = go.iter().map(|&k| &*group[k]).collect();
                let started = std::time::Instant::now();
                for (&k, r) in go.iter().zip(self.run_batch(store, &members).await) {
                    out[k] = Some((r, Notes::default()));
                }
                self.timings.record("op: batch of cloud changes", started);
            }
        }
        if out.iter().flatten().any(|(r, _)| matches!(r, Err(Fail::Skip(Skip::CloudFull)))) {
            full.store(true, Relaxed);
        }
        let out = out.into_iter().map(|o| o.unwrap_or_else(|| (Err(Fail::Halt(NOT_RUN.into())), Notes::default())));
        (u, out.collect())
    }

    /// One operation on its own, then the settling of the write-ahead
    /// record it left if it didn't finish: right after it, while it still
    /// holds its claims, so nothing that touches its item or the name it
    /// frees or takes, or changes a folder it works in, runs first. Others
    /// may go on while it waits for the store: the settling reads only this
    /// item.
    async fn single(&self, store: &Db<'_>, a: &Arc<Approved>, may_run: &(dyn Fn(&Op) -> bool + Sync)) -> (Res, Notes) {
        let mut notes = Notes::default();
        let started = std::time::Instant::now();
        let mut outcome = self.one(store, a, &mut notes, may_run).await;
        self.timings.record(stage(a.op()), started);
        if let Some(key) = pending_key(a.op())
            && !matches!(outcome, Err(Fail::Halt(_)))
            && let Err(e) = self.recover(store, &key).await
        {
            outcome = Err(e);
        }
        (outcome, notes)
    }

    /// Runs a group from [`batch_of`]: every precondition in the store,
    /// then one cloud batch, then each record in order. After a halt (an
    /// injected crash) nothing more is recorded, as after a real crash: the
    /// cloud changes stay unrecorded and the next round finds them.
    async fn run_batch(&self, store: &Db<'_>, group: &[&Approved]) -> Vec<Res> {
        let mut outcomes: Vec<Option<Res>> = std::iter::repeat_with(|| None).take(group.len()).collect();
        let mut metas = Vec::new();
        let mut sent = Vec::new();
        for (k, a) in group.iter().enumerate() {
            match self.meta_pre(store, a.op()).await {
                Ok(m) => {
                    metas.push(m);
                    sent.push(k);
                }
                Err(e) => outcomes[k] = Some(Err(e)),
            }
        }
        let results = self.cloud.batch(&metas).await;
        let mut halted = false;
        for ((k, r), m) in sent.into_iter().zip(results).zip(&metas) {
            if halted {
                break;
            }
            let res = match r {
                Err(CloudError::TooManyItems) => self.move_in_parts(store, group[k].op(), m).await,
                Err(e) => Err(e.into()),
                Ok(done) => match self.crash_point() {
                    Ok(()) => self.meta_post(store, group[k].op(), done).await,
                    Err(e) => Err(e),
                },
            };
            halted = matches!(res, Err(Fail::Halt(_)));
            outcomes[k] = Some(res);
        }
        // Not reached after a halt: reported as not run.
        outcomes.into_iter().map(|o| o.unwrap_or_else(|| Err(Fail::Halt(NOT_RUN.into())))).collect()
    }

    /// The store-side precondition of a cloud metadata change, and what to send.
    async fn meta_pre(&self, store: &Db<'_>, op: &Op) -> Res<Meta> {
        match op {
            Op::MkdirRemote { local, to, parent, name, link } => {
                store
                    .write(|o| need(local_ok(o, local)? && link_ok(o, *link)? && o.synced_get(*parent)?.is_some()))
                    .await?;
                Ok(Meta::Mkdir { parent: to.clone(), name: name.clone() })
            }
            Op::MoveRemote { sid, remote, to, name, .. } => {
                store.write(|o| need(o.synced_get(*sid)?.is_some())).await?;
                Ok(Meta::Move { pre: remote.clone(), to: to.clone(), name: name.clone() })
            }
            Op::DeleteRemote { sid, remote } => {
                store.write(|o| need(o.synced_get(*sid)?.is_some())).await?;
                Ok(Meta::Delete { pre: remote.clone() })
            }
            // The eTag (`If-Match`) is the whole precondition: the item isn't
            // in the Synced tree.
            Op::RenameRemote { remote, dir, name } => {
                Ok(Meta::Move { pre: remote.clone(), to: dir.clone(), name: name.clone() })
            }
            _ => Err(odd()),
        }
    }

    /// Records a cloud metadata change that was made.
    async fn meta_post(&self, store: &Db<'_>, op: &Op, done: MetaDone) -> Res {
        match (op, done) {
            (Op::MkdirRemote { local, parent, name, link, .. }, MetaDone::Item(row)) => {
                store
                    .write(|o| {
                        o.upsert_item(&row, false)?;
                        link_row(o, *link, folder_base(*parent, name, local.id, &row))
                    })
                    .await?;
                if self.fs.on_demand()
                    && let Err(e) = self.fs.convert_folder(local.id, &folder_version(&row))
                {
                    tracing::debug!(error = %e, "a new folder wasn't made a placeholder");
                }
                Ok(())
            }
            (Op::MoveRemote { sid, remote, parent, name, .. }, MetaDone::Item(row)) => {
                // A move keeps the item's ID in OneDrive. In a vault, an
                // entry whose stored name changes shape is rebuilt, so the
                // decrypted item gets a new ID: the Synced row follows it,
                // and so does an online-only file's identity.
                let renewed = row.id != remote.id.0;
                let local = store
                    .write(|o| {
                        o.upsert_item(&row, false)?;
                        let Some(mut b) = o.synced_get(*sid)? else { return Err(Fail::Skip(Skip::Changed)) };
                        b.parent = Some(*parent);
                        b.name.clone_from(name);
                        b.remote_etag = row.etag.clone().unwrap_or_default();
                        if renewed {
                            b.remote = Some(RemoteId(row.id.clone()));
                        }
                        o.synced_put(&b)?;
                        let local = match b.local {
                            Some(l) if renewed => o.local_get(l.0)?,
                            _ => None,
                        };
                        Ok::<_, Fail>(local)
                    })
                    .await?;
                if let Some(l) = local.filter(|l| l.kind == oneup_store::LocalKind::File)
                    && self.fs.on_demand()
                    && let Some(mut v) = version(&row)
                {
                    // Its own time stays: it wasn't changed here.
                    v.modified = l.mtime;
                    let pre = LocalPre { id: LocalId(l.file_id), stamp: Some((l.size, l.mtime)) };
                    if let Err(e) = self.fs.renew_identity(&pre, &v) {
                        tracing::debug!(error = %e, "a moved file's placeholder identity wasn't renewed");
                    }
                }
                Ok(())
            }
            (Op::RenameRemote { .. }, MetaDone::Item(row)) => store.write(|o| Ok(o.upsert_item(&row, false)?)).await,
            (Op::DeleteRemote { sid, remote }, MetaDone::Deleted) => {
                store
                    .write(|o| {
                        o.delete_subtree(&remote.id.0)?;
                        o.synced_remove(*sid)?;
                        Ok(())
                    })
                    .await
            }
            _ => Err(odd()),
        }
    }

    /// One cloud metadata change on its own: the same three steps.
    async fn meta_one(&self, store: &Db<'_>, op: &Op) -> Res {
        let m = self.meta_pre(store, op).await?;
        let done = match &m {
            Meta::Mkdir { parent, name } => MetaDone::Item(self.cloud.mkdir(parent, name).await?),
            Meta::Move { pre, to, name } => match self.cloud.move_item(pre, to, name).await {
                Err(CloudError::TooManyItems) => return self.move_in_parts(store, op, &m).await,
                r => MetaDone::Item(r?),
            },
            Meta::Delete { pre } => {
                self.cloud.delete(pre).await?;
                MetaDone::Deleted
            }
        };
        self.crash_point()?;
        self.meta_post(store, op, done).await
    }

    /// A move OneDrive refused because the folder has too many items below
    /// it ([`CloudError::TooManyItems`]), done in parts ([`Self::rebuild`])
    /// and then recorded as the move it stands for.
    async fn move_in_parts(&self, store: &Db<'_>, op: &Op, m: &Meta) -> Res {
        let Meta::Move { pre, to, name } = m else { return Err(CloudError::TooManyItems.into()) };
        tracing::info!("OneDrive won't move a folder this large in one request; moving its contents item by item");
        let row = self.rebuild(store, pre, to, name).await?;
        self.meta_post(store, op, MetaDone::Item(row)).await
    }

    /// Moves folder `pre` to `to/name` without moving it as a whole, which
    /// OneDrive refuses above its list view threshold: makes a new folder
    /// there, moves the contents into it a batch at a time (a subfolder that
    /// is itself too large the same way), then removes the emptied original
    /// into OneDrive's recycle bin. Returns the new folder, which stands for
    /// the old one from then on: a new ID, everything inside it unchanged.
    ///
    /// Nothing is deleted but a folder OneDrive confirms empty, on its
    /// current eTag. Each step is recorded as it happens, so the store
    /// always matches the cloud: the Synced row follows the new folder as
    /// soon as it exists, and an interruption leaves what wasn't moved yet
    /// in the original folder, which the next round sees as a folder of its
    /// own (the user's move, done in part).
    async fn rebuild(&self, store: &Db<'_>, pre: &RemotePre, to: &RemoteId, name: &str) -> Res<RemoteRow> {
        let now = self.cloud.item(&pre.id).await?.ok_or(Fail::Skip(Skip::Changed))?;
        need(now.etag.as_deref() == Some(pre.etag.as_str()))?;
        if now.kind != Kind::Folder {
            return Err(CloudError::TooManyItems.into());
        }
        // OneDrive compares names ignoring case, so the new folder can't be
        // made next to the original under the same name in other case. A
        // temporary name doesn't help: renaming the rebuilt folder to the
        // right one is refused for the same reason the move was.
        if now.parent_id.as_deref() == Some(to.0.as_str()) && fold(&now.name) == fold(name) {
            return Err(Fail::Failed(
                "OneDrive won't change only the case of a name on a folder this large: \
                 rename it to another name first, then to the one you want"
                    .into(),
            ));
        }
        let mut page = self.cloud.children(&pre.id).await?;
        let made = self.cloud.mkdir(to, name).await?;
        self.crash_point()?;
        store.write(|o| record_moved(o, &pre.id.0, &made)).await?;
        let into = RemoteId(made.id.clone());
        while !page.is_empty() {
            let mut moved = 0usize;
            for chunk in page.chunks(oneup_graph::client::BATCH_MAX) {
                let metas: Vec<Meta> = chunk
                    .iter()
                    .map(|c| Meta::Move { pre: pre_of(c), to: into.clone(), name: c.name.clone() })
                    .collect();
                let results = self.cloud.batch(&metas).await;
                self.crash_point()?;
                for (c, r) in chunk.iter().zip(results) {
                    let row = match r {
                        Ok(MetaDone::Item(row)) => row,
                        Err(CloudError::TooManyItems) => {
                            Box::pin(self.rebuild(store, &pre_of(c), &into, &c.name)).await?
                        }
                        // Already moved: a listing that still showed it, or
                        // a move OneDrive made whose answer was lost, sent
                        // again on the eTag it had before. Recorded all the
                        // same: left stored below the original, it would be
                        // removed with it.
                        Err(CloudError::Changed) => match self.cloud.item(&RemoteId(c.id.clone())).await? {
                            Some(row) if row.parent_id.as_deref() == Some(made.id.as_str()) => row,
                            _ => return Err(CloudError::Changed.into()),
                        },
                        Err(e) => return Err(e.into()),
                        Ok(MetaDone::Deleted) => return Err(odd()),
                    };
                    store.write(|o| record_moved(o, &c.id, &row)).await?;
                    moved += 1;
                }
            }
            if moved == 0 {
                return Err(Fail::Failed("OneDrive still lists items already moved out of a folder".into()));
            }
            page = self.cloud.children(&pre.id).await?;
        }
        // Its eTag moved on with every item taken out.
        let emptied = self.cloud.item(&pre.id).await?.ok_or(Fail::Skip(Skip::Changed))?;
        // The moves recorded above must outlive the original's removal: a
        // power cut rolling them back would leave the Synced row on the
        // original, whose deletion the next listing then reports as one to
        // bring to this PC. A durable commit flushes every commit before it.
        store.write_ahead(|o| Ok::<_, Fail>(o.require_durable()?)).await?;
        self.cloud.delete(&pre_of(&emptied)).await?;
        self.crash_point()?;
        // Nothing points at it any more: the Synced row follows the new
        // folder, and its contents were recorded as moved. Anything still
        // stored below it went elsewhere in OneDrive meanwhile (moved on the
        // web, say), not away, so its rows go without deletion evidence
        // (invariant 3): it reads as missing until the next listing brings
        // it where it is now. Unpruning drops the tombstones pruning leaves.
        store
            .write(|o| {
                o.prune_subtree(&pre.id.0, true)?;
                o.unprune_subtree(&pre.id.0)?;
                Ok::<_, Fail>(())
            })
            .await?;
        self.cloud.item(&into).await?.ok_or(Fail::Skip(Skip::Changed))
    }

    /// Runs one operation, noting in `notes` what else a done one reports
    /// (see [`Notes`]). `may_run`: see [`Self::run_checked`].
    async fn one(
        &self,
        store: &Db<'_>,
        a: &Arc<Approved>,
        notes: &mut Notes,
        may_run: &(dyn Fn(&Op) -> bool + Sync),
    ) -> Res {
        match a.op() {
            // ------------------------------------------------ bookkeeping
            Op::Pair { local, remote, parent, name, kind, size, hash } => {
                let row = store
                    .write(|o| {
                        need(local_ok(o, local)? && remote_ok(o, remote)? && o.synced_get(*parent)?.is_some())?;
                        o.synced_insert(&Base {
                            sid: 0,
                            parent: Some(*parent),
                            name: name.clone(),
                            kind: *kind,
                            size: *size,
                            hash: *hash,
                            local: Some(local.id),
                            local_mtime: local.stamp.map_or(0, |s| s.1),
                            remote: Some(remote.id.clone()),
                            remote_etag: remote.etag.clone(),
                        })?;
                        Ok::<_, Fail>(o.get_item(&remote.id.0)?)
                    })
                    .await?;
                // On demand: what was already here, the same as OneDrive's,
                // becomes a placeholder in sync, as an upload or a download
                // leaves it (else Explorer shows it pending forever, and it
                // can't be freed: a first sync once left 230k such files).
                // Best effort, like after an upload: the
                // exclusive handle refuses a file open elsewhere.
                if self.fs.on_demand()
                    && let Some(row) = row
                {
                    let converted = match kind {
                        NodeKind::Folder => self.fs.convert_folder(local.id, &folder_version(&row)),
                        _ => match (version(&row), local.stamp) {
                            (Some(mut v), Some((_, mtime))) if row.qxh == *hash => {
                                v.modified = mtime;
                                self.fs.convert_in_sync(local, &v)
                            }
                            _ => Ok(()),
                        },
                    };
                    if let Err(e) = converted {
                        tracing::debug!(error = %e, "an item already here wasn't made a placeholder");
                    }
                }
                Ok(())
            }
            Op::Record(b) => {
                store
                    .write(|o| {
                        need(o.synced_get(b.sid)?.is_some())?;
                        o.synced_put(b)?;
                        Ok(())
                    })
                    .await
            }
            Op::Forget(sid) => {
                store
                    .write(|o| {
                        o.synced_remove(*sid)?;
                        Ok(())
                    })
                    .await
            }
            Op::Hash(pre) => {
                let hashed = self.off_task(a, |fs, a| fs.apply(a)).await??;
                let Done::Hashed { hash, size, mtime } = hashed else { return Err(odd()) };
                self.crash_point()?;
                store.write(|o| Ok(o.local_hash_put(pre.id.0, size, mtime, &hash)?)).await
            }

            // ------------------------------------------------ transfers
            Op::Upload { local, to, parent, name, replace, link } => {
                store.write(|o| need(link_ok(o, *link)? && o.synced_get(*parent)?.is_some())).await?;
                // Opened here (an open and an oplock request: short); its
                // reads run on blocking threads.
                let opened = self.fs.open_upload(a)?;
                let (size, mtime) = (opened.size, opened.mtime);
                let mut src = OffTask::new(opened, &self.jobs);
                let (row, sent, sent_len, mark) = match self
                    .copy_instead(store, &mut src, size, to, name, replace.is_none())
                    .await?
                {
                    Some((row, hash)) => {
                        notes.copied = true;
                        (row, hash, u64::try_from(size).unwrap_or(0), None)
                    }
                    None => {
                        // What the replacement goes over, to check afterwards.
                        let mark = match replace {
                            Some(pre) => match self.cloud.version_mark(pre, size).await {
                                Ok(mark) => mark,
                                Err(CloudError::Changed) => return Err(Fail::Skip(Skip::Changed)),
                                // Not a reason to hold the upload: the eTag is
                                // still checked before it commits.
                                Err(e) => {
                                    tracing::warn!(error = %e, "a file's version history couldn't be read before its upload");
                                    None
                                }
                            },
                            None => None,
                        };
                        let uploaded = self.cloud.upload(to, name, replace.as_ref(), size, &mut src).await;
                        let (row, sent) = match uploaded {
                            Err(CloudError::Io(e)) if oneup_effects::ChangedMeanwhile::is(&e) => {
                                return Err(Fail::Skip(Skip::Changed));
                            }
                            r => r?,
                        };
                        // In the cloud's terms (see `cloud`): what it sent, if it transformed it.
                        (row, sent.unwrap_or_else(|| src.hash.finalize()), src.hash.len(), mark)
                    }
                };
                self.crash_point()?;
                // Written after its last read: what was sent is one version,
                // but the stamp no longer vouches for what's here, so the
                // next round hashes the file again (a same-size write that
                // kept the time would otherwise look synced).
                // A read lost with its thread counts as written: nothing
                // vouches for the file.
                let written = !src.get().is_some_and(oneup_effects::UploadSource::unchanged);
                drop(src);
                let local_mtime = if written { STAMP_UNKNOWN } else { mtime };
                store.write(|o| Ok::<_, Fail>(o.upsert_item(&row, false)?)).await?;
                if row.qxh != Some(sent) || i64::try_from(sent_len).ok() != Some(size) || row.size != size {
                    return Err(Fail::Skip(Skip::ContentMismatch));
                }
                store
                    .write(|o| {
                        if !written {
                            o.local_hash_put(local.id.0, size, mtime, &sent)?;
                        }
                        let b = file_base(*parent, name, size, sent, local.id, local_mtime, &row);
                        link_row(o, *link, b)
                    })
                    .await?;
                // On demand: the file now is (or stays) a placeholder in sync
                // with what was sent. Best effort: if it changed meanwhile,
                // the next round sends it again.
                if self.fs.on_demand()
                    && !written
                    && let Some(mut v) = version(&row)
                {
                    v.modified = mtime;
                    let pre = LocalPre { id: local.id, stamp: Some((size, mtime)) };
                    if let Err(e) = self.fs.mark_uploaded(&pre, &v) {
                        tracing::debug!(error = %e, "an uploaded file wasn't marked in sync");
                    }
                }
                // The upload is done and recorded from here on, whatever
                // happens to the version it may have gone over: one that
                // isn't kept is reported on its own (it's still in the
                // file's version history), not as the upload failing, which
                // would leave it out of what the round did.
                if let (Some(pre), Some(mark)) = (replace, mark) {
                    match self.keep_replaced(store, &pre.id, *parent, to, name, &mark).await {
                        Ok(kept) => notes.theirs = kept,
                        Err(Fail::Failed(why)) => notes.theirs_not_kept = Some(why),
                        Err(Fail::Skip(s)) => notes.theirs_not_kept = Some(format!("{THEIRS_NOT_KEPT}: {s:?}")),
                        Err(Fail::NotNow) => notes.theirs_not_kept = Some(THEIRS_NOT_KEPT.into()),
                        Err(Fail::Halt(m)) => {
                            notes.theirs_not_kept = Some(format!("{THEIRS_NOT_KEPT}: {m}"));
                            notes.halt = Some(m);
                        }
                    }
                }
                Ok(())
            }
            // Files On-Demand: a placeholder instead of the content (no
            // download, nothing written into the sync folder).
            Op::Download { remote, to, parent, name, replace, link } if self.fs.on_demand() => {
                let (expected, synced) = store
                    .write(|o| {
                        need(link_ok(o, *link)? && o.synced_get(*parent)?.is_some())?;
                        Ok::<_, Fail>((o.get_item(&remote.id.0)?, synced_hash(o, *link)?))
                    })
                    .await?;
                let Some(expected) =
                    expected.filter(|r| r.kind == Kind::File && r.etag.as_deref().unwrap_or_default() == remote.etag)
                else {
                    return Err(Fail::Skip(Skip::Changed));
                };
                // Without OneDrive's hash, nothing could vouch for the content
                // a placeholder stands for.
                let Some(v) = version(&expected) else { return Err(Fail::Skip(Skip::Refused)) };
                let placed = match replace {
                    None => self.fs.place_placeholder(a, &v)?,
                    // Reads the old copy, to see whether it can go.
                    Some(_) => {
                        let new = v.clone();
                        self.off_task(a, move |fs, a| fs.replace_placeholder(a, &new, synced.as_ref())).await??
                    }
                };
                let Done::Placed { entry, kept, .. } = placed else { return Err(odd()) };
                notes.kept_instead = replace.is_some() && kept.is_some() && a.old_copy() == OldCopy::DropIfSynced;
                self.crash_point()?;
                store
                    .write(|o| {
                        let row = self.local_row(&entry, *to);
                        o.local_update(&row)?;
                        o.local_hash_put(row.file_id, row.size, row.mtime, &v.identity.hash)?;
                        let b = file_base(
                            *parent,
                            name,
                            row.size,
                            v.identity.hash,
                            LocalId(row.file_id),
                            row.mtime,
                            &expected,
                        );
                        link_row(o, *link, b)
                    })
                    .await
            }
            Op::Download { remote, to, parent, name, replace, link } => {
                let (expected, synced) = store
                    .write(|o| {
                        need(link_ok(o, *link)? && o.synced_get(*parent)?.is_some())?;
                        Ok::<_, Fail>((o.get_item(&remote.id.0)?, synced_hash(o, *link)?))
                    })
                    .await?;
                let Some(expected) =
                    expected.filter(|r| r.kind == Kind::File && r.etag.as_deref().unwrap_or_default() == remote.etag)
                else {
                    return Err(Fail::Skip(Skip::Changed));
                };
                let mut staged = self.fs.stage()?;
                let (len, digest) = {
                    let Some(file) = staged.file() else { return Err(odd()) };
                    let go = || may_run(a.op());
                    let mut w = Hashing::new(Stoppable { inner: file, go: &go, stopped: false });
                    let received = match self.cloud.download(remote, &mut w).await {
                        // Only staged so far, which goes with `staged`.
                        Err(_) if w.inner.stopped => return Err(Fail::NotNow),
                        r => r?,
                    };
                    // In the cloud's terms (see `cloud`): what it received, if it transformed it.
                    (w.hash.len(), received.unwrap_or_else(|| w.hash.finalize()))
                };
                // Verified before anything local changes: the content must be
                // exactly what the Remote tree says this eTag holds.
                if i64::try_from(len).ok() != Some(expected.size) || expected.qxh.is_some_and(|q| q != digest) {
                    return Err(Fail::Skip(Skip::ContentMismatch));
                }
                if let Some(old) = replace {
                    store
                        .write_ahead(|o| {
                            o.note_local_deleted(old.id.0)?;
                            Ok::<_, Fail>(o.meta_set_ahead(&pending_of(old.id), &Pending::Evidence(old.id).encode())?)
                        })
                        .await?;
                    self.crash_point()?;
                }
                let placed = self.off_task(a, move |fs, a| fs.place_download(a, staged, synced.as_ref())).await?;
                let entry = match placed {
                    Ok(Done::Placed { entry, kept, .. }) => {
                        notes.kept_instead =
                            replace.is_some() && kept.is_some() && a.old_copy() == OldCopy::DropIfSynced;
                        entry
                    }
                    Ok(_) => return Err(odd()),
                    Err(e) => return Err(e.into()),
                };
                self.crash_point()?;
                store
                    .write(|o| {
                        if let Some(old) = replace {
                            o.local_remove_subtree(old.id.0)?;
                            o.meta_delete(&pending_of(old.id))?;
                        }
                        let row = self.local_row(&entry, *to);
                        o.local_update(&row)?;
                        o.local_hash_put(row.file_id, row.size, row.mtime, &digest)?;
                        let b = file_base(*parent, name, row.size, digest, LocalId(row.file_id), row.mtime, &expected);
                        link_row(o, *link, b)
                    })
                    .await
            }
            Op::MkdirRemote { .. } | Op::MoveRemote { .. } | Op::RenameRemote { .. } | Op::DeleteRemote { .. } => {
                self.meta_one(store, a.op()).await
            }
            Op::MkdirLocal { remote, to: _, parent, name, link } => {
                let row = store
                    .write(|o| {
                        need(link_ok(o, *link)? && o.synced_get(*parent)?.is_some())?;
                        Ok::<_, Fail>(o.get_item(&remote.id.0)?)
                    })
                    .await?;
                let Some(row) = row.filter(|r| r.etag.as_deref().unwrap_or_default() == remote.etag) else {
                    return Err(Fail::Skip(Skip::Changed));
                };
                let placed = if self.fs.on_demand() {
                    self.fs.place_placeholder(a, &folder_version(&row))?
                } else {
                    self.fs.apply(a)?
                };
                let Done::Placed { parent: dir, entry, .. } = placed else { return Err(odd()) };
                self.crash_point()?;
                store
                    .write(|o| {
                        let local = self.local_row(&entry, dir);
                        o.local_update(&local)?;
                        link_row(o, *link, folder_base(*parent, name, LocalId(local.file_id), &row))
                    })
                    .await
            }

            // ------------------------------------------------ moves
            Op::MoveLocal { sid, local: _, to: _, parent, name } => {
                let listed = store
                    .write(|o| {
                        let Some(b) = o.synced_get(*sid)? else { return Err(Fail::Skip(Skip::Changed)) };
                        Ok::<_, Fail>(match b.remote {
                            Some(r) => o.get_item(&r.0)?,
                            None => None,
                        })
                    })
                    .await?;
                // This move takes the item where the listing has it in
                // OneDrive: only while OneDrive still has it there. A listing
                // behind the cloud can show it where it was before a move of
                // ours, and following that would undo a move made here since.
                if let Some(row) = listed
                    && let Some(at) = &row.parent_id
                {
                    let still = self.cloud.is_at(&RemoteId(row.id.clone()), at, &row.name).await?;
                    need(still != Some(false))?;
                }
                let Done::Placed { parent: dir, entry, .. } = self.fs.apply(a)? else { return Err(odd()) };
                self.crash_point()?;
                store
                    .write(|o| {
                        o.local_update(&self.local_row(&entry, dir))?;
                        let Some(mut b) = o.synced_get(*sid)? else { return Err(Fail::Skip(Skip::Changed)) };
                        b.parent = Some(*parent);
                        b.name.clone_from(name);
                        o.synced_put(&b)?;
                        Ok(())
                    })
                    .await
            }
            Op::RenameLocal { .. } => {
                let Done::Placed { parent: dir, entry, .. } = self.fs.apply(a)? else { return Err(odd()) };
                self.crash_point()?;
                store.write(|o| Ok(o.local_update(&self.local_row(&entry, dir))?)).await
            }
            Op::ConflictCopy { sid, local, name, .. } => {
                // Detach first (see the crate docs): the renamed copy must
                // never be taken for a rename of the Synced item.
                store
                    .write_ahead(|o| {
                        let Some(b) = o.synced_get(*sid)? else { return Err(Fail::Skip(Skip::Changed)) };
                        need(b.local == Some(local.id))?;
                        o.synced_put(&Base { local: None, ..b })?;
                        let p = Pending::Detached { sid: *sid, local: local.id, name: name.clone() };
                        Ok(o.meta_set_ahead(&pending_of(local.id), &p.encode())?)
                    })
                    .await?;
                self.crash_point()?;
                let Done::Placed { parent, entry, .. } = self.fs.apply(a)? else { return Err(odd()) };
                self.crash_point()?;
                store
                    .write(|o| {
                        o.local_update(&self.local_row(&entry, parent))?;
                        Ok(o.meta_delete(&pending_of(local.id))?)
                    })
                    .await
            }

            // ------------------------------------------------ deletions
            Op::RecycleLocal { sid, local } => {
                let synced = store
                    .write_ahead(|o| {
                        let Some(b) = o.synced_get(*sid)? else { return Err(Fail::Skip(Skip::Changed)) };
                        o.note_local_deleted(local.id.0)?;
                        o.meta_set_ahead(&pending_of(local.id), &Pending::Evidence(local.id).encode())?;
                        Ok::<_, Fail>(b.hash)
                    })
                    .await?;
                self.crash_point()?;
                match self.off_task(a, move |fs, a| fs.recycle(a, synced.as_ref())).await? {
                    Ok(Done::Recycled { kept }) => {
                        notes.kept_instead = kept.is_some() && a.old_copy() == OldCopy::DropIfSynced;
                    }
                    Ok(_) => return Err(odd()),
                    Err(e) => return Err(e.into()),
                }
                self.crash_point()?;
                store
                    .write(|o| {
                        o.local_remove_subtree(local.id.0)?;
                        o.synced_remove(*sid)?;
                        Ok(o.meta_delete(&pending_of(local.id))?)
                    })
                    .await
            }
            Op::PruneRemote { remote, forget, keep, .. } => {
                // Only once the copy here is complete (data-safety.md invariant 1).
                for pre in keep {
                    need(self.fs.has_content(pre)?)?;
                }
                // The Synced rows go first (see the operation's docs): after
                // a crash here, nothing is left that could later read as a
                // cloud deletion to bring to this PC. Durably: rolled back
                // by a power cut after the prune, they would.
                store
                    .write_ahead(|o| {
                        o.require_durable()?;
                        for sid in forget {
                            o.synced_remove(*sid)?;
                        }
                        Ok::<_, Fail>(())
                    })
                    .await?;
                self.crash_point()?;
                self.cloud.prune(remote).await?;
                self.crash_point()?;
                store
                    .write(|o| {
                        o.delete_subtree(&remote.id.0)?;
                        Ok::<_, Fail>(())
                    })
                    .await?;
                // Placeholders here no longer have a OneDrive copy behind
                // them: ordinary files from now on. Best effort (the content
                // is here either way).
                if self.fs.on_demand() {
                    for pre in keep {
                        if let Err(e) = self.fs.make_ordinary(pre) {
                            tracing::warn!(error = %e, "a placeholder whose OneDrive copy was removed stays one");
                        }
                    }
                }
                Ok(())
            }
            Op::Dehydrate { local, hash } => {
                // Only the cloud's current version is freed: its content is
                // then exactly what OneDrive holds (`data-safety.md` invariant 1).
                let current = store
                    .write(|o| {
                        let Some(b) = o.synced_by_local(local.id.0)? else { return Ok::<_, Fail>(false) };
                        let Some(r) = b.remote else { return Ok(false) };
                        Ok(o.get_item(&r.0)?.is_some_and(|row| row.qxh == Some(*hash)) && b.hash == Some(*hash))
                    })
                    .await?;
                need(current)?;
                let Done::Freed { entry } = self.off_task(a, |fs, a| fs.free_up(a)).await?? else { return Err(odd()) };
                self.crash_point()?;
                store
                    .write(|o| {
                        let Some(row) = o.local_get(local.id.0)? else { return Ok(()) };
                        let parent = row.parent_id.map(LocalId).unwrap_or(self.fs.root_id());
                        Ok(o.local_update(&self.local_row(&entry, parent))?)
                    })
                    .await
            }
            Op::Hold { .. } => Err(Fail::Failed("a held item can't be executed".into())),
        }
    }

    /// The Local tree row for an item the effects layer just placed.
    fn local_row(&self, entry: &oneup_effects::DirEntry, parent: LocalId) -> LocalRow {
        let rules = self.rules.read().unwrap_or_else(|p| p.into_inner());
        oneup_local::classify::to_row(entry, parent.0, parent == self.fs.root_id(), &rules)
    }

    /// Settles the write-ahead record under `key` if its operation didn't
    /// finish: it failed, or the process died (then this runs at the start
    /// of the next round). If the effect didn't happen, the record is
    /// undone; if it did, the record stands and the change journal reports
    /// the rest. Whether it happened is read from the folder itself, which,
    /// unlike the journal, can't lag behind.
    async fn recover(&self, store: &Db<'_>, key: &str) -> Res {
        let Some(p) = store.write(|o| Ok::<_, Fail>(o.meta_get(key)?)).await? else { return Ok(()) };
        let pending = Pending::decode(&p);
        let undo = match &pending {
            // Recycling and replacing take the item out of the folder.
            Some(Pending::Evidence(id)) => self.fs.is_present(*id),
            // A conflict copy renames it in place.
            Some(Pending::Detached { local, name, .. }) => self.fs.name_of(*local).is_some_and(|n| n != *name),
            None => false,
        };
        tracing::debug!(undo, "settling an unfinished write-ahead record");
        store
            .write(|o| {
                match pending {
                    Some(Pending::Evidence(id)) if undo => o.unnote_local_deleted(id.0)?,
                    Some(Pending::Detached { sid, local, .. }) if undo => {
                        if let Some(b) = o.synced_get(sid)?
                            && b.local.is_none()
                            && o.synced_by_local(local.0)?.is_none()
                        {
                            o.synced_put(&Base { local: Some(local), ..b })?;
                        }
                    }
                    _ => {}
                }
                Ok(o.meta_delete(key)?)
            })
            .await
    }

    /// Settles every record a round that was cut short left, including
    /// one under [`PENDING`] itself, where versions that ran one operation
    /// at a time kept theirs.
    async fn recover_all(&self, store: &Db<'_>) -> Res {
        for (key, _) in store.read(|o| o.meta_under(PENDING))? {
            self.recover(store, &key).await?;
        }
        Ok(())
    }
}

/// Where write-ahead records are kept (see the crate docs): under
/// `exec.pending.<local file ID>`, one per item, so operations under way
/// at once each have their own. Set in the same transaction as the
/// write-ahead, committed durably ([`Db::write_ahead`]), cleared in the
/// same transaction as the final record.
const PENDING: &str = "exec.pending";

/// The key of item `id`'s write-ahead record.
fn pending_of(id: LocalId) -> String {
    format!("{PENDING}.{:x}", id.0)
}

/// The key of the record `op` writes ahead of its effect, if it writes one.
fn pending_key(op: &Op) -> Option<String> {
    match op {
        Op::RecycleLocal { local, .. } | Op::ConflictCopy { local, .. } => Some(pending_of(local.id)),
        Op::Download { replace: Some(old), .. } => Some(pending_of(old.id)),
        _ => None,
    }
}

#[derive(Debug, PartialEq, Eq)]
enum Pending {
    /// Deletion evidence noted for an item about to be recycled or replaced.
    Evidence(LocalId),
    /// Synced item `sid` detached from `local`, about to be renamed `name`.
    Detached { sid: Sid, local: LocalId, name: String },
}

impl Pending {
    fn encode(&self) -> String {
        match self {
            Pending::Evidence(id) => format!("evidence {:x}", id.0),
            Pending::Detached { sid, local, name } => format!("detached {sid} {:x} {name}", local.0),
        }
    }

    fn decode(s: &str) -> Option<Self> {
        let mut it = s.splitn(4, ' ');
        match it.next()? {
            "evidence" => Some(Pending::Evidence(LocalId(u128::from_str_radix(it.next()?, 16).ok()?))),
            "detached" => {
                let sid = it.next()?.parse().ok()?;
                let local = LocalId(u128::from_str_radix(it.next()?, 16).ok()?);
                Some(Pending::Detached { sid, local, name: it.next()?.to_string() })
            }
            _ => None,
        }
    }
}

/// Passes a download's bytes on while `go` says it may go on; once it says
/// no, refuses them (and notes it in `stopped`), which ends the download.
/// Asked at every write, as data arrives: a stalled connection writes
/// nothing, and uses nothing either.
struct Stoppable<'a, W> {
    inner: W,
    go: &'a (dyn Fn() -> bool + Sync),
    stopped: bool,
}

impl<W: std::io::Write> std::io::Write for Stoppable<'_, W> {
    fn write(&mut self, buf: &[u8]) -> std::io::Result<usize> {
        if self.stopped || !(self.go)() {
            self.stopped = true;
            // Not `Interrupted`: `write_all` would try again.
            return Err(std::io::Error::other("stopped: not to run now"));
        }
        self.inner.write(buf)
    }
    fn flush(&mut self) -> std::io::Result<()> {
        self.inner.flush()
    }
}

/// What a placeholder for cloud item `r` stands for; `None` without
/// OneDrive's content hash.
fn version(r: &RemoteRow) -> Option<Version> {
    Some(Version {
        identity: Identity { item: r.id.clone(), etag: r.etag.clone().unwrap_or_default(), size: r.size, hash: r.qxh? },
        modified: filetime(r.mtime_ms),
    })
}

/// A folder placeholder's version (no content, so no hash).
fn folder_version(r: &RemoteRow) -> Version {
    Version {
        identity: Identity { item: r.id.clone(), etag: r.etag.clone().unwrap_or_default(), size: 0, hash: [0; 20] },
        modified: filetime(r.mtime_ms),
    }
}

/// Unix milliseconds as a Windows FILETIME (now if unknown).
fn filetime(ms: Option<i64>) -> i64 {
    let ms = ms.unwrap_or_else(|| {
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map_or(0, |d| i64::try_from(d.as_millis()).unwrap_or(0))
    });
    ms.saturating_mul(10_000).saturating_add(116_444_736_000_000_000)
}

/// An outcome the effects layer never produces for that operation.
fn odd() -> Fail {
    Fail::Failed("unexpected effect outcome".into())
}

fn local_ok(o: &Ops<'_>, pre: &LocalPre) -> oneup_store::Result<bool> {
    let Some(row) = o.local_get(pre.id.0)? else { return Ok(false) };
    Ok(match pre.stamp {
        None => matches!(row.kind, LocalKind::Folder | LocalKind::Root),
        Some((size, mtime)) => row.kind == LocalKind::File && row.size == size && row.mtime == mtime,
    })
}

/// The precondition for changing cloud item `r`: its current eTag.
fn pre_of(r: &RemoteRow) -> RemotePre {
    RemotePre { id: RemoteId(r.id.clone()), etag: r.etag.clone().unwrap_or_default() }
}

/// Records that cloud item `old` is now `row` (moved, or rebuilt under a
/// new ID by [`Executor::rebuild`]): in the Remote tree if it is stored
/// there (keeping whether it is excluded), and in the Synced row that
/// stands for it.
fn record_moved(o: &Ops<'_>, old: &str, row: &RemoteRow) -> Res {
    use oneup_store::ItemState;
    let excluded = match (o.item_state(old)?, o.item_state(&row.id)?) {
        (ItemState::Included, _) | (_, ItemState::Included) => false,
        (ItemState::Excluded, _) | (_, ItemState::Excluded) => true,
        // Not stored (inside an excluded folder, or new since the last
        // listing): the next listing brings it.
        _ => return Ok(()),
    };
    o.upsert_item(row, excluded)?;
    if let Some(mut b) = o.synced_by_remote(old)?.or(o.synced_by_remote(&row.id)?) {
        b.remote = Some(RemoteId(row.id.clone()));
        b.remote_etag = row.etag.clone().unwrap_or_default();
        o.synced_put(&b)?;
    }
    Ok(())
}

fn remote_ok(o: &Ops<'_>, pre: &RemotePre) -> oneup_store::Result<bool> {
    Ok(o.get_item(&pre.id.0)?.is_some_and(|r| r.etag.unwrap_or_default() == pre.etag))
}

/// The hash of the Synced row a replacement updates: the version both sides
/// last agreed on, which OneDrive keeps in its version history.
fn synced_hash(o: &Ops<'_>, link: Link) -> oneup_store::Result<Option<Hash>> {
    Ok(match link {
        Link::New => None,
        Link::Existing(sid) => o.synced_get(sid)?.and_then(|b| b.hash),
    })
}

fn link_ok(o: &Ops<'_>, link: Link) -> oneup_store::Result<bool> {
    Ok(match link {
        Link::New => true,
        Link::Existing(sid) => o.synced_get(sid)?.is_some(),
    })
}

fn link_row(o: &Ops<'_>, link: Link, b: Base) -> Res {
    // Checked when the operation started too; other operations ran since.
    if let Some(parent) = b.parent {
        need(o.synced_get(parent)?.is_some())?;
    }
    match link {
        Link::New => {
            o.synced_insert(&b)?;
        }
        Link::Existing(sid) => {
            need(o.synced_get(sid)?.is_some())?;
            o.synced_put(&Base { sid, ..b })?;
        }
    }
    Ok(())
}

/// A synced file's last-write time when it isn't known: no file has it,
/// so the next round sees the file as changed and hashes it.
const STAMP_UNKNOWN: i64 = 0;

fn file_base(
    parent: Sid,
    name: &str,
    size: i64,
    hash: Hash,
    local: LocalId,
    local_mtime: i64,
    remote: &RemoteRow,
) -> Base {
    Base {
        sid: 0,
        parent: Some(parent),
        name: name.to_string(),
        kind: NodeKind::File,
        size,
        hash: Some(hash),
        local: Some(local),
        local_mtime,
        remote: Some(RemoteId(remote.id.clone())),
        remote_etag: remote.etag.clone().unwrap_or_default(),
    }
}

fn folder_base(parent: Sid, name: &str, local: LocalId, remote: &RemoteRow) -> Base {
    Base {
        sid: 0,
        parent: Some(parent),
        name: name.to_string(),
        kind: NodeKind::Folder,
        size: 0,
        hash: None,
        local: Some(local),
        local_mtime: 0,
        remote: Some(RemoteId(remote.id.clone())),
        remote_etag: remote.etag.clone().unwrap_or_default(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn pending_records_round_trip() {
        for p in [
            Pending::Evidence(LocalId(u128::MAX)),
            Pending::Detached { sid: 7, local: LocalId(0x1234), name: "a b (conflict 1).txt".into() },
        ] {
            assert_eq!(Pending::decode(&p.encode()), Some(p));
        }
        assert_eq!(Pending::decode("something else"), None);
    }

    fn local(id: u128, parent: Option<u128>, name: &str, kind: LocalKind) -> LocalRow {
        LocalRow {
            file_id: id,
            parent_id: parent,
            name: name.into(),
            name_lossy: false,
            kind,
            size: 1,
            mtime: 1,
            attributes: 0,
            reparse_tag: 0,
            skip: oneup_store::Skip::None,
        }
    }

    fn remote(id: &str, parent: Option<&str>, name: &str, kind: Kind) -> RemoteRow {
        RemoteRow {
            id: id.into(),
            parent_id: parent.map(Into::into),
            name: name.into(),
            kind,
            size: 1,
            etag: Some("e".into()),
            qxh: None,
            mtime_ms: None,
        }
    }

    /// Folder `x` (local 10, cloud `rx`) holding files `a.txt` (11, `ra`)
    /// and `b.txt` (12, `rb`), all synced. Returns their Sids.
    fn tree(store: &mut Store) -> (Sid, Sid, Sid) {
        store
            .write(|o| {
                o.local_put(&local(1, None, "", LocalKind::Root))?;
                o.local_put(&local(10, Some(1), "x", LocalKind::Folder))?;
                o.local_put(&local(11, Some(10), "a.txt", LocalKind::File))?;
                o.local_put(&local(12, Some(10), "b.txt", LocalKind::File))?;
                o.upsert_item(&remote("r", None, "", Kind::Root), false)?;
                o.upsert_item(&remote("rx", Some("r"), "x", Kind::Folder), false)?;
                o.upsert_item(&remote("ra", Some("rx"), "a.txt", Kind::File), false)?;
                o.upsert_item(&remote("rb", Some("rx"), "b.txt", Kind::File), false)?;
                link_roots(o, 1, "r")?;
                let base = |parent, name: &str, kind, l: u128, r: &str| Base {
                    sid: 0,
                    parent: Some(parent),
                    name: name.into(),
                    kind,
                    size: 1,
                    hash: None,
                    local: Some(LocalId(l)),
                    local_mtime: 1,
                    remote: Some(RemoteId(r.into())),
                    remote_etag: "e".into(),
                };
                let x = o.synced_insert(&base(ROOT, "x", NodeKind::Folder, 10, "rx"))?;
                let a = o.synced_insert(&base(x, "a.txt", NodeKind::File, 11, "ra"))?;
                let b = o.synced_insert(&base(x, "b.txt", NodeKind::File, 12, "rb"))?;
                Ok((x, a, b))
            })
            .unwrap()
    }

    /// Whether `b` has to wait for `a` (or either runs alone).
    fn waits(o: &Ops<'_>, a: &Op, b: &Op) -> bool {
        let (ca, cb) = (claims_in(o, a).unwrap(), claims_in(o, b).unwrap());
        let mut held = Held::default();
        held.add(&ca);
        ca.alone || cb.alone || held.conflicts(&cb)
    }

    fn lpre(id: u128) -> LocalPre {
        LocalPre { id: LocalId(id), stamp: Some((1, 1)) }
    }

    fn rpre(id: &str) -> RemotePre {
        RemotePre { id: RemoteId(id.into()), etag: "e".into() }
    }

    fn upload(id: u128, x: Sid, name: &str) -> Op {
        Op::Upload {
            local: lpre(id),
            to: RemoteId("rx".into()),
            parent: x,
            name: name.into(),
            replace: None,
            link: Link::New,
        }
    }

    #[test]
    fn only_downloads_with_onedrives_hash_have_their_links_read_ahead() {
        let mut store = Store::open_in_memory().unwrap();
        let (x, _, _) = tree(&mut store);
        store
            .write(|o| {
                o.upsert_item(
                    &RemoteRow { qxh: Some([1; 20]), ..remote("rh", Some("rx"), "h.txt", Kind::File) },
                    false,
                )?;
                o.upsert_item(&remote("rn", Some("rx"), "n.txt", Kind::File), false)
            })
            .unwrap();
        let download = |id: &str| Op::Download {
            remote: rpre(id),
            to: LocalId(10),
            parent: x,
            name: format!("{id}.txt"),
            replace: None,
            link: Link::New,
        };
        let gate = oneup_gate::Policy::default();
        let r = oneup_gate::review_in(
            &gate,
            &oneup_gate::Context::default(),
            vec![download("rh"), download("rn"), upload(13, x, "c.txt")],
        );
        let db = Db::new(&mut store);
        let coming: Vec<String> = coming_downloads(&db, &r.approved).into_iter().map(|p| p.id.0).collect();
        assert_eq!(coming, ["rh"]);
    }

    #[test]
    fn transfers_in_one_folder_run_together_and_wait_for_a_change_to_it() {
        let mut store = Store::open_in_memory().unwrap();
        let (x, a, b) = tree(&mut store);
        store
            .read(|o| {
                let (c, d) = (upload(13, x, "c.txt"), upload(14, x, "d.txt"));
                assert!(!waits(o, &c, &d));
                let replace = Op::Download {
                    remote: rpre("ra"),
                    to: LocalId(10),
                    parent: x,
                    name: "a.txt".into(),
                    replace: Some(lpre(11)),
                    link: Link::Existing(a),
                };
                assert!(!waits(o, &c, &replace));
                let edit_b = Op::Upload {
                    local: lpre(12),
                    to: RemoteId("rx".into()),
                    parent: x,
                    name: "b.txt".into(),
                    replace: Some(rpre("rb")),
                    link: Link::Existing(b),
                };
                assert!(!waits(o, &replace, &edit_b));
                // Moving the folder they go into: one after the other.
                let mv = Op::MoveRemote {
                    sid: x,
                    remote: rpre("rx"),
                    to: RemoteId("r".into()),
                    parent: ROOT,
                    name: "y".into(),
                };
                assert!(claims_in(o, &mv)?.alone);
                assert!(waits(o, &c, &mv) && waits(o, &mv, &c));
                Ok(())
            })
            .unwrap();
    }

    #[test]
    fn a_name_freed_and_taken_and_the_same_item_twice_keep_their_order() {
        let mut store = Store::open_in_memory().unwrap();
        let (x, a, _) = tree(&mut store);
        store
            .read(|o| {
                // a.txt goes, then a new a.txt arrives.
                let recycle = Op::RecycleLocal { sid: a, local: lpre(11) };
                let arrive = Op::Download {
                    remote: rpre("rnew"),
                    to: LocalId(10),
                    parent: x,
                    name: "A.TXT".into(),
                    replace: None,
                    link: Link::New,
                };
                assert!(!claims_in(o, &recycle)?.alone);
                assert!(waits(o, &recycle, &arrive));
                // A rename that frees b.txt, and one that takes it.
                let free_b = Op::RenameLocal { local: lpre(12), dir: LocalId(10), name: "c.txt".into() };
                let take_b = Op::RenameLocal { local: lpre(11), dir: LocalId(10), name: "b.txt".into() };
                assert!(waits(o, &free_b, &take_b));
                // Hashing a file, then uploading it.
                assert!(waits(o, &Op::Hash(lpre(11)), &upload(11, x, "a.txt")));
                // Unrelated names in the same folder don't wait.
                assert!(!waits(o, &free_b, &upload(13, x, "d.txt")));
                Ok(())
            })
            .unwrap();
    }

    #[test]
    fn forgetting_or_recycling_a_folder_runs_alone_but_reading_a_large_file_does_not() {
        let mut store = Store::open_in_memory().unwrap();
        let (x, a, _) = tree(&mut store);
        store
            .write(|o| {
                // OneDrive has a file the size of the large one: an upload
                // of it reads it whole to see whether it can be a copy.
                o.upsert_item(&RemoteRow { size: 1 << 40, ..remote("rbig", Some("rx"), "big", Kind::File) }, false)
            })
            .unwrap();
        store
            .read(|o| {
                assert!(claims_in(o, &Op::Forget(x))?.alone);
                assert!(!claims_in(o, &Op::Forget(a))?.alone);
                assert!(
                    claims_in(o, &Op::RecycleLocal { sid: x, local: LocalPre { id: LocalId(10), stamp: None } })?.alone
                );
                // Something the store doesn't have.
                assert!(claims_in(o, &Op::Forget(999))?.alone);
                // Reading a large file whole holds nothing else up: it's
                // read on a blocking thread (`crate::blocking`).
                let big = LocalPre { id: LocalId(11), stamp: Some((1 << 40, 1)) };
                let reads = [
                    Op::Hash(big.clone()),
                    Op::RecycleLocal { sid: a, local: big.clone() },
                    Op::Dehydrate { local: big.clone(), hash: [0; 20] },
                    Op::Upload {
                        local: big,
                        to: RemoteId("rx".into()),
                        parent: x,
                        name: "copy of big".into(),
                        replace: None,
                        link: Link::New,
                    },
                ];
                for op in reads {
                    let c = claims_in(o, &op)?;
                    assert!(!c.alone, "{op:?}");
                    // Hashing keeps to its own limit instead.
                    assert_eq!(c.hash, matches!(op, Op::Hash(_)), "{op:?}");
                }
                Ok(())
            })
            .unwrap();
    }
}
