use std::collections::HashSet;
use std::future::Future;
use std::sync::atomic::{AtomicBool, AtomicU8, AtomicU64, Ordering::Relaxed};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

use oneup_graph::{DriveItem, Page};
use oneup_rules::ExcludeSet;
use oneup_store::{Cursor, CursorState, ItemState, Ops, Stats, Store, Unit, UnitKind};
use tokio::task::JoinSet;

use crate::hint::SplitHint;
use crate::ingest::{self, Mode, Tally};
use crate::{Error, Result};

/// The remote operations the mirror needs. Implemented by
/// [`oneup_graph::Graph`], and by an in-memory fake in tests.
pub trait RemoteApi: Send + Sync + 'static {
    fn root(&self) -> impl Future<Output = oneup_graph::Result<DriveItem>> + Send;
    /// `None` if the item no longer exists.
    fn item(&self, id: &str) -> impl Future<Output = oneup_graph::Result<Option<DriveItem>>> + Send;
    fn page(&self, url: &str) -> impl Future<Output = oneup_graph::Result<Page>> + Send;
    fn latest_cursor(&self) -> impl Future<Output = oneup_graph::Result<String>> + Send;
    fn children_url(&self, folder_id: &str) -> String;
    fn delta_by_id_url(&self, folder_id: &str) -> String;
    fn delta_by_path_url(&self, path: &str) -> String;
}

impl RemoteApi for oneup_graph::Graph {
    async fn root(&self) -> oneup_graph::Result<DriveItem> {
        oneup_graph::Graph::root(self).await
    }
    async fn item(&self, id: &str) -> oneup_graph::Result<Option<DriveItem>> {
        oneup_graph::Graph::item(self, id).await
    }
    async fn page(&self, url: &str) -> oneup_graph::Result<Page> {
        oneup_graph::Graph::page(self, url).await
    }
    async fn latest_cursor(&self) -> oneup_graph::Result<String> {
        oneup_graph::Graph::latest_cursor(self).await
    }
    fn children_url(&self, folder_id: &str) -> String {
        oneup_graph::Graph::children_url(self, folder_id)
    }
    fn delta_by_id_url(&self, folder_id: &str) -> String {
        oneup_graph::Graph::delta_by_id_url(self, folder_id)
    }
    fn delta_by_path_url(&self, path: &str) -> String {
        oneup_graph::Graph::delta_by_path_url(self, path)
    }
}

#[derive(Debug, Clone)]
pub struct Config {
    /// Levels always listed with `children` before splitting into delta units.
    pub list_depth: u32,
    /// Below `list_depth`, folders at least `split_bytes` large (Graph reports
    /// recursive size) keep being listed, down to this depth. Listing is how
    /// excluded subtrees get skipped without downloading their entries, and
    /// large folders are where dependency trees live, so this spends listing
    /// requests only where they're likely to pay off.
    pub max_list_depth: u32,
    pub split_bytes: i64,
    pub rules: ExcludeSet,
    /// Upper bound on concurrently running units (each holds at most one page).
    pub max_tasks: usize,
    /// Where excluded folders are expected. When present (and non-empty) it
    /// *replaces* the size heuristic below `list_depth`: a folder is listed
    /// iff the hint says it leads to an excluded folder, at any depth.
    pub hint: Option<Arc<SplitHint>>,
}

impl Config {
    /// How a child folder at `depth` with recursive `size` bytes, at drive
    /// path `path`, is enumerated. This is the *only* thing hints influence.
    fn unit_kind(&self, depth: u32, size: i64, path: &str) -> UnitKind {
        let list = depth < self.list_depth
            || match self.hint.as_deref().filter(|h| !h.is_empty()) {
                Some(h) => h.leads_to_excluded(path),
                None => depth < self.max_list_depth && size >= self.split_bytes,
            };
        if list { UnitKind::List } else { UnitKind::Delta }
    }
}

impl Default for Config {
    fn default() -> Self {
        Self {
            list_depth: 2,
            max_list_depth: 6,
            split_bytes: 256 << 20,
            rules: ExcludeSet::dev(),
            max_tasks: 32,
            hint: None,
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[repr(u8)]
pub enum Phase {
    Starting = 0,
    Enumerating = 1,
    ResolvingOrphans = 2,
    CatchingUp = 3,
    Verifying = 4,
    Done = 5,
}

/// Live counters for progress display.
#[derive(Debug, Default)]
pub struct Progress {
    phase: AtomicU8,
    pub units_done: AtomicU64,
    pub units_known: AtomicU64,
    pub pages: AtomicU64,
    pub entries: AtomicU64,
    pub stored: AtomicU64,
    pub excluded: AtomicU64,
    pub pruned: AtomicU64,
    pub deleted: AtomicU64,
}

impl Progress {
    pub fn phase(&self) -> Phase {
        match self.phase.load(Relaxed) {
            1 => Phase::Enumerating,
            2 => Phase::ResolvingOrphans,
            3 => Phase::CatchingUp,
            4 => Phase::Verifying,
            5 => Phase::Done,
            _ => Phase::Starting,
        }
    }

    fn set_phase(&self, p: Phase) {
        self.phase.store(p as u8, Relaxed);
    }

    fn record(&self, entries: usize, t: &Tally) {
        self.pages.fetch_add(1, Relaxed);
        self.entries.fetch_add(entries as u64, Relaxed);
        self.stored.fetch_add(t.stored, Relaxed);
        self.excluded.fetch_add(t.excluded, Relaxed);
        self.pruned.fetch_add(t.pruned, Relaxed);
        self.deleted.fetch_add(t.deleted, Relaxed);
    }
}

#[derive(Debug, Clone)]
pub struct Summary {
    pub stats: Stats,
    pub elapsed: Duration,
    pub fresh: bool,
}

pub struct Mirror<A> {
    api: Arc<A>,
    store: Arc<Mutex<Store>>,
    cfg: Arc<Config>,
    progress: Arc<Progress>,
    cancel: Arc<AtomicBool>,
}

fn now_ms() -> i64 {
    SystemTime::now().duration_since(UNIX_EPOCH).map(|d| d.as_millis() as i64).unwrap_or(0)
}

fn join_path(parent: &str, name: &str) -> String {
    if parent == "/" { format!("/{name}") } else { format!("{parent}/{name}") }
}

/// (Re)queues a folder-scoped delta unit for `id`, resetting any finished one.
fn requeue_enumeration(o: &Ops<'_>, id: &str) -> oneup_store::Result<()> {
    let path = o.path_of(id)?.unwrap_or_default();
    o.requeue_unit(&Unit {
        folder_id: id.to_string(),
        path,
        kind: UnitKind::Delta,
        depth: 1000,
        next_link: None,
        items: 0,
    })
}

impl<A: RemoteApi> Mirror<A> {
    pub fn new(api: Arc<A>, store: Store, cfg: Config) -> Self {
        Self {
            api,
            store: Arc::new(Mutex::new(store)),
            cfg: Arc::new(cfg),
            progress: Arc::new(Progress::default()),
            cancel: Arc::new(AtomicBool::new(false)),
        }
    }

    pub fn progress(&self) -> Arc<Progress> {
        Arc::clone(&self.progress)
    }

    /// Setting this flag stops the run at the next page boundary; all
    /// committed progress is kept.
    pub fn cancel_flag(&self) -> Arc<AtomicBool> {
        Arc::clone(&self.cancel)
    }

    /// Read access to the store (e.g. for status and tests).
    pub fn read<T>(&self, f: impl FnOnce(&Ops<'_>) -> oneup_store::Result<T>) -> Result<T> {
        let guard = self.store.lock().map_err(|_| Error::Task("store lock poisoned".into()))?;
        Ok(guard.read(f)?)
    }

    /// Gives the store back after [`Mirror::run`], so a sync round can hand
    /// it on to the planner and executor. Fails only if a task still holds
    /// it, which can't happen once `run` has returned.
    pub fn into_store(self) -> Result<Store> {
        let mutex = Arc::try_unwrap(self.store).map_err(|_| Error::Task("store still in use".into()))?;
        mutex.into_inner().map_err(|_| Error::Task("store lock poisoned".into()))
    }

    async fn db<T: Send + 'static>(
        store: &Arc<Mutex<Store>>,
        f: impl FnOnce(&mut Store) -> oneup_store::Result<T> + Send + 'static,
    ) -> Result<T> {
        let store = Arc::clone(store);
        tokio::task::spawn_blocking(move || {
            let mut guard = store.lock().map_err(|_| Error::Task("store lock poisoned".into()))?;
            f(&mut guard).map_err(Error::from)
        })
        .await
        .map_err(|e| Error::Task(e.to_string()))?
    }

    /// Brings the mirror up to date and checks the whole store's invariants.
    /// Safe to interrupt and re-run at any point.
    pub async fn run(&self) -> Result<Summary> {
        let started = Instant::now();
        let fresh = self.bring().await?;
        self.progress.set_phase(Phase::Verifying);
        let (problems, stats) = Self::db(&self.store, |s| s.read(|o| Ok((o.check_invariants()?, o.stats()?)))).await?;
        if !problems.is_empty() {
            return Err(Error::Inconsistent(problems.join("; ")));
        }
        self.progress.set_phase(Phase::Done);
        Ok(Summary { stats, elapsed: started.elapsed(), fresh })
    }

    /// Brings the mirror up to date without the checks [`Mirror::run`]
    /// ends with, which read every row of all three trees (hundreds of
    /// megabytes on a large drive). For the agent's rounds, whose cost must
    /// follow the changes. Returns whether it listed OneDrive from scratch.
    pub async fn update(&self) -> Result<bool> {
        let fresh = self.bring().await?;
        self.progress.set_phase(Phase::Done);
        Ok(fresh)
    }

    /// What [`Mirror::run`] and [`Mirror::update`] share: everything but
    /// the end.
    async fn bring(&self) -> Result<bool> {
        self.progress.set_phase(Phase::Starting);

        // Rules changed since the last run? Re-evaluate stored folders first.
        let rules = self.cfg.rules.clone();
        Self::db(&self.store, move |s| {
            s.write(|o| {
                let fp = rules.fingerprint();
                if o.meta_get("rules")?.is_some_and(|old| old != fp) {
                    let t = ingest::reapply_rules(o, &rules)?;
                    for id in &t.needs_enumeration {
                        requeue_enumeration(o, id)?;
                    }
                }
                o.meta_set("rules", &fp)
            })
        })
        .await?;

        let fresh = Self::db(&self.store, |s| s.read(|o| o.cursor())).await?.is_none();
        if fresh {
            // Cursor first: anything that changes from here on is replayed later.
            let link = self.api.latest_cursor().await?;
            let root = self.api.root().await?;
            let rules = self.cfg.rules.clone();
            Self::db(&self.store, move |s| {
                s.write(|o| {
                    o.clear_units()?;
                    o.set_cursor(&Cursor { delta_link: link, taken_at_ms: now_ms(), state: CursorState::Pending })?;
                    let mut t = Tally::default();
                    ingest::apply(o, &rules, Mode::Enumerate, &root, &mut t)?;
                    o.put_unit(&Unit {
                        folder_id: root.id.clone(),
                        path: "/".into(),
                        kind: UnitKind::List,
                        depth: 0,
                        next_link: None,
                        items: 0,
                    })
                })
            })
            .await?;
        }

        self.enumerate_until_settled().await?;
        self.catch_up().await?;
        Ok(fresh)
    }

    /// Runs pending units and resolves orphans until neither produces new work.
    async fn enumerate_until_settled(&self) -> Result<()> {
        loop {
            self.run_units().await?;
            self.resolve_orphans().await?;
            if !Self::db(&self.store, |s| s.read(|o| o.units_pending())).await? {
                return Ok(());
            }
        }
    }

    async fn run_units(&self) -> Result<()> {
        self.progress.set_phase(Phase::Enumerating);
        let mut running: HashSet<String> = HashSet::new();
        let mut set: JoinSet<(String, Result<()>)> = JoinSet::new();
        let mut first_error: Option<Error> = None;
        let mut recount = Throttle::new(UNIT_COUNT_PERIOD);
        loop {
            if first_error.is_none() && !self.cancel.load(Relaxed) && running.len() < self.cfg.max_tasks {
                // The running units are still pending, so the first
                // `max_tasks` hold every unit there's room to start.
                let max = self.cfg.max_tasks;
                // Counting the pending units costs O(pending), and this runs
                // once per finished unit: counted every time, an enumeration
                // of N units would be O(N²). The count only feeds a progress
                // estimate, so it's refreshed at most once a period.
                let count = recount.due(Instant::now());
                let (pending, waiting) = Self::db(&self.store, move |s| {
                    s.read(|o| {
                        let waiting = if count { Some(o.pending_unit_count()?) } else { None };
                        Ok((o.pending_units(max)?, waiting))
                    })
                })
                .await?;
                let done = self.progress.units_done.load(Relaxed);
                match waiting {
                    Some(waiting) => self.progress.units_known.store(done + waiting, Relaxed),
                    // Between counts, at least never show more done than known.
                    None => {
                        self.progress.units_known.fetch_max(done, Relaxed);
                    }
                }
                for unit in pending {
                    if running.len() >= self.cfg.max_tasks {
                        break;
                    }
                    if running.insert(unit.folder_id.clone()) {
                        let (api, store, cfg, progress, cancel) = (
                            Arc::clone(&self.api),
                            Arc::clone(&self.store),
                            Arc::clone(&self.cfg),
                            Arc::clone(&self.progress),
                            Arc::clone(&self.cancel),
                        );
                        set.spawn(async move {
                            let id = unit.folder_id.clone();
                            (id, run_unit(api, store, cfg, progress, cancel, unit).await)
                        });
                    }
                }
            }
            let Some(joined) = set.join_next().await else { break };
            let (id, result) = joined.map_err(|e| Error::Task(e.to_string()))?;
            running.remove(&id);
            match result {
                Ok(()) => {
                    self.progress.units_done.fetch_add(1, Relaxed);
                }
                Err(e) => {
                    // Stop scheduling, let in-flight units reach a page boundary.
                    self.cancel.store(true, Relaxed);
                    first_error.get_or_insert(e);
                }
            }
        }
        if let Some(e) = first_error {
            return Err(e);
        }
        if self.cancel.load(Relaxed) {
            return Err(Error::Cancelled);
        }
        Ok(())
    }

    async fn resolve_orphans(&self) -> Result<()> {
        self.progress.set_phase(Phase::ResolvingOrphans);
        for _round in 0..64 {
            let rules = self.cfg.rules.clone();
            let missing = Self::db(&self.store, move |s| {
                s.write(|o| {
                    let mut t = Tally::default();
                    ingest::resolve_local_orphans(o, &rules, &mut t)?;
                    for id in &t.needs_enumeration {
                        requeue_enumeration(o, id)?;
                    }
                    o.orphan_missing_parents()
                })
            })
            .await?;
            if missing.is_empty() {
                return Ok(());
            }
            for pid in missing {
                if self.cancel.load(Relaxed) {
                    return Err(Error::Cancelled);
                }
                let fetched = self.api.item(&pid).await?;
                let rules = self.cfg.rules.clone();
                Self::db(&self.store, move |s| {
                    s.write(|o| match fetched {
                        // The parent is gone, so whatever waited on it is gone too.
                        None => o.drop_orphans_of(&pid).map(|_| ()),
                        // A folder fetched on its own isn't part of any stream that
                        // delivers its contents, so it is always fully enumerated.
                        Some(item) => {
                            let mut t = Tally::default();
                            ingest::apply(o, &rules, Mode::CatchUp, &item, &mut t)?;
                            for id in &t.needs_enumeration {
                                requeue_enumeration(o, id)?;
                            }
                            Ok(())
                        }
                    })
                })
                .await?;
            }
        }
        Err(Error::Inconsistent("orphaned entries could not be placed after 64 rounds".into()))
    }

    /// Replays the delta cursor, enumerates folders that appeared with
    /// content, and advances the cursor.
    async fn catch_up(&self) -> Result<()> {
        self.progress.set_phase(Phase::CatchingUp);
        let cursor = Self::db(&self.store, |s| s.read(|o| o.cursor()))
            .await?
            .ok_or_else(|| Error::Inconsistent("no delta cursor".into()))?;
        let mut url = cursor.delta_link;
        let final_link = loop {
            if self.cancel.load(Relaxed) {
                return Err(Error::Cancelled);
            }
            let page = match self.api.page(&url).await {
                Ok(p) => p,
                Err(e) if e.status() == Some(410) => return Err(Error::ResyncRequired),
                Err(e) => return Err(e.into()),
            };
            let (next, delta) = (page.next_link.clone(), page.delta_link.clone());
            let n = page.value.len();
            let rules = self.cfg.rules.clone();
            let t = Self::db(&self.store, move |s| {
                s.write(|o| {
                    let mut t = Tally::default();
                    for it in &page.value {
                        ingest::apply(o, &rules, Mode::CatchUp, it, &mut t)?;
                    }
                    for id in &t.needs_enumeration {
                        requeue_enumeration(o, id)?;
                    }
                    Ok(t)
                })
            })
            .await?;
            self.progress.record(n, &t);
            match (next, delta) {
                (Some(n), _) => url = n,
                (None, Some(d)) => break d,
                (None, None) => return Err(Error::Inconsistent("delta page without next or delta link".into())),
            }
        };
        self.enumerate_until_settled().await?;
        Self::db(&self.store, move |s| {
            s.write(|o| {
                o.set_cursor(&Cursor { delta_link: final_link, taken_at_ms: now_ms(), state: CursorState::Current })
            })
        })
        .await
    }
}

/// Where folder `id` is now ("/a/b"), from its parents in OneDrive up to
/// the mirrored root. `None` if it or a folder above it no longer exists,
/// or it has left the mirrored root.
async fn current_path<A: RemoteApi>(api: &A, store: &Arc<Mutex<Store>>, id: &str) -> Result<Option<String>> {
    let root = Mirror::<A>::db(store, |s| s.read(|o| o.remote_root())).await?.map(|r| r.id);
    let mut names = Vec::new();
    let mut cur = id.to_string();
    for _ in 0..4096 {
        let Some(item) = api.item(&cur).await? else { return Ok(None) };
        if item.is_root() {
            if root.as_deref() != Some(item.id.as_str()) {
                return Ok(None);
            }
            names.reverse();
            return Ok(Some(format!("/{}", names.join("/"))));
        }
        let parent = item.parent_id().map(str::to_string);
        let (Some(name), Some(parent)) = (item.name, parent) else { return Ok(None) };
        names.push(name);
        cur = parent;
    }
    Err(Error::Inconsistent(format!("parent chain of {id} in OneDrive too deep or cyclic")))
}

async fn run_unit<A: RemoteApi>(
    api: Arc<A>,
    store: Arc<Mutex<Store>>,
    cfg: Arc<Config>,
    progress: Arc<Progress>,
    cancel: Arc<AtomicBool>,
    unit: Unit,
) -> Result<()> {
    let id = unit.folder_id.clone();
    let start = match unit.kind {
        UnitKind::List => api.children_url(&id),
        UnitKind::Delta => api.delta_by_id_url(&id),
    };
    let mut url = unit.next_link.clone().unwrap_or_else(|| start.clone());
    let mut first_page = unit.next_link.is_none();
    // Whether `url` is a link OneDrive gave us (saved or from the last page)
    // rather than a first page we built.
    let mut given_link = unit.next_link.is_some();
    let mut restarted = false;
    let mut tried_path = None;
    let mut repathed = false;
    loop {
        if cancel.load(Relaxed) {
            return Err(Error::Cancelled);
        }
        let page = match api.page(&url).await {
            Ok(p) => p,
            // The link expired (OneDrive documents 410 for that, and a unit
            // resumed days later meets it): start the unit over. Entries
            // already stored are stored again, which changes nothing.
            Err(e) if e.status() == Some(410) && given_link && !restarted => {
                restarted = true;
                let u = unit.clone();
                Mirror::<A>::db(&store, move |s| s.write(|o| o.requeue_unit(&u))).await?;
                (url, first_page, given_link, tried_path) = (start.clone(), true, false, None);
                continue;
            }
            // Id-addressed folder delta not accepted: use the path form (verified against OneDrive Personal).
            Err(e)
                if unit.kind == UnitKind::Delta
                    && first_page
                    && tried_path.is_none()
                    && matches!(e.status(), Some(400 | 501)) =>
            {
                url = api.delta_by_path_url(&unit.path);
                tried_path = Some(unit.path.clone());
                continue;
            }
            // Not found. Only a folder gone by its id is done with (catch-up
            // records the deletion). The path form uses the path the unit
            // was queued with, and the folder may have been renamed or moved
            // since: catch-up wouldn't bring its contents (they didn't
            // change), so the unit lists them from where the folder is now.
            Err(e) if e.status() == Some(404) => match current_path(&*api, &store, &id).await? {
                None => {
                    let fid = id.clone();
                    Mirror::<A>::db(&store, move |s| s.write(|o| o.finish_unit(&fid, 0))).await?;
                    return Ok(());
                }
                Some(path) if unit.kind == UnitKind::Delta && !repathed && tried_path.as_ref() != Some(&path) => {
                    repathed = true;
                    url = api.delta_by_path_url(&path);
                    (first_page, given_link, tried_path) = (true, false, Some(path));
                    continue;
                }
                Some(_) => return Err(e.into()),
            },
            Err(e) => return Err(e.into()),
        };
        first_page = false;
        given_link = true;
        let next = page.next_link.clone();
        let n = page.value.len();
        let (cfg2, fid, unit2) = (Arc::clone(&cfg), id.clone(), unit.clone());
        let next2 = next.clone();
        let t = Mirror::<A>::db(&store, move |s| {
            s.write(|o| {
                let mut t = Tally::default();
                for it in &page.value {
                    ingest::apply(o, &cfg2.rules, Mode::Enumerate, it, &mut t)?;
                }
                // A folder that comes back from a pruned/excluded state may have
                // had children dropped earlier in this same stream (delivery order
                // isn't guaranteed), so it gets its own enumeration.
                for fid in &t.needs_enumeration {
                    requeue_enumeration(o, fid)?;
                }
                if unit2.kind == UnitKind::List {
                    for it in &page.value {
                        let is_child_folder = it.is_folder() && !it.is_root() && it.parent_id() == Some(fid.as_str());
                        let empty = it.folder.as_ref().and_then(|f| f.child_count) == Some(0);
                        if is_child_folder && !empty && o.item_state(&it.id)? == ItemState::Included {
                            let depth = unit2.depth + 1;
                            let path = join_path(&unit2.path, it.name.as_deref().unwrap_or_default());
                            let kind = cfg2.unit_kind(depth, it.size.unwrap_or(0), &path);
                            o.put_unit(&Unit {
                                folder_id: it.id.clone(),
                                path,
                                kind,
                                depth,
                                next_link: None,
                                items: 0,
                            })?;
                        }
                    }
                }
                match &next2 {
                    Some(l) => o.set_unit_progress(&fid, l, n as i64)?,
                    None => o.finish_unit(&fid, n as i64)?,
                }
                Ok(t)
            })
        })
        .await?;
        progress.record(n, &t);
        match next {
            Some(l) => url = l,
            None => return Ok(()),
        }
    }
}

/// How often enumeration recounts its pending units for the progress figure.
const UNIT_COUNT_PERIOD: Duration = Duration::from_secs(1);

/// Lets something run at most once per `period`; the first call is always due.
#[derive(Debug)]
struct Throttle {
    period: Duration,
    last: Option<Instant>,
}

impl Throttle {
    fn new(period: Duration) -> Self {
        Self { period, last: None }
    }

    /// Whether the period has passed since the last time this returned `true`
    /// (and if so, starts a new period at `now`).
    fn due(&mut self, now: Instant) -> bool {
        let due = self.last.is_none_or(|last| now.saturating_duration_since(last) >= self.period);
        if due {
            self.last = Some(now);
        }
        due
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_throttle_is_due_first_then_once_a_period() {
        let mut t = Throttle::new(Duration::from_secs(1));
        let t0 = Instant::now();
        assert!(t.due(t0), "the first pass always counts");
        assert!(!t.due(t0));
        assert!(!t.due(t0 + Duration::from_millis(999)));
        assert!(t.due(t0 + Duration::from_secs(1)));
        assert!(!t.due(t0 + Duration::from_millis(1500)));
        assert!(t.due(t0 + Duration::from_secs(2)));
    }
}
