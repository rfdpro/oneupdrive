//! Search-only catalogs of fixed local volumes. Each catalog has its own
//! scanner, journal bookmark and trigram table, outside the sync database.

use std::collections::{HashMap, HashSet};
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

use anyhow::{Context, Result, bail};
use oneup_local::observe::{Error as ObserveError, Observer, Rescan};
use oneup_ntfs::win::{Volume, search_identity, search_listable};
use oneup_rules::{ExcludeSet, RuleSet};
use oneup_store::Store;
use serde_json::{Value, json};

const READY: &str = "drive_search.ready";
const PERIOD: Duration = Duration::from_secs(60);
/// The shortest wait between passes ([`pass_wait`]).
const SHORTEST_WAIT: Duration = Duration::from_secs(5);
/// A catalog is compacted only on a drive whose journal reaches back at
/// least this far: compacting rewrites the whole catalog (minutes for a
/// gigabyte) without reading the journal.
const COMPACT_HISTORY: Duration = Duration::from_secs(30 * 60);
/// How often building the name index stops to read the drive's journal.
const CATCH_UP_EVERY: Duration = Duration::from_secs(10);
const BATCH: usize = 8_192;
/// In a catalog's file name: its format. `v2` writes file IDs as
/// `KeyOrder::Ordered` keys; a catalog without it is an earlier one, which
/// the worker removes and scans again ([`remove_old_catalogs`]).
const FORMAT: &str = "v2";
/// Folders no catalog lists, wherever they are.
const ALWAYS_EXCLUDED: [&str; 3] = [".oneup", "$Recycle.Bin", "System Volume Information"];
/// In the data folder: Settings or `oneup search --compact-index` asked the
/// worker to compact the catalogs.
const COMPACT_REQUEST: &str = "drive-index-compact";
/// The longest a scan step lists folders before it writes them and follows
/// the journal again. Listing at background priority on a busy disk can be
/// slow, and a build can write a 32 MiB journal's worth of changes in a few
/// minutes: a step must not outlast that.
const STEP_LISTING: Duration = Duration::from_secs(10);

/// The user's exclusion rules that mean the same on every drive: the
/// presets and the unanchored lines of `[rules] exclude` (`*.iso`,
/// `node_modules/`). A line with a `/` before its end is anchored to the
/// synced folder and means nothing at a drive's root.
struct Exclusions {
    rules: RuleSet,
}

impl Exclusions {
    fn load(dir: &Path) -> Result<Self> {
        let rules = crate::config::load_rules_peeked(dir)?;
        let everywhere: Vec<String> = rules.exclude.into_iter().filter(|line| !is_anchored(line)).collect();
        Ok(Self { rules: RuleSet::new(&rules.presets, &everywhere)? })
    }

    /// The folders a scan leaves out by name.
    fn folders(&self) -> ExcludeSet {
        let names = self.rules.pruning().patterns();
        ExcludeSet::from_patterns(ALWAYS_EXCLUDED.into_iter().chain(names.iter().map(String::as_str)))
    }

    /// Whether a result at `rel` (from the drive's root) is excluded: file
    /// rules (`*.log`) and folders a scan can't leave out by name alone.
    fn hides(&self, rel: &str, folder: bool) -> bool {
        self.rules.excludes(rel, folder)
    }
}

fn is_anchored(line: &str) -> bool {
    let mut line = line.trim().trim_start_matches('!');
    // `**/name` matches anywhere, as `name` does.
    while let Some(rest) = line.strip_prefix("**/").or_else(|| line.strip_prefix("**\\")) {
        line = rest;
    }
    line.trim_end_matches(['/', '\\']).contains(['/', '\\'])
}

/// Removes the files of catalogs made in an earlier format (no
/// [`FORMAT`] in the name): the next pass scans those drives again. Our own
/// files in the data folder; one a search still has open goes next time.
fn remove_old_catalogs(dir: &Path) {
    let Ok(entries) = std::fs::read_dir(dir.join("drive-index")) else { return };
    for entry in entries.flatten() {
        if is_old_catalog(&entry.file_name().to_string_lossy()) && std::fs::remove_file(entry.path()).is_ok() {
            tracing::info!("drive index of an earlier format removed");
        }
    }
}

/// `{letter}-{serial:016x}-{id:032x}.db`, or its `-wal` or `-shm`.
fn is_old_catalog(name: &str) -> bool {
    let base = name.strip_suffix("-wal").or_else(|| name.strip_suffix("-shm")).unwrap_or(name);
    let Some(stem) = base.strip_suffix(".db") else { return false };
    let hex = |s: &str, n: usize| s.len() == n && s.bytes().all(|b| b.is_ascii_hexdigit());
    matches!(stem.split('-').collect::<Vec<_>>()[..], [letter, serial, id]
        if letter.chars().count() == 1 && hex(serial, 16) && hex(id, 32))
}

struct Drive {
    root: PathBuf,
    serial: u64,
    id: u128,
}

impl Drive {
    fn open(root: PathBuf) -> Result<Self> {
        let (serial, id) = search_identity(&root)?;
        Ok(Self { root, serial, id: id.0 })
    }

    fn db(&self, dir: &Path) -> PathBuf {
        let letter = self.root.to_string_lossy().chars().next().unwrap_or('_');
        dir.join("drive-index").join(format!("{letter}-{:016x}-{:032x}.{FORMAT}.db", self.serial, self.id))
    }

    fn config(&self, protected: &[(u64, u128)], exclusions: &Exclusions) -> oneup_local::Config {
        let excluded = protected.iter().filter(|(serial, _)| *serial == self.serial).map(|(_, id)| *id).collect();
        oneup_local::Config {
            list_budget: Some(STEP_LISTING),
            ..oneup_local::Config::new(exclusions.folders().with_folder_ids(excluded))
        }
    }

    fn matches_root(&self, root: &oneup_store::LocalRoot) -> bool {
        root.volume_serial == self.serial
            && root.root_file_id == self.id
            && crate::test_account::same_dir(Path::new(root.path.trim_start_matches(r"\\?\")), &self.root)
    }

    fn journal(&self) -> oneup_ntfs::win::Result<oneup_ntfs::win::JournalInfo> {
        Volume::open_containing(&self.root)?.query_journal()
    }
}

fn journal_covers(journal: &oneup_ntfs::win::JournalInfo, bookmark: Option<(u64, i64)>) -> bool {
    bookmark.is_some_and(|(id, cursor)| {
        id == journal.journal_id
            && (journal.first_usn.max(journal.lowest_valid_usn)..=journal.next_usn).contains(&cursor)
    })
}

fn drives() -> Vec<Drive> {
    let mut drives = Vec::new();
    for root in oneup_sys::disk::fixed_search_roots() {
        if let Ok(drive) = Drive::open(root)
            && !drives.iter().any(|other: &Drive| other.serial == drive.serial && other.id == drive.id)
        {
            drives.push(drive);
        }
    }
    drives
}

fn listable(path: &Path, cache: &mut HashMap<PathBuf, bool>) -> bool {
    let ancestors: Vec<_> = path.ancestors().skip(1).collect();
    for parent in ancestors.into_iter().rev() {
        let ok = cache.get(parent).copied().unwrap_or_else(|| {
            let ok = search_listable(parent);
            if cache.len() < 512 {
                cache.insert(parent.to_path_buf(), ok);
            }
            ok
        });
        if !ok {
            return false;
        }
    }
    true
}

/// Fail closed when vault locations cannot be established. The database
/// roots themselves are omitted to keep logs, keys, and catalog writes out.
pub(super) fn protected_roots(dir: &Path) -> Result<Vec<(u64, u128)>> {
    let mut dirs = crate::datafolders::all_strict()?;
    dirs.push(dir.to_path_buf());
    dirs.extend(crate::vaultpair::list_strict(dir)?.into_iter().map(|entry| entry.dir));
    let mut protected = Vec::new();
    for dir in dirs {
        remember_root(&mut protected, &dir)?;
        if crate::vaultpair::is_vault_dir(&dir) {
            let path = Store::peek_meta(&dir.join("state.db"), crate::sync::KEY_LOCAL)?;
            if let Some(path) = path {
                remember_root(&mut protected, Path::new(&path))?;
            }
        }
    }
    protected.sort_unstable();
    protected.dedup();
    Ok(protected)
}

fn remember_root(ids: &mut Vec<(u64, u128)>, path: &Path) -> Result<()> {
    match std::fs::canonicalize(path) {
        Ok(path) => {
            let (serial, id) = search_identity(&path)?;
            ids.push((serial, id.0));
        }
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
        Err(error) => return Err(error.into()),
    }
    Ok(())
}

pub(super) fn signal(dir: &Path) -> String {
    crate::tray::instance_name(dir).replace("-tray-", "-drive-index-")
}

fn worker_lock(dir: &Path) -> String {
    format!("{}-worker", signal(dir))
}

/// Stop for an app update or removal, retaining the user's setting.
pub fn request_stop(dir: &Path) {
    super::shares::request_stop(dir);
    let name = signal(dir);
    let _ = oneup_sys::signal::raise(&format!("{name}-stop"));
    let _ = oneup_sys::signal::raise(&name);
}

pub fn running(dir: &Path) -> bool {
    oneup_sys::tray::single_instance(&worker_lock(dir)).is_ok_and(|lock| lock.is_none())
}

/// Wake an existing worker, or start it if requested. Configuration is
/// committed before the event is raised.
pub fn wake(dir: &Path) -> Result<()> {
    let settings = crate::config::load_search(dir)?;
    if !settings.network_shares || settings.index_paused {
        super::shares::request_stop(dir);
    }
    if oneup_sys::signal::raise(&signal(dir))? {
        return Ok(());
    }
    if (settings.local_drives || settings.network_shares) && !settings.index_paused {
        // Our own file in the data folder: the last run's output is kept.
        let log = dir.join(LOG_FILE);
        if log.exists() {
            let _ = std::fs::rename(&log, dir.join(format!("{LOG_FILE}.1")));
        }
        let out = std::fs::File::create(&log).with_context(|| format!("creating {}", log.display()))?;
        let mut cmd = Command::new(crate::launch::sibling("oneup.exe")?);
        cmd.arg("--data-dir")
            .arg(dir)
            // Info: why a drive scans again, once per scan.
            .arg("-v")
            .arg("drive-index-worker")
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(out);
        crate::launch::spawn_outside_job(&mut cmd, 0x0800_0000 | 0x0000_0200)?;
    }
    Ok(())
}

pub fn worker(dir: &Path) -> Result<()> {
    let event = oneup_sys::signal::Signal::new(&signal(dir))?;
    let stop = oneup_sys::signal::Signal::new(&format!("{}-stop", signal(dir)))?;
    let Some(_lock) = oneup_sys::tray::single_instance(&worker_lock(dir))? else {
        return Ok(());
    };
    let stopped = std::cell::Cell::new(false);
    let shares = std::cell::RefCell::new(super::shares::Workers::default());
    let next_share_check = std::cell::Cell::new(Instant::now() + PERIOD);
    let check_shares = |enabled| {
        shares.borrow_mut().reconcile(dir, enabled);
        next_share_check.set(Instant::now() + PERIOD);
    };
    let running = || {
        stopped.set(stopped.get() || stop.wait(Duration::ZERO));
        // A first scan can take several rounds of network discovery.
        if !stopped.get() && Instant::now() >= next_share_check.get() {
            check_shares(crate::config::load_search(dir).is_ok_and(|s| s.network_shares && !s.index_paused));
        }
        !stopped.get()
    };
    // What the status file shows, the last whole pass, and the last failure
    // logged per drive (each is logged once, not every minute).
    let mut shown: Option<Vec<Value>> = None;
    let mut last = Vec::new();
    let mut errors = HashMap::new();
    while running() {
        let settings = crate::config::load_search(dir)?;
        if !(settings.local_drives || settings.network_shares) || settings.index_paused {
            return Ok(());
        }
        let _ = oneup_sys::priority::set_background(crate::config::load_agent(dir).map_or(true, |a| a.background));
        check_shares(settings.network_shares);
        let mut show = |status: &[Value]| -> Result<()> {
            if shown.as_deref() != Some(status) {
                crate::ownfile::write(&dir.join(STATUS_FILE), &serde_json::to_vec(status)?)?;
                shown = Some(status.to_vec());
            }
            Ok(())
        };
        let status = if settings.local_drives {
            update(dir, &running, &last, &mut errors, &mut show).unwrap_or_else(|e| {
                note_error(&mut errors, "", Some(&e));
                vec![json!({ "drive": null, "status": crate::t_key!("Index unavailable") })]
            })
        } else {
            Vec::new()
        };
        if running() {
            show(&status)?;
            last = status;
            let wait = if settings.local_drives {
                pass_wait(drives().iter().filter_map(|d| journal_history(&d.root)))
            } else {
                PERIOD
            };
            event.wait(wait);
        }
    }
    Ok(())
}

/// In the data folder: what the worker last found, per drive.
const STATUS_FILE: &str = "drive-index-status.json";
/// In the data folder: the worker's output. The previous run's is kept as
/// `drive-index.log.1`.
const LOG_FILE: &str = "drive-index.log";

/// One pass over the drives. While a drive is scanned, `show` gets the
/// drives done so far, that drive as indexing, and the rest as `last` had them.
fn update(
    dir: &Path,
    running: &impl Fn() -> bool,
    last: &[Value],
    errors: &mut HashMap<String, String>,
    show: &mut impl FnMut(&[Value]) -> Result<()>,
) -> Result<Vec<Value>> {
    let protected = protected_roots(dir)?;
    let exclusions = Exclusions::load(dir)?;
    let cache_kib = catalog_cache_kib(dir);
    remove_old_catalogs(dir);
    let compact = dir.join(COMPACT_REQUEST).exists();
    note_error(errors, "", None);
    let mut status = Vec::new();
    let all = drives();
    for (i, drive) in all.iter().enumerate() {
        if !running() {
            break;
        }
        let name = drive.root.to_string_lossy().into_owned();
        let state = if protected.contains(&(drive.serial, drive.id)) {
            crate::t_key!("Index unavailable")
        } else if drive.journal().is_err() {
            crate::t_key!("Change journal unavailable")
        } else {
            let keep_going = || {
                running()
                    && crate::config::load_search(dir).is_ok_and(|s| s.local_drives && !s.index_paused)
                    && protected_roots(dir).is_ok_and(|now| now == protected)
            };
            let scanning = || show(&interim(&status, &name, last, crate::t_key!("Indexing…")));
            let cfg = drive.config(&protected, &exclusions);
            let mut others = Others::new(dir, &all, i, &protected, &exclusions, cache_kib);
            let between_steps = || others.follow(&keep_going);
            let result =
                update_drive(dir, drive, cfg, cache_kib, keep_going, scanning, between_steps).and_then(|updated| {
                    // A catalog still being scanned is compacted when its scan ends.
                    if compact && updated == Updated::Ready && running() {
                        show(&interim(&status, &name, last, crate::t_key!("Compacting…")))?;
                        let db = drive.db(dir);
                        let mut follow = || others.follow(&keep_going);
                        compact_following(Store::open(&db, cache_kib)?, &db, &drive.root, &mut follow)?;
                    }
                    Ok(updated)
                });
            note_error(errors, &name, result.as_ref().err());
            match result {
                Ok(Updated::Ready) => crate::t_key!("Ready"),
                Ok(Updated::Stopped) => crate::t_key!("Indexing paused"),
                Ok(Updated::Rescan) => crate::t_key!("Indexing…"),
                Err(_) => crate::t_key!("Index unavailable"),
            }
        };
        status.push(json!({ "drive": name, "status": state }));
    }
    if compact && running() {
        let _ = std::fs::remove_file(dir.join(COMPACT_REQUEST));
    }
    if status.is_empty() {
        status.push(json!({ "drive": null, "status": crate::t_key!("No supported local drives") }));
    }
    Ok(status)
}

/// The catalogs' page cache: the memory profile's (`[agent]`), which a scan
/// raises while it runs (`oneup_local::scan_cache_kib`).
fn catalog_cache_kib(dir: &Path) -> u32 {
    crate::config::load_agent(dir).map_or(16 << 10, |a| a.cache_kib())
}

/// Compacts a catalog if the disk has room for the copy: SQLite documents
/// that VACUUM may need up to twice the database (a temporary copy, then the
/// rewrite in the WAL). Too little room is no failure: it's logged, and the
/// catalog is left as it is.
fn compact_catalog(store: &Store, db: &Path, drive: &Path) -> Result<()> {
    let history = journal_history(drive);
    if !compaction_fits(history) {
        tracing::info!(
            drive = %drive.display(),
            journal_minutes = history.map_or(0, |h| h.as_secs() / 60),
            "drive index not compacted: its journal doesn't reach back long enough"
        );
        return Ok(());
    }
    let space = store.space()?;
    let need = store.files_size().max(space.total).saturating_mul(2);
    let room = oneup_sys::disk::free_bytes(db.parent().unwrap_or(db)).unwrap_or(0);
    if room < need {
        tracing::info!(drive = %drive.display(), room, need, "not enough free disk to compact the drive index");
        return Ok(());
    }
    let c = store.compact()?;
    tracing::info!(drive = %drive.display(), before = c.before, after = c.after, pending = c.pending, "drive index compacted");
    Ok(())
}

/// [`compact_catalog`] on a thread of its own, while `between` follows the
/// other drives' journals: compacting a large catalog takes minutes, longer
/// than a busy drive's journal may keep its records.
fn compact_following(store: Store, db: &Path, drive: &Path, between: &mut impl FnMut()) -> Result<()> {
    use std::sync::mpsc::RecvTimeoutError;
    std::thread::scope(|scope| {
        let (tx, rx) = std::sync::mpsc::channel();
        scope.spawn(move || {
            let _ = tx.send(compact_catalog(&store, db, drive));
        });
        loop {
            match rx.recv_timeout(SHORTEST_WAIT) {
                Ok(done) => return done,
                Err(RecvTimeoutError::Timeout) => between(),
                Err(RecvTimeoutError::Disconnected) => bail!("the drive index's compaction stopped"),
            }
        }
    })
}

/// Whether a catalog can be compacted without its drive's journal turning
/// over meanwhile: compacting doesn't read the journal, and a catalog whose
/// journal lost records must be scanned again. Unknown: as before.
fn compaction_fits(history: Option<Duration>) -> bool {
    history.is_none_or(|h| h >= COMPACT_HISTORY)
}

/// How far back a drive's change journal reaches: the age of its oldest
/// record. A system drive's 32 MiB can hold as little as a minute or two
/// (measured: test runs in its temp folder writing 3,800 records a second).
/// `None` if the journal can't be read.
fn journal_history(root: &Path) -> Option<Duration> {
    let volume = Volume::open_containing(root).ok()?;
    let j = volume.query_journal().ok()?;
    let mut buf = vec![0u8; 4096];
    let (_, mut records) =
        volume.read_journal(j.journal_id, j.first_usn.max(j.lowest_valid_usn), false, &mut buf).ok()?;
    let oldest = records.next()?.ok()?.timestamp;
    // FILETIME: 100 ns ticks since 1601.
    let now = SystemTime::now().duration_since(UNIX_EPOCH).ok()?.as_nanos() / 100 + 116_444_736_000_000_000;
    let ticks = u64::try_from(i128::try_from(now).ok()? - i128::from(oldest)).ok()?;
    Some(Duration::from_nanos(ticks.saturating_mul(100)))
}

/// The wait between passes: a quarter of the shortest reach of the drives'
/// journals, so each is read well before it turns over, from
/// [`SHORTEST_WAIT`] to [`PERIOD`].
fn pass_wait(histories: impl IntoIterator<Item = Duration>) -> Duration {
    histories.into_iter().min().map_or(PERIOD, |h| (h / 4).clamp(SHORTEST_WAIT, PERIOD))
}

/// `done`, then `drive` as `state`, then the drives after it as `last` had them.
fn interim(done: &[Value], drive: &str, last: &[Value], state: &str) -> Vec<Value> {
    let mut out = done.to_vec();
    out.push(json!({ "drive": drive, "status": state }));
    let listed: HashSet<String> = out.iter().filter_map(|v| v["drive"].as_str().map(str::to_owned)).collect();
    out.extend(last.iter().filter(|v| v["drive"].as_str().is_some_and(|d| !listed.contains(d))).cloned());
    out
}

/// Logs a drive's failure once, until it fails differently or recovers.
/// Paths in it name only the drive, never a folder on it.
fn note_error(errors: &mut HashMap<String, String>, drive: &str, error: Option<&anyhow::Error>) {
    let Some(error) = error else {
        errors.remove(drive);
        return;
    };
    let text = format!("{error:#}");
    if errors.get(drive) != Some(&text) {
        tracing::warn!(drive, error = %text, "drive index unavailable");
        errors.insert(drive.to_owned(), text);
    }
}

/// Where one update left a drive's catalog.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Updated {
    Ready,
    /// Stopped before it was done: paused, or the settings changed.
    Stopped,
    /// Its scan ended, but the journal lost the last step's changes: the
    /// next update scans again.
    Rescan,
}

/// Brings a drive's catalog up to date: from the journal, or else by a
/// scan, after which it's compacted. `scanning` runs before a scan starts,
/// `between_steps` after each of its steps.
fn update_drive(
    dir: &Path,
    drive: &Drive,
    cfg: oneup_local::Config,
    cache_kib: u32,
    keep_going: impl Fn() -> bool,
    scanning: impl FnOnce() -> Result<()>,
    mut between_steps: impl FnMut(),
) -> Result<Updated> {
    if !keep_going() {
        return Ok(Updated::Stopped);
    }
    let db = drive.db(dir);
    std::fs::create_dir_all(db.parent().context("catalog parent")?)?;
    let mut store = open_catalog(&db, cache_kib)?;
    if let Some(root) = store.read(|o| o.local_root())?
        && !drive.matches_root(&root)
    {
        bail!("The volume identity changed");
    }
    // Changed exclusions (or vaults) adjust the finished tree in place.
    if let Some(r) = store.write(|o| oneup_local::observe::reapply_rules(o, &cfg.rules))? {
        tracing::info!(
            drive = %drive.root.display(), excluded = r.excluded, included = r.included,
            "drive index follows the changed exclusions"
        );
    }
    let needs_scan = match Observer::open(store, cfg.clone()) {
        Ok(mut observer) => match observer
            .catch_up_while(&keep_going)
            .and_then(|_| finish(&mut observer, &keep_going, &mut between_steps))
        {
            Ok(updated) => return Ok(updated),
            Err(ObserveError::NeedsRescan(why)) => {
                tracing::info!(drive = %drive.root.display(), %why, "drive index scans again");
                if journal_lost(why) {
                    drop(observer);
                    start_over(&db, cache_kib)?.0
                } else {
                    observer.into_store()
                }
            }
            Err(err) => return Err(err.into()),
        },
        Err(ObserveError::NeedsRescan(Rescan::OlderScanner | Rescan::RulesChanged)) => start_over(&db, cache_kib)?.0,
        // Nothing scanned yet, a scan to carry on, or a volume that may be
        // readable again later: the scan decides, and keeps the catalog
        // until it can start.
        Err(ObserveError::NeedsRescan(_)) => open_catalog(&db, cache_kib)?,
        Err(err) => return Err(err.into()),
    };
    scanning()?;
    let mut store = needs_scan;
    store.write(|o| o.meta_set(READY, "0"))?;
    let mut scan = oneup_local::Scan::open(store, &drive.root, cfg.clone())?;
    same_volume(drive, &scan)?;
    let mut log = ScanLog::new(&drive.root, &scan);
    while keep_going() {
        // A whole drive can take longer to list than its journal keeps
        // history. Replaying the journal only after the scan would then find
        // it wrapped, and the next update would start over, without end.
        let following = Instant::now();
        match scan.follow_journal(&keep_going) {
            Ok(ch) => log.followed(ch.records, following.elapsed()),
            Err(ObserveError::NeedsRescan(why)) => {
                tracing::info!(drive = %drive.root.display(), %why, "drive index scan starts over");
                scan = if journal_lost(why) {
                    drop(scan);
                    let (store, fresh) = start_over(&db, cache_kib)?;
                    let scan = oneup_local::Scan::open(store, &drive.root, cfg.clone())?;
                    if fresh { scan } else { scan.restart()? }
                } else {
                    scan.restart()?
                };
                same_volume(drive, &scan)?;
                log = ScanLog::new(&drive.root, &scan);
                continue;
            }
            Err(err) => return Err(err.into()),
        }
        let more = scan.step()?;
        log.note(&scan, !more);
        between_steps();
        if !more {
            let mut observer = Observer::open(scan.into_store(), cfg)?;
            return match observer
                .catch_up_while(&keep_going)
                .and_then(|_| finish(&mut observer, &keep_going, &mut between_steps))
            {
                Ok(updated) => {
                    // Pages a scan fills in no order are part empty: the
                    // rewrite packs them.
                    if updated == Updated::Ready
                        && let Err(e) = compact_following(observer.into_store(), &db, &drive.root, &mut between_steps)
                    {
                        tracing::warn!(drive = %drive.root.display(), error = %e, "drive index not compacted");
                    }
                    Ok(updated)
                }
                Err(ObserveError::NeedsRescan(why)) => {
                    tracing::info!(drive = %drive.root.display(), %why, "drive index scans again");
                    Ok(Updated::Rescan)
                }
                Err(err) => Err(err.into()),
            };
        }
    }
    Ok(Updated::Stopped)
}

/// The journal no longer has the records a catalog needs: only a scan from
/// scratch brings it back.
fn journal_lost(why: Rescan) -> bool {
    matches!(why, Rescan::JournalWrapped | Rescan::JournalReplaced)
}

/// A scan opened on a new catalog takes whatever volume is at the drive's
/// root: it must be the one this catalog is named for.
fn same_volume(drive: &Drive, scan: &oneup_local::Scan) -> Result<()> {
    if !drive.matches_root(scan.root()) {
        bail!("The volume identity changed");
    }
    Ok(())
}

/// Opens a drive's catalog, made if there's none: file IDs sorted as the
/// MFT is, so a scan's rows land on few B-tree pages (`KeyOrder::Ordered`).
fn open_catalog(db: &Path, cache_kib: u32) -> Result<Store> {
    let mut store = Store::open(db, cache_kib)?;
    if store.read(|o| o.local_root())?.is_none() {
        match store.use_ordered_keys() {
            Ok(()) | Err(oneup_store::Error::KeysInUse) => {}
            Err(e) => return Err(e.into()),
        }
    }
    store.install_drive_search()?;
    Ok(store)
}

/// A new, empty catalog in place of the one at `db`, for a scan from
/// scratch: emptying the old one row by row took 8 to 15 minutes for a
/// large drive (every row firing the catalog's triggers) and wrote the
/// whole table out again through the WAL. The caller has closed its own
/// connection. Also says whether the file is new: it isn't while the old
/// one is still open elsewhere (a search), and the scan empties it then.
fn start_over(db: &Path, cache_kib: u32) -> Result<(Store, bool)> {
    // Our own files in the data folder. Windows refuses the rename while
    // anyone has the database open; only once it's moved are its -wal and
    // -shm free to go, and a new database must never meet the old WAL. The
    // -shm (an index SQLite rebuilds) goes first: if the WAL then can't,
    // the old database is put back with its WAL, which may hold commits
    // its file doesn't have yet.
    let old = db.with_extension("old");
    let _ = std::fs::remove_file(&old);
    let sibling = |suffix: &str| {
        let mut path = db.as_os_str().to_owned();
        path.push(suffix);
        PathBuf::from(path)
    };
    let gone = |path: PathBuf| match std::fs::remove_file(path) {
        Ok(()) => true,
        Err(e) => e.kind() == std::io::ErrorKind::NotFound,
    };
    let fresh = std::fs::rename(db, &old).is_ok() && {
        let cleared = gone(sibling("-shm")) && gone(sibling("-wal"));
        if !cleared {
            let _ = std::fs::rename(&old, db);
        }
        cleared
    };
    if fresh {
        let _ = std::fs::remove_file(&old);
    } else {
        tracing::info!("drive index in use: emptied in place");
    }
    Ok((open_catalog(db, cache_kib)?, fresh))
}

/// How often a running scan says how it goes, in the worker's log.
const SCAN_LOG_EVERY: Duration = Duration::from_secs(300);

/// A scan's progress for the log: how fast it lists, where its time goes
/// (listing folders, writing the catalog, following the journal) and how
/// much of the journal it followed. A drive only by its root.
struct ScanLog {
    drive: String,
    progress: std::sync::Arc<oneup_local::Progress>,
    started: Instant,
    logged: Instant,
    entries_logged: u64,
    follow: Duration,
    records: u64,
}

impl ScanLog {
    fn new(drive: &Path, scan: &oneup_local::Scan) -> Self {
        let now = Instant::now();
        Self {
            drive: drive.display().to_string(),
            progress: scan.progress(),
            started: now,
            logged: now,
            entries_logged: 0,
            follow: Duration::ZERO,
            records: 0,
        }
    }

    fn followed(&mut self, records: u64, took: Duration) {
        self.records += records;
        self.follow += took;
    }

    /// After a step; `done`: the scan's last.
    fn note(&mut self, scan: &oneup_local::Scan, done: bool) {
        use std::sync::atomic::Ordering::Relaxed;
        if !done && self.logged.elapsed() < SCAN_LOG_EVERY {
            return;
        }
        let p = &self.progress;
        let entries = p.entries.load(Relaxed);
        let secs = self.logged.elapsed().as_secs_f64().max(0.001);
        tracing::info!(
            drive = %self.drive,
            start = ?scan.start(),
            entries,
            folders = p.folders_listed.load(Relaxed),
            per_second = ((entries - self.entries_logged) as f64 / secs).round(),
            minutes = (self.started.elapsed().as_secs_f64() / 60.0).round(),
            listing_s = p.list_us.load(Relaxed) / 1_000_000,
            writing_s = p.store_us.load(Relaxed) / 1_000_000,
            journal_s = self.follow.as_secs(),
            journal_records = self.records,
            "{}",
            if done { "drive index scan done" } else { "drive index scanning" }
        );
        self.logged = Instant::now();
        self.entries_logged = entries;
    }
}

/// How often a scan stops to catch up the other drives' catalogs, at most:
/// more often when one's journal turns over fast ([`pass_wait`]).
const FOLLOW_OTHERS: Duration = Duration::from_secs(30);
/// The longest one such stop takes: the scanned drive's own journal waits
/// meanwhile. A catalog not caught up by then goes on at the next stop.
const FOLLOW_OTHERS_FOR: Duration = Duration::from_secs(10);

/// The other drives' ready catalogs, kept current from their journals while
/// one drive is scanned. A first scan can take hours, and a system drive can
/// turn its 32 MiB journal over in minutes (measured: 70 to 170 KiB of
/// records a second with a browser and a build running): a catalog left that
/// long would need a scan of its own once its turn came. Opened when the
/// scan first asks, so a pass that scans nothing opens nothing. One that
/// can't be followed (not ready, or its journal lost records) is left for
/// its turn.
struct Others<'a> {
    dir: &'a Path,
    drives: Vec<(&'a Drive, oneup_local::Config)>,
    cache_kib: u32,
    observers: Option<Vec<(String, Observer)>>,
    next: Instant,
    every: Duration,
}

impl<'a> Others<'a> {
    fn new(
        dir: &'a Path,
        all: &'a [Drive],
        scanned: usize,
        protected: &[(u64, u128)],
        exclusions: &Exclusions,
        cache_kib: u32,
    ) -> Self {
        let drives = all
            .iter()
            .enumerate()
            .filter(|&(i, d)| i != scanned && !protected.contains(&(d.serial, d.id)))
            .map(|(_, d)| (d, d.config(protected, exclusions)))
            .collect();
        Self { dir, drives, cache_kib, observers: None, next: Instant::now(), every: FOLLOW_OTHERS }
    }

    fn follow(&mut self, keep_going: &impl Fn() -> bool) {
        if Instant::now() < self.next || !keep_going() {
            return;
        }
        let (dir, cache_kib) = (self.dir, self.cache_kib);
        let observers = self.observers.get_or_insert_with(|| {
            self.drives.iter().filter_map(|(drive, cfg)| Self::open(dir, drive, cfg.clone(), cache_kib)).collect()
        });
        let started = Instant::now();
        let in_time = || keep_going() && started.elapsed() < FOLLOW_OTHERS_FOR;
        observers.retain_mut(|(name, observer)| {
            let caught = observer
                .catch_up_while(in_time)
                .map_err(anyhow::Error::from)
                .and_then(|_| flush(observer.store_mut(), &in_time));
            caught
                .inspect_err(
                    |e| tracing::info!(drive = %name, error = %format!("{e:#}"), "drive index left for its turn"),
                )
                .is_ok()
        });
        // A different drive first next time, so a long backlog can't keep
        // the others waiting.
        if !observers.is_empty() {
            observers.rotate_left(1);
        }
        let every = pass_wait(observers.iter().filter_map(|(name, _)| journal_history(Path::new(name))));
        self.next = Instant::now() + self.every.min(every);
    }

    /// A drive's catalog, if it has a finished scan of this drive to follow.
    fn open(dir: &Path, drive: &Drive, cfg: oneup_local::Config, cache_kib: u32) -> Option<(String, Observer)> {
        let db = drive.db(dir);
        if !db.exists() {
            return None;
        }
        let store = Store::open(&db, cache_kib).ok()?;
        if !store.read(|o| o.local_root()).ok()?.is_some_and(|root| drive.matches_root(&root)) {
            return None;
        }
        let observer = Observer::open(store, cfg).ok()?;
        Some((drive.root.display().to_string(), observer))
    }
}

/// Brings the name index up to date and marks the catalog ready. Its first
/// build after a scan adds millions of names, minutes of work: it stops
/// every [`CATCH_UP_EVERY`] to read the drive's journal, which a busy system
/// drive can turn over in a minute or two, and lets `between` follow the
/// other drives'.
fn finish(
    observer: &mut Observer,
    keep_going: &impl Fn() -> bool,
    between: &mut impl FnMut(),
) -> oneup_local::observe::Result<Updated> {
    let mut caught = Instant::now();
    while keep_going() && observer.store_mut().flush_drive_search(BATCH)? == BATCH {
        // The other drives' journals too (`Others`: no more often than they need).
        between();
        if caught.elapsed() >= CATCH_UP_EVERY {
            observer.catch_up_while(keep_going)?;
            caught = Instant::now();
        }
    }
    let store = observer.store_mut();
    if keep_going() && store.read(|o| o.meta_get(READY))?.as_deref() != Some("1") {
        store.write(|o| o.meta_set(READY, "1"))?;
    }
    Ok(if keep_going() { Updated::Ready } else { Updated::Stopped })
}

fn flush(store: &mut Store, keep_going: &impl Fn() -> bool) -> Result<()> {
    while keep_going() && store.flush_drive_search(BATCH)? == BATCH {}
    Ok(())
}

pub fn status(dir: &Path) -> Result<Value> {
    let settings = crate::config::load_search(dir)?;
    let mut drives: Vec<Value> = std::fs::read(dir.join(STATUS_FILE))
        .ok()
        .and_then(|data| serde_json::from_slice(&data).ok())
        .unwrap_or_default();
    if !settings.local_drives {
        drives.clear();
    }
    if settings.network_shares {
        drives.extend(super::shares::status(dir, settings.index_paused));
    }
    let (index_bytes, free_bytes) = catalogs_space(dir);
    Ok(json!({
        "enabled": settings.local_drives || settings.network_shares, "paused": settings.index_paused, "drives": drives,
        "index_bytes": index_bytes, "free_bytes": free_bytes, "compacting": dir.join(COMPACT_REQUEST).exists()
    }))
}

/// The catalogs' files on disk (database and WAL), and what compacting them
/// would free at least: their free pages. Pages a scan left part full come
/// on top, so a first compaction frees more.
fn catalogs_space(dir: &Path) -> (u64, u64) {
    let (mut bytes, mut free) = (0, 0);
    let Ok(entries) = std::fs::read_dir(dir.join("drive-index")) else { return (0, 0) };
    for entry in entries.flatten() {
        let path = entry.path();
        if !path.to_string_lossy().ends_with(&format!(".{FORMAT}.db")) {
            continue;
        }
        let len = |p: &Path| std::fs::metadata(p).map_or(0, |m| m.len());
        bytes += len(&path) + len(&PathBuf::from(format!("{}-wal", path.display())));
        free += Store::peek_space(&path).map_or(0, |s| s.free);
    }
    (bytes, free)
}

/// Compacts the catalogs: the worker does it between updates, or this
/// process when no worker runs (indexing paused). Returns the status after.
pub fn compact(dir: &Path) -> Result<Value> {
    crate::ownfile::write(&dir.join(COMPACT_REQUEST), b"")?;
    if running(dir) {
        oneup_sys::signal::raise(&signal(dir))?;
        return status(dir);
    }
    let cache_kib = catalog_cache_kib(dir);
    for drive in drives() {
        let db = drive.db(dir);
        if db.is_file() {
            compact_catalog(&Store::open(&db, cache_kib)?, &db, &drive.root)?;
        }
    }
    let _ = std::fs::remove_file(dir.join(COMPACT_REQUEST));
    status(dir)
}

/// Search a bounded page of the mounted catalogs. Reopen each displayed
/// candidate for attributes so missing, replaced and inaccessible paths
/// cannot escape from the cache. File contents are never opened by search.
pub fn find(dir: &Path, q: &str, words: bool, folder: Option<bool>, limit: usize) -> Result<Value> {
    if !crate::config::load_search(dir)?.local_drives {
        bail!("Turn on local drive indexing in General Settings.");
    }
    let limit = limit.clamp(1, super::browse::MAX_RESULTS);
    let q = super::pattern(q)?;
    if words && !oneup_store::search::WordQuery::new(q).can_use_index() {
        bail!("Include a word with at least 3 characters.");
    }
    let protected = protected_roots(dir)?;
    let exclusions = Exclusions::load(dir)?;
    let mut items = Vec::new();
    let mut ready = 0;
    let mut limited = false;
    let mut parents = HashMap::new();
    oneup_cfapi::expose_placeholders()?;
    for drive in drives() {
        let db = drive.db(dir);
        if !db.is_file() || protected.contains(&(drive.serial, drive.id)) {
            continue;
        }
        let Ok(journal) = drive.journal() else { continue };
        let mut store = Store::open(&db, 1024)?;
        let fingerprint = drive.config(&protected, &exclusions).rules.fingerprint();
        let before = items.len();
        let mut bookmark = None;
        let partial = store.snapshot(|o| {
            let Some(root) = o.local_root()? else { return Ok(None) };
            if o.meta_get(READY)?.as_deref() != Some("1") || !drive.matches_root(&root)
                || root.rules != fingerprint || !journal_covers(&journal, root.journal)
            {
                return Ok(None);
            }
            bookmark = root.journal;
            o.visit_drive_names(q, words, folder, |row| {
            if let Some((rel, true)) = o.local_named_path_of(row.file_id)?
                && !exclusions.hides(rel.trim_start_matches('/'), row.kind == oneup_store::LocalKind::Folder)
                && let Some(path) = crate::filetree::search_local_path(
                    &drive.root.to_string_lossy(), rel.trim_start_matches('/'))
                && listable(Path::new(&path), &mut parents)
                && search_identity(Path::new(&path)).is_ok_and(|(serial, id)| serial == drive.serial && id.0 == row.file_id)
            {
                let folder = row.kind == oneup_store::LocalKind::Folder;
                items.push(json!({
                    "id": format!("{:016x}:{:032x}", drive.serial, row.file_id),
                    "name": row.name, "path": path.replace('\\', "/"), "local": path, "folder": folder,
                    "placement": if row.attributes & super::RECALL_ON_DATA_ACCESS != 0 { "online-only" } else { "here" },
                    "size": (!folder).then_some(row.size), "modified_ms": (row.mtime > 0).then(|| crate::space::unix_ms(row.mtime))
                }));
            }
            Ok(items.len() <= limit)
            }).map(Some)
        })?;
        if let Some(partial) = partial {
            if !drive.journal().is_ok_and(|journal| journal_covers(&journal, bookmark)) {
                items.truncate(before);
                continue;
            }
            ready += 1;
            limited |= partial;
        }
        if items.len() > limit {
            break;
        }
    }
    if protected_roots(dir)? != protected || !crate::config::load_search(dir)?.local_drives {
        bail!("Local drive search settings changed. Search again.");
    }
    let more = items.len() > limit;
    items.truncate(limit);
    Ok(json!({ "items": items, "more": more, "too_many": limited.then_some(super::TOO_MANY), "ready": ready }))
}

/// Combine the drive and share catalogs and the mirrors for this profile, including
/// configured shared pairs. Use only indexed names and suppress duplicates.
pub fn everywhere(
    dir: &Path,
    q: &str,
    words: bool,
    folder: Option<bool>,
    limit: usize,
    mut progress: impl FnMut(u64),
) -> Result<Value> {
    let limit = limit.clamp(1, super::browse::MAX_RESULTS);
    let settings = crate::config::load_search(dir)?;
    let local = if settings.local_drives { find(dir, q, words, folder, limit)? } else { Value::Null };
    let network =
        if settings.network_shares { super::shares::find(dir, q, words, folder, limit)? } else { Value::Null };
    let mut local_items = local["items"].as_array().cloned().unwrap_or_default();
    let mut share_items = network["items"].as_array().cloned().unwrap_or_default();
    let mut paths: HashSet<_> = local_items
        .iter()
        .chain(&share_items)
        .filter_map(|item| item["local"].as_str())
        .map(|path| oneup_sync::fold(&path.replace('/', "\\")))
        .collect();
    let mut remote_ids = HashSet::new();
    let query = super::name_query(q, if words { super::MatchMode::Words } else { super::MatchMode::Text }, true)?;
    let pairs = crate::pairs::list_strict(dir)?;
    let roots = std::iter::once(("main".to_owned(), dir.to_path_buf()))
        .chain(pairs.into_iter().map(|pair| (format!("pair:{}", pair.name), pair.dir)));
    let mut cloud_items = Vec::new();
    let mut more_cloud = false;
    for (root, dir) in roots {
        if !dir.join("state.db").try_exists()? {
            continue;
        }
        let store = super::open(&dir, &mut progress)?;
        let found = super::find_matching(&dir, &store, &query, folder, limit + 1)?;
        more_cloud |= found.more;
        for item in found.items {
            if remote_ids.contains(&item.id) {
                continue;
            }
            if let Some(path) = item.local.as_ref()
                && matches!(item.placement, super::Placement::Here | super::Placement::OnlineOnly)
                && !paths.insert(oneup_sync::fold(&path.to_string_lossy().replace('/', "\\")))
            {
                continue;
            }
            remote_ids.insert(item.id.clone());
            let mut value = super::item_json(&item);
            value["pair"] = json!(root);
            cloud_items.push(value);
            if cloud_items.len() > limit {
                break;
            }
        }
        if cloud_items.len() > limit {
            break;
        }
    }
    let more = local["more"] == true
        || network["more"] == true
        || more_cloud
        || local_items.len() + share_items.len() + cloud_items.len() > limit;
    // Alternate sources so one catalog cannot use up the page.
    let mut items = Vec::new();
    local_items.reverse();
    share_items.reverse();
    cloud_items.reverse();
    let mut sources = [local_items, share_items, cloud_items];
    while items.len() < limit && sources.iter().any(|source| !source.is_empty()) {
        for source in &mut sources {
            if items.len() < limit {
                items.extend(source.pop());
            }
        }
    }
    let too_many = local["too_many"].as_str().or_else(|| network["too_many"].as_str());
    let ready = local["ready"].as_u64().unwrap_or(0) + network["ready"].as_u64().unwrap_or(0);
    Ok(json!({
        "items": items, "more": more, "ready": ready, "too_many": too_many,
        "note": super::LIMIT_NOTE
    }))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn no_exclusions() -> Exclusions {
        Exclusions { rules: RuleSet::new::<&str>(&[], &[]).unwrap() }
    }

    #[test]
    fn only_the_current_journal_can_serve_the_catalog() {
        let journal = oneup_ntfs::win::JournalInfo {
            journal_id: 20,
            first_usn: 30,
            lowest_valid_usn: 40,
            next_usn: 50,
            max_usn: i64::MAX,
            max_size: 0,
            max_major_version: 3,
        };
        assert!(journal_covers(&journal, Some((20, 40))));
        assert!(journal_covers(&journal, Some((20, 50))));
        assert!(!journal_covers(&journal, None));
        assert!(!journal_covers(&journal, Some((19, 45))));
        assert!(!journal_covers(&journal, Some((20, 39))));
        assert!(!journal_covers(&journal, Some((20, 51))));
    }

    #[test]
    fn temporary_catalog_recovers_after_pause_and_applies_the_journal() {
        let temp = tempfile::tempdir().unwrap();
        let root = temp.path().join("files");
        let profile = temp.path().join("profile");
        std::fs::create_dir_all(root.join("protected")).unwrap();
        std::fs::create_dir_all(&profile).unwrap();
        std::fs::write(root.join("Invoice-2026.pdf"), "fixture").unwrap();
        std::fs::write(root.join("protected").join("Private-2026.pdf"), "fixture").unwrap();
        let drive = Drive::open(root.clone()).unwrap();
        let (serial, protected) = search_identity(&root.join("protected")).unwrap();
        let cfg = drive.config(&[(serial, protected.0)], &no_exclusions());
        assert_eq!(
            update_drive(&profile, &drive, cfg.clone(), 1024, || false, || Ok(()), || {}).unwrap(),
            Updated::Stopped
        );
        assert!(!drive.db(&profile).exists());
        assert_eq!(
            update_drive(&profile, &drive, cfg.clone(), 1024, || true, || Ok(()), || {}).unwrap(),
            Updated::Ready
        );
        let find = |text: &str| {
            let store = Store::open(&drive.db(&profile), 1024).unwrap();
            let mut names = Vec::new();
            store
                .read(|o| {
                    o.visit_drive_names(text, false, None, |row| {
                        names.push(row.name);
                        Ok(true)
                    })
                })
                .unwrap();
            names
        };
        assert_eq!(find("invoice"), ["Invoice-2026.pdf"]);
        assert!(find("private").is_empty());
        let status = Command::new("cmd")
            .current_dir(&root)
            .args(["/d", "/c", "ren Invoice-2026.pdf Receipt-2026.pdf"])
            .status()
            .unwrap();
        assert!(status.success());
        assert_eq!(update_drive(&profile, &drive, cfg, 1024, || true, || Ok(()), || {}).unwrap(), Updated::Ready);
        assert!(find("invoice").is_empty());
        assert_eq!(find("receipt"), ["Receipt-2026.pdf"]);
    }

    /// A scan stopped part way (paused, or the PC was off) whose journal
    /// lost records meanwhile. Before, the resumed scan ran to its end, the
    /// replay failed ("Index unavailable"), and the next update started
    /// over: on a drive that takes longer to list than its journal keeps
    /// history, forever. Now the scan finds the loss as it resumes, starts
    /// over at once, and follows the journal until it's done.
    #[test]
    fn a_scan_resumed_after_the_journal_lost_records_starts_over_and_finishes() {
        let temp = tempfile::tempdir().unwrap();
        let root = temp.path().join("files");
        let profile = temp.path().join("profile");
        for folder in ["a", "b", "c"] {
            std::fs::create_dir_all(root.join(folder)).unwrap();
            std::fs::write(root.join(folder).join(format!("Report-{folder}.txt")), "fixture").unwrap();
        }
        std::fs::create_dir_all(&profile).unwrap();
        let drive = Drive::open(root).unwrap();
        let cfg = oneup_local::Config { batch_dirs: 1, ..drive.config(&[], &no_exclusions()) };
        let calls = std::cell::Cell::new(0);
        let scans = std::cell::Cell::new(0);
        let scanning = || {
            scans.set(scans.get() + 1);
            Ok(())
        };
        let stop_soon = || {
            calls.set(calls.get() + 1);
            // Enough for the root's step and at most one more.
            calls.get() < 4
        };
        assert_eq!(
            update_drive(&profile, &drive, cfg.clone(), 1024, stop_soon, scanning, || {}).unwrap(),
            Updated::Stopped
        );
        let mut store = Store::open(&drive.db(&profile), 64).unwrap();
        let stats = store.read(|o| o.local_stats()).unwrap();
        assert!(stats.queued > 0, "stopped part way: {stats:?}");
        // The bookmark falls out of the journal: here past its end, as after
        // the journal was reset.
        store.write(|o| o.local_set_journal_cursor(i64::MAX)).unwrap();
        drop(store);
        assert_eq!(update_drive(&profile, &drive, cfg, 1024, || true, scanning, || {}).unwrap(), Updated::Ready);
        assert_eq!(scans.get(), 2, "the status says indexing before each scan");
        let store = Store::open(&drive.db(&profile), 64).unwrap();
        let mut names = Vec::new();
        store
            .read(|o| {
                o.visit_drive_names("report", false, None, |row| {
                    names.push(row.name);
                    Ok(true)
                })
            })
            .unwrap();
        names.sort();
        assert_eq!(names, ["Report-a.txt", "Report-b.txt", "Report-c.txt"]);
    }

    /// A scan from scratch gets a new catalog file rather than deleting the
    /// old one's rows one by one; one a search still has open is kept (and
    /// its -wal with it), for the scan to empty.
    #[test]
    fn a_catalog_starts_over_as_a_new_file_unless_it_is_open() {
        let temp = tempfile::tempdir().unwrap();
        let db = temp.path().join("C-0000000000000001-00000000000000000000000000000005.v2.db");
        let marked = |store: &Store| store.read(|o| o.meta_get("test.mark")).unwrap().is_some();
        let mut store = open_catalog(&db, 64).unwrap();
        store.write(|o| o.meta_set("test.mark", "old")).unwrap();
        let search = Store::open(&db, 64).unwrap();
        drop(store);
        let (store, fresh) = start_over(&db, 64).unwrap();
        assert!(!fresh && marked(&store), "open elsewhere: kept");
        drop((store, search));
        let (store, fresh) = start_over(&db, 64).unwrap();
        assert!(fresh && !marked(&store), "a new file");
        assert_eq!(store.key_order(), oneup_store::KeyOrder::Ordered);
        assert!(!db.with_extension("old").exists());
    }

    /// Names in a catalog, as search finds them before checking the files.
    fn names_in(db: &Path, text: &str) -> Vec<String> {
        let store = Store::open(db, 64).unwrap();
        let mut names = Vec::new();
        store
            .read(|o| {
                o.visit_drive_names(text, false, None, |row| {
                    names.push(row.name);
                    Ok(true)
                })
            })
            .unwrap();
        names.sort();
        names
    }

    #[test]
    fn a_catalog_follows_the_exclusions_and_changes_with_them_without_a_scan() {
        let temp = tempfile::tempdir().unwrap();
        let root = temp.path().join("files");
        let profile = temp.path().join("profile");
        for rel in ["node_modules/pkg/Report-dep.js", "Secret/Report-plan.txt", "Report-app.log", "Report-keep.txt"] {
            let path = root.join(rel);
            std::fs::create_dir_all(path.parent().unwrap()).unwrap();
            std::fs::write(&path, "fixture").unwrap();
        }
        std::fs::create_dir_all(&profile).unwrap();
        let rules = |exclude: &str| {
            std::fs::write(
                profile.join("config.toml"),
                format!("[rules]\npresets = [\"dev\"]\nexclude = [{exclude}]\n"),
            )
            .unwrap();
            Exclusions::load(&profile).unwrap()
        };
        let drive = Drive::open(root).unwrap();
        let db = drive.db(&profile);
        let scans = std::cell::Cell::new(0);
        let scanning = || {
            scans.set(scans.get() + 1);
            Ok(())
        };

        let first = rules(r#""*.log", "Secret/", "/Report-keep.txt", "Sub/Report-keep.txt""#);
        let cfg = drive.config(&[], &first);
        assert_eq!(update_drive(&profile, &drive, cfg, 1024, || true, scanning, || {}).unwrap(), Updated::Ready);
        assert_eq!(Store::open(&db, 64).unwrap().key_order(), oneup_store::KeyOrder::Ordered);
        // Folders left out by the scan; the log file's name is stored and
        // hidden when results are shown; anchored lines don't apply.
        assert_eq!(names_in(&db, "report"), ["Report-app.log", "Report-keep.txt"]);
        assert!(first.hides("Report-app.log", false));
        assert!(!first.hides("Report-keep.txt", false));
        assert!(first.hides("node_modules", true) && first.hides("Secret", true));

        // Secret/ no longer excluded: listed in place, without a new scan.
        let second = rules(r#""*.log""#);
        let cfg = drive.config(&[], &second);
        assert_eq!(update_drive(&profile, &drive, cfg, 1024, || true, scanning, || {}).unwrap(), Updated::Ready);
        assert_eq!(scans.get(), 1, "one scan, at the start");
        assert_eq!(names_in(&db, "report"), ["Report-app.log", "Report-keep.txt", "Report-plan.txt"]);
    }

    /// A file renamed while the scan runs, in a folder already listed: the
    /// journal renames a row still waiting for the name index. That failed
    /// the scan ("UNIQUE constraint failed: drive_dirty.file_id").
    #[test]
    fn a_rename_during_the_scan_reaches_the_index() {
        let temp = tempfile::tempdir().unwrap();
        let root = temp.path().join("files");
        let profile = temp.path().join("profile");
        for folder in ["a", "b", "c"] {
            std::fs::create_dir_all(root.join(folder)).unwrap();
        }
        std::fs::write(root.join("Report-draft.txt"), "fixture").unwrap();
        std::fs::create_dir_all(&profile).unwrap();
        let drive = Drive::open(root.clone()).unwrap();
        let cfg = oneup_local::Config { batch_dirs: 1, ..drive.config(&[], &no_exclusions()) };
        let calls = std::cell::Cell::new(0);
        let keep_going = || {
            calls.set(calls.get() + 1);
            // After the root's step: its files are stored, its folders queued.
            if calls.get() == 4 {
                let status = Command::new("cmd")
                    .current_dir(&root)
                    .args(["/d", "/c", "ren Report-draft.txt Report-final.txt"])
                    .status()
                    .unwrap();
                assert!(status.success());
            }
            true
        };
        assert_eq!(update_drive(&profile, &drive, cfg, 1024, keep_going, || Ok(()), || {}).unwrap(), Updated::Ready);
        assert!(calls.get() > 4, "the rename happened during the scan");
        assert_eq!(names_in(&drive.db(&profile), "report"), ["Report-final.txt"]);
    }

    /// While one drive is scanned, a ready catalog of another follows its
    /// own journal: a rename there reaches its index before its turn comes.
    #[test]
    fn a_ready_catalog_follows_its_journal_while_another_drive_is_scanned() {
        let temp = tempfile::tempdir().unwrap();
        let (a, b) = (temp.path().join("a"), temp.path().join("b"));
        let profile = temp.path().join("profile");
        std::fs::create_dir_all(&a).unwrap();
        std::fs::write(a.join("Invoice-2026.pdf"), "fixture").unwrap();
        for folder in ["x", "y", "z"] {
            std::fs::create_dir_all(b.join(folder)).unwrap();
        }
        std::fs::create_dir_all(&profile).unwrap();
        let all = [Drive::open(a.clone()).unwrap(), Drive::open(b).unwrap()];
        let cfg = |drive: &Drive| oneup_local::Config { batch_dirs: 1, ..drive.config(&[], &no_exclusions()) };
        let update = |drive: &Drive, between: &mut dyn FnMut()| {
            update_drive(&profile, drive, cfg(drive), 1024, || true, || Ok(()), between).unwrap()
        };
        assert_eq!(update(&all[0], &mut || {}), Updated::Ready);
        let mut others = Others::new(&profile, &all, 1, &[], &no_exclusions(), 1024);
        others.every = Duration::ZERO;
        let mut steps = 0;
        let mut between = || {
            steps += 1;
            if steps == 1 {
                let status = Command::new("cmd")
                    .current_dir(&a)
                    .args(["/d", "/c", "ren Invoice-2026.pdf Receipt-2026.pdf"])
                    .status()
                    .unwrap();
                assert!(status.success());
            }
            others.follow(&|| true);
        };
        assert_eq!(update(&all[1], &mut between), Updated::Ready);
        assert!(steps > 1, "several steps");
        assert_eq!(names_in(&all[0].db(&profile), "2026"), ["Receipt-2026.pdf"]);
    }

    /// A drive whose journal turns over in minutes is read more often than
    /// once a minute, and its catalog isn't compacted: that would lose the
    /// journal's place and scan the drive again.
    #[test]
    fn a_short_journal_is_read_often_and_not_compacted_over() {
        let minutes = |m: u64| Duration::from_secs(m * 60);
        assert_eq!(pass_wait([minutes(90), Duration::from_secs(90)]), Duration::from_millis(22_500));
        assert_eq!(pass_wait([Duration::from_secs(8)]), SHORTEST_WAIT);
        assert_eq!(pass_wait([minutes(600)]), PERIOD);
        assert_eq!(pass_wait([]), PERIOD);
        assert!(!compaction_fits(Some(Duration::from_secs(90))));
        assert!(compaction_fits(Some(minutes(45))) && compaction_fits(None));
        let temp = tempfile::tempdir().unwrap();
        let history = journal_history(temp.path()).expect("the temp folder's volume has a journal");
        assert!(history > Duration::ZERO && history < Duration::from_secs(10 * 365 * 24 * 3600), "{history:?}");
    }

    #[test]
    fn catalogs_of_the_old_format_are_told_apart_by_name() {
        let old = "C-e49050c590509fba-00000000000000000005000000000005.db";
        assert!(is_old_catalog(old));
        assert!(is_old_catalog(&format!("{old}-wal")) && is_old_catalog(&format!("{old}-shm")));
        assert!(!is_old_catalog("C-e49050c590509fba-00000000000000000005000000000005.v2.db"));
        assert!(!is_old_catalog("C-e49050c590509fba-00000000000000000005000000000005.v2.db-wal"));
        assert!(!is_old_catalog("notes.db") && !is_old_catalog("C-xyz-123.db"));
        let temp = tempfile::tempdir().unwrap();
        let index = temp.path().join("drive-index");
        std::fs::create_dir_all(&index).unwrap();
        let new = "C-e49050c590509fba-00000000000000000005000000000005.v2.db";
        for name in [old, &format!("{old}-wal"), new, "other.txt"] {
            std::fs::write(index.join(name), "x").unwrap();
        }
        remove_old_catalogs(temp.path());
        let mut left: Vec<String> =
            std::fs::read_dir(&index).unwrap().map(|e| e.unwrap().file_name().to_string_lossy().into_owned()).collect();
        left.sort();
        assert_eq!(left, [new, "other.txt"]);
    }

    #[test]
    fn only_lines_meant_anywhere_apply_to_a_whole_drive() {
        for anywhere in ["*.iso", "node_modules/", "**/build/", "!keep.iso", ".venv"] {
            assert!(!is_anchored(anywhere), "{anywhere}");
        }
        for anchored in ["/Docs/", "Docs/drafts/", ".claude/worktrees", "**/a/b"] {
            assert!(is_anchored(anchored), "{anchored}");
        }
    }

    #[test]
    fn a_scanned_drive_shows_as_indexing_between_the_drives_before_and_after_it() {
        let last = [
            json!({ "drive": "C:\\", "status": "Ready" }),
            json!({ "drive": "D:\\", "status": "Ready" }),
            json!({ "drive": "E:\\", "status": "Indexing paused" }),
        ];
        let done = [json!({ "drive": "C:\\", "status": "Ready" })];
        assert_eq!(
            interim(&done, "D:\\", &last, "Indexing…"),
            [
                json!({ "drive": "C:\\", "status": "Ready" }),
                json!({ "drive": "D:\\", "status": "Indexing…" }),
                json!({ "drive": "E:\\", "status": "Indexing paused" }),
            ]
        );
        assert_eq!(interim(&[], "C:\\", &[], "Compacting…"), [json!({ "drive": "C:\\", "status": "Compacting…" })]);
    }
}
