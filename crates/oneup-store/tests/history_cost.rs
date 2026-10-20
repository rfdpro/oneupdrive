//! What keeping the Synced tree's history costs on a large synced folder
//! (`oneup_store::history`). Ignored by default: it builds a database of
//! millions of records on disk. Run it in a release build:
//!
//! ```text
//! cargo test --release -p oneup-store --test history_cost -- --ignored --nocapture
//! ```
//!
//! `HISTORY_ITEMS` (default 3,000,000) sets the size of the Synced tree,
//! `HISTORY_CHANGES` (default 1,000,000) how many changes are recorded.
#![allow(clippy::unwrap_used)]

use std::time::Instant;

use oneup_store::Store;
use oneup_store::history::Keep;
use oneup_sync::{Base, LocalId, NodeKind, ROOT, RemoteId, Sid};

fn env(name: &str, default: u64) -> u64 {
    std::env::var(name).ok().and_then(|v| v.parse().ok()).unwrap_or(default)
}

fn file(sid: Sid, parent: Sid, name: String, size: i64) -> Base {
    Base {
        sid,
        parent: Some(parent),
        name,
        kind: NodeKind::File,
        size,
        hash: Some([(sid % 251) as u8; 20]),
        local: Some(LocalId(u128::from(sid) + 1_000_000_000)),
        local_mtime: 133_000_000_000_000_000,
        remote: Some(RemoteId(format!("synthetic-drive!{sid}"))),
        remote_etag: format!("aRDRGM0EzQjFCMkM1RTZGNzA4ITEyMy4w,{sid}"),
    }
}

fn size_of(path: &std::path::Path) -> u64 {
    ["", "-wal"].iter().map(|e| std::fs::metadata(format!("{}{e}", path.display())).map_or(0, |m| m.len())).sum()
}

#[test]
#[ignore = "builds a large database on disk; run by hand in a release build"]
fn the_history_costs_what_changes_cost() {
    let items = env("HISTORY_ITEMS", 3_000_000);
    let changes = env("HISTORY_CHANGES", 1_000_000);
    let dir = std::env::temp_dir().join(format!("oneup-history-cost-{}", std::process::id()));
    std::fs::create_dir_all(&dir).unwrap();
    let path = dir.join("state.db");
    let mut s = Store::open(&path, 64 * 1024).unwrap();

    // A tree of folders of 100 files each, as a first sync records it
    // (the history isn't kept yet).
    let t = Instant::now();
    let per = 100u64;
    let folders = items / per;
    let mut sid: Sid = 1;
    let root = Base {
        sid: ROOT,
        parent: None,
        name: String::new(),
        kind: NodeKind::Folder,
        size: 0,
        hash: None,
        local: Some(LocalId(1)),
        local_mtime: 0,
        remote: Some(RemoteId("root".into())),
        remote_etag: String::new(),
    };
    s.write(|o| o.synced_put(&root)).unwrap();
    let mut files: Vec<(Sid, Sid)> = Vec::new();
    for f in 0..folders {
        let fsid = sid;
        sid += 1;
        let mut batch =
            vec![Base { kind: NodeKind::Folder, size: 0, hash: None, ..file(fsid, ROOT, format!("Folder {f:06}"), 0) }];
        for i in 0..per - 1 {
            batch.push(file(sid, fsid, format!("Document {i:03} of folder {f}.docx"), 20_000 + i as i64));
            files.push((sid, fsid));
            sid += 1;
        }
        if f % 1000 == 0 || f + 1 == folders {
            s.checkpoint().ok();
        }
        s.write(|o| {
            for b in &batch {
                o.synced_put(b)?;
            }
            Ok(())
        })
        .unwrap();
    }
    s.checkpoint().unwrap();
    let base_size = size_of(&path);
    let rows = s.read(|o| o.history_rows()).unwrap();
    println!(
        "synced tree: {} records in {:.1} s, {} MB; history rows written by the first sync: {rows}",
        sid,
        t.elapsed().as_secs_f64(),
        base_size / 1_000_000
    );

    // Changes: an edit (size and hash), a rename or a move, round-robin.
    let change = |s: &mut Store, n: u64, salt: u64| {
        let t = Instant::now();
        let mut done = 0u64;
        while done < n {
            let chunk = (n - done).min(10_000);
            s.write(|o| {
                for k in 0..chunk {
                    let i = ((done + k) * 7919 + salt) as usize % files.len();
                    let (fsid, parent) = files[i];
                    let mut b = o.synced_get(fsid)?.unwrap();
                    match (done + k) % 3 {
                        0 => {
                            b.size += 1;
                            b.hash = Some([((done + k) % 251) as u8; 20]);
                        }
                        1 => b.name = format!("Renamed {} {}.docx", done + k, salt),
                        _ => {
                            b.parent =
                                Some(if b.parent == Some(parent) { files[(i + 1) % files.len()].1 } else { parent })
                        }
                    }
                    b.remote_etag.push('x');
                    o.synced_put(&b)?;
                }
                Ok(())
            })
            .unwrap();
            done += chunk;
        }
        t.elapsed().as_secs_f64()
    };

    // Without the history and with it, half the changes each, twice, so
    // the database growing and the cache warming weigh on both alike.
    let half = changes / 2;
    let mut off = change(&mut s, half, 1);
    s.write(|o| o.history_start(1)).unwrap();
    s.checkpoint().unwrap();
    let before = size_of(&path);
    let mut on = change(&mut s, half, 2);
    s.checkpoint().unwrap();
    let after = size_of(&path);
    let first_rows = s.read(|o| o.history_rows()).unwrap();
    s.write(|o| o.meta_delete(oneup_store::history::ON)).unwrap();
    off += change(&mut s, half, 3);
    s.write(|o| o.history_start(1)).unwrap();
    on += change(&mut s, half, 4);
    let rows = s.read(|o| o.history_rows()).unwrap();
    println!(
        "{changes} changes, half with the history kept: {off:.1} s without, {on:.1} s with ({:.1} µs more each); \
         {rows} history rows (a move within its folder changes nothing kept); the first {first_rows} took {} MB, \
         {:.0} bytes per row (row and indexes)",
        (on - off) * 1e6 / changes as f64,
        (after.saturating_sub(before)) / 1_000_000,
        after.saturating_sub(before) as f64 / first_rows.max(1) as f64
    );

    // Reading it: what a rewind asks, per item and per folder.
    let t = Instant::now();
    let probes = 10_000usize;
    s.read(|o| {
        for k in 0..probes {
            let (fsid, parent) = files[(k * 104_729) % files.len()];
            o.history_change_after(fsid, 0)?;
            if k % 10 == 0 {
                o.history_children_after(parent, 0)?;
            }
        }
        Ok(())
    })
    .unwrap();
    println!(
        "{probes} lookups of an item's first change and {} of a folder's past contents: {:.1} µs on average",
        probes / 10,
        t.elapsed().as_secs_f64() * 1e6 / (probes + probes / 10) as f64
    );

    // Pruning to the default limit.
    let t = Instant::now();
    let gone = s.write(|o| o.history_prune(Keep { days: 0, rows: Keep::default().rows / 2 }, 0)).unwrap();
    println!("pruned {gone} rows in {:.2} s", t.elapsed().as_secs_f64());
    drop(s);
    let _ = std::fs::remove_dir_all(&dir);
}
