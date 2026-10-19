//! Scanner tests on real NTFS folders. Every test builds its own tree under
//! the system temp folder and deletes it afterwards; nothing else is touched.
#![cfg(windows)]
#![allow(clippy::unwrap_used)]

use std::collections::{BTreeMap, BTreeSet};
use std::ffi::OsString;
use std::os::windows::ffi::OsStringExt;
use std::path::{Path, PathBuf};
use std::process::Command;
use std::sync::atomic::{AtomicU32, Ordering::Relaxed};

use oneup_local::{Config, Error, Replaced, Scan, Start};
use oneup_rules::ExcludeSet;
use oneup_store::{LocalKind, Skip, Store};

/// A private temp folder, removed on drop (after lifting any deny ACLs a test set).
struct Tmp {
    path: PathBuf,
    denied: Vec<PathBuf>,
}

impl Tmp {
    fn new(tag: &str) -> Self {
        static N: AtomicU32 = AtomicU32::new(0);
        let path = std::env::temp_dir().join(format!(
            "oneup-local-test-{}-{}-{tag}",
            std::process::id(),
            N.fetch_add(1, Relaxed)
        ));
        std::fs::create_dir_all(&path).unwrap();
        Self { path, denied: Vec::new() }
    }

    fn file(&self, rel: &str, content: &[u8]) -> PathBuf {
        let p = self.path.join(rel);
        std::fs::create_dir_all(p.parent().unwrap()).unwrap();
        std::fs::write(&p, content).unwrap();
        p
    }

    fn dir(&self, rel: &str) -> PathBuf {
        let p = self.path.join(rel);
        std::fs::create_dir_all(&p).unwrap();
        p
    }
}

impl Drop for Tmp {
    fn drop(&mut self) {
        for d in &self.denied {
            let _ = Command::new("icacls").arg(d).args(["/remove:d", &whoami(), "/Q"]).output();
        }
        let _ = std::fs::remove_dir_all(&self.path);
    }
}

fn whoami() -> String {
    let out = Command::new("whoami").output().unwrap();
    String::from_utf8_lossy(&out.stdout).trim().to_string()
}

fn junction(link: &Path, target: &Path) -> bool {
    Command::new("cmd")
        .args(["/C", "mklink", "/J"])
        .arg(link)
        .arg(target)
        .output()
        .map(|o| o.status.success())
        .unwrap_or(false)
}

/// Everything about the tree an observer could notice changing: every path
/// (links not followed) with its size and last-write time.
fn snapshot(root: &Path) -> BTreeMap<PathBuf, (u64, Option<std::time::SystemTime>)> {
    let mut out = BTreeMap::new();
    let mut stack = vec![root.to_path_buf()];
    while let Some(dir) = stack.pop() {
        let Ok(rd) = std::fs::read_dir(&dir) else { continue };
        for e in rd.flatten() {
            let Ok(md) = std::fs::symlink_metadata(e.path()) else { continue };
            out.insert(e.path(), (md.len(), md.modified().ok()));
            if md.is_dir() && !md.file_type().is_symlink() {
                stack.push(e.path());
            }
        }
    }
    out
}

/// Every stored row as (relative path, kind, size, skip), by walking the
/// Local tree from its root through `local_children`.
fn dump(store: &Store) -> BTreeSet<(String, i64, i64, i64)> {
    store
        .read(|o| {
            let root = o.local_root()?.unwrap().root_file_id;
            let mut out = BTreeSet::new();
            let mut stack = vec![(root, String::new())];
            while let Some((id, path)) = stack.pop() {
                for c in o.local_children(id)? {
                    let p = format!("{path}/{}", c.name);
                    out.insert((p.clone(), c.kind as i64, c.size, c.skip as i64));
                    stack.push((c.file_id, p));
                }
            }
            Ok(out)
        })
        .unwrap()
}

fn names(store: &Store) -> BTreeSet<String> {
    dump(store).into_iter().map(|(p, ..)| p).collect()
}

fn scan(store: Store, path: &Path, rules: ExcludeSet) -> (Scan, oneup_local::Summary) {
    let mut s = Scan::open(store, path, Config::new(rules)).unwrap();
    let summary = s.run().unwrap();
    (s, summary)
}

fn invariants(store: &Store) {
    let problems = store.read(|o| o.check_invariants()).unwrap();
    assert!(problems.is_empty(), "{problems:?}");
}

#[test]
fn prunes_excluded_folders_and_changes_nothing() {
    let t = Tmp::new("prune");
    t.file("a.txt", b"hello");
    t.file("src/main.rs", b"fn main() {}");
    t.file("src/deep/x/y/z.txt", b"z");
    t.file("app/node_modules/pkg/index.js", b"junk");
    t.file("app/node_modules/pkg/lib/more.js", b"junk");
    t.file(".git/objects/ab/cdef", b"junk");
    t.file("node_modules_backup/kept.txt", b"kept"); // similar name, not excluded
    t.dir("empty");
    let before = snapshot(&t.path);

    let (s, sum) = scan(Store::open_in_memory().unwrap(), &t.path, ExcludeSet::dev());
    let store = s.into_store();

    assert_eq!(snapshot(&t.path), before, "a scan must not change anything");
    assert!(sum.finished);
    assert_eq!(sum.start, Start::Fresh);
    let st = &sum.stats;
    assert_eq!((st.files, st.file_bytes), (4, 5 + 12 + 1 + 4));
    // root, src, deep, x, y, app, node_modules (excl), .git (excl), backup, empty
    assert_eq!((st.folders, st.excluded_folders, st.queued), (10, 2, 0));
    assert!(sum.root.journal.is_some(), "NTFS temp folders have a change journal");
    assert!(sum.root.finished_at_ms.is_some());
    assert!(sum.root.path.ends_with(t.path.file_name().unwrap().to_str().unwrap()));

    let all = names(&store);
    assert!(all.contains("/src/deep/x/y/z.txt"));
    assert!(all.contains("/node_modules_backup/kept.txt"));
    assert!(!all.iter().any(|p| p.contains("index.js") || p.contains("/objects")), "excluded contents stored");
    assert_eq!(store.read(|o| o.local_excluded_paths()).unwrap(), ["/.git", "/app/node_modules"]);
    invariants(&store);
}

#[test]
fn no_rules_lists_everything() {
    let t = Tmp::new("none");
    t.file("app/node_modules/pkg/index.js", b"x");
    let (s, sum) = scan(Store::open_in_memory().unwrap(), &t.path, ExcludeSet::none());
    assert_eq!((sum.stats.files, sum.stats.excluded_folders), (1, 0));
    assert!(names(&s.into_store()).contains("/app/node_modules/pkg/index.js"));
}

#[test]
fn links_are_recorded_but_never_followed() {
    let outside = Tmp::new("outside");
    outside.file("secret.txt", b"must not be scanned");
    let t = Tmp::new("links");
    t.file("real/file.txt", b"x");
    assert!(junction(&t.path.join("junction"), &outside.path), "mklink /J failed");
    // A loop back into the tree: following it would never terminate.
    assert!(junction(&t.path.join("real").join("loop"), &t.path));
    // Directory symlinks need Developer Mode or admin; cover them when possible.
    let symlink = std::os::windows::fs::symlink_dir(&outside.path, t.path.join("symlink")).is_ok();

    let (s, sum) = scan(Store::open_in_memory().unwrap(), &t.path, ExcludeSet::none());
    let store = s.into_store();
    let rows = dump(&store);
    let link = LocalKind::Link as i64;
    assert!(rows.iter().any(|(p, k, ..)| p == "/junction" && *k == link));
    assert!(rows.iter().any(|(p, k, ..)| p == "/real/loop" && *k == link));
    if symlink {
        assert!(rows.iter().any(|(p, k, ..)| p == "/symlink" && *k == link));
    }
    assert!(!rows.iter().any(|(p, ..)| p.contains("secret") || p.starts_with("/real/loop/")));
    assert_eq!(sum.stats.links, 2 + i64::from(symlink));
    assert_eq!(sum.stats.files, 1);
    invariants(&store);
}

#[test]
fn long_paths_and_unusual_names() {
    let t = Tmp::new("names");
    // > 400 characters below the root: beyond MAX_PATH and OneDrive's limit.
    let deep: Vec<String> = (0..8).map(|i| format!("{i}{}", "d".repeat(49))).collect();
    let deep_rel = deep.join("/");
    t.file(&format!("{deep_rel}/leaf.txt"), b"deep");
    let unicode = ["naïve café.txt", "日本語のファイル.txt", "emoji 😀🎉.txt", "e\u{301}.txt", "CON_not_reserved.txt"];
    for n in unicode {
        t.file(n, n.as_bytes());
    }
    // An unpaired UTF-16 surrogate: legal on NTFS, not valid Unicode.
    let lossy: OsString = OsString::from_wide(&[0x61, 0xD800, 0x62, 0x2E, 0x74]); // "a\u{D800}b.t"
    std::fs::write(t.path.join(&lossy), b"lossy").unwrap();
    // A trailing dot needs a verbatim path to create; Windows APIs normally strip it.
    let verbatim = PathBuf::from(format!(r"\\?\{}", t.path.join("trailing.").display()));
    std::fs::write(&verbatim, b"dot").unwrap();

    let (s, sum) = scan(Store::open_in_memory().unwrap(), &t.path, ExcludeSet::none());
    let store = s.into_store();
    let all = names(&store);
    let leaf = format!("/{deep_rel}/leaf.txt");
    assert!(leaf.len() > 400 && all.contains(&leaf), "deep path not scanned");
    for n in unicode {
        assert!(all.contains(&format!("/{n}")), "{n} missing");
    }
    assert!(all.contains("/trailing."));
    assert_eq!(sum.stats.lossy_names, 1);
    assert_eq!(sum.stats.files, 1 + unicode.len() as i64 + 2);
    invariants(&store);
}

#[test]
fn unreadable_folders_are_counted_and_skipped() {
    let mut t = Tmp::new("denied");
    t.file("ok/visible.txt", b"x");
    let locked = t.dir("locked");
    t.file("locked/hidden.txt", b"x");
    // Deny ourselves "list folder" on `locked` (removed again on drop).
    let out = Command::new("icacls").arg(&locked).args(["/deny", &format!("{}:(RD)", whoami())]).output().unwrap();
    assert!(out.status.success(), "icacls: {}", String::from_utf8_lossy(&out.stderr));
    t.denied.push(locked);

    let (s, sum) = scan(Store::open_in_memory().unwrap(), &t.path, ExcludeSet::none());
    assert!(sum.finished);
    assert_eq!((sum.stats.unreadable_folders, sum.stats.files), (1, 1));
    let store = s.into_store();
    let rows = dump(&store);
    assert!(rows.contains(&("/locked".into(), LocalKind::Folder as i64, 0, Skip::Unreadable as i64)));
    assert!(!rows.iter().any(|(p, ..)| p.contains("hidden")));
    invariants(&store);
}

/// Builds a tree with many folders so a scan takes many small steps.
fn wide_tree(t: &Tmp) {
    for a in 0..6 {
        for b in 0..5 {
            t.file(&format!("d{a}/e{b}/f.txt"), format!("{a}{b}").as_bytes());
            t.file(&format!("d{a}/e{b}/node_modules/x/junk.js"), b"j");
        }
        t.file(&format!("d{a}/top.txt"), b"t");
    }
}

#[test]
fn interrupted_scan_resumes_to_the_same_result() {
    let t = Tmp::new("resume");
    wide_tree(&t);
    let reference = {
        let (s, _) = scan(Store::open_in_memory().unwrap(), &t.path, ExcludeSet::dev());
        dump(&s.into_store())
    };

    let db_dir = Tmp::new("resume-db");
    let db = db_dir.path.join("state.db");
    {
        let cfg = Config { batch_dirs: 2, ..Config::new(ExcludeSet::dev()) };
        let mut s = Scan::open(Store::open(&db, 1024).unwrap(), &t.path, cfg).unwrap();
        for _ in 0..4 {
            assert!(s.step().unwrap());
        }
        // Dropped mid-scan, as after Ctrl-C or a crash.
    }
    let store = Store::open(&db, 1024).unwrap();
    invariants(&store);
    let partial = store.read(|o| o.local_stats()).unwrap();
    assert!(partial.queued > 0, "the interruption should leave work queued");

    let mut s = Scan::open(store, &t.path, Config::new(ExcludeSet::dev())).unwrap();
    assert_eq!(s.start(), Start::Resumed);
    let sum = s.run().unwrap();
    assert!(sum.finished);
    let store = s.into_store();
    assert_eq!(dump(&store), reference);
    invariants(&store);
}

#[test]
fn cancelled_run_stops_and_resumes() {
    let t = Tmp::new("cancel");
    wide_tree(&t);
    let db_dir = Tmp::new("cancel-db");
    let db = db_dir.path.join("state.db");
    let cfg = Config { batch_dirs: 1, ..Config::new(ExcludeSet::dev()) };
    let mut s = Scan::open(Store::open(&db, 1024).unwrap(), &t.path, cfg.clone()).unwrap();
    s.cancel_flag().store(true, Relaxed);
    let sum = s.run().unwrap();
    assert!(!sum.finished);
    assert!(sum.root.finished_at_ms.is_none());
    drop(s);
    let mut s = Scan::open(Store::open(&db, 1024).unwrap(), &t.path, cfg).unwrap();
    assert_eq!(s.start(), Start::Resumed);
    assert!(s.run().unwrap().finished);
}

#[test]
fn a_different_folder_or_rule_set_starts_over() {
    let t1 = Tmp::new("one");
    wide_tree(&t1);
    let t2 = Tmp::new("two");
    t2.file("only.txt", b"x");
    let db_dir = Tmp::new("restart-db");
    let db = db_dir.path.join("state.db");
    let open = |p: &Path, rules: ExcludeSet| {
        Scan::open(Store::open(&db, 1024).unwrap(), p, Config { batch_dirs: 1, ..Config::new(rules) }).unwrap()
    };

    let mut s = open(&t1.path, ExcludeSet::dev());
    assert_eq!(s.start(), Start::Fresh);
    s.step().unwrap();
    drop(s);
    let mut s = open(&t1.path, ExcludeSet::none());
    assert_eq!(s.start(), Start::Replaced(Replaced::RulesChanged));
    s.step().unwrap();
    drop(s);
    let mut s = open(&t2.path, ExcludeSet::none());
    assert_eq!(s.start(), Start::Replaced(Replaced::OtherFolder));
    assert!(s.run().unwrap().finished);
    drop(s);
    let mut s = open(&t2.path, ExcludeSet::none());
    assert_eq!(s.start(), Start::Replaced(Replaced::Finished));
    let sum = s.run().unwrap();
    assert_eq!(sum.stats.files, 1, "nothing from the first folder survives");
    invariants(&s.into_store());

    // An unfinished scan made by an older scanner isn't resumed.
    let mut s = open(&t1.path, ExcludeSet::none());
    s.step().unwrap();
    let mut store = s.into_store();
    store.write(|o| o.meta_set("local_scan_format", "1")).unwrap();
    drop(store);
    let s = open(&t1.path, ExcludeSet::none());
    assert_eq!(s.start(), Start::Replaced(Replaced::OlderScanner));
}

/// An unlinking between the start of a scan and its resumption: the half
/// listed while another sync app held the folder doesn't describe it now.
#[test]
fn a_change_of_sync_app_starts_over() {
    let t = Tmp::new("manager");
    wide_tree(&t);
    let db_dir = Tmp::new("manager-db");
    let db = db_dir.path.join("state.db");
    let open = || {
        Scan::open(
            Store::open(&db, 1024).unwrap(),
            &t.path,
            Config { batch_dirs: 1, ..Config::new(ExcludeSet::none()) },
        )
        .unwrap()
    };
    oneup_cfapi::provider::register_as(
        &t.path,
        "FakeOneDrive",
        0x0dd0_0dd0_0dd0_0dd0_0dd0_0dd0_0dd0_0dd6,
        b"theirs",
        true,
    )
    .unwrap();
    let mut s = open();
    assert_eq!(s.root().sync_root_provider.as_deref(), Some("FakeOneDrive"));
    s.step().unwrap();
    drop(s);
    oneup_cfapi::provider::unregister(&t.path).unwrap();
    let s = open();
    assert_eq!(s.start(), Start::Replaced(Replaced::ManagerChanged));
    assert_eq!(s.root().sync_root_provider, None);
}

#[test]
fn files_and_links_are_refused_as_roots() {
    let t = Tmp::new("roots");
    let f = t.file("f.txt", b"x");
    let target = t.dir("target");
    assert!(junction(&t.path.join("j"), &target));
    let open = |p: &Path| Scan::open(Store::open_in_memory().unwrap(), p, Config::new(ExcludeSet::none()));
    assert!(matches!(open(&f), Err(Error::NotAFolder(_))));
    assert!(matches!(open(&t.path.join("j")), Err(Error::RootIsLink(_))));
    assert!(open(&t.path.join("missing")).is_err());
}

#[test]
fn temp_folders_are_not_another_engines() {
    let t = Tmp::new("coexist");
    let s = Scan::open(Store::open_in_memory().unwrap(), &t.path, Config::new(ExcludeSet::none())).unwrap();
    // The test machine's temp folder isn't inside OneDrive.
    assert_eq!(s.other_engine(), None);
    assert_eq!(s.root().sync_root_provider, None);
}

/// `--verify` hashes a file only while it is still exactly the file the scan
/// recorded, and changes nothing by reading.
#[test]
fn verify_hashes_only_unchanged_ordinary_files() {
    use oneup_local::verify::{Checked, Verifier};
    let t = Tmp::new("verify");
    let big: Vec<u8> = (0..3_000_000u32).map(|i| (i.wrapping_mul(7) >> 3) as u8).collect();
    t.file("same.txt", b"hello world");
    t.file("big.bin", &big);
    let edited = t.file("edited.txt", b"before");
    let gone = t.file("gone.txt", b"bye");
    let target = t.dir("target");
    assert!(junction(&t.path.join("link"), &target));

    let (s, sum) = scan(Store::open_in_memory().unwrap(), &t.path, ExcludeSet::none());
    let store = s.into_store();
    let rows: BTreeMap<String, oneup_store::LocalRow> = store
        .read(|o| Ok(o.local_children(sum.root.root_file_id)?.into_iter().map(|r| (r.name.clone(), r)).collect()))
        .unwrap();

    // After the scan: same-size edit with a different timestamp, and a deletion.
    std::fs::write(&edited, b"AFTER!").unwrap();
    let f = std::fs::File::options().write(true).open(&edited).unwrap();
    f.set_modified(std::time::SystemTime::UNIX_EPOCH + std::time::Duration::from_secs(1_000_000_000)).unwrap();
    drop(f);
    std::fs::remove_file(&gone).unwrap();
    let before = snapshot(&t.path);

    let qxh = |d: &[u8]| {
        let mut h = oneup_hash::QuickXorHash::new();
        h.update(d);
        h.finalize()
    };
    let mut v = Verifier::open(&sum.root, 64 * 1024).unwrap();
    assert_eq!(v.hash(&rows["same.txt"]), Checked::Hash(qxh(b"hello world")));
    assert_eq!(v.hash(&rows["big.bin"]), Checked::Hash(qxh(&big)), "multi-buffer files hash correctly");
    assert_eq!(v.hash(&rows["edited.txt"]), Checked::Changed);
    assert_eq!(v.hash(&rows["gone.txt"]), Checked::Changed);
    assert_eq!(v.hash(&rows["link"]), Checked::NotOrdinary);
    assert_eq!(v.opened_by_id(), 1, "found by path, in their folders; only the deleted one looked up by ID");
    // A placeholder as the scan recorded it is refused without being opened.
    let placeholder = oneup_store::LocalRow { reparse_tag: 0x9000_101A, ..rows["same.txt"].clone() };
    assert_eq!(v.hash(&placeholder), Checked::NotOrdinary);
    // Rule files are read with the same guards, and bounded.
    use oneup_local::verify::Small;
    assert_eq!(v.read_small(&rows["same.txt"], 64), Small::Read(b"hello world".to_vec()));
    assert_eq!(v.read_small(&rows["same.txt"], 5), Small::TooLarge);
    assert_eq!(v.read_small(&rows["edited.txt"], 64), Small::Checked(Checked::Changed));
    assert_eq!(v.read_small(&placeholder, 64), Small::Checked(Checked::NotOrdinary));
    assert_eq!(v.read_small(&rows["link"], 64), Small::Checked(Checked::NotOrdinary));
    assert_eq!(snapshot(&t.path), before, "verifying must not change anything");

    // Moved since the scan, unchanged: found by ID, and hashed. A program
    // that has it open for writing, without writing, doesn't stop that.
    std::fs::rename(t.path.join("same.txt"), target.join("same.txt")).unwrap();
    let writer = std::fs::OpenOptions::new().read(true).write(true).open(target.join("same.txt")).unwrap();
    assert_eq!(v.hash(&rows["same.txt"]), Checked::Hash(qxh(b"hello world")));
    assert_eq!(v.opened_by_id(), 2);
    drop(writer);

    // The scan describes this folder only: a different identity is refused.
    let other = oneup_store::LocalRoot { root_file_id: sum.root.root_file_id ^ 1, ..sum.root.clone() };
    assert!(matches!(Verifier::open(&other, 4096), Err(oneup_local::verify::Error::RootChanged(_))));
    // Nothing is read in a folder another sync app manages.
    let managed = oneup_store::LocalRoot { sync_root_provider: Some("OneDrive - Personal".into()), ..sum.root.clone() };
    assert!(matches!(Verifier::open(&managed, 4096), Err(oneup_local::verify::Error::OtherEngine(_))));
}
