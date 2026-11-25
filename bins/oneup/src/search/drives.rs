//! Search-only catalogs of fixed local volumes. Each catalog has its own
//! scanner, journal bookmark and trigram table, outside the sync database.

use std::collections::{HashMap, HashSet};
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::time::{Duration, Instant};

use anyhow::{Context, Result, bail};
use oneup_local::observe::{Error as ObserveError, Observer};
use oneup_ntfs::win::{Volume, search_identity, search_listable};
use oneup_store::Store;
use serde_json::{Value, json};

const READY: &str = "drive_search.ready";
const PERIOD: Duration = Duration::from_secs(60);
const BATCH: usize = 8_192;

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
        dir.join("drive-index").join(format!("{letter}-{:016x}-{:032x}.db", self.serial, self.id))
    }

    fn config(&self, protected: &[(u64, u128)]) -> oneup_local::Config {
        let excluded = protected.iter().filter(|(serial, _)| *serial == self.serial).map(|(_, id)| *id).collect();
        let rules = oneup_rules::ExcludeSet::from_names([".oneup", "$Recycle.Bin", "System Volume Information"])
            .with_folder_ids(excluded);
        let mut cfg = oneup_local::Config::new(rules);
        cfg.batch_dirs = 256;
        cfg.flush_entries = BATCH;
        cfg
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
        let mut cmd = Command::new(crate::launch::sibling("oneup.exe")?);
        cmd.arg("--data-dir")
            .arg(dir)
            .arg("drive-index-worker")
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(Stdio::null());
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
    let mut previous = None;
    while running() {
        let settings = crate::config::load_search(dir)?;
        if !(settings.local_drives || settings.network_shares) || settings.index_paused {
            return Ok(());
        }
        let _ = oneup_sys::priority::set_background(crate::config::load_agent(dir).map_or(true, |a| a.background));
        check_shares(settings.network_shares);
        let status = if settings.local_drives {
            update(dir, &running)
                .unwrap_or_else(|_| vec![json!({ "drive": null, "status": crate::t_key!("Index unavailable") })])
        } else {
            Vec::new()
        };
        if running() {
            if previous.as_ref() != Some(&status) {
                crate::ownfile::write(&dir.join("drive-index-status.json"), &serde_json::to_vec(&status)?)?;
                previous = Some(status);
            }
            event.wait(PERIOD);
        }
    }
    Ok(())
}

fn update(dir: &Path, running: &impl Fn() -> bool) -> Result<Vec<Value>> {
    let protected = protected_roots(dir)?;
    let mut status = Vec::new();
    for drive in drives() {
        if !running() {
            break;
        }
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
            match update_drive(dir, &drive, drive.config(&protected), keep_going) {
                Ok(true) => crate::t_key!("Ready"),
                Ok(false) => crate::t_key!("Indexing paused"),
                Err(_) => crate::t_key!("Index unavailable"),
            }
        };
        status.push(json!({ "drive": drive.root.to_string_lossy(), "status": state }));
    }
    if status.is_empty() {
        status.push(json!({ "drive": null, "status": crate::t_key!("No supported local drives") }));
    }
    Ok(status)
}

fn update_drive(dir: &Path, drive: &Drive, cfg: oneup_local::Config, keep_going: impl Fn() -> bool) -> Result<bool> {
    if !keep_going() {
        return Ok(false);
    }
    let db = drive.db(dir);
    std::fs::create_dir_all(db.parent().context("catalog parent")?)?;
    let mut store = Store::open(&db, 4096)?;
    if store.read(|o| o.local_root())?.is_some_and(|root| !drive.matches_root(&root)) {
        bail!("The volume identity changed");
    }
    store.install_drive_search()?;
    let needs_scan = match Observer::open(store, cfg.clone()) {
        Ok(mut observer) => match observer.catch_up_while(&keep_going) {
            Ok(_) => {
                flush(observer.store_mut(), &keep_going)?;
                publish(observer.store_mut(), &keep_going)?;
                return Ok(keep_going());
            }
            Err(ObserveError::NeedsRescan(_)) => observer.into_store(),
            Err(err) => return Err(err.into()),
        },
        Err(ObserveError::NeedsRescan(_)) => Store::open(&db, 4096)?,
        Err(err) => return Err(err.into()),
    };
    let mut store = needs_scan;
    store.write(|o| o.meta_set(READY, "0"))?;
    let mut scan = oneup_local::Scan::open(store, &drive.root, cfg.clone())?;
    while keep_going() {
        if !scan.step()? {
            let mut observer = Observer::open(scan.into_store(), cfg)?;
            observer.catch_up_while(&keep_going)?;
            flush(observer.store_mut(), &keep_going)?;
            publish(observer.store_mut(), &keep_going)?;
            return Ok(keep_going());
        }
    }
    Ok(false)
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
    let mut drives: Vec<Value> = std::fs::read(dir.join("drive-index-status.json"))
        .ok()
        .and_then(|data| serde_json::from_slice(&data).ok())
        .unwrap_or_default();
    if !settings.local_drives {
        drives.clear();
    }
    if settings.network_shares {
        drives.extend(super::shares::status(dir, settings.index_paused));
    }
    Ok(
        json!({ "enabled": settings.local_drives || settings.network_shares, "paused": settings.index_paused, "drives": drives }),
    )
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
        let fingerprint = drive.config(&protected).rules.fingerprint();
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
        let cfg = drive.config(&[(serial, protected.0)]);
        assert!(!update_drive(&profile, &drive, cfg.clone(), || false).unwrap());
        assert!(!drive.db(&profile).exists());
        assert!(update_drive(&profile, &drive, cfg.clone(), || true).unwrap());
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
        assert!(update_drive(&profile, &drive, cfg, || true).unwrap());
        assert!(find("invoice").is_empty());
        assert_eq!(find("receipt"), ["Receipt-2026.pdf"]);
    }
}
