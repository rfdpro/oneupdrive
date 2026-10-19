//! End-to-end tests of the mirror against an in-memory fake OneDrive.
//!
//! The fake reproduces the behaviours measured on OneDrive Personal: paged
//! responses, folders re-emitted in delta feeds, delta returning the
//! *current* state of changed items, moved folders reported without their
//! contents, and deletes reported for the top item only. It can also deliver
//! entries out of order and fail mid-run.

// Test harness code: panicking on unexpected states is the point.
#![allow(clippy::unwrap_used, clippy::type_complexity)]

use std::collections::{BTreeMap, BTreeSet};
use std::sync::Arc;
use std::sync::Mutex;
use std::sync::atomic::{AtomicBool, AtomicI64, Ordering::SeqCst};

use oneup_graph::model::{Facet, FileFacet, FolderFacet, ParentReference};
use oneup_graph::{DriveItem, Page};
use oneup_remote::{Config, Error, Mirror, RemoteApi, SplitHint};
use oneup_rules::ExcludeSet;
use oneup_store::{Kind, Store};

#[derive(Debug, Clone)]
struct Node {
    parent: Option<String>,
    name: String,
    kind: Kind,
}

#[derive(Default)]
struct Inner {
    nodes: BTreeMap<String, Node>,
    /// (seq, id) of every change, for catch-up deltas.
    log: Vec<(u64, String)>,
    seq: u64,
    next_id: u64,
    /// Changes applied right after the next `latest_cursor` (i.e. during enumeration).
    during_enumeration: Vec<Box<dyn FnOnce(&mut Inner) + Send>>,
    /// Human-readable record of mutations, printed when a simulation fails.
    trace: Vec<String>,
}

impl Inner {
    fn touch(&mut self, id: &str) {
        self.seq += 1;
        self.log.push((self.seq, id.to_string()));
    }

    fn add(&mut self, parent: &str, name: &str, kind: Kind) -> String {
        self.next_id += 1;
        let id = format!("I{}", self.next_id);
        self.nodes.insert(id.clone(), Node { parent: Some(parent.into()), name: name.into(), kind });
        self.touch(&id);
        self.trace.push(format!("seq {} add {id} {name:?} {kind:?} under {parent}", self.seq));
        id
    }

    fn delete(&mut self, id: &str) {
        for d in self.subtree(id) {
            self.nodes.remove(&d);
        }
        self.touch(id); // like Graph: only the top item is reported
        self.trace.push(format!("seq {} delete {id}", self.seq));
    }

    fn mv(&mut self, id: &str, new_parent: &str) {
        if let Some(n) = self.nodes.get_mut(id) {
            n.parent = Some(new_parent.into());
        }
        self.touch(id);
        self.trace.push(format!("seq {} move {id} -> {new_parent}", self.seq));
    }

    fn rename(&mut self, id: &str, name: &str) {
        if let Some(n) = self.nodes.get_mut(id) {
            n.name = name.into();
        }
        self.touch(id);
        self.trace.push(format!("seq {} rename {id} -> {name:?}", self.seq));
    }

    fn children(&self, id: &str) -> Vec<String> {
        self.nodes.iter().filter(|(_, n)| n.parent.as_deref() == Some(id)).map(|(k, _)| k.clone()).collect()
    }

    /// Parent-first (BFS) listing of a subtree including its top.
    fn subtree(&self, id: &str) -> Vec<String> {
        let mut out = vec![id.to_string()];
        let mut i = 0;
        while i < out.len() {
            let kids = self.children(&out[i]);
            out.extend(kids);
            i += 1;
        }
        out
    }

    fn item(&self, id: &str) -> Option<DriveItem> {
        let n = self.nodes.get(id)?;
        let child_count = self.children(id).len() as i64;
        Some(DriveItem {
            id: id.into(),
            name: Some(n.name.clone()),
            // Like Graph: folders report the recursive size of their contents.
            size: Some(match n.kind {
                Kind::File => 10,
                Kind::Folder | Kind::Root => {
                    10 * self.subtree(id).iter().filter(|d| self.nodes[*d].kind == Kind::File).count() as i64
                }
                Kind::Package => 0,
            }),
            e_tag: Some(format!("etag-{id}")),
            parent_reference: n.parent.as_ref().map(|p| ParentReference { id: Some(p.clone()), path: None }),
            file: (n.kind == Kind::File).then(FileFacet::default),
            folder: matches!(n.kind, Kind::Folder | Kind::Root)
                .then_some(FolderFacet { child_count: Some(child_count) }),
            package: (n.kind == Kind::Package).then(Facet::default),
            deleted: None,
            root: (n.kind == Kind::Root).then(Facet::default),
            file_system_info: None,
        })
    }

    fn deleted(id: &str) -> DriveItem {
        DriveItem {
            id: id.into(),
            name: None,
            size: None,
            e_tag: None,
            parent_reference: None,
            file: None,
            folder: None,
            package: None,
            deleted: Some(Facet::default()),
            root: None,
            file_system_info: None,
        }
    }
}

struct Fake {
    inner: Mutex<Inner>,
    page_size: usize,
    /// Deliver delta entries in a scrambled (but deterministic) order.
    shuffle: bool,
    /// Fail every request once this many pages have been served (`-1` = never).
    fail_after: AtomicI64,
    served: AtomicI64,
    /// Refuse id-addressed folder delta (400), so the path form is used.
    refuse_id_delta: AtomicBool,
    /// Links that have expired (410).
    expired: Mutex<BTreeSet<String>>,
}

impl Fake {
    fn new(page_size: usize, shuffle: bool) -> Self {
        let mut inner = Inner::default();
        inner.nodes.insert("ROOT".into(), Node { parent: None, name: "root".into(), kind: Kind::Root });
        Self {
            inner: Mutex::new(inner),
            page_size,
            shuffle,
            fail_after: AtomicI64::new(-1),
            served: AtomicI64::new(0),
            refuse_id_delta: AtomicBool::new(false),
            expired: Mutex::new(BTreeSet::new()),
        }
    }

    fn with<T>(&self, f: impl FnOnce(&mut Inner) -> T) -> T {
        f(&mut self.inner.lock().unwrap())
    }

    fn paged(&self, items: Vec<DriveItem>, skip: usize, url_base: &str, end_link: Option<String>) -> Page {
        let end = (skip + self.page_size).min(items.len());
        let value = items[skip.min(items.len())..end].to_vec();
        let more = end < items.len();
        Page {
            value,
            next_link: more.then(|| format!("{url_base}&skip={end}")),
            delta_link: if more { None } else { end_link },
        }
    }

    /// Delta-style entries for a subtree: parent-first, with every folder
    /// re-emitted after its children (as Graph does), optionally scrambled.
    fn delta_entries(&self, inner: &Inner, top: &str) -> Vec<DriveItem> {
        let mut ids = inner.subtree(top);
        let folders: Vec<String> =
            ids.iter().filter(|i| matches!(inner.nodes.get(*i).map(|n| n.kind), Some(Kind::Folder))).cloned().collect();
        ids.extend(folders);
        if self.shuffle {
            // Deterministic scramble: reverse chunks of 7.
            ids = ids.chunks(7).rev().flat_map(|c| c.iter().rev().cloned()).collect();
        }
        ids.iter().filter_map(|i| inner.item(i)).collect()
    }
}

fn parse(url: &str) -> (String, String, usize) {
    // fake://<kind>/<arg>?x&skip=N
    let rest = url.strip_prefix("fake://").unwrap();
    let (path, query) = rest.split_once('?').unwrap_or((rest, ""));
    let (kind, arg) = path.split_once('/').unwrap();
    let skip = query.split('&').find_map(|kv| kv.strip_prefix("skip=")).map_or(0, |s| s.parse().unwrap());
    (kind.to_string(), arg.to_string(), skip)
}

impl RemoteApi for Fake {
    async fn root(&self) -> oneup_graph::Result<DriveItem> {
        Ok(self.with(|i| i.item("ROOT")).unwrap())
    }

    async fn item(&self, id: &str) -> oneup_graph::Result<Option<DriveItem>> {
        Ok(self.with(|i| i.item(id)))
    }

    async fn page(&self, url: &str) -> oneup_graph::Result<Page> {
        let limit = self.fail_after.load(SeqCst);
        if limit >= 0 && self.served.load(SeqCst) >= limit {
            return Err(oneup_graph::Error::Status { status: 500, what: "fake".into(), body: "injected".into() });
        }
        self.served.fetch_add(1, SeqCst);
        if self.expired.lock().unwrap().contains(url) {
            return Err(oneup_graph::Error::Status { status: 410, what: "link".into(), body: String::new() });
        }
        let (kind, arg, skip) = parse(url);
        let mut inner = self.inner.lock().unwrap();
        let base = url.split("&skip=").next().unwrap().to_string();
        match kind.as_str() {
            "delta" if skip == 0 && self.refuse_id_delta.load(SeqCst) => {
                Err(oneup_graph::Error::Status { status: 400, what: arg, body: String::new() })
            }
            "path" => {
                // Resolved at request time, like Graph: a stale path finds nothing.
                let mut at = Some("ROOT".to_string());
                for seg in arg.split('/').filter(|s| !s.is_empty()) {
                    at = at.and_then(|p| inner.children(&p).into_iter().find(|c| inner.nodes[c].name == seg));
                }
                let Some(top) = at else {
                    return Err(oneup_graph::Error::Status { status: 404, what: arg, body: String::new() });
                };
                let entries = self.delta_entries(&inner, &top);
                Ok(self.paged(entries, skip, &base, Some("fake://unused/x?".into())))
            }
            "children" => {
                if !inner.nodes.contains_key(&arg) {
                    return Err(oneup_graph::Error::Status { status: 404, what: arg, body: String::new() });
                }
                let kids: Vec<DriveItem> = inner.children(&arg).iter().filter_map(|c| inner.item(c)).collect();
                Ok(self.paged(kids, skip, &base, None))
            }
            "delta" => {
                if !inner.nodes.contains_key(&arg) {
                    return Err(oneup_graph::Error::Status { status: 404, what: arg, body: String::new() });
                }
                let entries = self.delta_entries(&inner, &arg);
                Ok(self.paged(entries, skip, &base, Some("fake://unused/x?".into())))
            }
            "changes" => {
                let since: u64 = arg.parse().unwrap();
                if skip == 0 {
                    let marker = format!("-- catch-up from seq {since} (now {})", inner.seq);
                    drop(inner);
                    self.with(|i| i.trace.push(marker));
                    inner = self.inner.lock().unwrap();
                }
                let mut seen = BTreeSet::new();
                let mut entries = Vec::new();
                for (seq, id) in &inner.log {
                    if *seq > since && seen.insert(id.clone()) {
                        entries.push(inner.item(id).unwrap_or_else(|| Inner::deleted(id)));
                    }
                }
                let end = Some(format!("fake://changes/{}?", inner.seq));
                Ok(self.paged(entries, skip, &base, end))
            }
            other => panic!("unexpected url kind {other}"),
        }
    }

    async fn latest_cursor(&self) -> oneup_graph::Result<String> {
        let mut inner = self.inner.lock().unwrap();
        let link = format!("fake://changes/{}?", inner.seq);
        let marker = format!("-- cursor taken at seq {}", inner.seq);
        inner.trace.push(marker);
        for f in std::mem::take(&mut inner.during_enumeration) {
            f(&mut inner);
        }
        Ok(link)
    }

    fn children_url(&self, folder_id: &str) -> String {
        format!("fake://children/{folder_id}?")
    }

    fn delta_by_id_url(&self, folder_id: &str) -> String {
        format!("fake://delta/{folder_id}?")
    }

    fn delta_by_path_url(&self, path: &str) -> String {
        format!("fake://path/{path}?")
    }
}

/// What the mirror should contain: (id, parent, name, kind, excluded).
fn expected(fake: &Fake, rules: &ExcludeSet) -> BTreeSet<(String, Option<String>, String, i64, bool)> {
    fake.with(|inner| {
        let mut out = BTreeSet::new();
        let mut stack = vec!["ROOT".to_string()];
        while let Some(id) = stack.pop() {
            let n = &inner.nodes[&id];
            let engine = n.parent.as_deref() == Some("ROOT") && oneup_rules::is_engine_dir(&n.name);
            let excluded = n.kind == Kind::Folder && (rules.excludes_folder(&n.name) || engine);
            out.insert((id.clone(), n.parent.clone(), n.name.clone(), n.kind as i64, excluded));
            if !excluded {
                stack.extend(inner.children(&id));
            }
        }
        out
    })
}

fn actual(m: &Mirror<Fake>) -> BTreeSet<(String, Option<String>, String, i64, bool)> {
    m.read(|o| o.all_items()).unwrap().into_iter().map(|(r, x)| (r.id, r.parent_id, r.name, r.kind as i64, x)).collect()
}

/// A drive shaped like a developer's: projects with dependency trees at
/// various depths, empty folders, a package, and nested junk inside junk.
fn populate(fake: &Fake) {
    fake.with(|i| {
        let docs = i.add("ROOT", "Documents", Kind::Folder);
        i.add(&docs, "notes.txt", Kind::File);
        i.add(&docs, "Empty", Kind::Folder);
        i.add(&docs, "Notebook", Kind::Package);
        // OneUpDrive's own folder name: excluded in the drive root only.
        let engine = i.add("ROOT", ".OneUp", Kind::Folder);
        i.add(&engine, "marker", Kind::File);
        let deep_engine = i.add(&docs, ".oneup", Kind::Folder);
        i.add(&deep_engine, "ordinary.txt", Kind::File);
        let code = i.add("ROOT", "Code", Kind::Folder);
        for p in 0..3 {
            let proj = i.add(&code, &format!("proj{p}"), Kind::Folder);
            let src = i.add(&proj, "src", Kind::Folder);
            for f in 0..4 {
                i.add(&src, &format!("f{f}.rs"), Kind::File);
            }
            // Junk below the list depth, so it is pruned in-feed.
            let nm = i.add(&proj, "node_modules", Kind::Folder);
            for d in 0..3 {
                let dep = i.add(&nm, &format!("dep{d}"), Kind::Folder);
                i.add(&dep, "index.js", Kind::File);
                let inner_nm = i.add(&dep, "node_modules", Kind::Folder);
                i.add(&inner_nm, "deep.js", Kind::File);
            }
            let venv = i.add(&proj, ".venv", Kind::Folder);
            let sp = i.add(&venv, "site-packages", Kind::Folder);
            i.add(&sp, "x.py", Kind::File);
        }
        // Junk at the top level, pruned before any delta is requested.
        let top_cache = i.add("ROOT", ".cache", Kind::Folder);
        i.add(&top_cache, "blob", Kind::File);
        // Content under never-enumerated junk that a later test moves out.
        let keep = i.add(&top_cache, "keep", Kind::Folder);
        let keep_sub = i.add(&keep, "sub", Kind::Folder);
        i.add(&keep_sub, "kept.txt", Kind::File);
    });
}

fn mirror(fake: &Arc<Fake>, store: Store, rules: ExcludeSet) -> Mirror<Fake> {
    mirror_with(fake, store, Config { list_depth: 2, rules, max_tasks: 4, ..Config::default() })
}

fn mirror_with(fake: &Arc<Fake>, store: Store, cfg: Config) -> Mirror<Fake> {
    Mirror::new(Arc::clone(fake), store, cfg)
}

fn temp_db(tag: &str) -> std::path::PathBuf {
    let dir = std::env::temp_dir().join(format!("oneup-remote-test-{tag}-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).unwrap();
    dir.join("state.db")
}

#[tokio::test(flavor = "multi_thread")]
async fn fresh_mirror_matches_drive_minus_junk() {
    let fake = Arc::new(Fake::new(5, false));
    populate(&fake);
    let m = mirror(&fake, Store::open_in_memory().unwrap(), ExcludeSet::dev());
    let s = m.run().await.unwrap();
    assert!(s.fresh);
    assert_eq!(actual(&m), expected(&fake, &ExcludeSet::dev()));
    assert!(s.stats.excluded_folders >= 7, "{:?}", s.stats);
    assert!(s.stats.pruned_folders > 0, "junk inside delta units leaves tombstones");
    assert_eq!(s.stats.orphans, 0);
}

#[tokio::test(flavor = "multi_thread")]
async fn scrambled_and_repeated_delivery_converges() {
    let fake = Arc::new(Fake::new(3, true));
    populate(&fake);
    let m = mirror(&fake, Store::open_in_memory().unwrap(), ExcludeSet::dev());
    m.run().await.unwrap();
    assert_eq!(actual(&m), expected(&fake, &ExcludeSet::dev()));
}

#[tokio::test(flavor = "multi_thread")]
async fn without_rules_everything_is_mirrored() {
    let fake = Arc::new(Fake::new(4, true));
    populate(&fake);
    let m = mirror(&fake, Store::open_in_memory().unwrap(), ExcludeSet::none());
    let s = m.run().await.unwrap();
    assert_eq!(actual(&m), expected(&fake, &ExcludeSet::none()));
    assert_eq!(s.stats.excluded_folders, 1, "only the engine folder in the drive root");
}

#[tokio::test(flavor = "multi_thread")]
async fn changes_during_enumeration_are_caught_up() {
    let fake = Arc::new(Fake::new(4, false));
    populate(&fake);
    fake.with(|i| {
        i.during_enumeration.push(Box::new(|i: &mut Inner| {
            let docs = i.nodes.iter().find(|(_, n)| n.name == "Documents").map(|(k, _)| k.clone()).unwrap();
            let added = i.add(&docs, "added-during.txt", Kind::File);
            i.rename(&added, "renamed-during.txt");
            let notes = i.nodes.iter().find(|(_, n)| n.name == "notes.txt").map(|(k, _)| k.clone()).unwrap();
            i.delete(&notes);
        }));
    });
    let m = mirror(&fake, Store::open_in_memory().unwrap(), ExcludeSet::dev());
    m.run().await.unwrap();
    assert_eq!(actual(&m), expected(&fake, &ExcludeSet::dev()));
}

#[tokio::test(flavor = "multi_thread")]
async fn incremental_changes_including_moves_across_exclusion_boundaries() {
    let fake = Arc::new(Fake::new(4, false));
    populate(&fake);
    let path = temp_db("incremental");
    let rules = ExcludeSet::dev();
    mirror(&fake, Store::open(&path, 1024).unwrap(), rules.clone()).run().await.unwrap();

    fake.with(|i| {
        let find =
            |i: &Inner, name: &str| i.nodes.iter().find(|(_, n)| n.name == name).map(|(k, _)| k.clone()).unwrap();
        let docs = find(i, "Documents");
        let code = find(i, "Code");
        // A new folder with content.
        let newf = i.add(&docs, "New", Kind::Folder);
        i.add(&newf, "a.txt", Kind::File);
        let child = |i: &Inner, parent: &str, name: &str| {
            i.children(parent).into_iter().find(|c| i.nodes[c].name == name).unwrap()
        };
        let (proj0, proj1, proj2) = (find(i, "proj0"), find(i, "proj1"), find(i, "proj2"));
        // Move an included folder into junk, and a junk folder (with content) out of junk.
        let src0 = child(i, &proj0, "src");
        let nm0 = child(i, &proj0, "node_modules");
        let dep0 = child(i, &nm0, "dep0");
        i.mv(&src0, &nm0);
        i.mv(&dep0, &docs);
        // Rename a normal folder to a junk name, and delete a whole project.
        let src1 = child(i, &proj1, "src");
        i.rename(&src1, "target");
        // A folder (with nested content) moved out of never-enumerated top-level junk.
        let cache = find(i, ".cache");
        let keep = child(i, &cache, "keep");
        i.mv(&keep, &docs);
        i.delete(&proj2);
        i.add(&code, "README.md", Kind::File);
    });

    let m = mirror(&fake, Store::open(&path, 1024).unwrap(), rules.clone());
    let s = m.run().await.unwrap();
    assert!(!s.fresh);
    assert_eq!(actual(&m), expected(&fake, &rules));
    drop(m);
    let _ = std::fs::remove_dir_all(path.parent().unwrap());
}

/// `update` (the agent's every round) leaves out the whole-store checks
/// `run` ends with: they read every row of all three trees, which made each
/// push notification cost hundreds of megabytes on a large drive. A planted
/// inconsistency shows which of the two looked.
#[tokio::test(flavor = "multi_thread")]
async fn update_brings_changes_without_checking_the_whole_store() {
    let fake = Arc::new(Fake::new(4, false));
    populate(&fake);
    let path = temp_db("update");
    let rules = ExcludeSet::dev();
    mirror(&fake, Store::open(&path, 1024).unwrap(), rules.clone()).run().await.unwrap();
    let dangling = oneup_store::RemoteRow {
        id: "planted".into(),
        parent_id: Some("no-such-folder".into()),
        name: "planted.txt".into(),
        kind: Kind::File,
        size: 1,
        etag: Some("e".into()),
        qxh: None,
        mtime_ms: None,
    };
    Store::open(&path, 1024).unwrap().write(|o| o.upsert_item(&dangling, false)).unwrap();
    fake.with(|i| {
        let docs = i.nodes.iter().find(|(_, n)| n.name == "Documents").map(|(k, _)| k.clone()).unwrap();
        i.add(&docs, "after.txt", Kind::File);
    });

    let m = mirror(&fake, Store::open(&path, 1024).unwrap(), rules.clone());
    assert!(!m.update().await.unwrap(), "continued from the cursor");
    assert!(m.read(|o| o.all_items()).unwrap().iter().any(|(r, _)| r.name == "after.txt"), "the change was brought");
    match m.run().await {
        Err(Error::Inconsistent(why)) => assert!(why.contains("parent is missing"), "{why}"),
        other => panic!("run should check the store: {other:?}"),
    }
    drop(m);
    let _ = std::fs::remove_dir_all(path.parent().unwrap());
}

/// What the agent runs at every round reads no table that grows with the
/// tree: whole-table counts here once cost every push notification
/// hundreds of megabytes of reads on a large drive.
#[tokio::test(flavor = "multi_thread")]
async fn an_update_reads_no_whole_table() {
    let fake = Arc::new(Fake::new(4, false));
    populate(&fake);
    let path = temp_db("no-scans");
    let rules = ExcludeSet::dev();
    mirror(&fake, Store::open(&path, 1024).unwrap(), rules.clone()).run().await.unwrap();
    fake.with(|i| {
        let docs = i.nodes.iter().find(|(_, n)| n.name == "Documents").map(|(k, _)| k.clone()).unwrap();
        i.add(&docs, "after.txt", Kind::File);
    });

    let mut store = Store::open(&path, 1024).unwrap();
    store.log_statements();
    let m = mirror(&fake, store, rules);
    m.update().await.unwrap();
    let statements = Store::take_statements();
    let store = m.into_store().unwrap();
    assert!(statements.iter().any(|s| s.contains("remote_item")), "the change was stored: {statements:#?}");
    let scans = store.tree_scans(&statements).unwrap();
    assert!(scans.is_empty(), "{scans:#?}");
    drop(store);
    let _ = std::fs::remove_dir_all(path.parent().unwrap());
}

#[tokio::test(flavor = "multi_thread")]
async fn rule_change_reincludes_and_reexcludes() {
    let fake = Arc::new(Fake::new(4, false));
    populate(&fake);
    let path = temp_db("rules");
    mirror(&fake, Store::open(&path, 1024).unwrap(), ExcludeSet::dev()).run().await.unwrap();

    let none = ExcludeSet::none();
    let m = mirror(&fake, Store::open(&path, 1024).unwrap(), none.clone());
    m.run().await.unwrap();
    assert_eq!(actual(&m), expected(&fake, &none));
    drop(m);

    let m = mirror(&fake, Store::open(&path, 1024).unwrap(), ExcludeSet::dev());
    m.run().await.unwrap();
    assert_eq!(actual(&m), expected(&fake, &ExcludeSet::dev()));
    drop(m);
    let _ = std::fs::remove_dir_all(path.parent().unwrap());
}

#[tokio::test(flavor = "multi_thread")]
async fn interrupted_run_resumes_to_the_same_result() {
    let fake = Arc::new(Fake::new(2, true));
    populate(&fake);
    let path = temp_db("resume");
    let rules = ExcludeSet::dev();

    fake.fail_after.store(6, SeqCst);
    let err = mirror(&fake, Store::open(&path, 1024).unwrap(), rules.clone()).run().await.unwrap_err();
    assert!(matches!(err, Error::Graph(_)), "{err:?}");

    fake.fail_after.store(-1, SeqCst);
    let m = mirror(&fake, Store::open(&path, 1024).unwrap(), rules.clone());
    let s = m.run().await.unwrap();
    assert!(!s.fresh, "second run resumes rather than starting over");
    assert_eq!(actual(&m), expected(&fake, &rules));
    drop(m);
    let _ = std::fs::remove_dir_all(path.parent().unwrap());
}

/// After a full run: forgets what the mirror stored below `/Code/proj0/src`
/// and queues it as a delta unit again, as if its enumeration were still to
/// come. Returns the folder's id.
async fn unenumerate_src(fake: &Arc<Fake>, path: &std::path::Path, rules: &ExcludeSet) -> String {
    mirror(fake, Store::open(path, 1024).unwrap(), rules.clone()).run().await.unwrap();
    let src = fake.with(|i| {
        let proj0 = i.nodes.iter().find(|(_, n)| n.name == "proj0").map(|(k, _)| k.clone()).unwrap();
        i.children(&proj0).into_iter().find(|c| i.nodes[c].name == "src").unwrap()
    });
    let id = src.clone();
    Store::open(path, 1024)
        .unwrap()
        .write(|o| {
            for (c, _) in o.remote_children(&id)? {
                o.delete_subtree(&c.id)?;
            }
            o.requeue_unit(&oneup_store::Unit {
                folder_id: id.clone(),
                path: "/Code/proj0/src".into(),
                kind: oneup_store::UnitKind::Delta,
                depth: 3,
                next_link: None,
                items: 0,
            })
        })
        .unwrap();
    src
}

/// A unit resumed from a saved link that has expired since (410) starts
/// over, instead of failing the same way every round.
#[tokio::test(flavor = "multi_thread")]
async fn an_expired_saved_link_starts_the_unit_over() {
    let fake = Arc::new(Fake::new(2, false));
    populate(&fake);
    let path = temp_db("expired-link");
    let rules = ExcludeSet::dev();
    let src = unenumerate_src(&fake, &path, &rules).await;
    // Graph's links carry a token: a fresh start gets a different one.
    let link = format!("fake://delta/{src}?&token=old&skip=2");
    fake.expired.lock().unwrap().insert(link.clone());
    Store::open(&path, 1024).unwrap().write(|o| o.set_unit_progress(&src, &link, 2)).unwrap();

    let m = mirror(&fake, Store::open(&path, 1024).unwrap(), rules.clone());
    m.run().await.unwrap();
    assert_eq!(actual(&m), expected(&fake, &rules));
    drop(m);
    let _ = std::fs::remove_dir_all(path.parent().unwrap());
}

/// Where id-addressed delta is refused, a unit lists its folder by the path
/// it was queued with. Renamed since, the folder isn't found there, but it
/// still exists: its contents are listed from where it is now (catch-up
/// reports only the rename). A folder gone by its id is done with.
#[tokio::test(flavor = "multi_thread")]
async fn a_folder_renamed_before_its_unit_runs_is_listed_where_it_is_now() {
    let fake = Arc::new(Fake::new(2, false));
    populate(&fake);
    let path = temp_db("renamed-unit");
    let rules = ExcludeSet::dev();
    let src = unenumerate_src(&fake, &path, &rules).await;
    fake.with(|i| i.rename(&src, "lib"));
    fake.refuse_id_delta.store(true, SeqCst);
    Store::open(&path, 1024)
        .unwrap()
        .write(|o| {
            o.put_unit(&oneup_store::Unit {
                folder_id: "gone".into(),
                path: "/Code/gone".into(),
                kind: oneup_store::UnitKind::Delta,
                depth: 2,
                next_link: None,
                items: 0,
            })
        })
        .unwrap();

    let m = mirror(&fake, Store::open(&path, 1024).unwrap(), rules.clone());
    m.run().await.unwrap();
    assert_eq!(actual(&m), expected(&fake, &rules));
    assert!(!m.read(|o| o.units_pending()).unwrap(), "both units are done");
    drop(m);
    let _ = std::fs::remove_dir_all(path.parent().unwrap());
}

/// Small deterministic PRNG (xorshift) so failures are reproducible by seed.
struct Rng(u64);

impl Rng {
    fn next(&mut self) -> u64 {
        self.0 ^= self.0 << 13;
        self.0 ^= self.0 >> 7;
        self.0 ^= self.0 << 17;
        self.0
    }
    fn below(&mut self, n: usize) -> usize {
        (self.next() % n as u64) as usize
    }
}

const NAMES: &[&str] = &["a", "b", "src", "docs", "node_modules", ".venv", "__pycache__", "lib", "x", "target"];

fn random_mutation(i: &mut Inner, rng: &mut Rng) {
    let folders: Vec<String> =
        i.nodes.iter().filter(|(_, n)| matches!(n.kind, Kind::Folder | Kind::Root)).map(|(k, _)| k.clone()).collect();
    let all: Vec<String> = i.nodes.keys().filter(|k| *k != "ROOT").cloned().collect();
    let name = format!("{}{}", NAMES[rng.below(NAMES.len())], if rng.below(3) == 0 { "" } else { "_" });
    let name = name.trim_end_matches('_').to_string();
    match rng.below(6) {
        0 | 1 => {
            let p = folders[rng.below(folders.len())].clone();
            let kind = if rng.below(2) == 0 { Kind::File } else { Kind::Folder };
            i.add(&p, &name, kind);
        }
        2 if !all.is_empty() => {
            let id = all[rng.below(all.len())].clone();
            i.delete(&id);
        }
        3 | 4 if !all.is_empty() => {
            let id = all[rng.below(all.len())].clone();
            let target = folders[rng.below(folders.len())].clone();
            // A folder can't move into its own subtree.
            if !i.subtree(&id).contains(&target) {
                i.mv(&id, &target);
            }
        }
        _ if !all.is_empty() => {
            let id = all[rng.below(all.len())].clone();
            i.rename(&id, &name);
        }
        _ => {}
    }
}

/// The seeds to run: `SIM_SEED` alone if set (whatever the range), else
/// `SIM_SEEDS` of them (default 60, which keeps CI fast) from
/// `SIM_SEED_FROM` (default 1).
fn seeds() -> std::ops::RangeInclusive<u64> {
    let var = |name: &str| std::env::var(name).ok().and_then(|s| s.parse::<u64>().ok());
    if let Some(only) = var("SIM_SEED") {
        return only..=only;
    }
    let from = var("SIM_SEED_FROM").unwrap_or(1).max(1);
    let count = var("SIM_SEEDS").unwrap_or(60);
    from..=from.saturating_add(count).saturating_sub(1)
}

#[tokio::test(flavor = "multi_thread")]
async fn randomized_simulation_converges_every_round() {
    for seed in seeds() {
        // On its own task, so a failure anywhere in the seed (not only the
        // checks that name it) still says which seed to replay.
        if let Err(e) = tokio::spawn(simulate(seed)).await {
            eprintln!("simulation failed for seed {seed}; replay with SIM_SEED={seed}");
            match e.try_into_panic() {
                Ok(p) => std::panic::resume_unwind(p),
                Err(e) => panic!("seed {seed}: {e}"),
            }
        }
    }
}

/// One seed of [`randomized_simulation_converges_every_round`].
async fn simulate(seed: u64) {
    let mut rng = Rng(seed.wrapping_mul(0x9E37_79B9_7F4A_7C15));
    let fake = Arc::new(Fake::new(1 + rng.below(6), rng.below(2) == 0));
    populate(&fake);
    fake.with(|i| {
        for _ in 0..40 {
            random_mutation(i, &mut rng);
        }
    });
    let path = temp_db(&format!("sim-{seed}"));
    let rules = if rng.below(4) == 0 { ExcludeSet::none() } else { ExcludeSet::dev() };
    // Vary the split strategy: fixed depth, size-based deep listing, or delta-everything.
    let list_depth = rng.below(3) as u32;
    // Hints: none, correct (and growing stale as later rounds mutate the
    // drive), pure garbage, or both mixed. None of them may change the result.
    let hint = match rng.below(4) {
        0 => None,
        1 => Some(SplitHint::from_excluded_paths(excluded_paths(&fake, &rules))),
        2 => Some(SplitHint::from_excluded_paths(random_paths(&mut rng))),
        _ => {
            let mut h = SplitHint::from_excluded_paths(excluded_paths(&fake, &rules));
            h.add(random_paths(&mut rng));
            Some(h)
        }
    };
    let cfg = Config {
        list_depth,
        max_list_depth: list_depth + rng.below(5) as u32,
        split_bytes: [0, 30, 100, i64::MAX][rng.below(4)],
        rules: rules.clone(),
        max_tasks: 1 + rng.below(6),
        hint: hint.map(Arc::new),
    };
    for round in 0..6 {
        // Sometimes change things while the enumeration/catch-up runs.
        let n_during = rng.below(4);
        let seed_during = rng.next();
        fake.with(|i| {
            i.during_enumeration.push(Box::new(move |i: &mut Inner| {
                let mut r = Rng(seed_during | 1);
                for _ in 0..n_during {
                    random_mutation(i, &mut r);
                }
            }));
        });
        fake.with(|i| i.trace.push(format!("== round {round}")));
        // Sometimes fail part-way, then resume.
        if rng.below(3) == 0 {
            fake.served.store(0, SeqCst);
            fake.fail_after.store(rng.below(8) as i64, SeqCst);
            let mf = mirror_with(&fake, Store::open(&path, 1024).unwrap(), cfg.clone());
            let r = mf.run().await;
            debug_dump(&mf, &format!("after failing run ({:?})", r.err()));
            drop(mf);
            fake.fail_after.store(-1, SeqCst);
        }
        let m = mirror_with(&fake, Store::open(&path, 1024).unwrap(), cfg.clone());
        m.run().await.unwrap_or_else(|e| panic!("seed {seed} round {round}: {e}"));
        debug_dump(&m, &format!("after run 1 of round {round}"));
        // "during enumeration" changes only fire on a fresh cursor, so run
        // once more to pick up anything made after the last cursor.
        let m2 = mirror_with(&fake, Store::open(&path, 1024).unwrap(), cfg.clone());
        drop(m);
        m2.run().await.unwrap_or_else(|e| panic!("seed {seed} round {round} (2nd): {e}"));
        if actual(&m2) != expected(&fake, &rules) {
            let trace = fake.with(|i| {
                i.trace.join(
                    "
",
                )
            });
            let (got, want) = (actual(&m2), expected(&fake, &rules));
            panic!(
                "seed {seed} round {round} cfg {cfg:?}
missing: {:?}
unexpected: {:?}
trace:
{trace}",
                want.difference(&got).collect::<Vec<_>>(),
                got.difference(&want).collect::<Vec<_>>()
            );
        }
        drop(m2);
        fake.with(|i| {
            i.during_enumeration.clear();
            for _ in 0..rng.below(15) + 1 {
                random_mutation(i, &mut rng);
            }
        });
    }
    let _ = std::fs::remove_dir_all(path.parent().unwrap());
}

#[tokio::test(flavor = "multi_thread")]
async fn size_based_deep_listing_skips_junk_without_downloading_it() {
    let fake = Arc::new(Fake::new(5, false));
    populate(&fake);
    let rules = ExcludeSet::dev();
    // List everything at least 1 byte large down to depth 6: every
    // node_modules is found by listing, so no junk entries are downloaded.
    let cfg =
        Config { list_depth: 1, max_list_depth: 6, split_bytes: 1, rules: rules.clone(), max_tasks: 4, hint: None };
    let m = mirror_with(&fake, Store::open_in_memory().unwrap(), cfg);
    let s = m.run().await.unwrap();
    assert_eq!(actual(&m), expected(&fake, &rules));
    assert_eq!(m.progress().pruned.load(SeqCst), 0, "junk was skipped, not downloaded and dropped");
    assert_eq!(s.stats.pruned_folders, 0);
}

/// Drive-relative paths ("/Code/proj0/node_modules") of the top-most excluded
/// folders, i.e. what a local scan of the same tree would report.
fn excluded_paths(fake: &Fake, rules: &ExcludeSet) -> Vec<String> {
    fake.with(|inner| {
        let mut out = Vec::new();
        let mut stack = vec![("ROOT".to_string(), String::new())];
        while let Some((id, path)) = stack.pop() {
            for c in inner.children(&id) {
                let n = &inner.nodes[&c];
                let p = format!("{path}/{}", n.name);
                if n.kind == Kind::Folder && rules.excludes_folder(&n.name) {
                    out.push(p);
                } else if n.kind == Kind::Folder {
                    stack.push((c, p));
                }
            }
        }
        out
    })
}

/// Plausible-looking but arbitrary paths, for wrong hints.
fn random_paths(rng: &mut Rng) -> Vec<String> {
    (0..rng.below(12))
        .map(|_| (0..1 + rng.below(5)).map(|_| format!("/{}", NAMES[rng.below(NAMES.len())])).collect())
        .collect()
}

#[tokio::test(flavor = "multi_thread")]
async fn hint_lists_only_the_way_to_junk() {
    let fake = Arc::new(Fake::new(5, false));
    populate(&fake);
    let rules = ExcludeSet::dev();
    let hint = SplitHint::from_excluded_paths(excluded_paths(&fake, &rules));
    // The size heuristic alone would never list below depth 1 here.
    let cfg = Config {
        list_depth: 1,
        max_list_depth: 1,
        split_bytes: i64::MAX,
        rules: rules.clone(),
        max_tasks: 4,
        hint: Some(Arc::new(hint)),
    };
    let m = mirror_with(&fake, Store::open_in_memory().unwrap(), cfg);
    let s = m.run().await.unwrap();
    assert_eq!(actual(&m), expected(&fake, &rules));
    assert_eq!(m.progress().pruned.load(SeqCst), 0, "every excluded folder was skipped by listing");
    assert_eq!(s.stats.pruned_folders, 0);
    // Folders with no junk below them were not listed but delta-enumerated
    // as one unit: `Documents` and every `src`.
    let units = m.read(|o| o.heaviest_units(100)).unwrap();
    let kind_of = |path: &str| units.iter().find(|u| u.path == path).map(|u| u.kind);
    assert_eq!(kind_of("/Documents"), Some(oneup_store::UnitKind::Delta));
    assert_eq!(kind_of("/Code/proj0"), Some(oneup_store::UnitKind::List));
    assert_eq!(kind_of("/Code/proj0/src"), Some(oneup_store::UnitKind::Delta));
}

#[tokio::test(flavor = "multi_thread")]
async fn wrong_hint_costs_speed_not_correctness() {
    let fake = Arc::new(Fake::new(3, true));
    populate(&fake);
    let rules = ExcludeSet::dev();
    let hint = SplitHint::from_excluded_paths(["/Documents/Empty/node_modules", "/Nope/x/.venv", "/code/PROJ1/src/a"]);
    let cfg = Config {
        list_depth: 1,
        max_list_depth: 1,
        split_bytes: i64::MAX,
        rules: rules.clone(),
        max_tasks: 2,
        hint: Some(Arc::new(hint)),
    };
    let m = mirror_with(&fake, Store::open_in_memory().unwrap(), cfg);
    m.run().await.unwrap();
    assert_eq!(actual(&m), expected(&fake, &rules));
    assert!(m.progress().pruned.load(SeqCst) > 0, "junk the hint missed was downloaded and dropped");
}

/// Regression (simulation seed 146): a folder moved out of never-listed junk
/// whose contents include a *file* named like an excluded folder.
#[tokio::test(flavor = "multi_thread")]
async fn moved_out_folder_with_junk_named_file() {
    for shuffle in [false, true] {
        for during in [false, true] {
            let fake = Arc::new(Fake::new(2, shuffle));
            populate(&fake);
            let mutate = |i: &mut Inner| {
                let find = |i: &Inner, name: &str| {
                    i.nodes.iter().find(|(_, n)| n.name == name).map(|(k, _)| k.clone()).unwrap()
                };
                let (sub, kept, proj2) = (find(i, "sub"), find(i, "kept.txt"), find(i, "proj2"));
                i.add(&sub, "x", Kind::Folder);
                i.rename(&kept, ".venv");
                i.mv(&sub, &proj2);
            };
            let rules = ExcludeSet::dev();
            let cfg = Config {
                list_depth: 2,
                max_list_depth: 5,
                split_bytes: 100,
                rules: rules.clone(),
                max_tasks: 1,
                hint: None,
            };
            let path = temp_db(&format!("seed146-{shuffle}-{during}"));
            if during {
                fake.with(|i| i.during_enumeration.push(Box::new(mutate)));
                mirror_with(&fake, Store::open(&path, 1024).unwrap(), cfg.clone()).run().await.unwrap();
            } else {
                mirror_with(&fake, Store::open(&path, 1024).unwrap(), cfg.clone()).run().await.unwrap();
                fake.with(mutate);
            }
            let m = mirror_with(&fake, Store::open(&path, 1024).unwrap(), cfg.clone());
            m.run().await.unwrap();
            let (got, want) = (actual(&m), expected(&fake, &rules));
            assert_eq!(
                got,
                want,
                "shuffle={shuffle} during={during}\nmissing {:?}",
                want.difference(&got).collect::<Vec<_>>()
            );
            drop(m);
            let _ = std::fs::remove_dir_all(path.parent().unwrap());
        }
    }
}

/// With SIM_DEBUG=id1,id2,... prints each id's state in the mirror.
fn debug_dump(m: &Mirror<Fake>, label: &str) {
    let Ok(ids) = std::env::var("SIM_DEBUG") else { return };
    let line = m
        .read(|o| {
            Ok(ids
                .split(',')
                .map(|id| {
                    let st = o.item_state(id).unwrap();
                    let parent = o.get_item(id).unwrap().and_then(|r| r.parent_id);
                    format!("{id}:{st:?}(parent {parent:?})")
                })
                .collect::<Vec<_>>()
                .join(" "))
        })
        .unwrap();
    let st = m.read(|o| o.stats()).unwrap();
    eprintln!("[{label}] {line} | orphans {} units_pending {}", st.orphans, st.units_pending);
}
