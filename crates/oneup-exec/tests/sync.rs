//! End-to-end two-way sync of a real temp folder with an in-memory cloud:
//! scanner → observer → planner → gate → effects/executor, round after
//! round, until both sides agree. Never touches a user's folder or account.
#![cfg(windows)]
#![allow(clippy::unwrap_used)]

use std::collections::{BTreeMap, BTreeSet};
use std::io::{Read, Write};
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use oneup_effects::LocalFs;
use oneup_exec::{CRASHED, Cloud, CloudError, Executor, Meta, MetaDone, Report, VersionMark, link_roots};
use oneup_gate::{Choice, Context, Decision, Overflow, OverflowOutcome, Policy, review_in};
use oneup_local::observe::Observer;
use oneup_local::{Config, Scan};
use oneup_plan::{Parked, names};
use oneup_rules::{ExcludeSet, RuleSet};
use oneup_store::{Kind, RemoteRow, Store};
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
    /// No room left: uploads are refused (as with HTTP 507), and counted.
    full: bool,
    refused: u64,
}

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
    fn create(&mut self, parent: &str, name: &str, folder: bool, content: Vec<u8>) -> String {
        let id = format!("r{}", self.bump());
        let etag = self.bump();
        self.items.insert(id.clone(), Item { parent: Some(parent.into()), name: name.into(), folder, content, etag });
        id
    }
}

fn qxh(b: &[u8]) -> [u8; 20] {
    let mut h = oneup_hash::QuickXorHash::new();
    h.update(b);
    h.finalize()
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
        let old = std::mem::replace(&mut it.content, content.to_vec());
        it.etag = etag;
        s.kept.push(old);
    }
    fn rename(&self, path: &str, name: &str) {
        let mut s = self.0.lock().unwrap();
        let id = resolve(&s, path);
        let etag = s.bump();
        let it = s.items.get_mut(&id).unwrap();
        it.name = name.into();
        it.etag = etag;
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
        s.kept.push(it.content);
        s.deleted.push(id);
    }

    /// What delta would deliver: every live item (parents first), then the
    /// deletions since last time.
    fn pull(&self, store: &mut Store) {
        let mut s = self.0.lock().unwrap();
        let mut order = vec!["root".to_string()];
        let mut i = 0;
        while i < order.len() {
            let p = order[i].clone();
            order.extend(s.items.iter().filter(|(_, it)| it.parent.as_deref() == Some(&p)).map(|(id, _)| id.clone()));
            i += 1;
        }
        let rows: Vec<RemoteRow> = order.iter().map(|id| s.row(id)).collect();
        let deleted = std::mem::take(&mut s.deleted);
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

impl Cloud for Fake {
    async fn download(&self, pre: &RemotePre, out: &mut (dyn Write + Send)) -> Result<Option<[u8; 20]>, CloudError> {
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

    async fn upload(
        &self,
        parent: &RemoteId,
        name: &str,
        replace: Option<&RemotePre>,
        size: i64,
        src: &mut (dyn Read + Send),
    ) -> Result<(RemoteRow, Option<[u8; 20]>), CloudError> {
        let mut content = Vec::new();
        src.read_to_end(&mut content).map_err(|e| CloudError::Other(e.to_string()))?;
        assert_eq!(content.len() as i64, size);
        let mut s = self.0.lock().unwrap();
        if s.full {
            s.refused += 1;
            return Err(CloudError::Full);
        }
        s.uploaded += content.len() as u64;
        let id = match replace {
            Some(pre) => {
                s.check(pre)?;
                if let Some(theirs) = s.race.take() {
                    drop(s);
                    self.edit_id(&pre.id.0, &theirs);
                    s = self.0.lock().unwrap();
                }
                let etag = s.bump();
                let it = s.items.get_mut(&pre.id.0).unwrap();
                let over = it.etag;
                let old = std::mem::replace(&mut it.content, content);
                it.etag = etag;
                s.kept.push(old.clone());
                s.old.insert(over, old);
                s.replaced.insert(pre.id.0.clone(), over);
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

    async fn copy(
        &self,
        source: &RemotePre,
        hash: &[u8; 20],
        parent: &RemoteId,
        name: &str,
    ) -> Result<RemoteRow, CloudError> {
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
    }

    async fn mkdir(&self, parent: &RemoteId, name: &str) -> Result<RemoteRow, CloudError> {
        let mut s = self.0.lock().unwrap();
        if !s.items.get(&parent.0).is_some_and(|p| p.folder) {
            return Err(CloudError::Changed);
        }
        if s.child(&parent.0, name).is_some() {
            return Err(CloudError::NameTaken);
        }
        let id = s.create(&parent.0, name, true, vec![]);
        Ok(s.row(&id))
    }

    async fn move_item(&self, pre: &RemotePre, to: &RemoteId, name: &str) -> Result<RemoteRow, CloudError> {
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
    }

    async fn item(&self, id: &RemoteId) -> Result<Option<RemoteRow>, CloudError> {
        let s = self.0.lock().unwrap();
        Ok(s.items.contains_key(&id.0).then(|| s.row(&id.0)))
    }

    /// Every replacement is checked afterwards, as a large one over Graph.
    async fn version_mark(&self, pre: &RemotePre, _: i64) -> Result<Option<VersionMark>, CloudError> {
        let s = self.0.lock().unwrap();
        let it = s.check(pre)?;
        Ok(Some(VersionMark { id: it.etag.to_string(), modified_ms: None, size: Some(it.content.len() as i64) }))
    }

    async fn replaced_other(&self, id: &RemoteId, mark: &VersionMark) -> Result<Option<VersionMark>, CloudError> {
        let s = self.0.lock().unwrap();
        let over = s.replaced.get(&id.0).filter(|e| e.to_string() != mark.id);
        Ok(over.map(|e| VersionMark { id: e.to_string(), modified_ms: None, size: Some(s.old[e].len() as i64) }))
    }

    async fn download_version(
        &self,
        _: &RemoteId,
        version: &str,
        out: &mut (dyn Write + Send),
    ) -> Result<u64, CloudError> {
        let s = self.0.lock().unwrap();
        if s.versions_fail {
            return Err(CloudError::Other("HTTP 500".into()));
        }
        let content = s.old.get(&version.parse().unwrap()).cloned();
        drop(s);
        let content = content.ok_or_else(|| CloudError::Other("no such version".into()))?;
        out.write_all(&content)?;
        Ok(content.len() as u64)
    }

    /// Three at a time, so a folder takes several listings.
    async fn children(&self, id: &RemoteId) -> Result<Vec<RemoteRow>, CloudError> {
        let mut s = self.0.lock().unwrap();
        if let Some((item, to)) = s.wander.take() {
            let etag = s.bump();
            let it = s.items.get_mut(&item).unwrap();
            it.parent = Some(to);
            it.etag = etag;
        }
        let ids = s.items.iter().filter(|(_, it)| it.parent.as_deref() == Some(&id.0)).map(|(id, _)| id).take(3);
        Ok(ids.map(|id| s.row(id)).collect())
    }

    async fn prune(&self, pre: &RemotePre) -> Result<(), CloudError> {
        let mut s = self.0.lock().unwrap();
        s.check(pre)?;
        let mut gone = vec![pre.id.0.clone()];
        let mut i = 0;
        while i < gone.len() {
            let p = gone[i].clone();
            gone.extend(s.items.iter().filter(|(_, it)| it.parent.as_deref() == Some(&p)).map(|(id, _)| id.clone()));
            i += 1;
        }
        for id in gone {
            let it = s.items.remove(&id).unwrap();
            s.kept.push(it.content);
            s.deleted.push(id);
        }
        Ok(())
    }

    /// As Graph's batch: every change judged against the state before any
    /// of them (a folder still holds what another change in the batch
    /// removes), then applied in order.
    async fn batch(&self, ops: &[Meta]) -> Vec<Result<MetaDone, CloudError>> {
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
                (Meta::Mkdir { parent, name }, Ok(())) => self.mkdir(parent, name).await.map(MetaDone::Item),
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
        out
    }

    async fn delete(&self, pre: &RemotePre) -> Result<(), CloudError> {
        let mut s = self.0.lock().unwrap();
        s.check(pre)?;
        if s.items.values().any(|it| it.parent.as_deref() == Some(&pre.id.0)) {
            return Err(CloudError::NotEmpty);
        }
        let it = s.items.remove(&pre.id.0).unwrap();
        s.kept.push(it.content);
        s.deleted.push(pre.id.0.clone());
        Ok(())
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
            planned_rules: RuleSet::default().fingerprint(),
            pull: false,
        }
    }

    fn start(dir: &Path, store: Store, cloud: &Fake) -> (Observer, Executor<Fake>) {
        let rules = ExcludeSet::none();
        let root = store.read(|o| o.local_root()).unwrap().unwrap();
        let fs = LocalFs::open(dir, root.volume_serial, root.root_file_id, "test-token", true).unwrap();
        let obs = Observer::open(store, Config::new(rules.clone())).unwrap();
        (obs, Executor::new(fs, cloud.clone(), rules))
    }

    /// The process was killed and starts again: everything in memory is
    /// gone; the database, the folder and the cloud stay as they are.
    fn restart(self) -> Self {
        let Self { dir, db, obs, exec, cloud, policy, room, decisions, overflow: _, rules, planned_rules, pull } = self;
        drop((obs, exec));
        let store = Store::open(&db.as_ref().expect("restart needs a database on disk").0, 4096).unwrap();
        let (obs, exec) = Self::start(&dir.0, store, &cloud);
        Self { dir, db, obs, exec, cloud, policy, room, decisions, overflow: None, rules, planned_rules, pull }
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
        let store = self.obs.store_mut();
        self.cloud.pull(store);
        // Only the folders the store marked dirty are planned, as the
        // background agent does; `settle` checks a full plan agrees.
        let fingerprint = self.rules.fingerprint();
        let all = fingerprint != self.planned_rules;
        let rules = &self.rules;
        // As `oneup_app::sync` plans: in pull mode, excluded items stay
        // visible and what would send them is dropped.
        let hide = if self.pull { rules.upload_only() } else { rules.clone() };
        let pull = self.pull;
        let (scope, ops) = store
            .read(|o| {
                let p = Parked::with_rules(o, &hide);
                let scope = o.dirty_scope()?;
                let folders = if all { all_folders(&p)? } else { scope.folders.clone() };
                let mut ops = plan(&p, folders)?;
                if pull {
                    let judge = Parked::with_rules(o, rules);
                    let mut kept = Vec::new();
                    for op in ops {
                        if !judge.sends_excluded(&op)? {
                            kept.push(op);
                        }
                    }
                    ops = kept;
                }
                Ok((scope, ops))
            })
            .unwrap();
        let cx = Context { room: self.room, decisions: &self.decisions, ..Context::default() };
        let r = review_in(&self.policy, &cx, ops);
        assert!(r.breakers.is_empty(), "{:?}", r.breakers);
        self.overflow = r.overflow.clone();
        let awaiting = r.overflow.as_ref().is_some_and(|o| o.outcome == OverflowOutcome::Held);
        let planned = r.approved.len();
        let counted = std::sync::atomic::AtomicUsize::new(0);
        let count = |n: usize| {
            counted.fetch_add(n, std::sync::atomic::Ordering::Relaxed);
        };
        let report = self.exec.run_checked(store, r.approved, &count, may_run).await;
        // Progress counts every operation once, batched or not.
        if report.halted.is_none() {
            let counted = counted.into_inner();
            assert_eq!(counted, planned, "progress count");
            let ended = report.done.len() + report.skipped.len() + report.failed.len() + report.not_run.len();
            assert_eq!(counted, ended);
        }
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
                let p = Parked::with_rules(o, &self.rules);
                plan(&p, all_folders(&p)?)
            })
            .unwrap();
        ops.iter().any(|op| op.is_action())
    }

    /// Rounds until both sides agree and a round changes nothing. Delete
    /// records can reach the journal shortly after the call returns, so an
    /// idle round with the sides still differing waits and tries again.
    async fn settle(&mut self) -> usize {
        for n in 1..=40 {
            let report = self.round().await;
            if n > 30 {
                eprintln!("round {n}: {report:?}");
            }
            if report.is_idle() && report.skipped.is_empty() {
                if self.synced_part(local_tree(&self.dir.0)) == self.synced_part(self.cloud.tree()) {
                    let problems = self.obs.store().read(|o| o.check_invariants()).unwrap();
                    assert!(problems.is_empty(), "{problems:?}");
                    assert!(!self.full_plan_has_actions(), "dirty-folder planning missed work a full pass finds");
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
fn syncable(t: Tree) -> Tree {
    t.into_iter()
        .filter(|(path, _)| {
            let name = path.rsplit('/').next().unwrap_or_default();
            !(names::is_invalid(name) || names::not_synced(name).is_some())
        })
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

/// An edit an upload went over that can't be kept as a new file (its
/// version won't download): the upload is still done, counted as such and
/// not as a failure, and the edit is reported as not kept. It stays in the
/// file's version history.
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
    s.settle().await;
    for t in [local_tree(&s.dir.0), cloud.tree()] {
        assert_eq!(t["/notes.txt"].as_deref(), Some(&b"mine"[..]));
        assert_eq!(t.len(), 1, "{:?}", names(&t));
    }
    assert!(cloud.0.lock().unwrap().old.values().any(|v| v == b"theirs"), "in the version history");
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

/// A full OneDrive refuses an upload: the round sends no other upload
/// (each is skipped, not failed, so it's tried again once there's room),
/// and downloads go on.
#[tokio::test]
async fn a_full_onedrive_stops_the_rounds_uploads_while_downloads_go_on() {
    let cloud = Fake::new();
    let mut s = Sync::new("full", cloud.clone());
    write(&s.p("old.txt"), b"synced before");
    s.settle().await;
    for name in ["a.txt", "b.txt", "c.txt"] {
        write(&s.p(name), name.as_bytes());
    }
    cloud.add("d.txt", Some(b"from the cloud"));
    cloud.0.lock().unwrap().full = true;
    let refused = |c: &Fake| c.0.lock().unwrap().refused;
    let r = s.round().await;
    let full = r.skipped.iter().filter(|(op, k)| *k == oneup_exec::Skip::CloudFull && matches!(op, Op::Upload { .. }));
    assert_eq!(full.count(), 3, "{r:?}");
    assert_eq!(refused(&cloud), 1, "only the first upload was sent");
    assert!(r.done.iter().any(|op| matches!(op, Op::Download { .. })), "{r:?}");
    assert_eq!(std::fs::read(s.p("d.txt")).unwrap(), b"from the cloud");

    // Room again: they go.
    cloud.0.lock().unwrap().full = false;
    s.settle().await;
    let t = cloud.tree();
    assert_eq!(t["/b.txt"].as_deref(), Some(&b"b.txt"[..]));
    assert_eq!(refused(&cloud), 1);
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
    b"base",
    b"mine",
    b"theirs",
];

/// Synced state, then one change of every kind on both sides: what the
/// next round turns into uploads, downloads (new and replacing), folders
/// on both sides, moves both ways, deletions both ways (one of them a
/// local recycle) and a conflict copy.
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
    std::fs::remove_file(s.p("gone.txt")).unwrap();
    std::fs::rename(s.p("docs/move.txt"), s.p("moved.txt")).unwrap();
    write(&s.p("both.txt"), b"mine");
    cloud.add("cnew.txt", Some(b"cloud new"));
    cloud.add("cdir", None);
    cloud.add("cdir/x.txt", Some(b"x"));
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
        assert_eq!(store.read(|o| o.meta_get("exec.pending")).unwrap(), None, "crash point {n}");
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
        for c in [&b"x"[..], b"alpha", b"mine", b"theirs"] {
            assert!(here.contains(&c), "crash point {n}: {:?} isn't in the folder", String::from_utf8_lossy(c));
        }
        for c in [&b"local delete me"[..], b"cloud delete me"] {
            assert!(!here.contains(&c), "crash point {n}: {:?} came back", String::from_utf8_lossy(c));
        }
        assert!(t.contains_key("/moved.txt") && t.contains_key("/c2.txt"), "crash point {n}: {:?}", names(&t));
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
    /// What `oneup cloud prune --yes` does for the cloud item at `path`:
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
        let op = Op::PruneRemote { remote: RemotePre { id: RemoteId(id), etag }, forget, items: 2, keep };
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
        provider::register(path, b"test-token").unwrap();
        let staging = PathBuf::from(format!("{}.staging", path.display()));
        let h = Hydrator::new(
            cloud.clone(),
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
    let dir = Tmp(std::env::temp_dir().join(format!("oneup-exec-{tag}-{}", std::process::id())));
    let _ = std::fs::remove_dir_all(&dir.0);
    std::fs::create_dir_all(&dir.0).unwrap();
    let db = PathBuf::from(format!("{}.db", dir.0.display()));
    drop(DbFile(db.clone()));
    drop(Store::open(&db, 1024).unwrap());
    let od = OnDemand::new(&dir.0, &db, cloud);
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
    let fetched = oneup_exec::placement::fetch_pinned(s.obs.store(), &s.dir.0).await.unwrap();
    assert_eq!(fetched, ["/keep.txt"]);
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
