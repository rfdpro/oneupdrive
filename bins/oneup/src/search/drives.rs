//! Search-only catalogs of fixed local volumes. Each catalog has its own
//! scanner, journal bookmark and trigram table, outside the sync database.

use std::collections::{HashMap, HashSet};
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::time::{Duration, Instant};

use anyhow::{Context, Result, bail};
use oneup_local::observe::{Error as ObserveError, Observer};
use oneup_ntfs::win::{Volume, search_identity, search_listable};
use oneup_rules::{ExcludeSet, RuleSet};
use oneup_store::Store;
use serde_json::{Value, json};

const READY: &str = "drive_search.ready";
const PERIOD: Duration = Duration::from_secs(60);
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
        oneup_local::Config::new(exclusions.folders().with_folder_ids(excluded))
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
            event.wait(PERIOD);
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
    for drive in drives() {
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
            let result = update_drive(dir, &drive, cfg, cache_kib, keep_going, scanning).and_then(|updated| {
                // A catalog still being scanned is compacted when its scan ends.
                if compact && updated == Updated::Ready && running() {
                    show(&interim(&status, &name, last, crate::t_key!("Compacting…")))?;
                    let db = drive.db(dir);
                    compact_catalog(&Store::open(&db, cache_kib)?, &db, &drive.root)?;
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
/// scan, after which it's compacted. `scanning` runs before a scan starts.
fn update_drive(
    dir: &Path,
    drive: &Drive,
    cfg: oneup_local::Config,
    cache_kib: u32,
    keep_going: impl Fn() -> bool,
    scanning: impl FnOnce() -> Result<()>,
) -> Result<Updated> {
    if !keep_going() {
        return Ok(Updated::Stopped);
    }
    let db = drive.db(dir);
    std::fs::create_dir_all(db.parent().context("catalog parent")?)?;
    let mut store = Store::open(&db, cache_kib)?;
    match store.read(|o| o.local_root())? {
        Some(root) if !drive.matches_root(&root) => bail!("The volume identity changed"),
        Some(_) => {}
        // A new catalog: file IDs sorted as the MFT is, so a scan's rows land
        // on few B-tree pages (`KeyOrder::Ordered`).
        None => match store.use_ordered_keys() {
            Ok(()) | Err(oneup_store::Error::KeysInUse) => {}
            Err(e) => return Err(e.into()),
        },
    }
    store.install_drive_search()?;
    // Changed exclusions (or vaults) adjust the finished tree in place.
    if let Some(r) = store.write(|o| oneup_local::observe::reapply_rules(o, &cfg.rules))? {
        tracing::info!(
            drive = %drive.root.display(), excluded = r.excluded, included = r.included,
            "drive index follows the changed exclusions"
        );
    }
    let needs_scan = match Observer::open(store, cfg.clone()) {
        Ok(mut observer) => match observer.catch_up_while(&keep_going) {
            Ok(_) => return finish(observer.store_mut(), &keep_going),
            Err(ObserveError::NeedsRescan(why)) => {
                tracing::info!(drive = %drive.root.display(), %why, "drive index scans again");
                observer.into_store()
            }
            Err(err) => return Err(err.into()),
        },
        Err(ObserveError::NeedsRescan(_)) => Store::open(&db, cache_kib)?,
        Err(err) => return Err(err.into()),
    };
    scanning()?;
    let mut store = needs_scan;
    store.write(|o| o.meta_set(READY, "0"))?;
    let mut scan = oneup_local::Scan::open(store, &drive.root, cfg.clone())?;
    while keep_going() {
        // A whole drive can take longer to list than its journal keeps
        // history. Replaying the journal only after the scan would then find
        // it wrapped, and the next update would start over, without end.
        match scan.follow_journal(&keep_going) {
            Ok(_) => {}
            Err(ObserveError::NeedsRescan(why)) => {
                tracing::info!(drive = %drive.root.display(), %why, "drive index scan starts over");
                scan = scan.restart()?;
                continue;
            }
            Err(err) => return Err(err.into()),
        }
        if !scan.step()? {
            let mut observer = Observer::open(scan.into_store(), cfg)?;
            return match observer.catch_up_while(&keep_going) {
                Ok(_) => {
                    let updated = finish(observer.store_mut(), &keep_going)?;
                    // Pages a scan fills in no order are part empty: the
                    // rewrite packs them.
                    if updated == Updated::Ready
                        && let Err(e) = compact_catalog(observer.store(), &db, &drive.root)
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

fn finish(store: &mut Store, keep_going: &impl Fn() -> bool) -> Result<Updated> {
    flush(store, keep_going)?;
    publish(store, keep_going)?;
    Ok(if keep_going() { Updated::Ready } else { Updated::Stopped })
}

fn flush(store: &mut Store, keep_going: &impl Fn() -> bool) -> Result<()> {
    while keep_going() && store.flush_drive_search(BATCH)? == BATCH {}
    Ok(())
}

fn publish(store: &mut Store, keep_going: &impl Fn() -> bool) -> Result<()> {
    if keep_going() && store.read(|o| o.meta_get(READY))?.as_deref() != Some("1") {
        store.write(|o| o.meta_set(READY, "1"))?;
    }
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
        assert_eq!(update_drive(&profile, &drive, cfg.clone(), 1024, || false, || Ok(())).unwrap(), Updated::Stopped);
        assert!(!drive.db(&profile).exists());
        assert_eq!(update_drive(&profile, &drive, cfg.clone(), 1024, || true, || Ok(())).unwrap(), Updated::Ready);
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
        assert_eq!(update_drive(&profile, &drive, cfg, 1024, || true, || Ok(())).unwrap(), Updated::Ready);
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
        assert_eq!(update_drive(&profile, &drive, cfg.clone(), 1024, stop_soon, scanning).unwrap(), Updated::Stopped);
        let mut store = Store::open(&drive.db(&profile), 64).unwrap();
        let stats = store.read(|o| o.local_stats()).unwrap();
        assert!(stats.queued > 0, "stopped part way: {stats:?}");
        // The bookmark falls out of the journal: here past its end, as after
        // the journal was reset.
        store.write(|o| o.local_set_journal_cursor(i64::MAX)).unwrap();
        drop(store);
        assert_eq!(update_drive(&profile, &drive, cfg, 1024, || true, scanning).unwrap(), Updated::Ready);
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
        assert_eq!(update_drive(&profile, &drive, cfg, 1024, || true, scanning).unwrap(), Updated::Ready);
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
        assert_eq!(update_drive(&profile, &drive, cfg, 1024, || true, scanning).unwrap(), Updated::Ready);
        assert_eq!(scans.get(), 1, "one scan, at the start");
        assert_eq!(names_in(&db, "report"), ["Report-app.log", "Report-keep.txt", "Report-plan.txt"]);
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
