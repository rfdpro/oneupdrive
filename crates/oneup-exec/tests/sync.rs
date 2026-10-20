//! End-to-end two-way sync of a real temp folder with an in-memory cloud:
//! scanner → observer → planner → gate → effects/executor, round after
//! round, until both sides agree. Never touches a user's folder or account.
#![cfg(windows)]
#![allow(clippy::unwrap_used)]

use std::collections::{BTreeMap, BTreeSet};
use std::io::Write;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use oneup_effects::LocalFs;
use oneup_exec::{
    CRASHED, Cloud, CloudError, Executor, Meta, MetaDone, Replaced, Report, Source, VersionMark, link_roots,
};
use oneup_gate::{
    Breaker, BreakerKind, Choice, Context, Decision, ItemKey, Overflow, OverflowOutcome, Policy, Recent, Side,
    is_mass_sensitive, review_in,
};
use oneup_local::observe::Observer;
use oneup_local::{Config, Scan};
use oneup_plan::{Parked, names};
use oneup_rules::{ExcludeSet, RuleSet};
use oneup_store::{Kind, RemoteRow, Store};
use oneup_sync::rewind::Job;
use oneup_sync::{LocalPre, Op, RemoteId, RemotePre, all_folders, fold, plan};

// ---------------------------------------------------------------- fake cloud

#[derive(Clone)]
struct Item {
    parent: Option<String>,
    name: String,
    folder: bool,
    content: Vec<u8>,
    etag: u64,
}

#[derive(Default)]
struct State {
    items: BTreeMap<String, Item>,
    next: u64,
    /// Deleted since the last pull (what delta would report).
    deleted: Vec<String>,
    /// OneDrive's recycle bin and version history: every content the cloud
    /// can still give back.
    kept: Vec<Vec<u8>>,
    /// Bytes received by uploads, and server-side copies made.
    uploaded: u64,
    copies: u64,
    /// Server-side copies fail (as a service error would).
    copy_fails: bool,
    /// Sizes of the metadata batches sent.
    batches: Vec<usize>,
    /// OneDrive's list view threshold: a folder with more items than this
    /// below it can't be moved or renamed in one request.
    threshold: Option<usize>,
    /// The next batched move of this item is made, but its answer is lost
    /// and the request sent again on the old eTag (412).
    lost_move: Option<String>,
    /// The next folder listing first moves this item into that folder (a
    /// user on the web, while a large folder is being moved).
    wander: Option<(String, String)>,
    /// The next replacing upload commits over this edit, made in OneDrive
    /// after the upload's last check (Graph checks an upload session's
    /// If-Match only when it starts).
    race: Option<Vec<u8>>,
    /// The version history: content by eTag, and the version each file's
    /// last replacing upload went over.
    old: BTreeMap<u64, Vec<u8>>,
    replaced: BTreeMap<String, u64>,
    /// Versions in the history can't be downloaded (a service error).
    versions_fail: bool,
    /// Each file's version history, newest first: its versions' IDs (the
    /// eTag each content got), times and sizes. A rename moves the current
    /// version's time, as Graph's does (measured).
    history: BTreeMap<String, Vec<(u64, i64, i64)>>,
    /// Each file's last replacing upload: the eTag it left, and its version.
    last_upload: BTreeMap<String, (u64, u64)>,
    /// The history doesn't list the newest version for this many more
    /// looks (Graph's history can lag behind a commit).
    history_lag: u32,
    /// Versions downloaded whole from the history.
    version_downloads: u32,
    /// No room left: uploads and new folders are refused (as with HTTP
    /// 507), and the requests refused counted (a batch counts once).
    full: bool,
    refused: u64,
    /// Every request takes this long (a slow network). Every request lets
    /// other operations run before and after it either way, as the network
    /// would, so operations under way at once interleave.
    latency: Option<Duration>,
    /// Uploads and downloads under way, the most at once, and how many
    /// have finished.
    transfers: usize,
    most_transfers: usize,
    finished: usize,
    /// A download of the file of this name takes until many others could
    /// have finished, and how many did meanwhile.
    stall: Option<String>,
    finished_during_stall: Option<usize>,
    saved_meanwhile: Option<SaveDuringUpload>,
    /// The next folder made with this name finds it taken: someone made it
    /// in OneDrive meanwhile (it then exists, empty).
    beaten: Option<String>,
    /// The next folder made with this name fails (a service error).
    mkdir_fails: Option<String>,
    /// Removals of excluded items and duplicates fail (a service error).
    prune_fails: bool,
    /// The next removal first moves this item into that folder: a user on
    /// the web, between the removal's last check and its request.
    prune_moves: Option<(String, String)>,
    /// The listing lags behind the cloud (delta lag, [`Fake::lag`]): each
    /// pull delivers what the pull before would have, so it can be older
    /// than what our own requests just got back.
    lagging: bool,
    behind: Option<Listing>,
    /// Runs at the start of the next request: something a user does while
    /// a round is under way.
    meanwhile: Option<Box<dyn FnOnce() + Send>>,
    /// Told the ID of each item about to be removed (deleted or pruned),
    /// once its precondition held: what the store says at that moment.
    on_remove: Option<OnRemove>,
    /// Every upload that committed: the file's name and what it held.
    sent: Vec<(String, Vec<u8>)>,
    /// Runs once the next upload has read its first bytes: another
    /// program writing something else meanwhile.
    during_upload: Option<Box<dyn FnOnce() + Send>>,
    /// OneDrive's recycle bin, by ID: what `restore` brings back.
    bin: BTreeMap<String, Item>,
    /// Runs when the item with this ID is next asked for (`Cloud::item`):
    /// someone else acting on it between a round's plan and its step.
    on_item: Option<(String, Meanwhile)>,
    /// The next restore brings the item back under another name than
    /// asked, or (`new_id`) as another item.
    restore_odd: Option<(String, bool)>,
}

/// See [`State::on_remove`].
type OnRemove = Arc<dyn Fn(&str) + Send + std::marker::Sync>;

/// Something done to the cloud by someone else, at a chosen moment.
type Meanwhile = Box<dyn FnOnce(&mut State) + Send>;

/// What one pull delivers: every live item (parents first), then the
/// deletions since the pull before.
type Listing = (Vec<RemoteRow>, Vec<String>);

impl State {
    fn bump(&mut self) -> u64 {
        self.next += 1;
        self.next
    }
    fn child(&self, parent: &str, name: &str) -> Option<String> {
        self.items
            .iter()
            .find(|(_, it)| it.parent.as_deref() == Some(parent) && fold(&it.name) == fold(name))
            .map(|(id, _)| id.clone())
    }
    fn check(&self, pre: &RemotePre) -> Result<&Item, CloudError> {
        match self.items.get(&pre.id.0) {
            Some(it) if it.etag.to_string() == pre.etag => Ok(it),
            _ => Err(CloudError::Changed),
        }
    }
    fn row(&self, id: &str) -> RemoteRow {
        let it = &self.items[id];
        let kind = if it.parent.is_none() {
            Kind::Root
        } else if it.folder {
            Kind::Folder
        } else {
            Kind::File
        };
        RemoteRow {
            id: id.to_string(),
            parent_id: it.parent.clone(),
            name: it.name.clone(),
            kind,
            size: it.content.len() as i64,
            etag: Some(it.etag.to_string()),
            qxh: (!it.folder).then(|| qxh(&it.content)),
            mtime_ms: None,
        }
    }
    fn descendants(&self, id: &str) -> usize {
        let mut todo = vec![id.to_string()];
        let mut n = 0;
        while let Some(p) = todo.pop() {
            for (cid, _) in self.items.iter().filter(|(_, it)| it.parent.as_deref() == Some(&p)) {
                n += 1;
                todo.push(cid.clone());
            }
        }
        n
    }
    /// What a pull delivers now (see [`Listing`]).
    fn listing(&mut self) -> Listing {
        let mut order = vec!["root".to_string()];
        let mut i = 0;
        while i < order.len() {
            let p = order[i].clone();
            order
                .extend(self.items.iter().filter(|(_, it)| it.parent.as_deref() == Some(&p)).map(|(id, _)| id.clone()));
            i += 1;
        }
        let rows = order.iter().map(|id| self.row(id)).collect();
        (rows, std::mem::take(&mut self.deleted))
    }
    fn create(&mut self, parent: &str, name: &str, folder: bool, content: Vec<u8>) -> String {
        let id = format!("r{}", self.bump());
        let etag = self.bump();
        if !folder {
            self.history.entry(id.clone()).or_default().insert(0, (etag, etag as i64, content.len() as i64));
        }
        self.items.insert(id.clone(), Item { parent: Some(parent.into()), name: name.into(), folder, content, etag });
        id
    }
}

fn qxh(b: &[u8]) -> [u8; 20] {
    let mut h = oneup_hash::QuickXorHash::new();
    h.update(b);
    h.finalize()
}

/// The next upload, once it has read its first bytes, sees the file saved
/// in place with `content`.
struct SaveDuringUpload {
    path: PathBuf,
    content: Vec<u8>,
    /// Saved after the upload read its last byte, at the same size and
    /// with the file's time put back, so its stamp doesn't change.
    after_last_byte: bool,
    /// Saved by another program (`cmd.exe`), as Files On-Demand needs.
    by_app: bool,
    /// Saved by another program writing a new file and renaming it over
    /// the old one with `MoveFileEx` (`cmd`'s `move /y`), which fails
    /// while any other handle on the file is open.
    by_move: bool,
}

impl SaveDuringUpload {
    fn in_place(path: PathBuf, content: &[u8]) -> Self {
        Self { path, content: content.to_vec(), after_last_byte: false, by_app: false, by_move: false }
    }

    fn save(self) {
        if self.by_move {
            let tmp = self.path.with_extension("saving");
            std::fs::write(&tmp, &self.content).unwrap();
            let out = std::process::Command::new("cmd.exe")
                .current_dir(self.path.parent().unwrap())
                .args(["/c", "move", "/y"])
                .arg(tmp.file_name().unwrap())
                .arg(self.path.file_name().unwrap())
                .output()
                .unwrap();
            assert!(out.status.success(), "the save must not fail: {out:?}");
            return;
        }
        if self.by_app {
            use std::os::windows::process::CommandExt;
            let text = String::from_utf8(self.content).unwrap();
            let st = std::process::Command::new("cmd")
                .raw_arg(format!("/C echo {text}>\"{}\"", self.path.display()))
                .status();
            assert!(st.unwrap().success(), "the save must not fail");
            return;
        }
        let modified = std::fs::metadata(&self.path).unwrap().modified().unwrap();
        let mut f = std::fs::OpenOptions::new().write(true).truncate(!self.after_last_byte).open(&self.path).unwrap();
        f.write_all(&self.content).unwrap();
        if self.after_last_byte {
            f.set_modified(modified).unwrap();
        }
    }
}

#[derive(Clone)]
struct Fake(Arc<Mutex<State>>);

impl Fake {
    fn new() -> Self {
        let mut s = State::default();
        s.items
            .insert("root".into(), Item { parent: None, name: String::new(), folder: true, content: vec![], etag: 0 });
        Self(Arc::new(Mutex::new(s)))
    }

    // A user working in the cloud (e.g. on the web).
    fn add(&self, path: &str, content: Option<&[u8]>) -> String {
        let mut s = self.0.lock().unwrap();
        let (parent, name) = resolve_parent(&s, path);
        s.create(&parent, &name, content.is_none(), content.unwrap_or_default().to_vec())
    }
    fn edit(&self, path: &str, content: &[u8]) {
        let id = resolve(&self.0.lock().unwrap(), path);
        self.edit_id(&id, content);
    }
    fn edit_id(&self, id: &str, content: &[u8]) {
        let mut s = self.0.lock().unwrap();
        let etag = s.bump();
        let it = s.items.get_mut(id).unwrap();
        let over = it.etag;
        let old = std::mem::replace(&mut it.content, content.to_vec());
        it.etag = etag;
        s.kept.push(old.clone());
        s.old.insert(over, old);
        s.history.entry(id.to_string()).or_default().insert(0, (etag, etag as i64, content.len() as i64));
    }
    fn rename(&self, path: &str, name: &str) {
        let mut s = self.0.lock().unwrap();
        let id = resolve(&s, path);
        let etag = s.bump();
        let it = s.items.get_mut(&id).unwrap();
        it.name = name.into();
        it.etag = etag;
        if let Some(v) = s.history.get_mut(&id).and_then(|h| h.first_mut()) {
            v.1 = etag as i64;
        }
    }
    /// Moves `path` into folder `dir`.
    fn mv(&self, path: &str, dir: &str) {
        let mut s = self.0.lock().unwrap();
        let id = resolve(&s, path);
        let to = resolve(&s, dir);
        let etag = s.bump();
        let it = s.items.get_mut(&id).unwrap();
        it.parent = Some(to);
        it.etag = etag;
    }
    fn delete(&self, path: &str) {
        let mut s = self.0.lock().unwrap();
        let id = resolve(&s, path);
        let it = s.items.remove(&id).unwrap();
        s.kept.push(it.content.clone());
        s.bin.insert(id.clone(), it);
        s.deleted.push(id);
    }

    /// Empties OneDrive's recycle bin (what's in it can't come back).
    fn empty_bin(&self) {
        self.0.lock().unwrap().bin.clear();
    }

    /// What delta would deliver: every live item (parents first), then the
    /// deletions since last time. While the listing lags, what the pull
    /// before would have delivered instead.
    fn pull(&self, store: &mut Store) {
        let mut s = self.0.lock().unwrap();
        let now = s.listing();
        let deliveries = match (s.lagging, s.behind.take()) {
            (true, behind) => {
                s.behind = Some(now);
                behind.into_iter().collect()
            }
            (false, Some(behind)) => vec![behind, now],
            (false, None) => vec![now],
        };
        drop(s);
        for (rows, deleted) in deliveries {
            store
                .write(|o| {
                    for r in &rows {
                        o.upsert_item(r, false)?;
                    }
                    for id in &deleted {
                        o.delete_subtree(id)?;
                    }
                    Ok(())
                })
                .unwrap();
        }
    }

    /// From now on the listing lags a pull behind the cloud (delta lag).
    fn lag(&self) {
        let mut s = self.0.lock().unwrap();
        s.behind = Some(s.listing());
        s.lagging = true;
    }

    /// The listing catches up: the next pull delivers what it held back,
    /// then the cloud as it is.
    fn catch_up(&self) {
        self.0.lock().unwrap().lagging = false;
    }

    fn tree(&self) -> Tree {
        let s = self.0.lock().unwrap();
        let mut out = Tree::new();
        let mut stack = vec![("root".to_string(), String::new())];
        while let Some((id, path)) = stack.pop() {
            for (cid, it) in s.items.iter().filter(|(_, it)| it.parent.as_deref() == Some(&id)) {
                let p = format!("{path}/{}", it.name);
                out.insert(p.clone(), (!it.folder).then(|| it.content.clone()));
                if it.folder {
                    stack.push((cid.clone(), p));
                }
            }
        }
        out
    }

    fn holds(&self, content: &[u8]) -> bool {
        let s = self.0.lock().unwrap();
        s.items.values().any(|it| it.content == content) || s.kept.iter().any(|k| k == content)
    }
}

impl Fake {
    /// [`Cloud::mkdir`], alone or in a batch.
    fn make_dir(&self, parent: &RemoteId, name: &str) -> Result<RemoteRow, CloudError> {
        let mut s = self.0.lock().unwrap();
        if s.full {
            return Err(CloudError::Full);
        }
        if !s.items.get(&parent.0).is_some_and(|p| p.folder) {
            return Err(CloudError::Changed);
        }
        if s.mkdir_fails.as_deref() == Some(name) {
            s.mkdir_fails = None;
            return Err(CloudError::Other("service unavailable".into()));
        }
        if s.beaten.as_deref() == Some(name) {
            s.beaten = None;
            s.create(&parent.0, name, true, vec![]);
        }
        if s.child(&parent.0, name).is_some() {
            return Err(CloudError::NameTaken);
        }
        let id = s.create(&parent.0, name, true, vec![]);
        Ok(s.row(&id))
    }
}

fn resolve(s: &State, path: &str) -> String {
    let mut cur = "root".to_string();
    for part in path.split('/').filter(|p| !p.is_empty()) {
        cur = s.child(&cur, part).unwrap_or_else(|| panic!("no {path} in the cloud"));
    }
    cur
}

fn resolve_parent(s: &State, path: &str) -> (String, String) {
    let (dir, name) = path.rsplit_once('/').unwrap_or(("", path));
    (resolve(s, dir), name.to_string())
}

/// An upload or download under way ([`Fake::request`]).
struct Transfer<'a>(&'a Fake);

impl Drop for Transfer<'_> {
    fn drop(&mut self) {
        let mut s = self.0.0.lock().unwrap();
        s.transfers -= 1;
        s.finished += 1;
    }
}

impl Fake {
    /// The start of a request: other operations run first (and it takes
    /// `latency`).
    async fn request(&self) {
        tokio::task::yield_now().await;
        let meanwhile = self.0.lock().unwrap().meanwhile.take();
        if let Some(f) = meanwhile {
            f();
        }
        let latency = self.0.lock().unwrap().latency;
        if let Some(d) = latency {
            tokio::time::sleep(d).await;
        }
    }

    /// The end of a request: its answer is on its way back.
    async fn answer(&self) {
        tokio::task::yield_now().await;
    }

    /// A request whose effect is `f`: other operations run before it and
    /// after it, before its answer is back.
    async fn exchange<T>(&self, f: impl FnOnce() -> T) -> T {
        self.request().await;
        let done = f();
        self.answer().await;
        done
    }

    /// [`Fake::request`] for a transfer, counted while it lasts.
    async fn transfer(&self) -> Transfer<'_> {
        {
            let mut s = self.0.lock().unwrap();
            s.transfers += 1;
            s.most_transfers = s.most_transfers.max(s.transfers);
        }
        let t = Transfer(self);
        self.request().await;
        t
    }

    /// The most uploads and downloads that were under way at once.
    fn most_transfers(&self) -> usize {
        self.0.lock().unwrap().most_transfers
    }

    fn download_now(&self, pre: &RemotePre, out: &mut (dyn Write + Send)) -> Result<Option<[u8; 20]>, CloudError> {
        let content = {
            let s = self.0.lock().unwrap();
            let it = s.check(pre)?;
            if it.folder {
                return Err(CloudError::Other("not a file".into()));
            }
            it.content.clone()
        };
        // In pieces, as data arrives over the network.
        for piece in content.chunks(4096) {
            out.write_all(piece)?;
        }
        Ok(None)
    }

    /// Reads an upload's `size` bytes as Graph does, through
    /// [`Source::fill`]: the first two, then the rest (a save the test
    /// asked for lands in between, or after the last byte).
    async fn read_upload(&self, size: i64, src: &mut dyn Source) -> Result<Vec<u8>, CloudError> {
        let size = usize::try_from(size).unwrap();
        let first = size.min(2);
        let (mut content, read) = src.fill(vec![0; first], first).await;
        assert_eq!(read?, first, "the file ended early");
        let save = self.0.lock().unwrap().saved_meanwhile.take();
        let (now, last) = match save {
            Some(s) if s.after_last_byte => (None, Some(s)),
            s => (s, None),
        };
        if let Some(s) = now {
            s.save();
        }
        let during = self.0.lock().unwrap().during_upload.take();
        if let Some(f) = during {
            f();
        }
        // One more than is left: a file longer than its size shows.
        let (rest, read) = src.fill(vec![0; size - first + 1], size - first + 1).await;
        content.extend_from_slice(&rest[..read?]);
        if let Some(s) = last {
            s.save();
        }
        assert_eq!(content.len(), size);
        Ok(content)
    }

    fn upload_now(
        &self,
        parent: &RemoteId,
        name: &str,
        replace: Option<&RemotePre>,
        content: Vec<u8>,
    ) -> Result<(RemoteRow, Option<[u8; 20]>), CloudError> {
        let mut s = self.0.lock().unwrap();
        if s.full {
            s.refused += 1;
            return Err(CloudError::Full);
        }
        s.uploaded += content.len() as u64;
        s.sent.push((name.to_string(), content.clone()));
        let id = match replace {
            Some(pre) => {
                s.check(pre)?;
                if let Some(theirs) = s.race.take() {
                    drop(s);
                    self.edit_id(&pre.id.0, &theirs);
                    s = self.0.lock().unwrap();
                }
                let etag = s.bump();
                // The version gone over, as its history names it.
                let over = s.history.get(&pre.id.0).and_then(|h| h.first()).map_or(0, |v| v.0);
                let len = content.len() as i64;
                let it = s.items.get_mut(&pre.id.0).unwrap();
                let old = std::mem::replace(&mut it.content, content);
                it.etag = etag;
                s.kept.push(old.clone());
                s.old.insert(over, old);
                s.replaced.insert(pre.id.0.clone(), over);
                s.history.entry(pre.id.0.clone()).or_default().insert(0, (etag, etag as i64, len));
                s.last_upload.insert(pre.id.0.clone(), (etag, etag));
                pre.id.0.clone()
            }
            None => {
                if !s.items.get(&parent.0).is_some_and(|p| p.folder) {
                    return Err(CloudError::Changed);
                }
                if s.child(&parent.0, name).is_some() {
                    return Err(CloudError::NameTaken);
                }
                s.create(&parent.0, name, false, content)
            }
        };
        Ok((s.row(&id), None))
    }
}

impl Cloud for Fake {
    fn concurrent(&self) -> bool {
        true
    }

    fn moves_keep_ids(&self) -> bool {
        true
    }

    async fn download(&self, pre: &RemotePre, out: &mut (dyn Write + Send)) -> Result<Option<[u8; 20]>, CloudError> {
        let _t = self.transfer().await;
        let stalled = {
            let s = self.0.lock().unwrap();
            let name = s.items.get(&pre.id.0).map(|it| it.name.clone());
            s.stall.is_some() && s.stall == name
        };
        if stalled {
            let from = self.0.lock().unwrap().finished;
            for _ in 0..20_000 {
                tokio::task::yield_now().await;
            }
            let mut s = self.0.lock().unwrap();
            s.finished_during_stall = Some(s.finished - from);
        }
        let done = self.download_now(pre, out);
        self.answer().await;
        done
    }

    async fn upload(
        &self,
        parent: &RemoteId,
        name: &str,
        replace: Option<&RemotePre>,
        size: i64,
        src: &mut dyn Source,
    ) -> Result<(RemoteRow, Option<[u8; 20]>), CloudError> {
        let _t = self.transfer().await;
        let done = match self.read_upload(size, src).await {
            Ok(content) => self.upload_now(parent, name, replace, content),
            Err(e) => Err(e),
        };
        self.answer().await;
        done
    }

    async fn copy(
        &self,
        source: &RemotePre,
        hash: &[u8; 20],
        parent: &RemoteId,
        name: &str,
    ) -> Result<RemoteRow, CloudError> {
        self.exchange(|| -> Result<RemoteRow, CloudError> {
            let mut s = self.0.lock().unwrap();
            if s.copy_fails {
                return Err(CloudError::Other("copy failed".into()));
            }
            let content = s.check(source)?.content.clone();
            if qxh(&content) != *hash {
                return Err(CloudError::Changed);
            }
            if !s.items.get(&parent.0).is_some_and(|p| p.folder) {
                return Err(CloudError::Changed);
            }
            if s.child(&parent.0, name).is_some() {
                return Err(CloudError::NameTaken);
            }
            s.copies += 1;
            let id = s.create(&parent.0, name, false, content);
            Ok(s.row(&id))
        })
        .await
    }

    async fn mkdir(&self, parent: &RemoteId, name: &str) -> Result<RemoteRow, CloudError> {
        self.exchange(|| -> Result<RemoteRow, CloudError> {
            let made = self.make_dir(parent, name);
            if matches!(made, Err(CloudError::Full)) {
                self.0.lock().unwrap().refused += 1;
            }
            made
        })
        .await
    }

    async fn move_item(&self, pre: &RemotePre, to: &RemoteId, name: &str) -> Result<RemoteRow, CloudError> {
        self.exchange(|| -> Result<RemoteRow, CloudError> {
            let mut s = self.0.lock().unwrap();
            s.check(pre)?;
            if s.child(&to.0, name).is_some_and(|c| c != pre.id.0) {
                return Err(CloudError::NameTaken);
            }
            if s.threshold.is_some_and(|t| s.descendants(&pre.id.0) > t) {
                return Err(CloudError::TooManyItems);
            }
            let etag = s.bump();
            let it = s.items.get_mut(&pre.id.0).unwrap();
            it.parent = Some(to.0.clone());
            it.name = name.into();
            it.etag = etag;
            Ok(s.row(&pre.id.0))
        })
        .await
    }

    async fn item(&self, id: &RemoteId) -> Result<Option<RemoteRow>, CloudError> {
        {
            let mut s = self.0.lock().unwrap();
            if s.on_item.as_ref().is_some_and(|(at, _)| *at == id.0) {
                let (_, f) = s.on_item.take().unwrap();
                f(&mut s);
            }
        }
        self.exchange(|| -> Result<Option<RemoteRow>, CloudError> {
            let s = self.0.lock().unwrap();
            Ok(s.items.contains_key(&id.0).then(|| s.row(&id.0)))
        })
        .await
    }

    async fn is_at(&self, id: &RemoteId, parent: &str, name: &str) -> Result<Option<bool>, CloudError> {
        let now = self.item(id).await?;
        Ok(Some(now.is_some_and(|r| r.parent_id.as_deref() == Some(parent) && r.name == name)))
    }

    /// Every replacement is checked afterwards, as a large one over Graph.
    async fn version_mark(&self, pre: &RemotePre, _: i64) -> Result<Option<VersionMark>, CloudError> {
        self.exchange(|| -> Result<Option<VersionMark>, CloudError> {
            let s = self.0.lock().unwrap();
            s.check(pre)?;
            let newest = s.history.get(&pre.id.0).and_then(|h| h.first()).copied();
            Ok(newest.map(|(t, at, size)| VersionMark { id: t.to_string(), modified_ms: Some(at), size: Some(size) }))
        })
        .await
    }

    /// As Graph's, from the commit's answer: only for the upload that left
    /// `etag`.
    async fn uploaded_version(&self, id: &RemoteId, etag: Option<&str>) -> Result<Option<VersionMark>, CloudError> {
        self.exchange(|| -> Result<Option<VersionMark>, CloudError> {
            let s = self.0.lock().unwrap();
            let Some(&(left, v)) = s.last_upload.get(&id.0) else { return Ok(None) };
            if etag != Some(left.to_string().as_str()) {
                return Ok(None);
            }
            let size = s.history.get(&id.0).and_then(|h| h.iter().find(|e| e.0 == v)).map(|e| e.2);
            Ok(Some(VersionMark { id: v.to_string(), modified_ms: None, size }))
        })
        .await
    }

    /// As Graph's: the history, without its newest entry while it lags.
    async fn replaced_other(
        &self,
        id: &RemoteId,
        mark: &VersionMark,
        ours: &VersionMark,
    ) -> Result<Replaced, CloudError> {
        self.exchange(|| -> Result<Replaced, CloudError> {
            let mut s = self.0.lock().unwrap();
            let history = s.history.get(&id.0).cloned().unwrap_or_default();
            let mut listed: Vec<oneup_graph::FileVersion> = history
                .iter()
                .map(|&(t, at, size)| oneup_graph::FileVersion {
                    id: t.to_string(),
                    modified_ms: Some(at),
                    size: Some(size),
                    by: None,
                })
                .collect();
            if s.history_lag > 0 && !listed.is_empty() {
                s.history_lag -= 1;
                listed.remove(0);
            }
            Ok(oneup_exec::graph::replaced_version(&listed, mark, ours))
        })
        .await
    }

    async fn download_version(
        &self,
        _: &RemoteId,
        version: &str,
        out: &mut (dyn Write + Send),
    ) -> Result<u64, CloudError> {
        self.exchange(|| -> Result<u64, CloudError> {
            let s = self.0.lock().unwrap();
            if s.versions_fail {
                return Err(CloudError::Other("HTTP 500".into()));
            }
            let content = s.old.get(&version.parse().unwrap()).cloned();
            drop(s);
            let content = content.ok_or_else(|| CloudError::Other("no such version".into()))?;
            out.write_all(&content)?;
            self.0.lock().unwrap().version_downloads += 1;
            Ok(content.len() as u64)
        })
        .await
    }

    /// Three at a time, so a folder takes several listings.
    async fn children(&self, id: &RemoteId) -> Result<Vec<RemoteRow>, CloudError> {
        self.exchange(|| -> Result<Vec<RemoteRow>, CloudError> {
            let mut s = self.0.lock().unwrap();
            if let Some((item, to)) = s.wander.take() {
                let etag = s.bump();
                let it = s.items.get_mut(&item).unwrap();
                it.parent = Some(to);
                it.etag = etag;
            }
            let ids = s.items.iter().filter(|(_, it)| it.parent.as_deref() == Some(&id.0)).map(|(id, _)| id).take(3);
            Ok(ids.map(|id| s.row(id)).collect())
        })
        .await
    }

    async fn prune(&self, pre: &RemotePre) -> Result<(), CloudError> {
        self.exchange(|| -> Result<(), CloudError> {
            let mut s = self.0.lock().unwrap();
            if s.prune_fails {
                return Err(CloudError::Other("the service refused".into()));
            }
            if let Some((path, into)) = s.prune_moves.take() {
                let (id, to) = (resolve(&s, &path), resolve(&s, &into));
                let etag = s.bump();
                let it = s.items.get_mut(&id).unwrap();
                it.parent = Some(to);
                it.etag = etag;
            }
            s.check(pre)?;
            if let Some(f) = &s.on_remove {
                f(&pre.id.0);
            }
            let mut gone = vec![pre.id.0.clone()];
            let mut i = 0;
            while i < gone.len() {
                let p = gone[i].clone();
                gone.extend(
                    s.items.iter().filter(|(_, it)| it.parent.as_deref() == Some(&p)).map(|(id, _)| id.clone()),
                );
                i += 1;
            }
            for id in gone {
                let it = s.items.remove(&id).unwrap();
                s.kept.push(it.content.clone());
                s.bin.insert(id.clone(), it);
                s.deleted.push(id);
            }
            Ok(())
        })
        .await
    }

    /// Newest first, the current version included (its eTag is its ID).
    async fn versions(&self, id: &RemoteId) -> Result<Vec<VersionMark>, CloudError> {
        self.exchange(|| -> Result<Vec<VersionMark>, CloudError> {
            let s = self.0.lock().unwrap();
            if !s.items.contains_key(&id.0) {
                return Err(CloudError::Changed);
            }
            Ok(s.history
                .get(&id.0)
                .into_iter()
                .flatten()
                .map(|&(t, at, size)| VersionMark { id: t.to_string(), modified_ms: Some(at), size: Some(size) })
                .collect())
        })
        .await
    }

    async fn restore(&self, id: &RemoteId, parent: &RemoteId, name: &str) -> Result<RemoteRow, CloudError> {
        self.exchange(|| -> Result<RemoteRow, CloudError> {
            let mut s = self.0.lock().unwrap();
            if !s.bin.contains_key(&id.0) || !s.items.get(&parent.0).is_some_and(|p| p.folder) {
                return Err(CloudError::Changed);
            }
            if s.child(&parent.0, name).is_some() {
                return Err(CloudError::NameTaken);
            }
            let etag = s.bump();
            let mut it = s.bin.remove(&id.0).unwrap();
            it.parent = Some(parent.0.clone());
            it.name = name.into();
            it.etag = etag;
            let mut at = id.0.clone();
            if let Some((other, new_id)) = s.restore_odd.take() {
                it.name = other;
                if new_id {
                    at = format!("r{}", s.bump());
                }
            }
            s.items.insert(at.clone(), it);
            Ok(s.row(&at))
        })
        .await
    }

    /// As Graph's batch: every change judged against the state before any
    /// of them (a folder still holds what another change in the batch
    /// removes), then applied in order.
    async fn batch(&self, ops: &[Meta]) -> Vec<Result<MetaDone, CloudError>> {
        self.request().await;
        let before: Vec<Result<(), CloudError>> = {
            let mut s = self.0.lock().unwrap();
            s.batches.push(ops.len());
            ops.iter()
                .map(|m| match m {
                    Meta::Delete { pre } => {
                        s.check(pre)?;
                        if s.items.values().any(|it| it.parent.as_deref() == Some(&pre.id.0)) {
                            return Err(CloudError::NotEmpty);
                        }
                        Ok(())
                    }
                    _ => Ok(()),
                })
                .collect()
        };
        let mut out = Vec::new();
        for (m, ok) in ops.iter().zip(before) {
            out.push(match (m, ok) {
                (_, Err(e)) => Err(e),
                (Meta::Mkdir { parent, name }, Ok(())) => self.make_dir(parent, name).map(MetaDone::Item),
                (Meta::Move { pre, to, name }, Ok(())) => {
                    let done = self.move_item(pre, to, name).await.map(MetaDone::Item);
                    let mut s = self.0.lock().unwrap();
                    if done.is_ok() && s.lost_move.as_deref() == Some(pre.id.0.as_str()) {
                        s.lost_move = None;
                        Err(CloudError::Changed)
                    } else {
                        done
                    }
                }
                (Meta::Delete { pre }, Ok(())) => Cloud::delete(self, pre).await.map(|()| MetaDone::Deleted),
            });
        }
        if out.iter().any(|r| matches!(r, Err(CloudError::Full))) {
            self.0.lock().unwrap().refused += 1;
        }
        self.answer().await;
        out
    }

    async fn delete(&self, pre: &RemotePre) -> Result<(), CloudError> {
        self.exchange(|| -> Result<(), CloudError> {
            let mut s = self.0.lock().unwrap();
            s.check(pre)?;
            if s.items.values().any(|it| it.parent.as_deref() == Some(&pre.id.0)) {
                return Err(CloudError::NotEmpty);
            }
            if let Some(f) = &s.on_remove {
                f(&pre.id.0);
            }
            let it = s.items.remove(&pre.id.0).unwrap();
            s.kept.push(it.content.clone());
            s.bin.insert(pre.id.0.clone(), it);
            s.deleted.push(pre.id.0.clone());
            Ok(())
        })
        .await
    }
}

// ---------------------------------------------------------------- harness

/// Path → content (`None` for folders).
type Tree = BTreeMap<String, Option<Vec<u8>>>;

struct Tmp(PathBuf);

impl Drop for Tmp {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.0);
    }
}

/// A state database next to the sync folder, removed afterwards.
struct DbFile(PathBuf);

impl Drop for DbFile {
    fn drop(&mut self) {
        for ext in ["", "-wal", "-shm"] {
            let _ = std::fs::remove_file(format!("{}{ext}", self.0.display()));
        }
    }
}

fn local_tree(root: &Path) -> Tree {
    let mut out = Tree::new();
    // Verbatim (`\\?\`) paths, so names Win32 would normalise (a trailing
    // dot or space) can still be opened.
    let root = PathBuf::from(format!(r"\\?\{}", root.display()));
    let mut stack = vec![(root, String::new())];
    while let Some((dir, path)) = stack.pop() {
        for e in std::fs::read_dir(&dir).unwrap().flatten() {
            let name = e.file_name().into_string().unwrap();
            if path.is_empty() && name == ".oneup" {
                continue;
            }
            let p = format!("{path}/{name}");
            if e.file_type().unwrap().is_dir() {
                out.insert(p.clone(), None);
                stack.push((e.path(), p));
            } else {
                out.insert(p, Some(std::fs::read(e.path()).unwrap()));
            }
        }
    }
    out
}

struct Sync {
    dir: Tmp,
    /// Set for a database on disk, which [`Sync::restart`] needs.
    db: Option<DbFile>,
    obs: Observer,
    exec: Executor<Fake>,
    cloud: Fake,
    policy: Policy,
    /// The version store's room, as the caller would work it out.
    room: Option<u64>,
    decisions: Vec<Decision>,
    /// The last round's overflow batch.
    overflow: Option<Overflow>,
    /// The user's rules, and the fingerprint the last clean round planned
    /// with (a change plans every folder, as `oneup_app::sync` does).
    rules: RuleSet,
    planned_rules: String,
    /// `cloud_copies = "pull"`: excluded items are planned but never sent.
    pull: bool,
    /// What goes into the folders a round makes is planned and run in the
    /// same round (`Executor::run_following`), as `oneup_app::sync` does.
    follow: bool,
    /// At most this many operations taken in for new folders per round.
    follow_max: usize,
    /// Added to the next round's plan, as the agent adds the renames the
    /// user chose (`oneup_exec::resolve`).
    extra: Vec<Op>,
    /// The next round's `extra` wasn't answered by the user: its recovery
    /// steps aren't approved.
    unanswered: bool,
    /// Safety stops, as the agent handles them (`oneup_app::sync`).
    stops: Stops,
}

/// Safety stops in the harness, as the agent handles them: a tripped round
/// runs nothing and marks its list's folders for planning; an approved
/// list's items stay approved; with `window`, what ran is counted like the
/// agent's safety window.
#[derive(Default)]
struct Stops {
    /// Stops may trip (otherwise a round asserts none does).
    allowed: bool,
    /// The next round plans every folder (as the agent's first does).
    full_next: bool,
    window: Option<Recent>,
    approved: Vec<ItemKey>,
    /// The last round's stops.
    raised: Vec<Breaker>,
    /// A rewind under way, as the agent runs one (`oneup_app::rewind`).
    rewind: Option<Rewinding>,
}

/// A rewind: of `folder` to moment `at`. Until the user approves, each
/// round adds every step (and its stop shows them); once approved, only the
/// steps its approval still covers ([`Job::accepts`]).
#[derive(Default)]
struct Rewinding {
    folder: oneup_sync::Sid,
    at: i64,
    job: Option<Job>,
    /// The steps of the stop last shown.
    preview: Vec<Op>,
    /// Steps planned after the approval that it doesn't cover (their item
    /// changed after the preview), by round.
    left_alone: usize,
    /// Rounds in a row whose steps all failed or were skipped: after
    /// [`STALLS`], the rewind ends with what it could do, as the agent's.
    stalls: usize,
    /// Every step it approved has run, was left alone, or can't be done.
    done: bool,
    /// The code the user approved, taken at the next round only if the
    /// list planned then has it (as the agent takes it).
    answer: Option<u64>,
    /// Approvals not taken: the list had changed since it was shown.
    refused: usize,
}

/// See [`Rewinding::stalls`].
const STALLS: usize = 3;

impl Stops {
    /// The user approves the stops standing (a rewind's with them: its
    /// code, which the next round takes only for the same list).
    fn approve(&mut self) {
        // Asked for a rewind, the user approves that, not other stops.
        let rewind = self.raised.iter().any(|b| b.kind == BreakerKind::Rewind);
        for b in std::mem::take(&mut self.raised) {
            if rewind && b.kind != BreakerKind::Rewind {
                continue;
            }
            if b.kind == BreakerKind::Rewind {
                if let Some(r) = &mut self.rewind {
                    r.answer = Some(b.fingerprint.0);
                }
                continue;
            }
            self.approved.extend(b.items);
        }
        self.approved.sort_unstable();
    }
}

impl Sync {
    fn new(tag: &str, cloud: Fake) -> Self {
        let dir = Tmp(std::env::temp_dir().join(format!("oneup-exec-{tag}-{}", std::process::id())));
        let _ = std::fs::remove_dir_all(&dir.0);
        std::fs::create_dir_all(&dir.0).unwrap();
        Self::with_dir(dir, cloud)
    }

    fn with_dir(dir: Tmp, cloud: Fake) -> Self {
        Self::open(dir, None, cloud)
    }

    /// With the state database on disk, so the test can [`Sync::restart`].
    fn on_disk(tag: &str, cloud: Fake) -> Self {
        let dir = Tmp(std::env::temp_dir().join(format!("oneup-exec-{tag}-{}", std::process::id())));
        let _ = std::fs::remove_dir_all(&dir.0);
        std::fs::create_dir_all(&dir.0).unwrap();
        let path = PathBuf::from(format!("{}.db", dir.0.display()));
        // Whatever an earlier, aborted run left behind.
        drop(DbFile(path.clone()));
        let db = DbFile(path);
        Self::open(dir, Some(db), cloud)
    }

    fn open(dir: Tmp, db: Option<DbFile>, cloud: Fake) -> Self {
        let rules = ExcludeSet::none();
        let store = match &db {
            Some(f) => Store::open(&f.0, 4096).unwrap(),
            None => Store::open_in_memory().unwrap(),
        };
        let mut scan = Scan::open(store, &dir.0, Config::new(rules.clone())).unwrap();
        assert!(scan.run().unwrap().finished);
        let mut store = scan.into_store();
        let root = store.read(|o| o.local_root()).unwrap().unwrap();
        assert!(store.write(|o| link_roots(o, root.root_file_id, "root")).unwrap());
        let (obs, exec) = Self::start(&dir.0, store, &cloud);
        Self {
            dir,
            db,
            obs,
            exec,
            cloud,
            policy: Policy::default(),
            room: None,
            decisions: Vec::new(),
            overflow: None,
            rules: RuleSet::default(),
            planned_rules: format!("{} ", RuleSet::default().fingerprint()),
            pull: false,
            follow: true,
            follow_max: oneup_exec::FOLLOW_MAX,
            extra: Vec::new(),
            unanswered: false,
            stops: Stops::default(),
        }
    }

    fn start(dir: &Path, store: Store, cloud: &Fake) -> (Observer, Executor<Fake>) {
        let rules = ExcludeSet::none();
        let root = store.read(|o| o.local_root()).unwrap().unwrap();
        let mut fs = LocalFs::open(dir, root.volume_serial, root.root_file_id, "test-token", true).unwrap();
        // As the app does: the effects find items where the Local tree has
        // them, through their own connection (a database on disk only).
        if let Some(reader) = store.reopen(256).unwrap() {
            let reader = std::sync::Mutex::new(reader);
            fs.set_locator(Box::new(move |id| reader.lock().unwrap().read(|o| o.local_path_of(id.0)).unwrap()));
        }
        let obs = Observer::open(store, Config::new(rules.clone())).unwrap();
        (obs, Executor::new(fs, cloud.clone(), rules))
    }

    /// The process was killed and starts again: everything in memory is
    /// gone; the database, the folder and the cloud stay as they are.
    fn restart(self) -> Self {
        let Self {
            dir,
            db,
            obs,
            exec,
            cloud,
            policy,
            room,
            decisions,
            overflow: _,
            rules,
            planned_rules,
            pull,
            follow,
            follow_max,
            extra,
            unanswered,
            stops,
        } = self;
        drop((obs, exec));
        let store = Store::open(&db.as_ref().expect("restart needs a database on disk").0, 4096).unwrap();
        let (obs, exec) = Self::start(&dir.0, store, &cloud);
        Self {
            dir,
            db,
            obs,
            exec,
            cloud,
            policy,
            room,
            decisions,
            overflow: None,
            rules,
            planned_rules,
            pull,
            follow,
            follow_max,
            extra,
            unanswered,
            stops,
        }
    }

    /// The change journal lost track of the folder while a round ran (it
    /// wrapped past the observer's position): what `oneup_app::sync` does
    /// then. With `watch`, the agent's journal watcher read the records
    /// since that position as they were written, keeping the delete records
    /// of items in the trees (`remember_deletes`); without, it lost them
    /// too. The folder is rescanned (journal position first), the delete
    /// records of items the rescan didn't find become evidence
    /// (`note_seen_deletes`), and the observer goes on from the rescan.
    /// Returns how many became evidence.
    fn lose_journal(self, watch: bool) -> (Self, u64) {
        use oneup_local::observe::{JournalTail, SeenDeletes, note_seen_deletes, remember_deletes};
        let Self {
            dir,
            db,
            obs,
            exec,
            cloud,
            policy,
            room,
            decisions,
            overflow,
            rules,
            planned_rules,
            pull,
            follow,
            follow_max,
            extra,
            unanswered,
            stops,
        } = self;
        let seen = SeenDeletes::new();
        if watch {
            let mut tail = JournalTail::open(&dir.0, Some(obs.position())).unwrap();
            loop {
                let t = tail.next(false).unwrap();
                assert!(!t.lost, "the journal wrapped past the watcher");
                if t.records.is_empty() {
                    break;
                }
                remember_deletes(obs.store(), &seen, tail.position().0, &t.records).unwrap();
            }
        }
        let cfg = Config::new(ExcludeSet::none());
        let mut scan = Scan::open(obs.into_store(), &dir.0, cfg.clone()).unwrap();
        assert_eq!(scan.start(), oneup_local::Start::Replaced(oneup_local::Replaced::Finished));
        assert!(scan.run().unwrap().finished);
        let mut store = scan.into_store();
        let noted = note_seen_deletes(&mut store, &seen).unwrap();
        let mut obs = Observer::open(store, cfg).unwrap();
        obs.catch_up().unwrap();
        let s = Self {
            dir,
            db,
            obs,
            exec,
            cloud,
            policy,
            room,
            decisions,
            overflow,
            rules,
            planned_rules,
            pull,
            follow,
            follow_max,
            extra,
            unanswered,
            stops,
        };
        (s, noted)
    }

    fn p(&self, rel: &str) -> PathBuf {
        self.dir.0.join(rel)
    }

    async fn round(&mut self) -> Report {
        let report = self.attempt().await;
        assert!(report.halted.is_none(), "{report:?}");
        assert!(report.failed.is_empty(), "{report:?}");
        report
    }

    /// A round that may halt (an injected crash).
    async fn attempt(&mut self) -> Report {
        self.attempt_checked(&|_| true).await
    }

    /// [`Sync::attempt`], the executor asking `may_run` before each
    /// operation (as the agent's pause does).
    async fn attempt_checked(&mut self, may_run: &(dyn Fn(&Op) -> bool + std::marker::Sync)) -> Report {
        self.obs.catch_up().unwrap();
        // Reading OneDrive's changes is a request too (the delta listing).
        self.cloud.request().await;
        let extra = std::mem::take(&mut self.extra);
        let asked = extra.clone();
        let store = self.obs.store_mut();
        self.cloud.pull(store);
        // Only the folders the store marked dirty are planned, as the
        // background agent does; `settle` checks a full plan agrees.
        // The database patterns too, as the agent does: they decide which
        // working files are parked.
        let fingerprint = format!("{} {}", self.rules.fingerprint(), self.exec.local().databases().fingerprint());
        let all = fingerprint != self.planned_rules || std::mem::take(&mut self.stops.full_next);
        let rules = &self.rules;
        let dbs = self.exec.local().databases();
        let pull = self.pull;
        let rewinding = self.stops.rewind.as_ref().filter(|r| !r.done).map(|r| (r.folder, r.at));
        let (scope, mut ops, mut pending, mut paths, mut pending_paths, planned_rewind) = store
            .read(|o| {
                let scope = o.dirty_scope()?;
                let folders = if all { all_folders(&Parked::with_rules(o, rules))? } else { scope.folders.clone() };
                let mut ops = plan_in(o, rules, pull, folders, &dbs)?;
                // What a folder holds, unfiltered, as `oneup_app::sync` reads it.
                let pending =
                    oneup_sync::pending_deletes(&Parked::with_rules(o, rules), o, &ops, oneup_sync::Flow::TwoWay)?;
                // A removal of chosen duplicates waits for a round whose
                // plan doesn't touch what it removes or keeps, as in the agent.
                let (removal, other): (Vec<Op>, Vec<Op>) = extra.into_iter().partition(oneup_gate::is_removal);
                ops.extend(other);
                if !oneup_exec::dupes::touched(&ops, &removal) {
                    ops.extend(removal);
                }
                let (paths, pending_paths) = (shown_paths(o, &ops)?, shown_paths(o, &pending)?);
                let rewind = match rewinding {
                    Some((folder, at)) => Some(
                        oneup_sync::rewind::plan(&Parked::with_rules(o, rules), o, folder, at, 100_000)
                            .map_err(|e| oneup_store::Error::Corrupt(format!("{e:?}")))?,
                    ),
                    None => None,
                };
                Ok((scope, ops, pending, paths, pending_paths, rewind))
            })
            .unwrap();
        // A rewind's steps join the round, as the agent adds them; what the
        // planner would do to the same items waits for the rewind.
        let mut approved = self.stops.approved.clone();
        let mut rewound: Vec<ItemKey> = Vec::new();
        if let (Some(rw), Some(r)) = (planned_rewind, self.stops.rewind.as_mut()) {
            // An approval is taken for the list as it's planned now, only if
            // it's the one shown (its code): a change since gives another.
            if r.job.is_none() {
                let (all, all_paths): (Vec<Op>, Vec<String>) =
                    rw.ops.iter().cloned().zip(rw.paths.iter().cloned()).chain(rw.later.iter().cloned()).unzip();
                let code = oneup_gate::rewind_fingerprint(&all, &all_paths).0;
                match r.answer.take() {
                    Some(a) if a == code => r.job = Some(Job::approve(&all)),
                    Some(_) => r.refused += 1,
                    None => {}
                }
            }
            let approved_job = r.job.is_some();
            let (steps, step_paths, later): (Vec<Op>, Vec<String>, Vec<(Op, String)>) = match &r.job {
                None => {
                    r.preview = rw.ops.iter().cloned().chain(rw.later.iter().map(|(o, _)| o.clone())).collect();
                    (rw.ops, rw.paths, rw.later)
                }
                Some(job) => {
                    let (take, leave) = job.take(rw.ops, rw.paths);
                    r.left_alone += leave.len();
                    let later: Vec<(Op, String)> = rw.later.into_iter().filter(|(op, _)| job.accepts(op)).collect();
                    let waiting = rw.skipped.iter().any(|(_, _, why)| why.passes());
                    if take.is_empty() && later.is_empty() && !waiting {
                        r.done = true;
                    }
                    let (steps, step_paths): (Vec<Op>, Vec<String>) = take.into_iter().unzip();
                    approved.extend(oneup_gate::rewind_keys(&steps, &step_paths));
                    (steps, step_paths, later)
                }
            };
            let subjects = |op: &Op| {
                let (s, l, rm) = op.subjects();
                steps.iter().any(|st| {
                    let (ss, sl, srm) = st.subjects();
                    (s.is_some() && s == ss) || (l.is_some() && l == sl) || (rm.is_some() && rm == srm)
                })
            };
            // Approved, its items are its own; before, they sync as ever.
            let kept: Vec<(Op, String)> = ops
                .into_iter()
                .zip(paths)
                .filter(|(op, _)| !approved_job || !op.is_action() || !subjects(op))
                .collect();
            (ops, paths) = kept.into_iter().unzip();
            ops.extend(steps);
            paths.extend(step_paths);
            for (op, p) in later {
                pending.push(op);
                pending_paths.push(p);
            }
        }
        // A conflict answered or a restore asked for: the answer approves
        // its steps.
        let answered: Vec<(Op, String)> = ops
            .iter()
            .zip(&paths)
            .filter(|(o, _)| o.rewind_step().is_some())
            .map(|(o, p)| (o.clone(), p.clone()))
            .collect();
        if self.stops.rewind.as_ref().is_none_or(|r| r.done) && !std::mem::take(&mut self.unanswered) {
            let (o, p): (Vec<Op>, Vec<String>) = answered.into_iter().unzip();
            approved.extend(oneup_gate::rewind_keys(&o, &p));
        }
        // This PC following what it did: approved with it, until done.
        if let Some(job) = self.stops.rewind.as_ref().and_then(|r| r.job.as_ref()) {
            rewound = job.follows.iter().map(|k| ItemKey(*k)).collect();
        }
        approved.sort_unstable();
        approved.dedup();
        // The content guard, as the agent judges uploads (`judge_content`):
        // a file whose first bytes don't look like its type.
        let mut suspicious: Vec<LocalId> = ops
            .iter()
            .filter_map(|op| match op {
                Op::Upload { local, name, .. } => {
                    let rel = store.read(|o| o.local_path_of(local.id.0)).ok().flatten()?;
                    let mut head = std::fs::read(self.dir.0.join(rel.trim_start_matches(['\\', '/']))).ok()?;
                    head.truncate(oneup_gate::content::HEAD);
                    oneup_gate::content::looks_wrong(name, &head).then_some(local.id)
                }
                _ => None,
            })
            .collect();
        suspicious.sort_unstable();
        let cx = Context {
            room: self.room,
            decisions: &self.decisions,
            recent: self.stops.window.unwrap_or_default(),
            approved: &approved,
            pending: &pending,
            paths: &paths,
            pending_paths: &pending_paths,
            rewound: &rewound,
            suspicious: &suspicious,
            ..Context::default()
        };
        let r = review_in(&self.policy, &cx, ops);
        // Removing chosen duplicates, or a rewind, waits for its own
        // approval and holds nothing else back, as in the agent.
        let blocking = r.breakers.iter().any(|b| !matches!(b.kind, BreakerKind::Removal | BreakerKind::Rewind));
        assert!(self.stops.allowed || !blocking, "{:?}", r.breakers);
        if !r.breakers.is_empty() {
            // The marks stay, and the list's folders are marked so every
            // round plans all of it.
            let sids: Vec<oneup_sync::Sid> = r.breakers.iter().flat_map(|b| b.sids.clone()).collect();
            store.write(|o| o.dirty_mark_synced(&sids)).unwrap();
        }
        if blocking {
            // Nothing runs.
            self.stops.raised = r.breakers;
            return Report::default();
        }
        self.stops.raised.clone_from(&r.breakers);
        self.overflow = r.overflow.clone();
        let awaiting = r.overflow.as_ref().is_some_and(|o| o.outcome == OverflowOutcome::Held);
        let planned = r.approved.len();
        let order: Vec<Op> = r.approved.iter().map(|a| a.op().clone()).collect();
        let counted = std::sync::atomic::AtomicUsize::new(0);
        let count = |n: usize| {
            counted.fetch_add(n, std::sync::atomic::Ordering::Relaxed);
        };
        // What goes into new folders, as `oneup_app::sync` plans it: like a
        // round, by the same rule (`oneup_gate::may_follow`), at most `room`.
        let followed = Mutex::new(Vec::new());
        let (policy, on) = (&self.policy, self.follow);
        let follow = |o: &oneup_store::Ops<'_>, made: &[oneup_sync::Sid], room: usize| {
            let Some(ops) = on.then(|| plan_in(o, rules, pull, made.to_vec(), &dbs).unwrap()) else {
                return Vec::new();
            };
            if !oneup_gate::may_follow(&ops) {
                return Vec::new();
            }
            let r = review_in(policy, &Context::default(), ops);
            assert!(r.breakers.is_empty(), "nothing in a new folder trips a breaker: {:?}", r.breakers);
            let mut approved = r.approved;
            approved.truncate(room);
            followed.lock().unwrap().extend(approved.iter().map(|a| a.op().clone()));
            approved
        };
        let report = self.exec.run_following(store, r.approved, &count, may_run, &follow, self.follow_max).await;
        if let Some(w) = &mut self.stops.window {
            for op in &report.done {
                w.add_op(op, &[]);
            }
        }
        // A rewind's steps that ran: what they left their items in.
        if let Some(r) = self.stops.rewind.as_mut()
            && let Some(job) = r.job.as_mut()
        {
            let ran: Vec<&Op> = report.done.iter().filter(|o| matches!(o, Op::Rewind { .. })).collect();
            for op in &ran {
                let mark = store.read(|o| oneup_exec::rewind_mark(o, op)).unwrap();
                job.ran(op, mark);
                let follows = store.read(|o| oneup_exec::rewind_follows(o, op)).unwrap();
                job.follows.extend(follows.iter().map(|f| oneup_gate::follow_key(f).0));
            }
            // Each follow-up goes once it ran.
            for op in &report.done {
                if let Some(f) = oneup_gate::Follow::of(op) {
                    job.follows.remove(&oneup_gate::follow_key(&f).0);
                }
            }
            let tried = order.iter().any(|o| matches!(o, Op::Rewind { .. }));
            r.stalls = if tried && ran.is_empty() { r.stalls + 1 } else { 0 };
            if r.stalls >= STALLS {
                r.done = true;
            }
        }
        // A restore asked for that came back somewhere other than asked:
        // its move follows, as the agent queues it (`oneup_app::restores`);
        // one that didn't run waits, as in the queue.
        for op in &asked {
            if report.done.contains(op) {
                if let Some(next) = store.read(|o| oneup_exec::recover::follow_up(o, op)).unwrap() {
                    self.extra.push(next);
                }
            } else if !report.skipped.iter().any(|(o, _)| o == op) && !report.failed.iter().any(|(o, _)| o == op) {
                self.extra.push(op.clone());
            }
        }
        let followed = followed.into_inner().unwrap();
        let (planned, order) = (planned + followed.len(), [order, followed].concat());
        // Progress counts every operation once, batched or not.
        if report.halted.is_none() {
            let counted = counted.into_inner();
            assert_eq!(counted, planned, "progress count");
            let ended = report.done.len() + report.skipped.len() + report.failed.len() + report.not_run.len();
            assert_eq!(counted, ended);
        }
        // Each list follows the planner's order, whatever order the
        // operations finished in (`oneup sync` and the activity log rely on it).
        let in_order = |ops: Vec<&Op>| {
            let mut plan = order.iter();
            ops.into_iter().all(|op| plan.any(|p| p == op))
        };
        assert!(in_order(report.done.iter().collect()), "done out of order: {report:?}");
        assert!(in_order(report.skipped.iter().map(|(op, _)| op).collect()), "skipped out of order: {report:?}");
        assert!(in_order(report.failed.iter().map(|(op, _)| op).collect()), "failed out of order: {report:?}");
        assert!(in_order(report.not_run.iter().collect()), "not run out of order: {report:?}");
        // A halted round (a crash) leaves its marks, as a killed process
        // would. A skipped operation (say, a file held open) left nothing behind to
        // mark its folder again, so its marks stay for the retry; so do a
        // batch waiting for the user's decision and what wasn't run (as
        // `oneup sync` does).
        if report.halted.is_none() && report.skipped.is_empty() && report.not_run.is_empty() && !awaiting {
            store.write(|o| o.dirty_clear(scope.upto)).unwrap();
            self.planned_rules = fingerprint;
        }
        report
    }

    /// Whether planning every folder would still find something to do.
    fn full_plan_has_actions(&self) -> bool {
        let ops = self
            .obs
            .store()
            .read(|o| {
                // Hiding everything excluded: what pull mode leaves undone on
                // purpose isn't missed work.
                let dbs = self.exec.local().databases();
                let p = Parked::with_rules(o, &self.rules).with_databases(&dbs);
                plan(&p, all_folders(&p)?)
            })
            .unwrap();
        ops.iter().any(|op| op.is_action())
    }

    /// Rounds until both sides agree and a round changes nothing. Delete
    /// records can reach the journal shortly after the call returns, so an
    /// idle round with the sides still differing waits and tries again.
    async fn settle(&mut self) -> usize {
        self.settle_with(&|_, _| {}).await
    }

    /// [`Sync::settle`], handing each round's report to `seen` (as the
    /// agent hands what a round did to its event feed).
    async fn settle_with(&mut self, seen: &dyn Fn(&Sync, &Report)) -> usize {
        for n in 1..=40 {
            let report = self.round().await;
            seen(self, &report);
            if n > 30 {
                eprintln!("round {n}: {report:?}");
            }
            if report.is_idle() && report.skipped.is_empty() {
                if self.synced_part(local_tree(&self.dir.0)) == self.synced_part(self.cloud.tree()) {
                    let problems = self.obs.store().read(|o| o.check_invariants()).unwrap();
                    assert!(problems.is_empty(), "{problems:?}");
                    assert!(!self.full_plan_has_actions(), "dirty-folder planning missed work a full pass finds");
                    // As the agent does once a first sync has settled.
                    self.obs.store_mut().write(|o| o.history_start(now_ms())).unwrap();
                    return n;
                }
                std::thread::sleep(Duration::from_millis(100));
            }
        }
        panic!("no convergence:\nlocal {:#?}\ncloud {:#?}", names(&local_tree(&self.dir.0)), names(&self.cloud.tree()));
    }

    /// What the rules sync of a tree (parked names aside).
    fn synced_part(&self, t: Tree) -> Tree {
        syncable(t).into_iter().filter(|(path, c)| !self.rules.excludes(path, c.is_none())).collect()
    }

    fn versions(&self) -> Vec<Vec<u8>> {
        let mut out = Vec::new();
        for slot in std::fs::read_dir(self.p(".oneup/versions")).unwrap().flatten() {
            for e in std::fs::read_dir(slot.path()).unwrap().flatten() {
                if e.file_name() != "origin.txt" && e.file_type().unwrap().is_file() {
                    out.push(std::fs::read(e.path()).unwrap());
                }
            }
        }
        out
    }
}

/// The part of a tree a sync must make equal: without parked names, which
/// stay on their own side by design.
/// What a round plans for `folders`, as `oneup_app::sync` plans it: in pull
/// mode, excluded items stay visible and what would send them is dropped.
fn plan_in(
    o: &oneup_store::Ops<'_>,
    rules: &RuleSet,
    pull: bool,
    folders: Vec<oneup_sync::Sid>,
    dbs: &oneup_rules::database::Databases,
) -> oneup_store::Result<Vec<Op>> {
    let hide = if pull { rules.upload_only() } else { rules.clone() };
    let ops = plan(&Parked::with_rules(o, &hide).with_databases(dbs), folders)?;
    if !pull {
        return Ok(ops);
    }
    let judge = Parked::with_rules(o, rules);
    let mut kept = Vec::new();
    for op in ops {
        if !judge.sends_excluded(&op)? {
            kept.push(op);
        }
    }
    Ok(kept)
}

fn syncable(t: Tree) -> Tree {
    // Working files beside their namesake stay on their own side by design
    // (`oneup_plan::park`).
    let beside_database = |path: &str| {
        let Some((_, main)) = oneup_rules::database::Companion::of_name(path) else { return false };
        t.get(main).is_some_and(Option::is_some)
    };
    t.iter()
        .filter(|(path, _)| {
            let name = path.rsplit('/').next().unwrap_or_default();
            !(names::is_invalid(name) || names::not_synced(name).is_some() || beside_database(path))
        })
        .map(|(k, v)| (k.clone(), v.clone()))
        .collect()
}

fn names(t: &Tree) -> BTreeSet<&str> {
    t.keys().map(String::as_str).collect()
}

fn write(path: &Path, content: &[u8]) {
    std::fs::create_dir_all(path.parent().unwrap()).unwrap();
    std::fs::write(path, content).unwrap();
}

// ---------------------------------------------------------------- tests

#[tokio::test]
async fn first_sync_merges_both_sides_without_losing_anything() {
    let cloud = Fake::new();
    cloud.add("c.txt", Some(b"cloud c"));
    cloud.add("docs", None);
    cloud.add("docs/d.txt", Some(b"dee"));
    cloud.add("same.txt", Some(b"same content"));
    cloud.add("diff.txt", Some(b"cloud version"));
    cloud.add("photos", None);
    cloud.add("photos/p.jpg", Some(b"pixels"));

    let dir = Tmp(std::env::temp_dir().join(format!("oneup-exec-first-{}", std::process::id())));
    let _ = std::fs::remove_dir_all(&dir.0);
    write(&dir.0.join("a.txt"), b"local a");
    write(&dir.0.join("docs/b.txt"), b"bee");
    write(&dir.0.join("same.txt"), b"same content");
    write(&dir.0.join("diff.txt"), b"local version");
    let mut s = Sync::with_dir(dir, cloud.clone());

    s.settle().await;
    let t = local_tree(&s.dir.0);
    for p in ["/a.txt", "/c.txt", "/same.txt", "/docs/b.txt", "/docs/d.txt", "/photos/p.jpg"] {
        assert!(t.contains_key(p), "{p} missing: {:?}", names(&t));
    }
    // Different content under one name: both kept, nothing overwritten.
    let diffs: Vec<&Vec<u8>> =
        t.iter().filter(|(p, _)| p.starts_with("/diff")).filter_map(|(_, c)| c.as_ref()).collect();
    assert_eq!(diffs.len(), 2, "{:?}", names(&t));
    assert!(diffs.contains(&&b"local version".to_vec()) && diffs.contains(&&b"cloud version".to_vec()));
    // Nothing was replaced or recycled locally.
    assert!(s.versions().is_empty());
}

#[tokio::test]
async fn parked_names_stay_on_their_side_and_renaming_to_one_deletes_nothing() {
    let cloud = Fake::new();
    cloud.add("desktop.ini", Some(b"[.ShellClassInfo]"));
    let mut s = Sync::new("parked", cloud.clone());
    write(&s.p("keep.txt"), b"keep");
    write(&s.p("notes.tmp"), b"scratch");
    write(&s.p("~$report.docx"), b"office lock");
    // A trailing dot: valid on NTFS through a `\\?\` path, refused by
    // OneDrive (and unusable by most Windows apps elsewhere).
    let odd = PathBuf::from(format!(r"\\?\{}\odd.", s.dir.0.display()));
    std::fs::write(&odd, b"odd").unwrap();
    s.settle().await;

    let t = cloud.tree();
    assert!(t.contains_key("/keep.txt") && t.contains_key("/desktop.ini"), "{:?}", names(&t));
    assert_eq!(t.len(), 2, "parked files were uploaded: {:?}", names(&t));
    assert!(!s.p("desktop.ini").exists(), "a local-by-nature cloud file was downloaded");

    // Renaming a synced file to a parked name holds it; the cloud copy stays.
    std::fs::rename(s.p("keep.txt"), s.p("keep.tmp")).unwrap();
    s.round().await;
    s.round().await;
    assert_eq!(cloud.tree().get("/keep.txt"), Some(&Some(b"keep".to_vec())));
    assert!(s.p("keep.tmp").exists());
}

#[tokio::test]
async fn changes_on_both_sides_propagate_and_removals_are_recoverable() {
    let cloud = Fake::new();
    let mut s = Sync::new("changes", cloud.clone());
    write(&s.p("a.txt"), b"v1");
    write(&s.p("docs/b.txt"), b"bee");
    cloud.add("c.txt", Some(b"cloud c"));
    cloud.add("photos", None);
    cloud.add("photos/p.jpg", Some(b"pixels"));
    cloud.add("both.txt", Some(b"base"));
    s.settle().await;

    // Local edit, local delete, cloud delete, cloud rename, and an edit of
    // the same file on both sides.
    write(&s.p("a.txt"), b"v2, edited locally");
    std::fs::remove_file(s.p("docs/b.txt")).unwrap();
    cloud.delete("c.txt");
    cloud.rename("photos", "pictures");
    write(&s.p("both.txt"), b"mine");
    cloud.edit("both.txt", b"theirs");
    s.settle().await;

    let t = local_tree(&s.dir.0);
    assert_eq!(t["/a.txt"].as_deref(), Some(&b"v2, edited locally"[..]));
    assert!(!t.contains_key("/docs/b.txt") && !t.contains_key("/c.txt"));
    assert!(t.contains_key("/pictures/p.jpg") && !t.contains_key("/photos"));
    let both: Vec<&[u8]> = t.iter().filter(|(p, _)| p.starts_with("/both")).filter_map(|(_, c)| c.as_deref()).collect();
    assert_eq!(both.len(), 2, "{:?}", names(&t));
    assert!(both.contains(&&b"mine"[..]) && both.contains(&&b"theirs"[..]));

    // Removed content is recoverable: the cloud's recycle bin holds the
    // local deletion, the local version store the cloud deletion.
    assert!(cloud.holds(b"bee"));
    assert!(s.versions().contains(&b"cloud c".to_vec()));
    // The cloud kept the old version of the edited file.
    assert!(cloud.holds(b"v1"));
}

/// Graph checks a large upload's If-Match only when its session starts,
/// and the upload re-checks the eTag just before it commits. An edit that
/// lands in OneDrive between the two is gone over: it is kept as a
/// conflict copy, not only in the file's version history.
#[tokio::test]
async fn an_edit_in_onedrive_that_an_upload_went_over_is_kept_as_a_copy() {
    let cloud = Fake::new();
    let mut s = Sync::new("raced", cloud.clone());
    write(&s.p("notes.txt"), b"v1");
    s.settle().await;

    write(&s.p("notes.txt"), b"mine");
    cloud.0.lock().unwrap().race = Some(b"theirs".to_vec());
    let mut kept = Vec::new();
    for _ in 0..5 {
        kept.extend(s.round().await.kept_theirs.into_iter().map(|(_, name)| name));
        if !kept.is_empty() {
            break;
        }
    }
    assert_eq!(kept, ["notes (conflict 1).txt"]);
    s.settle().await;
    for t in [local_tree(&s.dir.0), cloud.tree()] {
        assert_eq!(t["/notes.txt"].as_deref(), Some(&b"mine"[..]));
        assert_eq!(t["/notes (conflict 1).txt"].as_deref(), Some(&b"theirs"[..]));
        assert_eq!(t.len(), 2, "{:?}", names(&t));
    }
    // Without a race, a replacement keeps nothing extra.
    write(&s.p("notes.txt"), b"mine, again");
    s.settle().await;
    assert_eq!(cloud.tree().len(), 2);
}

/// The kept edit takes the first conflict name free, names compared as
/// Windows compares them.
#[tokio::test]
async fn a_kept_edit_takes_the_first_conflict_name_not_taken() {
    let cloud = Fake::new();
    let mut s = Sync::new("raced-names", cloud.clone());
    write(&s.p("notes.txt"), b"v1");
    write(&s.p("notes (conflict 1).txt"), b"one");
    write(&s.p("NOTES (CONFLICT 2).TXT"), b"two");
    s.settle().await;

    write(&s.p("notes.txt"), b"mine");
    cloud.0.lock().unwrap().race = Some(b"theirs".to_vec());
    let mut kept = Vec::new();
    for _ in 0..5 {
        kept.extend(s.round().await.kept_theirs.into_iter().map(|(_, name)| name));
        if !kept.is_empty() {
            break;
        }
    }
    assert_eq!(kept, ["notes (conflict 3).txt"]);
    s.settle().await;
    assert_eq!(cloud.tree()["/notes (conflict 3).txt"].as_deref(), Some(&b"theirs"[..]));
}

/// An edit an upload went over that can't be kept as a new file yet (its
/// version won't download: the data budget used up, the network gone):
/// the upload is still done, counted as such and not as a failure, and the
/// edit is reported as not kept yet. It stays in the file's version
/// history, and keeping it is never dropped: once the version downloads
/// again, a later round keeps it as a new file.
#[tokio::test]
async fn an_upload_over_an_edit_that_cant_be_kept_is_still_done() {
    let cloud = Fake::new();
    let mut s = Sync::new("raced-unkept", cloud.clone());
    write(&s.p("notes.txt"), b"v1");
    s.settle().await;

    write(&s.p("notes.txt"), b"mine");
    {
        let mut st = cloud.0.lock().unwrap();
        st.race = Some(b"theirs".to_vec());
        st.versions_fail = true;
    }
    let mut not_kept = Vec::new();
    for _ in 0..5 {
        // `round` also checks nothing failed.
        let r = s.round().await;
        for (op, why) in &r.theirs_not_kept {
            assert!(matches!(op, Op::Upload { replace: Some(_), .. }), "{op:?}");
            assert!(r.done.contains(op), "the upload counts as done: {r:?}");
            assert!(why.contains("the file's version history in OneDrive has that edit"), "{why}");
        }
        assert!(r.kept_theirs.is_empty(), "{r:?}");
        not_kept.extend(r.theirs_not_kept);
        if !not_kept.is_empty() {
            break;
        }
    }
    assert_eq!(not_kept.len(), 1);
    assert!(not_kept[0].1.contains("tried again next round"), "{}", not_kept[0].1);
    s.settle().await;
    assert_eq!(cloud.tree().len(), 1, "not kept while its version won't download");
    assert!(cloud.0.lock().unwrap().old.values().any(|v| v == b"theirs"), "in the version history");
    // The version downloads again (more data allowed, the network back):
    // the next round keeps it.
    cloud.0.lock().unwrap().versions_fail = false;
    let r = s.round().await;
    assert_eq!(r.kept_later, ["notes (conflict 1).txt"]);
    s.settle().await;
    for t in [local_tree(&s.dir.0), cloud.tree()] {
        assert_eq!(t["/notes.txt"].as_deref(), Some(&b"mine"[..]));
        assert_eq!(t["/notes (conflict 1).txt"].as_deref(), Some(&b"theirs"[..]));
        assert_eq!(t.len(), 2, "{:?}", names(&t));
    }
    assert!(s.round().await.kept_later.is_empty(), "kept once");
}

/// The process killed at any point of an upload that goes over an edit
/// in OneDrive (between the record written ahead of it and the upload,
/// between the upload's commit and the version it is being recorded,
/// between that and its other records, between those and keeping the
/// edit, or while keeping it), then the file saved again in OneDrive on
/// top: after a restart, the edit is kept as a new file exactly once, never
/// the upload itself in its place. The record and the upload's version are
/// written durably (`meta_set_ahead` refuses otherwise), so a power cut
/// doesn't roll them back either. Killed after the commit but before the
/// upload's version is recorded, nothing tells the upload apart from the
/// later save: nothing is kept, and the record stays (to be given up and
/// listed).
#[tokio::test]
async fn a_crash_around_an_upload_over_an_edit_still_keeps_the_edit() {
    let count = |t: &Tree, c: &[u8]| t.values().filter(|v| v.as_deref() == Some(c)).count();
    // Both cases after the commit came up: its version recorded, and not.
    let (mut recorded, mut unrecorded) = (false, false);
    for n in 0.. {
        let cloud = Fake::new();
        let mut s = Sync::on_disk(&format!("raced-crash{n}"), cloud.clone());
        write(&s.p("notes.txt"), b"v1");
        s.settle().await;
        write(&s.p("notes.txt"), b"mine");
        cloud.0.lock().unwrap().race = Some(b"theirs".to_vec());

        s.exec.crash_after(Some(n));
        let mut crashed = false;
        for _ in 0..10 {
            let r = s.attempt().await;
            if r.halted.as_deref() == Some(CRASHED) {
                crashed = true;
                break;
            }
            assert!(r.halted.is_none() && r.failed.is_empty(), "crash point {n}: {r:?}");
            if r.is_idle() && r.skipped.is_empty() {
                break;
            }
        }
        if !crashed {
            assert!(n > 3, "only {n} crash points");
            assert!(recorded && unrecorded, "{recorded} {unrecorded}");
            return;
        }
        // Where it stopped: the upload committed or not, and its version
        // recorded or not.
        let committed = cloud.tree()["/notes.txt"].as_deref() == Some(&b"mine"[..]);
        let records = s.obs.store().read(|o| o.meta_under("exec.rescue")).unwrap();
        // (No record left: the edit was kept already.)
        let known = records.is_empty() || records.iter().any(|(_, v)| v.contains("\"ours\":{"));
        recorded |= committed && known && !records.is_empty();
        unrecorded |= committed && !known;
        if committed {
            // Saved again in OneDrive, on top of the upload.
            cloud.edit("notes.txt", b"later");
        }
        let mut s = s.restart();
        let kept = std::cell::RefCell::new(Vec::new());
        s.settle_with(&|_, r| kept.borrow_mut().extend(r.kept_later.iter().cloned())).await;
        let t = cloud.tree();
        let left = s.obs.store().read(|o| o.meta_under("exec.rescue")).unwrap();
        if !committed || known {
            assert_eq!(count(&t, b"theirs"), 1, "crash point {n}: {:?}", names(&t));
            assert!(left.is_empty(), "crash point {n}: {left:?}");
        } else {
            assert_eq!(count(&t, b"theirs"), 0, "crash point {n}: {:?}", names(&t));
            assert!(kept.borrow().is_empty(), "crash point {n}: nothing kept from a guess: {:?}", kept.borrow());
            assert_eq!(left.len(), 1, "crash point {n}: the record stays");
        }
        assert_eq!(local_tree(&s.dir.0), t, "crash point {n}");
    }
}

/// On a network that moves only the files the user opens, keeping the
/// edit waits; a rename in OneDrive meanwhile (which changes the eTag and
/// the current version's time) doesn't stop it: the upload's version was
/// recorded when it committed. Once downloads are allowed, it's kept.
#[tokio::test]
async fn a_rename_while_keeping_an_edit_waits_doesnt_lose_it() {
    let cloud = Fake::new();
    let mut s = Sync::new("raced-renamed", cloud.clone());
    write(&s.p("notes.txt"), b"v1");
    s.settle().await;

    s.exec.set_may_fetch(Some(Arc::new(|_| false)));
    write(&s.p("notes.txt"), b"mine");
    cloud.0.lock().unwrap().race = Some(b"theirs".to_vec());
    for _ in 0..3 {
        s.round().await;
    }
    cloud.rename("notes.txt", "renamed.txt");
    for _ in 0..3 {
        assert!(s.round().await.kept_later.is_empty());
    }
    s.exec.set_may_fetch(None);
    let r = s.round().await;
    assert_eq!(r.kept_later.len(), 1, "{r:?}");
    s.settle().await;
    let t = cloud.tree();
    assert_eq!(t["/renamed.txt"].as_deref(), Some(&b"mine"[..]));
    assert_eq!(t.values().filter(|v| v.as_deref() == Some(&b"theirs"[..])).count(), 1, "{:?}", names(&t));
}

/// OneDrive's version history can lag behind a commit: while it doesn't
/// show what an upload went over, keeping the edit waits and is tried
/// again at the next rounds, not given up as "nothing to keep".
#[tokio::test]
async fn an_edit_the_version_history_doesnt_show_yet_is_looked_for_again() {
    let cloud = Fake::new();
    let mut s = Sync::new("raced-lag", cloud.clone());
    write(&s.p("notes.txt"), b"v1");
    s.settle().await;

    write(&s.p("notes.txt"), b"mine");
    {
        let mut st = cloud.0.lock().unwrap();
        st.race = Some(b"theirs".to_vec());
        st.history_lag = 3;
    }
    let mut kept = Vec::new();
    let mut waited = Vec::new();
    for _ in 0..8 {
        let r = s.round().await;
        waited.extend(r.theirs_not_kept.into_iter().map(|(_, why)| why));
        kept.extend(r.kept_theirs.into_iter().map(|(_, name)| name));
        kept.extend(r.kept_later);
        if !kept.is_empty() {
            break;
        }
    }
    assert_eq!(kept, ["notes (conflict 1).txt"]);
    assert!(waited.iter().any(|w| w.contains("doesn't show it yet")), "{waited:?}");
    s.settle().await;
    assert_eq!(cloud.tree()["/notes (conflict 1).txt"].as_deref(), Some(&b"theirs"[..]));
}

/// A history that never shows it: given up after `RESCUE_TRIES` rounds,
/// listed for the user, and no longer asked for.
#[tokio::test]
async fn an_edit_the_version_history_never_shows_is_given_up_and_listed() {
    let cloud = Fake::new();
    let mut s = Sync::new("raced-never", cloud.clone());
    write(&s.p("notes.txt"), b"v1");
    s.settle().await;

    write(&s.p("notes.txt"), b"mine");
    {
        let mut st = cloud.0.lock().unwrap();
        st.race = Some(b"theirs".to_vec());
        st.history_lag = u32::MAX;
    }
    let mut given_up = Vec::new();
    let mut rounds = 0;
    while given_up.is_empty() && rounds < 3 * oneup_exec::RESCUE_TRIES {
        given_up.extend(s.round().await.rescues_given_up);
        rounds += 1;
    }
    assert_eq!(given_up, ["notes.txt"]);
    assert!(rounds <= oneup_exec::RESCUE_TRIES + 1, "{rounds} rounds");
    let listed = s.obs.store().read(oneup_exec::rescues_given_up).unwrap();
    assert_eq!(listed.iter().map(|(n, _)| n.as_str()).collect::<Vec<_>>(), ["notes.txt"]);
    let asked = cloud.0.lock().unwrap().history_lag;
    for _ in 0..3 {
        assert!(s.round().await.rescues_given_up.is_empty(), "reported once");
    }
    assert_eq!(cloud.0.lock().unwrap().history_lag, asked, "no longer asked for");
}

/// Working offline, or on a network that moves only names or only the
/// files the user opens: the edit an upload went over isn't downloaded
/// (nor uploaded again) until the network lets it through; then it's kept.
#[tokio::test]
async fn keeping_an_edit_waits_for_a_network_that_lets_it_download() {
    let cloud = Fake::new();
    let mut s = Sync::new("raced-offline", cloud.clone());
    write(&s.p("notes.txt"), b"v1");
    s.settle().await;

    let asked = Arc::new(std::sync::atomic::AtomicU32::new(0));
    let a = Arc::clone(&asked);
    s.exec.set_may_fetch(Some(Arc::new(move |_| {
        a.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
        false
    })));
    write(&s.p("notes.txt"), b"mine");
    cloud.0.lock().unwrap().race = Some(b"theirs".to_vec());
    let mut waits = Vec::new();
    for _ in 0..5 {
        let r = s.round().await;
        assert!(r.kept_later.is_empty() && r.kept_theirs.is_empty(), "{r:?}");
        waits.extend(r.theirs_not_kept.into_iter().map(|(_, why)| why));
    }
    assert!(waits.iter().any(|w| w.contains("waits for a network")), "{waits:?}");
    assert!(asked.load(std::sync::atomic::Ordering::Relaxed) >= 2, "asked at every round");
    let uploaded = {
        let st = cloud.0.lock().unwrap();
        assert_eq!(st.version_downloads, 0, "nothing downloaded");
        st.uploaded
    };
    assert_eq!(uploaded, b"v1".len() as u64 + b"mine".len() as u64, "nothing uploaded but the file");
    assert_eq!(cloud.tree().len(), 1);

    s.exec.set_may_fetch(None);
    let r = s.round().await;
    assert_eq!(r.kept_later, ["notes (conflict 1).txt"]);
    assert_eq!(cloud.0.lock().unwrap().version_downloads, 1);
}

/// A rescue download under way when the network stops letting it
/// through (working offline, another network) stops at its next write;
/// the record stays, and the edit is kept once it's allowed again.
#[tokio::test]
async fn a_rescue_download_stops_when_the_network_stops_letting_it_through() {
    use std::sync::atomic::{AtomicU32, Ordering::Relaxed};
    let cloud = Fake::new();
    let mut s = Sync::new("raced-stopped", cloud.clone());
    write(&s.p("notes.txt"), b"v1");
    s.settle().await;

    // Allowed when the rescue starts and when its download starts; not
    // from then on.
    let asked = Arc::new(AtomicU32::new(0));
    let a = Arc::clone(&asked);
    s.exec.set_may_fetch(Some(Arc::new(move |_| a.fetch_add(1, Relaxed) < 2)));
    write(&s.p("notes.txt"), b"mine");
    cloud.0.lock().unwrap().race = Some(b"theirs".to_vec());
    let mut waits = Vec::new();
    for _ in 0..5 {
        let r = s.round().await;
        assert!(r.kept_theirs.is_empty() && r.kept_later.is_empty(), "{r:?}");
        waits.extend(r.theirs_not_kept.into_iter().map(|(_, why)| why));
        if !waits.is_empty() {
            break;
        }
    }
    assert!(waits.iter().any(|w| w.contains("waits for a network")), "{waits:?}");
    assert_eq!(cloud.0.lock().unwrap().version_downloads, 0, "stopped before its data was written");
    assert_eq!(cloud.tree().len(), 1);
    let left = s.obs.store().read(|o| o.meta_under("exec.rescue")).unwrap();
    assert_eq!(left.len(), 1, "the record stays");

    s.exec.set_may_fetch(None);
    assert_eq!(s.round().await.kept_later, ["notes (conflict 1).txt"]);
}

/// Server-side copy: a new local file whose content OneDrive
/// already has (same size and hash, at least 1 MiB) is copied there
/// instead of uploaded; anything else, or a failed copy, is uploaded.
#[tokio::test]
async fn content_onedrive_already_has_is_copied_there_not_uploaded() {
    let cloud = Fake::new();
    let mut s = Sync::new("copy", cloud.clone());
    let big: Vec<u8> = (0..(1u32 << 20) + 5).map(|i| (i.wrapping_mul(7) % 251) as u8).collect();
    let mut twin = big.clone();
    twin[100] ^= 1; // same size, other content
    cloud.add("video.bin", Some(&big));
    s.settle().await;
    let up = |c: &Fake| c.0.lock().unwrap().uploaded;
    let copies = |c: &Fake| c.0.lock().unwrap().copies;
    let before = up(&cloud);

    write(&s.p("backup/video copy.bin"), &big);
    write(&s.p("twin.bin"), &twin);
    write(&s.p("small.bin"), &big[..1000]);
    s.settle().await;
    assert_eq!(copies(&cloud), 1);
    assert_eq!(up(&cloud) - before, twin.len() as u64 + 1000, "only the twin and the small file were sent");
    let t = cloud.tree();
    assert_eq!(t["/backup/video copy.bin"].as_deref(), Some(big.as_slice()));
    assert_eq!(t["/twin.bin"].as_deref(), Some(twin.as_slice()));

    // A failed copy falls back to an upload.
    cloud.0.lock().unwrap().copy_fails = true;
    write(&s.p("again.bin"), &big);
    s.settle().await;
    assert_eq!(copies(&cloud), 1);
    assert_eq!(up(&cloud) - before, twin.len() as u64 + 1000 + big.len() as u64);
    assert_eq!(cloud.tree()["/again.bin"].as_deref(), Some(big.as_slice()));
}

/// `$batch`: runs of independent cloud metadata changes go in one
/// request; changes that depend on each other (a folder deleted after its
/// contents, a name freed then taken) are never in the same one.
#[tokio::test]
async fn independent_cloud_changes_go_in_batches_and_dependent_ones_never_together() {
    let cloud = Fake::new();
    let mut s = Sync::new("batch", cloud.clone());
    for d in ["a", "b", "c", "d", "e"] {
        std::fs::create_dir_all(s.p(d)).unwrap();
    }
    for f in ["x.txt", "y.txt", "z.txt"] {
        write(&s.p(&format!("old/{f}")), f.as_bytes());
    }
    s.settle().await;
    let batches = |c: &Fake| std::mem::take(&mut c.0.lock().unwrap().batches);
    assert!(batches(&cloud).iter().any(|&n| n >= 5), "the five new folders went together");

    // Renames in one folder, then a swap of two names (a dependency).
    for f in ["x", "y", "z"] {
        std::fs::rename(s.p(&format!("old/{f}.txt")), s.p(&format!("old/{f}2.txt"))).unwrap();
    }
    s.settle().await;
    assert!(batches(&cloud).contains(&3));
    std::fs::rename(s.p("old/x2.txt"), s.p("old/tmp.txt")).unwrap();
    std::fs::rename(s.p("old/y2.txt"), s.p("old/x2.txt")).unwrap();
    std::fs::rename(s.p("old/tmp.txt"), s.p("old/y2.txt")).unwrap();
    s.settle().await;
    assert_eq!(cloud.tree()["/old/x2.txt"].as_deref(), Some(&b"y.txt"[..]));
    assert_eq!(cloud.tree()["/old/y2.txt"].as_deref(), Some(&b"x.txt"[..]));
    batches(&cloud);

    // A folder removed with its contents: the files go before the folder,
    // never in its batch, so nothing is refused and nothing is lost.
    std::fs::remove_dir_all(s.p("old")).unwrap();
    for d in ["a", "b", "c"] {
        std::fs::remove_dir(s.p(d)).unwrap();
    }
    s.settle().await;
    let t = cloud.tree();
    assert!(!t.contains_key("/old") && !t.contains_key("/a"));
    assert!(t.contains_key("/d") && t.contains_key("/e"));
    assert!(cloud.holds(b"x.txt") && cloud.holds(b"y.txt") && cloud.holds(b"z.txt"));
}

/// A folder's move and a change deeper inside it can share a batch: each
/// names its item by ID with its own eTag, and neither depends on where the
/// other's item is. Both land, and nothing else changes.
#[tokio::test]
async fn a_folder_move_and_a_change_deep_inside_it_both_land() {
    let cloud = Fake::new();
    let mut s = Sync::new("batch-deep", cloud.clone());
    write(&s.p("a/sub/g.txt"), b"grandchild");
    std::fs::create_dir_all(s.p("b")).unwrap();
    s.settle().await;
    cloud.0.lock().unwrap().batches.clear();
    std::fs::rename(s.p("a/sub/g.txt"), s.p("a/sub/h.txt")).unwrap();
    std::fs::rename(s.p("a"), s.p("b/a")).unwrap();
    s.settle().await;
    assert!(cloud.0.lock().unwrap().batches.contains(&2), "they went in one batch");
    let t = cloud.tree();
    assert_eq!(t["/b/a/sub/h.txt"].as_deref(), Some(&b"grandchild"[..]), "{:?}", names(&t));
    assert!(!t.contains_key("/a") && !t.contains_key("/b/a/sub/g.txt"));
    assert_eq!(local_tree(&s.dir.0), t);
}

#[tokio::test]
async fn a_cloud_edit_replaces_the_local_file_and_its_version_history_has_the_old_one() {
    let cloud = Fake::new();
    let mut s = Sync::new("replace", cloud.clone());
    write(&s.p("doc.txt"), b"first");
    s.settle().await;
    cloud.edit("doc.txt", b"second, from the cloud");
    s.settle().await;
    assert_eq!(std::fs::read(s.p("doc.txt")).unwrap(), b"second, from the cloud");
    // The old version was the synced one, so OneDrive has it; no local copy.
    assert!(cloud.holds(b"first"));
    assert!(s.versions().is_empty());

    // Asked to keep replaced files: the old one goes to the version store.
    s.policy.keep_replaced = true;
    cloud.edit("doc.txt", b"third");
    s.settle().await;
    assert_eq!(std::fs::read(s.p("doc.txt")).unwrap(), b"third");
    assert_eq!(s.versions(), vec![b"second, from the cloud".to_vec()]);
}

#[tokio::test]
async fn deletions_that_dont_fit_the_version_store_wait_for_one_decision() {
    let cloud = Fake::new();
    let mut s = Sync::new("overflow", cloud.clone());
    for name in ["a.bin", "b.bin", "c.bin", "d.bin"] {
        write(&s.p(name), &[name.as_bytes()[0]; 100]);
    }
    s.settle().await;

    // Two 100-byte files deleted in OneDrive, room for 150: held together,
    // untouched, while everything else keeps syncing.
    s.room = Some(150);
    cloud.delete("a.bin");
    cloud.delete("b.bin");
    cloud.add("new.txt", Some(b"new from the cloud"));
    for _ in 0..3 {
        s.round().await;
    }
    let o = s.overflow.clone().expect("an overflow batch");
    assert_eq!((o.count, o.bytes, o.outcome), (2, 200, OverflowOutcome::Held));
    assert!(s.p("a.bin").exists() && s.p("b.bin").exists());
    assert_eq!(std::fs::read(s.p("new.txt")).unwrap(), b"new from the cloud");

    // Discard: removed without a local copy (OneDrive's recycle bin has
    // them), once, for both.
    s.decisions.push(Decision { fingerprint: o.fingerprint, choice: Choice::Discard });
    s.settle().await;
    assert!(!s.p("a.bin").exists() && !s.p("b.bin").exists());
    assert!(s.versions().is_empty());
    assert!(cloud.holds(&[b'a'; 100]) && cloud.holds(&[b'b'; 100]));

    // The next batch needs its own decision; keep: into the store.
    cloud.delete("c.bin");
    cloud.delete("d.bin");
    for _ in 0..2 {
        s.round().await;
    }
    let o = s.overflow.clone().expect("a new batch");
    assert_eq!(o.outcome, OverflowOutcome::Held, "an old decision never covers a new batch");
    s.decisions.push(Decision { fingerprint: o.fingerprint, choice: Choice::Keep });
    s.settle().await;
    let mut kept = s.versions();
    kept.sort();
    assert_eq!(kept, vec![vec![b'c'; 100], vec![b'd'; 100]]);
}

#[tokio::test]
async fn a_cloud_deletion_of_an_open_file_waits_then_recycles() {
    let cloud = Fake::new();
    let mut s = Sync::new("recycle-busy", cloud.clone());
    write(&s.p("open.txt"), b"in use");
    s.settle().await;
    cloud.delete("open.txt");
    let held = std::fs::File::open(s.p("open.txt")).unwrap();
    let r = s.round().await;
    assert!(r.skipped.iter().any(|(_, k)| *k == oneup_exec::Skip::Busy), "{r:?}");
    // The write-ahead evidence was withdrawn: the file is still there and
    // still synced, so nothing else happens to it meanwhile.
    let r = s.round().await;
    assert!(r.done.is_empty(), "{r:?}");
    drop(held);
    s.settle().await;
    assert!(!s.p("open.txt").exists());
    assert_eq!(s.versions(), vec![b"in use".to_vec()]);
}

#[tokio::test]
async fn a_save_during_an_upload_goes_through_and_the_saved_version_is_sent() {
    let cloud = Fake::new();
    let mut s = Sync::new("saved-meanwhile", cloud.clone());
    write(&s.p("doc.txt"), b"first version");
    cloud.0.lock().unwrap().saved_meanwhile = Some(SaveDuringUpload::in_place(s.p("doc.txt"), b"saved meanwhile"));
    let r = s.round().await;
    assert!(r.skipped.iter().any(|(_, k)| *k == oneup_exec::Skip::Changed), "{r:?}");
    assert!(cloud.0.lock().unwrap().saved_meanwhile.is_none(), "the upload began");
    assert!(!cloud.tree().contains_key("/doc.txt"));
    assert_eq!(std::fs::read(s.p("doc.txt")).unwrap(), b"saved meanwhile");
    s.settle().await;
    assert_eq!(cloud.tree()["/doc.txt"].as_deref(), Some(&b"saved meanwhile"[..]));
    assert!(!cloud.holds(b"first version") && !cloud.holds(b"fisaved meanwhile"));
}

/// A program that saves by renaming a new file over the old one with
/// `MoveFileEx` (as `cmd`'s `move /y`, Node, Python, Go and .NET do) while
/// the file is being uploaded: the save goes through, the upload is
/// cancelled before it commits, and the saved version is sent next.
#[tokio::test]
async fn everyday_a_save_by_rename_during_an_upload_goes_through_and_the_saved_version_is_sent() {
    let cloud = Fake::new();
    let mut s = Sync::new("moved-meanwhile", cloud.clone());
    write(&s.p("doc.txt"), b"first version");
    cloud.0.lock().unwrap().saved_meanwhile =
        Some(SaveDuringUpload { by_move: true, ..SaveDuringUpload::in_place(s.p("doc.txt"), b"saved by rename") });
    let r = s.round().await;
    assert!(r.skipped.iter().any(|(_, k)| *k == oneup_exec::Skip::Changed), "{r:?}");
    assert!(cloud.0.lock().unwrap().saved_meanwhile.is_none(), "the upload began");
    assert!(!cloud.tree().contains_key("/doc.txt"), "nothing committed");
    assert_eq!(std::fs::read(s.p("doc.txt")).unwrap(), b"saved by rename");
    s.settle().await;
    assert_eq!(cloud.tree()["/doc.txt"].as_deref(), Some(&b"saved by rename"[..]));
    assert!(!cloud.holds(b"first version"));
    assert_eq!(local_tree(&s.dir.0), cloud.tree());
}

#[tokio::test]
async fn a_save_after_an_uploads_last_read_that_keeps_the_stamp_is_still_sent() {
    let cloud = Fake::new();
    let mut s = Sync::new("saved-after-read", cloud.clone());
    write(&s.p("doc.txt"), b"first version!!");
    cloud.0.lock().unwrap().saved_meanwhile = Some(SaveDuringUpload {
        after_last_byte: true,
        ..SaveDuringUpload::in_place(s.p("doc.txt"), b"SAVED VERSION!!")
    });
    s.round().await;
    assert!(cloud.0.lock().unwrap().saved_meanwhile.is_none(), "the upload began");
    assert_eq!(cloud.tree()["/doc.txt"].as_deref(), Some(&b"first version!!"[..]));
    s.settle().await;
    assert_eq!(cloud.tree()["/doc.txt"].as_deref(), Some(&b"SAVED VERSION!!"[..]));
}

#[tokio::test]
async fn a_file_held_open_is_retried_not_forced() {
    let cloud = Fake::new();
    let mut s = Sync::new("busy", cloud.clone());
    write(&s.p("locked.txt"), b"one");
    s.settle().await;
    cloud.edit("locked.txt", b"two");
    let held = std::fs::File::open(s.p("locked.txt")).unwrap();
    let mut busy = false;
    for _ in 0..3 {
        let r = s.round().await;
        busy |= r.skipped.iter().any(|(_, k)| *k == oneup_exec::Skip::Busy);
    }
    assert!(busy, "the download must wait while another program has the file open");
    assert_eq!(std::fs::read(s.p("locked.txt")).unwrap(), b"one");
    drop(held);
    s.settle().await;
    assert_eq!(std::fs::read(s.p("locked.txt")).unwrap(), b"two");
}

/// A pause of changes to OneDrive that comes while a round runs stops the
/// round's OneDrive changes before the next one, batched or not: the rest
/// aren't run (neither skipped nor failed), changes to this PC go on, and
/// resuming plans the rest again from the marks.
#[tokio::test]
async fn a_pause_mid_round_stops_that_sides_operations_and_resuming_finishes_them() {
    use oneup_gate::Side;
    let cloud = Fake::new();
    cloud.add("from-cloud.txt", Some(b"cloud"));
    let mut s = Sync::new("pause", cloud.clone());
    for d in ["a", "b", "c", "d"] {
        std::fs::create_dir_all(s.p(d)).unwrap();
    }
    for f in ["1.txt", "2.txt", "3.txt"] {
        write(&s.p(f), f.as_bytes());
    }
    // Paused once five of the seven OneDrive changes were let through, in
    // the middle of the four new folders, which would go in one batch
    // (asked again, those let through still may: the executor can ask twice).
    let allowed = Mutex::new(Vec::<Op>::new());
    let may_run = |op: &Op| {
        let mut a = allowed.lock().unwrap();
        if Side::changed_by(op) != Some(Side::Remote) || a.contains(op) {
            return true;
        }
        if a.len() < 5 {
            a.push(op.clone());
            return true;
        }
        false
    };
    let r = s.attempt_checked(&may_run).await;
    assert!(r.halted.is_none() && r.failed.is_empty() && r.skipped.is_empty(), "{r:?}");
    assert_eq!(r.not_run.len(), 2, "{r:?}");
    assert!(r.not_run.iter().all(|op| Side::changed_by(op) == Some(Side::Remote)));
    let made = |t: &Tree| {
        let all = ["/a", "/b", "/c", "/d", "/1.txt", "/2.txt", "/3.txt"];
        all.iter().filter(|p| t.contains_key(**p)).count()
    };
    assert_eq!(made(&cloud.tree()), 5, "only what went before the pause");
    assert_eq!(std::fs::read(s.p("from-cloud.txt")).unwrap(), b"cloud", "this PC's side went on");

    s.settle().await;
    assert_eq!(made(&cloud.tree()), 7);
}

/// A download under way when changes to this PC are paused stops as its
/// next data arrives: nothing is placed, its staging file goes, and it's
/// done again once resumed.
#[tokio::test]
async fn a_pause_stops_a_download_under_way_without_placing_anything() {
    use std::sync::atomic::{AtomicUsize, Ordering::Relaxed};
    let big: Vec<u8> = (0..64 * 1024u32).map(|i| (i % 251) as u8).collect();
    let cloud = Fake::new();
    cloud.add("big.bin", Some(&big));
    let mut s = Sync::new("pause-download", cloud.clone());
    // Asked before it starts, then at each piece: paused after the first.
    let asked = AtomicUsize::new(0);
    let may_run = |op: &Op| !matches!(op, Op::Download { .. }) || asked.fetch_add(1, Relaxed) < 2;
    let r = s.attempt_checked(&may_run).await;
    assert!(r.halted.is_none() && r.failed.is_empty() && r.skipped.is_empty(), "{r:?}");
    assert!(matches!(r.not_run.as_slice(), [Op::Download { .. }]), "{r:?}");
    assert_eq!(asked.load(Relaxed), 3, "stopped at the second of 16 pieces");
    assert!(!s.p("big.bin").exists());
    assert_eq!(std::fs::read_dir(s.p(".oneup/staging")).unwrap().count(), 0, "no partial download left");

    s.settle().await;
    assert_eq!(std::fs::read(s.p("big.bin")).unwrap(), big);
}

/// The most operations a round runs at once (the default profile's).
fn at_once() -> u64 {
    oneup_exec::Parallel::for_profile("balanced").ops as u64
}

/// A full OneDrive refuses an upload: the round starts no other upload
/// (only those already under way are refused too; each is skipped, not
/// failed, so it's tried again once there's room), and downloads go on.
#[tokio::test]
async fn a_full_onedrive_stops_the_rounds_uploads_while_downloads_go_on() {
    let cloud = Fake::new();
    let mut s = Sync::new("full", cloud.clone());
    write(&s.p("old.txt"), b"synced before");
    s.settle().await;
    // More uploads than run at once.
    let files: Vec<String> = (0..at_once() * 4).map(|k| format!("f{k:02}.txt")).collect();
    for name in &files {
        write(&s.p(name), name.as_bytes());
    }
    cloud.add("d.txt", Some(b"from the cloud"));
    cloud.0.lock().unwrap().full = true;
    let refused = |c: &Fake| c.0.lock().unwrap().refused;
    let r = s.round().await;
    let full = r.skipped.iter().filter(|(op, k)| *k == oneup_exec::Skip::CloudFull && matches!(op, Op::Upload { .. }));
    assert_eq!(full.count(), files.len(), "{r:?}");
    let sent = refused(&cloud);
    assert!((1..=at_once()).contains(&sent), "only uploads already under way were sent: {sent}");
    assert!(r.done.iter().any(|op| matches!(op, Op::Download { .. })), "{r:?}");
    assert_eq!(std::fs::read(s.p("d.txt")).unwrap(), b"from the cloud");

    // Room again: they go.
    cloud.0.lock().unwrap().full = false;
    s.settle().await;
    let t = cloud.tree();
    assert_eq!(t["/f01.txt"].as_deref(), Some(&b"f01.txt"[..]));
    assert_eq!(refused(&cloud), sent);
}

/// A full OneDrive refuses new folders too, also in a batch: after the
/// first refused request, batched or not, the round starts no other folder
/// creation or upload (only those already under way are refused too; each
/// is skipped as CloudFull, to be tried again once there's room), and
/// downloads go on.
#[tokio::test]
async fn a_full_onedrive_stops_the_rounds_new_folders_and_uploads_batched_or_not() {
    let cloud = Fake::new();
    let mut s = Sync::new("full-batch", cloud.clone());
    write(&s.p("old.txt"), b"synced before");
    s.settle().await;
    // More new folders than one batch takes, and new files.
    let folders: Vec<String> = (0..oneup_graph::client::BATCH_MAX + 5).map(|k| format!("d{k:02}")).collect();
    for d in &folders {
        std::fs::create_dir_all(s.p(d)).unwrap();
    }
    for name in ["a.txt", "b.txt", "c.txt"] {
        write(&s.p(name), name.as_bytes());
    }
    cloud.add("e.txt", Some(b"from the cloud"));
    cloud.0.lock().unwrap().full = true;
    let r = s.round().await;
    let sent = cloud.0.lock().unwrap().refused;
    assert!((1..=at_once()).contains(&sent), "refused requests already under way, then none: {sent} {r:?}");
    let held = |op: &Op| matches!(op, Op::Upload { .. } | Op::MkdirRemote { .. });
    let full = r.skipped.iter().filter(|(op, k)| *k == oneup_exec::Skip::CloudFull && held(op));
    assert_eq!(full.count(), folders.len() + 3, "{r:?}");
    assert!(!r.done.iter().any(held), "{r:?}");
    assert!(r.done.iter().any(|op| matches!(op, Op::Download { .. })), "{r:?}");
    assert_eq!(std::fs::read(s.p("e.txt")).unwrap(), b"from the cloud");

    // Room again: they go.
    cloud.0.lock().unwrap().full = false;
    s.settle().await;
    let t = cloud.tree();
    assert!(folders.iter().all(|d| t.get(&format!("/{d}")) == Some(&None)), "{:?}", names(&t));
    assert_eq!(t["/c.txt"].as_deref(), Some(&b"c.txt"[..]));
}

/// Every content the crash scenario ever writes, on either side.
const CRASH_CONTENTS: &[&[u8]] = &[
    b"alpha",
    b"edit v1",
    b"edit v2",
    b"local delete me",
    b"move me",
    b"new local",
    b"inner",
    b"cloud c",
    b"cloud edit v1",
    b"cloud edit v2",
    b"cloud delete me",
    b"cloud new",
    b"x",
    b"deeper",
    b"y",
    b"base",
    b"mine",
    b"theirs",
];

/// Synced state, then one change of every kind on both sides: what the
/// next round turns into uploads, downloads (new and replacing), folders
/// on both sides (two levels deep, whose contents go in the same round),
/// moves both ways, deletions both ways (one of them a local recycle) and a
/// conflict copy.
async fn crash_scenario(tag: &str) -> Sync {
    let cloud = Fake::new();
    let mut s = Sync::on_disk(tag, cloud.clone());
    write(&s.p("a.txt"), b"alpha");
    write(&s.p("edit.txt"), b"edit v1");
    write(&s.p("gone.txt"), b"local delete me");
    write(&s.p("docs/move.txt"), b"move me");
    cloud.add("c.txt", Some(b"cloud c"));
    cloud.add("cedit.txt", Some(b"cloud edit v1"));
    cloud.add("cgone.txt", Some(b"cloud delete me"));
    cloud.add("both.txt", Some(b"base"));
    s.settle().await;

    write(&s.p("edit.txt"), b"edit v2");
    write(&s.p("new.txt"), b"new local");
    write(&s.p("localdir/inner.txt"), b"inner");
    write(&s.p("localdir/sub/deeper.txt"), b"deeper");
    std::fs::remove_file(s.p("gone.txt")).unwrap();
    std::fs::rename(s.p("docs/move.txt"), s.p("moved.txt")).unwrap();
    write(&s.p("both.txt"), b"mine");
    cloud.add("cnew.txt", Some(b"cloud new"));
    cloud.add("cdir", None);
    cloud.add("cdir/x.txt", Some(b"x"));
    cloud.add("cdir/sub", None);
    cloud.add("cdir/sub/y.txt", Some(b"y"));
    cloud.edit("cedit.txt", b"cloud edit v2");
    cloud.delete("cgone.txt");
    cloud.rename("c.txt", "c2.txt");
    cloud.edit("both.txt", b"theirs");
    s
}

/// A crash (the process killed) at any point between an effect and its
/// record, or between a write-ahead record and its effect, loses nothing:
/// after a restart, sync converges to the same result, every content is
/// still somewhere (the folder, the version store, or OneDrive's recycle
/// bin and version history), deletions stay deleted, and the store's
/// invariants hold.
#[tokio::test]
async fn a_crash_at_any_point_of_a_round_loses_nothing() {
    for n in 0.. {
        let mut s = crash_scenario(&format!("crash{n}")).await;
        s.exec.crash_after(Some(n));
        let mut crashed = false;
        for _ in 0..40 {
            let r = s.attempt().await;
            if r.halted.as_deref() == Some(CRASHED) {
                crashed = true;
                break;
            }
            assert!(r.halted.is_none() && r.failed.is_empty(), "crash point {n}: {r:?}");
            if r.is_idle() && r.skipped.is_empty() {
                break;
            }
        }
        if !crashed {
            // Every crash point of the scenario has been tried.
            assert!(n > 10, "only {n} crash points");
            return;
        }
        eprintln!("crash point {n}");
        let mut s = s.restart();
        // The first run settles what the crash left, before any operation:
        // no write-ahead record outlives it, and the store is consistent.
        s.obs.catch_up().unwrap();
        let store = s.obs.store_mut();
        let r = s.exec.run(store, Vec::new()).await;
        assert!(r.halted.is_none(), "crash point {n}: {r:?}");
        assert_eq!(store.read(|o| o.meta_under("exec.pending")).unwrap(), vec![], "crash point {n}");
        let problems = store.read(|o| o.check_invariants()).unwrap();
        assert!(problems.is_empty(), "crash point {n}: {problems:?}");
        s.settle().await;

        let t = local_tree(&s.dir.0);
        let here: Vec<&[u8]> = t.values().filter_map(|c| c.as_deref()).collect();
        let versions = s.versions();
        for c in CRASH_CONTENTS {
            let kept = here.contains(c) || versions.iter().any(|v| v == c) || s.cloud.holds(c);
            assert!(kept, "crash point {n}: {:?} was lost", String::from_utf8_lossy(c));
        }
        for c in [&b"edit v2"[..], b"new local", b"inner", b"move me", b"cloud c", b"cloud edit v2", b"cloud new"] {
            assert!(here.contains(&c), "crash point {n}: {:?} isn't in the folder", String::from_utf8_lossy(c));
        }
        for c in [&b"x"[..], b"y", b"deeper", b"alpha", b"mine", b"theirs"] {
            assert!(here.contains(&c), "crash point {n}: {:?} isn't in the folder", String::from_utf8_lossy(c));
        }
        for c in [&b"local delete me"[..], b"cloud delete me"] {
            assert!(!here.contains(&c), "crash point {n}: {:?} came back", String::from_utf8_lossy(c));
        }
        assert!(t.contains_key("/moved.txt") && t.contains_key("/c2.txt"), "crash point {n}: {:?}", names(&t));
    }
}

/// What a crash inside a replacing download left: what's at the file's
/// name, whether the old version is in the version store, and the slots
/// left without an item.
#[derive(Debug, Clone, PartialEq, Eq)]
struct LeftAt {
    here: Option<Vec<u8>>,
    old_kept: bool,
    /// Slots left empty (killed after making one, before moving in).
    empty: usize,
    /// Slots left with only their `origin.txt` (killed after deleting a
    /// dropped old copy, before removing its slot).
    origin_only: usize,
}

impl Sync {
    fn left_at(&self) -> LeftAt {
        let (mut empty, mut origin_only) = (0, 0);
        for slot in std::fs::read_dir(self.p(".oneup/versions")).unwrap().flatten() {
            if !slot.file_type().unwrap().is_dir() {
                continue;
            }
            let names: Vec<_> = std::fs::read_dir(slot.path()).unwrap().flatten().map(|e| e.file_name()).collect();
            match names.as_slice() {
                [] => empty += 1,
                [only] if only == "origin.txt" => origin_only += 1,
                _ => {}
            }
        }
        LeftAt {
            here: std::fs::read(self.p("a.txt")).ok(),
            old_kept: self.versions().contains(&b"v1".to_vec()),
            empty,
            origin_only,
        }
    }
}

/// A crash inside a download that replaces a file, at each of its steps:
/// the old version moved into the version store (a slot made, the file
/// moved in), the new one renamed into place, the old copy dropped (when
/// OneDrive's history has it: deleted, then its slot removed). Between the
/// two renames the name is free and the old version is only in the version
/// store; between dropping the old copy and removing its slot, the slot
/// holds only its `origin.txt`. After a restart the new version arrives,
/// the old one is still in the version store or OneDrive's history,
/// nothing is left in staging, the store's listing shows no slot without
/// an item, and the store's invariants hold. With and without keeping
/// replaced files.
#[tokio::test]
async fn a_crash_inside_a_replacing_download_loses_nothing() {
    for keep in [false, true] {
        let mut left = Vec::new();
        for n in 0.. {
            let cloud = Fake::new();
            let mut s = Sync::on_disk(&format!("replace-crash-{keep}-{n}"), cloud.clone());
            s.policy.keep_replaced = keep;
            write(&s.p("a.txt"), b"v1");
            s.settle().await;
            cloud.edit("a.txt", b"v2");
            s.exec.crash_after(Some(n));
            let r = s.attempt().await;
            if r.halted.as_deref() != Some(CRASHED) {
                assert!(r.done.iter().any(|op| matches!(op, Op::Download { replace: Some(_), .. })), "{r:?}");
                break;
            }
            let at = s.left_at();
            if at.here.is_none() {
                assert!(at.old_kept, "keep {keep}, crash point {n}: v1 isn't kept");
            }
            left.push(at);

            let mut s = s.restart();
            let staging = std::fs::read_dir(s.p(".oneup/staging")).unwrap().count();
            assert_eq!(staging, 0, "keep {keep}, crash point {n}: a partial download outlived the restart");
            s.obs.catch_up().unwrap();
            let store = s.obs.store_mut();
            let r = s.exec.run(store, Vec::new()).await;
            assert!(r.halted.is_none(), "keep {keep}, crash point {n}: {r:?}");
            assert_eq!(store.read(|o| o.meta_under("exec.pending")).unwrap(), vec![]);
            s.settle().await;
            assert_eq!(std::fs::read(s.p("a.txt")).unwrap(), b"v2", "keep {keep}, crash point {n}");
            assert!(s.versions().contains(&b"v1".to_vec()) || cloud.holds(b"v1"), "keep {keep}, crash point {n}");
            if keep {
                assert!(s.versions().contains(&b"v1".to_vec()), "keep {keep}, crash point {n}: v1 isn't kept");
            }
            // A slot left empty or with only its origin is no kept item.
            let listed = s.exec.local().versions().unwrap();
            assert_eq!(listed.len(), s.versions().len(), "keep {keep}, crash point {n}: {listed:?}");
            let t = local_tree(&s.dir.0);
            assert_eq!(names(&t), BTreeSet::from(["/a.txt"]), "keep {keep}, crash point {n}");
        }
        // The crashes landed at each step inside the effect.
        let v2 = Some(b"v2".to_vec());
        let saw = |f: &dyn Fn(&LeftAt) -> bool| left.iter().any(f);
        assert!(saw(&|a| a.empty == 1 && a.here.as_deref() == Some(b"v1")), "keep {keep}, a slot made: {left:?}");
        assert!(saw(&|a| a.here.is_none() && a.old_kept), "keep {keep}, between the renames: {left:?}");
        assert!(saw(&|a| a.here == v2 && a.old_kept), "keep {keep}, both kept: {left:?}");
        let dropped = |a: &LeftAt| a.here == v2 && !a.old_kept && a.origin_only == 1;
        // Only a dropped old copy (`keep` off) has the step between deleting
        // it and removing its slot.
        assert_eq!(saw(&dropped), !keep, "keep {keep}, old copy deleted, slot left: {left:?}");
    }
}

/// OneDrive's listing lags a round behind the cloud (delta lag) while
/// changes go both ways: every listing is older than what our own uploads,
/// renames and deletions just got back, so rounds plan from a cloud that
/// has moved on. Downloads and cloud changes planned from it are refused
/// by their eTags, and whatever it moves here the next listing moves back.
/// Nothing is lost, a deletion stays deleted, and once the listing catches
/// up both sides agree.
#[tokio::test]
async fn a_listing_behind_the_cloud_loses_nothing() {
    let cloud = Fake::new();
    let mut s = Sync::new("lag", cloud.clone());
    write(&s.p("a.txt"), b"a v1");
    write(&s.p("b.txt"), b"bee");
    write(&s.p("docs/c.txt"), b"sea");
    write(&s.p("docs/keep.txt"), b"keep");
    cloud.add("d.txt", Some(b"d v1"));
    cloud.add("e.txt", Some(b"e"));
    s.settle().await;

    cloud.lag();
    write(&s.p("a.txt"), b"a v2");
    std::fs::rename(s.p("b.txt"), s.p("b2.txt")).unwrap();
    std::fs::remove_file(s.p("docs/c.txt")).unwrap();
    write(&s.p("new.txt"), b"new");
    write(&s.p("newdir/x.txt"), b"x");
    cloud.edit("d.txt", b"d v2");
    cloud.rename("e.txt", "e2.txt");
    s.round().await;
    // Edited and renamed here again while the listing shows a.txt from
    // before the upload of "a v2", and b2.txt under its old name.
    write(&s.p("a.txt"), b"a v3, longer");
    std::fs::rename(s.p("b2.txt"), s.p("b3.txt")).unwrap();
    for _ in 0..3 {
        s.round().await;
    }
    cloud.catch_up();
    s.settle().await;

    let t = local_tree(&s.dir.0);
    let here: Vec<&[u8]> = t.values().filter_map(|c| c.as_deref()).collect();
    let versions = s.versions();
    for c in [&b"a v1"[..], b"a v2", b"a v3, longer", b"bee", b"sea", b"keep", b"new", b"x", b"d v1", b"d v2", b"e"] {
        let kept = here.contains(&c) || versions.iter().any(|v| v == c) || cloud.holds(c);
        assert!(kept, "{:?} was lost", String::from_utf8_lossy(c));
    }
    for (path, c) in [("/b3.txt", &b"bee"[..]), ("/d.txt", b"d v2"), ("/e2.txt", b"e"), ("/new.txt", b"new")] {
        assert_eq!(t.get(path).cloned().flatten().as_deref(), Some(c), "{path}: {:?}", names(&t));
    }
    assert!(here.contains(&&b"a v3, longer"[..]) && here.contains(&&b"keep"[..]) && here.contains(&&b"x"[..]));
    assert!(!here.contains(&&b"sea"[..]) && !t.contains_key("/b.txt") && !t.contains_key("/b2.txt"), "{:?}", names(&t));
}

/// A file renamed here, renamed in OneDrive by the next round, then
/// renamed here again while OneDrive's listing still shows it under its
/// first name (delta lag): the round must not take that old name for a
/// rename in OneDrive and follow it, which undid the second rename here
/// and then in OneDrive too. It waits until OneDrive shows where the file
/// is, and the second rename is sent.
#[tokio::test]
async fn a_listing_behind_our_own_rename_never_undoes_a_rename_here() {
    let cloud = Fake::new();
    let mut s = Sync::new("lag-rename", cloud.clone());
    write(&s.p("b.txt"), b"bee");
    s.settle().await;

    cloud.lag();
    std::fs::rename(s.p("b.txt"), s.p("b2.txt")).unwrap();
    s.round().await;
    assert!(cloud.tree().contains_key("/b2.txt"), "{:?}", names(&cloud.tree()));
    std::fs::rename(s.p("b2.txt"), s.p("b3.txt")).unwrap();
    for _ in 0..3 {
        s.round().await;
        assert!(s.p("b3.txt").exists(), "{:?}", names(&local_tree(&s.dir.0)));
    }
    cloud.catch_up();
    s.settle().await;
    assert_eq!(names(&local_tree(&s.dir.0)), BTreeSet::from(["/b3.txt"]));
    assert_eq!(names(&cloud.tree()), BTreeSet::from(["/b3.txt"]));
}

/// The change journal wraps while a round runs, losing the records of what
/// people did meanwhile: deleted a file and a folder tree, moved a file out
/// of the folder (no deletion), moved one out and deleted it there, saved a
/// file by renaming a new one over it, renamed a file, deleted a file and
/// made a new one of the same name. The folder is rescanned.
///
/// With the agent's journal watcher, the deletions it read are evidence
/// after the rescan: exactly the deleted items' (the save and the re-made
/// file keep their names and send their new content; the moved-out file is
/// held, not deleted), and they're deleted in OneDrive, recoverably.
/// Without it (the journal wrapped past it too), nothing is deleted in
/// OneDrive: every missing synced item is held. Nothing is lost either way.
#[tokio::test]
async fn a_journal_lost_mid_round_never_invents_or_loses_a_deletion() {
    for watch in [true, false] {
        let cloud = Fake::new();
        let mut s = Sync::on_disk(&format!("journal-loss-{watch}"), cloud.clone());
        let outside = PathBuf::from(format!("{}-outside", s.dir.0.display()));
        let _ = std::fs::remove_dir_all(&outside);
        std::fs::create_dir_all(&outside).unwrap();
        let _outside = Tmp(outside.clone());
        let files: &[(&str, &[u8])] = &[
            ("keep.txt", b"keep"),
            ("del.txt", b"delete me"),
            ("deldir/x.txt", b"x"),
            ("deldir/sub/y.txt", b"y"),
            ("out.txt", b"moved out"),
            ("outdel.txt", b"moved out and deleted"),
            ("save.txt", b"save v1"),
            ("ren.txt", b"ren"),
            ("again.txt", b"again v1"),
        ];
        for (p, c) in files {
            write(&s.p(p), c);
        }
        s.settle().await;
        let saved = resolve(&cloud.0.lock().unwrap(), "save.txt");

        // Work for the round, and what people do while it runs.
        for n in 0..3 {
            cloud.add(&format!("c{n}.txt"), Some(format!("cloud {n}").as_bytes()));
        }
        let (dir, out) = (s.dir.0.clone(), outside.clone());
        cloud.0.lock().unwrap().meanwhile = Some(Box::new(move || {
            std::fs::remove_file(dir.join("del.txt")).unwrap();
            std::fs::remove_dir_all(dir.join("deldir")).unwrap();
            std::fs::rename(dir.join("out.txt"), out.join("out.txt")).unwrap();
            std::fs::rename(dir.join("outdel.txt"), out.join("outdel.txt")).unwrap();
            std::fs::remove_file(out.join("outdel.txt")).unwrap();
            std::fs::write(dir.join("save.tmp"), b"save v2").unwrap();
            std::fs::rename(dir.join("save.tmp"), dir.join("save.txt")).unwrap();
            std::fs::rename(dir.join("ren.txt"), dir.join("ren2.txt")).unwrap();
            std::fs::remove_file(dir.join("again.txt")).unwrap();
            std::fs::write(dir.join("again.txt"), b"again v2").unwrap();
        }));
        s.round().await;
        assert!(cloud.0.lock().unwrap().meanwhile.is_none(), "the round made no request");

        let (mut s, noted) = s.lose_journal(watch);
        // del.txt, deldir, x.txt, sub, y.txt, outdel.txt, and the replaced
        // save.txt and again.txt.
        assert_eq!(noted, if watch { 8 } else { 0 }, "watch {watch}");
        for _ in 0..20 {
            let r = s.round().await;
            if r.is_idle() && r.skipped.is_empty() {
                break;
            }
        }
        let problems = s.obs.store().read(|o| o.check_invariants()).unwrap();
        assert!(problems.is_empty(), "watch {watch}: {problems:?}");

        let (here, there) = (local_tree(&s.dir.0), cloud.tree());
        let at = |p: &str| here.get(p).cloned().flatten();
        for (p, c) in [("/keep.txt", &b"keep"[..]), ("/ren2.txt", b"ren"), ("/c0.txt", b"cloud 0")] {
            assert_eq!(at(p).as_deref(), Some(c), "watch {watch}: {p} {:?}", names(&here));
        }
        if watch {
            assert_eq!(at("/save.txt").as_deref(), Some(&b"save v2"[..]), "watch {watch}: {:?}", names(&here));
            assert_eq!(at("/again.txt").as_deref(), Some(&b"again v2"[..]), "watch {watch}: {:?}", names(&here));
            let now = resolve(&cloud.0.lock().unwrap(), "save.txt");
            assert_eq!(now, saved, "the save is a new version of the same item");
        } else {
            // The old ones are held under their names (their deletion has no
            // evidence), so the new ones are kept beside them.
            assert_eq!(at("/save (conflict 1).txt").as_deref(), Some(&b"save v2"[..]), "{:?}", names(&here));
            assert_eq!(at("/again (conflict 1).txt").as_deref(), Some(&b"again v2"[..]), "{:?}", names(&here));
            assert_eq!(there.get("/save.txt").cloned().flatten().as_deref(), Some(&b"save v1"[..]));
        }
        // Held, never deleted: no evidence says it was.
        assert_eq!(there.get("/out.txt").cloned().flatten().as_deref(), Some(&b"moved out"[..]), "watch {watch}");
        for (_, c) in files {
            assert!(cloud.holds(c), "watch {watch}: {:?} was lost", String::from_utf8_lossy(c));
        }
        let deleted = ["/del.txt", "/deldir", "/deldir/x.txt", "/deldir/sub/y.txt", "/outdel.txt"];
        for p in deleted {
            assert!(!here.contains_key(p), "watch {watch}: {p} came back");
            assert_eq!(there.contains_key(p), !watch, "watch {watch}: {p} in OneDrive: {:?}", names(&there));
        }
        if watch {
            let mut agreed = there.clone();
            agreed.remove("/out.txt");
            assert_eq!(s.synced_part(here), s.synced_part(agreed), "watch {watch}");
        }
    }
}

fn user_rules(lines: &[&str]) -> RuleSet {
    RuleSet::new(&[] as &[&str], lines).unwrap()
}

/// Excluding something that's synced stops syncing it: both copies stay,
/// nothing is deleted or held, changes wait. Removing the rule resumes from
/// the last synced state: the edit made meanwhile is sent.
#[tokio::test]
async fn excluding_a_synced_folder_deletes_nothing_and_resumes_when_re_included() {
    let cloud = Fake::new();
    let mut s = Sync::new("exclude-synced", cloud.clone());
    write(&s.p("renders/a.mp4"), b"v1");
    write(&s.p("notes.txt"), b"notes");
    s.settle().await;

    s.rules = user_rules(&["renders/"]);
    s.settle().await;
    write(&s.p("renders/a.mp4"), b"v2, edited while excluded");
    cloud.add("renders/b.mp4", Some(b"new in the cloud"));
    write(&s.p("notes.txt"), b"notes, still synced");
    s.settle().await;
    assert_eq!(cloud.tree()["/renders/a.mp4"].as_deref(), Some(&b"v1"[..]), "not sent while excluded");
    assert!(!s.p("renders/b.mp4").exists(), "not fetched while excluded");
    assert_eq!(cloud.tree()["/notes.txt"].as_deref(), Some(&b"notes, still synced"[..]));
    assert!(s.versions().is_empty(), "nothing replaced or recycled");

    s.rules = RuleSet::default();
    s.settle().await;
    assert_eq!(cloud.tree()["/renders/a.mp4"].as_deref(), Some(&b"v2, edited while excluded"[..]));
    assert_eq!(std::fs::read(s.p("renders/b.mp4")).unwrap(), b"new in the cloud");
    assert!(cloud.holds(b"v1"), "OneDrive's history keeps the old version");
}

/// A deletion made while excluded is recorded (with evidence) and goes
/// through when the rule is removed, recoverably: what "resume" means.
#[tokio::test]
async fn a_deletion_while_excluded_goes_through_on_re_include_recoverably() {
    let cloud = Fake::new();
    let mut s = Sync::new("exclude-delete", cloud.clone());
    write(&s.p("drafts/old.txt"), b"old draft");
    s.settle().await;
    s.rules = user_rules(&["drafts/"]);
    s.settle().await;
    std::fs::remove_file(s.p("drafts/old.txt")).unwrap();
    s.settle().await;
    assert!(cloud.tree().contains_key("/drafts/old.txt"), "kept in the cloud while excluded");
    s.rules = RuleSet::default();
    s.settle().await;
    assert!(!cloud.tree().contains_key("/drafts/old.txt"));
    assert!(cloud.holds(b"old draft"), "in OneDrive's recycle bin");
}

/// Excluded new items stay on their side; a `!` rule brings one back.
#[tokio::test]
async fn excluded_new_items_stay_on_their_side() {
    let cloud = Fake::new();
    let mut s = Sync::new("exclude-new", cloud.clone());
    s.rules = user_rules(&["*.iso", "!keep.iso", "/cache/"]);
    write(&s.p("disk.iso"), b"big");
    write(&s.p("keep.iso"), b"wanted");
    write(&s.p("cache/x.bin"), b"cache");
    write(&s.p("docs/cache/y.txt"), b"not the top-level cache");
    cloud.add("cloud.iso", Some(b"cloud big"));
    s.settle().await;
    let t = cloud.tree();
    assert!(t.contains_key("/keep.iso") && t.contains_key("/docs/cache/y.txt"), "{:?}", names(&t));
    assert!(!t.contains_key("/disk.iso") && !t.contains_key("/cache"), "{:?}", names(&t));
    assert!(!s.p("cloud.iso").exists());
    assert!(s.p("disk.iso").exists() && s.p("cache/x.bin").exists() && cloud.tree().contains_key("/cloud.iso"));
}

impl Sync {
    /// What `oneup cloud prune --yes` does for the cloud item at `path`, and
    /// what the agent does with a ready candidate in `prune` mode:
    /// one PruneRemote through the gate and the executor.
    async fn prune(&mut self, path: &str) -> Report {
        let (id, etag) = {
            let s = self.cloud.0.lock().unwrap();
            let id = resolve(&s, path);
            let etag = s.items[&id].etag.to_string();
            (id, etag)
        };
        let store = self.obs.store_mut();
        let forget = store
            .read(|o| {
                let Some(b) = o.synced_by_remote(&id)? else { return Ok(Vec::new()) };
                let mut all = vec![b.sid];
                let mut i = 0;
                while i < all.len() {
                    let kids = o.synced_children(all[i])?;
                    all.extend(kids.into_iter().map(|k| k.sid));
                    i += 1;
                }
                all.reverse();
                Ok(all)
            })
            .unwrap();
        // The copy here, as `oneup_app::prune` finds it.
        let keep = store
            .read(|o| {
                let mut v = Vec::new();
                for sid in &forget {
                    let Some(l) = o.synced_get(*sid)?.and_then(|b| b.local) else { continue };
                    if let Some(row) = o.local_get(l.0)? {
                        let stamp = (row.kind != oneup_store::LocalKind::Folder).then_some((row.size, row.mtime));
                        v.push(LocalPre { id: l, stamp });
                    }
                }
                Ok(v)
            })
            .unwrap();
        let op = Op::PruneRemote { remote: RemotePre { id: RemoteId(id), etag }, forget, items: 2, exact: true, keep };
        let r = review_in(&Policy::default(), &Context::default(), vec![op]);
        assert!(r.breakers.is_empty(), "{:?}", r.breakers);
        self.exec.run(store, r.approved).await
    }
}

/// Pruning removes the cloud copy (into the recycle bin) and nothing here;
/// removing the rule later uploads the copy here again rather than reading
/// the prune as a deletion to bring to this PC. Also with a crash at each
/// of the prune's crash points.
#[tokio::test]
async fn pruning_removes_only_the_cloud_copy_and_re_including_uploads_it_again() {
    for crash in [None, Some(0), Some(1)] {
        let tag = format!("prune-{crash:?}").replace(['(', ')'], "");
        let cloud = Fake::new();
        let mut s = Sync::on_disk(&tag, cloud.clone());
        write(&s.p("renders/a.mp4"), b"render a");
        write(&s.p("renders/b.mp4"), b"render b");
        write(&s.p("notes.txt"), b"notes");
        s.settle().await;
        s.rules = user_rules(&["renders/"]);
        s.settle().await;

        s.exec.crash_after(crash);
        let r = s.prune("renders").await;
        if crash.is_some() {
            assert_eq!(r.halted.as_deref(), Some(CRASHED));
            s = s.restart();
        } else {
            assert_eq!(r.done.len(), 1, "{r:?}");
        }
        s.settle().await;
        if crash != Some(0) {
            assert!(!cloud.tree().contains_key("/renders"), "pruned");
            assert!(cloud.holds(b"render a") && cloud.holds(b"render b"), "in OneDrive's recycle bin");
        }
        assert_eq!(std::fs::read(s.p("renders/a.mp4")).unwrap(), b"render a", "the copy here stays");
        assert!(s.versions().is_empty());

        s.rules = RuleSet::default();
        s.settle().await;
        assert_eq!(std::fs::read(s.p("renders/b.mp4")).unwrap(), b"render b", "still here after re-including");
        assert_eq!(cloud.tree()["/renders/a.mp4"].as_deref(), Some(&b"render a"[..]), "sent again");
        assert!(s.versions().is_empty(), "nothing here was replaced or recycled");
    }
}

/// What the store on disk says while the cloud removes `id`: its durable
/// commits so far, and whether a Synced row still points at `id`. Read
/// through a connection of its own, as after a restart.
type AtRemoval = Arc<Mutex<Option<(u64, bool)>>>;

fn watch_removal(s: &Sync, id: String) -> AtRemoval {
    let db = s.db.as_ref().expect("a database on disk").0.clone();
    let seen: AtRemoval = Arc::default();
    let out = seen.clone();
    s.cloud.0.lock().unwrap().on_remove = Some(Arc::new(move |removed: &str| {
        if removed == id {
            let store = Store::open(&db, 1024).unwrap();
            let state = store.read(|o| Ok((o.durable_commits()?, o.synced_by_remote(&id)?.is_some()))).unwrap();
            *out.lock().unwrap() = Some(state);
        }
    }));
    seen
}

/// Pruning forgets the Synced rows durably before OneDrive removes the
/// item: rolled back by a power cut after the prune, they would read the
/// cloud's deletion as one to bring to this PC.
#[tokio::test]
async fn pruning_forgets_its_records_durably_before_the_cloud_removes_them() {
    let cloud = Fake::new();
    let mut s = Sync::on_disk("prune-durable", cloud.clone());
    write(&s.p("renders/a.mp4"), b"render a");
    s.settle().await;
    s.rules = user_rules(&["renders/"]);
    s.settle().await;
    let id = resolve(&cloud.0.lock().unwrap(), "/renders");
    let before = s.obs.store().read(|o| o.durable_commits()).unwrap();
    let seen = watch_removal(&s, id);
    let r = s.prune("renders").await;
    assert_eq!(r.done.len(), 1, "{r:?}");
    let (durable, linked) = seen.lock().unwrap().expect("the cloud pruned it");
    assert!(!linked, "the Synced rows were gone before the prune");
    assert_eq!(durable, before + 1, "and they went in a durable commit");
}

/// A move in parts flushes what it recorded before it removes the emptied
/// original from OneDrive.
#[tokio::test]
async fn a_move_in_parts_is_recorded_durably_before_the_original_is_removed() {
    let cloud = Fake::new();
    let mut s = Sync::on_disk("large-durable", cloud.clone());
    large_folder(&s);
    s.settle().await;
    cloud.0.lock().unwrap().threshold = Some(5);
    let id = resolve(&cloud.0.lock().unwrap(), "/Projects/Bookmarks");
    let before = s.obs.store().read(|o| o.durable_commits()).unwrap();
    let seen = watch_removal(&s, id);
    move_large_folder_in_parts(&mut s).await;
    let (durable, linked) = seen.lock().unwrap().expect("the original was removed");
    assert!(!linked, "the Synced row had followed the new folder");
    assert!(durable > before, "in a durable commit before the removal");
}

/// `pull`: excluded items come from OneDrive and are never sent. A change
/// here waits; once OneDrive's copy changes too, both are kept. New cloud
/// files arrive; files only here, and deletions here, stay here.
#[tokio::test]
async fn pull_mode_brings_changes_here_and_never_sends() {
    let cloud = Fake::new();
    let mut s = Sync::new("pull", cloud.clone());
    write(&s.p("drafts/a.txt"), b"a v1");
    write(&s.p("drafts/b.txt"), b"b v1");
    s.settle().await;
    // Anchored, as the settings window writes it: a folder excluded by name
    // alone (`drafts/`) is never listed, so nothing in it is brought here.
    s.rules = user_rules(&["/drafts/"]);
    s.pull = true;
    s.settle().await;

    write(&s.p("drafts/a.txt"), b"a edited here");
    write(&s.p("drafts/only-here.txt"), b"local only");
    std::fs::remove_file(s.p("drafts/b.txt")).unwrap();
    cloud.add("drafts/new.txt", Some(b"new in the cloud"));
    s.settle().await;
    let t = cloud.tree();
    assert_eq!(t["/drafts/a.txt"].as_deref(), Some(&b"a v1"[..]), "not sent");
    assert!(!t.contains_key("/drafts/only-here.txt") && t.contains_key("/drafts/b.txt"), "{:?}", names(&t));
    assert_eq!(std::fs::read(s.p("drafts/new.txt")).unwrap(), b"new in the cloud", "brought here");

    // OneDrive's copy changes: a conflict, both kept here, nothing sent.
    cloud.edit("drafts/a.txt", b"a edited in the cloud");
    s.settle().await;
    let here = local_tree(&s.dir.0);
    let a: Vec<&[u8]> =
        here.iter().filter(|(p, _)| p.starts_with("/drafts/a")).filter_map(|(_, c)| c.as_deref()).collect();
    assert!(a.contains(&&b"a edited here"[..]) && a.contains(&&b"a edited in the cloud"[..]), "{a:?}");
    assert_eq!(std::fs::read(s.p("drafts/a.txt")).unwrap(), b"a edited in the cloud");
    assert_eq!(cloud.tree()["/drafts/a.txt"].as_deref(), Some(&b"a edited in the cloud"[..]));
    assert_eq!(cloud.tree().keys().filter(|p| p.starts_with("/drafts/a")).count(), 1, "the conflict copy isn't sent");
}

// ------------------------------------------------------------- on demand

use oneup_cfapi::provider;
use oneup_exec::hydrate::{Hydrator, Serve};

/// The sync folder registered as our sync root (with the harness's pair
/// token), served by the real hydration service over the fake cloud.
/// Dropped: disconnected, unregistered, removed.
struct OnDemand {
    path: PathBuf,
    conn: Option<provider::Connection>,
    hydrator: Arc<Hydrator<Fake>>,
}

impl OnDemand {
    fn new(path: &Path, db: &Path, cloud: &Fake) -> Self {
        Self::with_ours(path, db, cloud, None)
    }

    /// Serving OneUpDrive's own requests (pinned files) through `ours`.
    fn with_ours(path: &Path, db: &Path, cloud: &Fake, ours: Option<Fake>) -> Self {
        provider::register(path, b"test-token").unwrap();
        let staging = PathBuf::from(format!("{}.staging", path.display()));
        let h = Hydrator::with_ours(
            cloud.clone(),
            ours,
            Store::open(db, 1024).unwrap(),
            staging,
            path,
            tokio::runtime::Handle::current(),
            Box::new(|| {}),
        )
        .unwrap();
        let conn = provider::connect(path, Arc::new(Serve(Arc::clone(&h)))).unwrap();
        Self { path: path.to_path_buf(), conn: Some(conn), hydrator: h }
    }
}

impl Drop for OnDemand {
    fn drop(&mut self) {
        self.conn.take();
        let _ = provider::unregister(&self.path);
        let _ = std::fs::remove_dir_all(&self.path);
        let _ = std::fs::remove_dir_all(format!("{}.staging", self.path.display()));
    }
}

/// A sync folder in on-demand mode, with its database on disk (the
/// hydration service reads it through its own connection).
fn on_demand_sync(tag: &str, cloud: &Fake) -> (OnDemand, Sync) {
    on_demand_sync_with(tag, cloud, None)
}

fn on_demand_sync_with(tag: &str, cloud: &Fake, ours: Option<Fake>) -> (OnDemand, Sync) {
    let dir = Tmp(std::env::temp_dir().join(format!("oneup-exec-{tag}-{}", std::process::id())));
    let _ = std::fs::remove_dir_all(&dir.0);
    std::fs::create_dir_all(&dir.0).unwrap();
    let db = PathBuf::from(format!("{}.db", dir.0.display()));
    drop(DbFile(db.clone()));
    drop(Store::open(&db, 1024).unwrap());
    let od = OnDemand::with_ours(&dir.0, &db, cloud, ours);
    let s = Sync::open(dir, Some(DbFile(db)), cloud.clone());
    (od, s)
}

fn placeholder(p: &Path) -> provider::Info {
    provider::info(&provider::open(p).unwrap())
        .unwrap()
        .unwrap_or_else(|| panic!("{} isn't a placeholder", p.display()))
}

/// What another program reads (`cmd /c copy`), which hydrates on demand.
fn app_read(p: &Path) -> Option<Vec<u8>> {
    use std::os::windows::process::CommandExt;
    static N: std::sync::atomic::AtomicU32 = std::sync::atomic::AtomicU32::new(0);
    let n = N.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
    let out = std::env::temp_dir().join(format!("oneup-exec-read-{}-{n}.out", std::process::id()));
    let _ = std::fs::remove_file(&out);
    let ok = std::process::Command::new("cmd")
        .raw_arg(format!("/C copy /Y /B \"{}\" \"{}\" >nul 2>nul", p.display(), out.display()))
        .status()
        .unwrap()
        .success();
    let r = if ok { std::fs::read(&out).ok() } else { None };
    let _ = std::fs::remove_file(&out);
    r
}

fn app_append(p: &Path, text: &str) {
    use std::os::windows::process::CommandExt;
    let st = std::process::Command::new("cmd").raw_arg(format!("/C echo {text}>>\"{}\"", p.display())).status();
    assert!(st.unwrap().success());
}

impl Sync {
    /// Rounds until one does nothing (without reading local files, which
    /// would hydrate placeholders).
    async fn quiesce(&mut self) {
        for _ in 0..20 {
            let r = self.round().await;
            if r.is_idle() && r.skipped.is_empty() {
                std::thread::sleep(Duration::from_millis(150));
                if self.round().await.is_idle() {
                    return;
                }
            }
        }
        panic!("no quiet round");
    }
}

/// Files On-Demand end to end: cloud files arrive as dehydrated
/// placeholders and are downloaded when a program opens them; cloud edits
/// update placeholders (dehydrated or hydrated) without losing anything;
/// local edits and new files are sent and then marked in sync; cloud
/// deletions remove placeholders, keeping any local content.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn on_demand_placeholders_follow_both_sides() {
    let cloud = Fake::new();
    cloud.add("docs", None);
    cloud.add("docs/a.txt", Some(b"cloud a v1"));
    cloud.add("b.txt", Some(b"cloud b v1"));
    cloud.add("c.txt", Some(b"never opened here"));
    let (od, mut s) = on_demand_sync("ondemand", &cloud);
    assert!(s.exec.local().on_demand());
    s.quiesce().await;

    let (pa, pb) = (s.p("docs").join("a.txt"), s.p("b.txt"));
    let i = placeholder(&pa);
    assert!(i.in_sync && i.on_disk == 0, "arrives without its content: {i:?}");
    assert_eq!(std::fs::metadata(&pa).unwrap().len(), 10);
    assert_eq!(app_read(&pb).as_deref(), Some(&b"cloud b v1"[..]), "opened: downloaded");
    assert_eq!(placeholder(&pb).on_disk, 10);
    let log = s.obs.store().read(|o| o.activity_recent(Some("/b.txt"), 1)).unwrap();
    assert!(log.first().is_some_and(|e| e.action == "open" && e.detail.contains("cmd.exe")), "{log:?}");

    // Changed in the cloud but not yet updated here: opening it is
    // refused (never the new bytes in the old placeholder).
    cloud.edit("docs/a.txt", b"cloud a v1.5");
    s.cloud.pull(s.obs.store_mut());
    assert_eq!(app_read(&pa), None);
    assert!(od.hydrator.refused.lock().unwrap().contains(&oneup_exec::hydrate::Refusal::NotCurrent));
    assert_eq!(placeholder(&pa).on_disk, 0);

    // Cloud edits: a dehydrated placeholder takes the new version; a
    // hydrated one is dehydrated with it (its old content is OneDrive's
    // previous version).
    cloud.edit("docs/a.txt", b"cloud a v2, longer");
    cloud.edit("b.txt", b"cloud b v2");
    s.quiesce().await;
    assert_eq!(placeholder(&pa).on_disk, 0);
    assert_eq!(app_read(&pa).as_deref(), Some(&b"cloud a v2, longer"[..]));
    assert_eq!(placeholder(&pb).on_disk, 0, "dehydrated with the new version");
    assert_eq!(app_read(&pb).as_deref(), Some(&b"cloud b v2"[..]));
    assert!(cloud.holds(b"cloud b v1"));

    // A program edits a hydrated file; a new local file appears.
    app_append(&pb, "edited here");
    write(&s.p("docs").join("new.txt"), b"made here");
    s.quiesce().await;
    let sent = cloud.tree()["/b.txt"].clone().unwrap();
    assert!(String::from_utf8_lossy(&sent).contains("edited here"), "the edit was sent");
    assert!(placeholder(&pb).in_sync, "marked in sync after the upload");

    // A program saves it while it's being sent: the save goes through and
    // the saved version is sent next.
    app_append(&pb, "edited again");
    cloud.0.lock().unwrap().saved_meanwhile =
        Some(SaveDuringUpload { by_app: true, ..SaveDuringUpload::in_place(pb.clone(), b"saved during the upload") });
    s.quiesce().await;
    assert!(cloud.0.lock().unwrap().saved_meanwhile.is_none(), "the upload began");
    let sent = cloud.tree()["/b.txt"].clone().unwrap();
    assert!(String::from_utf8_lossy(&sent).contains("saved during the upload"), "{:?}", String::from_utf8_lossy(&sent));
    assert!(placeholder(&pb).in_sync);

    // Saved by renaming a new file over it (`MoveFileEx`, which fails while
    // any other handle on the file is open) while it's being sent: the save
    // goes through, and the saved version is sent next.
    app_append(&pb, "edited once more");
    cloud.0.lock().unwrap().saved_meanwhile =
        Some(SaveDuringUpload { by_move: true, ..SaveDuringUpload::in_place(pb.clone(), b"saved by rename") });
    s.quiesce().await;
    assert!(cloud.0.lock().unwrap().saved_meanwhile.is_none(), "the upload began");
    assert_eq!(cloud.tree()["/b.txt"].as_deref(), Some(&b"saved by rename"[..]));
    assert_eq!(app_read(&pb).as_deref(), Some(&b"saved by rename"[..]));
    assert_eq!(cloud.tree()["/docs/new.txt"].as_deref(), Some(&b"made here"[..]));
    let n = placeholder(&s.p("docs").join("new.txt"));
    assert!(n.in_sync && n.on_disk == 9, "converted, content kept: {n:?}");

    // Cloud deletions: the dehydrated placeholder goes, without downloading
    // it and without a copy (no local content); the hydrated file goes to
    // the version store, as an ordinary file.
    cloud.rename("c.txt", "c2.txt");
    s.quiesce().await;
    let pc = s.p("c2.txt");
    assert_eq!(placeholder(&pc).on_disk, 0, "moved without being downloaded");
    cloud.delete("c2.txt");
    cloud.delete("docs/new.txt");
    s.quiesce().await;
    assert!(!pc.exists() && !s.p("docs").join("new.txt").exists());
    assert_eq!(s.versions(), vec![b"made here".to_vec()]);
    assert!(cloud.holds(b"never opened here"), "in OneDrive's recycle bin");
    let kept = std::fs::read_dir(s.p(".oneup").join("versions")).unwrap().flatten().next().unwrap().path();
    let kept = std::fs::read_dir(kept).unwrap().flatten().find(|e| e.file_name() != "origin.txt").unwrap().path();
    assert!(provider::info(&provider::open(&kept).unwrap()).unwrap().is_none(), "kept as an ordinary file");
    let problems = s.obs.store().read(|o| o.check_invariants()).unwrap();
    assert!(problems.is_empty(), "{problems:?}");
    drop(s);
    drop(od);
}

/// Files already here with the same content as OneDrive's (a first sync
/// over an existing folder, as after moving over from the OneDrive app)
/// become placeholders in sync with their content kept, as uploads and
/// downloads leave them: not ordinary files that Explorer shows as pending
/// forever and that can't be freed up.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn on_demand_files_already_here_become_placeholders_in_sync() {
    let cloud = Fake::new();
    cloud.add("docs", None);
    cloud.add("docs/same.txt", Some(b"the same on both sides"));
    cloud.add("top.txt", Some(b"also the same"));
    let dir = Tmp(std::env::temp_dir().join(format!("oneup-exec-adopt-{}", std::process::id())));
    let _ = std::fs::remove_dir_all(&dir.0);
    std::fs::create_dir_all(dir.0.join("docs")).unwrap();
    write(&dir.0.join("docs").join("same.txt"), b"the same on both sides");
    write(&dir.0.join("top.txt"), b"also the same");
    let db = PathBuf::from(format!("{}.db", dir.0.display()));
    drop(DbFile(db.clone()));
    drop(Store::open(&db, 1024).unwrap());
    let od = OnDemand::new(&dir.0, &db, &cloud);
    let mut s = Sync::open(dir, Some(DbFile(db)), cloud.clone());
    assert!(s.exec.local().on_demand());
    s.quiesce().await;
    for (p, len) in [(s.p("docs").join("same.txt"), 22), (s.p("top.txt"), 13)] {
        let i = placeholder(&p);
        assert!(i.in_sync && i.on_disk == len, "{}: {i:?}", p.display());
        assert_eq!(std::fs::metadata(&p).unwrap().len(), len as u64);
    }
    assert!(placeholder(&s.p("docs")).in_sync, "the folder too");
    assert!(cloud.tree().len() == 3, "nothing was sent again: {:?}", cloud.tree().keys());
    drop(s);
    drop(od);
}

/// Exclusions only stop uploads (`pull`): an excluded file only in
/// OneDrive arrives here online-only. Removing it from OneDrive waits until
/// its content is here; then the placeholder becomes an ordinary file.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn on_demand_pruning_waits_for_the_content_and_leaves_an_ordinary_file() {
    let cloud = Fake::new();
    cloud.add("a.env", Some(b"SECRET=1"));
    let (od, mut s) = on_demand_sync("prune-od", &cloud);
    s.rules = user_rules(&["*.env"]);
    s.pull = true;
    s.quiesce().await;
    let p = s.p("a.env");
    assert_eq!(placeholder(&p).on_disk, 0, "excluded, and still here online-only");

    let r = s.prune("a.env").await;
    assert!(r.done.is_empty(), "{r:?}");
    assert!(cloud.tree().contains_key("/a.env"), "not removed: its content isn't here");

    assert_eq!(app_read(&p).as_deref(), Some(&b"SECRET=1"[..]), "downloaded when opened");
    s.quiesce().await;
    let r = s.prune("a.env").await;
    assert_eq!(r.done.len(), 1, "{r:?}");
    assert!(!cloud.tree().contains_key("/a.env") && cloud.holds(b"SECRET=1"), "in OneDrive's recycle bin");
    assert!(provider::info(&provider::open(&p).unwrap()).unwrap().is_none(), "an ordinary file now");
    assert_eq!(std::fs::read(&p).unwrap(), b"SECRET=1");
    s.quiesce().await;
    assert!(!cloud.tree().contains_key("/a.env"), "excluded: never sent back");
    drop(s);
    drop(od);
}

/// A hydrated placeholder moved by a cloud rename, then edited in the
/// cloud: the move leaves it in sync, so the cloud edit still updates it.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn on_demand_moved_hydrated_files_still_take_cloud_edits() {
    let cloud = Fake::new();
    cloud.add("m.txt", Some(b"moved v1"));
    let (od, mut s) = on_demand_sync("ondemand-move", &cloud);
    s.quiesce().await;
    assert_eq!(app_read(&s.p("m.txt")).as_deref(), Some(&b"moved v1"[..]));
    cloud.rename("m.txt", "m2.txt");
    s.quiesce().await;
    let p = s.p("m2.txt");
    let i = placeholder(&p);
    assert!(i.in_sync && i.on_disk == 8, "{i:?}");
    cloud.edit("m2.txt", b"moved v2!");
    s.quiesce().await;
    assert_eq!(placeholder(&p).on_disk, 0);
    assert_eq!(app_read(&p).as_deref(), Some(&b"moved v2!"[..]));
    drop(s);
    drop(od);
}

impl Sync {
    /// What the agent's placement pass does to free space: the gate, then
    /// the executor.
    async fn free_up(&mut self) -> Report {
        let ops = oneup_exec::placement::free_ops(self.obs.store()).unwrap();
        let r = review_in(&Policy::default(), &Context::default(), ops);
        self.exec.run(self.obs.store_mut(), r.approved).await
    }
}

/// "Always keep on this device" downloads; "Free up space" frees, through
/// the gate, only content that is the cloud's current version: an edit
/// made here is first sent, then freed.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn on_demand_pinned_files_download_and_freed_files_free_up() {
    let cloud = Fake::new();
    cloud.add("keep.txt", Some(b"always kept"));
    cloud.add("free.txt", Some(b"free me up"));
    let (od, mut s) = on_demand_sync("ondemand-pin", &cloud);
    s.quiesce().await;
    let (pk, pf) = (s.p("keep.txt"), s.p("free.txt"));

    provider::set_pin(&provider::open(&pk).unwrap(), provider::Pin::Pinned).unwrap();
    s.quiesce().await;
    // Asked before each file: a "no" (a pause, say) stops the pass there,
    // says why, and says more is waiting.
    use oneup_exec::placement::Stop;
    let fetched =
        oneup_exec::placement::fetch_pinned(s.obs.store(), &s.dir.0, &|_, _| Err(Stop::Paused)).await.unwrap();
    assert!(fetched.done.is_empty() && placeholder(&pk).on_disk == 0, "nothing downloaded");
    assert_eq!((fetched.stopped, fetched.more), (Some(Stop::Paused), true));
    let fetched = oneup_exec::placement::fetch_pinned(s.obs.store(), &s.dir.0, &|_, size| {
        if size == 11 { Ok(()) } else { Err(Stop::Share) }
    })
    .await
    .unwrap();
    assert_eq!(fetched.done.iter().map(|d| d.0.as_str()).collect::<Vec<_>>(), ["/keep.txt"]);
    assert_eq!((fetched.stopped, fetched.more), (None, false), "a batch not full: nothing more");
    assert_eq!(placeholder(&pk).on_disk, 11, "downloaded");

    assert_eq!(app_read(&pf).as_deref(), Some(&b"free me up"[..]));
    // Edited here before being freed: not freed until the edit is sent.
    app_append(&pf, "plus an edit");
    provider::set_pin(&provider::open(&pf).unwrap(), provider::Pin::Unpinned).unwrap();
    s.obs.catch_up().unwrap();
    let r = s.free_up().await;
    assert!(r.done.is_empty(), "{r:?}");
    s.quiesce().await;
    assert!(String::from_utf8_lossy(&cloud.tree()["/free.txt"].clone().unwrap()).contains("plus an edit"));
    s.obs.catch_up().unwrap();
    let r = s.free_up().await;
    assert_eq!(r.done.len(), 1, "{r:?}");
    assert_eq!(placeholder(&pf).on_disk, 0, "freed");
    let back = app_read(&pf).unwrap();
    assert!(String::from_utf8_lossy(&back).contains("plus an edit"), "the edit comes back from OneDrive");
    drop(s);
    drop(od);
}

/// A pinned file ("Always keep on this device", by the user or a pin rule)
/// is never freed: a freeing operation for it, as a space policy would plan
/// it (stamp and hash right, approved by the gate), is refused by the
/// effects layer on its share-none handle, and the content stays.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn on_demand_a_pinned_file_is_never_freed() {
    let cloud = Fake::new();
    cloud.add("kept.psd", Some(b"layers"));
    let (od, mut s) = on_demand_sync("ondemand-pinned-kept", &cloud);
    s.quiesce().await;
    let p = s.p("kept.psd");
    assert_eq!(app_read(&p).as_deref(), Some(&b"layers"[..]), "downloaded when opened");
    provider::set_pin(&provider::open(&p).unwrap(), provider::Pin::Pinned).unwrap();
    s.obs.catch_up().unwrap();
    let id = s.local_id("kept.psd");
    let row = s.obs.store().read(|o| o.local_get(id.0)).unwrap().unwrap();
    let op = Op::Dehydrate { local: LocalPre { id, stamp: Some((row.size, row.mtime)) }, hash: qxh(b"layers") };
    let r = review_in(&Policy::default(), &Context::default(), vec![op]);
    assert_eq!(r.approved.len(), 1, "the gate has no reason to stop it");
    let report = s.exec.run(s.obs.store_mut(), r.approved).await;
    assert!(report.done.is_empty(), "{report:?}");
    let info = placeholder(&p);
    assert_eq!((info.on_disk, info.pin), (6, provider::Pin::Pinned), "still here, still pinned");
    // Unpinned, the same operation frees it: the refusal was the pin.
    provider::set_pin(&provider::open(&p).unwrap(), provider::Pin::Unspecified).unwrap();
    s.obs.catch_up().unwrap();
    let row = s.obs.store().read(|o| o.local_get(id.0)).unwrap().unwrap();
    let op = Op::Dehydrate { local: LocalPre { id, stamp: Some((row.size, row.mtime)) }, hash: qxh(b"layers") };
    let r = review_in(&Policy::default(), &Context::default(), vec![op]);
    let report = s.exec.run(s.obs.store_mut(), r.approved).await;
    assert_eq!(report.done.len(), 1, "{report:?}");
    assert_eq!(placeholder(&p).on_disk, 0);
    drop(s);
    drop(od);
}

/// A full batch of pinned files downloaded says more may wait, so the
/// agent comes back for the rest at once instead of at its next timed check.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn on_demand_a_full_batch_of_pinned_files_says_more_may_wait() {
    let cloud = Fake::new();
    let n = oneup_exec::placement::BATCH;
    for i in 0..n {
        cloud.add(&format!("p{i}.txt"), Some(b"kept"));
    }
    let (od, mut s) = on_demand_sync("ondemand-pin-batch", &cloud);
    s.quiesce().await;
    for i in 0..n {
        provider::set_pin(&provider::open(&s.p(&format!("p{i}.txt"))).unwrap(), provider::Pin::Pinned).unwrap();
    }
    s.quiesce().await;
    let fetched = oneup_exec::placement::fetch_pinned(s.obs.store(), &s.dir.0, &|_, _| Ok(())).await.unwrap();
    assert_eq!((fetched.done.len(), fetched.stopped, fetched.more), (n, None, true));
    drop(s);
    drop(od);
}

/// Pinned files download through OneUpDrive's own cloud (the agent's is
/// on the data budget), decided before downloading; a program opening a
/// file goes through the other. Here OneUpDrive's own cloud has nothing,
/// so the pinned download fails while opening still works. A network that
/// says no before a pinned file leaves it for the next pass.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn on_demand_pins_download_on_the_budget_and_opening_files_does_not() {
    let cloud = Fake::new();
    cloud.add("keep.txt", Some(b"always kept"));
    cloud.add("open.txt", Some(b"opened by a program"));
    let budgeted = Fake::new();
    let (od, mut s) = on_demand_sync_with("ondemand-budget", &cloud, Some(budgeted));
    s.quiesce().await;
    let pk = s.p("keep.txt");
    provider::set_pin(&provider::open(&pk).unwrap(), provider::Pin::Pinned).unwrap();
    s.quiesce().await;
    use oneup_exec::placement::Stop;
    let none = oneup_exec::placement::fetch_pinned(s.obs.store(), &s.dir.0, &|_, _| Err(Stop::Paused)).await.unwrap();
    assert!(none.done.is_empty() && none.attempted.is_empty(), "the network said no");
    let fetched = oneup_exec::placement::fetch_pinned(s.obs.store(), &s.dir.0, &|_, _| Ok(())).await.unwrap();
    assert!(fetched.done.is_empty(), "served by OneUpDrive's own (budgeted) cloud, which hasn't got it");
    assert_eq!(placeholder(&pk).on_disk, 0);
    assert_eq!(app_read(&s.p("open.txt")).as_deref(), Some(&b"opened by a program"[..]), "opening isn't");
    drop(s);
    drop(od);
}

// ------------------------------------------------------------- everyday use
//
// What people do in File Explorer and on the web, one scenario each, so
// basic use breaks a test before it reaches anyone. Several come from
// problems seen in use.

impl Fake {
    /// Moves an item into another folder (as dragging it on the web does).
    fn move_to(&self, path: &str, folder: &str) {
        let mut s = self.0.lock().unwrap();
        let id = resolve(&s, path);
        let to = resolve(&s, folder);
        let etag = s.bump();
        let it = s.items.get_mut(&id).unwrap();
        it.parent = Some(to);
        it.etag = etag;
    }

    fn uploaded(&self) -> u64 {
        self.0.lock().unwrap().uploaded
    }
}

impl Sync {
    /// Records the deletion of the Synced items named `names` that are no
    /// longer here, and marks their folders: what the agent does for items
    /// it finds in the user's Recycle Bin (`oneup_app::recyclebin`).
    fn note_deleted(&mut self, names: &[&str]) -> usize {
        let store = self.obs.store_mut();
        store
            .write(|o| {
                let mut n = 0;
                for b in o.synced_all()? {
                    let Some(l) = b.local else { continue };
                    if names.contains(&b.name.as_str()) && o.local_get(l.0)?.is_none() {
                        o.note_local_deleted(l.0)?;
                        o.dirty_mark_synced(&[b.sid])?;
                        n += 1;
                    }
                }
                Ok(n)
            })
            .unwrap()
    }

    /// `n` rounds, for scenarios that end with something deliberately left
    /// undone (so they never settle).
    async fn rounds(&mut self, n: usize) -> Vec<Report> {
        let mut out = Vec::new();
        for _ in 0..n {
            let r = self.round().await;
            let idle = r.is_idle();
            out.push(r);
            if idle {
                std::thread::sleep(Duration::from_millis(100));
            }
        }
        out
    }
}

/// A folder outside the synced one, standing in for the Recycle Bin (the
/// same volume, so a move is a rename, as Explorer's delete is).
fn bin(tag: &str) -> Tmp {
    let b = Tmp(std::env::temp_dir().join(format!("oneup-exec-bin-{tag}-{}", std::process::id())));
    let _ = std::fs::remove_dir_all(&b.0);
    std::fs::create_dir_all(&b.0).unwrap();
    b
}

/// Unticking a synced folder in the settings window writes the rule naming
/// exactly it (`/projects/`, `oneup_app::filetree::set_sync`). Nothing is
/// lost on either side. With OneDrive's copies kept (`keep`), both copies
/// stay as they are, an edit here waits, and the rest of the folder keeps
/// syncing; with `prune`, OneDrive's copies go to its recycle bin, through
/// the gate, once everything is here, and the copies here stay.
#[tokio::test]
async fn everyday_unticking_a_synced_folder_loses_nothing_and_does_what_was_chosen() {
    for prune in [false, true] {
        let cloud = Fake::new();
        let mut s = Sync::new(if prune { "untick-prune" } else { "untick-keep" }, cloud.clone());
        write(&s.p("projects/app/main.rs"), b"fn main() {}");
        write(&s.p("projects/notes.md"), b"notes");
        write(&s.p("letters/a.txt"), b"letter");
        s.settle().await;
        assert!(cloud.tree().contains_key("/projects/app/main.rs"));

        s.rules = user_rules(&["/projects/"]);
        s.settle().await;
        let t = cloud.tree();
        assert_eq!(t["/projects/app/main.rs"].as_deref(), Some(&b"fn main() {}"[..]), "nothing removed by the rule");
        assert_eq!(std::fs::read(s.p("projects/notes.md")).unwrap(), b"notes");
        if prune {
            let r = s.prune("projects").await;
            assert_eq!(r.done.len(), 1, "{r:?}");
            s.settle().await;
            assert!(!cloud.tree().contains_key("/projects"), "OneDrive's copies removed");
            assert!(cloud.holds(b"fn main() {}") && cloud.holds(b"notes"), "into OneDrive's recycle bin");
        } else {
            write(&s.p("projects/notes.md"), b"notes, edited while excluded");
            s.settle().await;
            assert_eq!(cloud.tree()["/projects/notes.md"].as_deref(), Some(&b"notes"[..]), "not sent");
        }
        assert_eq!(std::fs::read(s.p("projects/app/main.rs")).unwrap(), b"fn main() {}", "the copy here stays");
        assert!(s.versions().is_empty(), "nothing here replaced or recycled");
        write(&s.p("letters/a.txt"), b"letter, edited");
        s.settle().await;
        assert_eq!(cloud.tree()["/letters/a.txt"].as_deref(), Some(&b"letter, edited"[..]), "the rest syncs");
    }
}

/// Renaming and moving files and folders here moves them in OneDrive:
/// nothing is uploaded again, nothing deleted.
#[tokio::test]
async fn everyday_renaming_and_moving_here_moves_in_onedrive_without_uploading_again() {
    let cloud = Fake::new();
    let mut s = Sync::new("everyday-moves", cloud.clone());
    write(&s.p("report.docx"), b"quarterly numbers");
    write(&s.p("projects/alpha/plan.md"), b"alpha plan");
    write(&s.p("projects/alpha/notes/n1.txt"), b"note one");
    write(&s.p("archive/old.txt"), b"old stuff");
    s.settle().await;
    let sent = cloud.uploaded();

    std::fs::rename(s.p("report.docx"), s.p("report 2026.docx")).unwrap();
    std::fs::rename(s.p("projects/alpha"), s.p("projects/alpha-final")).unwrap();
    std::fs::rename(s.p("archive/old.txt"), s.p("projects/old.txt")).unwrap();
    std::fs::rename(s.p("archive"), s.p("projects/alpha-final/archive")).unwrap();
    s.settle().await;

    let t = cloud.tree();
    for p in [
        "/report 2026.docx",
        "/projects/alpha-final/plan.md",
        "/projects/alpha-final/notes/n1.txt",
        "/projects/old.txt",
        "/projects/alpha-final/archive",
    ] {
        assert!(t.contains_key(p), "{p} missing: {:?}", names(&t));
    }
    assert!(!t.contains_key("/report.docx") && !t.contains_key("/projects/alpha") && !t.contains_key("/archive"));
    assert_eq!(cloud.uploaded(), sent, "moves and renames sent no content");
    assert!(s.versions().is_empty(), "nothing was removed here");
}

/// A folder with a large tree inside: more than the threshold the
/// tests give OneDrive, and so is one of its subfolders.
fn large_folder(s: &Sync) {
    for i in 0..4 {
        write(&s.p(&format!("Projects/Bookmarks/b{i}.html")), format!("bookmark {i}").as_bytes());
        write(&s.p(&format!("Projects/Bookmarks/deep/d{i}.html")), format!("deep {i}").as_bytes());
    }
    write(&s.p("Projects/Bookmarks/deep/deeper/x.txt"), b"deepest");
    write(&s.p("Projects/Bookmarks/small/s.txt"), b"small");
}

/// A folder OneDrive won't move in one request (more items below it than
/// its list view threshold) is moved anyway, item by item: everything
/// arrives, nothing is uploaded again, and only the emptied folders are
/// removed from OneDrive.
#[tokio::test]
async fn a_folder_too_large_to_move_at_once_is_moved_in_parts() {
    let cloud = Fake::new();
    let mut s = Sync::new("large-move", cloud.clone());
    large_folder(&s);
    for i in 0..6 {
        write(&s.p(&format!("Projects/Photos/p{i}.jpg")), format!("photo {i}").as_bytes());
    }
    s.settle().await;
    let sent = cloud.uploaded();
    let before = cloud.tree();
    cloud.0.lock().unwrap().threshold = Some(5);

    // Two at once: the moves go to OneDrive in one batch, which refuses both.
    // One goes into a new folder.
    std::fs::create_dir_all(s.p("Archive")).unwrap();
    std::fs::rename(s.p("Projects/Bookmarks"), s.p("Archive/Bookmarks 2025")).unwrap();
    std::fs::rename(s.p("Projects/Photos"), s.p("Projects/Photos 2025")).unwrap();
    let batches = cloud.0.lock().unwrap().batches.len();
    s.settle().await;
    assert!(cloud.0.lock().unwrap().batches.len() > batches);

    let t = cloud.tree();
    for (from, to) in
        [("/Projects/Bookmarks", "/Archive/Bookmarks 2025"), ("/Projects/Photos", "/Projects/Photos 2025")]
    {
        for (path, content) in before.iter().filter(|(p, _)| p.starts_with(from)) {
            let moved = path.replacen(from, to, 1);
            assert_eq!(t.get(&moved), Some(content), "{moved}: {:?}", names(&t));
        }
        assert!(!t.contains_key(from), "{:?}", names(&t));
    }
    assert_eq!(cloud.uploaded(), sent, "moves sent no content");
    assert!(s.versions().is_empty(), "nothing was removed here");
    assert_eq!(s.synced_part(local_tree(&s.dir.0)), s.synced_part(t));
}

/// The same move, with the process killed at every point of it: nothing is
/// lost, and sync converges.
#[tokio::test]
async fn a_crash_while_moving_a_large_folder_in_parts_loses_nothing() {
    for n in 0.. {
        let cloud = Fake::new();
        let mut s = Sync::on_disk(&format!("large-crash{n}"), cloud.clone());
        large_folder(&s);
        s.settle().await;
        let contents: Vec<Vec<u8>> = cloud.tree().into_values().flatten().collect();
        cloud.0.lock().unwrap().threshold = Some(5);
        std::fs::create_dir_all(s.p("Archive")).unwrap();
        std::fs::rename(s.p("Projects/Bookmarks"), s.p("Archive/Bookmarks 2025")).unwrap();

        s.exec.crash_after(Some(n));
        let mut crashed = false;
        for _ in 0..40 {
            let r = s.attempt().await;
            if r.halted.as_deref() == Some(CRASHED) {
                crashed = true;
                break;
            }
            assert!(r.halted.is_none() && r.failed.is_empty(), "crash point {n}: {r:?}");
            if r.is_idle() && r.skipped.is_empty() {
                break;
            }
        }
        if !crashed {
            assert!(n > 5, "only {n} crash points");
            return;
        }
        let mut s = s.restart();
        s.settle().await;
        let here = local_tree(&s.dir.0);
        assert_eq!(s.synced_part(here.clone()), s.synced_part(cloud.tree()), "crash point {n}");
        let here: Vec<Vec<u8>> = here.into_values().flatten().collect();
        for c in &contents {
            assert!(here.contains(c), "crash point {n}: {:?} isn't in the folder", String::from_utf8_lossy(c));
        }
        let problems = s.obs.store().read(|o| o.check_invariants()).unwrap();
        assert!(problems.is_empty(), "crash point {n}: {problems:?}");
    }
}

/// Moving things into a folder just made, here or on the web: the new
/// folder is created on the other side first, and then the moves follow
/// (without waiting for a full pass).
#[tokio::test]
async fn everyday_moving_into_a_new_folder_follows_both_ways() {
    let cloud = Fake::new();
    let mut s = Sync::new("everyday-new-folder", cloud.clone());
    write(&s.p("projects/alpha/plan.md"), b"alpha plan");
    write(&s.p("notes.txt"), b"notes");
    cloud.add("photos", None);
    cloud.add("photos/a.jpg", Some(b"photo a"));
    s.settle().await;
    let sent = cloud.uploaded();

    std::fs::create_dir_all(s.p("archive/2025")).unwrap();
    std::fs::rename(s.p("projects/alpha"), s.p("archive/2025/alpha")).unwrap();
    std::fs::rename(s.p("notes.txt"), s.p("archive/notes.txt")).unwrap();
    cloud.add("albums", None);
    cloud.mv("photos", "albums");
    s.settle().await;

    let t = cloud.tree();
    for p in ["/archive/2025/alpha/plan.md", "/archive/notes.txt", "/albums/photos/a.jpg"] {
        assert!(t.contains_key(p), "{p} missing in the cloud: {:?}", names(&t));
    }
    assert!(local_tree(&s.dir.0).contains_key("/albums/photos/a.jpg"));
    assert!(!t.contains_key("/projects/alpha") && !t.contains_key("/notes.txt") && !t.contains_key("/photos"));
    assert_eq!(cloud.uploaded(), sent, "moves sent no content");
    assert!(s.versions().is_empty(), "nothing was removed here");
}

/// Moves `Projects/Bookmarks` (from [`large_folder`]) to
/// `Projects/Bookmarks 2025` here and runs rounds until OneDrive no longer
/// has the original: the move in parts is done, and the store is as it
/// left it (no listing since).
async fn move_large_folder_in_parts(s: &mut Sync) {
    std::fs::rename(s.p("Projects/Bookmarks"), s.p("Projects/Bookmarks 2025")).unwrap();
    for _ in 0..20 {
        s.round().await;
        if !s.cloud.tree().contains_key("/Projects/Bookmarks") {
            return;
        }
        std::thread::sleep(Duration::from_millis(100));
    }
    panic!("the large folder wasn't moved: {:?}", names(&s.cloud.tree()));
}

/// Whether the store holds deletion evidence for cloud item `id`.
fn noted_deleted(s: &Sync, id: &str) -> bool {
    use oneup_sync::{Seen, Trees};
    let seen = s.obs.store().read(|o| o.remote(&RemoteId(id.into()))).unwrap();
    matches!(seen, Seen::Deleted)
}

/// A move in parts whose answer is lost (Graph sends it again, on an eTag
/// that moved on: 412) is recorded where the item now is, so removing the
/// emptied original records no deletion of it.
#[tokio::test]
async fn a_lost_answer_while_moving_a_large_folder_in_parts_is_recorded_as_moved() {
    let cloud = Fake::new();
    let mut s = Sync::new("large-lost", cloud.clone());
    large_folder(&s);
    s.settle().await;
    let before = cloud.tree();
    let child = resolve(&cloud.0.lock().unwrap(), "Projects/Bookmarks/b0.html");
    {
        let mut st = cloud.0.lock().unwrap();
        st.threshold = Some(5);
        st.lost_move = Some(child.clone());
    }

    move_large_folder_in_parts(&mut s).await;
    assert!(cloud.0.lock().unwrap().lost_move.is_none(), "the answer was lost");
    let made = resolve(&cloud.0.lock().unwrap(), "Projects/Bookmarks 2025");
    let row = s.obs.store().read(|o| o.get_item(&child)).unwrap();
    assert_eq!(row.and_then(|r| r.parent_id), Some(made), "recorded in the new folder");
    assert!(!noted_deleted(&s, &child), "no deletion recorded for an item OneDrive still has");

    s.settle().await;
    let t = cloud.tree();
    for (path, content) in before.iter().filter(|(p, _)| p.starts_with("/Projects/Bookmarks")) {
        let moved = path.replacen("/Projects/Bookmarks", "/Projects/Bookmarks 2025", 1);
        assert_eq!(t.get(&moved), Some(content), "{moved}: {:?}", names(&t));
    }
    assert!(s.versions().is_empty(), "nothing was removed here");
}

/// An item moved elsewhere on the web while its large folder is being
/// moved in parts is still stored below the emptied original when that is
/// removed: no deletion is recorded for it, and it follows its move here.
#[tokio::test]
async fn an_item_moved_away_while_a_large_folder_is_moved_in_parts_is_not_deleted() {
    let cloud = Fake::new();
    let mut s = Sync::new("large-wander", cloud.clone());
    large_folder(&s);
    s.settle().await;
    let child = resolve(&cloud.0.lock().unwrap(), "Projects/Bookmarks/b0.html");
    {
        let mut st = cloud.0.lock().unwrap();
        let projects = resolve(&st, "Projects");
        st.threshold = Some(5);
        st.wander = Some((child.clone(), projects));
    }

    move_large_folder_in_parts(&mut s).await;
    assert!(cloud.0.lock().unwrap().wander.is_none(), "it was moved away");
    assert!(!noted_deleted(&s, &child), "no deletion recorded for an item OneDrive still has");

    s.settle().await;
    let here = local_tree(&s.dir.0);
    assert_eq!(here.get("/Projects/b0.html"), Some(&Some(b"bookmark 0".to_vec())), "{:?}", names(&here));
    assert!(here.contains_key("/Projects/Bookmarks 2025/deep/deeper/x.txt"), "{:?}", names(&here));
    assert!(s.versions().is_empty(), "nothing was removed here");
}

/// Changing only the case of a large folder's name can't be done in parts
/// (the new folder can't sit next to the original under the same name):
/// it is reported as failed, and nothing changes in OneDrive.
#[tokio::test]
async fn a_case_only_rename_of_a_large_folder_is_reported() {
    let cloud = Fake::new();
    let mut s = Sync::new("large-case", cloud.clone());
    large_folder(&s);
    s.settle().await;
    let before = cloud.tree();
    cloud.0.lock().unwrap().threshold = Some(5);

    std::fs::rename(s.p("Projects/Bookmarks"), s.p("Projects/bookmarks")).unwrap();
    let mut failed = Vec::new();
    for _ in 0..20 {
        let r = s.attempt().await;
        assert!(r.halted.is_none() && r.skipped.is_empty(), "{r:?}");
        failed = r.failed;
        if !failed.is_empty() {
            break;
        }
        std::thread::sleep(Duration::from_millis(100));
    }
    assert!(failed.iter().any(|(_, why)| why.contains("case")), "{failed:?}");
    assert_eq!(cloud.tree(), before, "nothing changed in OneDrive");
}

/// Search finds anything in OneDrive by any part of its name, whichever
/// side it came from, and follows renames, moves and deletions on either
/// side as they sync (`oneup_store::search`).
#[tokio::test]
async fn everyday_search_finds_every_name_and_follows_changes() {
    let cloud = Fake::new();
    let mut s = Sync::new("everyday-search", cloud.clone());
    write(&s.p("Taxes/tax return 2024.pdf"), b"tax");
    write(&s.p("Taxes/receipts/fuel.jpg"), b"fuel");
    cloud.add("Photos", None);
    cloud.add("Photos/Duty-free TAX receipt.jpg", Some(b"px"));
    s.settle().await;
    let found = |s: &Sync, q: &str| -> Vec<String> {
        let mut v: Vec<String> =
            s.obs.store().read(|o| o.search_names(q, 50)).unwrap().hits.into_iter().map(|h| h.row.name).collect();
        v.sort();
        v
    };
    assert_eq!(found(&s, "tax"), ["Duty-free TAX receipt.jpg", "Taxes", "tax return 2024.pdf"]);
    assert_eq!(found(&s, "receipt"), ["Duty-free TAX receipt.jpg", "receipts"]);

    std::fs::rename(s.p("Taxes/tax return 2024.pdf"), s.p("Taxes/return 2024.pdf")).unwrap();
    std::fs::rename(s.p("Taxes/receipts"), s.p("receipts 2024")).unwrap();
    cloud.delete("Photos/Duty-free TAX receipt.jpg");
    cloud.add("Photos/tax-free.jpg", Some(b"new"));
    s.settle().await;
    assert_eq!(found(&s, "tax"), ["Taxes", "tax-free.jpg"]);
    assert_eq!(found(&s, "return 20"), ["return 2024.pdf"]);
    assert_eq!(found(&s, "receipt"), ["receipts 2024"]);
    assert_eq!(found(&s, "fuel"), ["fuel.jpg"], "moved along with its folder");
}

/// Changing only the case of a name, on either side, renames it on the
/// other (Windows and OneDrive both ignore case when comparing names).
#[tokio::test]
async fn everyday_case_only_renames_follow_both_ways() {
    let cloud = Fake::new();
    let mut s = Sync::new("everyday-case", cloud.clone());
    write(&s.p("invoice.pdf"), b"invoice");
    write(&s.p("Photos/IMG_1.jpg"), b"pixels");
    cloud.add("readme.TXT", Some(b"read me"));
    s.settle().await;
    let sent = cloud.uploaded();

    std::fs::rename(s.p("invoice.pdf"), s.p("Invoice.PDF")).unwrap();
    std::fs::rename(s.p("Photos"), s.p("photos")).unwrap();
    cloud.rename("readme.TXT", "README.txt");
    s.settle().await;

    let t = cloud.tree();
    assert!(t.contains_key("/Invoice.PDF") && t.contains_key("/photos/IMG_1.jpg"), "{:?}", names(&t));
    let here: Vec<String> =
        std::fs::read_dir(&s.dir.0).unwrap().flatten().map(|e| e.file_name().into_string().unwrap()).collect();
    assert!(here.contains(&"README.txt".to_string()), "{here:?}");
    assert_eq!(cloud.uploaded(), sent, "no content sent for a case change");
}

/// A new, deep folder tree with empty files and names from many scripts
/// arrives whole, parents before children, on both sides.
#[tokio::test]
async fn everyday_deep_trees_empty_files_and_unicode_names_sync_both_ways() {
    let cloud = Fake::new();
    let mut s = Sync::new("everyday-deep", cloud.clone());
    let deep = "a/b/c/d/e/f/g";
    write(&s.p(&format!("{deep}/leaf.txt")), b"deep leaf");
    write(&s.p("empty.txt"), b"");
    write(&s.p("Café/naïve résumé.txt"), b"accents");
    write(&s.p("日本語/ファイル.txt"), b"japanese");
    write(&s.p("emoji 🎉/party 🎈.txt"), b"emoji");
    write(&s.p("spaces and (brackets) [x] & more/file name.txt"), b"punctuation");
    cloud.add("Ελληνικά", None);
    cloud.add("Ελληνικά/αρχείο.txt", Some(b"greek"));
    cloud.add("nothing.txt", Some(b""));
    s.settle().await;

    let (here, there) = (local_tree(&s.dir.0), cloud.tree());
    assert_eq!(names(&here), names(&there));
    assert_eq!(here["/empty.txt"].as_deref(), Some(&b""[..]));
    assert_eq!(here["/nothing.txt"].as_deref(), Some(&b""[..]));
    assert_eq!(there[&format!("/{deep}/leaf.txt")].as_deref(), Some(&b"deep leaf"[..]));
    assert_eq!(here["/Ελληνικά/αρχείο.txt"].as_deref(), Some(&b"greek"[..]));
}

/// Deleting a whole folder tree here (permanently, as `rmdir /s` or
/// Shift+Delete does) deletes it in OneDrive, into its recycle bin.
#[tokio::test]
async fn everyday_deleting_a_folder_tree_here_deletes_it_in_onedrive_recoverably() {
    let cloud = Fake::new();
    let mut s = Sync::new("everyday-rmtree", cloud.clone());
    write(&s.p("old project/src/main.rs"), b"fn main() {}");
    write(&s.p("old project/src/lib/util.rs"), b"pub fn util() {}");
    write(&s.p("old project/README.md"), b"readme");
    write(&s.p("keep.txt"), b"keep");
    s.settle().await;

    std::fs::remove_dir_all(s.p("old project")).unwrap();
    s.settle().await;

    let t = cloud.tree();
    assert_eq!(names(&t), BTreeSet::from(["/keep.txt"]), "only the kept file is left");
    for c in [&b"fn main() {}"[..], b"pub fn util() {}", b"readme"] {
        assert!(cloud.holds(c), "OneDrive's recycle bin keeps {}", String::from_utf8_lossy(c));
    }
}

/// The paths a stop's list shows for deletions, as `oneup_app::sync` keys
/// them: where the item is now on the side it's deleted from.
fn shown_paths(o: &oneup_store::Ops<'_>, ops: &[Op]) -> oneup_store::Result<Vec<String>> {
    ops.iter()
        .map(|op| {
            Ok(match op {
                Op::DeleteRemote { remote, .. } => o.path_of(&remote.id.0)?.unwrap_or_default(),
                Op::RecycleLocal { local, .. } | Op::RemoveCopy { remote: None, local: Some(local), .. } => {
                    o.local_path_of(local.id.0)?.unwrap_or_default()
                }
                // As `oneup_app::sync` shows a duplicate's removal: by its
                // OneDrive path when it has one.
                Op::RemoveCopy { remote: Some(remote), .. } => o.path_of(&remote.id.0)?.unwrap_or_default(),
                _ => String::new(),
            })
        })
        .collect()
}

/// Small limits, so a few deletions trip a safety stop.
fn strict() -> Policy {
    Policy { max_count: 5, min_count: 2, ..Policy::default() }
}

/// Reads the change journal until a full plan has `n` deletions, moves and
/// replacements (its records can land a moment after the change).
fn wait_for_journal(s: &mut Sync, n: usize) {
    for _ in 0..50 {
        s.obs.catch_up().unwrap();
        let found = s
            .obs
            .store()
            .read(|o| {
                let p = Parked::new(o);
                Ok(plan(&p, all_folders(&p)?)?.iter().filter(|op| is_mass_sensitive(op)).count())
            })
            .unwrap();
        if found == n {
            return;
        }
        std::thread::sleep(Duration::from_millis(100));
    }
    panic!("the journal never showed {n} changes");
}

/// Rounds until one changes nothing, asserting no safety stop trips.
async fn without_another_stop(s: &mut Sync) {
    for _ in 0..12 {
        let r = s.attempt().await;
        assert!(s.stops.raised.is_empty(), "another safety stop: {:?}", s.stops.raised);
        assert!(r.failed.is_empty() && r.halted.is_none(), "{r:?}");
        if r.is_idle() {
            return;
        }
    }
    panic!("not settled");
}

/// A deleted folder tree is deleted in OneDrive a level per round (a
/// folder only once it's empty). One safety stop lists all of it, folders
/// included, and one approval lets every level go, however many there are,
/// counted on top of the safety window.
#[tokio::test]
async fn everyday_an_emptied_folder_tree_takes_one_safety_stop() {
    let cloud = Fake::new();
    let mut s = Sync::new("everyday-one-stop", cloud.clone());
    write(&s.p("old/a.txt"), b"a");
    write(&s.p("old/one/b.txt"), b"b");
    write(&s.p("old/one/two/c.txt"), b"c");
    write(&s.p("old/one/two/three/d.txt"), b"d");
    write(&s.p("keep.txt"), b"keep");
    s.settle().await;
    s.policy = strict();
    s.stops = Stops { allowed: true, window: Some(Recent::default()), ..Stops::default() };

    std::fs::remove_dir_all(s.p("old")).unwrap();
    wait_for_journal(&mut s, 4);
    let r = s.attempt().await;
    assert!(r.done.is_empty(), "a tripped round changes nothing");
    let b = &s.stops.raised[0];
    assert_eq!((b.side, b.count, b.later.len()), (Side::Remote, 8, 4), "4 files, then 4 folders");
    s.stops.approve();
    without_another_stop(&mut s).await;
    assert_eq!(names(&cloud.tree()), BTreeSet::from(["/keep.txt"]));
    assert_eq!(s.stops.window.unwrap().remote, 8);
    for c in [&b"a"[..], b"b", b"c", b"d"] {
        assert!(cloud.holds(c), "OneDrive's recycle bin keeps {}", String::from_utf8_lossy(c));
    }
}

/// A stop's list names each item where it was shown. A folder above the
/// items moved in OneDrive between `status` and `approve`: the same items,
/// now elsewhere, aren't what was approved, and need a stop of their own.
#[tokio::test]
async fn everyday_an_approval_doesnt_follow_items_moved_elsewhere() {
    let cloud = Fake::new();
    let mut s = Sync::new("everyday-stop-moved", cloud.clone());
    for f in ["1", "2", "3"] {
        write(&s.p(&format!("old/{f}.txt")), f.as_bytes());
    }
    s.settle().await;
    s.policy = strict();
    s.stops.allowed = true;

    for f in ["1", "2", "3"] {
        std::fs::remove_file(s.p(&format!("old/{f}.txt"))).unwrap();
    }
    wait_for_journal(&mut s, 3);
    s.attempt().await;
    assert_eq!(s.stops.raised[0].count, 3);
    let shown = s.stops.raised[0].fingerprint;

    cloud.rename("old", "critical");
    s.stops.approve();
    let r = s.attempt().await;
    assert!(r.done.is_empty(), "{r:?}");
    assert_eq!(s.stops.raised.len(), 1, "a stop of their own");
    assert_ne!(s.stops.raised[0].fingerprint, shown);
    assert!(cloud.tree().contains_key("/critical/1.txt"));
}

/// A folder deleted in OneDrive that still holds a file here which never
/// syncs (an Office lock file) isn't listed as emptied, and removing it is
/// refused while the file is there: nothing in it is recycled.
#[tokio::test]
async fn everyday_a_folder_holding_a_parked_file_isnt_listed_or_emptied() {
    let cloud = Fake::new();
    let mut s = Sync::new("everyday-stop-parked", cloud.clone());
    for f in ["1", "2", "3"] {
        write(&s.p(&format!("old/{f}.txt")), f.as_bytes());
    }
    write(&s.p("old/~$draft.docx"), b"office lock");
    s.settle().await;
    s.policy = strict();
    s.stops.allowed = true;

    cloud.delete("old");
    s.attempt().await;
    let b = &s.stops.raised[0];
    assert_eq!((b.side, b.count), (Side::Local, 3), "{b:?}");
    assert!(b.later.is_empty(), "the folder isn't emptied: {:?}", b.later);
    s.stops.approve();
    for _ in 0..4 {
        s.attempt().await;
    }
    assert!(s.p("old/~$draft.docx").exists(), "kept where it was");
    assert!(!s.p("old/1.txt").exists());
}

/// A safety stop raised by a full pass (the agent's first round), for
/// deletions the dirty marks don't cover: approving it runs its whole list
/// in the next round, which plans only marked folders. A file rewritten
/// meanwhile doesn't change what was approved.
#[tokio::test]
async fn everyday_approving_a_full_pass_stop_runs_its_whole_list() {
    let cloud = Fake::new();
    let mut s = Sync::new("everyday-full-pass-stop", cloud.clone());
    for d in ["x", "y", "z"] {
        for f in ["1", "2", "3"] {
            write(&s.p(&format!("{d}/{f}.txt")), format!("{d}{f}").as_bytes());
        }
    }
    write(&s.p("data.json"), b"{\"v\":1}");
    s.settle().await;
    s.policy = strict();
    s.stops.allowed = true;

    for d in ["x", "y", "z"] {
        for f in ["1", "2", "3"] {
            std::fs::remove_file(s.p(&format!("{d}/{f}.txt"))).unwrap();
        }
    }
    write(&s.p("data.json"), b"{\"v\":2}");
    // The deletions are read, but their marks are gone (as for evidence
    // noted while the agent was stopped): only a full pass finds them.
    wait_for_journal(&mut s, 10);
    s.obs.store_mut().write(|o| o.dirty_clear(i64::MAX).map(drop)).unwrap();
    s.stops.full_next = true;
    s.attempt().await;
    let b = &s.stops.raised[0];
    assert_eq!(b.count, 10, "9 deletions and the edit");

    // Rewritten while the user reads the list.
    write(&s.p("data.json"), b"{\"v\":3, \"longer\": true}");
    s.stops.approve();
    let r = s.attempt().await;
    assert!(s.stops.raised.is_empty(), "{:?}", s.stops.raised);
    assert_eq!(r.done.iter().filter(|op| matches!(op, Op::DeleteRemote { .. })).count(), 9, "{r:?}");
    without_another_stop(&mut s).await;
    let t = cloud.tree();
    assert_eq!(t.get("/data.json"), Some(&Some(b"{\"v\":3, \"longer\": true}".to_vec())));
    assert!(t.keys().all(|k| !k.ends_with(".txt")), "{:?}", names(&t));
}

/// File Explorer's Delete moves the item into the Recycle Bin: the change
/// journal shows only a move out of the folder, so on its own it's held,
/// never deleted on a guess. Once the agent finds it in the Recycle Bin and
/// records the deletion, it's deleted in OneDrive (into its recycle bin).
#[tokio::test]
async fn everyday_a_folder_sent_to_the_recycle_bin_is_deleted_in_onedrive_once_noted() {
    let cloud = Fake::new();
    let mut s = Sync::new("everyday-recycle", cloud.clone());
    let b = bin("recycle");
    write(&s.p("drafts/one.txt"), b"draft one");
    write(&s.p("drafts/sub/two.txt"), b"draft two");
    write(&s.p("keep.txt"), b"keep");
    s.settle().await;

    std::fs::rename(s.p("drafts"), b.0.join("drafts")).unwrap();
    s.rounds(3).await;
    assert!(cloud.tree().contains_key("/drafts/one.txt"), "held: moved out isn't deleted");

    assert_eq!(s.note_deleted(&["drafts", "one.txt", "sub", "two.txt"]), 4);
    s.settle().await;
    assert_eq!(names(&cloud.tree()), BTreeSet::from(["/keep.txt"]));
    assert!(cloud.holds(b"draft one") && cloud.holds(b"draft two"));
}

/// Restored from the Recycle Bin before its deletion reached OneDrive (a
/// safety stop waiting, changes to OneDrive paused): it simply stays. It
/// came back with its deletion noted, which stopped a test
/// agent ("present and noted deleted"); `settle` checks the invariants.
#[tokio::test]
async fn everyday_restored_before_its_deletion_reached_onedrive_it_stays() {
    let cloud = Fake::new();
    let mut s = Sync::new("everyday-restore-early", cloud.clone());
    let b = bin("restore-early");
    write(&s.p("letter.docx"), b"dear reader");
    s.settle().await;

    std::fs::rename(s.p("letter.docx"), b.0.join("letter.docx")).unwrap();
    s.rounds(2).await;
    assert_eq!(s.note_deleted(&["letter.docx"]), 1);
    // Back before a round carries the deletion out.
    std::fs::rename(b.0.join("letter.docx"), s.p("letter.docx")).unwrap();
    s.settle().await;

    assert_eq!(cloud.tree().get("/letter.docx"), Some(&Some(b"dear reader".to_vec())));
    assert_eq!(std::fs::read(s.p("letter.docx")).unwrap(), b"dear reader");
}

/// Restored after its deletion reached OneDrive: a new file, uploaded again.
#[tokio::test]
async fn everyday_restored_after_its_deletion_reached_onedrive_it_is_uploaded_again() {
    let cloud = Fake::new();
    let mut s = Sync::new("everyday-restore-late", cloud.clone());
    let b = bin("restore-late");
    write(&s.p("photo.jpg"), b"sunset");
    s.settle().await;

    std::fs::rename(s.p("photo.jpg"), b.0.join("photo.jpg")).unwrap();
    s.rounds(2).await;
    s.note_deleted(&["photo.jpg"]);
    s.settle().await;
    assert!(!cloud.tree().contains_key("/photo.jpg"));

    std::fs::rename(b.0.join("photo.jpg"), s.p("photo.jpg")).unwrap();
    s.settle().await;
    assert_eq!(cloud.tree().get("/photo.jpg"), Some(&Some(b"sunset".to_vec())));
}

/// A OneDrive folder deleted here that still holds items the rules leave
/// out (which sync doesn't list) isn't deleted with them: OneDrive refuses
/// a folder that isn't empty, and that's told apart from a changed item,
/// which the agent retries at once (it spun through 20 rounds at a time
/// on a real project folder).
#[tokio::test]
async fn everyday_a_onedrive_folder_holding_excluded_items_is_left_and_reported_not_empty() {
    let cloud = Fake::new();
    let mut s = Sync::new("everyday-notempty", cloud.clone());
    s.rules = user_rules(&["node_modules/"]);
    cloud.add("app", None);
    cloud.add("app/index.js", Some(b"console.log(1)"));
    cloud.add("app/node_modules", None);
    cloud.add("app/node_modules/dep.js", Some(b"dep"));
    s.settle().await;
    assert!(s.p("app/index.js").exists() && !s.p("app/node_modules").exists());

    std::fs::remove_dir_all(s.p("app")).unwrap();
    let reports = s.rounds(4).await;
    let skips: Vec<oneup_exec::Skip> = reports.iter().flat_map(|r| r.skipped.iter().map(|(_, k)| *k)).collect();
    assert!(skips.contains(&oneup_exec::Skip::NotEmpty), "{skips:?}");
    assert!(!skips.contains(&oneup_exec::Skip::Changed), "not taken for a change: {skips:?}");
    let t = cloud.tree();
    assert!(!t.contains_key("/app/index.js"), "what sync knew went: {:?}", names(&t));
    assert!(t.contains_key("/app/node_modules/dep.js"), "the excluded content stays");
}

/// A folder synced before its `node_modules` was excluded, then deleted on
/// both sides: its record goes with the excluded one below it. The planner
/// doesn't see that one, so it used to stay, pointing at a parent that was
/// gone, and the consistency check stopped every round after.
/// `settle` checks the invariants.
#[tokio::test]
async fn everyday_a_folder_deleted_on_both_sides_takes_its_excluded_records_along() {
    let cloud = Fake::new();
    let mut s = Sync::new("everyday-gone-excluded", cloud.clone());
    write(&s.p("app/index.js"), b"console.log(1)");
    write(&s.p("app/node_modules/dep.js"), b"dep");
    write(&s.p("keep.txt"), b"keep");
    s.settle().await;
    assert!(cloud.tree().contains_key("/app/node_modules/dep.js"), "synced while not excluded");

    s.rules = user_rules(&["node_modules/"]);
    s.settle().await;
    std::fs::remove_dir_all(s.p("app")).unwrap();
    cloud.delete("app/node_modules/dep.js");
    cloud.delete("app/node_modules");
    cloud.delete("app/index.js");
    cloud.delete("app");
    s.settle().await;

    assert_eq!(names(&local_tree(&s.dir.0)), BTreeSet::from(["/keep.txt"]));
    assert_eq!(names(&cloud.tree()), BTreeSet::from(["/keep.txt"]));
}

/// A file replaced by a folder of the same name (and the other way round,
/// on the web) is followed on the other side, the old content recoverable.
#[tokio::test]
async fn everyday_a_file_replaced_by_a_folder_of_the_same_name_follows() {
    let cloud = Fake::new();
    let mut s = Sync::new("everyday-kind", cloud.clone());
    write(&s.p("notes"), b"a single file of notes");
    cloud.add("music", None);
    cloud.add("music/song.mp3", Some(b"la la"));
    s.settle().await;

    std::fs::remove_file(s.p("notes")).unwrap();
    write(&s.p("notes/today.txt"), b"split into files");
    cloud.delete("music");
    cloud.add("music", Some(b"now a playlist file"));
    s.settle().await;

    let (here, there) = (local_tree(&s.dir.0), cloud.tree());
    assert_eq!(there["/notes/today.txt"].as_deref(), Some(&b"split into files"[..]));
    assert_eq!(here["/music"].as_deref(), Some(&b"now a playlist file"[..]));
    assert!(cloud.holds(b"a single file of notes"));
    assert!(s.versions().contains(&b"la la".to_vec()));
}

/// A file made and removed between two rounds (a temporary file that
/// doesn't look like one) never reaches OneDrive, and an edit saved many
/// times sends only its last version.
#[tokio::test]
async fn everyday_short_lived_files_never_upload_and_repeated_saves_send_the_last() {
    let cloud = Fake::new();
    let mut s = Sync::new("everyday-transient", cloud.clone());
    write(&s.p("doc.txt"), b"v0");
    s.settle().await;

    write(&s.p("scratch.dat"), b"gone soon");
    std::fs::remove_file(s.p("scratch.dat")).unwrap();
    for i in 1..=10 {
        write(&s.p("doc.txt"), format!("v{i}").as_bytes());
    }
    s.settle().await;

    let t = cloud.tree();
    assert!(!t.contains_key("/scratch.dat"), "{:?}", names(&t));
    assert!(!cloud.holds(b"gone soon"));
    assert_eq!(t["/doc.txt"].as_deref(), Some(&b"v10"[..]));
}

/// Both sides at once: a folder moved on the web while a file in it is
/// edited here; a file renamed here while edited on the web. Every change
/// survives.
#[tokio::test]
async fn everyday_concurrent_moves_and_edits_on_both_sides_all_survive() {
    let cloud = Fake::new();
    let mut s = Sync::new("everyday-concurrent", cloud.clone());
    write(&s.p("shared/budget.xlsx"), b"budget v1");
    write(&s.p("todo.txt"), b"todo v1");
    write(&s.p("archive/keep.txt"), b"k");
    s.settle().await;

    cloud.move_to("shared", "archive");
    write(&s.p("shared/budget.xlsx"), b"budget v2, edited here");
    std::fs::rename(s.p("todo.txt"), s.p("todo-2026.txt")).unwrap();
    cloud.edit("todo.txt", b"todo v2, edited on the web");
    s.settle().await;

    let (here, there) = (local_tree(&s.dir.0), cloud.tree());
    assert_eq!(names(&here), names(&there));
    let all: Vec<&[u8]> = there.values().filter_map(|c| c.as_deref()).collect();
    assert!(all.contains(&&b"budget v2, edited here"[..]), "{:?}", names(&there));
    assert!(all.contains(&&b"todo v2, edited on the web"[..]), "{:?}", names(&there));
    assert!(there.keys().any(|p| p.starts_with("/archive/shared/")), "the web move stands: {:?}", names(&there));
}

/// A folder tree deleted on the web is removed here, every level, its
/// files kept in the version store.
#[tokio::test]
async fn everyday_a_folder_tree_deleted_on_the_web_is_removed_here_recoverably() {
    let cloud = Fake::new();
    cloud.add("trip", None);
    cloud.add("trip/day1", None);
    cloud.add("trip/day1/beach.jpg", Some(b"beach"));
    cloud.add("trip/day2", None);
    cloud.add("trip/day2/museum.jpg", Some(b"museum"));
    cloud.add("stay.txt", Some(b"stay"));
    let mut s = Sync::new("everyday-webrmtree", cloud.clone());
    s.settle().await;
    assert!(s.p("trip/day2/museum.jpg").exists());

    cloud.delete("trip/day1/beach.jpg");
    cloud.delete("trip/day2/museum.jpg");
    cloud.delete("trip/day1");
    cloud.delete("trip/day2");
    cloud.delete("trip");
    s.settle().await;

    assert_eq!(names(&local_tree(&s.dir.0)), BTreeSet::from(["/stay.txt"]));
    let kept = s.versions();
    assert!(kept.contains(&b"beach".to_vec()) && kept.contains(&b"museum".to_vec()), "recoverable here");
}

/// Renamed here while deleted on the web: nothing is lost (the file is
/// either kept here or still somewhere OneDrive can give it back).
#[tokio::test]
async fn everyday_renamed_here_while_deleted_on_the_web_loses_nothing() {
    let cloud = Fake::new();
    let mut s = Sync::new("everyday-rename-delete", cloud.clone());
    write(&s.p("plan.txt"), b"the plan");
    s.settle().await;

    std::fs::rename(s.p("plan.txt"), s.p("plan-final.txt")).unwrap();
    cloud.delete("plan.txt");
    s.settle().await;

    let here = local_tree(&s.dir.0);
    let safe = here.values().any(|c| c.as_deref() == Some(&b"the plan"[..]))
        || s.versions().contains(&b"the plan".to_vec())
        || cloud.holds(b"the plan");
    assert!(safe, "the content is somewhere: here {:?}", names(&here));
    assert_eq!(names(&here), names(&cloud.tree()), "both sides agree");
}

/// Two new files whose names differ only in case, one on each side: both
/// are kept (one under another name), neither replaces the other.
#[tokio::test]
async fn everyday_new_names_differing_only_in_case_keep_both() {
    let cloud = Fake::new();
    cloud.add("Notes.txt", Some(b"from the web"));
    let dir = Tmp(std::env::temp_dir().join(format!("oneup-exec-everyday-caseclash-{}", std::process::id())));
    let _ = std::fs::remove_dir_all(&dir.0);
    write(&dir.0.join("notes.txt"), b"from this pc");
    let mut s = Sync::with_dir(dir, cloud.clone());
    s.settle().await;

    let all: Vec<Vec<u8>> = local_tree(&s.dir.0).into_values().flatten().collect();
    assert!(all.contains(&b"from the web".to_vec()) && all.contains(&b"from this pc".to_vec()), "both kept here");
    let up: Vec<Vec<u8>> = cloud.tree().into_values().flatten().collect();
    assert!(up.contains(&b"from the web".to_vec()) && up.contains(&b"from this pc".to_vec()), "both in OneDrive");
}

/// A burst of new files in one folder (a camera import, an unzip) arrives
/// whole, and a later round has nothing left to do.
#[tokio::test]
async fn everyday_hundreds_of_new_files_at_once_all_arrive() {
    let cloud = Fake::new();
    let mut s = Sync::new("everyday-burst", cloud.clone());
    for i in 0..300 {
        write(&s.p(&format!("import/IMG_{i:04}.jpg")), format!("photo {i}").as_bytes());
    }
    for i in 0..50 {
        cloud.add(&format!("web-{i:02}.txt"), Some(format!("web {i}").as_bytes()));
    }
    s.settle().await;

    let (here, there) = (local_tree(&s.dir.0), cloud.tree());
    assert_eq!(there.keys().filter(|p| p.starts_with("/import/")).count(), 300);
    assert_eq!(here.keys().filter(|p| p.starts_with("/web-")).count(), 50);
    assert_eq!(names(&here), names(&there));
}

/// New files on both sides: what a round transfers.
fn burst(s: &Sync, cloud: &Fake, n: usize) {
    for i in 0..n {
        write(&s.p(&format!("up/{i:03}.txt")), format!("up {i}").as_bytes());
        cloud.add(&format!("down-{i:03}.txt"), Some(format!("down {i}").as_bytes()));
    }
}

/// Independent uploads and downloads run side by side, up to the limit,
/// and end the same as one at a time.
#[tokio::test]
async fn independent_transfers_run_side_by_side() {
    let cloud = Fake::new();
    let mut s = Sync::new("side-by-side", cloud.clone());
    burst(&s, &cloud, 40);
    s.settle().await;
    let most = cloud.most_transfers();
    assert!(most > 1 && most as u64 <= at_once(), "{most} at once");
    let (here, there) = (local_tree(&s.dir.0), cloud.tree());
    assert_eq!(there.keys().filter(|p| p.starts_with("/up/")).count(), 40);
    assert_eq!(here.keys().filter(|p| p.starts_with("/down-")).count(), 40);
    assert_eq!(here, there);
}

/// One at a time when the limit says so.
#[tokio::test]
async fn one_at_a_time_never_overlaps_transfers() {
    let cloud = Fake::new();
    let mut s = Sync::new("one-at-a-time", cloud.clone());
    s.exec.set_parallel(oneup_exec::Parallel::ONE);
    burst(&s, &cloud, 10);
    s.settle().await;
    assert_eq!(cloud.most_transfers(), 1);
    assert_eq!(local_tree(&s.dir.0), cloud.tree());
}

/// Uploads larger than one request (each holds a fragment in memory) run
/// no more at once than their own limit.
#[tokio::test]
async fn large_uploads_keep_to_their_own_limit() {
    let cloud = Fake::new();
    let mut s = Sync::new("large-uploads", cloud.clone());
    s.exec.set_parallel(oneup_exec::Parallel { ops: 8, large_uploads: 2, hashes: 1 });
    let big = vec![7u8; oneup_graph::client::SIMPLE_UPLOAD_MAX + 1];
    for i in 0..6 {
        write(&s.p(&format!("big{i}.bin")), &big);
    }
    s.settle().await;
    // At most the limit; whether two overlap at all depends on the
    // machine's load.
    assert!((1..=2).contains(&cloud.most_transfers()), "{}", cloud.most_transfers());
    assert_eq!(cloud.tree().len(), 6);
}

/// Files here and in OneDrive under the same name and size are hashed to
/// compare them (a first sync over a copy), several at once on blocking
/// threads, never more than the limit: the same content is paired without
/// a transfer, and different content of the same size keeps both.
#[tokio::test]
async fn everyday_same_files_on_both_sides_are_hashed_side_by_side_and_paired() {
    use std::sync::atomic::{AtomicUsize, Ordering::SeqCst};
    let cloud = Fake::new();
    let dir = Tmp(std::env::temp_dir().join(format!("oneup-exec-hash-at-once-{}", std::process::id())));
    let _ = std::fs::remove_dir_all(&dir.0);
    cloud.add("photos", None);
    for i in 0..40 {
        let same = format!("photo {i:03}");
        cloud.add(&format!("photos/{i:03}.jpg"), Some(same.as_bytes()));
        write(&dir.0.join(format!("photos/{i:03}.jpg")), same.as_bytes());
    }
    cloud.add("notes.txt", Some(b"cloud 1"));
    write(&dir.0.join("notes.txt"), b"local 1");
    let mut s = Sync::with_dir(dir, cloud.clone());
    s.exec.set_parallel(oneup_exec::Parallel { ops: 8, large_uploads: 1, hashes: 3 });
    // Each hash held a moment on its thread, as a disk would: how many are
    // under way at once.
    let (now, most) = (Arc::new(AtomicUsize::new(0)), Arc::new(AtomicUsize::new(0)));
    let (n, m) = (Arc::clone(&now), Arc::clone(&most));
    s.exec.on_blocking(Some(Arc::new(move |op: &Op| {
        if matches!(op, Op::Hash(_)) {
            m.fetch_max(n.fetch_add(1, SeqCst) + 1, SeqCst);
            std::thread::sleep(Duration::from_millis(20));
            n.fetch_sub(1, SeqCst);
        }
    })));
    let r = s.round().await;
    let hashed = r.done.iter().filter(|op| matches!(op, Op::Hash(_))).count();
    assert_eq!(hashed, 41, "{r:?}");
    assert_eq!(most.load(SeqCst), 3, "side by side, up to the limit");
    s.exec.on_blocking(None);
    s.settle().await;
    assert_eq!(cloud.0.lock().unwrap().uploaded, 7, "only this PC's notes.txt, as a copy");
    let t = local_tree(&s.dir.0);
    assert_eq!(t.keys().filter(|p| p.starts_with("/photos/")).count(), 40);
    assert_eq!(t.keys().filter(|p| p.starts_with("/notes")).count(), 2, "both kept: {:?}", names(&t));
    assert_eq!(t, cloud.tree());
    assert!(s.versions().is_empty());
}

/// A round takes in at most its budget of new folders' contents; the rest
/// goes in the next round, which the new folder's mark brings.
#[tokio::test]
async fn a_new_folders_contents_past_the_budget_wait_for_the_next_round() {
    let cloud = Fake::new();
    let mut s = Sync::new("follow-budget", cloud.clone());
    s.follow_max = 5;
    for i in 0..12 {
        write(&s.p(&format!("big/{i:02}.txt")), format!("file {i}").as_bytes());
    }
    let r = s.round().await;
    let uploads = r.done.iter().filter(|op| matches!(op, Op::Upload { .. })).count();
    assert_eq!(uploads, 5, "{r:?}");
    s.settle().await;
    assert_eq!(cloud.tree().keys().filter(|p| p.starts_with("/big/")).count(), 12);
    assert_eq!(local_tree(&s.dir.0), cloud.tree());
}

/// A synced file moved into a new folder: the move is planned where the
/// file was (the planner decides a Synced item in its agreed folder), so
/// it goes through a round's own review, never in with the new folder's
/// contents (`oneup_gate::may_follow` refuses a move there). It lands,
/// as a move, and nothing else changes.
#[tokio::test]
async fn a_move_into_a_new_folder_waits_for_the_next_rounds_review() {
    let cloud = Fake::new();
    let mut s = Sync::new("follow-move", cloud.clone());
    write(&s.p("a.txt"), b"synced");
    s.settle().await;
    std::fs::create_dir_all(s.p("new")).unwrap();
    std::fs::rename(s.p("a.txt"), s.p("new/a.txt")).unwrap();
    write(&s.p("new/b.txt"), b"new file");
    let r = s.round().await;
    assert!(r.done.iter().any(|op| matches!(op, Op::MkdirRemote { .. })), "{r:?}");
    assert!(!r.done.iter().any(oneup_gate::is_mass_sensitive), "nothing moved in that round: {r:?}");
    s.settle().await;
    let t = cloud.tree();
    assert_eq!(t["/new/a.txt"].as_deref(), Some(&b"synced"[..]), "{:?}", names(&t));
    assert!(!t.contains_key("/a.txt"));
    assert_eq!(cloud.0.lock().unwrap().uploaded, 6 + 8, "moved, not uploaded again");
    assert_eq!(local_tree(&s.dir.0), t);
}

/// Behind one slow transfer, the rest of the round goes at most the
/// executor's window ahead (256 operations, `exec.rs` `WINDOW`, plus one
/// batch), so what it holds for the report stays bounded.
#[tokio::test]
async fn a_slow_transfer_holds_back_the_rest_of_the_round_by_at_most_a_window() {
    let cloud = Fake::new();
    let mut s = Sync::new("window", cloud.clone());
    let n = 600;
    cloud.add("a-slow.txt", Some(b"slow"));
    for i in 0..n {
        cloud.add(&format!("b-{i:03}.txt"), Some(format!("b {i}").as_bytes()));
    }
    cloud.0.lock().unwrap().stall = Some("a-slow.txt".into());
    let r = s.round().await;
    assert_eq!(r.done.len(), n + 1, "{r:?}");
    let slow = |op: &Op| matches!(op, Op::Download { name, .. } if name == "a-slow.txt");
    let before = r.done.iter().position(slow).unwrap();
    assert!(before < 10, "the slow one is planned near the start: {before}");
    let during = cloud.0.lock().unwrap().finished_during_stall.unwrap();
    assert!(during > 0, "others ran alongside it");
    assert!(during <= before + 256 + oneup_graph::client::BATCH_MAX, "{during} finished behind it");
}

/// Files `big.bin` (the same on both sides, so the first round hashes it
/// to pair the two), `up.txt` to upload and `down.txt` to download, and a
/// hook that holds the hash on its blocking thread until `until` says so
/// (or 20 s pass), then notes in the flag it returns that it let go.
fn a_held_hash(
    tag: &str,
    until: impl Fn(&Fake, &Path) -> bool + Send + std::marker::Sync + 'static,
) -> (Sync, Arc<std::sync::atomic::AtomicBool>) {
    let cloud = Fake::new();
    let s = Sync::on_disk(tag, cloud.clone());
    write(&s.p("big.bin"), b"the same on both sides");
    cloud.add("big.bin", Some(b"the same on both sides"));
    write(&s.p("up.txt"), b"up");
    cloud.add("down.txt", Some(b"down"));
    let released = Arc::new(std::sync::atomic::AtomicBool::new(false));
    let (dir, flag) = (s.dir.0.clone(), Arc::clone(&released));
    s.exec.on_blocking(Some(Arc::new(move |op: &Op| {
        if !matches!(op, Op::Hash(_)) {
            return;
        }
        let give_up = std::time::Instant::now() + Duration::from_secs(20);
        while !until(&cloud, &dir) && std::time::Instant::now() < give_up {
            std::thread::sleep(Duration::from_millis(10));
        }
        flag.store(true, std::sync::atomic::Ordering::SeqCst);
    })));
    (s, released)
}

/// Whether `up.txt` reached OneDrive and `down.txt` this folder.
fn up_and_down(cloud: &Fake, dir: &Path) -> bool {
    cloud.0.lock().unwrap().child("root", "up.txt").is_some() && dir.join("down.txt").exists()
}

/// Reading a file whole (here, hashing it to pair it with OneDrive's copy
/// of the same name) runs on a blocking thread. While it's held there, as a
/// slow disk or a large file would hold it, the round's other operations
/// go on: the upload and the download finish first. When it ran on the
/// round's task, everything under way waited for it.
#[tokio::test]
async fn a_long_read_doesnt_hold_up_the_rest_of_the_round() {
    let (mut s, released) = a_held_hash("long-read", up_and_down);
    let started = std::time::Instant::now();
    let r = s.round().await;
    assert!(r.done.iter().any(|op| matches!(op, Op::Hash(_))), "{r:?}");
    assert!(released.load(std::sync::atomic::Ordering::SeqCst));
    assert!(started.elapsed() < Duration::from_secs(15), "the hash let go once the others were done, not at 20 s");
    assert!(up_and_down(&s.cloud, &s.dir.0));
    s.exec.on_blocking(None);
    s.settle().await;
    assert_eq!(local_tree(&s.dir.0), s.cloud.tree());
}

/// A round that halts (an injected crash, here right after the upload)
/// while an effect runs on a blocking thread ends only once that effect
/// has: nothing the round started changes the folder after it, nor while
/// the next round settles the write-ahead records a crash leaves.
#[tokio::test]
async fn a_halted_round_ends_only_after_the_work_it_handed_to_a_blocking_thread() {
    // Held past the crash: until the upload is in OneDrive, then a while.
    let (mut s, released) = a_held_hash("halt-blocking", |cloud, _| {
        let up = cloud.0.lock().unwrap().child("root", "up.txt").is_some();
        if up {
            std::thread::sleep(Duration::from_millis(300));
        }
        up
    });
    s.exec.crash_after(Some(0));
    let r = s.attempt().await;
    assert_eq!(r.halted.as_deref(), Some(CRASHED), "{r:?}");
    assert!(!r.done.iter().any(|op| matches!(op, Op::Hash(_))), "the crash came first: {r:?}");
    assert!(released.load(std::sync::atomic::Ordering::SeqCst), "the round waited for the hash");
    s.exec.on_blocking(None);
    let mut s = s.restart();
    s.settle().await;
    assert_eq!(local_tree(&s.dir.0), s.cloud.tree());
}

/// A round dropped while it recycles a file on a blocking thread (the
/// caller stopped waiting for it) doesn't let go until the recycling is
/// done, so the next round's settling of write-ahead records never runs
/// beside it. The wait blocks the dropping thread, which here is the only
/// thread of a current-thread runtime (the test's): the recycling runs on
/// the blocking pool and needs nothing from that thread, so it can't
/// deadlock. Afterwards the record the round left is settled, the deletion
/// stays, and the content is still in the version store or OneDrive.
#[tokio::test]
async fn a_round_dropped_while_it_recycles_a_file_waits_for_the_recycling() {
    use std::sync::atomic::{AtomicBool, Ordering::SeqCst};
    let cloud = Fake::new();
    let mut s = Sync::on_disk("drop-recycle", cloud.clone());
    write(&s.p("gone.txt"), b"deleted in OneDrive");
    s.settle().await;
    cloud.delete("gone.txt");
    let (started, finished) = (Arc::new(AtomicBool::new(false)), Arc::new(AtomicBool::new(false)));
    let (began, ended) = (Arc::clone(&started), Arc::clone(&finished));
    s.exec.on_blocking(Some(Arc::new(move |op: &Op| {
        if matches!(op, Op::RecycleLocal { .. }) {
            began.store(true, SeqCst);
            std::thread::sleep(Duration::from_millis(300));
            ended.store(true, SeqCst);
        }
    })));
    tokio::select! {
        r = s.attempt() => panic!("the round ended before it was dropped: {r:?}"),
        () = async {
            while !started.load(SeqCst) {
                tokio::time::sleep(Duration::from_millis(5)).await;
            }
            // `select!` drops the round before it runs this branch's body.
            assert!(!finished.load(SeqCst), "dropped while recycling");
        } => {}
    }
    assert!(finished.load(SeqCst), "dropping the round waited for the recycling");
    s.exec.on_blocking(None);
    let pending = s.obs.store().read(|o| o.meta_under("exec.pending")).unwrap();
    assert_eq!(pending.len(), 1, "the round left its write-ahead record: {pending:?}");
    assert!(!s.p("gone.txt").exists(), "the recycling was done");

    let mut s = s.restart();
    s.obs.catch_up().unwrap();
    let store = s.obs.store_mut();
    let r = s.exec.run(store, Vec::new()).await;
    assert!(r.halted.is_none(), "{r:?}");
    assert_eq!(store.read(|o| o.meta_under("exec.pending")).unwrap(), vec![]);
    let problems = store.read(|o| o.check_invariants()).unwrap();
    assert!(problems.is_empty(), "{problems:?}");
    s.settle().await;
    assert!(!s.p("gone.txt").exists(), "the deletion stays");
    let content = &b"deleted in OneDrive"[..];
    assert!(s.versions().iter().any(|v| v == content) || s.cloud.holds(content), "the content was lost");
    assert_eq!(local_tree(&s.dir.0), s.cloud.tree());
}

/// While another program writes the database (the settings window, a
/// command), the round's own writes wait for it without blocking the task
/// the round runs on, so what shares that task (the transfers under way,
/// the agent's progress reports) goes on meanwhile. SQLite's own wait
/// blocked it until the other write was done.
#[tokio::test]
async fn a_round_waits_for_a_busy_database_without_blocking_its_task() {
    use std::sync::atomic::{AtomicUsize, Ordering::SeqCst};
    let cloud = Fake::new();
    let mut s = Sync::on_disk("busy-db", cloud.clone());
    cloud.add("down.txt", Some(b"down"));
    write(&s.p("up.txt"), b"up");
    let db = s.db.as_ref().unwrap().0.clone();
    let held = Duration::from_millis(500);
    // As the first operation starts, another connection takes the
    // database for a while.
    let taken = std::sync::Once::new();
    let may_run = |_: &Op| {
        taken.call_once(|| {
            let (tx, rx) = std::sync::mpsc::channel();
            let db = db.clone();
            std::thread::spawn(move || {
                let mut other = Store::open(&db, 1024).unwrap();
                other
                    .write(|_| {
                        tx.send(()).unwrap();
                        std::thread::sleep(held);
                        Ok(())
                    })
                    .unwrap();
            });
            rx.recv().unwrap();
        });
        true
    };
    let ticks = AtomicUsize::new(0);
    let started = std::time::Instant::now();
    let r = tokio::select! {
        r = s.attempt_checked(&may_run) => r,
        () = async {
            loop {
                tokio::time::sleep(Duration::from_millis(10)).await;
                ticks.fetch_add(1, SeqCst);
            }
        } => unreachable!(),
    };
    assert!(r.halted.is_none() && r.failed.is_empty(), "{r:?}");
    assert_eq!(r.done.len(), 2, "{r:?}");
    assert!(started.elapsed() >= held, "the round waited for the other write");
    let ticks = ticks.load(SeqCst);
    assert!(ticks >= 10, "the task went on meanwhile: {ticks} ticks");
}

/// A large file is read whole, to look for its content in OneDrive, off
/// the executor's task, so its upload doesn't have to run alone: one whose
/// size reached OneDrive earlier in the round (the first upload, which the
/// limit on large uploads runs first) is still found there, and copied.
#[tokio::test]
async fn a_large_upload_whose_content_reached_onedrive_earlier_in_the_round_is_copied() {
    let cloud = Fake::new();
    let mut s = Sync::new("copy-large", cloud.clone());
    s.exec.set_parallel(oneup_exec::Parallel { ops: 8, large_uploads: 1, hashes: 1 });
    let big = vec![9u8; (64 << 20) + 1];
    write(&s.p("a.bin"), &big);
    write(&s.p("b.bin"), &big);
    s.settle().await;
    let st = cloud.0.lock().unwrap();
    assert_eq!(st.copies, 1, "the second was copied after reading it whole");
    assert_eq!(st.uploaded, big.len() as u64);
}

/// Timing, on a network where every request takes 20 ms: a round of
/// uploads and downloads one at a time, then side by side (`balanced`,
/// `performance`). Run with
/// `cargo test -p oneup-exec --test sync -- --ignored --nocapture timing`.
#[tokio::test]
#[ignore = "timing only"]
async fn timing_one_at_a_time_and_side_by_side() {
    let profile = oneup_exec::Parallel::for_profile;
    for (tag, parallel) in
        [("one", oneup_exec::Parallel::ONE), ("bal", profile("balanced")), ("perf", profile("performance"))]
    {
        let cloud = Fake::new();
        let mut s = Sync::new(&format!("timing-{tag}"), cloud.clone());
        s.exec.set_parallel(parallel);
        burst(&s, &cloud, 100);
        cloud.0.lock().unwrap().latency = Some(Duration::from_millis(20));
        let started = std::time::Instant::now();
        s.settle().await;
        let took = started.elapsed();
        assert_eq!(local_tree(&s.dir.0), cloud.tree());
        eprintln!("{parallel:?}: 100 uploads and 100 downloads in {took:.2?}");
    }
}

/// Another program's file operation (`cmd /c ...`): renames and deletes by
/// the provider's own process aren't what a user does.
fn app_cmd(line: &str) {
    use std::os::windows::process::CommandExt;
    let st = std::process::Command::new("cmd").raw_arg(format!("/C {line} >nul 2>nul")).status().unwrap();
    assert!(st.success(), "{line}");
}

/// With Files On-Demand: renaming and moving online-only files and folders
/// here moves them in OneDrive, and downloads nothing.
#[tokio::test]
async fn everyday_on_demand_renaming_and_moving_online_only_items_downloads_nothing() {
    let cloud = Fake::new();
    cloud.add("big.iso", Some(b"a large disc image"));
    cloud.add("album", None);
    cloud.add("album/track1.flac", Some(b"track one"));
    cloud.add("album/track2.flac", Some(b"track two"));
    cloud.add("music", None);
    let (od, mut s) = on_demand_sync("everyday-od-moves", &cloud);
    s.quiesce().await;
    assert_eq!(placeholder(&s.p("big.iso")).on_disk, 0);
    let sent = cloud.uploaded();

    app_cmd(&format!("ren \"{}\" \"disc.iso\"", s.p("big.iso").display()));
    app_cmd(&format!("move \"{}\" \"{}\"", s.p("album").display(), s.p("music").display()));
    s.quiesce().await;

    let t = cloud.tree();
    assert!(t.contains_key("/disc.iso") && t.contains_key("/music/album/track2.flac"), "{:?}", names(&t));
    assert!(!t.contains_key("/big.iso") && !t.contains_key("/album"));
    assert_eq!(cloud.uploaded(), sent, "nothing sent");
    assert_eq!(placeholder(&s.p("disc.iso")).on_disk, 0, "still online-only");
    assert_eq!(placeholder(&s.p("music/album/track1.flac")).on_disk, 0, "still online-only");
    drop(s);
    drop(od);
}

/// With Files On-Demand: deleting a tree of online-only files here deletes
/// it in OneDrive, every level, into its recycle bin.
#[tokio::test]
async fn everyday_on_demand_deleting_a_tree_here_deletes_every_level_in_onedrive() {
    let cloud = Fake::new();
    cloud.add("old", None);
    cloud.add("old/a", None);
    cloud.add("old/a/one.txt", Some(b"one"));
    cloud.add("old/b", None);
    cloud.add("old/b/two.txt", Some(b"two"));
    cloud.add("keep.txt", Some(b"keep"));
    let (od, mut s) = on_demand_sync("everyday-od-rmtree", &cloud);
    s.quiesce().await;

    app_cmd(&format!("rmdir /s /q \"{}\"", s.p("old").display()));
    s.quiesce().await;

    assert_eq!(names(&cloud.tree()), BTreeSet::from(["/keep.txt"]));
    assert!(cloud.holds(b"one") && cloud.holds(b"two"), "in OneDrive's recycle bin");
    drop(s);
    drop(od);
}

/// A pre-release build (0.1.28) marked excluded folders for File Explorer
/// (placeholders pinned *excluded*; the concept is shelved). Clearing such a
/// mark resets the pin only: everything inside stays, including an
/// online-only file the OneDrive app left behind (a revert has removed
/// one). Sync goes on, never sending the excluded folder.
#[tokio::test]
async fn everyday_on_demand_a_legacy_excluded_folder_mark_is_cleared_keeping_everything() {
    let cloud = Fake::new();
    let (od, mut s) = on_demand_sync("everyday-od-legacy-mark", &cloud);
    s.rules = user_rules(&["node_modules/"]);
    write(&s.p("app/index.js"), b"app code");
    write(&s.p("app/node_modules/pkg/lib.js"), b"library");
    s.quiesce().await;
    let nm_path = s.p("app/node_modules");
    let stub =
        provider::NewPlaceholder { name: "left.txt", identity: b"FOREIGN!", size: 4, modified: 0, folder: false };
    provider::create(&nm_path, &[stub]).unwrap();
    // As 0.1.28 marked it.
    provider::Exclusive::open(&nm_path).unwrap().convert(oneup_effects::EXCLUDED_MARK).unwrap();
    provider::set_pin(&provider::open(&nm_path).unwrap(), provider::Pin::Excluded).unwrap();
    let row = s.obs.store().read(|o| o.local_find("node_modules", 5)).unwrap();
    let nm = row.iter().find(|r| r.name == "node_modules").unwrap().file_id;

    let fs = s.exec.local();
    assert!(fs.unmark_excluded_folder(nm).unwrap(), "cleared");
    assert!(!fs.unmark_excluded_folder(nm).unwrap(), "nothing left to clear");
    let i = placeholder(&nm_path);
    assert!(i.in_sync && i.pin == provider::Pin::Unspecified, "the pin only: {i:?}");
    assert!(nm_path.join("left.txt").exists(), "the online-only leftover stays");
    assert_eq!(std::fs::read(nm_path.join("pkg/lib.js")).unwrap(), b"library");

    write(&s.p("app/node_modules/pkg/new.js"), b"installed later");
    s.quiesce().await;
    let t = cloud.tree();
    assert!(t.contains_key("/app/index.js") && !t.keys().any(|p| p.contains("node_modules")), "{:?}", names(&t));
    drop(s);
    drop(od);
}

/// `n` new files on each side, spread over `depth` nested new folders
/// (`up/d1/d2/…` here, `down/d1/d2/…` in OneDrive; none: all in `up` and
/// `down`).
fn tree(s: &Sync, cloud: &Fake, n: usize, depth: usize) {
    let dirs: Vec<String> = (0..=depth).map(|d| (1..=d).map(|k| format!("/d{k}")).collect()).collect();
    let dirs = if depth == 0 { &dirs[..] } else { &dirs[1..] };
    cloud.add("down", None);
    for d in dirs.iter().filter(|d| !d.is_empty()) {
        cloud.add(&format!("down{d}"), None);
    }
    for i in 0..n {
        let d = &dirs[i % dirs.len()];
        write(&s.p(&format!("up{d}/{i:03}.txt")), format!("up {i}").as_bytes());
        cloud.add(&format!("down{d}/{i:03}.txt"), Some(format!("down {i}").as_bytes()));
    }
}

/// Timing, on a network where every request takes 100 ms: 200 new files
/// each way, in one new folder or spread over 5 nested ones, with what goes
/// into new folders run in the same round or left to the next. Run with
/// `cargo test -p oneup-exec --test sync -- --ignored --nocapture timing_new`.
#[tokio::test]
#[ignore = "timing only"]
async fn timing_new_folder_trees() {
    for follow in [false, true] {
        for depth in [0, 5] {
            let cloud = Fake::new();
            let mut s = Sync::new(&format!("timing-tree-{depth}-{follow}"), cloud.clone());
            s.follow = follow;
            tree(&s, &cloud, 200, depth);
            cloud.0.lock().unwrap().latency = Some(Duration::from_millis(100));
            let started = std::time::Instant::now();
            let rounds = s.settle().await;
            let took = started.elapsed();
            assert_eq!(local_tree(&s.dir.0), cloud.tree());
            eprintln!("follow {follow}, depth {depth}: {rounds} rounds, {took:.2?}");
        }
    }
}

/// A new folder tree, here and in OneDrive, takes as many rounds as the
/// same files in one new folder: what goes into each new folder is planned,
/// reviewed and run in the round that made the folder, not a level a round.
#[tokio::test]
async fn a_new_folder_tree_syncs_in_as_many_rounds_as_one_new_folder() {
    let mut rounds = Vec::new();
    for depth in [0, 5] {
        let cloud = Fake::new();
        let mut s = Sync::new(&format!("tree-rounds-{depth}"), cloud.clone());
        tree(&s, &cloud, 40, depth);
        rounds.push(s.settle().await);
        let (here, there) = (local_tree(&s.dir.0), cloud.tree());
        assert_eq!(here, there);
        let files = |t: &Tree, top: &str| t.keys().filter(|p| p.starts_with(top) && p.ends_with(".txt")).count();
        assert_eq!((files(&there, "/up/"), files(&here, "/down/")), (40, 40));
    }
    assert_eq!(rounds, [2, 2], "one round, and one that finds nothing left");
}

/// What a round plans for a new folder runs in that round, after the
/// folder: the report lists it after the rest, as the progress count does.
#[tokio::test]
async fn a_new_folders_contents_run_after_it_in_the_same_round() {
    let cloud = Fake::new();
    let mut s = Sync::new("tree-order", cloud.clone());
    write(&s.p("a/b/c.txt"), b"c");
    cloud.add("x", None);
    cloud.add("x/y", None);
    cloud.add("x/y/z.txt", Some(b"z"));
    let r = s.round().await;
    let at = |n: &str| r.done.iter().position(|op| op.target().is_some_and(|(_, name)| name == n)).unwrap();
    assert!(at("a") < at("b") && at("b") < at("c.txt"), "{r:?}");
    assert!(at("x") < at("y") && at("y") < at("z.txt"), "{r:?}");
    assert_eq!(r.done.len(), 6, "{r:?}");
    assert_eq!(local_tree(&s.dir.0), cloud.tree());
}

/// A new folder whose creation is refused, because the name was taken in
/// OneDrive meanwhile, takes none of its contents anywhere that round. The
/// next rounds pair the two folders and send the contents into OneDrive's.
#[tokio::test]
async fn the_contents_of_a_new_folder_whose_name_was_taken_meanwhile_wait() {
    let cloud = Fake::new();
    let mut s = Sync::new("tree-taken", cloud.clone());
    write(&s.p("proj/a.txt"), b"a");
    write(&s.p("proj/sub/b.txt"), b"b");
    cloud.0.lock().unwrap().beaten = Some("proj".into());
    let r = s.round().await;
    let taken =
        |(op, k): &(Op, oneup_exec::Skip)| matches!(op, Op::MkdirRemote { .. }) && *k == oneup_exec::Skip::NameTaken;
    assert!(r.skipped.iter().any(taken), "{r:?}");
    assert!(r.done.is_empty(), "{r:?}");
    assert_eq!(names(&cloud.tree()), BTreeSet::from(["/proj"]), "only the folder made meanwhile");
    s.settle().await;
    let t = cloud.tree();
    assert_eq!(names(&t), BTreeSet::from(["/proj", "/proj/a.txt", "/proj/sub", "/proj/sub/b.txt"]));
    assert_eq!(local_tree(&s.dir.0), t);
}

/// The same from OneDrive: a new folder that can't be made here (another
/// program made one of that name meanwhile) brings none of its contents.
#[tokio::test]
async fn the_contents_of_a_new_cloud_folder_not_made_here_wait() {
    let cloud = Fake::new();
    let mut s = Sync::new("tree-taken-here", cloud.clone());
    cloud.add("photos", None);
    cloud.add("photos/2024", None);
    cloud.add("photos/2024/a.jpg", Some(b"jpg"));
    let dir = s.p("photos");
    // Made by another program just before the folder is made here.
    let may_run = |op: &Op| {
        if matches!(op, Op::MkdirLocal { name, .. } if name == "photos") {
            let _ = std::fs::create_dir(&dir);
        }
        true
    };
    let r = s.attempt_checked(&may_run).await;
    assert!(r.halted.is_none() && r.failed.is_empty(), "{r:?}");
    let taken =
        |(op, k): &(Op, oneup_exec::Skip)| matches!(op, Op::MkdirLocal { .. }) && *k == oneup_exec::Skip::NameTaken;
    assert!(r.skipped.iter().any(taken), "{r:?}");
    assert!(r.done.is_empty(), "{r:?}");
    assert!(std::fs::read_dir(&dir).unwrap().next().is_none(), "nothing went into it");
    s.settle().await;
    assert_eq!(std::fs::read(s.p("photos/2024/a.jpg")).unwrap(), b"jpg");
    assert_eq!(local_tree(&s.dir.0), cloud.tree());
}

/// A new folder that fails to be made takes none of its contents along;
/// they go once a later round makes it.
#[tokio::test]
async fn the_contents_of_a_new_folder_that_failed_wait_for_it() {
    let cloud = Fake::new();
    let mut s = Sync::new("tree-failed", cloud.clone());
    write(&s.p("proj/a.txt"), b"a");
    write(&s.p("proj/sub/b.txt"), b"b");
    cloud.0.lock().unwrap().mkdir_fails = Some("sub".into());
    let r = s.attempt().await;
    assert!(r.halted.is_none(), "{r:?}");
    assert!(matches!(&r.failed[..], [(Op::MkdirRemote { name, .. }, _)] if name == "sub"), "{r:?}");
    assert_eq!(names(&cloud.tree()), BTreeSet::from(["/proj", "/proj/a.txt"]), "nothing of sub");
    s.settle().await;
    assert_eq!(local_tree(&s.dir.0), cloud.tree());
}

/// A pause that comes while a round runs stops what goes into its new
/// folders too: not run, and planned again from the folders' marks once
/// sync resumes.
#[tokio::test]
async fn a_pause_mid_round_stops_a_new_folders_contents_too() {
    use oneup_gate::Side;
    let cloud = Fake::new();
    let mut s = Sync::new("tree-pause", cloud.clone());
    write(&s.p("proj/a.txt"), b"a");
    write(&s.p("proj/b.txt"), b"b");
    // Paused once the folder was made.
    let may_run = |op: &Op| Side::changed_by(op) != Some(Side::Remote) || matches!(op, Op::MkdirRemote { .. });
    let r = s.attempt_checked(&may_run).await;
    assert!(r.halted.is_none() && r.failed.is_empty() && r.skipped.is_empty(), "{r:?}");
    assert_eq!(r.not_run.len(), 2, "{r:?}");
    assert!(r.not_run.iter().all(|op| matches!(op, Op::Upload { .. })), "{r:?}");
    assert_eq!(names(&cloud.tree()), BTreeSet::from(["/proj"]));
    assert_eq!(s.settle().await, 2, "resuming sends them in the next round");
    assert_eq!(local_tree(&s.dir.0), cloud.tree());
}

/// The gate reviews what goes into new folders as it reviews the round:
/// with changes to OneDrive paused, two new folders of the same name are
/// paired and what OneDrive's holds comes here, and nothing goes there
/// until sync resumes. Identical files are paired, different ones both
/// kept, a level deeper too.
#[tokio::test]
async fn new_folders_on_both_sides_merge_in_one_round_as_the_gate_allows() {
    use oneup_gate::Side;
    let cloud = Fake::new();
    let mut s = Sync::new("tree-merge", cloud.clone());
    write(&s.p("shared/mine.txt"), b"mine");
    write(&s.p("shared/deep/same.txt"), b"same");
    write(&s.p("shared/deep/diff.txt"), b"this pc");
    cloud.add("shared", None);
    cloud.add("shared/theirs.txt", Some(b"theirs"));
    cloud.add("shared/deep", None);
    cloud.add("shared/deep/same.txt", Some(b"same"));
    cloud.add("shared/deep/diff.txt", Some(b"onedrive's"));
    s.policy.pause_remote = true;
    let r = s.round().await;
    assert_eq!(std::fs::read(s.p("shared/theirs.txt")).unwrap(), b"theirs", "{r:?}");
    assert!(!r.done.iter().any(|op| Side::changed_by(op) == Some(Side::Remote)), "{r:?}");
    assert!(!cloud.tree().contains_key("/shared/mine.txt"));
    s.policy.pause_remote = false;
    s.settle().await;
    let t = cloud.tree();
    assert_eq!(local_tree(&s.dir.0), t);
    assert_eq!(t["/shared/mine.txt"].as_deref(), Some(&b"mine"[..]));
    assert_eq!(t["/shared/deep/same.txt"].as_deref(), Some(&b"same"[..]));
    let kept: BTreeSet<&[u8]> = t.values().flatten().map(Vec::as_slice).collect();
    assert!(kept.contains(&b"this pc"[..]) && kept.contains(&b"onedrive's"[..]), "{:?}", names(&t));
}

/// Files On-Demand: a new tree in OneDrive arrives in one round as
/// placeholders, folders and files, without its content; a new tree here
/// is sent in one round too, and marked in sync.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn on_demand_new_folder_trees_arrive_in_one_round() {
    let cloud = Fake::new();
    let (od, mut s) = on_demand_sync("ondemand-tree", &cloud);
    s.quiesce().await;
    cloud.add("photos", None);
    cloud.add("photos/2024", None);
    cloud.add("photos/2024/trip", None);
    cloud.add("photos/2024/trip/a.jpg", Some(b"a photo"));
    write(&s.p("notes/work/todo.txt"), b"todo");
    let r = s.round().await;
    let made = |f: &dyn Fn(&Op) -> bool| r.done.iter().filter(|op| f(op)).count();
    assert_eq!(made(&|op| matches!(op, Op::MkdirLocal { .. })), 3, "{r:?}");
    assert_eq!(made(&|op| matches!(op, Op::Download { .. })), 1, "{r:?}");
    assert_eq!(made(&|op| matches!(op, Op::MkdirRemote { .. })), 2, "{r:?}");
    assert_eq!(made(&|op| matches!(op, Op::Upload { .. })), 1, "{r:?}");
    for dir in ["photos", "photos/2024", "photos/2024/trip"] {
        assert!(placeholder(&s.p(dir)).in_sync, "{dir}");
    }
    let i = placeholder(&s.p("photos/2024/trip/a.jpg"));
    assert!(i.in_sync && i.on_disk == 0, "arrives without its content: {i:?}");
    assert_eq!(std::fs::metadata(s.p("photos/2024/trip/a.jpg")).unwrap().len(), 7);
    assert_eq!(cloud.tree()["/notes/work/todo.txt"].as_deref(), Some(&b"todo"[..]));
    assert!(placeholder(&s.p("notes/work/todo.txt")).in_sync, "marked in sync once sent");
    s.quiesce().await;
    let problems = s.obs.store().read(|o| o.check_invariants()).unwrap();
    assert!(problems.is_empty(), "{problems:?}");
    drop(s);
    drop(od);
}

// ------------------------------------------------- held items: the user's answers
//
// What `oneup unsynced resolve` does for items sync holds
// (`oneup_exec::resolve`): the choice only changes what the planner reads,
// and what follows runs through the gate and the executor like any change.

use oneup_exec::resolve::{Change, Refused};
use oneup_sync::LocalId;

impl Fake {
    /// Removes `path` and everything in it without delta telling of it:
    /// gone from OneDrive with no evidence the mirror could see (as after a
    /// mirror rebuild that doesn't find it). The content is kept, as
    /// OneDrive's recycle bin would.
    fn lose(&self, path: &str) {
        let mut s = self.0.lock().unwrap();
        let mut gone = vec![resolve(&s, path)];
        let mut i = 0;
        while i < gone.len() {
            let p = gone[i].clone();
            gone.extend(s.items.iter().filter(|(_, it)| it.parent.as_deref() == Some(&p)).map(|(id, _)| id.clone()));
            i += 1;
        }
        for id in gone {
            if let Some(it) = s.items.remove(&id) {
                s.kept.push(it.content);
            }
        }
    }
}

impl Sync {
    /// Forgets the mirror, as a rebuild does: the next round lists OneDrive
    /// again, and what it no longer has reads as missing, not deleted.
    fn rebuild_mirror(&mut self) {
        self.obs.store_mut().write(|o| o.clear_remote()).unwrap();
    }

    /// The Synced record named `name` (unique in these tests).
    fn sid(&self, name: &str) -> oneup_sync::Sid {
        let all = self.obs.store().read(|o| o.synced_all()).unwrap();
        all.into_iter().find(|b| b.name == name).unwrap_or_else(|| panic!("no record {name}")).sid
    }

    /// The local item at `rel`.
    fn local_id(&self, rel: &str) -> LocalId {
        self.obs
            .store()
            .read(|o| {
                let mut cur = o.local_root()?.unwrap().root_file_id;
                for part in rel.split('/') {
                    cur = o.local_named(cur, part)?.into_iter().find(|n| n.name == part).unwrap().file_id;
                }
                Ok(LocalId(cur))
            })
            .unwrap()
    }

    /// Carries out the user's choice: works it out from the store and the
    /// folder as they are now, then writes it (a rename joins the next
    /// round). Returns how many items it concerns.
    fn choose(
        &mut self,
        f: impl FnOnce(&oneup_store::Ops<'_>, &dyn Fn(LocalId) -> bool) -> oneup_exec::resolve::Result<Change>,
    ) -> Result<usize, Refused> {
        let Self { obs, exec, extra, .. } = self;
        let fs = exec.local();
        let present = |id| fs.presence(id) != Some(false);
        let done = obs.store_mut().try_write(|o| -> oneup_exec::resolve::Result<(usize, Option<Op>)> {
            let c = f(o, &present)?;
            let n = c.items();
            Ok((n, c.commit(o)?))
        });
        match done {
            Ok((n, op)) => {
                extra.extend(op);
                Ok(n)
            }
            Err(oneup_exec::resolve::Error::Refused(r)) => Err(r),
            Err(e) => panic!("{e}"),
        }
    }
}

/// Items gone from this PC without a recorded deletion are held. Choosing
/// "delete in OneDrive" for them (a folder with what's in it) deletes them
/// there, into its recycle bin; their folder and what's still here stay.
/// An item still here can't be chosen, nor restored into a folder that's
/// gone too.
#[tokio::test]
async fn held_items_gone_from_this_pc_are_deleted_in_onedrive_when_chosen() {
    use oneup_exec::resolve::{delete_there, restore};
    let cloud = Fake::new();
    let mut s = Sync::new("resolve-delete-there", cloud.clone());
    let b = bin("resolve-delete-there");
    write(&s.p("docs/a.txt"), b"alpha");
    write(&s.p("docs/old/b.txt"), b"bravo");
    write(&s.p("docs/old/deep/c.txt"), b"charlie");
    write(&s.p("docs/keep.txt"), b"keep");
    s.settle().await;

    std::fs::rename(s.p("docs/a.txt"), b.0.join("a.txt")).unwrap();
    std::fs::rename(s.p("docs/old"), b.0.join("old")).unwrap();
    s.rounds(2).await;
    assert!(cloud.tree().contains_key("/docs/old/deep/c.txt"), "held, not deleted on a guess");

    let (keep, c) = (s.sid("keep.txt"), s.sid("c.txt"));
    assert_eq!(s.choose(|o, p| delete_there(o, keep, p)), Err(Refused::Here));
    assert_eq!(s.choose(|o, p| restore(o, c, p)), Err(Refused::FolderGoneHere));
    let (a, old) = (s.sid("a.txt"), s.sid("old"));
    assert_eq!(s.choose(|o, p| delete_there(o, a, p)), Ok(1));
    assert_eq!(s.choose(|o, p| delete_there(o, old, p)), Ok(4));
    s.settle().await;
    assert_eq!(names(&cloud.tree()), BTreeSet::from(["/docs", "/docs/keep.txt"]));
    assert!(cloud.holds(b"alpha") && cloud.holds(b"bravo") && cloud.holds(b"charlie"));
    assert_eq!(s.choose(|o, p| delete_there(o, a, p)), Err(Refused::NotSynced));
}

/// A held item edited in OneDrive since it last synced isn't deleted there
/// when that's chosen: the edit wins over the deletion, as anywhere else,
/// and the file comes back here.
#[tokio::test]
async fn a_chosen_deletion_in_onedrive_gives_way_to_an_edit_made_there() {
    let cloud = Fake::new();
    let mut s = Sync::new("resolve-edit-wins", cloud.clone());
    let b = bin("resolve-edit-wins");
    write(&s.p("a.txt"), b"first");
    s.settle().await;
    std::fs::rename(s.p("a.txt"), b.0.join("a.txt")).unwrap();
    s.rounds(2).await;
    cloud.edit("a.txt", b"edited in OneDrive");

    let a = s.sid("a.txt");
    assert_eq!(s.choose(|o, p| oneup_exec::resolve::delete_there(o, a, p)), Ok(1));
    s.settle().await;
    assert_eq!(std::fs::read(s.p("a.txt")).unwrap(), b"edited in OneDrive");
    assert_eq!(cloud.tree()["/a.txt"].as_deref(), Some(&b"edited in OneDrive"[..]));
}

/// "Restore" brings a held item back from OneDrive, a folder with
/// everything in it, sending nothing.
#[tokio::test]
async fn restoring_held_items_brings_them_back_from_onedrive() {
    let cloud = Fake::new();
    let mut s = Sync::new("resolve-restore", cloud.clone());
    let b = bin("resolve-restore");
    write(&s.p("docs/old/b.txt"), b"bravo");
    write(&s.p("docs/old/deep/c.txt"), b"charlie");
    write(&s.p("docs/keep.txt"), b"keep");
    s.settle().await;
    let (uploaded, before) = (cloud.uploaded(), cloud.tree());
    std::fs::rename(s.p("docs/old"), b.0.join("old")).unwrap();
    s.rounds(2).await;

    let old = s.sid("old");
    assert_eq!(s.choose(|o, p| oneup_exec::resolve::restore(o, old, p)), Ok(4));
    s.settle().await;
    assert_eq!(std::fs::read(s.p("docs/old/b.txt")).unwrap(), b"bravo");
    assert_eq!(std::fs::read(s.p("docs/old/deep/c.txt")).unwrap(), b"charlie");
    assert_eq!((cloud.uploaded(), cloud.tree()), (uploaded, before));
}

/// Items gone from OneDrive without a recorded deletion are held. "Delete
/// here" moves one into the version store, "upload" sends one again, and a
/// file edited here since it last synced is uploaded even though "delete
/// here" was chosen. An item still in OneDrive can't be chosen.
#[tokio::test]
async fn held_items_gone_from_onedrive_are_deleted_here_or_uploaded_when_chosen() {
    use oneup_exec::resolve::{delete_here, upload};
    let cloud = Fake::new();
    let mut s = Sync::new("resolve-gone-there", cloud.clone());
    for (f, c) in [("x.txt", "x-ray"), ("y.txt", "yankee"), ("z.txt", "zulu"), ("keep.txt", "keep")] {
        write(&s.p(f), c.as_bytes());
    }
    s.settle().await;
    for f in ["x.txt", "y.txt", "z.txt"] {
        cloud.lose(f);
    }
    s.rebuild_mirror();
    s.rounds(2).await;
    assert!(s.p("x.txt").exists() && s.p("z.txt").exists(), "held, not deleted on a guess");
    write(&s.p("z.txt"), b"zulu, edited here");

    let keep = s.sid("keep.txt");
    assert_eq!(s.choose(|o, _| delete_here(o, keep)), Err(Refused::There));
    let (x, y, z) = (s.sid("x.txt"), s.sid("y.txt"), s.sid("z.txt"));
    assert_eq!(s.choose(|o, _| delete_here(o, x)), Ok(1));
    assert_eq!(s.choose(|o, _| upload(o, y)), Ok(1));
    assert_eq!(s.choose(|o, _| delete_here(o, z)), Ok(1));
    s.settle().await;
    assert!(!s.p("x.txt").exists());
    assert!(s.versions().contains(&b"x-ray".to_vec()), "kept in the version store");
    let t = cloud.tree();
    assert!(!t.contains_key("/x.txt"));
    assert_eq!(t["/y.txt"].as_deref(), Some(&b"yankee"[..]));
    assert_eq!(t["/z.txt"].as_deref(), Some(&b"zulu, edited here"[..]));
}

/// "Rename" for an item OneDrive won't take as it's named: refused for a
/// name Windows doesn't allow or one taken here or in OneDrive (as Windows
/// compares names); a free name renames it here, by its ID through the
/// gate, and it syncs. One changed after the choice is left as it is.
#[tokio::test]
async fn a_chosen_rename_lets_an_item_with_a_refused_name_sync() {
    use oneup_exec::resolve::rename;
    let cloud = Fake::new();
    let mut s = Sync::new("resolve-rename", cloud.clone());
    write(&s.p("_vti_notes.txt"), b"notes");
    write(&s.p("_vti_draft.txt"), b"draft");
    write(&s.p("taken.txt"), b"taken");
    s.settle().await;
    assert!(!cloud.tree().contains_key("/_vti_notes.txt"), "OneDrive refuses the name");
    cloud.add("there.txt", Some(b"only in OneDrive so far"));
    cloud.pull(s.obs.store_mut());

    let id = s.local_id("_vti_notes.txt");
    assert_eq!(s.choose(|o, _| rename(o, id, "TAKEN.txt")), Err(Refused::NameTaken));
    assert_eq!(s.choose(|o, _| rename(o, id, "there.txt")), Err(Refused::NameTakenThere));
    assert_eq!(s.choose(|o, _| rename(o, id, "a|b.txt")), Err(Refused::BadName));
    assert_eq!(s.choose(|o, _| rename(o, id, "_vti_notes.txt")), Err(Refused::SameName));
    assert_eq!(s.choose(|o, _| rename(o, id, "notes.txt")), Ok(1));
    s.settle().await;
    assert!(!s.p("_vti_notes.txt").exists());
    assert_eq!(cloud.tree()["/notes.txt"].as_deref(), Some(&b"notes"[..]));

    // Changed after it was chosen: the rename is skipped, nothing renamed.
    let id = s.local_id("_vti_draft.txt");
    assert_eq!(s.choose(|o, _| rename(o, id, "draft.txt")), Ok(1));
    write(&s.p("_vti_draft.txt"), b"draft, edited");
    let r = s.round().await;
    let skipped =
        r.skipped.iter().any(|(op, k)| matches!(op, Op::RenameLocal { .. }) && *k == oneup_exec::Skip::Changed);
    assert!(skipped, "{r:?}");
    assert!(s.p("_vti_draft.txt").exists() && !s.p("draft.txt").exists());
}

/// Deletions chosen for many held items are counted by the circuit
/// breakers like any others: above the limit, the round stops and asks.
#[tokio::test]
async fn deletions_chosen_for_many_held_items_still_trip_the_circuit_breaker() {
    let cloud = Fake::new();
    let mut s = Sync::new("resolve-breaker", cloud.clone());
    let b = bin("resolve-breaker");
    for i in 0..6 {
        write(&s.p(&format!("big/f{i}.txt")), format!("file {i}").as_bytes());
    }
    s.settle().await;
    std::fs::rename(s.p("big"), b.0.join("big")).unwrap();
    s.rounds(2).await;

    let big = s.sid("big");
    assert_eq!(s.choose(|o, p| oneup_exec::resolve::delete_there(o, big, p)), Ok(7));
    let dbs = s.exec.local().databases();
    let ops = s.obs.store().read(|o| plan_in(o, &s.rules, false, o.dirty_scope()?.folders, &dbs)).unwrap();
    let policy = Policy { max_count: 3, ..Policy::default() };
    let r = review_in(&policy, &Context::default(), ops);
    assert_eq!(r.breakers.len(), 1, "{:?}", r.breakers);
    assert!(!r.approved.iter().any(|a| matches!(a.op(), Op::DeleteRemote { .. })));
    assert!(cloud.tree().contains_key("/big/f0.txt"));
}

/// Edits, saves by rename, renames here and new versions from OneDrive sync
/// without opening any file by ID: while a file is open that way, a program
/// saving it by renaming another file over it fails. The executor finds
/// files where the Local tree has them.
#[tokio::test]
async fn everyday_syncing_opens_no_file_by_id() {
    let cloud = Fake::new();
    let mut s = Sync::on_disk("everyday-by-path", cloud.clone());
    write(&s.p("notes.txt"), b"v1");
    write(&s.p("docs/report.txt"), b"report v1");
    write(&s.p("docs/plan.txt"), b"plan");
    cloud.add("theirs.txt", Some(b"theirs v1"));
    s.settle().await;
    let before = s.exec.local().opened_by_id();

    write(&s.p("notes.txt"), b"v2, edited in place");
    write(&s.p("docs/report.tmp"), b"report v2, saved by rename");
    std::fs::rename(s.p("docs/report.tmp"), s.p("docs/report.txt")).unwrap();
    std::fs::rename(s.p("docs/plan.txt"), s.p("docs/plan v2.txt")).unwrap();
    cloud.edit("theirs.txt", b"theirs v2");
    s.settle().await;

    let t = cloud.tree();
    assert!(t.contains_key("/docs/plan v2.txt"), "{:?}", names(&t));
    assert_eq!(t["/notes.txt"].as_deref(), Some(&b"v2, edited in place"[..]));
    assert_eq!(t["/docs/report.txt"].as_deref(), Some(&b"report v2, saved by rename"[..]));
    assert_eq!(std::fs::read(s.p("theirs.txt")).unwrap(), b"theirs v2");
    assert_eq!(s.exec.local().opened_by_id(), before, "every file found by path");
}

// ---------------------------------------------------------------- hooks (F7)

/// What a round did, as the agent's event feed has it (`oneup_app::events`):
/// a `synced` event per item uploaded, by its path.
fn hook_events(s: &Sync, r: &Report) -> Vec<oneup_ipc::Event> {
    let paths: Vec<String> = s
        .obs
        .store()
        .read(|o| {
            Ok(r.done
                .iter()
                .filter_map(|op| match op {
                    Op::Upload { local, .. } => o.local_path_of(local.id.0).ok().flatten(),
                    _ => None,
                })
                .collect())
        })
        .unwrap();
    paths
        .into_iter()
        .map(|path| oneup_ipc::Event {
            seq: 0,
            at_ms: 0,
            kind: oneup_ipc::EventKind::Synced,
            path,
            action: "upload".into(),
            detail: String::new(),
        })
        .collect()
}

/// Per item: the runs a hook made, and the events folded into them.
type Fired = Arc<Mutex<BTreeMap<String, (u32, u32)>>>;

/// Starts nothing and reports success at once: for what the queue does,
/// apart from what starting programs costs.
struct NoProgram;

impl oneup_hooks::Launch for NoProgram {
    fn launch(&self, _: &oneup_hooks::LaunchRequest<'_>) -> std::io::Result<oneup_hooks::Finished> {
        Ok(oneup_hooks::Finished { exit_code: Some(0), timed_out: false, output: Vec::new(), truncated: false })
    }
}

/// A hook on every `synced` event, counting its runs by item, and the runs
/// that didn't end well. `launch`: the agent's (`Contained`, a real program
/// each time) or [`NoProgram`].
fn burst_hook(
    root: &Path,
    coalesce: Duration,
    launch: Arc<dyn oneup_hooks::Launch>,
) -> (oneup_hooks::Runner, Fired, Arc<Mutex<u32>>) {
    let cmd = PathBuf::from(std::env::var_os("SystemRoot").unwrap()).join("System32").join("cmd.exe");
    let spec = oneup_hooks::HookSpec {
        coalesce,
        per_minute: oneup_hooks::MAX_PER_MINUTE,
        ..oneup_hooks::HookSpec::new(
            "burst",
            &[oneup_ipc::EventKind::Synced],
            &cmd.to_string_lossy(),
            &["/c", "exit", "0"],
        )
    };
    let fired: Fired = Arc::default();
    let bad = Arc::new(Mutex::new(0u32));
    let (f, b) = (Arc::clone(&fired), Arc::clone(&bad));
    let notes = move |n: oneup_hooks::Note| match n {
        oneup_hooks::Note::Ran(r) => {
            let ok = matches!(&r.result, Ok(x) if x.exit_code == Some(0));
            if !ok {
                *b.lock().unwrap() += 1;
            }
            let mut f = f.lock().unwrap();
            let e = f.entry(r.event.path.to_lowercase()).or_default();
            e.0 += 1;
            e.1 += r.merged;
        }
        other => panic!("{other:?}"),
    };
    // No synced folders, known complete (the temp dirs here aren't synced),
    // so the hook is allowed; a safe working folder for the program.
    let places = oneup_hooks::program::Places::new(
        Vec::new(),
        true,
        PathBuf::from(std::env::var_os("SystemRoot").unwrap()).join("System32"),
        None,
    );
    let settings =
        oneup_hooks::Settings { hooks: vec![spec], paused: false, root: root.to_path_buf(), places, source: None };
    (oneup_hooks::Runner::start(settings, launch, Box::new(notes)), fired, bad)
}

/// A hook fires once per item however often the item changed before its
/// run (here: every file uploaded, then half of them again a round later,
/// within the hook's coalescing time), with the latest event, and the rounds
/// never wait for it.
#[tokio::test]
async fn everyday_a_hook_fires_once_per_coalesced_change() {
    let cloud = Fake::new();
    let mut s = Sync::new("hooks-coalesce", cloud.clone());
    // Editing half the files at once replaces that many in OneDrive, which
    // the mass stop would hold; this test is about the hook.
    s.policy = Policy { max_count: 100_000, min_count: 100_000, ..Policy::default() };
    // What it runs doesn't matter here (the benchmark below starts real
    // programs); below-normal programs on a busy PC would only slow it.
    let (runner, fired, bad) = burst_hook(&s.dir.0, Duration::from_secs(8), Arc::new(NoProgram));
    let offer = |s: &Sync, r: &Report| runner.offer(&hook_events(s, r));
    let n = 300;
    for i in 0..n {
        write(&s.p(&format!("d{}/f{i}.txt", i % 10)), b"one");
    }
    s.settle_with(&offer).await;
    for i in (0..n).step_by(2) {
        write(&s.p(&format!("d{}/f{i}.txt", i % 10)), b"two, longer");
    }
    s.settle_with(&offer).await;
    assert!(runner.wait_idle(Duration::from_secs(120)), "{:?}", runner.backlog());
    let fired = fired.lock().unwrap();
    assert_eq!(fired.len(), n, "every file");
    assert!(fired.values().all(|(runs, _)| *runs == 1), "once each");
    assert_eq!(fired.values().filter(|(_, merged)| *merged > 0).count(), n / 2, "the second upload folded in");
    assert_eq!(*bad.lock().unwrap(), 0);
}

/// The F7 exit benchmark: a 10,000-file burst (in ten waves of 1,000, so
/// hooks run while later waves sync) with and without a hook that starts
/// a program for every file. Sync time is measured over the rounds only,
/// three runs each, alternating; the hook must slow sync by no more than
/// 5% (medians). The last hooked run waits for every run and checks the
/// hook fired once per file. Hooks run at below-normal priority, so on a
/// busy PC they take far longer than the sync: that's the point. Run it
/// alone, in release:
/// `cargo test -p oneup-exec --release --test sync bench_hooks -- --ignored --nocapture`.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
#[ignore = "benchmark: run explicitly"]
async fn bench_hooks_under_a_10000_file_burst() {
    const FILES: usize = 10_000;
    const WAVES: usize = 10;
    type Hook = (oneup_hooks::Runner, Fired, Arc<Mutex<u32>>);
    async fn burst(tag: &str, with_hook: bool) -> (Duration, Option<Hook>) {
        let cloud = Fake::new();
        let mut s = Sync::new(tag, cloud.clone());
        let hook = with_hook
            .then(|| burst_hook(&s.dir.0, Duration::from_millis(500), Arc::new(oneup_hooks::Contained::new(None))));
        let mut synced = Duration::ZERO;
        for w in 0..WAVES {
            for i in 0..FILES / WAVES {
                write(&s.p(&format!("w{w}/d{}/f{i}.txt", i % 20)), format!("wave {w} file {i}").as_bytes());
            }
            let t = std::time::Instant::now();
            match &hook {
                Some((runner, ..)) => s.settle_with(&|s: &Sync, r: &Report| runner.offer(&hook_events(s, r))).await,
                None => s.settle().await,
            };
            synced += t.elapsed();
        }
        (synced, hook)
    }
    let median = |mut v: Vec<Duration>| {
        v.sort();
        v[v.len() / 2]
    };
    let (mut without, mut with) = (Vec::new(), Vec::new());
    for rep in 0..3 {
        let (t, _) = burst(&format!("bench-plain-{rep}"), false).await;
        eprintln!("run {rep}: without hooks: sync {:.2} s", t.as_secs_f64());
        without.push(t);
        let (t, hook) = burst(&format!("bench-hooks-{rep}"), true).await;
        let (runner, fired, bad) = hook.unwrap();
        let ran = fired.lock().unwrap().values().map(|(runs, _)| *runs).sum::<u32>();
        eprintln!("run {rep}: with hooks:    sync {:.2} s; {ran} hook runs done meanwhile", t.as_secs_f64());
        with.push(t);
        if rep == 2 {
            let waited = std::time::Instant::now();
            assert!(runner.wait_idle(Duration::from_secs(4 * 3600)), "{:?}", runner.backlog());
            let fired = fired.lock().unwrap();
            eprintln!(
                "all {} hook runs done {:.0} s after the sync",
                fired.values().map(|(runs, _)| *runs).sum::<u32>(),
                waited.elapsed().as_secs_f64()
            );
            assert_eq!(fired.len(), FILES, "every file");
            assert!(fired.values().all(|(runs, _)| *runs == 1), "once each");
            assert_eq!(*bad.lock().unwrap(), 0);
        }
        // Dropped: no more runs start.
        drop(runner);
    }
    let (a, b) = (median(without), median(with));
    let slower = b.as_secs_f64() / a.as_secs_f64() - 1.0;
    eprintln!("median sync: without {:.2} s, with {:.2} s: {:+.1}%", a.as_secs_f64(), b.as_secs_f64(), slower * 100.0);
    assert!(slower <= 0.05, "hooks slowed sync by {:.1}%", slower * 100.0);
}

// ---------------------------------------------------------------- recovery

/// Now, as the Synced history's clock tells it (Unix ms).
fn now_ms() -> i64 {
    let d = std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).unwrap();
    i64::try_from(d.as_millis()).unwrap()
}

/// A moment between what came before and what comes after: the history
/// times changes to the millisecond.
fn moment() -> i64 {
    std::thread::sleep(Duration::from_millis(20));
    let t = now_ms();
    std::thread::sleep(Duration::from_millis(20));
    t
}

/// What's below `folder` in a tree, named from there.
fn inside(t: &Tree, folder: &str) -> Tree {
    let at = format!("{folder}/");
    t.iter().filter_map(|(p, c)| p.strip_prefix(&at).map(|rest| (rest.to_string(), c.clone()))).collect()
}

impl Sync {
    /// Starts rewinding `folder` (`/docs`; `""` for everything) to moment
    /// `at`, as `oneup rewind --start` does.
    fn start_rewind(&mut self, folder: &str, at: i64) {
        let parts: Vec<&str> = folder.split('/').filter(|p| !p.is_empty()).collect();
        let sid = self
            .obs
            .store()
            .read(|o| oneup_sync::rewind::folder_at(o, o, &parts, at))
            .unwrap()
            .expect("the folder was there then");
        self.stops.allowed = true;
        // An earlier rewind's approval is spent (the agent drops it once its
        // changes ran).
        self.stops.approved.clear();
        self.stops.rewind = Some(Rewinding { folder: sid, at, ..Rewinding::default() });
    }

    /// Rewinds `folder` to `at` the way the agent does: the first rounds
    /// only show its stop and change nothing; once approved, the rounds
    /// carry it out and bring this PC along. `meanwhile` runs between the
    /// preview and the answer. Returns how many steps were left alone
    /// because their items changed after the preview.
    async fn rewind_to(&mut self, folder: &str, at: i64, meanwhile: impl FnOnce(&Sync)) -> usize {
        self.start_rewind(folder, at);
        let before = (local_tree(&self.dir.0), self.cloud.tree());
        for _ in 0..2 {
            let r = self.round().await;
            assert!(r.done.is_empty() && r.skipped.is_empty(), "nothing runs before the approval: {r:?}");
            assert!(self.stops.raised.iter().any(|b| b.kind == BreakerKind::Rewind), "a rewind is a safety stop");
            assert_eq!((local_tree(&self.dir.0), self.cloud.tree()), before, "nothing changed before the approval");
        }
        meanwhile(self);
        self.stops.approve();
        self.round().await;
        // Changed since it was shown (`meanwhile`): the approval isn't
        // taken, and the list shown again is approved in its turn.
        if self.stops.rewind.as_ref().is_some_and(|r| r.job.is_none()) {
            self.stops.approve();
        }
        self.settle().await;
        let r = self.stops.rewind.take().expect("a rewind");
        assert!(r.done, "the rewind finished");
        r.left_alone
    }
}

#[tokio::test]
async fn everyday_rewinding_a_folder_puts_back_what_it_held_and_a_rewind_can_be_rewound() {
    let cloud = Fake::new();
    let mut s = Sync::new("everyday-rewind", cloud.clone());
    write(&s.p("docs/a.txt"), b"a, first");
    write(&s.p("docs/b.txt"), b"b");
    write(&s.p("docs/sub/c.txt"), b"c");
    write(&s.p("other.txt"), b"other");
    s.settle().await;
    let then = inside(&local_tree(&s.dir.0), "/docs");
    let at = moment();

    // Since then: an edit, a deletion and a rename here, a new file in
    // OneDrive, and an edit outside the folder.
    write(&s.p("docs/a.txt"), b"a, second");
    std::fs::remove_file(s.p("docs/b.txt")).unwrap();
    std::fs::rename(s.p("docs/sub"), s.p("docs/moved")).unwrap();
    s.settle().await;
    cloud.add("docs/new.txt", Some(b"new in OneDrive"));
    cloud.edit("other.txt", b"other, second");
    s.settle().await;
    let before = local_tree(&s.dir.0);
    let before_at = moment();

    assert_eq!(s.rewind_to("/docs", at, |_| {}).await, 0);
    let now = local_tree(&s.dir.0);
    assert_eq!(inside(&now, "/docs"), then, "the folder is as it was");
    assert_eq!(now["/other.txt"], Some(b"other, second".to_vec()), "outside the folder, nothing changed");
    assert_eq!(s.synced_part(now.clone()), s.synced_part(cloud.tree()));
    // Nothing is destroyed: the newer content is in OneDrive's version
    // history, the file created since in its recycle bin and here in the
    // version store.
    assert!(cloud.holds(b"a, second") && cloud.holds(b"new in OneDrive"));
    assert!(s.versions().contains(&b"new in OneDrive".to_vec()));

    // So a rewind can itself be rewound.
    assert_eq!(s.rewind_to("/docs", before_at, |_| {}).await, 0);
    assert_eq!(local_tree(&s.dir.0), before, "rewinding the rewind puts back what was there");
    assert_eq!(s.synced_part(local_tree(&s.dir.0)), s.synced_part(cloud.tree()));
}

#[tokio::test]
async fn everyday_a_change_before_the_approval_shows_the_list_again() {
    // A rewind waits for its approval while the rest syncs. a.txt is saved
    // again after the list was shown: it syncs, the list changes, and the
    // approval given for the old one isn't taken. Nothing runs until the
    // user approves the list as it is now (which puts a.txt back too).
    let cloud = Fake::new();
    let mut s = Sync::new("everyday-rewind-changed", cloud.clone());
    write(&s.p("docs/a.txt"), b"a, first");
    write(&s.p("docs/b.txt"), b"b, first");
    s.settle().await;
    let at = moment();
    write(&s.p("docs/a.txt"), b"a, second");
    write(&s.p("docs/b.txt"), b"b, second");
    s.settle().await;
    s.start_rewind("/docs", at);
    s.round().await;
    std::thread::sleep(Duration::from_millis(20));
    write(&s.p("docs/a.txt"), b"a, third");
    s.stops.approve();
    s.settle().await;
    let r = s.stops.rewind.as_ref().unwrap();
    assert_eq!((r.refused, r.job.is_some()), (1, false), "the approval of the list before isn't taken");
    assert_eq!(cloud.tree()["/docs/a.txt"], Some(b"a, third".to_vec()), "the edit synced meanwhile");
    assert_eq!(cloud.tree()["/docs/b.txt"], Some(b"b, second".to_vec()), "nothing of the rewind ran");
    assert!(s.stops.raised.iter().any(|b| b.kind == BreakerKind::Rewind), "its new list is shown");
    // The list as it is now, approved.
    s.stops.approve();
    s.settle().await;
    let now = local_tree(&s.dir.0);
    assert_eq!(now["/docs/a.txt"], Some(b"a, first".to_vec()));
    assert_eq!(now["/docs/b.txt"], Some(b"b, first".to_vec()));
    assert!(s.versions().contains(&b"a, third".to_vec()) || cloud.holds(b"a, third"), "the newer edit is kept");
}

#[tokio::test]
async fn everyday_the_rest_syncs_while_a_rewind_waits() {
    let cloud = Fake::new();
    let mut s = Sync::new("everyday-rewind-waits", cloud.clone());
    write(&s.p("docs/a.txt"), b"a, first");
    write(&s.p("other.txt"), b"other");
    s.settle().await;
    let at = moment();
    write(&s.p("docs/a.txt"), b"a, second");
    s.settle().await;
    s.start_rewind("/docs", at);
    s.round().await;
    // Edits outside the folder, here and in OneDrive, while it waits.
    write(&s.p("other.txt"), b"other, edited");
    cloud.add("new.txt", Some(b"new"));
    s.settle().await;
    assert!(s.stops.raised.iter().any(|b| b.kind == BreakerKind::Rewind), "still waiting");
    assert_eq!(cloud.tree()["/other.txt"], Some(b"other, edited".to_vec()), "sent");
    assert_eq!(std::fs::read(s.p("new.txt")).unwrap(), b"new", "brought here");
    assert_eq!(std::fs::read(s.p("docs/a.txt")).unwrap(), b"a, second", "the rewind waits");
}

#[tokio::test]
async fn everyday_a_rewind_sets_aside_edits_not_synced_yet() {
    // As when ransomware encrypts files where they are: the edits haven't
    // reached OneDrive (the content guard holds them); the rewind moves
    // them into the version store and OneDrive's copies come back.
    let cloud = Fake::new();
    let mut s = Sync::new("everyday-rewind-aside", cloud.clone());
    for f in ["x", "y", "z"] {
        write(&s.p(&format!("docs/{f}.txt")), format!("{f}, as written").as_bytes());
    }
    s.settle().await;
    let then = local_tree(&s.dir.0);
    let at = moment();
    for f in ["x", "y", "z"] {
        write(&s.p(&format!("docs/{f}.txt")), format!("{f} \u{1}\u{2}scrambled, and long enough to judge").as_bytes());
    }
    // Not synced: the content guard holds them (with everything else, as
    // any stop but a rewind's does) until the rewind is approved, which
    // takes the items over.
    s.policy.max_suspicious = 1;
    assert_eq!(s.rewind_to("/docs", at, |_| {}).await, 0);
    assert_eq!(local_tree(&s.dir.0), then, "OneDrive's copies came back");
    for f in ["x", "y", "z"] {
        let scrambled = format!("{f} \u{1}\u{2}scrambled, and long enough to judge").into_bytes();
        assert!(!cloud.holds(&scrambled), "the scrambled {f} never reached OneDrive");
        assert!(s.versions().contains(&scrambled), "the scrambled {f} is kept in the version store");
    }
}

#[tokio::test]
async fn everyday_a_rewind_ends_with_what_onedrive_no_longer_has() {
    let cloud = Fake::new();
    let mut s = Sync::new("everyday-rewind-emptied", cloud.clone());
    write(&s.p("docs/a.txt"), b"a, first");
    write(&s.p("docs/b.txt"), b"b");
    s.settle().await;
    let at = moment();
    write(&s.p("docs/a.txt"), b"a, second");
    std::fs::remove_file(s.p("docs/b.txt")).unwrap();
    s.settle().await;
    // OneDrive's recycle bin was emptied since: b.txt can't come back.
    cloud.empty_bin();
    s.start_rewind("/docs", at);
    let r = s.round().await;
    assert!(r.done.is_empty());
    s.stops.approve();
    let mut failed = 0;
    for _ in 0..12 {
        let r = s.attempt().await;
        assert!(r.halted.is_none(), "{r:?}");
        failed += r.failed.len();
        if s.stops.rewind.as_ref().is_some_and(|r| r.done) && r.is_idle() && r.skipped.is_empty() {
            break;
        }
    }
    assert!(s.stops.rewind.as_ref().is_some_and(|r| r.done), "the rewind ends");
    assert!(failed > 0, "and says what it couldn't do");
    assert_eq!(std::fs::read(s.p("docs/a.txt")).unwrap(), b"a, first", "the rest of it ran");
    assert!(!s.p("docs/b.txt").exists());
}

#[tokio::test]
async fn everyday_restoring_a_kept_copy_brings_it_back_on_both_sides() {
    let cloud = Fake::new();
    let mut s = Sync::new("everyday-restore-kept", cloud.clone());
    write(&s.p("docs/a.txt"), b"keep me");
    s.settle().await;
    // Deleted in OneDrive: this PC's copy goes into the version store.
    cloud.delete("docs/a.txt");
    s.settle().await;
    assert!(!s.p("docs/a.txt").exists());
    let slot = s.exec.local().versions().unwrap().into_iter().find(|v| v.name == "a.txt").expect("kept");
    // A planned step, approved by the user's answer: unanswered, it stops.
    s.extra = vec![oneup_exec::recover::kept_op(&slot.id, &s.exec.local().kept_target(&slot.id, None).unwrap())];
    s.unanswered = true;
    s.stops.allowed = true;
    let r = s.attempt().await;
    assert!(r.done.is_empty() && !s.p("docs/a.txt").exists(), "nothing ran");
    assert!(s.stops.raised.iter().any(|b| b.kind == BreakerKind::Rewind), "a stop asks");
    s.stops.raised.clear();
    s.stops.allowed = false;
    s.extra = vec![oneup_exec::recover::kept_op(&slot.id, &s.exec.local().kept_target(&slot.id, None).unwrap())];
    s.settle().await;
    assert_eq!(std::fs::read(s.p("docs/a.txt")).unwrap(), b"keep me");
    assert_eq!(cloud.tree()["/docs/a.txt"], Some(b"keep me".to_vec()), "uploaded again");
    assert!(s.versions().is_empty());
}

#[tokio::test]
async fn everyday_restoring_a_onedrive_version_and_a_deleted_item() {
    let cloud = Fake::new();
    let mut s = Sync::new("everyday-restore-cloud", cloud.clone());
    write(&s.p("docs/a.txt"), b"version one");
    write(&s.p("docs/b.txt"), b"bee");
    s.settle().await;
    write(&s.p("docs/a.txt"), b"version two");
    s.settle().await;

    // A version from the file's history: downloaded and sent as a new one.
    let id = RemoteId(resolve(&cloud.0.lock().unwrap(), "docs/a.txt"));
    let versions = cloud.versions(&id).await.unwrap();
    let old = versions.iter().skip(1).find(|v| v.size == Some(11)).unwrap();
    let revert = s.obs.store().read(|o| oneup_exec::recover::version_op(o, &id, &old.id)).unwrap().unwrap();
    // Unanswered, it stops, and OneDrive keeps what it has.
    s.extra = vec![revert.clone()];
    s.unanswered = true;
    s.stops.allowed = true;
    s.attempt().await;
    assert!(s.stops.raised.iter().any(|b| b.kind == BreakerKind::Rewind), "a stop asks");
    assert_eq!(cloud.tree()["/docs/a.txt"], Some(b"version two".to_vec()));
    s.stops.raised.clear();
    s.stops.allowed = false;
    s.extra = vec![revert];
    s.settle().await;
    assert_eq!(std::fs::read(s.p("docs/a.txt")).unwrap(), b"version one");
    assert!(cloud.holds(b"version two"), "the version it replaced stays in the history");

    // An item deleted here comes back from OneDrive's recycle bin by the ID
    // the history recorded, beside a new file that took its name meanwhile.
    std::fs::remove_file(s.p("docs/b.txt")).unwrap();
    s.settle().await;
    write(&s.p("docs/b.txt"), b"another bee");
    s.settle().await;
    let gone = s.obs.store().read(|o| o.history_removed(10, None)).unwrap();
    let b = gone.iter().find(|r| r.was.name == "b.txt").expect("recorded");
    let parent = s.obs.store().read(|o| o.synced_get(b.was.parent.unwrap())).unwrap().unwrap().remote.unwrap();
    let id = b.was.remote.clone().unwrap();
    let op = s
        .obs
        .store()
        .read(|o| oneup_exec::recover::deleted_op(o, b.sid, &id, &parent, "b.txt", oneup_sync::NodeKind::File))
        .unwrap();
    assert!(
        matches!(op.rewind_step(), Some(oneup_sync::rewind::Step::Restore { name, .. }) if name == "b (restored 1).txt")
    );
    s.extra = vec![op];
    s.settle().await;
    assert_eq!(std::fs::read(s.p("docs/b (restored 1).txt")).unwrap(), b"bee");
    assert_eq!(std::fs::read(s.p("docs/b.txt")).unwrap(), b"another bee");
    let listed = s.obs.store().read(|o| o.history_removed(10, None)).unwrap();
    assert!(listed.iter().all(|r| r.was.remote.as_ref() != Some(&id)), "back in OneDrive: not listed as deleted");
}

#[tokio::test]
async fn everyday_a_restore_asked_for_leaves_a_file_changed_since_alone() {
    let cloud = Fake::new();
    let mut s = Sync::new("everyday-restore-changed", cloud.clone());
    write(&s.p("docs/a.txt"), b"version one");
    s.settle().await;
    write(&s.p("docs/a.txt"), b"version two");
    s.settle().await;
    let id = RemoteId(resolve(&cloud.0.lock().unwrap(), "docs/a.txt"));
    let old = cloud.versions(&id).await.unwrap().into_iter().skip(1).find(|v| v.size == Some(11)).unwrap();
    // Asked for, then the file changes in OneDrive before the agent gets to it.
    let revert = s.obs.store().read(|o| oneup_exec::recover::version_op(o, &id, &old.id)).unwrap().unwrap();
    cloud.edit("docs/a.txt", b"version three");
    s.extra = vec![revert.clone()];
    let r = s.attempt().await;
    assert!(!r.done.contains(&revert), "not run over a newer version");
    s.settle().await;
    assert_eq!(cloud.tree()["/docs/a.txt"], Some(b"version three".to_vec()));
    assert_eq!(std::fs::read(s.p("docs/a.txt")).unwrap(), b"version three");
    // A kept copy whose name was taken meanwhile comes back beside it.
    write(&s.p("docs/k.txt"), b"kept");
    s.settle().await;
    cloud.delete("docs/k.txt");
    s.settle().await;
    let slot = s.exec.local().versions().unwrap().into_iter().find(|v| v.name == "k.txt").expect("kept");
    write(&s.p("docs/k.txt"), b"a new k");
    s.extra = vec![oneup_exec::recover::kept_op(&slot.id, &s.exec.local().kept_target(&slot.id, None).unwrap())];
    s.settle().await;
    assert_eq!(std::fs::read(s.p("docs/k.txt")).unwrap(), b"a new k");
    assert_eq!(std::fs::read(s.p("docs/k (restored 1).txt")).unwrap(), b"kept");
    assert_eq!(s.synced_part(local_tree(&s.dir.0)), s.synced_part(cloud.tree()));
}

#[tokio::test]
async fn everyday_a_restore_that_comes_back_elsewhere_is_moved_where_asked() {
    let cloud = Fake::new();
    let mut s = Sync::new("everyday-restore-asked-odd", cloud.clone());
    write(&s.p("docs/b.txt"), b"bee");
    s.settle().await;
    std::fs::remove_file(s.p("docs/b.txt")).unwrap();
    s.settle().await;
    let gone = s.obs.store().read(|o| o.history_removed(10, None)).unwrap();
    let b = gone.iter().find(|r| r.was.name == "b.txt").expect("recorded");
    let parent = s.obs.store().read(|o| o.synced_get(b.was.parent.unwrap())).unwrap().unwrap().remote.unwrap();
    let id = b.was.remote.clone().unwrap();
    let op = s
        .obs
        .store()
        .read(|o| oneup_exec::recover::deleted_op(o, b.sid, &id, &parent, "b.txt", oneup_sync::NodeKind::File))
        .unwrap();
    // OneDrive brings it back under another name than asked.
    cloud.0.lock().unwrap().restore_odd = Some(("b 1.txt".into(), false));
    s.extra = vec![op];
    s.settle().await;
    let t = local_tree(&s.dir.0);
    assert_eq!(t.get("/docs/b.txt"), Some(&Some(b"bee".to_vec())), "where asked: {t:?}");
    assert!(!t.contains_key("/docs/b 1.txt"));
    assert_eq!(s.synced_part(t), s.synced_part(cloud.tree()));
    assert!(s.extra.is_empty(), "nothing left to do");
}

#[tokio::test]
async fn everyday_answering_a_conflict_keeps_one_copy_and_the_other_recoverably() {
    use oneup_exec::recover::{Keep, conflict_ops, conflicts_in};
    for keep in [Keep::Original, Keep::Copy] {
        let cloud = Fake::new();
        let mut s = Sync::new(&format!("everyday-conflict-{keep:?}"), cloud.clone());
        write(&s.p("docs/r.txt"), b"start");
        s.settle().await;
        write(&s.p("docs/r.txt"), b"mine, here");
        cloud.edit("docs/r.txt", b"theirs, in OneDrive");
        s.settle().await;
        let docs_id = resolve(&cloud.0.lock().unwrap(), "docs");
        let docs = s.obs.store().read(|o| Ok(o.synced_by_remote(&docs_id)?.unwrap().sid)).unwrap();
        let found = s.obs.store().read(|o| conflicts_in(o, docs)).unwrap();
        assert_eq!(found.len(), 1, "one conflict");
        let c = &found[0];
        assert_eq!((c.original.name.as_str(), c.copy.name.as_str()), ("r.txt", "r (conflict 1).txt"));
        s.extra = s.obs.store().read(|o| conflict_ops(o, c, keep)).unwrap().unwrap();
        s.settle().await;
        let now = local_tree(&s.dir.0);
        let (kept, other): (&[u8], &[u8]) = match keep {
            Keep::Original => (b"theirs, in OneDrive", b"mine, here"),
            Keep::Copy => (b"mine, here", b"theirs, in OneDrive"),
        };
        assert_eq!(now.get("/docs/r.txt"), Some(&Some(kept.to_vec())), "{keep:?}: {now:?}");
        assert!(!now.contains_key("/docs/r (conflict 1).txt"));
        assert_eq!(s.synced_part(now), s.synced_part(cloud.tree()));
        assert!(s.versions().contains(&other.to_vec()), "the other copy is in the version store");
        assert!(cloud.holds(other), "and in OneDrive's recycle bin");
        assert!(s.obs.store().read(|o| conflicts_in(o, docs)).unwrap().is_empty(), "answered");
    }
}

#[tokio::test]
async fn everyday_a_large_rewind_takes_one_approval() {
    // More than the breakers allow: approving the rewind approves this PC
    // following it, so no second stop asks again.
    let cloud = Fake::new();
    let mut s = Sync::new("everyday-rewind-large", cloud.clone());
    for i in 0..8 {
        write(&s.p(&format!("docs/f{i}.txt")), format!("{i}, first").as_bytes());
    }
    s.settle().await;
    let then = local_tree(&s.dir.0);
    let at = moment();
    for i in 0..8 {
        cloud.edit(&format!("docs/f{i}.txt"), format!("{i}, second").as_bytes());
    }
    s.stops.window = Some(Recent::default());
    s.settle().await;
    s.policy = strict();
    assert_eq!(s.rewind_to("/docs", at, |_| {}).await, 0);
    assert_eq!(local_tree(&s.dir.0), then);
    // Afterwards, changes the rewind didn't make are judged as ever.
    s.stops.allowed = true;
    for i in 0..8 {
        cloud.delete(&format!("docs/f{i}.txt"));
    }
    s.round().await;
    assert!(s.stops.raised.iter().any(|b| b.kind == BreakerKind::Mass), "a mass deletion still stops");
}

/// A rewind killed at any point of its rounds loses nothing: after a
/// restart it goes on (its approval is kept, as the agent keeps it in the
/// database), every content is still somewhere, and the folder ends as it
/// was at the moment chosen.
#[tokio::test]
async fn a_crash_at_any_point_of_a_rewind_loses_nothing() {
    const CONTENTS: [&[u8]; 12] = [
        b"a, first",
        b"a, second",
        b"b",
        b"new",
        b"x, first",
        b"x \x01\x02scrambled, long enough to judge",
        b"y, first",
        b"y \x01\x02scrambled, long enough to judge",
        b"c, first",
        b"c, second!",
        b"d, first",
        b"d, second!",
    ];
    for n in 0.. {
        let cloud = Fake::new();
        let mut s = Sync::on_disk(&format!("crash-rewind{n}"), cloud.clone());
        write(&s.p("docs/a.txt"), b"a, first");
        write(&s.p("docs/b.txt"), b"b");
        write(&s.p("docs/x.txt"), b"x, first");
        write(&s.p("docs/y.txt"), b"y, first");
        // Two-part steps: c.txt is restored, then its content put back; d.txt
        // gets its content back, then its name.
        write(&s.p("docs/c.txt"), b"c, first");
        write(&s.p("docs/d.txt"), b"d, first");
        s.settle().await;
        let then = local_tree(&s.dir.0);
        let at = moment();
        write(&s.p("docs/a.txt"), b"a, second");
        std::fs::remove_file(s.p("docs/b.txt")).unwrap();
        cloud.edit("docs/c.txt", b"c, second!");
        cloud.edit("docs/d.txt", b"d, second!");
        cloud.rename("docs/d.txt", "d2.txt");
        s.settle().await;
        cloud.delete("docs/c.txt");
        cloud.add("docs/new.txt", Some(b"new"));
        s.settle().await;
        // Not synced (the content guard holds them): set aside by the rewind.
        write(&s.p("docs/x.txt"), b"x \x01\x02scrambled, long enough to judge");
        write(&s.p("docs/y.txt"), b"y \x01\x02scrambled, long enough to judge");
        s.policy.max_suspicious = 1;
        s.start_rewind("/docs", at);
        assert!(s.round().await.done.is_empty());
        s.stops.approve();

        s.exec.crash_after(Some(n));
        let mut crashed = false;
        for _ in 0..40 {
            let r = s.attempt().await;
            if r.halted.as_deref() == Some(CRASHED) {
                crashed = true;
                break;
            }
            assert!(r.halted.is_none() && r.failed.is_empty(), "crash point {n}: {r:?}");
            if r.is_idle() && r.skipped.is_empty() && s.stops.rewind.as_ref().is_some_and(|r| r.done) {
                break;
            }
        }
        if !crashed {
            assert!(n > 5, "only {n} crash points");
            return;
        }
        let mut s = s.restart();
        s.obs.catch_up().unwrap();
        let store = s.obs.store_mut();
        let r = s.exec.run(store, Vec::new()).await;
        assert!(r.halted.is_none(), "crash point {n}: {r:?}");
        assert_eq!(store.read(|o| o.meta_under("exec.pending")).unwrap(), vec![], "crash point {n}");
        let problems = store.read(|o| o.check_invariants()).unwrap();
        assert!(problems.is_empty(), "crash point {n}: {problems:?}");
        s.settle().await;

        let t = local_tree(&s.dir.0);
        let here: Vec<&[u8]> = t.values().filter_map(|c| c.as_deref()).collect();
        let versions = s.versions();
        for c in CONTENTS {
            let kept = here.contains(&c) || versions.iter().any(|v| v == c) || s.cloud.holds(c);
            assert!(kept, "crash point {n}: {:?} was lost", String::from_utf8_lossy(c));
        }
        assert_eq!(t, then, "crash point {n}: the rewind went on after the restart");
    }
}

#[tokio::test]
async fn everyday_a_rewind_finds_its_version_among_many_of_the_same_size() {
    // OneDrive's version list has no hashes: versions of the right size are
    // downloaded and checked one by one, newest first, all of them.
    let cloud = Fake::new();
    let mut s = Sync::new("everyday-rewind-versions", cloud.clone());
    write(&s.p("docs/a.txt"), b"a, version 00");
    s.settle().await;
    let at = moment();
    for i in 1..=12 {
        cloud.edit("docs/a.txt", format!("a, version {i:02}").as_bytes());
        s.settle().await;
    }
    assert_eq!(s.rewind_to("/docs", at, |_| {}).await, 0);
    assert_eq!(std::fs::read(s.p("docs/a.txt")).unwrap(), b"a, version 00");
}

#[tokio::test]
async fn everyday_an_edit_after_a_rewind_ran_is_judged_on_its_own() {
    // The rewind put 8 files back in OneDrive; before this PC follows,
    // someone edits them all again there. Bringing that edit here isn't what
    // the rewind's approval covered: the breakers count it as ever.
    let cloud = Fake::new();
    let mut s = Sync::new("everyday-rewind-race", cloud.clone());
    for i in 0..8 {
        write(&s.p(&format!("docs/f{i}.txt")), format!("{i}, first").as_bytes());
    }
    s.settle().await;
    let at = moment();
    for i in 0..8 {
        cloud.edit(&format!("docs/f{i}.txt"), format!("{i}, second").as_bytes());
    }
    s.settle().await;
    s.policy = strict();
    s.start_rewind("/docs", at);
    assert!(s.round().await.done.is_empty());
    s.stops.approve();
    // The rewind's steps run in OneDrive.
    let r = s.round().await;
    assert_eq!(r.done.iter().filter(|o| matches!(o, Op::Rewind { .. })).count(), 8, "{r:?}");
    for i in 0..8 {
        cloud.edit(&format!("docs/f{i}.txt"), format!("{i}, third").as_bytes());
    }
    let r = s.round().await;
    assert!(r.done.is_empty(), "nothing runs: {r:?}");
    assert!(s.stops.raised.iter().any(|b| b.kind == BreakerKind::Mass), "a stop of its own");
}

#[tokio::test]
async fn everyday_what_a_restore_brings_back_is_checked_not_assumed() {
    // OneDrive brings a deleted file back under another name than asked:
    // the rewind moves it to its name then, through the gate.
    let cloud = Fake::new();
    let mut s = Sync::new("everyday-restore-odd", cloud.clone());
    write(&s.p("docs/a.txt"), b"a");
    write(&s.p("docs/b.txt"), b"b");
    s.settle().await;
    let at = moment();
    std::fs::remove_file(s.p("docs/b.txt")).unwrap();
    s.settle().await;
    cloud.0.lock().unwrap().restore_odd = Some(("b 1.txt".into(), false));
    assert_eq!(s.rewind_to("/docs", at, |_| {}).await, 0);
    let t = local_tree(&s.dir.0);
    assert_eq!(t.get("/docs/b.txt"), Some(&Some(b"b".to_vec())), "{t:?}");
    assert!(!t.contains_key("/docs/b 1.txt"));

    // As another item: said at once, and it comes here as a new file.
    std::fs::remove_file(s.p("docs/b.txt")).unwrap();
    s.settle().await;
    cloud.0.lock().unwrap().restore_odd = Some(("b.txt".into(), true));
    s.start_rewind("/docs", at);
    assert!(s.round().await.done.is_empty());
    s.stops.approve();
    let r = s.attempt().await;
    assert!(r.failed.iter().any(|(_, why)| why.contains("as another item")), "said at once: {r:?}");
    for _ in 0..10 {
        let r = s.attempt().await;
        if r.is_idle() && r.skipped.is_empty() && s.stops.rewind.as_ref().is_some_and(|r| r.done) {
            break;
        }
    }
    assert_eq!(std::fs::read(s.p("docs/b.txt")).unwrap(), b"b", "nothing lost");
}

#[tokio::test]
async fn a_folder_moved_after_the_rewind_was_approved_isnt_removed() {
    // new/ was made since the moment; the rewind removes it. Someone moves
    // it out of the folder after the approval, before the step runs: it's
    // no longer what was approved, and stays where they put it.
    let cloud = Fake::new();
    let mut s = Sync::new("rewind-folder-moved", cloud.clone());
    write(&s.p("docs/a.txt"), b"a");
    s.settle().await;
    let at = moment();
    std::fs::create_dir_all(s.p("docs/new")).unwrap();
    s.settle().await;
    let id = resolve(&cloud.0.lock().unwrap(), "docs/new");
    let mover = id.clone();
    cloud.0.lock().unwrap().on_item = Some((
        id,
        Box::new(move |st: &mut State| {
            let etag = st.bump();
            let it = st.items.get_mut(&mover).unwrap();
            it.parent = Some("root".into());
            it.etag = etag;
        }),
    ));
    s.rewind_to("/docs", at, |_| {}).await;
    s.settle().await;
    let t = cloud.tree();
    assert!(t.contains_key("/new"), "moved, not removed: {t:?}");
    assert!(cloud.0.lock().unwrap().on_item.is_none(), "the step looked at it");
}

#[tokio::test]
async fn a_file_whose_folder_moved_after_the_approval_isnt_removed() {
    // n.txt was made since the moment in sub/, which was there then; the
    // rewind removes it. After the approval, before the step runs, someone
    // moves sub/ out of docs/: n.txt keeps its eTag, but it's no longer
    // where it was shown, so it stays (and later rounds don't take it up
    // from there either).
    let cloud = Fake::new();
    let mut s = Sync::new("rewind-ancestor-moved", cloud.clone());
    write(&s.p("docs/sub/a.txt"), b"a");
    s.settle().await;
    let at = moment();
    write(&s.p("docs/sub/n.txt"), b"new since");
    s.settle().await;
    let id = resolve(&cloud.0.lock().unwrap(), "docs/sub/n.txt");
    let sub = resolve(&cloud.0.lock().unwrap(), "docs/sub");
    cloud.0.lock().unwrap().on_item = Some((
        id,
        Box::new(move |st: &mut State| {
            let etag = st.bump();
            let it = st.items.get_mut(&sub).unwrap();
            it.parent = Some("root".into());
            it.etag = etag;
        }),
    ));
    s.rewind_to("/docs", at, |_| {}).await;
    s.settle().await;
    assert!(cloud.0.lock().unwrap().on_item.is_none(), "the step looked at it");
    let t = cloud.tree();
    assert_eq!(t.get("/sub/n.txt"), Some(&Some(b"new since".to_vec())), "not removed from where it went: {t:?}");
}

#[tokio::test]
async fn a_copy_moved_here_after_the_approval_isnt_set_aside() {
    // x.txt was edited here and not synced (the content guard holds it);
    // the rewind sets it aside. After the approval, before the step runs,
    // it's renamed: same ID, size and last-write time, but not where it was
    // shown, so it stays.
    let cloud = Fake::new();
    let mut s = Sync::new("rewind-aside-moved", cloud.clone());
    write(&s.p("docs/x.txt"), b"x, as written");
    write(&s.p("docs/y.txt"), b"y, as written");
    s.settle().await;
    let at = moment();
    write(&s.p("docs/x.txt"), b"x \x01\x02scrambled, long enough to judge");
    write(&s.p("docs/y.txt"), b"y \x01\x02scrambled, long enough to judge");
    s.policy.max_suspicious = 1;
    s.start_rewind("/docs", at);
    s.round().await;
    s.stops.approve();
    let (from, to) = (s.p("docs/x.txt"), s.p("docs/x2.txt"));
    cloud.0.lock().unwrap().meanwhile = Some(Box::new(move || std::fs::rename(&from, &to).unwrap()));
    s.attempt().await;
    assert_eq!(
        std::fs::read(s.p("docs/x2.txt")).unwrap(),
        b"x \x01\x02scrambled, long enough to judge",
        "renamed after the approval: left where it went"
    );
    assert!(!s.versions().contains(&b"x \x01\x02scrambled, long enough to judge".to_vec()), "not set aside");
    assert!(s.versions().contains(&b"y \x01\x02scrambled, long enough to judge".to_vec()), "the other one is");
}

#[tokio::test]
async fn a_restore_into_a_name_taken_meanwhile_replaces_nothing() {
    // b.txt is asked back from the recycle bin under its name, free then.
    // Before the step runs, a new b.txt is made in OneDrive: the restore is
    // refused (as OneDrive refuses a taken name), and the new one stays.
    let cloud = Fake::new();
    let mut s = Sync::new("restore-name-taken", cloud.clone());
    write(&s.p("docs/b.txt"), b"bee");
    s.settle().await;
    std::fs::remove_file(s.p("docs/b.txt")).unwrap();
    s.settle().await;
    let gone = s.obs.store().read(|o| o.history_removed(10, None)).unwrap();
    let b = gone.iter().find(|r| r.was.name == "b.txt").expect("recorded");
    let parent = s.obs.store().read(|o| o.synced_get(b.was.parent.unwrap())).unwrap().unwrap().remote.unwrap();
    let id = b.was.remote.clone().unwrap();
    let op = s
        .obs
        .store()
        .read(|o| oneup_exec::recover::deleted_op(o, b.sid, &id, &parent, "b.txt", oneup_sync::NodeKind::File))
        .unwrap();
    assert!(matches!(op.rewind_step(), Some(oneup_sync::rewind::Step::Restore { name, .. }) if name == "b.txt"));
    let c = cloud.clone();
    cloud.0.lock().unwrap().meanwhile = Some(Box::new(move || {
        c.add("docs/b.txt", Some(b"another bee"));
    }));
    s.extra = vec![op.clone()];
    let r = s.attempt().await;
    assert!(r.failed.iter().any(|(o, _)| *o == op) || r.skipped.iter().any(|(o, _)| *o == op), "{r:?}");
    s.settle().await;
    assert_eq!(cloud.tree()["/docs/b.txt"], Some(b"another bee".to_vec()), "nothing replaced");
    assert!(cloud.0.lock().unwrap().bin.contains_key(&id.0), "still in the recycle bin");
}

#[tokio::test]
async fn an_item_someone_else_restored_and_edited_isnt_put_back_over() {
    // a.txt was "first" then, "second" when it was deleted. The rewind
    // restores it and puts "first" back. After the approval, someone else
    // restores it from the recycle bin and saves "theirs": the rewind
    // leaves their version current.
    let cloud = Fake::new();
    let mut s = Sync::new("rewind-restored-meanwhile", cloud.clone());
    write(&s.p("docs/a.txt"), b"first");
    s.settle().await;
    let at = moment();
    write(&s.p("docs/a.txt"), b"second");
    s.settle().await;
    let id = resolve(&cloud.0.lock().unwrap(), "docs/a.txt");
    std::fs::remove_file(s.p("docs/a.txt")).unwrap();
    s.settle().await;
    let theirs = id.clone();
    cloud.0.lock().unwrap().on_item = Some((
        id,
        Box::new(move |st: &mut State| {
            let etag = st.bump();
            let docs = resolve(st, "docs");
            let mut it = st.bin.remove(&theirs).unwrap();
            it.parent = Some(docs);
            it.content = b"theirs".to_vec();
            it.etag = etag;
            st.items.insert(theirs, it);
        }),
    ));
    s.rewind_to("/docs", at, |_| {}).await;
    s.settle().await;
    assert!(cloud.0.lock().unwrap().on_item.is_none(), "the step looked at it");
    assert_eq!(cloud.tree()["/docs/a.txt"], Some(b"theirs".to_vec()), "their version stays current");
    assert_eq!(std::fs::read(s.p("docs/a.txt")).unwrap(), b"theirs");
}

#[tokio::test]
async fn everyday_a_rewind_keeps_what_was_there_then_but_synced_only_since() {
    // old.txt was on this PC at the moment, left out by a rule; the rule
    // went since, and it synced. Its record is newer than the moment, its
    // content isn't: the rewind leaves it (and says so), and removes only
    // what's new.
    let cloud = Fake::new();
    let mut s = Sync::new("everyday-rewind-existed", cloud.clone());
    write(&s.p("docs/a.txt"), b"a");
    write(&s.p("docs/old.txt"), b"left out by a rule");
    s.rules = user_rules(&["/docs/old.txt"]);
    s.settle().await;
    assert!(!cloud.tree().contains_key("/docs/old.txt"));
    let at = moment();
    s.rules = RuleSet::default();
    s.settle().await;
    write(&s.p("docs/new.txt"), b"new since");
    s.settle().await;
    let skipped = s.obs.store().read(|o| {
        let sid = oneup_sync::rewind::folder_at(o, o, &["docs"], at)?.unwrap();
        oneup_sync::rewind::plan(o, o, sid, at, 1000).map_err(|e| oneup_store::Error::Corrupt(format!("{e:?}")))
    });
    let r = skipped.unwrap();
    assert!(
        r.skipped.iter().any(|(_, p, why)| p == "/docs/old.txt" && *why == oneup_sync::rewind::Skip::ExistedBefore)
    );
    assert_eq!(s.rewind_to("/docs", at, |_| {}).await, 0);
    let t = local_tree(&s.dir.0);
    assert_eq!(t.get("/docs/old.txt"), Some(&Some(b"left out by a rule".to_vec())), "it was there then");
    assert!(!t.contains_key("/docs/new.txt"), "removed: created since");
}

// ---------------------------------------------------------------- duplicates
// Copies the user chose to remove (`oneup_exec::dupes`): the gate holds them
// until the user approves exactly that batch, whatever the circuit breakers'
// limits, and they run as ordinary operations: OneDrive's copies into its
// recycle bin, the copies here into the version store.

use oneup_exec::dupes::{Chosen, CopyId, Kept, Plan};

/// Content large enough for the cloud files by size index (1 MiB or more).
fn big(fill: u8) -> Vec<u8> {
    let mut v = vec![fill; (1 << 20) + 7];
    v[0] = b'#';
    v
}

impl Sync {
    /// The copies of `content`, as the Duplicates view lists them: its path
    /// (in OneDrive, or here for a file only here) and how it's chosen.
    fn copies_of(&self, content: &[u8]) -> Vec<(String, Chosen)> {
        let (size, hash) = (content.len() as i64, qxh(content));
        self.obs
            .store()
            .read(|o| {
                let mut v = Vec::new();
                for r in o.dup_cloud_copies(size, &hash, 100)? {
                    let path = o.path_of(&r.id)?.unwrap();
                    let copy = CopyId::Cloud { id: RemoteId(r.id), etag: r.etag.unwrap() };
                    v.push((path, Chosen { copy, size, hash }));
                }
                for l in o.dup_local_copies(size, &hash, 100)? {
                    let path = o.local_path_of(l.file_id)?.unwrap();
                    let copy = CopyId::Local { id: LocalId(l.file_id), size: l.size, mtime: l.mtime };
                    v.push((path, Chosen { copy, size, hash }));
                }
                Ok(v)
            })
            .unwrap()
    }

    /// The copies of `content` at `paths`.
    fn chosen(&self, content: &[u8], paths: &[&str]) -> Vec<Chosen> {
        let all = self.copies_of(content);
        paths
            .iter()
            .map(|p| all.iter().find(|(q, _)| q == p).unwrap_or_else(|| panic!("no copy {p}")).1.clone())
            .collect()
    }

    /// What the agent adds to its next round for a choice still waiting:
    /// worked out again from the store as it is now.
    fn remove_copies(&mut self, chosen: &[Chosen]) -> Plan {
        let p = self.obs.store().read(|o| oneup_exec::dupes::plan(o, chosen, &[])).unwrap();
        self.extra = p.ops.clone();
        p
    }

    /// The code the preview shows for `p`, its paths as the stop's list
    /// shows them.
    fn preview_code(&self, p: &Plan) -> String {
        p.code(&self.obs.store().read(|o| shown_paths(o, &p.ops)).unwrap())
    }

    /// The user approves the stop the last round showed.
    fn approve_stop(&mut self) {
        assert!(!self.stops.raised.is_empty(), "a stop to approve");
        self.stops.approve();
    }

    /// What OneDrive's recycle bin and version history hold of `content`.
    fn recycled(cloud: &Fake, content: &[u8]) -> usize {
        cloud.0.lock().unwrap().kept.iter().filter(|k| k.as_slice() == content).count()
    }
}

/// Duplicates the user chose go only once the safety stop for exactly them
/// is approved, even a few, far below the circuit breakers' limits. Then
/// OneDrive's copies go to its recycle bin and the copies here, synced or
/// only here, to the version store; the copy left stays on both sides, and
/// being the last, it can't be chosen.
#[tokio::test]
async fn everyday_duplicates_chosen_for_removal_go_recoverably_once_approved() {
    let cloud = Fake::new();
    let mut s = Sync::new("dupes-remove", cloud.clone());
    let photo = big(b'p');
    for p in ["photos/a.jpg", "backup/a.jpg", "old/a copy.jpg", "_vti_a.jpg"] {
        write(&s.p(p), &photo);
    }
    write(&s.p("notes.txt"), b"notes");
    s.settle().await;
    assert!(!cloud.tree().contains_key("/_vti_a.jpg"), "a name OneDrive refuses: only here");
    // Hashed on request (`oneup duplicates --hash-local`).
    let id = s.local_id("_vti_a.jpg");
    s.obs
        .store_mut()
        .write(|o| {
            let row = o.local_get(id.0)?.unwrap();
            o.local_only_hash_put(id.0, row.size, row.mtime, &qxh(&photo)).map(|_| ())
        })
        .unwrap();
    assert_eq!(s.copies_of(&photo).len(), 4);

    let chosen = s.chosen(&photo, &["/backup/a.jpg", "/old/a copy.jpg", "/_vti_a.jpg"]);
    let plan = s.remove_copies(&chosen);
    assert_eq!((plan.ops.len(), plan.kept.len()), (3, 0), "one removal per copy, a synced one from both sides");
    s.round().await;
    assert_eq!(s.stops.raised.len(), 1, "a stop of its own, below every limit");
    assert_eq!(
        format!("{:016x}", s.stops.raised[0].fingerprint.0),
        s.preview_code(&plan),
        "the code the preview showed"
    );
    for p in ["backup/a.jpg", "old/a copy.jpg", "_vti_a.jpg"] {
        assert!(s.p(p).exists(), "{p}: not removed without approval");
    }
    assert!(cloud.tree().contains_key("/backup/a.jpg"));

    s.approve_stop();
    s.remove_copies(&chosen);
    let r = s.round().await;
    assert!(s.stops.raised.is_empty());
    assert_eq!(r.done.len(), 3, "{r:?}");
    s.settle().await;
    for p in ["backup/a.jpg", "old/a copy.jpg", "_vti_a.jpg"] {
        assert!(!s.p(p).exists(), "{p}");
    }
    let t = cloud.tree();
    assert!(!t.contains_key("/backup/a.jpg") && !t.contains_key("/old/a copy.jpg"));
    assert_eq!(t["/photos/a.jpg"].as_deref(), Some(photo.as_slice()));
    assert_eq!(std::fs::read(s.p("photos/a.jpg")).unwrap(), photo);
    assert_eq!(s.versions().iter().filter(|v| **v == photo).count(), 3, "each copy here, in the version store");
    assert_eq!(Sync::recycled(&cloud, &photo), 2, "each copy in OneDrive, in its recycle bin");

    let left = s.copies_of(&photo);
    assert_eq!(left.len(), 1);
    let p = s.remove_copies(&[left[0].1.clone()]);
    assert!(p.ops.is_empty());
    assert_eq!(p.kept, [(0, Kept::LastCopy)]);
}

/// A copy that changed after the preview isn't removed: worked out again
/// for the round, it's left out (and the stop's code changes); changed
/// after the round planned it, the executor's preconditions skip what it
/// touches. The edit survives on both sides either way.
#[tokio::test]
async fn a_duplicate_changed_after_the_preview_is_skipped_and_its_edit_survives() {
    let cloud = Fake::new();
    let mut s = Sync::new("dupes-changed", cloud.clone());
    let data = big(b'd');
    for p in ["a/x.bin", "b/x.bin", "c/x.bin"] {
        write(&s.p(p), &data);
    }
    s.settle().await;

    // Edited here after it was shown: once that's seen, it's left out.
    let chosen = s.chosen(&data, &["/b/x.bin", "/c/x.bin"]);
    let shown = s.remove_copies(&chosen);
    s.round().await;
    let edited_here = big(b'h');
    write(&s.p("b/x.bin"), &edited_here);
    s.extra.clear();
    s.round().await;
    let now = s.remove_copies(&chosen);
    assert_eq!(now.kept.len(), 1, "{now:?}");
    assert!(matches!(now.kept[0], (0, Kept::NotInSync | Kept::Changed)), "{now:?}");
    assert_ne!(s.preview_code(&now), s.preview_code(&shown));

    // Edited in OneDrive after the removal was worked out: the round that
    // brings the edit here runs none of the removal; worked out again, the
    // copy has changed.
    s.round().await;
    s.approve_stop();
    let edited_there = big(b't');
    cloud.edit("c/x.bin", &edited_there);
    s.remove_copies(&chosen);
    let r = s.round().await;
    let ran = r.done.iter().chain(r.skipped.iter().map(|(o, _)| o)).any(oneup_gate::is_removal);
    assert!(!ran, "put off while the round changes the copy: {r:?}");
    let again = s.remove_copies(&chosen);
    assert!(again.kept.iter().any(|(i, k)| *i == 1 && *k == Kept::Changed), "{again:?}");
    s.extra.clear();
    s.settle().await;
    assert_eq!(std::fs::read(s.p("b/x.bin")).unwrap(), edited_here);
    assert_eq!(cloud.tree()["/b/x.bin"].as_deref(), Some(edited_here.as_slice()));
    assert_eq!(std::fs::read(s.p("c/x.bin")).unwrap(), edited_there);
    assert_eq!(cloud.tree()["/c/x.bin"].as_deref(), Some(edited_there.as_slice()));
    assert_eq!(std::fs::read(s.p("a/x.bin")).unwrap(), data, "never chosen");
    // The version it replaced: in OneDrive's version history, or the
    // version store if the copy here went first.
    assert!(Sync::recycled(&cloud, &data) > 0 || s.versions().contains(&data));
}

/// A crash at any point of an approved removal loses nothing: after a
/// restart, sync converges, the copy kept is on both sides, and every copy
/// that went is in the version store or OneDrive's recycle bin.
#[tokio::test]
async fn a_crash_while_removing_duplicates_loses_nothing() {
    for n in 0.. {
        let cloud = Fake::new();
        let mut s = Sync::on_disk(&format!("dupes-crash{n}"), cloud.clone());
        let photo = big(b'c');
        for p in ["keep/a.jpg", "dup/a.jpg", "_vti_a.jpg"] {
            write(&s.p(p), &photo);
        }
        s.settle().await;
        let id = s.local_id("_vti_a.jpg");
        s.obs
            .store_mut()
            .write(|o| {
                let row = o.local_get(id.0)?.unwrap();
                o.local_only_hash_put(id.0, row.size, row.mtime, &qxh(&photo)).map(|_| ())
            })
            .unwrap();
        let chosen = s.chosen(&photo, &["/dup/a.jpg", "/_vti_a.jpg"]);
        s.remove_copies(&chosen);
        s.round().await;
        s.approve_stop();
        s.remove_copies(&chosen);
        s.exec.crash_after(Some(n));
        let r = s.attempt().await;
        if r.halted.as_deref() != Some(CRASHED) {
            assert!(n >= 3, "only {n} crash points");
            return;
        }
        let mut s = s.restart();
        s.extra.clear();
        s.settle().await;
        assert_eq!(std::fs::read(s.p("keep/a.jpg")).unwrap(), photo, "crash point {n}");
        assert_eq!(cloud.tree()["/keep/a.jpg"].as_deref(), Some(photo.as_slice()), "crash point {n}");
        let here = local_tree(&s.dir.0).values().filter(|c| c.as_deref() == Some(photo.as_slice())).count();
        let kept = s.versions().iter().filter(|v| **v == photo).count();
        assert!(here + kept >= 3, "crash point {n}: a copy on this PC was lost ({here} here, {kept} kept)");
        let there = cloud.tree().values().filter(|c| c.as_deref() == Some(photo.as_slice())).count();
        assert!(there + Sync::recycled(&cloud, &photo) >= 2, "crash point {n}: a copy in OneDrive was lost");
    }
}

/// The store's durable commits, and the record written ahead, as a second
/// connection sees them while OneDrive's copy goes.
type DurableAndRecord = (u64, Option<String>);

/// A synced copy's removal writes its record ahead durably before
/// OneDrive's copy goes: a power cut then can't lose it, so the copy here
/// is never left to an ordinary deletion.
#[tokio::test]
async fn a_removal_is_recorded_durably_before_onedrives_copy_goes() {
    let cloud = Fake::new();
    let mut s = Sync::on_disk("dupes-durable", cloud.clone());
    let data = big(b'd');
    write(&s.p("keep/x.bin"), &data);
    write(&s.p("dup/x.bin"), &data);
    s.settle().await;
    let plan = s.remove_copies(&s.chosen(&data, &["/dup/x.bin"]));
    s.extra.clear();
    let id = resolve(&cloud.0.lock().unwrap(), "/dup/x.bin");
    let key = format!("exec.pending.{:x}", s.local_id("dup/x.bin").0);
    let before = s.obs.store().read(|o| o.durable_commits()).unwrap();
    let db = s.db.as_ref().expect("a database on disk").0.clone();
    let seen: Arc<Mutex<Option<DurableAndRecord>>> = Arc::default();
    let out = seen.clone();
    s.cloud.0.lock().unwrap().on_remove = Some(Arc::new(move |removed: &str| {
        if removed == id {
            let store = Store::open(&db, 1024).unwrap();
            *out.lock().unwrap() = Some(store.read(|o| Ok((o.durable_commits()?, o.meta_get(&key)?))).unwrap());
        }
    }));
    let r = s.run_approved_now(plan.ops).await;
    assert!(r.done.iter().any(oneup_gate::is_removal), "{r:?}");
    let (durable, record) = seen.lock().unwrap().clone().expect("OneDrive's copy went");
    assert!(record.as_deref().is_some_and(|r| r.starts_with("removing ")), "{record:?}");
    assert_eq!(durable, before + 1, "in a durable commit");
}

impl Sync {
    /// Runs `ops` now, as the agent would once their stop is approved, but
    /// without reading either side first: what changes between a round's
    /// plan and its run.
    async fn run_approved_now(&mut self, ops: Vec<Op>) -> Report {
        let first = review_in(&Policy::default(), &Context::default(), ops.clone());
        let approvals: Vec<oneup_gate::Approval> =
            first.breakers.iter().map(|b| oneup_gate::Approval { side: b.side, fingerprint: b.fingerprint }).collect();
        let r = review_in(&Policy::default(), &Context { approvals: &approvals, ..Context::default() }, ops);
        assert!(r.breakers.is_empty(), "{:?}", r.breakers);
        self.exec.run(self.obs.store_mut(), r.approved).await
    }
}

/// The copy that stays is re-read just before a removal runs: changed in
/// OneDrive meanwhile (another content), the removal is skipped and the
/// chosen copy stays on both sides.
#[tokio::test]
async fn a_removal_whose_survivor_changed_just_before_is_skipped() {
    let cloud = Fake::new();
    let mut s = Sync::new("dupes-witness", cloud.clone());
    let data = big(b'w');
    write(&s.p("keep/x.bin"), &data);
    write(&s.p("dup/x.bin"), &data);
    s.settle().await;
    let plan = s.remove_copies(&s.chosen(&data, &["/dup/x.bin"]));
    assert_eq!(plan.ops.len(), 1);
    s.extra.clear();
    // The copy that stays is edited in OneDrive; nothing has read it yet.
    cloud.edit("keep/x.bin", &big(b'e'));
    let r = s.run_approved_now(plan.ops).await;
    assert!(r.skipped.iter().any(|(op, k)| oneup_gate::is_removal(op) && *k == oneup_exec::Skip::Changed), "{r:?}");
    assert_eq!(std::fs::read(s.p("dup/x.bin")).unwrap(), data);
    assert_eq!(cloud.tree()["/dup/x.bin"].as_deref(), Some(data.as_slice()));
}

/// A round whose own plan deletes the copy that stays runs none of the
/// removal: deleted here meanwhile, the survivor goes to OneDrive's recycle
/// bin as any deletion, and the chosen copy, the last one now, stays.
#[tokio::test]
async fn a_removal_waits_while_the_round_deletes_its_survivor() {
    let cloud = Fake::new();
    let mut s = Sync::new("dupes-survivor-race", cloud.clone());
    let data = big(b's');
    write(&s.p("keep/x.bin"), &data);
    write(&s.p("dup/x.bin"), &data);
    s.settle().await;
    let chosen = s.chosen(&data, &["/dup/x.bin"]);
    s.remove_copies(&chosen);
    s.round().await;
    s.approve_stop();
    std::fs::remove_file(s.p("keep/x.bin")).unwrap();
    wait_for_journal(&mut s, 1);
    s.remove_copies(&chosen);
    let r = s.round().await;
    let ran = r.done.iter().chain(r.skipped.iter().map(|(o, _)| o)).any(oneup_gate::is_removal);
    assert!(!ran, "put off: the round deletes the copy that stays: {r:?}");
    assert!(r.done.iter().any(|op| matches!(op, Op::DeleteRemote { .. })), "{r:?}");
    let left = s.remove_copies(&chosen);
    assert_eq!(left.kept, [(0, Kept::LastCopy)]);
    s.extra.clear();
    s.settle().await;
    assert_eq!(std::fs::read(s.p("dup/x.bin")).unwrap(), data);
    assert_eq!(cloud.tree()["/dup/x.bin"].as_deref(), Some(data.as_slice()));
}

/// A synced copy renamed here after it was chosen isn't removed: just
/// before acting, its name isn't the synced one; once the rename is seen,
/// the whole choice waits until it has synced. The rename then syncs.
#[tokio::test]
async fn a_chosen_copy_renamed_meanwhile_is_left_alone() {
    let cloud = Fake::new();
    let mut s = Sync::new("dupes-renamed", cloud.clone());
    let data = big(b'r');
    write(&s.p("keep/x.bin"), &data);
    write(&s.p("dup/x.bin"), &data);
    s.settle().await;
    let chosen = s.chosen(&data, &["/dup/x.bin"]);
    let plan = s.remove_copies(&chosen);
    s.extra.clear();
    std::fs::rename(s.p("dup/x.bin"), s.p("dup/renamed.bin")).unwrap();
    let r = s.run_approved_now(plan.ops).await;
    assert!(r.skipped.iter().any(|(op, _)| oneup_gate::is_removal(op)), "{r:?}");
    assert!(s.p("dup/renamed.bin").exists() && cloud.tree().contains_key("/dup/x.bin"));
    wait_for_journal(&mut s, 1);
    let now = s.remove_copies(&chosen);
    assert!(now.wait && now.ops.is_empty(), "{now:?}");
    s.extra.clear();
    s.settle().await;
    assert_eq!(cloud.tree()["/dup/renamed.bin"].as_deref(), Some(data.as_slice()));
    assert_eq!(std::fs::read(s.p("dup/renamed.bin")).unwrap(), data);
}

/// The copy here is checked against the chosen content, not only its size
/// and time: rewritten with the same size and its time put back, it isn't
/// removed, and neither is OneDrive's copy.
#[tokio::test]
async fn a_copy_rewritten_with_its_old_stamp_is_not_removed() {
    let cloud = Fake::new();
    let mut s = Sync::new("dupes-same-stamp", cloud.clone());
    let data = big(b'm');
    write(&s.p("keep/x.bin"), &data);
    write(&s.p("dup/x.bin"), &data);
    s.settle().await;
    let plan = s.remove_copies(&s.chosen(&data, &["/dup/x.bin"]));
    s.extra.clear();
    let path = s.p("dup/x.bin");
    let was = std::fs::metadata(&path).unwrap().modified().unwrap();
    let other = big(b'o');
    assert_eq!(other.len(), data.len());
    std::fs::write(&path, &other).unwrap();
    std::fs::File::options().write(true).open(&path).unwrap().set_modified(was).unwrap();
    let r = s.run_approved_now(plan.ops).await;
    assert!(r.skipped.iter().any(|(op, k)| oneup_gate::is_removal(op) && *k == oneup_exec::Skip::Changed), "{r:?}");
    assert_eq!(std::fs::read(&path).unwrap(), other, "left alone");
    assert_eq!(cloud.tree()["/dup/x.bin"].as_deref(), Some(data.as_slice()), "OneDrive's copy too");
}

/// A synced copy moved here to another folder under the same name after it
/// was chosen isn't removed: just before acting, it isn't where it was
/// shown (its folder, by the handle's final path), and neither copy goes.
#[tokio::test]
async fn a_chosen_copy_moved_meanwhile_under_its_name_is_left_alone() {
    let cloud = Fake::new();
    let mut s = Sync::new("dupes-moved", cloud.clone());
    let data = big(b'v');
    write(&s.p("keep/x.bin"), &data);
    write(&s.p("dup/x.bin"), &data);
    write(&s.p("important/other.txt"), b"other");
    s.settle().await;
    let plan = s.remove_copies(&s.chosen(&data, &["/dup/x.bin"]));
    s.extra.clear();
    std::fs::rename(s.p("dup/x.bin"), s.p("important/x.bin")).unwrap();
    let r = s.run_approved_now(plan.ops).await;
    assert!(r.skipped.iter().any(|(op, _)| oneup_gate::is_removal(op)), "{r:?}");
    assert_eq!(std::fs::read(s.p("important/x.bin")).unwrap(), data);
    assert_eq!(cloud.tree()["/dup/x.bin"].as_deref(), Some(data.as_slice()), "nor OneDrive's");
    s.settle().await;
    assert_eq!(cloud.tree()["/important/x.bin"].as_deref(), Some(data.as_slice()));
}

/// A chosen copy whose folder above its own moved after it was shown isn't
/// removed: it's no longer at the full path the approval named (here, by
/// the handle's final path), though its own folder and name are the same.
#[tokio::test]
async fn a_chosen_copy_whose_folder_above_moved_here_is_left_alone() {
    let cloud = Fake::new();
    let mut s = Sync::new("dupes-above-here", cloud.clone());
    let data = big(b'u');
    write(&s.p("keep/x.bin"), &data);
    write(&s.p("a/dup/x.bin"), &data);
    write(&s.p("important/other.txt"), b"other");
    s.settle().await;
    let plan = s.remove_copies(&s.chosen(&data, &["/a/dup/x.bin"]));
    s.extra.clear();
    std::fs::rename(s.p("a"), s.p("important/a")).unwrap();
    let r = s.run_approved_now(plan.ops).await;
    assert!(r.skipped.iter().any(|(op, _)| oneup_gate::is_removal(op)), "{r:?}");
    assert_eq!(std::fs::read(s.p("important/a/dup/x.bin")).unwrap(), data);
    assert_eq!(cloud.tree()["/a/dup/x.bin"].as_deref(), Some(data.as_slice()), "nor OneDrive's");
    s.settle().await;
    assert_eq!(cloud.tree()["/important/a/dup/x.bin"].as_deref(), Some(data.as_slice()));
}

/// The same in OneDrive: a folder above the chosen copy moved there after
/// it was shown, so neither copy goes.
#[tokio::test]
async fn a_chosen_copy_whose_folder_above_moved_in_onedrive_is_left_alone() {
    let cloud = Fake::new();
    let mut s = Sync::new("dupes-above-cloud", cloud.clone());
    let data = big(b't');
    write(&s.p("keep/x.bin"), &data);
    write(&s.p("a/dup/x.bin"), &data);
    write(&s.p("important/other.txt"), b"other");
    s.settle().await;
    let plan = s.remove_copies(&s.chosen(&data, &["/a/dup/x.bin"]));
    s.extra.clear();
    cloud.mv("/a", "/important");
    let r = s.run_approved_now(plan.ops).await;
    assert!(r.skipped.iter().any(|(op, _)| oneup_gate::is_removal(op)), "{r:?}");
    assert_eq!(cloud.tree()["/important/a/dup/x.bin"].as_deref(), Some(data.as_slice()));
    assert_eq!(std::fs::read(s.p("a/dup/x.bin")).unwrap(), data, "nor the copy here");
    s.settle().await;
    assert_eq!(std::fs::read(s.p("important/a/dup/x.bin")).unwrap(), data);
}

/// While the copy here is open to be removed, its place checked, no folder
/// above it can be moved or renamed by another program: the path checked is
/// the one it leaves from. Tried in that moment (`cmd /c move` and `ren`),
/// both are refused, and the copy goes from the path shown.
#[tokio::test]
async fn a_folder_above_a_copy_being_removed_cant_move() {
    use std::os::windows::process::CommandExt;
    let cloud = Fake::new();
    let mut s = Sync::new("dupes-above-held", cloud.clone());
    let data = big(b'm');
    write(&s.p("keep/x.bin"), &data);
    write(&s.p("a/dup/x.bin"), &data);
    write(&s.p("important/other.txt"), b"other");
    s.settle().await;
    let plan = s.remove_copies(&s.chosen(&data, &["/a/dup/x.bin"]));
    s.extra.clear();
    let tried = Arc::new(std::sync::Mutex::new(Vec::new()));
    let (t, a, dup, to) = (tried.clone(), s.p("a"), s.p("a/dup"), s.p("important"));
    s.exec.local().on_removing(Some(Arc::new(move |_: &Path| {
        let cmd = |line: String| std::process::Command::new("cmd").raw_arg(line).status().unwrap().success();
        let moved = cmd(format!("/C move \"{}\" \"{}\" >nul 2>nul", a.display(), to.display()));
        let renamed = cmd(format!("/C ren \"{}\" dup2 >nul 2>nul", dup.display()));
        t.lock().unwrap().extend([moved, renamed]);
    })));
    let r = s.run_approved_now(plan.ops).await;
    s.exec.local().on_removing(None);
    assert_eq!(*tried.lock().unwrap(), [false, false], "neither folder could move");
    assert!(r.done.iter().any(oneup_gate::is_removal), "{r:?}");
    assert!(!s.p("a/dup/x.bin").exists());
    assert!(s.versions().contains(&data), "kept from the path shown");
    assert_eq!(std::fs::read(s.p("keep/x.bin")).unwrap(), data);
    // The same move, once nothing holds the copy open, goes through.
    let line = format!("/C move \"{}\" \"{}\" >nul 2>nul", s.p("a").display(), s.p("important").display());
    assert!(std::process::Command::new("cmd").raw_arg(line).status().unwrap().success());
    assert!(s.p("important/a/dup").is_dir());
}

/// A folder above OneDrive's copy, moved there by someone else between the
/// removal's last check of it and its request (Graph can't make the two
/// one step), is harmless: the item removed is exactly the one approved (by
/// ID and eTag: a folder moving doesn't change what's in it), it goes to
/// OneDrive's recycle bin, the copy that stays stands, and the moved folder
/// and everything else in it are untouched.
#[tokio::test]
async fn a_folder_above_moved_in_onedrive_during_the_removal_is_harmless() {
    let cloud = Fake::new();
    let mut s = Sync::new("dupes-above-race", cloud.clone());
    let data = big(b'r');
    write(&s.p("keep/x.bin"), &data);
    write(&s.p("a/dup/x.bin"), &data);
    write(&s.p("a/dup/other.txt"), b"other");
    write(&s.p("important/note.txt"), b"note");
    s.settle().await;
    let plan = s.remove_copies(&s.chosen(&data, &["/a/dup/x.bin"]));
    s.extra.clear();
    let id = resolve(&cloud.0.lock().unwrap(), "/a/dup/x.bin");
    cloud.0.lock().unwrap().prune_moves = Some(("/a".into(), "/important".into()));
    let r = s.run_approved_now(plan.ops).await;
    assert!(r.done.iter().any(oneup_gate::is_removal), "{r:?}");
    assert!(!cloud.0.lock().unwrap().items.contains_key(&id), "the item approved went");
    assert_eq!(Sync::recycled(&cloud, &data), 1, "to OneDrive's recycle bin");
    let t = cloud.tree();
    assert_eq!(t["/important/a/dup/other.txt"].as_deref(), Some(&b"other"[..]));
    assert_eq!(t["/keep/x.bin"].as_deref(), Some(data.as_slice()));
    assert!(s.versions().contains(&data), "the copy here, from the path shown");
    s.settle().await;
    assert_eq!(std::fs::read(s.p("important/a/dup/other.txt")).unwrap(), b"other");
    assert!(!s.p("important/a/dup/x.bin").exists());
    assert_eq!(std::fs::read(s.p("keep/x.bin")).unwrap(), data);
}

/// OneDrive refusing a removal leaves both copies as they were: nothing
/// here goes first, so no ordinary deletion is left to carry the removal
/// out later, without its stop or its survivor. Even once the copy that
/// stayed is deleted, the chosen copy stays on both sides.
#[tokio::test]
async fn a_refused_removal_never_finishes_without_its_survivor() {
    let cloud = Fake::new();
    let mut s = Sync::new("dupes-refused", cloud.clone());
    let data = big(b'f');
    write(&s.p("keep/x.bin"), &data);
    write(&s.p("dup/x.bin"), &data);
    s.settle().await;
    let plan = s.remove_copies(&s.chosen(&data, &["/dup/x.bin"]));
    s.extra.clear();
    cloud.0.lock().unwrap().prune_fails = true;
    let r = s.run_approved_now(plan.ops).await;
    assert!(!r.done.iter().any(oneup_gate::is_removal), "{r:?}");
    cloud.0.lock().unwrap().prune_fails = false;
    assert_eq!(std::fs::read(s.p("dup/x.bin")).unwrap(), data, "nothing here went first");
    std::fs::remove_file(s.p("keep/x.bin")).unwrap();
    s.settle().await;
    assert!(!cloud.tree().contains_key("/keep/x.bin"), "the survivor's deletion is the user's own");
    assert_eq!(cloud.tree()["/dup/x.bin"].as_deref(), Some(data.as_slice()), "the chosen copy stays");
    assert_eq!(std::fs::read(s.p("dup/x.bin")).unwrap(), data);
}

/// A copy that stays here is read and hashed just before the removal, not
/// trusted by its size and time: rewritten with the same size and its time
/// put back, it no longer counts, and the chosen copy isn't removed.
#[tokio::test]
async fn a_removal_whose_survivor_here_was_rewritten_with_its_old_stamp_is_skipped() {
    let cloud = Fake::new();
    let mut s = Sync::new("dupes-survivor-stamp", cloud.clone());
    let data = big(b'q');
    write(&s.p("dup/x.bin"), &data);
    write(&s.p("_vti_x.bin"), &data);
    s.settle().await;
    let id = s.local_id("_vti_x.bin");
    s.obs
        .store_mut()
        .write(|o| {
            let row = o.local_get(id.0)?.unwrap();
            o.local_only_hash_put(id.0, row.size, row.mtime, &qxh(&data)).map(|_| ())
        })
        .unwrap();
    let plan = s.remove_copies(&s.chosen(&data, &["/dup/x.bin"]));
    assert!(matches!(&plan.ops[0], Op::RemoveCopy { keep: oneup_sync::Keep::Local(_), .. }), "{plan:?}");
    s.extra.clear();
    let path = s.p("_vti_x.bin");
    let was = std::fs::metadata(&path).unwrap().modified().unwrap();
    std::fs::write(&path, big(b'z')).unwrap();
    std::fs::File::options().write(true).open(&path).unwrap().set_modified(was).unwrap();
    let r = s.run_approved_now(plan.ops).await;
    assert!(r.skipped.iter().any(|(op, k)| oneup_gate::is_removal(op) && *k == oneup_exec::Skip::Changed), "{r:?}");
    assert_eq!(std::fs::read(s.p("dup/x.bin")).unwrap(), data);
    assert_eq!(cloud.tree()["/dup/x.bin"].as_deref(), Some(data.as_slice()));
}

/// OneDrive's copy went, and then the copy here changed before it could go
/// too (rewritten under its old stamp): the removal isn't finished by an
/// ordinary deletion later. The copy here keeps its new content and is
/// uploaded again, and the copy that stays is untouched.
#[tokio::test]
async fn a_removal_whose_copy_here_changes_after_onedrives_went_is_undone() {
    use std::sync::atomic::{AtomicUsize, Ordering::SeqCst};
    let cloud = Fake::new();
    let mut s = Sync::new("dupes-undone", cloud.clone());
    let data = big(b'u');
    write(&s.p("keep/x.bin"), &data);
    write(&s.p("dup/x.bin"), &data);
    s.settle().await;
    let plan = s.remove_copies(&s.chosen(&data, &["/dup/x.bin"]));
    s.extra.clear();
    // The second job of the removal is the copy here going (the first read
    // it): just before, it's rewritten with the same size and time.
    let path = s.p("dup/x.bin");
    let rewritten = big(b'n');
    let (calls, p, new) = (Arc::new(AtomicUsize::new(0)), path.clone(), rewritten.clone());
    s.exec.on_blocking(Some(Arc::new(move |op: &Op| {
        if oneup_gate::is_removal(op) && calls.fetch_add(1, SeqCst) == 1 {
            let was = std::fs::metadata(&p).unwrap().modified().unwrap();
            std::fs::write(&p, &new).unwrap();
            std::fs::File::options().write(true).open(&p).unwrap().set_modified(was).unwrap();
        }
    })));
    let r = s.run_approved_now(plan.ops).await;
    s.exec.on_blocking(None);
    assert!(r.skipped.iter().any(|(op, _)| oneup_gate::is_removal(op)), "{r:?}");
    assert!(!cloud.tree().contains_key("/dup/x.bin"), "OneDrive's went first");
    s.settle().await;
    assert_eq!(std::fs::read(&path).unwrap(), rewritten, "kept here");
    assert_eq!(cloud.tree()["/dup/x.bin"].as_deref(), Some(rewritten.as_slice()), "and sent again");
    assert_eq!(cloud.tree()["/keep/x.bin"].as_deref(), Some(data.as_slice()));
}

/// The copy that stays here is saved in the moment between the executor's
/// last look at it and the copy here going (OneDrive's already went): the
/// effect checks its watch once the copy has left its place, puts it back,
/// so the chosen copy stays, is uploaded again, and both contents survive.
#[tokio::test]
async fn a_survivor_saved_just_before_the_copy_here_goes_keeps_the_copy() {
    use std::sync::atomic::{AtomicUsize, Ordering::SeqCst};
    let cloud = Fake::new();
    let mut s = Sync::new("dupes-survivor-gap", cloud.clone());
    let data = big(b'g');
    write(&s.p("dup/x.bin"), &data);
    write(&s.p("_vti_x.bin"), &data);
    s.settle().await;
    let id = s.local_id("_vti_x.bin");
    s.obs
        .store_mut()
        .write(|o| {
            let row = o.local_get(id.0)?.unwrap();
            o.local_only_hash_put(id.0, row.size, row.mtime, &qxh(&data)).map(|_| ())
        })
        .unwrap();
    let plan = s.remove_copies(&s.chosen(&data, &["/dup/x.bin"]));
    assert!(matches!(&plan.ops[0], Op::RemoveCopy { keep: oneup_sync::Keep::Local(_), .. }), "{plan:?}");
    s.extra.clear();
    // The removal's jobs: the survivor read, the copy here read, then the
    // copy here going. Just before that last one, the survivor is saved.
    let survivor = s.p("_vti_x.bin");
    let saved = big(b'h');
    let (calls, p, new) = (Arc::new(AtomicUsize::new(0)), survivor.clone(), saved.clone());
    s.exec.on_blocking(Some(Arc::new(move |op: &Op| {
        if oneup_gate::is_removal(op) && calls.fetch_add(1, SeqCst) == 2 {
            std::fs::write(&p, &new).unwrap();
        }
    })));
    let r = s.run_approved_now(plan.ops).await;
    s.exec.on_blocking(None);
    assert!(r.skipped.iter().any(|(op, _)| oneup_gate::is_removal(op)), "{r:?}");
    assert_eq!(std::fs::read(s.p("dup/x.bin")).unwrap(), data, "the chosen copy stays here");
    assert_eq!(std::fs::read_dir(s.p(".oneup/versions")).unwrap().count(), 0, "back out of the version store");
    assert_eq!(std::fs::read(&survivor).unwrap(), saved);
    s.settle().await;
    assert_eq!(cloud.tree()["/dup/x.bin"].as_deref(), Some(data.as_slice()), "and is sent again");
    assert_eq!(std::fs::read(s.p("dup/x.bin")).unwrap(), data);
}

/// Files On-Demand: a chosen duplicate that's a dehydrated placeholder goes
/// without being downloaded. OneDrive's copy goes to its recycle bin; the
/// placeholder leaves its place through the version store, like any copy,
/// and is deleted there (it had no content here); the copy that stays is
/// untouched.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn on_demand_a_dehydrated_duplicate_goes_without_downloading() {
    let cloud = Fake::new();
    let data = big(b'o');
    cloud.add("keep", None);
    cloud.add("keep/x.bin", Some(&data));
    cloud.add("dup", None);
    cloud.add("dup/x.bin", Some(&data));
    let (_od, mut s) = on_demand_sync("dupes-od", &cloud);
    s.quiesce().await;
    let (dup, keep) = (s.p("dup").join("x.bin"), s.p("keep").join("x.bin"));
    assert_eq!(placeholder(&dup).on_disk, 0);
    let plan = s.remove_copies(&s.chosen(&data, &["/dup/x.bin"]));
    s.extra.clear();
    let r = s.run_approved_now(plan.ops).await;
    assert!(r.done.iter().any(oneup_gate::is_removal), "{r:?}");
    assert!(!dup.exists());
    assert!(!cloud.tree().contains_key("/dup/x.bin"));
    assert_eq!(Sync::recycled(&cloud, &data), 1, "in OneDrive's recycle bin");
    let slots = std::fs::read_dir(s.p(".oneup/versions")).unwrap().count();
    assert_eq!(slots, 0, "nothing of it kept here");
    assert_eq!(placeholder(&keep).on_disk, 0, "nothing downloaded");
    assert_eq!(cloud.tree()["/keep/x.bin"].as_deref(), Some(data.as_slice()));
}

// ---------------------------------------------------------------- databases

/// `oneup-test-sqlite` (this crate's test helper): a program of its own
/// that writes SQLite databases, as a user's program would.
struct SqliteApp {
    child: std::process::Child,
    out: std::io::BufReader<std::process::ChildStdout>,
}

impl SqliteApp {
    fn start(args: &[&str]) -> Self {
        let mut child = std::process::Command::new(env!("CARGO_BIN_EXE_oneup-test-sqlite"))
            .args(args)
            .stdin(std::process::Stdio::piped())
            .stdout(std::process::Stdio::piped())
            .spawn()
            .unwrap();
        let out = std::io::BufReader::new(child.stdout.take().unwrap());
        Self { child, out }
    }

    /// The next line it prints.
    fn line(&mut self) -> String {
        let mut l = String::new();
        std::io::BufRead::read_line(&mut self.out, &mut l).unwrap();
        l.trim().to_string()
    }

    fn running(&mut self) -> bool {
        self.child.try_wait().unwrap().is_none()
    }

    /// Tells a `write` waiting at a quiet moment to go on.
    fn go(&mut self) {
        let stdin = self.child.stdin.as_mut().unwrap();
        stdin
            .write_all(
                b"go
",
            )
            .unwrap();
        stdin.flush().unwrap();
    }

    /// Asks a `hold` to close its database, and waits until it has.
    fn close(mut self) {
        let mut stdin = self.child.stdin.take().unwrap();
        stdin.write_all(b"close\n").unwrap();
        drop(stdin);
        while !matches!(self.line().as_str(), "closed" | "") {}
        assert!(self.child.wait().unwrap().success());
    }

    /// Killed: whatever it had open stays as it was.
    fn kill(mut self) {
        self.child.kill().unwrap();
        self.child.wait().unwrap();
    }
}

/// The helper's executable name, as Windows reports the programs that
/// have a file open (compared in lower case).
const SQLITE_APP: &str = "oneup-test-sqlite.exe";

/// Opens a copy of a database's bytes the way a program would, and checks
/// it: SQLite's own `integrity_check` says `ok`, and it holds as many rows
/// as the counter its transactions raise with each one (a copy that mixes
/// two states of the file shows). Returns the rows. A database the program
/// hasn't made its tables in yet (a state it was in) has none.
fn check_sqlite(bytes: &[u8]) -> Result<i64, String> {
    static N: std::sync::atomic::AtomicUsize = std::sync::atomic::AtomicUsize::new(0);
    let n = N.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
    let dir = Tmp(std::env::temp_dir().join(format!("oneup-exec-sqlite-check-{}-{n}", std::process::id())));
    let _ = std::fs::remove_dir_all(&dir.0);
    std::fs::create_dir_all(&dir.0).unwrap();
    let path = dir.0.join("copy.db");
    std::fs::write(&path, bytes).unwrap();
    let c = rusqlite::Connection::open(&path).map_err(|e| e.to_string())?;
    let ok: String = c.query_row("PRAGMA integrity_check", [], |r| r.get(0)).map_err(|e| e.to_string())?;
    if ok != "ok" {
        return Err(ok);
    }
    let tables: i64 = c.query_row("SELECT count(*) FROM sqlite_master", [], |r| r.get(0)).map_err(|e| e.to_string())?;
    if tables == 0 {
        return Ok(0);
    }
    let rows: i64 = c.query_row("SELECT count(*) FROM item", [], |r| r.get(0)).map_err(|e| e.to_string())?;
    let counted: i64 = c.query_row("SELECT n FROM counter", [], |r| r.get(0)).map_err(|e| e.to_string())?;
    if rows != counted {
        return Err(format!("{rows} rows, but the counter says {counted}"));
    }
    Ok(rows)
}

impl Fake {
    /// What every upload of a file called `name` sent, in order.
    fn sent(&self, name: &str) -> Vec<Vec<u8>> {
        self.0.lock().unwrap().sent.iter().filter(|(n, _)| n == name).map(|(_, c)| c.clone()).collect()
    }
}

/// The waits a round reported for `name`: why, and which programs (in
/// lower case).
fn db_waits(r: &Report, name: &str) -> Vec<(oneup_effects::Wait, Vec<String>)> {
    r.databases
        .iter()
        .filter(|(op, _)| match op {
            Op::Upload { name: n, .. } | Op::Download { name: n, .. } => n == name,
            _ => false,
        })
        .filter_map(|(_, d)| Some((d.wait.clone()?, d.programs.iter().map(|p| p.to_lowercase()).collect())))
        .collect()
}

/// A program writes a SQLite database without pause, in WAL mode and in
/// rollback-journal mode, while rounds run: every version sent opens, and
/// passes SQLite's integrity check with its rows matching its counter. Its
/// `-wal` and `-journal` are sent only empty or with no transaction in
/// them, and never renamed or removed under the program.
#[tokio::test]
async fn everyday_a_sqlite_database_written_without_pause_is_only_sent_whole() {
    for mode in ["wal", "delete"] {
        let cloud = Fake::new();
        let mut s = Sync::new(&format!("sqlite-{mode}"), cloud.clone());
        s.settle().await;
        let db = s.p("app.db");
        let mut app = SqliteApp::start(&["write", db.to_str().unwrap(), "6", mode, "3"]);
        let mut waited = 0;
        // At its first quiet moments, the program waits for a round: what's
        // in the file then is whole, and is sent while it's still writing.
        let mut at_quiet = 0;
        for _ in 0..3 {
            assert_eq!(app.line(), "quiet");
            let before = cloud.sent("app.db").len();
            s.round().await;
            at_quiet += cloud.sent("app.db").len() - before;
            app.go();
        }
        while app.running() {
            let r = s.round().await;
            waited += db_waits(&r, "app.db").len();
            std::thread::sleep(Duration::from_millis(20));
        }
        let rows: i64 = app.line().strip_prefix("done ").unwrap().parse().unwrap();
        let during = cloud.sent("app.db").len();
        s.settle().await;
        let sent = cloud.sent("app.db");
        assert!(!sent.is_empty());
        for (i, v) in sent.iter().enumerate() {
            if let Err(e) = check_sqlite(v) {
                panic!("{mode}: version {i} of {} sent is broken: {e}", sent.len());
            }
        }
        assert_eq!(check_sqlite(sent.last().unwrap()), Ok(rows), "{mode}: the last version sent has every row");
        assert_eq!(cloud.tree()["/app.db"].as_deref(), Some(std::fs::read(&db).unwrap().as_slice()));
        // Its working files go only when they carry nothing it needs.
        for v in cloud.sent("app.db-wal") {
            assert!(v.is_empty(), "{mode}: a -wal holding changes was sent");
        }
        for v in cloud.sent("app.db-journal") {
            assert!(!oneup_rules::database::journal_active(&v), "{mode}: a -journal in a transaction was sent");
        }
        eprintln!("{mode}: {rows} transactions, {} versions sent ({during} while writing), {waited} waits", sent.len());
        assert!(at_quiet > 0 && during > 0, "{mode}: sent while the program was still writing");
    }
}

/// A database open with changes in its `-wal` (not yet in the file) isn't
/// sent, and the round says which program has it open; once the program
/// closes it (SQLite moves the changes into the file), it is, with them.
#[tokio::test]
async fn everyday_a_database_with_changes_in_its_wal_waits_until_they_are_in_it() {
    let cloud = Fake::new();
    let mut s = Sync::new("sqlite-hold", cloud.clone());
    let db = s.p("app.db");
    let mut app = SqliteApp::start(&["hold", db.to_str().unwrap(), "30"]);
    assert_eq!(app.line(), "held");
    let mut waits = Vec::new();
    for _ in 0..3 {
        let r = s.round().await;
        waits.extend(db_waits(&r, "app.db"));
    }
    assert!(cloud.sent("app.db").is_empty(), "not sent while its changes are in the -wal");
    assert!(cloud.sent("app.db-wal").is_empty());
    assert!(
        waits.iter().any(|(w, p)| *w == oneup_effects::Wait::Wal && p.iter().any(|p| p == SQLITE_APP)),
        "{waits:?}"
    );
    app.close();
    s.settle().await;
    let t = cloud.tree();
    assert_eq!(check_sqlite(t["/app.db"].as_deref().unwrap()), Ok(30));
    assert!(!t.contains_key("/app.db-wal"), "{:?}", names(&t));
}

/// A newer version of a database from OneDrive waits while a program has
/// the database open (the round names it), and replaces it once closed.
#[tokio::test]
async fn everyday_a_newer_version_from_onedrive_waits_while_the_database_is_open() {
    let cloud = Fake::new();
    let mut s = Sync::new("sqlite-download", cloud.clone());
    let db = s.p("app.db");
    SqliteApp::start(&["hold", db.to_str().unwrap(), "5"]).close();
    s.settle().await;
    let ours = std::fs::read(&db).unwrap();
    // Theirs: the same database with more rows, made elsewhere.
    let other = Tmp(std::env::temp_dir().join(format!("oneup-exec-sqlite-theirs-{}", std::process::id())));
    let _ = std::fs::remove_dir_all(&other.0);
    std::fs::create_dir_all(&other.0).unwrap();
    let theirs_path = other.0.join("app.db");
    std::fs::copy(&db, &theirs_path).unwrap();
    SqliteApp::start(&["hold", theirs_path.to_str().unwrap(), "7"]).close();
    let theirs = std::fs::read(&theirs_path).unwrap();
    assert_eq!(check_sqlite(&theirs), Ok(12));

    // Opened, with nothing written.
    let mut app = SqliteApp::start(&["hold", db.to_str().unwrap(), "0"]);
    assert_eq!(app.line(), "held");
    cloud.edit("app.db", &theirs);
    let mut waits = Vec::new();
    for _ in 0..3 {
        let r = s.round().await;
        waits.extend(db_waits(&r, "app.db"));
    }
    assert_eq!(std::fs::read(&db).unwrap(), ours, "not replaced while open");
    assert!(
        waits.iter().any(|(w, p)| *w == oneup_effects::Wait::Open && p.iter().any(|p| p == SQLITE_APP)),
        "{waits:?}"
    );
    app.close();
    s.settle().await;
    assert_eq!(std::fs::read(&db).unwrap(), theirs);
    assert_eq!(check_sqlite(&std::fs::read(&db).unwrap()), Ok(12));
}

/// A program that died with changes in a database's `-wal` leaves them
/// there: replacing the database then would have SQLite apply them to the
/// new file. A newer version from OneDrive waits, with `[databases] wait`
/// off too, and the round says why.
#[tokio::test]
async fn a_database_left_with_changes_in_its_wal_is_never_replaced() {
    let cloud = Fake::new();
    let mut s = Sync::new("sqlite-left", cloud.clone());
    let db = s.p("app.db");
    SqliteApp::start(&["hold", db.to_str().unwrap(), "3"]).close();
    s.settle().await;
    let ours = std::fs::read(&db).unwrap();
    let mut app = SqliteApp::start(&["hold", db.to_str().unwrap(), "10"]);
    assert_eq!(app.line(), "held");
    app.kill();
    let wal = std::fs::read(s.p("app.db-wal")).unwrap();
    assert!(!wal.is_empty());
    cloud.edit("app.db", &ours[..ours.len() / 2]);

    // With waiting off too, and the round says why.
    s.exec.set_databases(oneup_rules::database::Databases::off());
    let r = s.round().await;
    assert!(db_waits(&r, "app.db").iter().any(|(w, _)| *w == oneup_effects::Wait::Wal), "{r:?}");
    assert_eq!(std::fs::read(&db).unwrap(), ours);

    s.exec.set_databases(oneup_rules::database::Databases::default());
    let r = s.round().await;
    assert!(db_waits(&r, "app.db").iter().any(|(w, p)| *w == oneup_effects::Wait::Wal && p.is_empty()), "{r:?}");
    assert_eq!(std::fs::read(&db).unwrap(), ours);
    assert_eq!(std::fs::read(s.p("app.db-wal")).unwrap(), wal);
}

/// Another program keeping a file open (PowerShell, here), so the test
/// isn't the one holding it.
struct HeldOpen(std::process::Child);

impl HeldOpen {
    fn new(path: &Path) -> Self {
        let script = format!(
            "$f=[IO.File]::Open('{}','Open','Read','ReadWrite'); [Console]::Out.WriteLine('open'); \
             [Console]::In.ReadLine() | Out-Null; $f.Close()",
            path.display()
        );
        let mut child = std::process::Command::new("powershell.exe")
            .args(["-NoProfile", "-NonInteractive", "-Command", &script])
            .stdin(std::process::Stdio::piped())
            .stdout(std::process::Stdio::piped())
            .spawn()
            .unwrap();
        let mut line = String::new();
        std::io::BufRead::read_line(&mut std::io::BufReader::new(child.stdout.take().unwrap()), &mut line).unwrap();
        assert_eq!(line.trim(), "open");
        Self(child)
    }

    fn close(mut self) {
        drop(self.0.stdin.take());
        self.0.wait().unwrap();
    }
}

/// A password database (and a file a `[databases]` pattern names) open in
/// another program waits, and the round names the program; once closed,
/// it's sent.
#[tokio::test]
async fn everyday_a_database_open_in_another_program_is_sent_once_closed() {
    let cloud = Fake::new();
    let mut s = Sync::new("db-open", cloud.clone());
    s.exec.set_databases(oneup_rules::database::Databases::new(true, &[], &["*.ledger"]).unwrap());
    let mut kdbx = vec![0x03, 0xD9, 0xA2, 0x9A, 0x67, 0xFB, 0x4B, 0xB5, 0x01, 0x00, 0x04, 0x00];
    kdbx.extend(std::iter::repeat_n(0x5A, 500));
    write(&s.p("vault.kdbx"), &kdbx);
    write(&s.p("home.ledger"), b"2026-10-01 coffee 3.50\n");
    let held = [HeldOpen::new(&s.p("vault.kdbx")), HeldOpen::new(&s.p("home.ledger"))];
    let mut waits = Vec::new();
    for _ in 0..2 {
        let r = s.round().await;
        waits.extend(db_waits(&r, "vault.kdbx"));
        waits.extend(db_waits(&r, "home.ledger"));
    }
    let t = cloud.tree();
    assert!(!t.contains_key("/vault.kdbx") && !t.contains_key("/home.ledger"), "{:?}", names(&t));
    let by_powershell = waits.iter().filter(|(w, p)| *w == oneup_effects::Wait::Open && p == &["powershell.exe"]);
    assert!(by_powershell.count() >= 2, "{waits:?}");
    for h in held {
        h.close();
    }
    s.settle().await;
    let t = cloud.tree();
    assert_eq!(t["/vault.kdbx"].as_deref(), Some(kdbx.as_slice()));
    assert!(t.contains_key("/home.ledger"));
}

/// [`check_sqlite`] for a database and the `-wal` beside it, opened
/// together as SQLite would find them on a PC.
fn check_sqlite_with_wal(main: &[u8], wal: &[u8]) -> Result<i64, String> {
    static N: std::sync::atomic::AtomicUsize = std::sync::atomic::AtomicUsize::new(0);
    let n = N.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
    let dir = Tmp(std::env::temp_dir().join(format!("oneup-exec-sqlite-pair-{}-{n}", std::process::id())));
    let _ = std::fs::remove_dir_all(&dir.0);
    std::fs::create_dir_all(&dir.0).unwrap();
    let path = dir.0.join("copy.db");
    std::fs::write(&path, main).unwrap();
    std::fs::write(dir.0.join("copy.db-wal"), wal).unwrap();
    let c = rusqlite::Connection::open(&path).map_err(|e| e.to_string())?;
    let rows: i64 = c.query_row("SELECT count(*) FROM item", [], |r| r.get(0)).map_err(|e| e.to_string())?;
    let counted: i64 = c.query_row("SELECT n FROM counter", [], |r| r.get(0)).map_err(|e| e.to_string())?;
    let ok: String = c.query_row("PRAGMA integrity_check", [], |r| r.get(0)).map_err(|e| e.to_string())?;
    if ok != "ok" || rows != counted {
        return Err(format!("{ok}: {rows} rows, counter {counted}"));
    }
    Ok(rows)
}

/// A database made elsewhere by `oneup-test-sqlite`, with `rows` rows
/// committed into its `-wal` and none moved into the file (the program was
/// killed): its file and its `-wal`. `from`: the file to start from.
fn sqlite_with_wal(tag: &str, from: Option<&[u8]>, rows: u32) -> (Vec<u8>, Vec<u8>) {
    let dir = Tmp(std::env::temp_dir().join(format!("oneup-exec-sqlite-elsewhere-{tag}-{}", std::process::id())));
    let _ = std::fs::remove_dir_all(&dir.0);
    std::fs::create_dir_all(&dir.0).unwrap();
    let path = dir.0.join("app.db");
    match from {
        Some(b) => std::fs::write(&path, b).unwrap(),
        None => SqliteApp::start(&["hold", path.to_str().unwrap(), "0"]).close(),
    }
    let mut app = SqliteApp::start(&["hold", path.to_str().unwrap(), &rows.to_string()]);
    assert_eq!(app.line(), "held");
    app.kill();
    let wal = std::fs::read(dir.0.join("app.db-wal")).unwrap();
    assert!(!wal.is_empty());
    (std::fs::read(&path).unwrap(), wal)
}

/// SQLite's working files never sync beside their database: a first sync
/// of a database whose `-wal` is in OneDrive too (another PC's program
/// left changes in it) brings the database here, as it is, and leaves the
/// `-wal` in OneDrive, untouched.
#[tokio::test]
async fn a_first_sync_leaves_a_databases_wal_where_it_is() {
    let cloud = Fake::new();
    let (main, wal) = sqlite_with_wal("first", None, 6);
    cloud.add("app.db", Some(&main));
    cloud.add("app.db-wal", Some(&wal));
    let mut s = Sync::new("sqlite-first", cloud.clone());
    s.settle().await;
    assert_eq!(std::fs::read(s.p("app.db")).unwrap(), main);
    assert!(!s.p("app.db-wal").exists(), "not brought here");
    assert_eq!(cloud.tree()["/app.db-wal"].as_deref(), Some(wal.as_slice()), "nor removed from OneDrive");
    assert_eq!(check_sqlite(&main), Ok(0), "the database as it is in its file");
}

/// A `-wal` synced before its database was treated as one (here, while a
/// `!` pattern opted it out) stops syncing once it is, and nothing reads as
/// deleted: a newer `-wal` in OneDrive isn't brought here, one here isn't
/// sent, and both copies stay.
#[tokio::test]
async fn a_wal_synced_before_is_left_alone_on_both_sides() {
    let cloud = Fake::new();
    let mut s = Sync::new("sqlite-synced-wal", cloud.clone());
    let db = s.p("app.db");
    SqliteApp::start(&["hold", db.to_str().unwrap(), "3"]).close();
    write(&s.p("app.db-wal"), b"");
    s.exec.set_databases(oneup_rules::database::Databases::new(true, &[], &["!app.db"]).unwrap());
    s.settle().await;
    assert!(cloud.tree().contains_key("/app.db-wal"), "synced while opted out");

    s.exec.set_databases(oneup_rules::database::Databases::default());
    cloud.edit("app.db-wal", b"another PC's changes");
    s.settle().await;
    assert_eq!(std::fs::read(s.p("app.db-wal")).unwrap(), b"", "OneDrive's isn't brought here");
    write(&s.p("app.db-wal"), b"changes here");
    for _ in 0..2 {
        let r = s.round().await;
        let touched = r.done.iter().any(|op| match op {
            Op::Upload { name, .. } | Op::Download { name, .. } => name == "app.db-wal",
            Op::DeleteRemote { .. } | Op::RecycleLocal { .. } | Op::ConflictCopy { .. } => true,
            _ => false,
        });
        assert!(!touched, "{r:?}");
    }
    assert_eq!(std::fs::read(s.p("app.db-wal")).unwrap(), b"changes here");
    assert_eq!(cloud.tree()["/app.db-wal"].as_deref(), Some(&b"another PC's changes"[..]));
    assert!(cloud.tree().contains_key("/app.db"));
}

/// A file named like a working file of a file beside it stays here, by
/// that name alone, whatever its namesake holds (text here); a `!` pattern
/// naming the namesake opts it out, and then it syncs like any file. One
/// with no namesake beside it is ordinary.
#[tokio::test]
async fn files_named_like_working_files_stay_beside_their_namesake_unless_opted_out() {
    let cloud = Fake::new();
    let mut s = Sync::new("sqlite-names", cloud.clone());
    write(&s.p("travel"), b"plans");
    write(&s.p("travel-journal"), b"day one");
    write(&s.p("alone.db-wal"), b"no database beside it");
    s.settle().await;
    let t = cloud.tree();
    assert!(!t.contains_key("/travel-journal"), "stays beside its namesake: {:?}", names(&t));
    assert!(t.contains_key("/alone.db-wal"), "nothing beside it to stay with: {:?}", names(&t));

    s.exec.set_databases(oneup_rules::database::Databases::new(true, &[], &["!travel"]).unwrap());
    s.settle().await;
    assert_eq!(cloud.tree()["/travel-journal"].as_deref(), Some(&b"day one"[..]));
    write(&s.p("travel-journal"), b"day two, here");
    cloud.edit("travel-journal", b"day two, there");
    s.settle().await;
    let t = cloud.tree();
    assert!(t.keys().any(|k| k.starts_with("/travel-journal (conflict")), "both kept: {:?}", names(&t));
}

/// Whether a working file stays here is one decision for its name: once its
/// namesake is on either side, or in the Synced tree, the file here, the one
/// in OneDrive and their Synced row are all set aside together. A working
/// file synced while alone, whose namesake then appears on only one side
/// (either one), is neither sent nor brought as new, nothing halts, and
/// both copies keep their bytes.
#[tokio::test]
async fn a_synced_working_file_stays_put_when_its_namesake_appears_on_one_side() {
    for there in [true, false] {
        let cloud = Fake::new();
        let mut s = Sync::new(if there { "names-one-side-cloud" } else { "names-one-side-here" }, cloud.clone());
        write(&s.p("travel-journal"), b"day one");
        s.settle().await;
        assert_eq!(cloud.tree()["/travel-journal"].as_deref(), Some(&b"day one"[..]), "synced while alone");

        if there {
            cloud.add("travel", Some(b"plans from another PC"));
        } else {
            write(&s.p("travel"), b"plans");
        }
        for _ in 0..3 {
            let r = s.round().await;
            let touched = r.done.iter().chain(r.skipped.iter().map(|(op, _)| op)).any(|op| match op {
                Op::Upload { name, .. } | Op::Download { name, .. } => name == "travel-journal",
                Op::DeleteRemote { .. } | Op::RecycleLocal { .. } | Op::ConflictCopy { .. } => true,
                _ => false,
            });
            assert!(!touched, "{r:?}");
        }
        assert_eq!(std::fs::read(s.p("travel-journal")).unwrap(), b"day one");
        let t = cloud.tree();
        assert_eq!(t["/travel-journal"].as_deref(), Some(&b"day one"[..]));
        assert!(!t.keys().any(|k| k.contains("conflict")), "{:?}", names(&t));
    }
}

/// A synced working file renamed away from its working name on one side
/// (either one), its namesake beside it, keeps its Synced record: the
/// renamed copy isn't taken for a new file and sent or brought (which would
/// collide with that record and halt every round), nothing is deleted, and
/// both copies keep their bytes.
#[tokio::test]
async fn a_synced_working_file_renamed_on_one_side_isnt_taken_for_new() {
    for there in [true, false] {
        let cloud = Fake::new();
        let mut s = Sync::new(if there { "names-renamed-cloud" } else { "names-renamed-here" }, cloud.clone());
        write(&s.p("travel-journal"), b"day one");
        s.settle().await;
        write(&s.p("travel"), b"plans");
        s.settle().await;
        let t = cloud.tree();
        assert_eq!(t["/travel-journal"].as_deref(), Some(&b"day one"[..]), "synced while alone");
        assert!(t.contains_key("/travel"));

        if there {
            cloud.rename("travel-journal", "notes.txt");
        } else {
            std::fs::rename(s.p("travel-journal"), s.p("notes.txt")).unwrap();
        }
        for _ in 0..3 {
            let r = s.round().await;
            let touched = r.done.iter().chain(r.skipped.iter().map(|(op, _)| op)).any(|op| match op {
                Op::Upload { name, .. } | Op::Download { name, .. } => name == "travel-journal" || name == "notes.txt",
                Op::DeleteRemote { .. } | Op::RecycleLocal { .. } | Op::ConflictCopy { .. } => true,
                _ => false,
            });
            assert!(!touched, "{r:?}");
        }
        let t = cloud.tree();
        let (here, cloud_name) =
            if there { ("travel-journal", "/notes.txt") } else { ("notes.txt", "/travel-journal") };
        assert_eq!(std::fs::read(s.p(here)).unwrap(), b"day one");
        assert_eq!(t[cloud_name].as_deref(), Some(&b"day one"[..]), "{:?}", names(&t));
        assert!(!t.keys().any(|k| k.contains("conflict")), "{:?}", names(&t));
    }
}

/// A database's upload is cancelled before it commits when, while it's
/// read, another program writes its `-journal` (which stays as it was in
/// meaning: zeroed, no transaction): the joint watch, not a state check,
/// sees it.
#[tokio::test]
async fn a_write_to_a_databases_journal_during_its_upload_cancels_it() {
    let cloud = Fake::new();
    let mut s = Sync::new("sqlite-joint-watch", cloud.clone());
    let journal = s.p("app.db-journal");
    write(&journal, &[0u8; 512]);
    s.settle().await;
    let made = Tmp(std::env::temp_dir().join(format!("oneup-exec-sqlite-made-{}", std::process::id())));
    let _ = std::fs::remove_dir_all(&made.0);
    std::fs::create_dir_all(&made.0).unwrap();
    SqliteApp::start(&["hold", made.0.join("app.db").to_str().unwrap(), "3"]).close();
    std::fs::copy(made.0.join("app.db"), s.p("app.db")).unwrap();
    let script = format!("[IO.File]::WriteAllBytes('{}', (New-Object byte[] 512))", journal.display());
    cloud.0.lock().unwrap().during_upload = Some(Box::new(move || {
        let st = std::process::Command::new("powershell.exe")
            .args(["-NoProfile", "-NonInteractive", "-Command", &script])
            .status()
            .unwrap();
        assert!(st.success());
    }));
    let r = s.round().await;
    let changed = r
        .skipped
        .iter()
        .any(|(op, k)| matches!(op, Op::Upload { name, .. } if name == "app.db") && *k == oneup_exec::Skip::Changed);
    assert!(changed, "{r:?}");
    assert!(cloud.sent("app.db").is_empty());
    s.settle().await;
    assert_eq!(check_sqlite(cloud.tree()["/app.db"].as_deref().unwrap()), Ok(3));
}

/// A `-wal` created with changes while a database is read cancels its
/// upload before it commits: every read checks nothing waits outside it.
#[tokio::test]
async fn a_wal_created_during_a_databases_upload_cancels_it() {
    use std::os::windows::process::CommandExt;
    let cloud = Fake::new();
    let mut s = Sync::new("sqlite-wal-created", cloud.clone());
    s.settle().await;
    let made = Tmp(std::env::temp_dir().join(format!("oneup-exec-sqlite-made2-{}", std::process::id())));
    let _ = std::fs::remove_dir_all(&made.0);
    std::fs::create_dir_all(&made.0).unwrap();
    SqliteApp::start(&["hold", made.0.join("app.db").to_str().unwrap(), "3"]).close();
    std::fs::copy(made.0.join("app.db"), s.p("app.db")).unwrap();
    let wal = s.p("app.db-wal");
    cloud.0.lock().unwrap().during_upload = Some(Box::new(move || {
        let st = std::process::Command::new("cmd").raw_arg(format!("/C echo x>\"{}\"", wal.display())).status();
        assert!(st.unwrap().success());
    }));
    let r = s.round().await;
    let changed = r
        .skipped
        .iter()
        .any(|(op, k)| matches!(op, Op::Upload { name, .. } if name == "app.db") && *k == oneup_exec::Skip::Changed);
    assert!(changed, "{r:?}");
    assert!(cloud.sent("app.db").is_empty());
}

/// A `-wal` that gets changes after a replacement's first check, while the
/// newer version downloads, still stops it: the replacement checks again
/// under its exclusive handle, and the round says why.
#[tokio::test]
async fn a_wal_written_during_a_download_stops_the_replacement() {
    use std::os::windows::process::CommandExt;
    let cloud = Fake::new();
    let mut s = Sync::new("sqlite-wal-download", cloud.clone());
    let db = s.p("app.db");
    SqliteApp::start(&["hold", db.to_str().unwrap(), "3"]).close();
    s.settle().await;
    let ours = std::fs::read(&db).unwrap();
    cloud.edit("app.db", &ours[..ours.len() / 2]);
    // The round's first request is its listing; the next, the download.
    let (inner, wal) = (cloud.clone(), s.p("app.db-wal"));
    cloud.0.lock().unwrap().meanwhile = Some(Box::new(move || {
        inner.0.lock().unwrap().meanwhile = Some(Box::new(move || {
            let st = std::process::Command::new("cmd").raw_arg(format!("/C echo x>\"{}\"", wal.display())).status();
            assert!(st.unwrap().success());
        }));
    }));
    let r = s.round().await;
    assert!(db_waits(&r, "app.db").iter().any(|(w, _)| *w == oneup_effects::Wait::Wal), "{r:?}");
    assert_eq!(std::fs::read(&db).unwrap(), ours);
}

/// With `[databases] wait` off, a SQLite database is still sent only once
/// every change is in the file: the setting only decides whether other
/// databases wait for their programs.
#[tokio::test]
async fn a_database_with_changes_in_its_wal_waits_with_waiting_off_too() {
    let cloud = Fake::new();
    let mut s = Sync::new("sqlite-wait-off", cloud.clone());
    s.exec.set_databases(oneup_rules::database::Databases::off());
    let db = s.p("app.db");
    let mut app = SqliteApp::start(&["hold", db.to_str().unwrap(), "20"]);
    assert_eq!(app.line(), "held");
    let mut waits = Vec::new();
    for _ in 0..3 {
        let r = s.round().await;
        waits.extend(db_waits(&r, "app.db"));
    }
    assert!(cloud.sent("app.db").is_empty(), "not sent while its changes are in the -wal");
    assert!(cloud.sent("app.db-wal").is_empty(), "nor the -wal holding them");
    assert!(waits.iter().any(|(w, _)| *w == oneup_effects::Wait::Wal), "{waits:?}");
    app.close();
    s.settle().await;
    assert_eq!(check_sqlite(cloud.tree()["/app.db"].as_deref().unwrap()), Ok(20));
}

/// A SQLite database that appears here after OneUpDrive last looked isn't
/// one a `-wal` from OneDrive may go beside: nothing says the file belongs
/// to it. The download waits until the database is synced.
#[tokio::test]
async fn a_wal_from_onedrive_waits_beside_a_database_not_known_yet() {
    use std::os::windows::process::CommandExt;
    let cloud = Fake::new();
    let mut s = Sync::new("sqlite-unknown-db", cloud.clone());
    s.settle().await;
    cloud.add("app.db-wal", Some(b"from another PC"));
    let made = Tmp(std::env::temp_dir().join(format!("oneup-exec-sqlite-made3-{}", std::process::id())));
    let _ = std::fs::remove_dir_all(&made.0);
    std::fs::create_dir_all(&made.0).unwrap();
    let from = made.0.join("app.db");
    SqliteApp::start(&["keep", from.to_str().unwrap(), "delete"]).close();
    // Copied in by another program after the round looked at this PC.
    let to = s.p("app.db");
    cloud.0.lock().unwrap().meanwhile = Some(Box::new(move || {
        let st = std::process::Command::new("cmd")
            .raw_arg(format!("/C copy /y \"{}\" \"{}\" >nul", from.display(), to.display()))
            .status();
        assert!(st.unwrap().success());
    }));
    let r = s.round().await;
    assert!(s.p("app.db").exists());
    assert!(!s.p("app.db-wal").exists(), "not placed beside a database not known yet");
    assert!(db_waits(&r, "app.db-wal").iter().any(|(w, _)| *w == oneup_effects::Wait::Beside), "{r:?}");
}

/// When Windows can't say which programs have a database open, it isn't
/// taken for closed: the upload is tried again later.
#[tokio::test]
async fn a_database_whose_users_windows_cant_tell_isnt_taken_for_closed() {
    let cloud = Fake::new();
    let mut s = Sync::new("db-rm-fails", cloud.clone());
    let mut kdbx = vec![0x03, 0xD9, 0xA2, 0x9A, 0x67, 0xFB, 0x4B, 0xB5, 0x01, 0x00, 0x04, 0x00];
    kdbx.extend(std::iter::repeat_n(0x5A, 500));
    write(&s.p("vault.kdbx"), &kdbx);
    s.exec.local().fail_restart_manager(true);
    let r = s.round().await;
    let busy = r
        .skipped
        .iter()
        .any(|(op, k)| matches!(op, Op::Upload { name, .. } if name == "vault.kdbx") && *k == oneup_exec::Skip::Busy);
    assert!(busy, "{r:?}");
    assert!(cloud.sent("vault.kdbx").is_empty());
    s.exec.local().fail_restart_manager(false);
    s.settle().await;
    assert_eq!(cloud.tree()["/vault.kdbx"].as_deref(), Some(kdbx.as_slice()));
}

/// An Outlook data file renamed to an ordinary name is known by its
/// content when it's sent, so the agent can still warn about it.
#[tokio::test]
async fn a_renamed_outlook_file_is_known_by_its_content() {
    let cloud = Fake::new();
    let mut s = Sync::new("db-renamed-pst", cloud.clone());
    let mut pst = b"!BDN".to_vec();
    pst.extend(std::iter::repeat_n(0x11, 600));
    write(&s.p("mail backup.dat"), &pst);
    let r = s.round().await;
    let known = r.recognised.iter().any(|(op, f)| {
        matches!(op, Op::Upload { name, .. } if name == "mail backup.dat")
            && *f == oneup_rules::database::Format::Outlook
    });
    assert!(known, "{r:?}");
}

/// A SQLite database with changes in its `-wal` is never moved, renamed
/// or recycled here away from them (a rename or deletion in OneDrive):
/// they'd be left behind, to be applied to whatever takes its name. It
/// waits, untouched, and the round says why.
#[tokio::test]
async fn a_database_is_never_moved_or_recycled_away_from_its_wal() {
    for delete in [false, true] {
        let cloud = Fake::new();
        let mut s = Sync::new(&format!("sqlite-move-{delete}"), cloud.clone());
        let db = s.p("app.db");
        SqliteApp::start(&["hold", db.to_str().unwrap(), "3"]).close();
        s.settle().await;
        let mut app = SqliteApp::start(&["hold", db.to_str().unwrap(), "5"]);
        assert_eq!(app.line(), "held");
        app.kill();
        let (main, wal) = (std::fs::read(&db).unwrap(), std::fs::read(s.p("app.db-wal")).unwrap());
        if delete {
            cloud.delete("app.db");
        } else {
            cloud.rename("app.db", "renamed.db");
        }
        let mut waits = Vec::new();
        for _ in 0..2 {
            let r = s.round().await;
            let moved = r.done.iter().any(|op| matches!(op, Op::MoveLocal { .. } | Op::RecycleLocal { .. }));
            assert!(!moved, "{r:?}");
            waits.extend(r.databases.iter().filter_map(|(_, d)| d.wait.clone()));
        }
        assert!(waits.contains(&oneup_effects::Wait::Wal), "delete {delete}: {waits:?}");
        assert_eq!(std::fs::read(&db).unwrap(), main);
        assert_eq!(std::fs::read(s.p("app.db-wal")).unwrap(), wal);
        assert_eq!(check_sqlite_with_wal(&main, &wal), Ok(8));
    }
}

/// A new database from OneDrive isn't placed beside a `-wal` with changes
/// that didn't come from OneDrive with it (another program's, new here):
/// SQLite would apply them to it.
#[tokio::test]
async fn a_new_database_isnt_placed_beside_a_wal_of_unknown_origin() {
    use std::os::windows::process::CommandExt;
    let cloud = Fake::new();
    let mut s = Sync::new("sqlite-new-beside", cloud.clone());
    s.settle().await;
    let made = Tmp(std::env::temp_dir().join(format!("oneup-exec-sqlite-made4-{}", std::process::id())));
    let _ = std::fs::remove_dir_all(&made.0);
    std::fs::create_dir_all(&made.0).unwrap();
    let from = made.0.join("app.db");
    SqliteApp::start(&["keep", from.to_str().unwrap(), "delete"]).close();
    cloud.add("app.db", Some(&std::fs::read(&from).unwrap()));
    // Written by another program after the round looked at this PC.
    let wal = s.p("app.db-wal");
    cloud.0.lock().unwrap().meanwhile = Some(Box::new(move || {
        let st = std::process::Command::new("cmd").raw_arg(format!("/C echo x>\"{}\"", wal.display())).status();
        assert!(st.unwrap().success());
    }));
    let r = s.round().await;
    assert!(!s.p("app.db").exists(), "not placed beside a -wal it didn't come with");
    assert!(db_waits(&r, "app.db").iter().any(|(w, _)| *w == oneup_effects::Wait::Wal), "{r:?}");
}

/// Moves judge the names they actually use, right before they act: a
/// database renamed in OneDrive isn't moved beside another database's
/// `-wal` with changes (SQLite would apply them to it), and nothing is
/// renamed into a database's working-file name.
#[tokio::test]
async fn a_move_never_lands_a_database_beside_changes_or_a_file_in_a_working_files_name() {
    let cloud = Fake::new();
    let mut s = Sync::new("sqlite-move-into", cloud.clone());
    let old = s.p("old.sqlite");
    SqliteApp::start(&["keep", old.to_str().unwrap(), "delete"]).close();
    write(&s.p("app.sqlite-wal"), b"another database's changes");
    let db = s.p("data.db");
    SqliteApp::start(&["keep", db.to_str().unwrap(), "delete"]).close();
    write(&s.p("notes.txt"), b"notes");
    s.settle().await;

    cloud.rename("old.sqlite", "app.sqlite");
    cloud.rename("notes.txt", "data.db-wal");
    let mut waits = 0usize;
    for _ in 0..2 {
        let r = s.round().await;
        assert!(!r.done.iter().any(|op| matches!(op, Op::MoveLocal { .. })), "{r:?}");
        waits += r.databases.iter().filter(|(op, _)| matches!(op, Op::MoveLocal { .. })).count();
    }
    assert!(old.exists() && !s.p("app.sqlite").exists(), "not moved beside another database's changes");
    assert!(s.p("notes.txt").exists() && !s.p("data.db-wal").exists(), "not renamed into a working file's name");
    assert!(waits >= 1, "said why: {waits}");
}

/// A database's `-wal` whose attributes can't be read isn't taken for
/// absent: the database is neither replaced nor sent; both are tried again.
#[tokio::test]
async fn a_wal_that_cant_be_looked_at_is_never_taken_for_absent() {
    let cloud = Fake::new();
    let mut s = Sync::new("sqlite-wal-unreadable", cloud.clone());
    let db = s.p("app.db");
    SqliteApp::start(&["hold", db.to_str().unwrap(), "3"]).close();
    s.settle().await;
    let ours = std::fs::read(&db).unwrap();
    let mut app = SqliteApp::start(&["hold", db.to_str().unwrap(), "5"]);
    assert_eq!(app.line(), "held");
    app.kill();
    cloud.edit("app.db", &ours[..ours.len() / 2]);
    s.exec.local().fail_metadata(Some("-wal"));
    for _ in 0..2 {
        let r = s.round().await;
        assert!(!r.done.iter().any(|op| matches!(op, Op::Download { name, .. } if name == "app.db")), "{r:?}");
    }
    assert_eq!(std::fs::read(&db).unwrap(), ours);

    // Sending: a database whose -wal can't be looked at isn't sent.
    let cloud = Fake::new();
    let mut s = Sync::new("sqlite-wal-unreadable-up", cloud.clone());
    let db = s.p("app.db");
    let mut app = SqliteApp::start(&["hold", db.to_str().unwrap(), "5"]);
    assert_eq!(app.line(), "held");
    s.exec.local().fail_metadata(Some("-wal"));
    for _ in 0..2 {
        s.round().await;
    }
    assert!(cloud.sent("app.db").is_empty());
    app.close();
}

/// A database recognised by its name whose content has no SQLite
/// signature (encrypted, as SQLCipher's) is still protected: with changes
/// in its `-wal`, it's neither replaced (waiting off, too) nor recycled.
#[tokio::test]
async fn an_encrypted_database_known_by_its_name_keeps_its_wal() {
    use std::os::windows::process::CommandExt;
    let cloud = Fake::new();
    let mut s = Sync::new("sqlite-encrypted", cloud.clone());
    let db = s.p("app.sqlite");
    let cipher: Vec<u8> = (0..4096u32).map(|i| (i.wrapping_mul(2_654_435_761) >> 13) as u8).collect();
    write(&db, &cipher);
    s.settle().await;
    let wal = s.p("app.sqlite-wal");
    let st = std::process::Command::new("cmd").raw_arg(format!("/C echo changes>\"{}\"", wal.display())).status();
    assert!(st.unwrap().success());

    s.exec.set_databases(oneup_rules::database::Databases::off());
    cloud.edit("app.sqlite", b"a newer version");
    let r = s.round().await;
    assert!(db_waits(&r, "app.sqlite").iter().any(|(w, _)| *w == oneup_effects::Wait::Wal), "{r:?}");
    assert_eq!(std::fs::read(&db).unwrap(), cipher);

    s.exec.set_databases(oneup_rules::database::Databases::default());
    cloud.delete("app.sqlite");
    for _ in 0..2 {
        let r = s.round().await;
        assert!(!r.done.iter().any(|op| matches!(op, Op::RecycleLocal { .. })), "{r:?}");
    }
    assert_eq!(std::fs::read(&db).unwrap(), cipher);
}

/// A downloaded Outlook data file under an ordinary name is known by its
/// content, so the agent can warn about it.
#[tokio::test]
async fn a_downloaded_outlook_file_is_known_by_its_content() {
    let cloud = Fake::new();
    let mut pst = b"!BDN".to_vec();
    pst.extend(std::iter::repeat_n(0x22, 600));
    cloud.add("mail.dat", Some(&pst));
    let mut s = Sync::new("db-downloaded-pst", cloud.clone());
    let r = s.round().await;
    let known = r.recognised.iter().any(|(op, f)| {
        matches!(op, Op::Download { name, .. } if name == "mail.dat") && *f == oneup_rules::database::Format::Outlook
    });
    assert!(known, "{r:?}");
}

/// A `!` pattern added, with nothing else changing, plans every folder
/// again: a working file it opts out syncs from then on.
#[tokio::test]
async fn a_database_opt_out_added_unparks_its_working_files_at_once() {
    let cloud = Fake::new();
    let mut s = Sync::new("db-pattern-change", cloud.clone());
    write(&s.p("books.ledger"), b"2026-10-01 coffee 3.50\n");
    write(&s.p("books.ledger-wal"), b"");
    s.settle().await;
    assert!(!cloud.tree().contains_key("/books.ledger-wal"), "parked beside its namesake");
    s.exec.set_databases(oneup_rules::database::Databases::new(true, &[], &["!books.ledger"]).unwrap());
    s.round().await;
    assert!(cloud.tree().contains_key("/books.ledger-wal"), "synced as soon as it's opted out");
}

/// The names a move uses are judged each on its own: a file a `!` pattern
/// opts out where it is still isn't renamed to a database's name beside
/// changes waiting there (SQLite would apply them to it).
#[tokio::test]
async fn an_opted_out_file_isnt_renamed_into_a_databases_name_beside_changes() {
    let cloud = Fake::new();
    let mut s = Sync::new("db-opted-out-move", cloud.clone());
    s.exec.set_databases(oneup_rules::database::Databases::new(true, &[], &["!old.txt"]).unwrap());
    write(&s.p("old.txt"), b"notes, not a database");
    write(&s.p("app.db-wal"), b"another database's changes");
    s.settle().await;
    cloud.rename("old.txt", "app.db");
    let mut waits = 0;
    for _ in 0..2 {
        let r = s.round().await;
        assert!(!r.done.iter().any(|op| matches!(op, Op::MoveLocal { .. })), "{r:?}");
        waits += r.databases.iter().filter(|(op, _)| matches!(op, Op::MoveLocal { .. })).count();
    }
    assert!(s.p("old.txt").exists() && !s.p("app.db").exists());
    assert!(waits > 0, "said why");
}

/// Another provider's placeholder, hydrated, is never read: not to send
/// it, nor to tell whether it's a database. (Simulated: a file another
/// program makes here, made a placeholder with an identity that isn't
/// OneUpDrive's.)
#[tokio::test]
async fn another_providers_placeholder_is_never_read() {
    use std::os::windows::process::CommandExt;
    let cloud = Fake::new();
    let (od, mut s) = on_demand_sync("db-foreign-placeholder", &cloud);
    s.quiesce().await;
    let p = s.p("theirs.data");
    let st = std::process::Command::new("cmd").raw_arg(format!("/C echo plain text>\"{}\"", p.display())).status();
    assert!(st.unwrap().success());
    provider::Exclusive::open(&p).unwrap().convert(b"another provider's identity").unwrap();
    for _ in 0..2 {
        let r = s.round().await;
        assert!(!r.done.iter().any(|op| matches!(op, Op::Upload { name, .. } if name == "theirs.data")), "{r:?}");
    }
    assert!(cloud.sent("theirs.data").is_empty(), "never read to be sent");
    drop(s);
    drop(od);
}

/// A database a pattern names (its content no SQLite signature) is read
/// with its `-wal` and `-journal` watched, waiting for programs or not: a
/// `-wal` with changes created while it's read cancels the upload.
#[tokio::test]
async fn a_database_named_by_a_pattern_is_read_with_its_wal_watched() {
    use std::os::windows::process::CommandExt;
    for wait in [true, false] {
        let cloud = Fake::new();
        let mut s = Sync::new(&format!("db-pattern-watch-{wait}"), cloud.clone());
        let dbs = oneup_rules::database::Databases::new(wait, &[], &["*.ledger"]).unwrap();
        s.exec.set_databases(dbs);
        s.settle().await;
        write(&s.p("books.ledger"), &[0x5Au8; 600]);
        let wal = s.p("books.ledger-wal");
        cloud.0.lock().unwrap().during_upload = Some(Box::new(move || {
            let st = std::process::Command::new("cmd").raw_arg(format!("/C echo x>\"{}\"", wal.display())).status();
            assert!(st.unwrap().success());
        }));
        let r = s.round().await;
        let changed = r.skipped.iter().any(|(op, k)| {
            matches!(op, Op::Upload { name, .. } if name == "books.ledger") && *k == oneup_exec::Skip::Changed
        });
        assert!(changed, "wait {wait}: {r:?}");
        assert!(cloud.sent("books.ledger").is_empty(), "wait {wait}");
    }
}

/// A new database from OneDrive isn't placed beside a `-wal` with changes
/// already here, wherever it came from (here, synced before as an ordinary
/// file, with no database beside it then): SQLite would apply it to the
/// database.
#[tokio::test]
async fn a_new_database_isnt_placed_beside_an_older_wal() {
    let cloud = Fake::new();
    let mut s = Sync::new("sqlite-older-wal", cloud.clone());
    write(&s.p("app.db-wal"), b"an older database's changes");
    s.settle().await;
    assert!(cloud.tree().contains_key("/app.db-wal"), "synced while nothing was beside it");
    let made = Tmp(std::env::temp_dir().join(format!("oneup-exec-sqlite-made5-{}", std::process::id())));
    let _ = std::fs::remove_dir_all(&made.0);
    std::fs::create_dir_all(&made.0).unwrap();
    let from = made.0.join("app.db");
    SqliteApp::start(&["keep", from.to_str().unwrap(), "delete"]).close();
    cloud.add("app.db", Some(&std::fs::read(&from).unwrap()));
    let r = s.round().await;
    assert!(!s.p("app.db").exists(), "not placed beside a -wal with changes: {r:?}");
    assert!(db_waits(&r, "app.db").iter().any(|(w, _)| *w == oneup_effects::Wait::Wal), "{r:?}");
}
