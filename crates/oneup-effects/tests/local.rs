//! Local effects against real temp folders (never a user's folder).
#![cfg(windows)]
#![allow(clippy::unwrap_used)]

use std::io::{Read as _, Write as _};
use std::path::{Path, PathBuf};

use oneup_effects::{Done, Error, LocalFs};
use oneup_gate::{Approved, Choice, Context, Decision, OldCopy, Policy, review_in};
use oneup_ntfs::dirent::DirEntry;
use oneup_ntfs::win;
use oneup_sync::{Hash, Link, LocalId, LocalPre, Op, RemoteId, RemotePre};

struct Root {
    dir: PathBuf,
    serial: u64,
    id: u128,
}

impl Root {
    fn new(tag: &str) -> Self {
        let dir = std::env::temp_dir().join(format!("oneup-effects-{tag}-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        let (serial, id) = win::file_id(&win::open_dir(&dir).unwrap()).unwrap();
        Self { dir, serial, id: id.0 }
    }

    fn fs(&self) -> LocalFs {
        LocalFs::open(&self.dir, self.serial, self.id, "token-1", true).unwrap()
    }

    fn p(&self, rel: &str) -> PathBuf {
        self.dir.join(rel)
    }
}

impl Drop for Root {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.dir);
    }
}

fn entry(path: &Path) -> DirEntry {
    let parent = win::open_dir(path.parent().unwrap()).unwrap();
    let mut buf = vec![0u8; 64 * 1024];
    let mut v = Vec::new();
    win::list_dir(&parent, &mut buf, &mut v).unwrap();
    let name = path.file_name().unwrap().to_str().unwrap();
    v.into_iter().find(|e| e.name == name).unwrap()
}

fn id(path: &Path) -> LocalId {
    LocalId(entry(path).file_id.0)
}

/// The precondition a planner would have recorded for `path` right now.
fn pre(path: &Path) -> LocalPre {
    let e = entry(path);
    let stamp = (!e.is_dir()).then_some((e.size as i64, e.modified));
    LocalPre { id: LocalId(e.file_id.0), stamp }
}

fn approve(op: Op) -> Approved {
    approve_with(&Policy::default(), op)
}

fn approve_with(policy: &Policy, op: Op) -> Approved {
    let r = review_in(policy, &Context { synced_items: 1_000_000, ..Context::default() }, vec![op]);
    assert!(r.withheld.is_empty(), "{:?}", r.withheld);
    r.approved.into_iter().next().unwrap()
}

/// A recovery step the user asked for: its key approved, as the agent
/// approves a queued restore (`oneup_app::restores`).
fn approve_asked(op: Op) -> Approved {
    let keys = oneup_gate::rewind_keys(std::slice::from_ref(&op), &[String::new()]);
    let cx = Context { synced_items: 1_000_000, approved: &keys, ..Context::default() };
    let r = review_in(&Policy::default(), &cx, vec![op]);
    assert!(r.withheld.is_empty() && r.breakers.is_empty(), "{:?}", r.breakers);
    r.approved.into_iter().next().unwrap()
}

/// Puts kept item `slot` back at `to` (or where it came from), as a
/// restore asked for and approved: bound to the item and the folder as
/// found now.
fn restore(fs: &LocalFs, slot: &str, to: Option<&str>) -> Result<String, Error> {
    let t = fs.kept_target(slot, to)?;
    let step = oneup_sync::rewind::Step::RestoreKept { slot: slot.into(), item: t.item, dir: t.dir, name: t.name };
    fs.restore_approved(&approve_asked(Op::rewind(0, step)))
}

/// A recycle the user chose to discard (an overflow batch of one).
fn approve_discard(op: Op) -> Approved {
    let p = Policy::default();
    let cx = Context { synced_items: 1_000_000, room: Some(0), ..Context::default() };
    let fingerprint = review_in(&p, &cx, vec![op.clone()]).overflow.unwrap().fingerprint;
    let decisions = [Decision { fingerprint, choice: Choice::Discard }];
    let r = review_in(&p, &Context { decisions: &decisions, ..cx }, vec![op]);
    let a = r.approved.into_iter().next().unwrap();
    assert_eq!(a.old_copy(), OldCopy::DropIfSynced);
    a
}

fn qxh(content: &[u8]) -> Hash {
    let mut h = oneup_hash::QuickXorHash::new();
    h.update(content);
    h.finalize()
}

/// Changes a file's content and last-write time from a different
/// executable, as a user's app would.
fn edit(path: &Path, text: &str) {
    use std::os::windows::process::CommandExt as _;
    std::thread::sleep(std::time::Duration::from_millis(20));
    // `raw_arg`: cmd.exe has its own quoting rules, which Rust's argument
    // escaping would break.
    let status = std::process::Command::new("cmd.exe")
        .raw_arg(format!("/C echo {text}>> \"{}\"", path.display()))
        .status()
        .unwrap();
    assert!(status.success());
}

fn remote() -> RemotePre {
    RemotePre { id: RemoteId("r".into()), etag: "e".into() }
}

#[test]
fn identity_is_checked_and_engine_folder_set_up() {
    let r = Root::new("identity");
    // Not first use and no marker: refuse (could be a restored/replaced folder).
    let err = LocalFs::open(&r.dir, r.serial, r.id, "t", false).unwrap_err();
    assert!(matches!(err, Error::RootChanged(_)), "{err}");
    // Wrong file ID: refuse.
    let err = LocalFs::open(&r.dir, r.serial, r.id ^ 1, "t", true).unwrap_err();
    assert!(matches!(err, Error::RootChanged(_)), "{err}");

    let fs = LocalFs::open(&r.dir, r.serial, r.id, "t", true).unwrap();
    assert!(r.p(".oneup/versions").is_dir() && r.p(".oneup/staging").is_dir());
    fs.check_root().unwrap();
    drop(fs);
    // Another setup's token: refuse.
    let err = LocalFs::open(&r.dir, r.serial, r.id, "other", false).unwrap_err();
    assert!(matches!(err, Error::RootChanged(_)), "{err}");

    // Leftover partial downloads are cleared, anything else in staging stays.
    std::fs::write(r.p(".oneup/staging/1-1.oneup-part"), b"partial").unwrap();
    std::fs::write(r.p(".oneup/staging/user.txt"), b"not ours").unwrap();
    let fs = LocalFs::open(&r.dir, r.serial, r.id, "t", false).unwrap();
    assert!(!r.p(".oneup/staging/1-1.oneup-part").exists());
    assert!(r.p(".oneup/staging/user.txt").exists());

    // Losing the marker later halts.
    std::fs::rename(r.p(".oneup/root.marker"), r.p(".oneup/moved")).unwrap();
    assert!(matches!(fs.check_root(), Err(Error::RootChanged(_))));
}

/// If another program unregisters OneUpDrive's Files On-Demand folder (as
/// the OneDrive app's leftover unlinking could), Windows removes the
/// online-only placeholders: the folder must then fail its identity check,
/// in a running round and on the next start, so those removals never reach
/// OneDrive. OneUpDrive's own unregistering says so first and isn't halted.
#[test]
fn losing_the_on_demand_registration_halts() {
    let r = Root::new("lost-on-demand");
    drop(r.fs());
    oneup_cfapi::provider::register(&r.dir, b"token-1").unwrap();
    let fs = r.fs();
    assert!(fs.on_demand() && r.p(".oneup/on-demand").exists());
    fs.check_root().unwrap();

    // Another program unregisters it.
    oneup_cfapi::provider::unregister(&r.dir).unwrap();
    assert!(matches!(fs.check_root(), Err(Error::RootChanged(_))), "in a running round");
    drop(fs);
    let err = LocalFs::open(&r.dir, r.serial, r.id, "token-1", false).unwrap_err();
    assert!(matches!(err, Error::RootChanged(_)), "on the next start: {err}");

    // OneUpDrive's own way (`syncroot::unregister`) forgets it first.
    oneup_cfapi::provider::register(&r.dir, b"token-1").unwrap();
    drop(r.fs());
    LocalFs::forget_on_demand(&r.dir).unwrap();
    oneup_cfapi::provider::unregister(&r.dir).unwrap();
    let fs = r.fs();
    assert!(!fs.on_demand());
    fs.check_root().unwrap();
}

/// Our rename of a placeholder (which can clear its in-sync bit) leaves it
/// in sync only if it was in sync before: one a user edited, not yet sent,
/// stays "not in sync", so nothing frees it and loses the edit
/// (invariant 12).
#[test]
fn renaming_a_placeholder_keeps_an_edit_not_in_sync() {
    use oneup_cfapi::provider;
    let r = Root::new("rename-in-sync");
    drop(r.fs());
    provider::register(&r.dir, b"token-1").unwrap();
    let fs = r.fs();
    assert!(fs.on_demand());
    let root = fs.root_id();
    let in_sync = |p: &Path| provider::info(&provider::open(p).unwrap()).unwrap().unwrap().in_sync;
    for n in ["synced.txt", "edited.txt"] {
        std::fs::write(r.p(n), b"synced content").unwrap();
        provider::Exclusive::open(&r.p(n)).unwrap().convert(b"OD!x").unwrap();
        assert!(in_sync(&r.p(n)), "{n}");
    }
    edit(&r.p("edited.txt"), "edited here");
    assert!(!in_sync(&r.p("edited.txt")), "a program's edit clears the bit");

    let rename = |from: &str, to: &str| {
        fs.apply(&approve(Op::ConflictCopy { sid: 1, local: pre(&r.p(from)), dir: root, name: to.into() })).unwrap();
    };
    rename("synced.txt", "synced2.txt");
    rename("edited.txt", "edited2.txt");
    let result = (in_sync(&r.p("synced2.txt")), in_sync(&r.p("edited2.txt")));
    drop(fs);
    let _ = provider::unregister(&r.dir);
    assert_eq!(result, (true, false), "(was in sync, edited)");
}

/// Serves the first MiB of a placeholder's content, then fails the rest:
/// a download cut short. Counts the requests that reach it.
struct CutShort {
    data: Vec<u8>,
    fetches: std::sync::atomic::AtomicU32,
}

struct ServeCutShort(std::sync::Arc<CutShort>);

impl oneup_cfapi::provider::Provider for ServeCutShort {
    fn fetch_data(&self, fetch: oneup_cfapi::provider::Fetch) {
        let me = std::sync::Arc::clone(&self.0);
        std::thread::spawn(move || {
            me.fetches.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
            if fetch.offset == 0 {
                let _ = fetch.transfer(0, &me.data[..1 << 20]);
            }
            fetch.fail(oneup_cfapi::provider::NOT_IN_SYNC);
        });
    }
}

/// A placeholder partly here (a download cut short) holds only bytes of
/// its cloud version. Replaced by a cloud edit or removed after a cloud
/// deletion, it is never read: reading the rest would ask the cloud for a
/// version it no longer has, fail, and fail the operation every round.
#[test]
fn partly_downloaded_placeholders_change_without_being_read() {
    use oneup_cfapi::identity::Identity;
    use oneup_cfapi::provider::{self, NewPlaceholder};
    use std::sync::atomic::Ordering::Relaxed;
    oneup_cfapi::expose_placeholders().unwrap();
    let r = Root::new("partial");
    drop(r.fs());
    provider::register(&r.dir, b"token-1").unwrap();
    let data: Vec<u8> = (0..3 << 20).map(|i: u32| (i.wrapping_mul(2_654_435_761) >> 13) as u8).collect();
    let cloud = std::sync::Arc::new(CutShort { data: data.clone(), fetches: Default::default() });
    let conn = provider::connect(&r.dir, std::sync::Arc::new(ServeCutShort(std::sync::Arc::clone(&cloud)))).unwrap();
    let fs = r.fs();
    assert!(fs.on_demand());
    let root = fs.root_id();
    let size = data.len() as i64;
    let old = Identity { item: "P".into(), etag: "e1".into(), size, hash: qxh(&data) };
    let info = |p: &Path| provider::info(&provider::open(p).unwrap()).unwrap().unwrap();
    let names = ["replaced.bin", "removed.bin", "discarded.bin"];
    for name in names {
        let item = NewPlaceholder { name, identity: &old.encode(), size, modified: 0, folder: false };
        provider::create(&r.dir, &[item]).unwrap();
        // A program's read, cut short.
        use std::os::windows::process::CommandExt as _;
        let out = r.dir.with_extension("read");
        std::process::Command::new("cmd.exe")
            .raw_arg(format!("/C copy /Y /B \"{}\" \"{}\" >nul 2>nul", r.p(name).display(), out.display()))
            .status()
            .unwrap();
        let _ = std::fs::remove_file(out);
        let i = info(&r.p(name));
        assert!(i.on_disk > 0 && i.on_disk < size && i.in_sync && i.modified == 0, "{name}: {i:?}");
    }
    let asked = cloud.fetches.load(Relaxed);

    // A cloud edit: dehydrated with the new identity, no copy kept.
    let new = Identity { item: "P".into(), etag: "e2".into(), size: 11, hash: qxh(b"new version") };
    let replace = Op::Download {
        remote: remote(),
        to: root,
        parent: 0,
        name: names[0].into(),
        replace: Some(pre(&r.p(names[0]))),
        link: Link::Existing(1),
    };
    let v = oneup_effects::Version { identity: new.clone(), modified: 0 };
    let replaced = fs.replace_placeholder(&approve(replace), &v, Some(&old.hash));
    // A cloud deletion, kept or discarded: removed, nothing kept.
    let removed = fs.apply(&approve(Op::RecycleLocal { sid: 2, local: pre(&r.p(names[1])) }));
    let discard = approve_discard(Op::RecycleLocal { sid: 3, local: pre(&r.p(names[2])) });
    let discarded = fs.recycle(&discard, Some(&old.hash));
    let after = info(&r.p(names[0]));
    let (gone, versions) = ([1, 2].map(|i| !r.p(names[i]).exists()), fs.versions().unwrap());
    let fetched = cloud.fetches.load(Relaxed) - asked;
    drop(fs);
    drop(conn);
    let _ = provider::unregister(&r.dir);

    assert!(matches!(replaced, Ok(Done::Placed { kept: None, .. })), "{replaced:?}");
    assert_eq!((after.on_disk, after.identity), (0, new.encode()), "dehydrated with the new identity");
    assert!(matches!(removed, Ok(Done::Recycled { kept: None })), "{removed:?}");
    assert!(matches!(discarded, Ok(Done::Recycled { kept: None })), "{discarded:?}");
    assert_eq!(gone, [true, true]);
    assert!(versions.is_empty(), "{versions:?}");
    assert_eq!(fetched, 0, "nothing asked for the old version");
}

#[test]
fn mkdir_never_reuses_a_name_and_checks_names() {
    let r = Root::new("mkdir");
    let fs = r.fs();
    let root = fs.root_id();
    let mk = |name: &str| {
        fs.apply(&approve(Op::MkdirLocal { remote: remote(), to: root, parent: 0, name: name.into(), link: Link::New }))
    };
    let Done::Placed { parent, entry, .. } = mk("Docs").unwrap() else { panic!() };
    assert_eq!((parent, entry.name.as_str()), (root, "Docs"));
    assert!(entry.is_dir() && r.p("Docs").is_dir());
    assert!(matches!(mk("docs"), Err(Error::NameTaken)), "case-insensitively taken");
    for bad in ["..", "a\\b", ".oneup", "NUL", "x."] {
        assert!(matches!(mk(bad), Err(Error::BadName)), "{bad}");
    }
    // Deeper, `.oneup` is an ordinary name.
    let docs = id(&r.p("Docs"));
    fs.apply(&approve(Op::MkdirLocal {
        remote: remote(),
        to: docs,
        parent: 1,
        name: ".oneup".into(),
        link: Link::New,
    }))
    .unwrap();
    assert!(r.p("Docs/.oneup").is_dir());
}

#[test]
fn moves_check_the_stamp_and_never_replace() {
    let r = Root::new("move");
    let fs = r.fs();
    std::fs::create_dir(r.p("sub")).unwrap();
    std::fs::write(r.p("a.txt"), b"hello").unwrap();
    std::fs::write(r.p("taken.txt"), b"keep me").unwrap();
    let sub = id(&r.p("sub"));
    let root = fs.root_id();
    let mv = |pre: LocalPre, to: LocalId, name: &str| {
        fs.apply(&approve(Op::MoveLocal { sid: 1, local: pre, to, parent: 0, name: name.into() }))
    };

    // Planned, then edited by the user: refused, untouched.
    let stale = pre(&r.p("a.txt"));
    edit(&r.p("a.txt"), "more");
    assert!(matches!(mv(stale, sub, "a.txt"), Err(Error::Changed)));
    assert!(r.p("a.txt").exists());

    // Target taken: refused, both files intact.
    assert!(matches!(mv(pre(&r.p("a.txt")), root, "taken.txt"), Err(Error::NameTaken)));
    assert_eq!(std::fs::read(r.p("taken.txt")).unwrap(), b"keep me");

    // Open in another program: refused.
    let held = std::fs::File::open(r.p("a.txt")).unwrap();
    assert!(matches!(mv(pre(&r.p("a.txt")), sub, "b.txt"), Err(Error::Busy)));
    drop(held);

    // A real move keeps the file ID and reports where it went.
    let a = pre(&r.p("a.txt"));
    let Done::Placed { parent, entry, .. } = mv(a.clone(), sub, "b.txt").unwrap() else { panic!() };
    assert_eq!((parent, entry.name.as_str(), LocalId(entry.file_id.0)), (sub, "b.txt", a.id));
    assert!(r.p("sub/b.txt").exists() && !r.p("a.txt").exists());

    // Case-only rename in place.
    let b = pre(&r.p("sub/b.txt"));
    fs.apply(&approve(Op::RenameLocal { local: b.clone(), dir: sub, name: "B.txt".into() })).unwrap();
    assert_eq!(entry_name(&r.p("sub"), b.id), "B.txt");

    // A rename "in place" in a folder it isn't in: refused.
    let err =
        fs.apply(&approve(Op::ConflictCopy { sid: 1, local: pre(&r.p("sub/B.txt")), dir: root, name: "c.txt".into() }));
    assert!(matches!(err, Err(Error::Changed)));

    // A folder moves with its contents.
    let Done::Placed { entry, .. } = mv(pre(&r.p("sub")), root, "renamed").unwrap() else { panic!() };
    assert!(entry.is_dir() && r.p("renamed/B.txt").exists());
}

fn entry_name(dir: &Path, id: LocalId) -> String {
    let h = win::open_dir(dir).unwrap();
    let mut buf = vec![0u8; 64 * 1024];
    let mut v = Vec::new();
    win::list_dir(&h, &mut buf, &mut v).unwrap();
    v.into_iter().find(|e| e.file_id.0 == id.0).unwrap().name
}

#[test]
fn recycling_keeps_everything_in_the_version_store() {
    let r = Root::new("recycle");
    let fs = r.fs();
    std::fs::create_dir_all(r.p("full/inner")).unwrap();
    std::fs::create_dir(r.p("empty")).unwrap();
    std::fs::write(r.p("gone.txt"), b"bye").unwrap();
    let rc = |pre: LocalPre| fs.apply(&approve(Op::RecycleLocal { sid: 1, local: pre }));

    let stale = pre(&r.p("gone.txt"));
    edit(&r.p("gone.txt"), "changed");
    assert!(matches!(rc(stale), Err(Error::Changed)));
    let Done::Recycled { kept: Some(kept) } = rc(pre(&r.p("gone.txt"))).unwrap() else { panic!() };
    assert!(!r.p("gone.txt").exists());
    assert!(std::fs::read_to_string(&kept).unwrap().starts_with("bye"));
    assert_eq!(kept.file_name().unwrap(), "gone.txt");
    assert_eq!(std::fs::read_to_string(kept.parent().unwrap().join("origin.txt")).unwrap(), "gone.txt");

    assert!(matches!(rc(pre(&r.p("full"))), Err(Error::NotEmpty)));
    assert!(r.p("full/inner").is_dir());
    let Done::Recycled { kept: Some(kept) } = rc(pre(&r.p("empty"))).unwrap() else { panic!() };
    assert!(kept.is_dir() && !r.p("empty").exists());

    // The sync folder itself and our own folder are never targets.
    let root = LocalPre { id: fs.root_id(), stamp: None };
    assert!(matches!(rc(root), Err(Error::Outside)));
    assert!(matches!(rc(pre(&r.p(".oneup/versions"))), Err(Error::Outside)));
}

#[test]
fn kept_items_are_listed_restored_without_replacing_and_pruned() {
    let r = Root::new("restore");
    let fs = r.fs();
    std::fs::create_dir(r.p("docs")).unwrap();
    std::fs::write(r.p("docs/a.txt"), b"first").unwrap();
    std::fs::write(r.p("b.txt"), b"bee").unwrap();
    for p in ["docs/a.txt", "b.txt"] {
        fs.apply(&approve(Op::RecycleLocal { sid: 1, local: pre(&r.p(p)) })).unwrap();
    }
    let slots = fs.versions().unwrap();
    assert_eq!(slots.iter().map(|s| s.name.as_str()).collect::<Vec<_>>(), ["a.txt", "b.txt"]);
    assert_eq!(slots[0].origin.as_deref(), Some("docs\\a.txt"));
    assert_eq!(slots[0].bytes, 5);

    // Back where it came from; the slot is gone.
    assert_eq!(restore(&fs, &slots[0].id, None).unwrap(), "docs\\a.txt");
    assert_eq!(std::fs::read(r.p("docs/a.txt")).unwrap(), b"first");
    assert_eq!(fs.versions().unwrap().len(), 1);

    // The name is taken meanwhile: restored beside it, nothing replaced.
    std::fs::write(r.p("b.txt"), b"new bee").unwrap();
    assert_eq!(restore(&fs, &slots[1].id, None).unwrap(), "b (restored 1).txt");
    assert_eq!(std::fs::read(r.p("b.txt")).unwrap(), b"new bee");
    assert_eq!(std::fs::read(r.p("b (restored 1).txt")).unwrap(), b"bee");

    // Never into OneUpDrive's own folder, never outside, never a bad slot.
    fs.apply(&approve(Op::RecycleLocal { sid: 1, local: pre(&r.p("b.txt")) })).unwrap();
    let id = fs.versions().unwrap()[0].id.clone();
    assert!(matches!(restore(&fs, &id, Some(".oneup\\x.txt")), Err(Error::BadName)));
    assert!(matches!(restore(&fs, &id, Some("..\\x.txt")), Err(Error::BadName)));
    assert!(matches!(restore(&fs, "..", None), Err(Error::BadName)));
    assert!(matches!(restore(&fs, &id, Some("missing\\x.txt")), Err(Error::Changed)));

    // Bound to what was chosen: the kept item by its ID and stamp, the
    // folder by its ID. A kept file changed since, or a folder swapped for
    // another of the same name, isn't acted on.
    std::fs::create_dir(r.p("dest")).unwrap();
    let step = |t: &oneup_effects::KeptTarget| {
        Op::rewind(
            0,
            oneup_sync::rewind::Step::RestoreKept {
                slot: id.clone(),
                item: t.item.clone(),
                dir: t.dir,
                name: t.name.clone(),
            },
        )
    };
    let t = fs.kept_target(&id, Some("dest\\x.txt")).unwrap();
    let kept = r.p(&format!(".oneup/versions/{id}/b.txt"));
    std::fs::write(&kept, b"changed in the store").unwrap();
    assert!(matches!(fs.restore_approved(&approve_asked(step(&t))), Err(Error::Changed)), "item changed");
    let t = fs.kept_target(&id, Some("dest\\x.txt")).unwrap();
    std::fs::rename(r.p("dest"), r.p("dest-was")).unwrap();
    std::fs::create_dir(r.p("dest")).unwrap();
    // The folder chosen, wherever it is now; never another of its name.
    assert_eq!(fs.restore_approved(&approve_asked(step(&t))).unwrap(), "dest-was\\x.txt");
    assert!(!r.p("dest/x.txt").exists());
    assert_eq!(std::fs::read(r.p("dest-was/x.txt")).unwrap(), b"changed in the store");
    // Once done, the same step finds nothing to act on.
    assert!(fs.restore_approved(&approve_asked(step(&t))).is_err());
    // Kept again, as it was, for what follows.
    std::fs::write(r.p("dest-was/x.txt"), b"bee").unwrap();
    std::fs::rename(r.p("dest-was/x.txt"), r.p("b.txt")).unwrap();
    fs.apply(&approve(Op::RecycleLocal { sid: 1, local: pre(&r.p("b.txt")) })).unwrap();

    // Pruning: a preview removes nothing; by size, oldest first.
    std::fs::write(r.p("c.txt"), b"0123456789").unwrap();
    fs.apply(&approve(Op::RecycleLocal { sid: 1, local: pre(&r.p("c.txt")) })).unwrap();
    let would = fs.prune_versions(None, Some(10), true).unwrap();
    assert_eq!(would.iter().map(|s| s.name.as_str()).collect::<Vec<_>>(), ["b.txt"]);
    assert_eq!(fs.versions().unwrap().len(), 2);
    fs.prune_versions(None, Some(10), false).unwrap();
    assert_eq!(fs.versions().unwrap().iter().map(|s| s.name.as_str()).collect::<Vec<_>>(), ["c.txt"]);
    // By age: everything kept before now.
    fs.prune_versions(Some(i64::MAX), None, false).unwrap();
    assert!(fs.versions().unwrap().is_empty());
}

/// Listing the store measures each slot once: later listings take its
/// size from `.oneup\versions.cache`, checked against the slot folder's
/// creation time, and follow slots added and removed. A missing or
/// damaged cache only costs measuring again.
#[test]
fn the_version_store_is_measured_once_per_slot() {
    let r = Root::new("versions-cache");
    let fs = r.fs();
    let cache = r.p(".oneup/versions.cache");
    let keep = |name: &str, content: &[u8]| {
        std::fs::write(r.p(name), content).unwrap();
        fs.apply(&approve(Op::RecycleLocal { sid: 1, local: pre(&r.p(name)) })).unwrap();
    };
    let bytes = || fs.versions().unwrap().iter().map(|s| (s.name.clone(), s.bytes)).collect::<Vec<_>>();
    // Rewrites one field of every cache line.
    let set_field = |n: usize, value: &str| {
        let text = std::fs::read_to_string(&cache).unwrap();
        let lines: Vec<String> = text
            .lines()
            .map(|l| {
                let mut f: Vec<&str> = l.split('\t').collect();
                f[n] = value;
                f.join("\t")
            })
            .collect();
        std::fs::write(&cache, lines.join("\n") + "\n").unwrap();
    };

    keep("a.txt", b"12345");
    assert_eq!(bytes(), [("a.txt".to_string(), 5)]);
    assert_eq!(std::fs::read_to_string(&cache).unwrap().lines().count(), 1);

    // Not measured again: the cache is what's read.
    set_field(2, "999");
    assert_eq!(bytes(), [("a.txt".to_string(), 999)]);
    // Of another slot folder (another creation time): measured.
    set_field(1, "1");
    assert_eq!(bytes(), [("a.txt".to_string(), 5)]);
    // Damaged or missing: measured, and written again.
    std::fs::write(&cache, "garbage\tline\n").unwrap();
    assert_eq!(bytes(), [("a.txt".to_string(), 5)]);
    std::fs::remove_file(&cache).unwrap();
    assert_eq!(bytes(), [("a.txt".to_string(), 5)]);
    assert!(cache.exists());

    // Slots added and removed.
    std::thread::sleep(std::time::Duration::from_millis(5));
    keep("b.txt", b"0123456789");
    assert_eq!(bytes(), [("a.txt".to_string(), 5), ("b.txt".to_string(), 10)]);
    let a = fs.versions().unwrap()[0].id.clone();
    restore(&fs, &a, None).unwrap();
    assert_eq!(bytes(), [("b.txt".to_string(), 10)]);
    assert_eq!(std::fs::read_to_string(&cache).unwrap().lines().count(), 1);
    fs.prune_versions(Some(i64::MAX), None, false).unwrap();
    assert!(bytes().is_empty());
    assert_eq!(std::fs::read_to_string(&cache).unwrap(), "");
    // Only the cache itself in our folder: no temp files left.
    let ours: Vec<_> = std::fs::read_dir(r.p(".oneup")).unwrap().map(|e| e.unwrap().file_name()).collect();
    assert!(!ours.iter().any(|n| n.to_string_lossy().ends_with(".tmp")), "{ours:?}");
}

#[test]
fn automatic_trimming_follows_the_rule() {
    let r = Root::new("trim");
    let fs = r.fs();
    for (name, content) in [("old.txt", &b"0123456789"[..]), ("new.txt", &b"abcdefghij"[..])] {
        std::fs::write(r.p(name), content).unwrap();
        fs.apply(&approve(Op::RecycleLocal { sid: 1, local: pre(&r.p(name)) })).unwrap();
        std::thread::sleep(std::time::Duration::from_millis(5));
    }
    // The defaults protect anything younger than 30 days: nothing goes.
    assert!(fs.trim_versions(&oneup_effects::Trim::default()).unwrap().is_empty());
    // No age protection and a 15-byte cap: the oldest goes, the store fits.
    let tight =
        oneup_effects::Trim { min_age_days: 0, max_gib: 15.0 / (1u64 << 30) as f64, max_percent: 0.0, rotate: false };
    let gone = fs.trim_versions(&tight).unwrap();
    assert_eq!(gone.iter().map(|s| s.name.as_str()).collect::<Vec<_>>(), ["old.txt"]);
    assert_eq!(fs.versions().unwrap().iter().map(|s| s.name.as_str()).collect::<Vec<_>>(), ["new.txt"]);
    // Both size limits off: never.
    let off = oneup_effects::Trim { min_age_days: 0, max_gib: 0.0, max_percent: 0.0, rotate: false };
    assert!(fs.trim_versions(&off).unwrap().is_empty());
}

#[test]
fn downloads_place_new_files_and_replace_only_the_planned_one() {
    let r = Root::new("download");
    let fs = r.fs();
    let root = fs.root_id();
    std::fs::write(r.p("doc.txt"), b"old content").unwrap();
    std::fs::write(r.p("taken.txt"), b"mine").unwrap();
    let staged = |text: &[u8]| {
        let mut s = fs.stage().unwrap();
        s.file().unwrap().write_all(text).unwrap();
        s
    };
    let dl = |name: &str, replace: Option<LocalPre>| {
        approve(Op::Download { remote: remote(), to: root, parent: 0, name: name.into(), replace, link: Link::New })
    };
    let staging_files = || std::fs::read_dir(r.p(".oneup/staging")).unwrap().count();

    // New file.
    let Done::Placed { entry, kept, .. } = fs.place_download(&dl("new.txt", None), staged(b"fresh"), None).unwrap()
    else {
        panic!()
    };
    assert_eq!((entry.name.as_str(), entry.size, kept), ("new.txt", 5, None));
    assert_eq!(std::fs::read(r.p("new.txt")).unwrap(), b"fresh");

    // A taken name is never overwritten; the staged file is cleaned up.
    let err = fs.place_download(&dl("Taken.txt", None), staged(b"theirs"), None).unwrap_err();
    assert!(matches!(err, Error::NameTaken), "{err}");
    assert_eq!(std::fs::read(r.p("taken.txt")).unwrap(), b"mine");
    assert_eq!(staging_files(), 0);

    // Replacing a file the user edited after planning: refused.
    let stale = pre(&r.p("doc.txt"));
    edit(&r.p("doc.txt"), "edit");
    let err = fs.place_download(&dl("doc.txt", Some(stale)), staged(b"cloud"), None).unwrap_err();
    assert!(matches!(err, Error::Changed), "{err}");
    assert!(std::fs::read_to_string(r.p("doc.txt")).unwrap().starts_with("old content"));

    // Replacing the planned file without a synced hash to prove it's in
    // OneDrive: the old content goes to the version store.
    let current = pre(&r.p("doc.txt"));
    let Done::Placed { entry, kept, .. } =
        fs.place_download(&dl("doc.txt", Some(current)), staged(b"cloud"), None).unwrap()
    else {
        panic!()
    };
    assert_eq!(std::fs::read(r.p("doc.txt")).unwrap(), b"cloud");
    assert_eq!(entry.name, "doc.txt");
    let kept = kept.unwrap();
    assert!(std::fs::read_to_string(kept).unwrap().starts_with("old content"));
    assert_eq!(staging_files(), 0);

    // Replace with the wrong name planned: refused.
    let err = fs.place_download(&dl("other.txt", Some(pre(&r.p("doc.txt")))), staged(b"x"), None).unwrap_err();
    assert!(matches!(err, Error::Changed), "{err}");

    // Downloads only through `place_download`.
    assert!(matches!(fs.apply(&dl("z.txt", None)), Err(Error::WrongOp)));
}

fn upload_of(path: &Path) -> Approved {
    approve(Op::Upload {
        local: pre(path),
        to: RemoteId("root".into()),
        parent: 0,
        name: "f.bin".into(),
        replace: None,
        link: Link::New,
    })
}

#[test]
fn hashing_and_upload_reads_let_others_read() {
    let r = Root::new("read");
    let fs = r.fs();
    std::fs::write(r.p("f.bin"), b"some bytes to hash").unwrap();
    let Done::Hashed { hash, size, .. } = fs.apply(&approve(Op::Hash(pre(&r.p("f.bin"))))).unwrap() else { panic!() };
    let mut expect = oneup_hash::QuickXorHash::new();
    expect.update(b"some bytes to hash");
    assert_eq!((hash, size), (expect.finalize(), 18));

    let mut src = fs.open_upload(&upload_of(&r.p("f.bin"))).unwrap();
    assert!(std::fs::File::open(r.p("f.bin")).is_ok());
    let mut s = String::new();
    src.read_to_string(&mut s).unwrap();
    assert_eq!(s, "some bytes to hash");
    assert!(src.unchanged());
    drop(src);

    // Outside the sync folder: refused even with a matching stamp.
    let outside = r.dir.with_extension("outside.txt");
    std::fs::write(&outside, b"x").unwrap();
    let err = fs.apply(&approve(Op::Hash(pre(&outside))));
    std::fs::remove_file(&outside).unwrap();
    assert!(matches!(err, Err(Error::Outside)), "{err:?}");
}

#[test]
fn a_save_in_place_during_an_upload_read_goes_through_and_fails_the_read() {
    let r = Root::new("read-saved");
    let fs = r.fs();
    std::fs::write(r.p("f.bin"), b"some bytes to hash").unwrap();
    let mut src = fs.open_upload(&upload_of(&r.p("f.bin"))).unwrap();
    let mut head = [0u8; 4];
    src.read_exact(&mut head).unwrap();
    std::fs::OpenOptions::new().write(true).open(r.p("f.bin")).unwrap().write_all(b"SOME").unwrap();
    let err = src.read_to_string(&mut String::new()).unwrap_err();
    assert!(oneup_effects::ChangedMeanwhile::is(&err), "{err}");
    assert!(!src.unchanged());
}

/// A save by renaming a new file over the one being read goes through,
/// however the program renames: with POSIX semantics (Rust's), or
/// `MoveFileEx` from another program (`cmd`'s `move /y`), which fails while
/// any other handle on the file is open. The read then fails, so the
/// upload is cancelled before it commits.
#[test]
fn a_save_by_rename_during_an_upload_read_goes_through_and_fails_the_read() {
    let r = Root::new("read-renamed");
    let fs = r.fs();
    for classic in [false, true] {
        std::fs::write(r.p("f.bin"), b"some bytes to hash").unwrap();
        let mut src = fs.open_upload(&upload_of(&r.p("f.bin"))).unwrap();
        let mut head = [0u8; 4];
        src.read_exact(&mut head).unwrap();
        std::fs::write(r.p("f.tmp"), b"new").unwrap();
        if classic {
            let out = std::process::Command::new("cmd.exe")
                .current_dir(&r.dir)
                .args(["/c", "move", "/y", "f.tmp", "f.bin"])
                .output()
                .unwrap();
            assert!(out.status.success(), "the save must not fail: {out:?}");
        } else {
            std::fs::rename(r.p("f.tmp"), r.p("f.bin")).unwrap();
        }
        assert_eq!(std::fs::read(r.p("f.bin")).unwrap(), b"new");
        let err = src.read_to_string(&mut String::new()).unwrap_err();
        assert!(oneup_effects::ChangedMeanwhile::is(&err), "{err}");
        assert!(!src.unchanged());
    }
}

/// In a Files On-Demand folder the placeholder checks (is all of it
/// here, does its identity vouch for it) go through the read's own handle,
/// so no other handle is ever open, and they queue nothing on its
/// completion port. A save by `MoveFileEx` during the read goes through.
#[test]
fn on_demand_reads_hold_one_handle_and_saves_go_through() {
    let r = Root::new("read-on-demand");
    drop(r.fs());
    oneup_cfapi::provider::register(&r.dir, b"token-1").unwrap();
    let fs = r.fs();
    assert!(fs.on_demand());
    let ignored = win::ignored_completions();
    std::fs::write(r.p("f.bin"), b"some bytes to hash").unwrap();
    let Done::Hashed { size, .. } = fs.apply(&approve(Op::Hash(pre(&r.p("f.bin"))))).unwrap() else { panic!() };
    assert_eq!(size, 18);
    let mut src = fs.open_upload(&upload_of(&r.p("f.bin"))).unwrap();
    let mut head = [0u8; 4];
    src.read_exact(&mut head).unwrap();
    std::fs::write(r.p("f.tmp"), b"new").unwrap();
    let out = std::process::Command::new("cmd.exe")
        .current_dir(&r.dir)
        .args(["/c", "move", "/y", "f.tmp", "f.bin"])
        .output()
        .unwrap();
    assert!(out.status.success(), "the save must not fail: {out:?}");
    let err = src.read_to_string(&mut String::new()).unwrap_err();
    assert!(oneup_effects::ChangedMeanwhile::is(&err), "{err}");
    drop((src, fs));
    assert_eq!(win::ignored_completions(), ignored, "nothing but our own requests on the port");
    oneup_cfapi::provider::unregister(&r.dir).unwrap();
}

/// A file planned as an ordinary one that has become placeholder-like
/// since (marked offline, as an archiver does; same size and time) is
/// refused on the handle opened to read it, before any byte is read.
#[test]
fn a_file_that_became_offline_since_planning_is_refused_before_reading() {
    let r = Root::new("read-offline");
    let fs = r.fs();
    std::fs::write(r.p("f.bin"), b"some bytes to hash").unwrap();
    let (hash, upload) = (approve(Op::Hash(pre(&r.p("f.bin")))), upload_of(&r.p("f.bin")));
    let st = std::process::Command::new("attrib").arg("+O").arg(r.p("f.bin")).status().unwrap();
    assert!(st.success());
    assert!(matches!(fs.apply(&hash), Err(Error::NotOrdinary)));
    assert!(matches!(fs.open_upload(&upload), Err(Error::NotOrdinary)));
    let Op::Upload { local, .. } = upload.op() else { panic!() };
    assert_eq!(fs.sample(local, 64), None);
}

#[test]
fn replaced_files_are_dropped_only_when_they_are_the_synced_version() {
    let r = Root::new("replace-drop");
    let fs = r.fs();
    let root = fs.root_id();
    let staged = |text: &[u8]| {
        let mut s = fs.stage().unwrap();
        s.file().unwrap().write_all(text).unwrap();
        s
    };
    let dl = |policy: &Policy, replace: LocalPre| {
        let op = Op::Download {
            remote: remote(),
            to: root,
            parent: 0,
            name: "doc.txt".into(),
            replace: Some(replace),
            link: Link::Existing(1),
        };
        approve_with(policy, op)
    };

    // Unchanged since the last sync: OneDrive's version history has it, so
    // no local copy is kept.
    std::fs::write(r.p("doc.txt"), b"version 1").unwrap();
    let a = dl(&Policy::default(), pre(&r.p("doc.txt")));
    assert_eq!(a.old_copy(), OldCopy::DropIfSynced);
    let Done::Placed { kept, .. } = fs.place_download(&a, staged(b"version 2"), Some(&qxh(b"version 1"))).unwrap()
    else {
        panic!()
    };
    assert_eq!(kept, None);
    assert_eq!(std::fs::read(r.p("doc.txt")).unwrap(), b"version 2");
    assert!(fs.versions().unwrap().is_empty(), "nothing kept, no empty slot left");

    // Same size and time but other content (an edit the stamp missed):
    // not provably in OneDrive, so kept.
    let a = dl(&Policy::default(), pre(&r.p("doc.txt")));
    let Done::Placed { kept, .. } = fs.place_download(&a, staged(b"version 3"), Some(&qxh(b"version X"))).unwrap()
    else {
        panic!()
    };
    assert_eq!(std::fs::read(kept.unwrap()).unwrap(), b"version 2");

    // The user asked to keep replaced files: kept even when provable.
    let keep = Policy { keep_replaced: true, ..Policy::default() };
    let a = dl(&keep, pre(&r.p("doc.txt")));
    assert_eq!(a.old_copy(), OldCopy::Keep);
    let Done::Placed { kept, .. } = fs.place_download(&a, staged(b"version 4"), Some(&qxh(b"version 3"))).unwrap()
    else {
        panic!()
    };
    assert_eq!(std::fs::read(kept.unwrap()).unwrap(), b"version 3");
    assert_eq!(fs.versions().unwrap().len(), 2);
}

#[test]
fn discarding_removes_only_the_synced_version_and_never_through_links() {
    let r = Root::new("discard");
    let fs = r.fs();
    let rc = |pre: LocalPre, synced: &[u8]| {
        fs.recycle(&approve_discard(Op::RecycleLocal { sid: 1, local: pre }), Some(&qxh(synced)))
    };

    // The last synced version: removed, nothing kept.
    std::fs::write(r.p("big.bin"), b"synced bytes").unwrap();
    let Done::Recycled { kept } = rc(pre(&r.p("big.bin")), b"synced bytes").unwrap() else { panic!() };
    assert_eq!(kept, None);
    assert!(!r.p("big.bin").exists());
    assert!(fs.versions().unwrap().is_empty());

    // Read-only files too.
    std::fs::write(r.p("ro.txt"), b"ro").unwrap();
    let mut perm = std::fs::metadata(r.p("ro.txt")).unwrap().permissions();
    perm.set_readonly(true);
    std::fs::set_permissions(r.p("ro.txt"), perm).unwrap();
    let Done::Recycled { kept } = rc(pre(&r.p("ro.txt")), b"ro").unwrap() else { panic!() };
    assert_eq!(kept, None);
    assert!(!r.p("ro.txt").exists());

    // Other content: kept in the store instead.
    std::fs::write(r.p("other.txt"), b"not what was synced").unwrap();
    let Done::Recycled { kept } = rc(pre(&r.p("other.txt")), b"synced").unwrap() else { panic!() };
    assert_eq!(std::fs::read(kept.unwrap()).unwrap(), b"not what was synced");

    // Changed since planning: refused, untouched.
    std::fs::write(r.p("edited.txt"), b"synced").unwrap();
    let stale = pre(&r.p("edited.txt"));
    edit(&r.p("edited.txt"), "more");
    assert!(matches!(rc(stale, b"synced"), Err(Error::Changed)));
    assert!(r.p("edited.txt").exists());

    // Without the user's discard, a recycle keeps it even if provable.
    std::fs::write(r.p("plain.txt"), b"p").unwrap();
    let a = approve(Op::RecycleLocal { sid: 1, local: pre(&r.p("plain.txt")) });
    let Done::Recycled { kept } = fs.recycle(&a, Some(&qxh(b"p"))).unwrap() else { panic!() };
    assert!(kept.is_some());

    // A junction to a folder outside: never acted on, whatever was planned.
    let outside = r.dir.with_extension("outside");
    std::fs::create_dir_all(&outside).unwrap();
    std::fs::write(outside.join("precious.txt"), b"precious").unwrap();
    let link = r.p("link");
    let status =
        std::process::Command::new("cmd.exe").args(["/C", "mklink", "/J"]).arg(&link).arg(&outside).output().unwrap();
    assert!(status.status.success(), "{status:?}");
    let as_file = LocalPre { id: id(&link), stamp: Some((1, entry(&link).modified)) };
    let err = rc(as_file, b"").unwrap_err();
    let as_dir = LocalPre { id: id(&link), stamp: None };
    // (Folders are never in a discard batch; an ordinary recycle refuses it.)
    let err2 = fs.recycle(&approve(Op::RecycleLocal { sid: 1, local: as_dir }), None).unwrap_err();
    let survived = std::fs::read(outside.join("precious.txt")).unwrap();
    let _ = std::fs::remove_dir(&link);
    let _ = std::fs::remove_dir_all(&outside);
    assert!(matches!(err, Error::NotOrdinary | Error::Changed), "{err}");
    assert!(matches!(err2, Error::NotOrdinary), "{err2}");
    assert_eq!(survived, b"precious");
}

#[test]
fn rotation_makes_room_by_removing_the_oldest() {
    let r = Root::new("rotate");
    let fs = r.fs();
    for name in ["a.txt", "b.txt", "c.txt"] {
        std::fs::write(r.p(name), b"0123456789").unwrap();
        fs.apply(&approve(Op::RecycleLocal { sid: 1, local: pre(&r.p(name)) })).unwrap();
        std::thread::sleep(std::time::Duration::from_millis(5));
    }
    // A 25-byte cap holding 30 recent bytes: no room without rotation, all
    // of the cap with it.
    let cap = 25.0 / (1u64 << 30) as f64;
    let plain = oneup_effects::Trim { min_age_days: 30, max_gib: cap, max_percent: 0.0, rotate: false };
    let rotate = oneup_effects::Trim { rotate: true, ..plain };
    assert_eq!(fs.room(&plain).unwrap(), Some(0));
    assert_eq!(fs.room(&rotate).unwrap(), Some(25));
    assert!(fs.make_room(&plain, 10).unwrap().is_empty());
    // 10 more bytes: 40 > 25, so the two oldest go (20 bytes).
    let gone = fs.make_room(&rotate, 10).unwrap();
    assert_eq!(gone.iter().map(|s| s.name.as_str()).collect::<Vec<_>>(), ["a.txt", "b.txt"]);
    // More than the whole cap: nothing is removed for nothing.
    assert!(fs.make_room(&rotate, 26).unwrap().is_empty());
    assert_eq!(fs.versions().unwrap().len(), 1);
}

/// With a locator, items are found by path where the state database has
/// them, never opened by ID; a path leading to another item is never acted
/// on: the item is then found by ID.
#[test]
fn items_are_found_where_the_store_has_them_and_only_there() {
    use std::collections::HashMap;
    use std::sync::{Arc, Mutex};
    let r = Root::new("locate");
    let mut fs = r.fs();
    let paths: Arc<Mutex<HashMap<LocalId, String>>> = Arc::default();
    let known = Arc::clone(&paths);
    fs.set_locator(Box::new(move |id| known.lock().unwrap().get(&id).cloned()));
    std::fs::create_dir(r.p("sub")).unwrap();
    std::fs::write(r.p("f.bin"), b"the planned file").unwrap();
    std::fs::write(r.p("other.bin"), b"another file!!!!").unwrap();
    let (f, other) = (id(&r.p("f.bin")), id(&r.p("other.bin")));
    let hash = |fs: &LocalFs| match fs.apply(&approve(Op::Hash(pre(&r.p("f.bin"))))).unwrap() {
        Done::Hashed { hash, .. } => hash,
        _ => panic!(),
    };

    paths.lock().unwrap().insert(f, "/f.bin".into());
    assert_eq!(hash(&fs), qxh(b"the planned file"));
    let mut src = fs.open_upload(&upload_of(&r.p("f.bin"))).unwrap();
    src.read_to_string(&mut String::new()).unwrap();
    drop(src);
    assert!(fs.is_present(f) && fs.presence(f) == Some(true));
    assert_eq!(fs.name_of(f).as_deref(), Some("f.bin"));
    assert_eq!(fs.opened_by_id(), 0, "found where the store has it");

    // The store is behind: its path leads to another file of the same size.
    paths.lock().unwrap().insert(f, "/other.bin".into());
    assert_eq!(hash(&fs), qxh(b"the planned file"), "the planned file, not the one at the path");
    assert_eq!(fs.opened_by_id(), 1);
    let sub = id(&r.p("sub"));
    let mv = fs.apply(&approve(Op::MoveLocal {
        sid: 1,
        local: pre(&r.p("f.bin")),
        to: sub,
        parent: 0,
        name: "f.bin".into(),
    }));
    assert!(mv.is_ok(), "{mv:?}");
    assert!(r.p("sub/f.bin").exists() && r.p("other.bin").exists() && !r.p("f.bin").exists());
    assert_eq!(std::fs::read(r.p("other.bin")).unwrap(), b"another file!!!!");
    assert_eq!(fs.name_of(other).as_deref(), Some("other.bin"));

    // Paths that aren't one name per level are no hint; a path to nothing
    // isn't taken for the item being gone.
    for bad in ["/../f.bin", "/sub/./f.bin", r"/sub\f.bin"] {
        paths.lock().unwrap().insert(f, bad.into());
        let before = fs.opened_by_id();
        assert_eq!(fs.name_of(f).as_deref(), Some("f.bin"), "{bad}");
        assert_eq!(fs.opened_by_id(), before + 1, "{bad}");
    }
    paths.lock().unwrap().insert(f, "/f.bin".into());
    assert_eq!(fs.presence(f), Some(true));
    std::fs::remove_file(r.p("sub/f.bin")).unwrap();
    paths.lock().unwrap().insert(f, "/sub/f.bin".into());
    assert_eq!(fs.presence(f), Some(false));
}
