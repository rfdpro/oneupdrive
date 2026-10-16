//! The provider layer on a real, temporary sync root. User programs are
//! simulated with `cmd.exe` (a different executable: Windows ignores the
//! provider process's own writes). Everything lives under the
//! temp folder and is unregistered and deleted at the end.
#![cfg(windows)]
#![allow(clippy::unwrap_used)]

use std::collections::HashMap;
use std::io::Read;
use std::os::windows::process::CommandExt;
use std::path::{Path, PathBuf};
use std::process::Command;
use std::sync::atomic::{AtomicU32, Ordering::Relaxed};
use std::sync::{Arc, Mutex};

use oneup_cfapi::identity::Identity;
use oneup_cfapi::provider::{self, Exclusive, Fetch, NewPlaceholder, Pin, Provider};

/// Serves content by identity, on its own thread, as `oneupd` will.
#[derive(Default)]
struct Cloud {
    content: Mutex<HashMap<Vec<u8>, Vec<u8>>>,
    fetches: AtomicU32,
    refused: AtomicU32,
    /// The file ID of the latest fetch.
    last_id: Mutex<i64>,
}

struct Serve(Arc<Cloud>);

impl Provider for Serve {
    fn fetch_data(&self, fetch: Fetch) {
        let me = Arc::clone(&self.0);
        std::thread::spawn(move || {
            me.fetches.fetch_add(1, Relaxed);
            *me.last_id.lock().unwrap() = fetch.file_id;
            let data = me.content.lock().unwrap().get(&fetch.identity).cloned();
            // As the real hydration does: the identity's size must be the
            // placeholder's, or the request is refused.
            match data {
                Some(d) if d.len() as i64 == fetch.file_size => {
                    for (i, chunk) in d.chunks(1 << 20).enumerate() {
                        fetch.transfer((i << 20) as i64, chunk).unwrap();
                    }
                    fetch.complete();
                }
                _ => {
                    me.refused.fetch_add(1, Relaxed);
                    fetch.fail(provider::NOT_IN_SYNC);
                }
            }
        });
    }
}

struct Root {
    base: PathBuf,
    root: PathBuf,
    _conn: Option<provider::Connection>,
}

impl Drop for Root {
    fn drop(&mut self) {
        self._conn.take();
        let _ = provider::unregister(&self.root);
        let _ = std::fs::remove_dir_all(&self.base);
    }
}

fn root(tag: &str, cloud: &Arc<Cloud>) -> Root {
    let base = std::env::temp_dir().join(format!("oneup-cfapi-{tag}-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&base);
    let root = base.join("root");
    std::fs::create_dir_all(&root).unwrap();
    provider::register(&root, b"test-pair").unwrap();
    let conn = provider::connect(&root, Arc::new(Serve(Arc::clone(cloud)))).unwrap();
    Root { base, root, _conn: Some(conn) }
}

fn identity(item: &str, content: &[u8]) -> Vec<u8> {
    let mut h = oneup_hash::QuickXorHash::new();
    h.update(content);
    Identity { item: item.into(), etag: "e1".into(), size: content.len() as i64, hash: h.finalize() }.encode()
}

/// What another program reads (`cmd /c copy` to a file outside the root).
fn app_read(base: &Path, path: &Path) -> Option<Vec<u8>> {
    let out = base.join("read.out");
    let _ = std::fs::remove_file(&out);
    let ok = Command::new("cmd")
        .raw_arg(format!("/C copy /Y /B \"{}\" \"{}\" >nul 2>nul", path.display(), out.display()))
        .status()
        .unwrap()
        .success();
    if !ok {
        return None;
    }
    std::fs::read(&out).ok()
}

fn app_append(path: &Path, text: &str) {
    let ok = Command::new("cmd").raw_arg(format!("/C echo {text}>>\"{}\"", path.display())).status().unwrap();
    assert!(ok.success());
}

fn info(p: &Path) -> provider::Info {
    provider::info(&provider::open(p).unwrap()).unwrap().expect("a placeholder")
}

#[test]
fn placeholders_hydrate_on_demand_and_dehydrate_only_safely() {
    let cloud = Arc::new(Cloud::default());
    let r = root("main", &cloud);
    let a = b"content of a, from the cloud".to_vec();
    let big: Vec<u8> = (0..3 * 1024 * 1024 + 17u32).map(|i| (i.wrapping_mul(2_654_435_761) >> 13) as u8).collect();
    let (ida, idb) = (identity("A", &a), identity("B", &big));
    cloud.content.lock().unwrap().extend([(ida.clone(), a.clone()), (idb.clone(), big.clone())]);
    let n = provider::create(
        &r.root,
        &[
            NewPlaceholder {
                name: "a.txt",
                identity: &ida,
                size: a.len() as i64,
                modified: 133_000_000_000_000_000,
                folder: false,
            },
            NewPlaceholder {
                name: "big.bin",
                identity: &idb,
                size: big.len() as i64,
                modified: 133_000_000_000_000_000,
                folder: false,
            },
            NewPlaceholder {
                name: "docs",
                identity: b"folder",
                size: 0,
                modified: 133_000_000_000_000_000,
                folder: true,
            },
        ],
    )
    .unwrap();
    assert_eq!(n, 3);
    let pa = r.root.join("a.txt");
    let i = info(&pa);
    assert!(i.in_sync && i.on_disk == 0, "{i:?}");
    assert_eq!(Identity::decode(&i.identity).unwrap().item, "A");
    assert_eq!(std::fs::metadata(&pa).unwrap().len(), a.len() as u64, "the size shows before any download");

    // Opening from another program downloads, verified by content.
    assert_eq!(app_read(&r.base, &pa).as_deref(), Some(&a[..]));
    assert_eq!(app_read(&r.base, &r.root.join("big.bin")).as_deref(), Some(&big[..]));
    assert_eq!(info(&pa).on_disk, a.len() as i64);
    let fetched = cloud.fetches.load(Relaxed);
    assert_eq!(app_read(&r.base, &pa).as_deref(), Some(&a[..]));
    assert_eq!(cloud.fetches.load(Relaxed), fetched, "served locally once hydrated");

    // Safe dehydration of an in-sync file: verified through the exclusive
    // handle, then freed through it.
    {
        let x = Exclusive::open(&pa).unwrap();
        let mut seen = Vec::new();
        x.file().read_to_end(&mut seen).unwrap();
        assert_eq!(seen, a);
        assert!(Exclusive::open(&pa).is_err(), "no one else can open it meanwhile");
        x.dehydrate(None).unwrap();
    }
    assert_eq!(info(&pa).on_disk, 0);
    assert_eq!(app_read(&r.base, &pa).as_deref(), Some(&a[..]), "downloads again");

    // An edit by a user's program clears in-sync, and dehydration is then
    // refused: the edit survives.
    app_append(&pa, "edited");
    assert!(!info(&pa).in_sync);
    let x = Exclusive::open(&pa).unwrap();
    assert!(x.dehydrate(None).is_err(), "VERIFY_IN_SYNC refuses an unsynced edit");
    drop(x);
    let after = std::fs::read(&pa).unwrap();
    assert!(String::from_utf8_lossy(&after).contains("edited"), "the edit is intact");

    // After its upload, the edit is marked in sync with its new identity.
    let ida2 = identity("A", &after);
    Exclusive::open(&pa).unwrap().mark_in_sync(&ida2).unwrap();
    let i = info(&pa);
    assert!(i.in_sync && i.identity == ida2);

    // Pin state.
    let f = provider::open(&pa).unwrap();
    provider::set_pin(&f, Pin::Pinned).unwrap();
    assert_eq!(info(&pa).pin, Pin::Pinned);
}

/// A folder registered with the shell by a program with OneUpDrive's
/// package identity (`oneup dev-shell-root FOLDER`, set in
/// `ONEUP_SHELL_ROOT`), served by this test binary, which has none: as the
/// agent does once it runs without the identity. Run by hand.
#[test]
#[ignore]
fn a_shell_registered_root_is_served_by_a_program_without_identity() {
    let root = PathBuf::from(std::env::var("ONEUP_SHELL_ROOT").expect("ONEUP_SHELL_ROOT"));
    // Nothing connected yet (the agent stopped): still recognised as ours.
    let info = oneup_cfapi::sync_root_containing(&root).unwrap();
    assert_eq!(info.provider_name, oneup_cfapi::PROVIDER_NAME, "{info:?}");
    assert_eq!(oneup_cfapi::other_sync_root(&root), None);
    // The shell's own record tells it too, for this pair only (what's left
    // when Windows empties the Cloud Files registration).
    assert!(oneup_cfapi::shell::is_ours(&root) && oneup_cfapi::shell::is_ours(&root.join("inside")));
    assert!(oneup_cfapi::shell::is_pairs_root(&root, "devtoken00000000"));
    assert!(!oneup_cfapi::shell::is_pairs_root(&root, "anothertoken0000"));
    assert!(!oneup_cfapi::shell::is_ours(&root.with_file_name("elsewhere")));
    let cloud = Arc::new(Cloud::default());
    let _conn = provider::connect(&root, Arc::new(Serve(Arc::clone(&cloud)))).unwrap();
    let content = b"served without package identity".to_vec();
    let id = identity("S", &content);
    cloud.content.lock().unwrap().insert(id.clone(), content.clone());
    provider::create(
        &root,
        &[NewPlaceholder {
            name: "s.txt",
            identity: &id,
            size: content.len() as i64,
            modified: 133_000_000_000_000_000,
            folder: false,
        }],
    )
    .unwrap();
    let base = root.parent().unwrap().to_path_buf();
    assert_eq!(app_read(&base, &root.join("s.txt")).as_deref(), Some(&content[..]));
    assert_eq!(cloud.fetches.load(Relaxed), 1);
}

/// What the agent's app tracker does with a file the change journal named
/// (`oneup_app::apps`), on an online-only placeholder, from the provider's
/// own process as in `oneupd`: which steps download it. Measured because
/// the agent once downloaded 2,260 online-only files by itself, right
/// after creating or freeing them.
#[test]
fn what_downloads_an_online_only_file_from_the_providers_process() {
    let cloud = Arc::new(Cloud::default());
    let r = root("tracker", &cloud);
    let content = b"an online-only file".to_vec();
    let id = identity("T", &content);
    cloud.content.lock().unwrap().insert(id.clone(), content.clone());
    let make = |name: &str| {
        provider::create(
            &r.root,
            &[NewPlaceholder {
                name,
                identity: &id,
                size: content.len() as i64,
                modified: 133_000_000_000_000_000,
                folder: false,
            }],
        )
        .unwrap();
        r.root.join(name)
    };
    oneup_cfapi::expose_placeholders().unwrap();
    let settle = || std::thread::sleep(std::time::Duration::from_millis(500));

    // Attributes only: nothing downloaded, and they say it's online-only.
    let p = make("attrs.txt");
    let m = std::fs::symlink_metadata(&p).unwrap();
    settle();
    assert_eq!(cloud.fetches.load(Relaxed), 0, "symlink_metadata");
    assert_ne!(std::os::windows::fs::MetadataExt::file_attributes(&m) & 0x0040_0000, 0, "recall on data access");
    assert_eq!(info(&p).on_disk, 0);

    // Restart Manager, as the tracker asked it who had the file open: it
    // downloads the whole file. This is why the
    // tracker checks the attributes above first and leaves such files alone.
    let p = make("rm.txt");
    let _ = oneup_sys::in_use::users(&p);
    settle();
    assert_eq!(cloud.fetches.load(Relaxed), 1, "Restart Manager downloads an online-only file");
    assert_eq!(info(&p).on_disk, content.len() as i64);
}

/// Windows names the program that closed a placeholder, even one that
/// wrote and closed it in milliseconds, where Restart Manager is too late.
/// The program tracker relies on this (`oneup_cfapi::closes`).
#[test]
fn closing_a_placeholder_names_the_program() {
    let cloud = Arc::new(Cloud::default());
    let r = root("closes", &cloud);
    let content = b"written by another program".to_vec();
    let id = identity("W", &content);
    cloud.content.lock().unwrap().insert(id.clone(), content.clone());
    provider::create(
        &r.root,
        &[NewPlaceholder { name: "w.txt", identity: &id, size: content.len() as i64, modified: 0, folder: false }],
    )
    .unwrap();
    let p = r.root.join("w.txt");
    let start = std::time::Instant::now();
    app_append(&p, "more");
    let file_id = u128::from(*cloud.last_id.lock().unwrap() as u64);
    assert_ne!(file_id, 0, "the append downloaded the file first");
    let deadline = start + std::time::Duration::from_secs(5);
    let closes = loop {
        let c = oneup_cfapi::closes::since(file_id, start);
        if c.iter().any(|c| c.exe.eq_ignore_ascii_case("cmd.exe")) || std::time::Instant::now() > deadline {
            break c;
        }
        std::thread::sleep(std::time::Duration::from_millis(20));
    };
    eprintln!("closes after {:?}: {closes:?}", start.elapsed());
    assert!(closes.iter().any(|c| c.exe.eq_ignore_ascii_case("cmd.exe")), "{closes:?}");
    assert!(oneup_sys::in_use::users(&p).unwrap_or_default().is_empty(), "nobody has it open any more");
}

#[test]
fn cloud_edits_update_placeholders_and_wrong_versions_are_refused() {
    let cloud = Arc::new(Cloud::default());
    let r = root("update", &cloud);
    let v1 = b"version one".to_vec();
    let v2 = b"version two, longer than one".to_vec();
    let (id1, id2) = (identity("C", &v1), identity("C", &v2));
    cloud.content.lock().unwrap().insert(id2.clone(), v2.clone());
    provider::create(
        &r.root,
        &[NewPlaceholder { name: "c.txt", identity: &id1, size: v1.len() as i64, modified: 0, folder: false }],
    )
    .unwrap();
    let pc = r.root.join("c.txt");

    // The cloud only has v2 now: the v1 placeholder is refused, never
    // filled with v2's bytes or zeros, and the reader isn't kept waiting.
    let started = std::time::Instant::now();
    assert_eq!(app_read(&r.base, &pc), None);
    eprintln!("refused read took {:?}, {} refusals", started.elapsed(), cloud.refused.load(Relaxed));
    assert!(started.elapsed() < std::time::Duration::from_secs(10), "{:?}", started.elapsed());
    assert!(cloud.refused.load(Relaxed) >= 1);
    assert_eq!(info(&pc).on_disk, 0, "nothing was written into it");

    // Updated to v2 (still dehydrated), it downloads v2.
    provider::update_dehydrated(&provider::open(&pc).unwrap(), &id2, v2.len() as i64, 0).unwrap();
    assert_eq!(std::fs::metadata(&pc).unwrap().len(), v2.len() as u64);
    assert_eq!(app_read(&r.base, &pc).as_deref(), Some(&v2[..]));

    // A hydrated file can't simply take a new size: that goes through the
    // exclusive handle, which frees the content with the update.
    let f = provider::open(&pc).unwrap();
    assert!(matches!(provider::update_dehydrated(&f, &id1, v1.len() as i64, 0), Err(provider::Error::Hydrated)));
    drop(f);
    cloud.content.lock().unwrap().insert(id1.clone(), v1.clone());
    Exclusive::open(&pc).unwrap().dehydrate(Some((&id1, v1.len() as i64, 0))).unwrap();
    assert_eq!(app_read(&r.base, &pc).as_deref(), Some(&v1[..]));
}

#[test]
fn ordinary_files_convert_and_revert() {
    let cloud = Arc::new(Cloud::default());
    let r = root("convert", &cloud);
    let p = r.root.join("plain.txt");
    std::fs::write(&p, b"an ordinary file").unwrap();
    assert!(provider::info(&provider::open(&p).unwrap()).unwrap().is_none(), "not a placeholder yet");
    let id = identity("P", b"an ordinary file");
    Exclusive::open(&p).unwrap().convert(&id).unwrap();
    let i = info(&p);
    assert!(i.in_sync && i.on_disk == 16 && i.identity == id, "{i:?}");
    provider::revert(&provider::open(&p).unwrap(), 16).unwrap();
    assert!(provider::info(&provider::open(&p).unwrap()).unwrap().is_none());
    assert_eq!(std::fs::read(&p).unwrap(), b"an ordinary file");
    let (name, identity) = provider::registration(&r.root).unwrap();
    assert_eq!((name.as_str(), identity.as_slice()), (provider::PROVIDER_NAME, &b"test-pair"[..]));
}

#[test]
fn without_the_provider_reads_fail_cleanly() {
    let cloud = Arc::new(Cloud::default());
    let mut r = root("offline", &cloud);
    let id = identity("D", b"never served");
    provider::create(&r.root, &[NewPlaceholder { name: "d.txt", identity: &id, size: 12, modified: 0, folder: false }])
        .unwrap();
    r._conn.take(); // the agent isn't running
    let started = std::time::Instant::now();
    assert_eq!(app_read(&r.base, &r.root.join("d.txt")), None);
    assert!(started.elapsed() < std::time::Duration::from_secs(30), "no hang");
    assert_eq!(info(&r.root.join("d.txt")).on_disk, 0, "no zero-filled content");
}

#[test]
fn our_own_sync_root_is_not_another_providers() {
    let cloud = Arc::new(Cloud::default());
    let mut r = root("ours", &cloud);
    let info = oneup_cfapi::sync_root_containing(&r.root).unwrap();
    assert_eq!(info.provider_name, oneup_cfapi::PROVIDER_NAME);
    assert_eq!(oneup_cfapi::other_sync_root(&r.root), None);
    // With the agent stopped (no provider connected), as `oneup on-demand on`
    // runs: still ours. It read as another provider's, named garbage.
    r._conn.take();
    assert_eq!(oneup_cfapi::sync_root_containing(&r.root).unwrap().provider_name, oneup_cfapi::PROVIDER_NAME);
    assert_eq!(oneup_cfapi::other_sync_root(&r.root), None);
}

/// Opening a dehydrated placeholder with data access downloads it (so
/// OneUpDrive never does), and a hold refuses every fetch meanwhile.
#[test]
fn data_access_hydrates_and_holds_refuse_fetches() {
    let cloud = Arc::new(Cloud::default());
    let r = root("hold", &cloud);
    let a = b"held content".to_vec();
    let id = identity("H", &a);
    cloud.content.lock().unwrap().insert(id.clone(), a.clone());
    let new = |name| NewPlaceholder { name, identity: &id, size: a.len() as i64, modified: 0, folder: false };
    provider::create(&r.root, &[new("x.txt"), new("y.txt")]).unwrap();
    let (px, py) = (r.root.join("x.txt"), r.root.join("y.txt"));

    drop(provider::open(&px).unwrap());
    assert_eq!(info(&px).on_disk, 0, "attribute access: nothing downloaded");
    drop(std::fs::File::open(&px).unwrap());
    assert_eq!(info(&px).on_disk, a.len() as i64, "data access alone downloads it");

    let hold = provider::hold(&id);
    assert_eq!(app_read(&r.base, &py), None, "refused while held");
    assert_eq!(info(&py).on_disk, 0);
    drop(hold);
    assert_eq!(app_read(&r.base, &py).as_deref(), Some(&a[..]));
}

/// What Windows does with pin states, which the agent
/// relies on: unpinning frees nothing by itself; pinning a folder doesn't
/// change its children's explicit states, but a new placeholder inside it
/// inherits the pin, dehydrated (the provider must download it); the state
/// shows in the attributes (`FILE_ATTRIBUTE_PINNED` / `UNPINNED`).
#[test]
fn pin_states_behave_as_the_agent_expects() {
    use std::os::windows::fs::MetadataExt;
    const PINNED: u32 = 0x0008_0000;
    const UNPINNED: u32 = 0x0010_0000;
    let attrs = |p: &Path| std::fs::metadata(p).unwrap().file_attributes();
    let cloud = Arc::new(Cloud::default());
    let r = root("pin", &cloud);
    let a = b"pin me".to_vec();
    let id = identity("PN", &a);
    cloud.content.lock().unwrap().insert(id.clone(), a.clone());
    let d = r.root.join("d");
    provider::create(&r.root, &[NewPlaceholder { name: "d", identity: b"folder", size: 0, modified: 0, folder: true }])
        .unwrap();
    let new = |name| NewPlaceholder { name, identity: &id, size: 6, modified: 0, folder: false };
    provider::create(&d, &[new("p.txt")]).unwrap();
    let p = d.join("p.txt");
    assert_eq!(app_read(&r.base, &p).as_deref(), Some(&a[..]));

    provider::set_pin(&provider::open(&p).unwrap(), Pin::Unpinned).unwrap();
    std::thread::sleep(std::time::Duration::from_millis(300));
    assert_eq!(info(&p).on_disk, 6, "unpinning frees nothing by itself");
    assert_ne!(attrs(&p) & UNPINNED, 0);

    provider::set_pin(&provider::open(&d).unwrap(), Pin::Pinned).unwrap();
    assert_ne!(attrs(&d) & PINNED, 0);
    assert_eq!(info(&p).pin, Pin::Unpinned, "an explicit child state stays");
    provider::create(&d, &[new("q.txt")]).unwrap();
    let q = d.join("q.txt");
    assert_eq!(info(&q).pin, Pin::Pinned, "inherited");
    assert!(attrs(&q) & PINNED != 0 && info(&q).on_disk == 0, "pinned but not downloaded");
}

/// Bytes a program wrote stay "modified" after being marked in sync, and
/// dehydration frees only validated data; reverting and converting again
/// (through one exclusive handle) validates everything.
/// `mark_uploaded` does that after an edit was sent.
#[test]
fn edited_content_frees_fully_only_after_reconversion() {
    let cloud = Arc::new(Cloud::default());
    let r = root("reconvert", &cloud);
    let a = b"0123456789".to_vec();
    let id = identity("RC", &a);
    cloud.content.lock().unwrap().insert(id.clone(), a.clone());
    provider::create(&r.root, &[NewPlaceholder { name: "m.txt", identity: &id, size: 10, modified: 0, folder: false }])
        .unwrap();
    let p = r.root.join("m.txt");
    app_read(&r.base, &p).unwrap();
    app_append(&p, "appended");
    let content = std::fs::read(&p).unwrap();
    let id2 = identity("RC", &content);
    cloud.content.lock().unwrap().insert(id2.clone(), content.clone());
    assert!(info(&p).modified > 0, "a program's bytes count as modified");
    let x = Exclusive::open(&p).unwrap();
    x.revalidate(&id2, content.len() as i64).unwrap();
    assert_eq!(provider::info(x.file()).unwrap().unwrap().modified, 0);
    x.dehydrate(None).unwrap();
    drop(x);
    assert_eq!(info(&p).on_disk, 0, "all of it freed");
    assert_eq!(app_read(&r.base, &p), Some(content));
}

/// Migration (files-on-demand.md § Moving over from the OneDrive app):
/// what unregistering a provider (standing in for the OneDrive app being
/// unlinked) leaves behind. Windows itself removes the online-only
/// placeholders and the folders never listed (their content is only in the
/// cloud), and turns every file with content here into an ordinary file
/// with its bytes, including an edit not yet sent and a pinned file. A
/// placeholder held open meanwhile goes when it's closed. So nothing of the
/// app's is left for OneUpDrive to replace: `oneup migrate check` verifies
/// beforehand that each online-only file is in OneDrive. If this test
/// starts failing, Windows changed and the migration design must be
/// revisited.
#[test]
fn unregistering_another_provider_removes_only_what_is_in_the_cloud() {
    let base = std::env::temp_dir().join(format!("oneup-cfapi-unlink-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&base);
    let root = base.join("root");
    std::fs::create_dir_all(&root).unwrap();
    let theirs = Arc::new(Cloud::default());
    for (k, v) in [(&b"OD!b"[..], &b"bravo bravo"[..]), (b"OD!e", b"echo"), (b"OD!p", b"pinned!"), (b"OD!n", b"nested")]
    {
        theirs.content.lock().unwrap().insert(k.to_vec(), v.to_vec());
    }
    provider::register_as(&root, "FakeOneDrive", 0x0dd0_0dd0_0dd0_0dd0_0dd0_0dd0_0dd0_0dd0, b"their-root", false)
        .unwrap();
    let conn = provider::connect(&root, Arc::new(Serve(Arc::clone(&theirs)))).unwrap();
    let ph = |name: &'static str, id: &'static [u8], size: i64, folder: bool| NewPlaceholder {
        name,
        identity: id,
        size,
        modified: 133_000_000_000_000_000,
        folder,
    };
    provider::create(
        &root,
        &[
            ph("a.txt", b"OD!a", 5, false),
            ph("b.txt", b"OD!b", 11, false),
            ph("e.txt", b"OD!e", 4, false),
            ph("p.txt", b"OD!p", 7, false),
            ph("sub", b"OD!sub", 0, true),
        ],
    )
    .unwrap();
    let sub = root.join("sub");
    provider::create(&sub, &[ph("n.txt", b"OD!n", 6, false), ph("m.txt", b"OD!m", 3, false)]).unwrap();
    provider::create_unpopulated_folder(&root, "unpop", b"OD!u").unwrap();
    app_read(&base, &root.join("b.txt")).unwrap();
    app_read(&base, &root.join("e.txt")).unwrap();
    app_append(&root.join("e.txt"), "edited");
    app_read(&base, &sub.join("n.txt")).unwrap();
    provider::set_pin(&provider::open(&root.join("p.txt")).unwrap(), Pin::Pinned).unwrap();
    app_read(&base, &root.join("p.txt")).unwrap();
    // Some program holds the online-only m.txt open across the unlinking.
    let held = provider::open(&sub.join("m.txt")).unwrap();
    drop(conn);
    provider::unregister(&root).unwrap();

    let ordinary = |p: &Path| {
        let f = provider::open(p).unwrap();
        provider::info(&f).unwrap().is_none()
    };
    for p in [root.join("b.txt"), root.join("e.txt"), root.join("p.txt"), sub.join("n.txt"), sub.clone()] {
        assert!(ordinary(&p), "{} is an ordinary item now", p.display());
    }
    assert_eq!(std::fs::read(root.join("b.txt")).unwrap(), b"bravo bravo");
    assert_eq!(std::fs::read(root.join("e.txt")).unwrap(), b"echoedited\r\n", "the unsent edit survives");
    assert_eq!(std::fs::read(root.join("p.txt")).unwrap(), b"pinned!");
    assert_eq!(std::fs::read(sub.join("n.txt")).unwrap(), b"nested");
    assert!(!root.join("a.txt").exists(), "online-only: removed by Windows");
    assert!(!root.join("unpop").exists(), "never listed: removed by Windows");
    drop(held);
    assert!(!sub.join("m.txt").exists(), "removed once closed");
    let _ = std::fs::remove_dir_all(&base);
}

/// How fast placeholders are made one at a time, as the executor makes
/// them (`files-on-demand.md` § Lazy folders): `cargo test --release -p
/// oneup-cfapi --test provider placeholder_rate -- --ignored --nocapture`.
#[test]
#[ignore = "a measurement, not a check"]
fn placeholder_rate() {
    let cloud = Arc::new(Cloud::default());
    let r = root("rate", &cloud);
    let n = 20_000usize;
    let id = identity("x", b"0123456789");
    let started = std::time::Instant::now();
    for d in 0..(n / 1000) {
        let dir = r.root.join(format!("d{d}"));
        std::fs::create_dir(&dir).unwrap();
        for i in 0..1000 {
            let name = format!("file {i}.txt");
            let p = provider::NewPlaceholder {
                name: &name,
                identity: &id,
                size: 10,
                modified: 133_000_000_000_000_000,
                folder: false,
            };
            provider::create(&dir, &[p]).unwrap();
        }
    }
    let t = started.elapsed();
    println!("{n} placeholders one at a time: {t:.1?} ({:.0}/s)", n as f64 / t.as_secs_f64());
}

/// Where [`stand_in_provider_exits_without_unregistering`] builds its root.
const STAND_IN_ROOT: &str = "ONEUP_CFAPI_STAND_IN_ROOT";

/// The child half of
/// [`a_stalled_unlink_can_be_finished_by_another_program`]: in its own
/// process, a stand-in for the OneDrive app registers a sync root, fills
/// it as the app does, and then ends without unregistering (the app killed
/// mid sign-out). Does nothing unless the parent set [`STAND_IN_ROOT`].
#[test]
#[ignore = "run by a_stalled_unlink_can_be_finished_by_another_program in its own process"]
fn stand_in_provider_exits_without_unregistering() {
    let Some(root) = std::env::var_os(STAND_IN_ROOT).map(PathBuf::from) else { return };
    let base = root.parent().unwrap().to_path_buf();
    let theirs = Arc::new(Cloud::default());
    for (k, v) in [(&b"OD!b"[..], &b"bravo bravo"[..]), (b"OD!e", b"echo"), (b"OD!p", b"pinned!"), (b"OD!n", b"nested")]
    {
        theirs.content.lock().unwrap().insert(k.to_vec(), v.to_vec());
    }
    provider::register_as(&root, "FakeOneDrive", 0x0dd0_0dd0_0dd0_0dd0_0dd0_0dd0_0dd0_0dd1, b"their-root", false)
        .unwrap();
    let conn = provider::connect(&root, Arc::new(Serve(Arc::clone(&theirs)))).unwrap();
    let ph = |name: &'static str, id: &'static [u8], size: i64, folder: bool| NewPlaceholder {
        name,
        identity: id,
        size,
        modified: 133_000_000_000_000_000,
        folder,
    };
    provider::create(
        &root,
        &[
            ph("a.txt", b"OD!a", 5, false),
            ph("b.txt", b"OD!b", 11, false),
            ph("e.txt", b"OD!e", 4, false),
            ph("p.txt", b"OD!p", 7, false),
            ph("q.txt", b"OD!q", 8, false),
            ph("sub", b"OD!sub", 0, true),
        ],
    )
    .unwrap();
    let sub = root.join("sub");
    provider::create(&sub, &[ph("n.txt", b"OD!n", 6, false), ph("m.txt", b"OD!m", 3, false)]).unwrap();
    provider::create_unpopulated_folder(&root, "unpop", b"OD!u").unwrap();
    app_read(&base, &root.join("b.txt")).unwrap();
    app_read(&base, &root.join("e.txt")).unwrap();
    app_append(&root.join("e.txt"), "edited");
    app_read(&base, &sub.join("n.txt")).unwrap();
    provider::set_pin(&provider::open(&root.join("p.txt")).unwrap(), Pin::Pinned).unwrap();
    app_read(&base, &root.join("p.txt")).unwrap();
    // Pinned but never downloaded (the app hadn't got to it yet).
    provider::set_pin(&provider::open(&root.join("q.txt")).unwrap(), Pin::Pinned).unwrap();
    // Ends without disconnecting or unregistering: process exit drops it.
    std::mem::forget(conn);
}

/// A stalled unlink: the
/// OneDrive app removed its account but never unregistered its sync root,
/// then stopped. Measured here: another program (this test process, not the
/// one that registered it) can unregister it, and Windows then does what an
/// ordinary unlink does: online-only placeholders and never-listed folders
/// go, everything with content here becomes an ordinary file with its
/// bytes (an unsent edit too), and the pin attribute stays on pinned files.
/// OneUpDrive's "finish unlinking" (`oneup migrate finish-unlink`) relies on
/// all of it; if this fails, Windows changed.
#[test]
fn a_stalled_unlink_can_be_finished_by_another_program() {
    const PINNED: u32 = 0x0008_0000;
    let base = std::env::temp_dir().join(format!("oneup-cfapi-stalled-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&base);
    let root = base.join("root");
    std::fs::create_dir_all(&root).unwrap();
    let status = Command::new(std::env::current_exe().unwrap())
        .args(["stand_in_provider_exits_without_unregistering", "--exact", "--ignored", "--quiet"])
        .env(STAND_IN_ROOT, &root)
        .status()
        .unwrap();
    assert!(status.success(), "the stand-in provider ran");
    let (name, _) = provider::registration(&root).expect("its sync root outlives it");
    assert_eq!(name, "FakeOneDrive");
    assert_eq!(oneup_cfapi::other_sync_root(&root).map(|i| i.provider_name).as_deref(), Some("FakeOneDrive"));

    provider::unregister(&root).unwrap();

    assert!(provider::registration(&root).is_none(), "released");
    assert!(oneup_cfapi::other_sync_root(&root).is_none());
    let ordinary = |p: &Path| provider::info(&provider::open(p).unwrap()).unwrap().is_none();
    let sub = root.join("sub");
    for p in [root.join("b.txt"), root.join("e.txt"), root.join("p.txt"), sub.join("n.txt"), sub.clone()] {
        assert!(ordinary(&p), "{} is an ordinary item now", p.display());
    }
    assert_eq!(std::fs::read(root.join("b.txt")).unwrap(), b"bravo bravo");
    assert_eq!(std::fs::read(root.join("e.txt")).unwrap(), b"echoedited\r\n", "the unsent edit survives");
    assert_eq!(std::fs::read(root.join("p.txt")).unwrap(), b"pinned!");
    assert_eq!(std::fs::read(sub.join("n.txt")).unwrap(), b"nested");
    let attrs = |p: &Path| {
        use std::os::windows::fs::MetadataExt;
        std::fs::metadata(p).unwrap().file_attributes()
    };
    assert_ne!(attrs(&root.join("p.txt")) & PINNED, 0, "the pin attribute stays");
    assert!(!root.join("a.txt").exists(), "online-only: removed by Windows");
    assert!(!root.join("q.txt").exists(), "pinned but never downloaded: removed too");
    assert!(!sub.join("m.txt").exists());
    assert!(!root.join("unpop").exists(), "never listed: removed by Windows");
    let _ = std::fs::remove_dir_all(&base);
}

/// Found on a real stalled unlink: when another program has
/// a folder in the sync root open (its current folder, as Explorer or a
/// terminal has Documents), unregistering **succeeds** but leaves that
/// folder's whole subtree as it was: online-only placeholders stay, files
/// with content stay placeholders, orphaned with no root, also after the
/// program ends. Registering a root on the folder again and unregistering
/// it, with nothing holding it, finishes the job: what OneUpDrive's
/// "finish unlinking" does when a rescan finds placeholders left behind.
#[test]
fn a_folder_held_open_is_left_orphaned_until_released_again() {
    let base = std::env::temp_dir().join(format!("oneup-cfapi-held-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&base);
    let root = base.join("root");
    std::fs::create_dir_all(&root).unwrap();
    let theirs = Arc::new(Cloud::default());
    theirs.content.lock().unwrap().insert(b"OD!h".to_vec(), b"hydrated".to_vec());
    provider::register_as(&root, "FakeOneDrive", 0x0dd0_0dd0_0dd0_0dd0_0dd0_0dd0_0dd0_0dd3, b"their-root", false)
        .unwrap();
    let conn = provider::connect(&root, Arc::new(Serve(Arc::clone(&theirs)))).unwrap();
    let ph = |name: &'static str, id: &'static [u8], size: i64, folder: bool| NewPlaceholder {
        name,
        identity: id,
        size,
        modified: 133_000_000_000_000_000,
        folder,
    };
    provider::create(&root, &[ph("docs", b"OD!docs", 0, true), ph("top.txt", b"OD!t", 3, false)]).unwrap();
    let docs = root.join("docs");
    provider::create(&docs, &[ph("online.txt", b"OD!o", 4, false), ph("h.txt", b"OD!h", 8, false)]).unwrap();
    app_read(&base, &docs.join("h.txt")).unwrap();
    let placeholder = |p: &Path| provider::open(p).ok().and_then(|f| provider::info(&f).ok().flatten()).is_some();

    // Another program sits in docs while the provider's root is released.
    let mut holder = Command::new("cmd")
        .args(["/C", "pause"])
        .current_dir(&docs)
        .stdin(std::process::Stdio::piped())
        .stdout(std::process::Stdio::null())
        .spawn()
        .unwrap();
    std::thread::sleep(std::time::Duration::from_millis(500));
    drop(conn);
    provider::unregister(&root).unwrap();
    let _ = holder.kill();
    let _ = holder.wait();

    assert!(provider::registration(&root).is_none(), "released, by Windows' account");
    assert!(!root.join("top.txt").exists(), "outside the held folder: done");
    assert!(placeholder(&docs) && placeholder(&docs.join("h.txt")), "inside it: still placeholders");
    assert!(docs.join("online.txt").exists(), "and still online-only");
    // Windows doesn't describe it: that's an error, never "an ordinary file"
    // (which would mean all its content is here).
    assert!(provider::info(&provider::open(&docs.join("online.txt")).unwrap()).is_err());

    // Released again, with nothing holding it.
    provider::register_as(&root, "Cleanup", 0x0dd0_0dd0_0dd0_0dd0_0dd0_0dd0_0dd0_0dd4, b"cleanup", false).unwrap();
    provider::unregister(&root).unwrap();
    assert!(provider::registration(&root).is_none());
    assert!(!placeholder(&docs) && !placeholder(&docs.join("h.txt")), "ordinary now");
    assert_eq!(std::fs::read(docs.join("h.txt")).unwrap(), b"hydrated");
    assert!(!docs.join("online.txt").exists(), "online-only: removed");
    let _ = std::fs::remove_dir_all(&base);
}

/// Spike (run by hand): what File Explorer shows for items in
/// a sync root that aren't placeholders (files the rules leave out), and
/// for the folders holding them; and whether anything short of making them
/// placeholders makes it show them as fine. Builds the cases side by side
/// in a temporary root, writes `ready` next to it, and waits for the
/// caller to read the shell's properties.
#[test]
#[ignore = "a spike: run by hand, then read the shell's properties during the pause"]
fn spike_status_of_items_that_are_not_placeholders() {
    let cloud = Arc::new(Cloud::default());
    // Explorer shows sync states only for a root the shell registered
    // (`oneup dev-shell-root DIR`, which needs the package identity): given
    // in ONEUP_SPIKE_ROOT, left registered for its caller to remove.
    let r = match std::env::var_os("ONEUP_SPIKE_ROOT") {
        Some(dir) => {
            let root = PathBuf::from(dir);
            let conn = provider::connect(&root, Arc::new(Serve(Arc::clone(&cloud)))).unwrap();
            Root { base: root.parent().unwrap().to_path_buf(), root, _conn: Some(conn) }
        }
        None => root("spike-status", &cloud),
    };
    let folder = |dir: &Path, name: &str| {
        let id = identity(name, b"");
        provider::create(dir, &[NewPlaceholder { name, identity: &id, size: 0, modified: 0, folder: true }]).unwrap();
        dir.join(name)
    };
    let attrib = |flags: &str, p: &Path| {
        let ok = Command::new("cmd").raw_arg(format!("/C attrib {flags} \"{}\"", p.display())).status().unwrap();
        assert!(ok.success(), "attrib {flags}");
    };
    // Files at the top.
    std::fs::write(r.root.join("plain.txt"), b"plain").unwrap();
    std::fs::write(r.root.join("pinned.txt"), b"pinned").unwrap();
    attrib("+P", &r.root.join("pinned.txt"));
    std::fs::write(r.root.join("unpinned.txt"), b"unpinned").unwrap();
    attrib("+U", &r.root.join("unpinned.txt"));
    let ph = r.root.join("placeholder.txt");
    std::fs::write(&ph, b"converted").unwrap();
    Exclusive::open(&ph).unwrap().convert(&identity("ph", b"converted")).unwrap();
    // Folders (placeholders, in sync) each holding one kind of child.
    let only_placeholders = folder(&r.root, "only-placeholders");
    let c = only_placeholders.join("c.txt");
    std::fs::write(&c, b"c").unwrap();
    Exclusive::open(&c).unwrap().convert(&identity("c", b"c")).unwrap();
    let with_plain = folder(&r.root, "with-plain");
    std::fs::write(with_plain.join("x.txt"), b"x").unwrap();
    let with_pinned = folder(&r.root, "with-pinned");
    std::fs::write(with_pinned.join("x.txt"), b"x").unwrap();
    attrib("+P", &with_pinned.join("x.txt"));
    let with_hidden = folder(&r.root, "with-hidden-system");
    std::fs::write(with_hidden.join("desktop.ini"), b"[.ShellClassInfo]").unwrap();
    attrib("+H +S", &with_hidden.join("desktop.ini"));
    let with_excluded = folder(&r.root, "with-excluded-pin");
    let e = with_excluded.join("x.txt");
    std::fs::write(&e, b"x").unwrap();
    Exclusive::open(&e).unwrap().convert(&identity("e", b"x")).unwrap();
    provider::set_pin(&provider::open(&e).unwrap(), Pin::Excluded).unwrap();
    // An ordinary folder (not a placeholder) with an ordinary file.
    std::fs::create_dir_all(r.root.join("plain-folder")).unwrap();
    std::fs::write(r.root.join("plain-folder/x.txt"), b"x").unwrap();
    let pinned_folder = r.root.join("pinned-plain-folder");
    std::fs::create_dir_all(&pinned_folder).unwrap();
    std::fs::write(pinned_folder.join("x.txt"), b"x").unwrap();
    attrib("+P", &pinned_folder);
    attrib("+P", &pinned_folder.join("x.txt"));

    // How far down Explorer looks: an ordinary file two placeholder
    // folders below.
    let deep = folder(&r.root, "deep");
    let deep1 = folder(&deep, "level1");
    std::fs::write(deep1.join("x.txt"), b"x").unwrap();
    // A project folder holding an excluded folder
    // of ordinary files (node_modules), and only that folder made a
    // placeholder, in sync, pinned "excluded"; its contents stay ordinary.
    let project = folder(&r.root, "project");
    let src = project.join("src.txt");
    std::fs::write(&src, b"code").unwrap();
    Exclusive::open(&src).unwrap().convert(&identity("src", b"code")).unwrap();
    let nm = project.join("node_modules");
    std::fs::create_dir_all(nm.join("pkg/lib")).unwrap();
    std::fs::write(nm.join("pkg/index.js"), b"index").unwrap();
    std::fs::write(nm.join("pkg/lib/deep.js"), b"deep").unwrap();
    Exclusive::open(&nm).unwrap().convert(&identity("nm", b"")).unwrap();
    provider::set_pin(&provider::open(&nm).unwrap(), Pin::Excluded).unwrap();
    // The same, but the excluded folder only converted (no pin).
    let project2 = folder(&r.root, "project-no-pin");
    let nm2 = project2.join("node_modules");
    std::fs::create_dir_all(&nm2).unwrap();
    std::fs::write(nm2.join("index.js"), b"index").unwrap();
    Exclusive::open(&nm2).unwrap().convert(&identity("nm2", b"")).unwrap();
    // "Free up space" on the project, recursively: does it reach the
    // ordinary files under the excluded folder?
    let project3 = folder(&r.root, "project-freed");
    let nm3 = project3.join("node_modules");
    std::fs::create_dir_all(&nm3).unwrap();
    std::fs::write(nm3.join("index.js"), b"index").unwrap();
    Exclusive::open(&nm3).unwrap().convert(&identity("nm3", b"")).unwrap();
    provider::set_pin(&provider::open(&nm3).unwrap(), Pin::Excluded).unwrap();
    let freed = provider::set_pin_tree(&provider::open(&project3).unwrap(), Pin::Unpinned);
    eprintln!("free up space on project-freed: {freed:?}");
    eprintln!("node_modules pin after: {:?}", provider::info(&provider::open(&nm3).unwrap()).unwrap().map(|i| i.pin));

    std::fs::write(r.base.join("ready"), r.root.display().to_string()).unwrap();
    eprintln!("ready: {}", r.root.display());
    std::thread::sleep(std::time::Duration::from_secs(150));
}

/// Marking an excluded folder (`node_modules`, `.git`…) as an *excluded*
/// placeholder clears "Sync pending" from every folder above it, without
/// converting any of its files. Its contents must never pay
/// for that: they stay ordinary files through "free up space" on a parent,
/// unmarking, and the sync root's registration being removed (an
/// uninstall, Files On-Demand turned off, or Windows dropping it).
#[test]
fn an_excluded_folder_mark_never_costs_its_ordinary_contents() {
    let base = std::env::temp_dir().join(format!("oneup-cfapi-excluded-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&base);
    let root = base.join("root");
    std::fs::create_dir_all(&root).unwrap();
    // A file outside the root, hard-linked inside (a conda environment).
    std::fs::write(base.join("shared.dll"), b"shared library").unwrap();
    provider::register(&root, b"test-pair").unwrap();
    let cloud = Arc::new(Cloud::default());
    let conn = provider::connect(&root, Arc::new(Serve(Arc::clone(&cloud)))).unwrap();
    let id = identity("project", b"");
    provider::create(&root, &[NewPlaceholder { name: "project", identity: &id, size: 0, modified: 0, folder: true }])
        .unwrap();
    let nm = root.join("project").join("node_modules");
    std::fs::create_dir_all(nm.join("pkg/lib")).unwrap();
    std::fs::write(nm.join("pkg/index.js"), b"module.exports = 1").unwrap();
    std::fs::write(nm.join("pkg/lib/deep.js"), b"deep inside").unwrap();
    std::fs::hard_link(base.join("shared.dll"), nm.join("shared.dll")).unwrap();
    // An online-only placeholder nobody serves, as the OneDrive app has
    // been seen to leave inside excluded folders.
    let stub = NewPlaceholder { name: "leftover.txt", identity: b"FOREIGN!", size: 5, modified: 0, folder: false };
    provider::create(&nm, &[stub]).unwrap();
    let stub_here = |why: &str| assert!(nm.join("leftover.txt").exists(), "the leftover stays: {why}");
    let mark = |p: &Path| {
        Exclusive::open(p).unwrap().convert(&identity("excluded", b"")).unwrap();
        provider::set_pin(&provider::open(p).unwrap(), Pin::Excluded).unwrap();
    };
    let intact = |why: &str| {
        assert_eq!(std::fs::read(nm.join("pkg/index.js")).unwrap(), b"module.exports = 1", "{why}");
        assert_eq!(std::fs::read(nm.join("pkg/lib/deep.js")).unwrap(), b"deep inside", "{why}");
        assert_eq!(std::fs::read(nm.join("shared.dll")).unwrap(), b"shared library", "{why}");
        for f in ["pkg/index.js", "pkg/lib/deep.js", "shared.dll"] {
            assert!(provider::info(&provider::open(&nm.join(f)).unwrap()).unwrap().is_none(), "{f} ordinary: {why}");
        }
    };

    mark(&nm);
    let i = provider::info(&provider::open(&nm).unwrap()).unwrap().unwrap();
    assert!(i.in_sync && i.pin == Pin::Excluded, "{i:?}");
    intact("marked");
    stub_here("marked");

    // "Free up space" on the project: the files have nothing to free (not
    // placeholders), and no dehydration can touch them.
    provider::set_pin_tree(&provider::open(&root.join("project")).unwrap(), Pin::Unpinned).unwrap();
    assert!(Exclusive::open(&nm.join("pkg/index.js")).unwrap().dehydrate(None).is_err(), "not a placeholder");
    intact("after free up space on the parent");
    stub_here("after free up space on the parent");

    // Unmarked as the agent does it: only the pin, never a revert (which
    // has removed a leftover like this one).
    provider::set_pin(&provider::open(&nm).unwrap(), Pin::Unspecified).unwrap();
    let i = provider::info(&provider::open(&nm).unwrap()).unwrap().unwrap();
    assert!(i.in_sync && i.pin == Pin::Unspecified, "{i:?}");
    intact("unmarked");
    stub_here("unmarked");

    // Marked again as the agent does (already a placeholder: the pin
    // only), then the registration removed (ordinary files stay).
    provider::set_pin(&provider::open(&nm).unwrap(), Pin::Excluded).unwrap();
    drop(conn);
    provider::unregister(&root).unwrap();
    assert!(provider::info(&provider::open(&nm).unwrap()).unwrap().is_none(), "an ordinary folder after unregistering");
    intact("after the registration was removed");
    let _ = std::fs::remove_dir_all(&base);
}

/// Spike (after a loss in use): what happens to
/// online-only placeholders inside an excluded folder when the folder is
/// marked (converted, pinned excluded), read, and unmarked (reverted).
#[test]
#[ignore = "a spike: prints what it finds"]
fn spike_online_only_files_inside_a_marked_folder() {
    let cloud = Arc::new(Cloud::default());
    let mut r = root("spike-marked-online", &cloud);
    // ONEUP_SPIKE_DISCONNECT: no provider connected (the agent stopped).
    let disconnect = std::env::var_os("ONEUP_SPIKE_DISCONNECT").is_some();
    let nm = r.root.join("excluded");
    std::fs::create_dir_all(&nm).unwrap();
    std::fs::write(nm.join("ordinary.txt"), b"ordinary").unwrap();
    let ph = |name: &'static str, id: &'static [u8], size: i64| NewPlaceholder {
        name,
        identity: id,
        size,
        modified: 133_000_000_000_000_000,
        folder: false,
    };
    // Online-only placeholders nobody serves (as the OneDrive app's left behind).
    provider::create(&nm, &[ph("never-read.txt", b"FOREIGN!1", 5), ph("read-me.txt", b"FOREIGN!2", 5)]).unwrap();
    let list = |when: &str| {
        let mut names: Vec<String> =
            std::fs::read_dir(&nm).unwrap().flatten().map(|e| e.file_name().into_string().unwrap()).collect();
        names.sort();
        eprintln!("{when}: {names:?}");
    };
    list("before marking");
    eprintln!("read before marking: {:?}", app_read(&r.base, &nm.join("read-me.txt")).map(|b| b.len()));
    Exclusive::open(&nm).unwrap().convert(&identity("excluded", b"")).unwrap();
    provider::set_pin(&provider::open(&nm).unwrap(), Pin::Excluded).unwrap();
    list("marked");
    if disconnect {
        r._conn.take();
        eprintln!("provider disconnected");
    }
    eprintln!("read while marked: {:?}", app_read(&r.base, &nm.join("read-me.txt")).map(|b| b.len()));
    list("marked, after a read");
    provider::set_pin(&provider::open(&nm).unwrap(), Pin::Unspecified).unwrap();
    list("pin set back to unspecified");
    provider::revert(&provider::open(&nm).unwrap(), 0).unwrap();
    list("reverted");
}

/// Spike: what Explorer shows for a folder whose files are a
/// mix of online-only and downloaded placeholders, all in sync. Run with
/// ONEUP_SPIKE_ROOT set to a shell-registered folder (`oneup
/// dev-shell-root DIR`); writes `ready` beside it and waits.
#[test]
#[ignore = "a spike: run by hand, then read the shell's properties during the pause"]
fn spike_status_of_mixed_folders() {
    let cloud = Arc::new(Cloud::default());
    let dir = PathBuf::from(std::env::var_os("ONEUP_SPIKE_ROOT").expect("ONEUP_SPIKE_ROOT"));
    let conn = provider::connect(&dir, Arc::new(Serve(Arc::clone(&cloud)))).unwrap();
    let folder = |name: &str| {
        let id = identity(name, b"");
        provider::create(&dir, &[NewPlaceholder { name, identity: &id, size: 0, modified: 0, folder: true }]).unwrap();
        dir.join(name)
    };
    let file = |d: &Path, name: &str, content: &[u8], download: bool| {
        let id = identity(name, content);
        cloud.content.lock().unwrap().insert(id.clone(), content.to_vec());
        let ph = NewPlaceholder { name, identity: &id, size: content.len() as i64, modified: 0, folder: false };
        provider::create(d, &[ph]).unwrap();
        if download {
            assert_eq!(app_read(dir.parent().unwrap(), &d.join(name)).unwrap(), content);
        }
    };
    let all_online = folder("all-online-only");
    file(&all_online, "a.txt", b"aaaa", false);
    file(&all_online, "b.txt", b"bbbb", false);
    let all_here = folder("all-downloaded");
    file(&all_here, "a.txt", b"aaaa", true);
    file(&all_here, "b.txt", b"bbbb", true);
    let mixed = folder("mixed");
    file(&mixed, "a.txt", b"aaaa", true);
    file(&mixed, "b.txt", b"bbbb", false);
    let mixed_pinned = folder("mixed-pinned-file");
    file(&mixed_pinned, "a.txt", b"aaaa", true);
    provider::set_pin(&provider::open(&mixed_pinned.join("a.txt")).unwrap(), Pin::Pinned).unwrap();
    file(&mixed_pinned, "b.txt", b"bbbb", false);
    let nested = folder("online-subfolder-and-a-file");
    let id = identity("sub", b"");
    provider::create(&nested, &[NewPlaceholder { name: "sub", identity: &id, size: 0, modified: 0, folder: true }])
        .unwrap();
    file(&nested.join("sub"), "c.txt", b"cccc", false);
    file(&nested, "d.txt", b"dddd", true);
    std::fs::write(dir.parent().unwrap().join("ready"), dir.display().to_string()).unwrap();
    eprintln!("ready");
    std::thread::sleep(std::time::Duration::from_secs(120));
    drop(conn);
}
