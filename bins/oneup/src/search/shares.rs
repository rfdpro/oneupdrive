//! Search-only mapped SMB catalogs. A live watcher owns each catalog;
//! reconnecting requires a fresh listing because SMB can't replay changes.

use std::collections::{BTreeMap, HashMap};
use std::os::windows::fs::MetadataExt;
use std::path::{Path, PathBuf};
use std::thread::{self, JoinHandle};
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use anyhow::{Context, Result, ensure};
use oneup_ntfs::dirent::{FILE_ATTRIBUTE_DIRECTORY, FILE_ATTRIBUTE_REPARSE_POINT};
use oneup_ntfs::win::{self, Handle};
use oneup_store::Store;
use oneup_sys::directory_watch::Watch;
use oneup_sys::signal::{self, Signal};
use serde_json::{Value, json};
use sha2::{Digest, Sha256};

const PERIOD: Duration = Duration::from_secs(60);
const SESSION: &str = "share_search.session";
type Identity = (u64, u128);

#[derive(Clone, Debug, PartialEq, Eq, Hash)]
struct Share {
    letter: PathBuf,
    target: PathBuf,
}

fn shares() -> impl Iterator<Item = Share> {
    oneup_sys::disk::mapped_search_roots().into_iter().map(|(letter, target)| Share { letter, target })
}

impl Share {
    fn key(&self) -> String {
        format!("{:x}", Sha256::digest(self.target.as_os_str().as_encoded_bytes()))
    }

    fn db(&self, dir: &Path) -> PathBuf {
        let key = self.key();
        let letter = self.letter.to_string_lossy().chars().next().unwrap_or('_');
        dir.join("share-index").join(format!("{letter}-{key}.db"))
    }

    fn stopping(&self, dir: &Path) -> String {
        let letter = self.letter.to_string_lossy().chars().next().unwrap_or('_');
        stop_signal(dir, letter)
    }

    fn lease(&self, dir: &Path, identity: Identity, session: &str) -> String {
        let (serial, id) = identity;
        format!("{}-{serial:016x}-{id:032x}-{session}", self.stopping(dir))
    }

    fn is_mapped(&self) -> bool {
        shares().any(|share| share == *self)
    }

    fn root_identity(&self, protected: &[Identity]) -> Result<Option<Identity>> {
        for ancestor in self.target.ancestors().skip(1).filter(|path| path.has_root()) {
            let (serial, id) = win::search_identity(ancestor)?;
            if protected.contains(&(serial, id.0)) {
                return Ok(None);
            }
        }
        let Some(handle) = checked_directory(&self.target, protected)? else { return Ok(None) };
        Ok(Some(identity(&handle)?))
    }
}

fn stop_signal(dir: &Path, letter: char) -> String {
    format!("{}-share-{letter}", super::drives::signal(dir))
}

pub(super) fn request_stop(dir: &Path) {
    for letter in b'A'..=b'Z' {
        let _ = signal::raise(&stop_signal(dir, char::from(letter)));
    }
}

#[derive(Default)]
pub(super) struct Workers(HashMap<Share, (String, JoinHandle<()>)>);

impl Workers {
    pub(super) fn reconcile(&mut self, dir: &Path, enabled: bool) {
        let mapped: Vec<_> = if enabled { shares().collect() } else { Vec::new() };
        self.0.retain(|share, (stop, worker)| {
            let keep = mapped.contains(share) && !worker.is_finished();
            if !keep {
                let _ = signal::raise(stop);
            }
            // Keep the thread until it exits so a remap cannot launch two
            // workers for the same letter, even during a stalled SMB call.
            !worker.is_finished()
        });
        for share in mapped {
            if self.0.keys().any(|running| running.letter == share.letter) {
                continue;
            }
            let profile = dir.to_path_buf();
            let source = share.clone();
            let stop_name = share.stopping(dir);
            let Ok(stop) = Signal::new(&stop_name) else { continue };
            let worker = thread::Builder::new().name("share-index".into()).spawn(move || {
                if run(&profile, &source, &stop).is_err() {
                    tracing::debug!("Network share index unavailable");
                }
            });
            if let Ok(worker) = worker {
                self.0.insert(share, (stop_name, worker));
            }
        }
    }
}

impl Drop for Workers {
    fn drop(&mut self) {
        for (stop, _) in self.0.values() {
            let _ = signal::raise(stop);
        }
        for (_, (_, worker)) in self.0.drain() {
            let _ = worker.join();
        }
    }
}

fn enabled(dir: &Path) -> bool {
    crate::config::load_search(dir).is_ok_and(|settings| settings.network_shares && !settings.index_paused)
}

fn identity(handle: &Handle) -> oneup_ntfs::win::Result<Identity> {
    win::file_id(handle).map(|(serial, id)| (serial, id.0))
}

fn eligible(attrs: u32) -> bool {
    attrs & FILE_ATTRIBUTE_REPARSE_POINT == 0 && !oneup_local::classify::is_unpopulated(attrs, 0)
}

fn checked_directory(path: &Path, protected: &[Identity]) -> Result<Option<Handle>> {
    let handle = match win::search_open_dir(path) {
        Ok(handle) => handle,
        Err(e) if e.is_access_denied() || e.is_not_found() => return Ok(None),
        Err(e) => return Err(e.into()),
    };
    let (attrs, _) = win::attribute_tag(&handle)?;
    if attrs & FILE_ATTRIBUTE_DIRECTORY == 0 || !eligible(attrs) || protected.contains(&identity(&handle)?) {
        return Ok(None);
    }
    Ok(Some(handle))
}

fn usable_name(name: &str) -> bool {
    !name.is_empty()
        && !name.ends_with(['.', ' '])
        && !name.contains([':', '/', '\\', '\0'])
        && ![".oneup", "$Recycle.Bin", "System Volume Information"].iter().any(|part| part.eq_ignore_ascii_case(name))
}

fn relative_path(path: &str) -> Option<String> {
    let path = path.replace('\\', "/");
    path.split('/').all(usable_name).then_some(path)
}

/// Check the chain down from the UNC target, including the target itself.
/// Listing uses the final checked handle, so it can't follow a swapped leaf.
fn directory(share: &Share, relative: &str, protected: &[Identity]) -> Result<Option<Handle>> {
    let mut path = share.target.clone();
    let Some(mut handle) = checked_directory(&path, protected)? else { return Ok(None) };
    if !relative.is_empty() {
        for part in relative.split('/') {
            path.push(part);
            let Some(next) = checked_directory(&path, protected)? else { return Ok(None) };
            handle = next;
        }
    }
    Ok(Some(handle))
}

type Pending = BTreeMap<String, bool>;

fn collect(watch: &mut Watch, stop: &Signal, timeout: Duration, pending: &mut Pending) -> Result<()> {
    if let Some(names) = watch.changes(stop, timeout)? {
        pending.extend(
            names.into_iter().filter_map(|change| relative_path(&change.path).map(|path| (path, change.removed))),
        );
        // Bound the unpublished backlog even on a constantly changing share.
        ensure!(
            pending.len() <= 8_192 && pending.keys().map(String::len).sum::<usize>() <= 4 * 1024 * 1024,
            "Network share notification backlog exceeded"
        );
    }
    Ok(())
}

fn walk(
    store: &mut Store,
    share: &Share,
    protected: &[Identity],
    mut checkpoint: impl FnMut() -> Result<()>,
) -> Result<()> {
    let mut buf = vec![0; 64 * 1024];
    loop {
        checkpoint()?;
        let work = store.read(|o| o.share_next())?;
        if work.is_empty() {
            return Ok(());
        }
        for path in work {
            checkpoint()?;
            let Some(handle) = directory(share, &path, protected)? else {
                ensure!(!path.is_empty(), "Network share root unavailable");
                store.write(|o| o.share_dequeue(&path))?;
                continue;
            };
            let prefix = if path.is_empty() { String::new() } else { format!("{path}/") };
            let mut restart = true;
            loop {
                let entries = match win::list_dir_batch(&handle, &mut buf, restart) {
                    Ok(Some(entries)) => entries,
                    Ok(None) => break,
                    Err(e) if !path.is_empty() && (e.is_access_denied() || e.is_not_found()) => break,
                    Err(e) => return Err(e.into()),
                };
                restart = false;
                checkpoint()?;
                store.write(|o| {
                    for entry in &entries {
                        if entry.name_lossy || !usable_name(&entry.name) || !eligible(entry.attributes) {
                            continue;
                        }
                        let relative = format!("{prefix}{}", entry.name);
                        let folder = entry.is_dir();
                        o.share_put(&relative, &entry.name, folder)?;
                        if folder {
                            o.share_enqueue(&relative)?;
                        }
                    }
                    Ok(())
                })?;
            }
            store.write(|o| o.share_dequeue(&path))?;
        }
    }
}

fn apply(
    store: &mut Store,
    share: &Share,
    pending: Pending,
    protected: &[Identity],
    mut checkpoint: impl FnMut() -> Result<()>,
) -> Result<()> {
    for (path, &removed) in &pending {
        if path.match_indices('/').any(|(index, _)| pending.contains_key(&path[..index])) {
            continue;
        }
        checkpoint()?;
        let (parent, name) = path.rsplit_once('/').unwrap_or(("", path));
        let current = if !removed && directory(share, parent, protected)?.is_some() {
            match std::fs::symlink_metadata(share.target.join(path)) {
                Ok(meta) if eligible(meta.file_attributes()) => Some(meta.is_dir()),
                Ok(_) => None,
                Err(e) if matches!(e.kind(), std::io::ErrorKind::NotFound | std::io::ErrorKind::PermissionDenied) => {
                    None
                }
                Err(e) => return Err(e.into()),
            }
        } else {
            None
        };
        store.write(|o| {
            o.share_remove(path)?;
            if let Some(folder) = current {
                o.share_put(path, name, folder)?;
                if folder {
                    o.share_enqueue(path)?;
                }
            }
            Ok(())
        })?;
    }
    Ok(())
}

fn run(dir: &Path, share: &Share, stop: &Signal) -> Result<()> {
    oneup_cfapi::expose_placeholders()?;
    let protected = super::drives::protected_roots(dir)?;
    let identity = share.root_identity(&protected)?.context("Network share root unavailable")?;
    let mut watch = Watch::open(&share.target)?;
    ensure!(identity.1 != 0 && watch.identity()? == identity, "Network share root changed");
    let db = share.db(dir);
    std::fs::create_dir_all(db.parent().context("Network catalog parent")?)?;
    let mut store = Store::open(&db, 2048)?;
    let session = format!("{}-{}", std::process::id(), SystemTime::now().duration_since(UNIX_EPOCH)?.as_nanos());
    store.reset_share_search()?;
    store.write(|o| o.meta_set(SESSION, &session))?;
    let mut pending = Pending::default();
    let mut lease = None;
    loop {
        walk(&mut store, share, &protected, || collect(&mut watch, stop, Duration::ZERO, &mut pending))?;
        ensure!(enabled(dir) && share.is_mapped(), "Network share mapping changed");
        ensure!(super::drives::protected_roots(dir)? == protected, "Protected directories changed");
        ensure!(share.root_identity(&protected)? == Some(identity), "Network share root changed");
        if pending.is_empty() {
            if lease.is_none() {
                lease = Some(Signal::new(&share.lease(dir, identity, &session))?);
            }
            collect(&mut watch, stop, PERIOD, &mut pending)?;
        }
        if !pending.is_empty() {
            apply(&mut store, share, std::mem::take(&mut pending), &protected, || {
                collect(&mut watch, stop, Duration::ZERO, &mut pending)
            })?;
        }
    }
}

pub(super) fn status(dir: &Path, paused: bool) -> Vec<Value> {
    let Ok(protected) = super::drives::protected_roots(dir) else { return Vec::new() };
    let mut items = Vec::new();
    for share in shares() {
        let active = !paused && signal::present(&share.stopping(dir));
        let ready = active
            && share.db(dir).is_file()
            && (|| -> Result<bool> {
                let Some(identity) = share.root_identity(&protected)? else { return Ok(false) };
                let store = Store::open(&share.db(dir), 1024)?;
                Ok(store
                    .read(|o| o.meta_get(SESSION))?
                    .is_some_and(|session| signal::present(&share.lease(dir, identity, &session))))
            })()
            .unwrap_or(false);
        let status = if paused {
            crate::t_key!("Indexing paused")
        } else if ready {
            crate::t_key!("Ready")
        } else if active {
            crate::t_key!("Indexing network share")
        } else {
            crate::t_key!("Index unavailable")
        };
        items.push(json!({ "drive": share.letter, "status": status }));
    }
    if items.is_empty() {
        items.push(json!({ "drive": null, "status": crate::t_key!("No mapped network shares") }));
    }
    items
}

pub fn find(dir: &Path, q: &str, words: bool, folder: Option<bool>, limit: usize) -> Result<Value> {
    let settings = crate::config::load_search(dir)?;
    ensure!(settings.network_shares, "Turn on network share indexing in General Settings.");
    let q = super::pattern(q)?;
    let limit = limit.clamp(1, super::browse::MAX_RESULTS);
    if words && !oneup_store::search::WordQuery::new(q).can_use_index() {
        anyhow::bail!("Include a word with at least 3 characters.");
    }
    if settings.index_paused {
        return Ok(json!({ "items": [], "ready": 0, "more": false }));
    }
    oneup_cfapi::expose_placeholders()?;
    let protected = super::drives::protected_roots(dir)?;
    let mut items = Vec::new();
    let mut ready = 0;
    let mut limited = false;
    for share in shares() {
        let db = share.db(dir);
        if !db.is_file() || !signal::present(&share.stopping(dir)) {
            continue;
        }
        let Ok(Some(identity)) = share.root_identity(&protected) else { continue };
        let mut store = Store::open(&db, 1024)?;
        let before = items.len();
        let mut lease = None;
        let partial = store.snapshot(|o| {
            let Some(session) = o.meta_get(SESSION)? else { return Ok(false) };
            let name = share.lease(dir, identity, &session);
            if !signal::present(&name) {
                return Ok(false);
            }
            lease = Some(name);
            let mut parents = HashMap::new();
            o.visit_share_names(q, words, folder, |path, name, folder| {
                let parent = path.rsplit_once('/').map_or("", |(parent, _)| parent);
                let allowed = parents.get(parent).copied().unwrap_or_else(|| {
                    let allowed = directory(&share, parent, &protected).is_ok_and(|handle| handle.is_some());
                    if parents.len() < 512 {
                        parents.insert(parent.to_owned(), allowed);
                    }
                    allowed
                });
                if allowed
                    && let Ok(meta) = std::fs::symlink_metadata(share.target.join(path))
                    && eligible(meta.file_attributes())
                    && meta.is_dir() == folder
                    && (!folder || directory(&share, path, &protected).is_ok_and(|handle| handle.is_some()))
                    && let Some(local) = crate::filetree::search_local_path(&share.letter.to_string_lossy(), path)
                {
                    items.push(json!({
                        "id": &local, "name": name, "folder": folder, "path": local.replace('\\', "/"),
                        "local": local, "placement": "on network share", "size": (!folder).then_some(meta.file_size()),
                        "modified_ms": Some(crate::space::unix_ms(meta.last_write_time() as i64))
                    }));
                }
                Ok(items.len() <= limit)
            })
        })?;
        if lease.as_deref().is_some_and(signal::present)
            && share.is_mapped()
            && share.root_identity(&protected).is_ok_and(|now| now == Some(identity))
        {
            ready += 1;
            limited |= partial;
        } else {
            items.truncate(before);
        }
        if items.len() > limit {
            break;
        }
    }
    ensure!(
        enabled(dir) && super::drives::protected_roots(dir)? == protected,
        "Search settings changed. Search again."
    );
    let more = items.len() > limit;
    items.truncate(limit);
    Ok(json!({ "items": items, "more": more, "too_many": limited.then_some(super::TOO_MANY), "ready": ready }))
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::BTreeSet;

    #[test]
    fn enumeration_and_notifications_update_only_the_named_subtree() {
        let fixture = tempfile::tempdir().unwrap();
        let root = fixture.path().join("share");
        std::fs::create_dir_all(root.join("Reports")).unwrap();
        std::fs::create_dir_all(root.join("Reports-B")).unwrap();
        std::fs::create_dir_all(root.join("Protected")).unwrap();
        std::fs::write(root.join("Reports/Quarter.txt"), "").unwrap();
        std::fs::write(root.join("Reports-B/Quarter-B.txt"), "").unwrap();
        std::fs::write(root.join("Protected/Quarter-P.txt"), "").unwrap();
        let protected = [identity(&win::search_open_dir(&root.join("Protected")).unwrap()).unwrap()];
        let share = Share { letter: root.clone(), target: root.clone() };
        let stop = Signal::new(&format!("OneUpDrive-test-share-{}", std::process::id())).unwrap();
        let mut watch = Watch::open(&root).unwrap();
        let mut store = Store::open_in_memory().unwrap();
        store.reset_share_search().unwrap();
        walk(&mut store, &share, &protected, || Ok(())).unwrap();
        let found = |store: &Store| {
            let mut paths = BTreeSet::new();
            store
                .read(|o| {
                    o.visit_share_names("Quarter", false, Some(false), |path, _, _| {
                        paths.insert(path.to_owned());
                        Ok(true)
                    })
                })
                .unwrap();
            paths
        };
        assert_eq!(found(&store), ["Reports-B/Quarter-B.txt".into(), "Reports/Quarter.txt".into()].into());
        std::fs::rename(root.join("Reports"), root.join("Archive")).unwrap();
        let mut changes = Pending::new();
        for _ in 0..2 {
            collect(&mut watch, &stop, Duration::from_secs(2), &mut changes).unwrap();
            if changes.contains_key("Reports") && changes.contains_key("Archive") {
                break;
            }
        }
        assert_eq!(changes.get("Reports"), Some(&true), "{changes:?}");
        assert_eq!(changes.get("Archive"), Some(&false), "{changes:?}");
        apply(&mut store, &share, changes, &protected, || Ok(())).unwrap();
        walk(&mut store, &share, &protected, || Ok(())).unwrap();
        assert_eq!(found(&store), ["Archive/Quarter.txt".into(), "Reports-B/Quarter-B.txt".into()].into());
    }

    #[test]
    fn notification_paths_cannot_escape_the_share() {
        assert_eq!(relative_path(r"Folder\file.txt").as_deref(), Some("Folder/file.txt"));
        for path in [r"..\outside", r"\rooted", r"C:\other", r"foo\.oneup\keys", "file.txt:stream"] {
            assert!(relative_path(path).is_none(), "{path}");
        }
    }
}
