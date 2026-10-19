//! Change-journal observer on a real NTFS folder: changes made after a scan
//! must leave the observed Local tree identical to a fresh scan, and only
//! deletions the journal recorded may become deletion evidence.
//!
//! Everything happens under the system temp folder and is deleted
//! afterwards. The changes are made by this test process; the journal
//! records every process's changes alike (unlike Cloud Files, which ignores
//! its own provider's writes).
#![cfg(windows)]
#![allow(clippy::unwrap_used)]

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};
use std::time::Duration;

use oneup_local::observe::{Changes, Observer};
use oneup_local::{Config, Scan};
use oneup_rules::ExcludeSet;
use oneup_store::{LocalKind, Skip, Store};
use oneup_sync::{Base, LocalId, NodeKind, ROOT, RemoteId, Seen, Trees};

struct Tmp(PathBuf);

impl Tmp {
    fn new(tag: &str) -> Self {
        let p = std::env::temp_dir().join(format!("oneup-observe-test-{}-{tag}", std::process::id()));
        let _ = std::fs::remove_dir_all(&p);
        std::fs::create_dir_all(&p).unwrap();
        Self(p)
    }

    fn file(&self, rel: &str, content: &[u8]) -> PathBuf {
        let p = self.0.join(rel);
        std::fs::create_dir_all(p.parent().unwrap()).unwrap();
        std::fs::write(&p, content).unwrap();
        p
    }

    fn at(&self, rel: &str) -> PathBuf {
        self.0.join(rel)
    }
}

impl Drop for Tmp {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.0);
    }
}

/// What a sync engine would compare: path → (kind, skip, size, mtime, id).
/// Folder times are left out: a folder's last-write time changes when a
/// child is added, without a journal record for the folder, and sync never
/// uses it.
type View = BTreeMap<String, (LocalKind, Skip, i64, i64, u128)>;

fn view(s: &Store) -> View {
    s.read(|o| {
        let root = o.local_root()?.unwrap().root_file_id;
        let mut out = View::new();
        let mut stack = vec![(root, String::new())];
        while let Some((id, path)) = stack.pop() {
            for c in o.local_children(id)? {
                let p = format!("{path}/{}", c.name);
                let file = c.kind == LocalKind::File;
                let (size, mtime) = if file { (c.size, c.mtime) } else { (0, 0) };
                out.insert(p.clone(), (c.kind, c.skip, size, mtime, c.file_id));
                if c.kind == LocalKind::Folder {
                    stack.push((c.file_id, p));
                }
            }
        }
        Ok(out)
    })
    .unwrap()
}

fn scan(root: &Path) -> Store {
    let mut scan = Scan::open(Store::open_in_memory().unwrap(), root, Config::new(ExcludeSet::dev())).unwrap();
    assert!(scan.run().unwrap().finished);
    scan.into_store()
}

fn id_of(v: &View, path: &str) -> u128 {
    v.get(path).unwrap_or_else(|| panic!("{path} not in {v:#?}")).4
}

fn synced_file(sid: u64, name: &str, local: u128) -> Base {
    Base {
        sid,
        parent: Some(ROOT),
        name: name.into(),
        kind: NodeKind::File,
        size: 0,
        hash: None,
        local: Some(LocalId(local)),
        local_mtime: 0,
        remote: Some(RemoteId(format!("r{sid}"))),
        remote_etag: String::new(),
    }
}

#[test]
fn observed_tree_matches_a_fresh_scan_and_evidence_is_exact() {
    let t = Tmp::new("root");
    let outside = Tmp::new("outside");
    t.file("keep.txt", b"k");
    t.file("docs/a.txt", b"a");
    t.file("edit.txt", b"e");
    t.file("save.txt", b"s1");
    t.file("gone.txt", b"g");
    t.file("held.txt", b"h");
    t.file("out/x.txt", b"x");
    outside.file("incoming/inner.txt", b"i");
    outside.file("incoming/sub/deep.txt", b"d");

    let mut store = scan(&t.0);
    let before = view(&store);
    let (gone, save, x) = (id_of(&before, "/gone.txt"), id_of(&before, "/save.txt"), id_of(&before, "/out/x.txt"));
    let held = id_of(&before, "/held.txt");
    let root_id = store.read(|o| o.local_root()).unwrap().unwrap().root_file_id;
    store
        .write(|o| {
            let mut root = synced_file(ROOT, "", root_id);
            (root.parent, root.kind) = (None, NodeKind::Folder);
            o.synced_put(&root)?;
            o.synced_put(&synced_file(1, "gone.txt", gone))?;
            o.synced_put(&synced_file(2, "save.txt", save))?;
            o.synced_put(&synced_file(3, "x.txt", x))?;
            o.synced_put(&synced_file(4, "held.txt", held))?;
            Ok(())
        })
        .unwrap();

    // Every kind of change, made after the scan.
    t.file("new.txt", b"n");
    t.file("newdir/n.txt", b"nn");
    std::fs::write(t.at("edit.txt"), b"edited, longer").unwrap();
    std::fs::rename(t.at("docs/a.txt"), t.at("docs/b.txt")).unwrap();
    std::fs::rename(t.at("keep.txt"), t.at("docs/keep.txt")).unwrap();
    std::fs::remove_file(t.at("gone.txt")).unwrap();
    {
        // Deleted while another handle is open (as antivirus or an indexer
        // often has): Windows moves it to `$Extend\$Deleted` first, and the
        // delete record only comes once the handle closes.
        use std::os::windows::fs::OpenOptionsExt;
        let open = std::fs::OpenOptions::new().read(true).share_mode(7).open(t.at("held.txt")).unwrap();
        std::fs::remove_file(t.at("held.txt")).unwrap();
        drop(open);
    }
    t.file("save.tmp", b"s2, saved atomically");
    std::fs::rename(t.at("save.tmp"), t.at("save.txt")).unwrap();
    std::fs::rename(t.at("out"), outside.at("out")).unwrap();
    std::fs::rename(outside.at("incoming"), t.at("incoming")).unwrap();
    t.file("node_modules/pkg/index.js", b"js");

    // Delete records can land shortly after the call returns (measured),
    // so catch up until the tree matches, within a bound.
    let mut obs = Observer::open(store, Config::new(ExcludeSet::dev())).unwrap();
    let mut total = Changes::default();
    let expected = view(&scan(&t.0));
    let mut observed = View::new();
    for _ in 0..50 {
        let ch = obs.catch_up().unwrap();
        total.records += ch.records;
        total.deleted += ch.deleted;
        total.evidence += ch.evidence;
        total.opened_by_id += ch.opened_by_id;
        total.dirty.extend(ch.dirty);
        observed = view(obs.store());
        if observed == expected && total.evidence == 3 {
            break;
        }
        std::thread::sleep(Duration::from_millis(100));
    }
    assert_eq!(observed, expected, "observed tree differs from a fresh scan");
    assert!(observed.contains_key("/incoming/sub/deep.txt"), "a folder moved in arrives whole");
    assert_eq!(observed["/node_modules"].1, Skip::Excluded);
    assert!(!observed.contains_key("/node_modules/pkg"), "nothing stored below an excluded folder");
    assert!(total.records > 0 && total.dirty.contains(&root_id));
    assert_eq!(total.opened_by_id, 0, "every item read from its folder's listing, none opened");

    let store = obs.into_store();
    store
        .read(|o| {
            assert!(o.check_invariants()?.is_empty(), "{:?}", o.check_invariants()?);
            assert_eq!(o.local(LocalId(gone))?, Seen::Deleted, "deleted: evidence");
            assert_eq!(o.local(LocalId(save))?, Seen::Deleted, "replaced by an atomic save: evidence");
            assert_eq!(o.local(LocalId(held))?, Seen::Deleted, "deleted while open: evidence");
            assert_eq!(o.local(LocalId(x))?, Seen::Missing, "moved out of the folder: no evidence");
            Ok(())
        })
        .unwrap();
    assert_eq!(total.evidence, 3);
}

#[test]
fn an_unfinished_or_differently_ruled_scan_is_refused() {
    let t = Tmp::new("refuse");
    t.file("a.txt", b"a");
    let store = scan(&t.0);
    let err = Observer::open(store, Config::new(ExcludeSet::none())).err().unwrap();
    assert!(err.to_string().contains("exclusion rules changed"), "{err}");
    let err = Observer::open(Store::open_in_memory().unwrap(), Config::new(ExcludeSet::dev())).err().unwrap();
    assert!(err.to_string().contains("no folder has been scanned"), "{err}");
}

fn scan_with(root: &Path, rules: ExcludeSet) -> Store {
    let mut scan = Scan::open(Store::open_in_memory().unwrap(), root, Config::new(rules)).unwrap();
    assert!(scan.run().unwrap().finished);
    scan.into_store()
}

/// Changed name-only rules are applied to the stored tree without a
/// rescan: excluding drops a folder's contents, including lists it again,
/// and either way the result equals a fresh scan with the new rules. The
/// engine's own folder stays excluded throughout.
#[test]
fn changed_folder_rules_are_reapplied_without_a_rescan() {
    let t = Tmp::new("reapply");
    t.file("app/node_modules/pkg/index.js", b"js");
    t.file("app/node_modules/pkg/deep/more.js", b"more");
    t.file("app/src/main.rs", b"fn main() {}");
    t.file(".oneup/versions/x.txt", b"engine");
    let mut store = scan(&t.0); // dev: node_modules excluded

    let none = ExcludeSet::none();
    let r = store.write(|o| oneup_local::observe::reapply_rules(o, &none)).unwrap();
    assert_eq!(r.map(|r| (r.excluded, r.included)), Some((0, 1)));
    let mut obs = Observer::open(store, Config::new(none.clone())).unwrap();
    obs.catch_up().unwrap();
    let mut store = obs.into_store();
    assert_eq!(view(&store), view(&scan_with(&t.0, none.clone())));
    assert_eq!(store.write(|o| oneup_local::observe::reapply_rules(o, &none)).unwrap(), None, "already applied");

    let dev = ExcludeSet::dev();
    let r = store.write(|o| oneup_local::observe::reapply_rules(o, &dev)).unwrap();
    assert_eq!(r.map(|r| (r.excluded, r.included)), Some((1, 0)));
    let mut obs = Observer::open(store, Config::new(dev.clone())).unwrap();
    obs.catch_up().unwrap();
    let store = obs.into_store();
    assert_eq!(view(&store), view(&scan_with(&t.0, dev)));
    let problems = store.read(|o| o.check_invariants()).unwrap();
    assert!(problems.is_empty(), "{problems:?}");
}

/// Unregisters a test sync root, before [`Tmp`] deletes the folder.
struct Registered<'a>(&'a Path);

impl Drop for Registered<'_> {
    fn drop(&mut self) {
        let _ = oneup_cfapi::provider::unregister(self.0);
    }
}

/// Another provider's folder whose contents are still in the cloud, created
/// after the scan: a journal record must not get it listed, which would make
/// the provider download its file list.
#[test]
fn an_unpopulated_placeholder_folder_from_the_journal_is_not_listed() {
    // RECALL_ON_OPEN | RECALL_ON_DATA_ACCESS | OFFLINE
    const RECALL: u32 = 0x0004_0000 | 0x0040_0000 | 0x1000;
    let t = Tmp::new("unpop");
    t.file("keep.txt", b"k");
    oneup_cfapi::provider::register_as(
        &t.0,
        "FakeOneDrive",
        0x0dd0_0dd0_0dd0_0dd0_0dd0_0dd0_0dd0_0dd7,
        b"theirs",
        false,
    )
    .unwrap();
    let _registered = Registered(&t.0);
    let store = scan(&t.0);
    oneup_cfapi::provider::create_unpopulated_folder(&t.0, "unpop", b"OD!u").unwrap();
    // A placeholder folder that is populated is listed as usual.
    let ph = |name, folder| oneup_cfapi::provider::NewPlaceholder {
        name,
        identity: b"OD!f",
        size: if folder { 0 } else { 3 },
        modified: 133_000_000_000_000_000,
        folder,
    };
    oneup_cfapi::provider::create(&t.0, &[ph("full", true)]).unwrap();
    oneup_cfapi::provider::create(&t.at("full"), &[ph("a.txt", false)]).unwrap();

    let mut obs = Observer::open(store, Config::new(ExcludeSet::dev())).unwrap();
    let mut queued = 0;
    let mut observed = View::new();
    for _ in 0..50 {
        queued += obs.catch_up().unwrap().folders_queued;
        observed = view(obs.store());
        if observed.contains_key("/unpop") && observed.contains_key("/full/a.txt") {
            break;
        }
        std::thread::sleep(Duration::from_millis(100));
    }
    assert_eq!(observed.get("/unpop").map(|e| (e.0, e.1)), Some((LocalKind::Folder, Skip::Unpopulated)));
    assert_eq!(observed.get("/full").map(|e| e.1), Some(Skip::None));
    assert_eq!(queued, 1, "only the populated folder is queued for listing");
    assert_eq!(observed, view(&scan(&t.0)), "as a fresh scan sees it");
    let listed = std::fs::read_dir(&t.0).unwrap().map(Result::unwrap).find(|e| e.file_name() == "unpop").unwrap();
    use std::os::windows::fs::MetadataExt;
    assert_ne!(listed.metadata().unwrap().file_attributes() & RECALL, 0, "still unpopulated");
}

/// Saves by renaming a new file over the old one, by another program
/// (`cmd`'s `move /y`, which fails while any handle on the file is open),
/// all go through while the observer reads the folder over and over: it
/// reads items from their folder's listing and never opens them.
#[test]
fn saves_by_rename_go_through_while_the_observer_reads() {
    let t = Tmp::new("saves");
    t.file("doc.txt", b"v0");
    t.file("other.txt", b"o");
    let store = scan(&t.0);
    let mut obs = Observer::open(store, Config::new(ExcludeSet::dev())).unwrap();
    let stop = std::sync::Arc::new(std::sync::atomic::AtomicBool::new(false));
    let reader = {
        let stop = std::sync::Arc::clone(&stop);
        std::thread::spawn(move || {
            let (mut rounds, mut by_id) = (0u32, 0u64);
            while !stop.load(std::sync::atomic::Ordering::Relaxed) || rounds == 0 {
                by_id += obs.catch_up().unwrap().opened_by_id;
                rounds += 1;
            }
            (obs, rounds, by_id)
        })
    };
    let mut failed = Vec::new();
    for i in 0..30 {
        let body = format!("v{}", i + 1);
        t.file("doc.tmp", body.as_bytes());
        let out = std::process::Command::new("cmd.exe")
            .current_dir(&t.0)
            .args(["/c", "move", "/y", "doc.tmp", "doc.txt"])
            .output()
            .unwrap();
        if !out.status.success() || std::fs::read(t.at("doc.txt")).unwrap() != body.as_bytes() {
            failed.push((i, String::from_utf8_lossy(&out.stdout).into_owned()));
            let _ = std::fs::remove_file(t.at("doc.tmp"));
        }
    }
    stop.store(true, std::sync::atomic::Ordering::Relaxed);
    let (mut obs, rounds, mut by_id) = reader.join().unwrap();
    assert!(failed.is_empty(), "saves failed: {failed:?}");
    assert!(rounds > 1, "the observer read meanwhile");

    // Then an Office-style save (the old file moved aside, the new one
    // renamed in, the old one deleted) and a rename that changes only the
    // case: every item is still read from the listing.
    t.file("~tmp1.tmp", b"office");
    std::fs::rename(t.at("other.txt"), t.at("~old.tmp")).unwrap();
    std::fs::rename(t.at("~tmp1.tmp"), t.at("other.txt")).unwrap();
    std::fs::remove_file(t.at("~old.tmp")).unwrap();
    std::fs::rename(t.at("doc.txt"), t.at("Doc.txt")).unwrap();
    let expected = view(&scan(&t.0));
    let mut observed = View::new();
    for _ in 0..50 {
        by_id += obs.catch_up().unwrap().opened_by_id;
        observed = view(obs.store());
        if observed == expected {
            break;
        }
        std::thread::sleep(Duration::from_millis(100));
    }
    assert_eq!(observed, expected);
    assert!(observed.contains_key("/Doc.txt"), "the new case");
    assert_eq!(by_id, 0);
}

/// An item with a new name in a folder too large to list whole is the one
/// case read by opening it by ID; the tree still comes out right.
#[test]
fn a_new_name_in_a_large_folder_is_read_by_id() {
    let t = Tmp::new("large");
    for i in 0..=oneup_local::observe::LIST_WHOLE_MAX {
        t.file(&format!("big/f{i:05}"), b"");
    }
    let store = scan(&t.0);
    t.file("big/new.txt", b"n");
    std::fs::write(t.at("big/f00000"), b"edited").unwrap();
    let mut obs = Observer::open(store, Config::new(ExcludeSet::dev())).unwrap();
    let expected = view(&scan(&t.0));
    let (mut observed, mut by_id) = (View::new(), 0);
    for _ in 0..50 {
        by_id += obs.catch_up().unwrap().opened_by_id;
        observed = view(obs.store());
        if observed == expected {
            break;
        }
        std::thread::sleep(Duration::from_millis(100));
    }
    assert_eq!(observed, expected);
    assert_eq!(by_id, 1, "the new name only; the edited file is found by its stored name");
}
