//! The executor proper (see the crate docs).

use std::collections::BTreeSet;
use std::io::Seek as _;

use oneup_cfapi::identity::Identity;
use oneup_effects::{Done, Error as FxError, LocalFs, Version};
use oneup_gate::{Approved, OldCopy};
use oneup_rules::ExcludeSet;
use oneup_store::{Kind, LocalKind, LocalRow, Ops, RemoteRow, Store};
use oneup_sync::{
    Base, Hash, Link, LocalId, LocalPre, NodeKind, Op, ROOT, RemoteId, RemotePre, Sid, conflict_name, fold,
};

use crate::cloud::{Cloud, CloudError, Hashing, Meta, MetaDone, VersionMark};

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
    /// The OneDrive has no room left: an upload refused for it, or one not
    /// tried after another was in the same round.
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

/// Adds one operation's outcome to the report; true if the round halts.
fn record(report: &mut Report, op: Op, outcome: Res, notes: Notes) -> bool {
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
            match notes.halt {
                Some(m) => {
                    if report.halted.is_none() {
                        tracing::warn!(reason = %m, "round halted");
                        report.halted = Some(m);
                    }
                    true
                }
                None => false,
            }
        }
        Err(Fail::Skip(s)) => {
            tracing::debug!(reason = ?s, "operation skipped");
            report.skipped.push((op, s));
            false
        }
        Err(Fail::Failed(m)) => {
            tracing::warn!(error = %m, "operation failed");
            report.failed.push((op, m));
            false
        }
        Err(Fail::NotNow) => {
            report.not_run.push(op);
            false
        }
        Err(Fail::Halt(m)) => {
            if m != NOT_RUN && report.halted.is_none() {
                tracing::warn!(reason = %m, "round halted");
                report.halted = Some(m);
            }
            true
        }
    }
}

/// How many of the operations at the start of `ops` can go to the cloud in
/// one batch: consecutive cloud metadata changes (create folder, move or
/// rename, delete), at most a batch's worth, none depending on another.
/// Conservative: the group ends before an operation that touches an item
/// another one in the group touches, moves into or deletes a folder another
/// one changes or moves into, takes a name another one frees or takes, or
/// deletes a folder something in the group came from (Graph would see it
/// still holding that item). Below 2 the caller runs them one by one.
fn batchable(store: &Store, ops: &[Approved]) -> usize {
    use std::collections::HashSet;
    let mut touched: HashSet<String> = HashSet::new(); // items moved or deleted
    let mut parents: HashSet<String> = HashSet::new(); // their folders, and folders moved into
    let mut names: HashSet<(String, String)> = HashSet::new(); // names freed or taken
    let mut n = 0;
    for a in ops.iter().take(oneup_graph::client::BATCH_MAX) {
        // The item's current folder and name, from the Remote tree.
        let at = |id: &RemoteId| {
            store.read(|o| o.get_item(&id.0)).ok().flatten().and_then(|r| r.parent_id.map(|p| (p, fold(&r.name))))
        };
        let (item, from, to) = match a.op() {
            Op::MkdirRemote { to, name, .. } => (None, None, Some((to.0.clone(), fold(name)))),
            Op::MoveRemote { remote, to, name, .. } | Op::RenameRemote { remote, dir: to, name } => {
                match at(&remote.id) {
                    Some(from) => (Some(remote.id.0.clone()), Some(from), Some((to.0.clone(), fold(name)))),
                    None => break,
                }
            }
            Op::DeleteRemote { remote, .. } => match at(&remote.id) {
                Some(from) => (Some(remote.id.0.clone()), Some(from), None),
                None => break,
            },
            _ => break,
        };
        let clash = item.as_ref().is_some_and(|i| touched.contains(i) || parents.contains(i))
            || from.as_ref().is_some_and(|f| touched.contains(&f.0) || names.contains(f))
            || to.as_ref().is_some_and(|t| touched.contains(&t.0) || names.contains(t));
        if clash {
            break;
        }
        if let Some(i) = item {
            touched.insert(i);
        }
        if let Some(f) = from {
            parents.insert(f.0.clone());
            names.insert(f);
        }
        if let Some(t) = to {
            parents.insert(t.0.clone());
            names.insert(t);
        }
        n += 1;
    }
    n
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
    fs: LocalFs,
    cloud: C,
    /// The name-only folder rules the Local tree follows; they can change
    /// between rounds ([`Executor::set_rules`]).
    rules: std::sync::RwLock<ExcludeSet>,
    /// Crash points left to pass before the injected crash; negative: off.
    #[cfg(feature = "crash-test")]
    crash: std::sync::atomic::AtomicI64,
}

impl<C: Cloud> Executor<C> {
    /// `rules`: the exclusion rules the Local tree was scanned with, so
    /// rows the executor writes match what the observer would write.
    pub fn new(fs: LocalFs, cloud: C, rules: ExcludeSet) -> Self {
        Self {
            fs,
            cloud,
            rules: std::sync::RwLock::new(rules),
            #[cfg(feature = "crash-test")]
            crash: std::sync::atomic::AtomicI64::new(-1),
        }
    }

    /// The rules changed: rows written from now on follow these.
    pub fn set_rules(&self, rules: ExcludeSet) {
        *self.rules.write().unwrap_or_else(|p| p.into_inner()) = rules;
    }

    /// Test-only: the round stops at the crash point after `n` others have
    /// passed, as if the process had been killed there, and reports
    /// [`CRASHED`] as its halt. Crash points are right after each effect
    /// (before its record) and right after each write-ahead record (before
    /// its effect). `None` turns it off.
    #[cfg(feature = "crash-test")]
    pub fn crash_after(&self, n: Option<u32>) {
        self.crash.store(n.map_or(-1, i64::from), std::sync::atomic::Ordering::Relaxed);
    }

    /// A place where a killed process would leave the effects layer and the
    /// store out of step. Nothing runs after an injected crash: no record,
    /// no withdrawal.
    fn crash_point(&self) -> Res {
        #[cfg(feature = "crash-test")]
        if self.crash.fetch_sub(1, std::sync::atomic::Ordering::Relaxed) == 0 {
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
        store: &mut Store,
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
        let mut taken = store.read(|o| {
            let mut taken: BTreeSet<String> = o.remote_children(&to.0)?.iter().map(|(r, _)| fold(&r.name)).collect();
            if let Some(local) = o.synced_get(parent)?.and_then(|b| b.local) {
                taken.extend(o.local_children(local.0)?.iter().map(|r| fold(&r.name)));
            }
            Ok(taken)
        })?;
        // A name OneDrive has that the Remote tree doesn't yet: another.
        for _ in 0..NAME_TRIES {
            let copy = conflict_name(name, NodeKind::File, &mut taken);
            file.rewind().map_err(|e| lost(e.to_string()))?;
            let mut reader = Hashing::new(&mut *file);
            match self.cloud.upload(to, &copy, None, size, &mut reader).await {
                Ok((row, sent)) => {
                    let sent = sent.unwrap_or_else(|| reader.hash.finalize());
                    let read = reader.hash.len();
                    store.try_write(|o| Ok::<_, Fail>(o.upsert_item(&row, false)?))?;
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

    /// For a new file (`new`) of at least `COPY_MIN` bytes: if OneDrive
    /// already has a file with the same size and content, has OneDrive copy
    /// it to `to/name` instead of uploading (server-side copy).
    /// Reads the open file to hash it only when a cloud file of exactly its
    /// size exists, and leaves it rewound for an upload otherwise. `None`:
    /// upload it. The copy is verified: one whose content turned out
    /// different (the source changed in between) is removed again, into
    /// OneDrive's recycle bin, before uploading instead.
    async fn copy_instead(
        &self,
        store: &Store,
        src: &mut oneup_effects::UploadSource,
        to: &RemoteId,
        name: &str,
        new: bool,
    ) -> Res<Option<(RemoteRow, Hash)>> {
        use std::io::{Seek, SeekFrom};
        if !new || src.size < oneup_store::COPY_MIN || !self.cloud.can_copy() {
            return Ok(None);
        }
        let candidates = store.read(|o| o.remote_files_of_size(src.size, 16))?;
        if candidates.is_empty() {
            return Ok(None);
        }
        let mut buf = vec![0u8; 1 << 20];
        let h = oneup_hash::hash_reader(&src.file, &mut buf).map_err(|e| Fail::Failed(e.to_string()))?;
        src.file.seek(SeekFrom::Start(0)).map_err(|e| Fail::Failed(e.to_string()))?;
        let hash = h.finalize();
        if i64::try_from(h.len()).ok() != Some(src.size) {
            return Err(Fail::Skip(Skip::Changed));
        }
        let Some(source) = candidates.into_iter().find(|c| c.qxh == Some(hash)) else { return Ok(None) };
        let pre = RemotePre { id: RemoteId(source.id.clone()), etag: source.etag.clone().unwrap_or_default() };
        match self.cloud.copy(&pre, &hash, to, name).await {
            Ok(row) if row.qxh == Some(hash) && row.size == src.size => Ok(Some((row, hash))),
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

    /// Runs `approved` in order. Stops at the first halt.
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

    /// [`Self::run_counted`], asking `may_run` before each operation
    /// whether it may run now (a pause that came meanwhile, say). One it
    /// refuses isn't started ([`Report::not_run`]). A download under way is
    /// asked again as its data arrives, and stopped once refused: nothing
    /// was placed yet, so it's simply done again later. Anything else that
    /// started runs to its end (an upload commits, a batch is answered).
    /// `may_run` is called often: it must be cheap.
    pub async fn run_checked(
        &self,
        store: &mut Store,
        approved: Vec<Approved>,
        done: &(dyn Fn(usize) + Sync),
        may_run: &(dyn Fn(&Op) -> bool + Sync),
    ) -> Report {
        let mut report = Report::default();
        if let Err(e) = self.fs.check_root() {
            report.halted = Some(e.to_string());
            return report;
        }
        // A record left by a round that was cut short.
        if let Err(Fail::Halt(m) | Fail::Failed(m)) = self.recover(store) {
            report.halted = Some(m);
            return report;
        }
        // Once OneDrive says it's full, the round's other uploads aren't
        // sent only to be refused; everything else goes on.
        let mut full = false;
        let mut i = 0;
        while i < approved.len() {
            // A run of independent cloud metadata changes goes in one batch;
            // each keeps its own precondition and outcome.
            let n = batchable(store, &approved[i..]);
            // Up to the first the caller refuses, which goes on its own.
            let n = approved[i..i + n].iter().take_while(|a| may_run(a.op())).count();
            if n >= 2 {
                tracing::debug!(n, "cloud changes sent in one batch");
                let outcomes = self.run_batch(store, &approved[i..i + n]).await;
                let halted = approved[i..i + n]
                    .iter()
                    .zip(outcomes)
                    .any(|(a, outcome)| record(&mut report, a.op().clone(), outcome, Notes::default()));
                done(n);
                if halted {
                    break;
                }
                i += n;
                continue;
            }
            let a = &approved[i];
            i += 1;
            if !may_run(a.op()) {
                record(&mut report, a.op().clone(), Err(Fail::NotNow), Notes::default());
                done(1);
                continue;
            }
            if full && matches!(a.op(), Op::Upload { .. }) {
                record(&mut report, a.op().clone(), Err(Fail::Skip(Skip::CloudFull)), Notes::default());
                done(1);
                continue;
            }
            let mut notes = Notes::default();
            let mut outcome = self.one(store, a, &mut notes, may_run).await;
            if writes_ahead(a.op())
                && !matches!(outcome, Err(Fail::Halt(_)))
                && let Err(e) = self.recover(store)
            {
                outcome = Err(e);
            }
            full |= matches!(outcome, Err(Fail::Skip(Skip::CloudFull)));
            let halted = record(&mut report, a.op().clone(), outcome, notes);
            done(1);
            if halted {
                break;
            }
        }
        report
    }

    /// Runs a group from [`batchable`]: every precondition in the store,
    /// then one cloud batch, then each record in order. After a halt (an
    /// injected crash) nothing more is recorded, as after a real crash: the
    /// cloud changes stay unrecorded and the next round finds them.
    async fn run_batch(&self, store: &mut Store, group: &[Approved]) -> Vec<Res> {
        let mut outcomes: Vec<Option<Res>> = std::iter::repeat_with(|| None).take(group.len()).collect();
        let mut metas = Vec::new();
        let mut sent = Vec::new();
        for (k, a) in group.iter().enumerate() {
            match self.meta_pre(store, a.op()) {
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
                Ok(done) => self.crash_point().and_then(|()| self.meta_post(store, group[k].op(), done)),
            };
            halted = matches!(res, Err(Fail::Halt(_)));
            outcomes[k] = Some(res);
        }
        // Not reached after a halt: reported as not run.
        outcomes.into_iter().map(|o| o.unwrap_or_else(|| Err(Fail::Halt(NOT_RUN.into())))).collect()
    }

    /// The store-side precondition of a cloud metadata change, and what to send.
    fn meta_pre(&self, store: &mut Store, op: &Op) -> Res<Meta> {
        match op {
            Op::MkdirRemote { local, to, parent, name, link } => {
                store.try_write(|o| {
                    need(local_ok(o, local)? && link_ok(o, *link)? && o.synced_get(*parent)?.is_some())
                })?;
                Ok(Meta::Mkdir { parent: to.clone(), name: name.clone() })
            }
            Op::MoveRemote { sid, remote, to, name, .. } => {
                store.try_write(|o| need(o.synced_get(*sid)?.is_some()))?;
                Ok(Meta::Move { pre: remote.clone(), to: to.clone(), name: name.clone() })
            }
            Op::DeleteRemote { sid, remote } => {
                store.try_write(|o| need(o.synced_get(*sid)?.is_some()))?;
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
    fn meta_post(&self, store: &mut Store, op: &Op, done: MetaDone) -> Res {
        match (op, done) {
            (Op::MkdirRemote { local, parent, name, link, .. }, MetaDone::Item(row)) => {
                store.try_write(|o| {
                    o.upsert_item(&row, false)?;
                    link_row(o, *link, folder_base(*parent, name, local.id, &row))
                })?;
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
                let local = store.try_write(|o| {
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
                })?;
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
            (Op::RenameRemote { .. }, MetaDone::Item(row)) => store.try_write(|o| Ok(o.upsert_item(&row, false)?)),
            (Op::DeleteRemote { sid, remote }, MetaDone::Deleted) => store.try_write(|o| {
                o.delete_subtree(&remote.id.0)?;
                o.synced_remove(*sid)?;
                Ok(())
            }),
            _ => Err(odd()),
        }
    }

    /// One cloud metadata change on its own: the same three steps.
    async fn meta_one(&self, store: &mut Store, op: &Op) -> Res {
        let m = self.meta_pre(store, op)?;
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
        self.meta_post(store, op, done)
    }

    /// A move OneDrive refused because the folder has too many items below
    /// it ([`CloudError::TooManyItems`]), done in parts ([`Self::rebuild`])
    /// and then recorded as the move it stands for.
    async fn move_in_parts(&self, store: &mut Store, op: &Op, m: &Meta) -> Res {
        let Meta::Move { pre, to, name } = m else { return Err(CloudError::TooManyItems.into()) };
        tracing::info!("OneDrive won't move a folder this large in one request; moving its contents item by item");
        let row = self.rebuild(store, pre, to, name).await?;
        self.meta_post(store, op, MetaDone::Item(row))
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
    async fn rebuild(&self, store: &mut Store, pre: &RemotePre, to: &RemoteId, name: &str) -> Res<RemoteRow> {
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
        store.try_write(|o| record_moved(o, &pre.id.0, &made))?;
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
                    store.try_write(|o| record_moved(o, &c.id, &row))?;
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
        self.cloud.delete(&pre_of(&emptied)).await?;
        self.crash_point()?;
        // Nothing points at it any more: the Synced row follows the new
        // folder, and its contents were recorded as moved. Anything still
        // stored below it went elsewhere in OneDrive meanwhile (moved on the
        // web, say), not away, so its rows go without deletion evidence
        // (invariant 3): it reads as missing until the next listing brings
        // it where it is now. Unpruning drops the tombstones pruning leaves.
        store.try_write(|o| {
            o.prune_subtree(&pre.id.0, true)?;
            o.unprune_subtree(&pre.id.0)?;
            Ok::<_, Fail>(())
        })?;
        self.cloud.item(&into).await?.ok_or(Fail::Skip(Skip::Changed))
    }

    /// Runs one operation, noting in `notes` what else a done one reports
    /// (see [`Notes`]). `may_run`: see [`Self::run_checked`].
    async fn one(
        &self,
        store: &mut Store,
        a: &Approved,
        notes: &mut Notes,
        may_run: &(dyn Fn(&Op) -> bool + Sync),
    ) -> Res {
        match a.op() {
            // ------------------------------------------------ bookkeeping
            Op::Pair { local, remote, parent, name, kind, size, hash } => {
                let row = store.try_write(|o| {
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
                })?;
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
            Op::Record(b) => store.try_write(|o| {
                need(o.synced_get(b.sid)?.is_some())?;
                o.synced_put(b)?;
                Ok(())
            }),
            Op::Forget(sid) => store.try_write(|o| {
                o.synced_remove(*sid)?;
                Ok(())
            }),
            Op::Hash(pre) => {
                let Done::Hashed { hash, size, mtime } = self.fs.apply(a)? else { return Err(odd()) };
                self.crash_point()?;
                store.try_write(|o| Ok(o.local_hash_put(pre.id.0, size, mtime, &hash)?))
            }

            // ------------------------------------------------ transfers
            Op::Upload { local, to, parent, name, replace, link } => {
                store.try_write(|o| need(link_ok(o, *link)? && o.synced_get(*parent)?.is_some()))?;
                let mut src = self.fs.open_upload(a)?;
                let (size, mtime) = (src.size, src.mtime);
                // The file stays open (no writers) from here until recorded.
                let (row, sent, sent_len, mark) = match self
                    .copy_instead(store, &mut src, to, name, replace.is_none())
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
                        let mut reader = Hashing::new(&mut src.file);
                        let (row, sent) = self.cloud.upload(to, name, replace.as_ref(), size, &mut reader).await?;
                        // In the cloud's terms (see `cloud`): what it sent, if it transformed it.
                        (row, sent.unwrap_or_else(|| reader.hash.finalize()), reader.hash.len(), mark)
                    }
                };
                self.crash_point()?;
                drop(src);
                store.try_write(|o| Ok::<_, Fail>(o.upsert_item(&row, false)?))?;
                if row.qxh != Some(sent) || i64::try_from(sent_len).ok() != Some(size) || row.size != size {
                    return Err(Fail::Skip(Skip::ContentMismatch));
                }
                store.try_write(|o| {
                    o.local_hash_put(local.id.0, size, mtime, &sent)?;
                    let b = file_base(*parent, name, size, sent, local.id, mtime, &row);
                    link_row(o, *link, b)
                })?;
                // On demand: the file now is (or stays) a placeholder in sync
                // with what was sent. Best effort: if it changed meanwhile,
                // the next round sends it again.
                if self.fs.on_demand()
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
                let (expected, synced) = store.try_write(|o| {
                    need(link_ok(o, *link)? && o.synced_get(*parent)?.is_some())?;
                    Ok::<_, Fail>((o.get_item(&remote.id.0)?, synced_hash(o, *link)?))
                })?;
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
                    Some(_) => self.fs.replace_placeholder(a, &v, synced.as_ref())?,
                };
                let Done::Placed { entry, kept, .. } = placed else { return Err(odd()) };
                notes.kept_instead = replace.is_some() && kept.is_some() && a.old_copy() == OldCopy::DropIfSynced;
                self.crash_point()?;
                store.try_write(|o| {
                    let row = self.local_row(&entry, *to);
                    o.local_update(&row)?;
                    o.local_hash_put(row.file_id, row.size, row.mtime, &v.identity.hash)?;
                    let b =
                        file_base(*parent, name, row.size, v.identity.hash, LocalId(row.file_id), row.mtime, &expected);
                    link_row(o, *link, b)
                })
            }
            Op::Download { remote, to, parent, name, replace, link } => {
                let (expected, synced) = store.try_write(|o| {
                    need(link_ok(o, *link)? && o.synced_get(*parent)?.is_some())?;
                    Ok::<_, Fail>((o.get_item(&remote.id.0)?, synced_hash(o, *link)?))
                })?;
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
                    store.try_write(|o| {
                        o.note_local_deleted(old.id.0)?;
                        Ok::<_, Fail>(o.meta_set(PENDING, &Pending::Evidence(old.id).encode())?)
                    })?;
                    self.crash_point()?;
                }
                let entry = match self.fs.place_download(a, staged, synced.as_ref()) {
                    Ok(Done::Placed { entry, kept, .. }) => {
                        notes.kept_instead =
                            replace.is_some() && kept.is_some() && a.old_copy() == OldCopy::DropIfSynced;
                        entry
                    }
                    Ok(_) => return Err(odd()),
                    Err(e) => return Err(e.into()),
                };
                self.crash_point()?;
                store.try_write(|o| {
                    if let Some(old) = replace {
                        o.local_remove_subtree(old.id.0)?;
                        o.meta_delete(PENDING)?;
                    }
                    let row = self.local_row(&entry, *to);
                    o.local_update(&row)?;
                    o.local_hash_put(row.file_id, row.size, row.mtime, &digest)?;
                    let b = file_base(*parent, name, row.size, digest, LocalId(row.file_id), row.mtime, &expected);
                    link_row(o, *link, b)
                })
            }
            Op::MkdirRemote { .. } | Op::MoveRemote { .. } | Op::RenameRemote { .. } | Op::DeleteRemote { .. } => {
                self.meta_one(store, a.op()).await
            }
            Op::MkdirLocal { remote, to: _, parent, name, link } => {
                let row = store.try_write(|o| {
                    need(link_ok(o, *link)? && o.synced_get(*parent)?.is_some())?;
                    Ok::<_, Fail>(o.get_item(&remote.id.0)?)
                })?;
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
                store.try_write(|o| {
                    let local = self.local_row(&entry, dir);
                    o.local_update(&local)?;
                    link_row(o, *link, folder_base(*parent, name, LocalId(local.file_id), &row))
                })
            }

            // ------------------------------------------------ moves
            Op::MoveLocal { sid, local: _, to: _, parent, name } => {
                store.try_write(|o| need(o.synced_get(*sid)?.is_some()))?;
                let Done::Placed { parent: dir, entry, .. } = self.fs.apply(a)? else { return Err(odd()) };
                self.crash_point()?;
                store.try_write(|o| {
                    o.local_update(&self.local_row(&entry, dir))?;
                    let Some(mut b) = o.synced_get(*sid)? else { return Err(Fail::Skip(Skip::Changed)) };
                    b.parent = Some(*parent);
                    b.name.clone_from(name);
                    o.synced_put(&b)?;
                    Ok(())
                })
            }
            Op::RenameLocal { .. } => {
                let Done::Placed { parent: dir, entry, .. } = self.fs.apply(a)? else { return Err(odd()) };
                self.crash_point()?;
                store.try_write(|o| Ok(o.local_update(&self.local_row(&entry, dir))?))
            }
            Op::ConflictCopy { sid, local, name, .. } => {
                // Detach first (see the crate docs): the renamed copy must
                // never be taken for a rename of the Synced item.
                store.try_write(|o| {
                    let Some(b) = o.synced_get(*sid)? else { return Err(Fail::Skip(Skip::Changed)) };
                    need(b.local == Some(local.id))?;
                    o.synced_put(&Base { local: None, ..b })?;
                    let p = Pending::Detached { sid: *sid, local: local.id, name: name.clone() };
                    Ok(o.meta_set(PENDING, &p.encode())?)
                })?;
                self.crash_point()?;
                let Done::Placed { parent, entry, .. } = self.fs.apply(a)? else { return Err(odd()) };
                self.crash_point()?;
                store.try_write(|o| {
                    o.local_update(&self.local_row(&entry, parent))?;
                    Ok(o.meta_delete(PENDING)?)
                })
            }

            // ------------------------------------------------ deletions
            Op::RecycleLocal { sid, local } => {
                let synced = store.try_write(|o| {
                    let Some(b) = o.synced_get(*sid)? else { return Err(Fail::Skip(Skip::Changed)) };
                    o.note_local_deleted(local.id.0)?;
                    o.meta_set(PENDING, &Pending::Evidence(local.id).encode())?;
                    Ok::<_, Fail>(b.hash)
                })?;
                self.crash_point()?;
                match self.fs.recycle(a, synced.as_ref()) {
                    Ok(Done::Recycled { kept }) => {
                        notes.kept_instead = kept.is_some() && a.old_copy() == OldCopy::DropIfSynced;
                    }
                    Ok(_) => return Err(odd()),
                    Err(e) => return Err(e.into()),
                }
                self.crash_point()?;
                store.try_write(|o| {
                    o.local_remove_subtree(local.id.0)?;
                    o.synced_remove(*sid)?;
                    Ok(o.meta_delete(PENDING)?)
                })
            }
            Op::PruneRemote { remote, forget, keep, .. } => {
                // Only once the copy here is complete (data-safety.md invariant 1).
                for pre in keep {
                    need(self.fs.has_content(pre)?)?;
                }
                // The Synced rows go first (see the operation's docs): after
                // a crash here, nothing is left that could later read as a
                // cloud deletion to bring to this PC.
                store.try_write(|o| {
                    for sid in forget {
                        o.synced_remove(*sid)?;
                    }
                    Ok::<_, Fail>(())
                })?;
                self.crash_point()?;
                self.cloud.prune(remote).await?;
                self.crash_point()?;
                store.try_write(|o| {
                    o.delete_subtree(&remote.id.0)?;
                    Ok::<_, Fail>(())
                })?;
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
                let current = store.try_write(|o| {
                    let Some(b) = o.synced_by_local(local.id.0)? else { return Ok::<_, Fail>(false) };
                    let Some(r) = b.remote else { return Ok(false) };
                    Ok(o.get_item(&r.0)?.is_some_and(|row| row.qxh == Some(*hash)) && b.hash == Some(*hash))
                })?;
                need(current)?;
                let Done::Freed { entry } = self.fs.free_up(a)? else { return Err(odd()) };
                self.crash_point()?;
                store.try_write(|o| {
                    let Some(row) = o.local_get(local.id.0)? else { return Ok(()) };
                    let parent = row.parent_id.map(LocalId).unwrap_or(self.fs.root_id());
                    Ok(o.local_update(&self.local_row(&entry, parent))?)
                })
            }
            Op::Hold { .. } => Err(Fail::Failed("a held item can't be executed".into())),
        }
    }

    /// The Local tree row for an item the effects layer just placed.
    fn local_row(&self, entry: &oneup_effects::DirEntry, parent: LocalId) -> LocalRow {
        let rules = self.rules.read().unwrap_or_else(|p| p.into_inner());
        oneup_local::classify::to_row(entry, parent.0, parent == self.fs.root_id(), &rules)
    }

    /// Settles a write-ahead record whose operation didn't finish: it
    /// failed, or the process died (then this runs at the start of the next
    /// round). If the effect didn't happen, the record is undone; if it
    /// did, the record stands and the change journal reports the rest.
    /// Whether it happened is read from the folder itself, which, unlike
    /// the journal, can't lag behind.
    fn recover(&self, store: &mut Store) -> Res {
        let Some(p) = store.try_write(|o| Ok::<_, Fail>(o.meta_get(PENDING)?))? else { return Ok(()) };
        let pending = Pending::decode(&p);
        let undo = match &pending {
            // Recycling and replacing take the item out of the folder.
            Some(Pending::Evidence(id)) => self.fs.is_present(*id),
            // A conflict copy renames it in place.
            Some(Pending::Detached { local, name, .. }) => self.fs.name_of(*local).is_some_and(|n| n != *name),
            None => false,
        };
        tracing::debug!(undo, "settling an unfinished write-ahead record");
        store.try_write(|o| {
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
            Ok(o.meta_delete(PENDING)?)
        })
    }
}

/// The meta key holding the write-ahead record of the operation under way
/// (see the crate docs): set in the same transaction as the write-ahead,
/// cleared in the same transaction as the final record. Operations run one
/// at a time, so there is at most one.
const PENDING: &str = "exec.pending";

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

/// Whether `op` writes a record ahead of its effect (see [`PENDING`]).
fn writes_ahead(op: &Op) -> bool {
    matches!(op, Op::RecycleLocal { .. } | Op::ConflictCopy { .. } | Op::Download { replace: Some(_), .. })
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
}
