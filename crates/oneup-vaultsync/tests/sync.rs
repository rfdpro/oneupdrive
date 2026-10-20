//! A vault pair synced end to end: the real scanner, journal observer,
//! planner, safety gate and executor (and, on demand, the Files On-Demand
//! provider and hydration service) on a temp folder, with the vault's
//! encrypted files in an in-memory cloud. Another device changes the vault
//! through its own `VaultCloud`. What ends up in the cloud is checked with
//! the format library (`oneup_vault::folder`), as Cryptomator would see it.
#![cfg(windows)]
#![allow(clippy::unwrap_used)]

mod common;

use std::collections::BTreeMap;
use std::os::windows::process::CommandExt;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::Duration;

use common::*;
use oneup_cfapi::provider;
use oneup_effects::LocalFs;
use oneup_exec::hydrate::{Hydrator, Serve};
use oneup_exec::{Cloud, Executor, Report, link_roots};
use oneup_gate::{Context, Policy, review_in};
use oneup_local::observe::Observer;
use oneup_local::{Config, Scan};
use oneup_plan::Parked;
use oneup_rules::ExcludeSet;
use oneup_store::Store;
use oneup_sync::{RemoteId, all_folders, plan};
use oneup_vault::folder::{Dir, Folder};
use oneup_vaultsync::{VaultCloud, apply, project};

const TOKEN: &str = "vault-pair-token";

struct Pair {
    dir: PathBuf,
    db: PathBuf,
    obs: Observer,
    exec: Executor<VaultCloud<Fake>>,
    s: Setup,
    conn: Option<provider::Connection>,
    _hydrator: Option<Arc<Hydrator<VaultCloud<Fake>>>>,
}

impl Drop for Pair {
    fn drop(&mut self) {
        self.conn.take();
        let _ = provider::unregister(&self.dir);
        let _ = std::fs::remove_dir_all(&self.dir);
        let _ = std::fs::remove_dir_all(format!("{}.staging", self.dir.display()));
        for ext in ["", "-wal", "-shm"] {
            let _ = std::fs::remove_file(format!("{}{ext}", self.db.display()));
        }
    }
}

impl Pair {
    fn new(tag: &str, s: Setup, on_demand: bool) -> Self {
        let dir = std::env::temp_dir().join(format!("oneup-vaultpair-{tag}-{}", std::process::id()));
        let _ = provider::unregister(&dir);
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        let db = PathBuf::from(format!("{}.db", dir.display()));
        for ext in ["", "-wal", "-shm"] {
            let _ = std::fs::remove_file(format!("{}{ext}", db.display()));
        }
        if on_demand {
            provider::register(&dir, TOKEN.as_bytes()).unwrap();
        }
        let rules = ExcludeSet::none();
        let mut scan = Scan::open(Store::open(&db, 4096).unwrap(), &dir, Config::new(rules.clone())).unwrap();
        assert!(scan.run().unwrap().finished);
        let mut store = scan.into_store();
        let root = store.read(|o| o.local_root()).unwrap().unwrap();
        assert!(store.write(|o| link_roots(o, root.root_file_id, &s.vault.root)).unwrap());
        let fs = LocalFs::open(&dir, root.volume_serial, root.root_file_id, TOKEN, true).unwrap();
        assert_eq!(fs.on_demand(), on_demand);
        let cloud = || VaultCloud::new(s.cloud.clone(), s.vault.clone(), Arc::clone(&s.nodes), Arc::clone(&s.listing));
        let exec = Executor::new(fs, cloud(), rules.clone());
        let (conn, hydrator) = if on_demand {
            let h = Hydrator::new(
                cloud(),
                Store::open(&db, 1024).unwrap(),
                PathBuf::from(format!("{}.staging", dir.display())),
                &dir,
                tokio::runtime::Handle::current(),
                Box::new(|| {}),
            )
            .unwrap();
            (Some(provider::connect(&dir, Arc::new(Serve(Arc::clone(&h)))).unwrap()), Some(h))
        } else {
            (None, None)
        };
        let obs = Observer::open(store, Config::new(rules)).unwrap();
        Self { dir, db, obs, exec, s, conn, _hydrator: hydrator }
    }

    fn p(&self, rel: &str) -> PathBuf {
        self.dir.join(rel)
    }

    /// One round as the agent runs it for a vault pair: the local journal,
    /// the decrypted view of the cloud listing, a plan, the gate, the
    /// executor.
    async fn round(&mut self) -> Report {
        self.obs.catch_up().unwrap();
        let mut cache = oneup_vaultsync::project::Cache::default();
        let p = project(&self.s.src, &self.s.vault, &mut cache).await.unwrap();
        assert!(p.problems.is_empty(), "{:?}", p.problems);
        *self.s.nodes.write().unwrap() = p.nodes.clone();
        let store = self.obs.store_mut();
        apply(store, &self.s.src, &p).unwrap();
        let ops = store
            .read(|o| {
                let p = Parked::new(o);
                plan(&p, all_folders(&p)?)
            })
            .unwrap();
        let r = review_in(&Policy::default(), &Context::default(), ops);
        assert!(r.breakers.is_empty(), "{:?}", r.breakers);
        let report = self.exec.run(store, r.approved).await;
        assert!(report.halted.is_none() && report.failed.is_empty(), "{report:?}");
        report
    }

    /// Rounds until one does nothing.
    async fn quiesce(&mut self) {
        for _ in 0..25 {
            let r = self.round().await;
            if r.is_idle() && r.skipped.is_empty() {
                std::thread::sleep(Duration::from_millis(150));
                if self.round().await.is_idle() {
                    return;
                }
            }
        }
        panic!("no convergence");
    }

    /// The vault as Cryptomator would see it: path → content.
    fn vault(&self) -> BTreeMap<String, Option<Vec<u8>>> {
        let out = PathBuf::from(format!("{}.export", self.dir.display()));
        let _ = std::fs::remove_dir_all(&out);
        export(&self.s.cloud, &self.s.vault.root, &out);
        let v = Folder::open(&out, "pw").unwrap();
        let mut tree = BTreeMap::new();
        let mut stack = vec![(Dir::root(), String::new())];
        while let Some((d, path)) = stack.pop() {
            let l = v.list(&d).unwrap();
            assert!(l.problems.is_empty(), "{:?}", l.problems);
            for e in l.entries {
                let p = format!("{path}/{}", e.name);
                match &e.kind {
                    oneup_vault::folder::Kind::Dir(sub) => {
                        tree.insert(p.clone(), None);
                        stack.push((sub.clone(), p));
                    }
                    _ => {
                        let mut c = Vec::new();
                        v.read(&e, &mut c).unwrap();
                        tree.insert(p, Some(c));
                    }
                }
            }
        }
        let _ = std::fs::remove_dir_all(&out);
        tree
    }

    /// Path → content of the local folder (ordinary files only: never
    /// called with placeholders around).
    fn local(&self) -> BTreeMap<String, Option<Vec<u8>>> {
        let mut out = BTreeMap::new();
        let mut stack = vec![(self.dir.clone(), String::new())];
        while let Some((d, path)) = stack.pop() {
            for e in std::fs::read_dir(&d).unwrap().flatten() {
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
}

fn write(p: &Path, c: &[u8]) {
    std::fs::create_dir_all(p.parent().unwrap()).unwrap();
    std::fs::write(p, c).unwrap();
}

/// Another program (not the provider's executable) reads a file.
fn app_read(p: &Path) -> Option<Vec<u8>> {
    static N: std::sync::atomic::AtomicU32 = std::sync::atomic::AtomicU32::new(0);
    let n = N.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
    let out = std::env::temp_dir().join(format!("oneup-vaultpair-read-{}-{n}.out", std::process::id()));
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
    let st = std::process::Command::new("cmd").raw_arg(format!("/C echo {text}>>\"{}\"", p.display())).status();
    assert!(st.unwrap().success());
}

/// Option 2 (full files): both sides' changes go through, encrypted in the
/// cloud, decrypted here.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_vault_pair_with_full_files_syncs_both_ways() {
    let mut s = setup();
    s.refresh().await;
    let root = RemoteId(s.vault.root.clone());
    // Another device put something in the vault.
    let docs = s.vc.mkdir(&root, "Docs").await.unwrap();
    s.vc.upload(&RemoteId(docs.id), "from cloud.txt", None, 10, &mut &b"from cloud"[..]).await.unwrap();
    let mut p = Pair::new("full", s, false);
    write(&p.p("here.txt"), b"made here");
    write(&p.p("Sub/inner.txt"), b"inner");
    p.quiesce().await;
    assert_eq!(p.local(), p.vault());
    assert_eq!(p.vault()["/Docs/from cloud.txt"].as_deref(), Some(&b"from cloud"[..]));
    assert_eq!(p.vault()["/Sub/inner.txt"].as_deref(), Some(&b"inner"[..]));

    // Edits, a rename and a deletion here; an edit on the other device.
    write(&p.p("here.txt"), b"edited here");
    std::fs::rename(p.p("Sub/inner.txt"), p.p("Sub/renamed.txt")).unwrap();
    std::fs::remove_file(p.p("Docs/from cloud.txt")).unwrap();
    p.quiesce().await;
    // The other device sees the vault as it is now.
    p.s.refresh().await;
    let pre = p.s.pre("/Sub/renamed.txt");
    p.s.vc.upload(&p.s.id("/Sub"), "renamed.txt", Some(&pre), 13, &mut &b"edited there!"[..]).await.unwrap();
    p.quiesce().await;
    let v = p.vault();
    assert_eq!(p.local(), v);
    assert_eq!(v["/here.txt"].as_deref(), Some(&b"edited here"[..]));
    assert_eq!(v["/Sub/renamed.txt"].as_deref(), Some(&b"edited there!"[..]));
    assert!(!v.contains_key("/Docs/from cloud.txt") && !v.contains_key("/Sub/inner.txt"));

    // Names too long to store as they are (Cryptomator keeps them in a
    // folder of their own): a file and a folder renamed to one, and back.
    let long = "a name far too long to keep as it is".repeat(5);
    std::fs::rename(p.p("here.txt"), p.p(&format!("{long}.txt"))).unwrap();
    std::fs::rename(p.p("Sub"), p.p(&long)).unwrap();
    p.quiesce().await;
    let v = p.vault();
    assert_eq!(p.local(), v);
    assert_eq!(v[&format!("/{long}.txt")].as_deref(), Some(&b"edited here"[..]));
    assert_eq!(v[&format!("/{long}/renamed.txt")].as_deref(), Some(&b"edited there!"[..]));
    std::fs::rename(p.p(&long), p.p("Short again")).unwrap();
    p.quiesce().await;
    assert_eq!(p.local(), p.vault());
    assert!(p.vault().contains_key("/Short again/renamed.txt"));
    drop(p);
}

/// Option 3 (the default): the vault's files are online-only here, and
/// decrypted only when a program opens one; a program's edit goes back
/// encrypted.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_vault_pair_on_demand_decrypts_only_what_is_opened() {
    let mut s = setup();
    s.refresh().await;
    let root = RemoteId(s.vault.root.clone());
    s.vc.upload(&root, "secret.txt", None, 6, &mut &b"secret"[..]).await.unwrap();
    s.vc.upload(&root, "unopened.txt", None, 8, &mut &b"unopened"[..]).await.unwrap();
    let mut p = Pair::new("ondemand", s, true);
    p.quiesce().await;

    let (secret, unopened) = (p.p("secret.txt"), p.p("unopened.txt"));
    let info = |f: &Path| provider::info(&provider::open(f).unwrap()).unwrap().unwrap();
    assert_eq!(info(&secret).on_disk, 0, "arrives online-only");
    assert_eq!(std::fs::metadata(&secret).unwrap().len(), 6, "with its decrypted size");
    assert_eq!(app_read(&secret).unwrap(), b"secret", "decrypted when opened");
    assert_eq!(info(&unopened).on_disk, 0, "nothing else was decrypted");

    // A program edits the opened file: it goes back encrypted.
    app_append(&secret, " plus more");
    p.quiesce().await;
    assert_eq!(p.vault()["/secret.txt"].as_deref(), Some(&b"secret plus more\r\n"[..]));
    // A new file here, too.
    std::process::Command::new("cmd")
        .raw_arg(format!("/C echo new>\"{}\"", p.p("new.txt").display()))
        .status()
        .unwrap();
    p.quiesce().await;
    assert_eq!(p.vault()["/new.txt"].as_deref(), Some(&b"new\r\n"[..]));
    assert_eq!(info(&unopened).on_disk, 0);

    // Renamed to a name too long to store as it is, while online-only: the
    // vault rebuilds its entry, and the file still opens.
    let long = p.p(&format!("{}.txt", "still online-only after a long rename ".repeat(5)));
    std::fs::rename(&unopened, &long).unwrap();
    p.quiesce().await;
    assert_eq!(info(&long).on_disk, 0, "still online-only");
    assert_eq!(app_read(&long).unwrap(), b"unopened", "and opens with its content");
    drop(p);
}

/// Another client moves a long-named file into another folder by moving
/// its `.c9s` folder whole (renamed, with its `name.c9s` rewritten), and
/// the next round plans to follow it here. Before that move runs, the
/// entry's contents move out (a later move of that client that changed
/// the entry's shape, whose old entry stayed behind with only its name),
/// so the listing shows a shell where the file went. The executor asks
/// OneDrive (`Cloud::is_at`), finds the entry without its contents, and
/// doesn't follow it: the file stays where it is.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_vault_move_to_an_entry_left_without_its_contents_is_not_followed() {
    use oneup_store::Kind;
    use oneup_sync::Op;
    use oneup_vault::names::{self, Stored};
    let mut s = setup();
    s.refresh().await;
    let root = RemoteId(s.vault.root.clone());
    let long = format!("{}.txt", "a long name ".repeat(20));
    let a = RemoteId(s.vc.mkdir(&root, "A").await.unwrap().id);
    let b = RemoteId(s.vc.mkdir(&root, "B").await.unwrap().id);
    s.vc.upload(&a, &long, None, 4, &mut &b"long"[..]).await.unwrap();
    s.vc.upload(&b, "b.txt", None, 1, &mut &b"b"[..]).await.unwrap();
    let mut p = Pair::new("shell-move", s, false);
    p.quiesce().await;
    assert!(p.p(&format!("A/{long}")).exists());

    // The other client's move of the whole `.c9s` folder into B.
    p.s.refresh().await;
    let holder = p.s.id(&format!("/A/{long}"));
    let to = p.s.nodes.read().unwrap()[&b.0].clone();
    let (b_dir, b_storage) = (to.dir_id.unwrap(), to.storage.unwrap());
    let enc = names::encrypt_name(&p.s.vault.key, &long, &b_dir).unwrap();
    let Stored::Short { dir, name } = Stored::of(enc, p.s.vault.threshold) else { panic!("a long name") };
    let (rows, contents) = {
        let mut c = p.s.cloud.0.lock().unwrap();
        let (e1, e2) = (c.bump(), c.bump());
        let h = c.items.get_mut(&holder.0).unwrap();
        (h.parent, h.name, h.etag) = (Some(b_storage), dir, e1);
        let name_file = c.child(&holder.0, names::NAME_FILE).unwrap();
        let n = c.items.get_mut(&name_file).unwrap();
        (n.content, n.etag) = (name.into_bytes(), e2);
        let contents = c.child(&holder.0, names::CONTENTS_FILE).unwrap();
        (vec![c.row(&holder.0), c.row(&name_file)], contents)
    };
    p.s.listing.lock().unwrap().write(|o| rows.iter().try_for_each(|r| o.upsert_item(r, false))).unwrap();

    // The round's plan follows it here.
    p.obs.catch_up().unwrap();
    let mut cache = oneup_vaultsync::project::Cache::default();
    let projected = project(&p.s.src, &p.s.vault, &mut cache).await.unwrap();
    assert!(projected.problems.is_empty(), "{:?}", projected.problems);
    *p.s.nodes.write().unwrap() = projected.nodes.clone();
    let store = p.obs.store_mut();
    apply(store, &p.s.src, &projected).unwrap();
    let ops = store.read(|o| plan(&Parked::new(o), all_folders(&Parked::new(o))?)).unwrap();
    assert!(ops.iter().any(|op| matches!(op, Op::MoveLocal { .. })), "{ops:?}");
    let r = review_in(&Policy::default(), &Context::default(), ops);

    // Meanwhile its contents leave the moved entry, which the listing
    // doesn't show yet.
    {
        let mut c = p.s.cloud.0.lock().unwrap();
        let aside = c.create(&root.0, "aside", true, vec![]).unwrap();
        assert!(!c.items[&contents].folder);
        c.items.get_mut(&contents).unwrap().parent = Some(aside);
        assert!(c.row(&holder.0).kind == Kind::Folder);
    }
    let report = p.exec.run(store, r.approved).await;
    assert!(report.halted.is_none() && report.failed.is_empty(), "{report:?}");
    assert!(
        report.skipped.iter().any(|(op, k)| matches!(op, Op::MoveLocal { .. }) && *k == oneup_exec::Skip::Changed),
        "{report:?}"
    );
    assert!(p.p(&format!("A/{long}")).exists(), "the file stays where it is");
    assert!(!p.p(&format!("B/{long}")).exists());
}
