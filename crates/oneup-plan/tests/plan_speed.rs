//! How long planning every folder takes on a large tree (60k
//! folders, 300k files, all in sync), through the views
//! the agent plans with. Run by hand:
//! `cargo test --release -p oneup-plan --test plan_speed -- --ignored --nocapture`.
#![allow(clippy::unwrap_used)]

use std::time::Instant;

use oneup_plan::Parked;
use oneup_rules::RuleSet;
use oneup_store::{Kind, LocalKind, LocalRow, Ops, RemoteRow, Skip, Store};
use oneup_sync::{Base, LocalId, NodeKind, RemoteId, all_folders, plan};

const TOPS: u64 = 245;
const SUBS: u64 = 245;
const FILES: u64 = 5;

fn local(id: u64, parent: Option<u64>, name: &str, kind: LocalKind) -> LocalRow {
    LocalRow {
        file_id: u128::from(id) + 1,
        parent_id: parent.map(|p| u128::from(p) + 1),
        name: name.into(),
        name_lossy: false,
        kind,
        size: if kind == LocalKind::File { 3 } else { 0 },
        mtime: 9,
        attributes: if kind == LocalKind::File { 32 } else { 16 },
        reparse_tag: 0,
        skip: Skip::None,
    }
}

fn remote(id: u64, parent: Option<u64>, name: &str, kind: Kind, hash: Option<[u8; 20]>) -> RemoteRow {
    RemoteRow {
        id: format!("r{id}"),
        parent_id: parent.map(|p| format!("r{p}")),
        name: name.into(),
        kind,
        size: if kind == Kind::File { 3 } else { 0 },
        etag: Some("e".into()),
        qxh: hash,
        mtime_ms: None,
    }
}

fn base(sid: u64, parent: Option<u64>, name: &str, kind: NodeKind, hash: Option<[u8; 20]>) -> Base {
    Base {
        sid,
        parent,
        name: name.into(),
        kind,
        size: if kind == NodeKind::File { 3 } else { 0 },
        hash,
        local: Some(LocalId(u128::from(sid) + 1)),
        local_mtime: 9,
        remote: Some(RemoteId(format!("r{sid}"))),
        remote_etag: "e".into(),
    }
}

fn build(o: &Ops<'_>) -> oneup_store::Result<()> {
    let hash = [7u8; 20];
    let folder = |sid: u64, parent: Option<u64>, name: &str| -> oneup_store::Result<()> {
        let (lk, rk) = if parent.is_none() { (LocalKind::Root, Kind::Root) } else { (LocalKind::Folder, Kind::Folder) };
        o.local_put(&local(sid, parent, name, lk))?;
        o.upsert_item(&remote(sid, parent, name, rk, None), false)?;
        o.synced_put(&base(sid, parent, name, NodeKind::Folder, None))
    };
    folder(0, None, "")?;
    let mut sid = 0;
    for t in 0..TOPS {
        sid += 1;
        let top = sid;
        folder(top, Some(0), &format!("top{t}"))?;
        for u in 0..SUBS {
            sid += 1;
            let sub = sid;
            folder(sub, Some(top), &format!("sub{u}"))?;
            for f in 0..FILES {
                sid += 1;
                let name = format!("file{f}.txt");
                o.local_put(&local(sid, Some(sub), &name, LocalKind::File))?;
                o.local_hash_put(u128::from(sid) + 1, 3, 9, &hash)?;
                o.upsert_item(&remote(sid, Some(sub), &name, Kind::File, Some(hash)), false)?;
                o.synced_put(&base(sid, Some(sub), &name, NodeKind::File, Some(hash)))?;
            }
        }
    }
    o.dirty_clear(i64::MAX).map(drop)
}

#[test]
#[ignore]
fn full_plan_speed() {
    let path = std::env::temp_dir().join(format!("oneup-plan-speed-{}.db", std::process::id()));
    let _ = std::fs::remove_file(&path);
    let t = Instant::now();
    Store::open(&path, 64 * 1024).unwrap().write(build).unwrap();
    let mb = std::fs::metadata(&path).unwrap().len() >> 20;
    println!("built {} items, {mb} MiB, in {:.1?}", TOPS * SUBS * (FILES + 1) + TOPS + 1, t.elapsed());
    // Typical rules for a developer's drive.
    let rules = RuleSet::new(&["dev", "temp", "os-junk"], &["*.log", "*.safetensors", ".safetensors"]).unwrap();
    let s = Store::open(&path, 64 * 1024).unwrap();
    let time = |what: &str, f: &dyn Fn(&Ops<'_>) -> usize| {
        let t = Instant::now();
        let n = s.read(|o| Ok(f(o))).unwrap();
        println!("{what}: {n} ops, {:.2?}", t.elapsed());
    };
    time("store directly", &|o| plan(o, all_folders(o).unwrap()).unwrap().len());
    time("parked, no rules", &|o| {
        let p = Parked::new(o);
        plan(&p, all_folders(&p).unwrap()).unwrap().len()
    });
    time("parked, with rules", &|o| {
        let p = Parked::with_rules(o, &rules);
        plan(&p, all_folders(&p).unwrap()).unwrap().len()
    });
    drop(s);

    // A large WAL: changes made while another connection kept reading
    // (the hydration service, the settings window), so they couldn't be
    // folded into the database. One has been seen reaching 890 MB.
    let reader = Store::open(&path, 1024).unwrap();
    let (hold, release) = std::sync::mpsc::channel::<()>();
    let (held, ready) = std::sync::mpsc::channel::<()>();
    let t = std::thread::spawn(move || {
        // A query part-way through holds its snapshot.
        let mut first = true;
        reader
            .read(|o| {
                o.for_each_local_folder(|_, _, _, _| {
                    if std::mem::take(&mut first) {
                        held.send(()).unwrap();
                        release.recv().unwrap();
                    }
                })
            })
            .unwrap();
        reader
    });
    ready.recv().unwrap();
    let mut w = Store::open(&path, 64 * 1024).unwrap();
    for round in 0..3 {
        w.write(|o| {
            for sid in 0..=(TOPS * SUBS * (FILES + 1) + TOPS) {
                let (kind, hash) =
                    if sid % (FILES + 1) == 0 { (Kind::Folder, None) } else { (Kind::File, Some([7u8; 20])) };
                let mut r = remote(sid, None, "n", kind, hash);
                r.etag = Some(format!("e{round}"));
                // Only the eTag changes: same parent and name as built.
                if let Some(old) = o.get_item(&r.id)? {
                    r.parent_id = old.parent_id;
                    r.name = old.name;
                    r.kind = old.kind;
                }
                o.upsert_item(&r, false)?;
            }
            o.dirty_clear(i64::MAX).map(drop)
        })
        .unwrap();
    }
    hold.send(()).unwrap();
    let reader = t.join().unwrap();
    let wal = std::fs::metadata(format!("{}-wal", path.display())).map_or(0, |m| m.len() >> 20);
    println!("WAL: {wal} MiB");
    let s = Store::open(&path, 64 * 1024).unwrap();
    let time = |what: &str, s: &Store| {
        let t = Instant::now();
        let n = s.read(|o| {
            let p = Parked::with_rules(o, &rules);
            Ok(plan(&p, all_folders(&p).unwrap()).unwrap().len())
        });
        println!("{what}: {} ops, {:.2?}", n.unwrap(), t.elapsed());
    };
    time("parked, rules, large WAL", &s);
    assert!(s.checkpoint().unwrap());
    let wal = std::fs::metadata(format!("{}-wal", path.display())).map_or(0, |m| m.len() >> 20);
    println!("WAL after checkpoint: {wal} MiB");
    time("parked, rules, after a checkpoint", &s);
    drop((s, w, reader));
    for ext in ["", "-wal", "-shm"] {
        let _ = std::fs::remove_file(format!("{}{ext}", path.display()));
    }
}
