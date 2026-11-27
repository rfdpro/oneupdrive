//! What a drive scan's writes cost (`docs/architecture/search.md` § Local
//! volume catalog): the same simulated scan of a drive catalog, with file ID
//! keys in either order, a small or a large page cache, and small or large
//! steps. Slow, so not run by default:
//!
//! ```text
//! cargo test -p oneup-store --release --test scan_order_cost -- --ignored --nocapture
//! ```
//!
//! Settings, each a comma-separated list (every combination runs):
//! `SCAN_COST_ITEMS` (default 1,000,000), `SCAN_COST_ORDER` (`native,ordered`),
//! `SCAN_COST_CACHE_MIB` (`4,64`), `SCAN_COST_BATCH` (folders per step,
//! `256,4096`), `SCAN_COST_ALLOC` (`clustered`).
//!
//! The rows go in as the scanner writes them (`oneup_local::scan`): deepest
//! folder first, a step's folders and entries in one transaction, the name
//! index fed afterwards. No folder is listed, so this is the database's
//! share of a scan alone. Record numbers follow one of two models:
//! `clustered`, a folder's entries made together in a run of nearby MFT
//! records, a few of them reusing old records anywhere on the volume; and
//! `random`, every entry anywhere (an old, much-churned volume: the worst
//! case for ordered keys).

#![allow(clippy::unwrap_used)]

use std::time::{Duration, Instant};

use oneup_store::{LocalKind, LocalRoot, LocalRow, Skip, Store};

struct Rng(u64);
impl Rng {
    fn next(&mut self) -> u64 {
        let mut x = self.0;
        x ^= x >> 12;
        x ^= x << 25;
        x ^= x >> 27;
        self.0 = x;
        x.wrapping_mul(0x2545_F491_4F6C_DD1D)
    }
    fn below(&mut self, n: u64) -> u64 {
        self.next() % n
    }
}

/// An NTFS file reference: sequence number above the 48-bit record number.
fn reference(record: u64, seq: u64) -> u128 {
    (u128::from(seq & 0xffff) << 48) | u128::from(record & 0xffff_ffff_ffff)
}

struct Records {
    rng: Rng,
    random: bool,
    next: u64,
    span: u64,
}

impl Records {
    /// IDs for one folder's `n` entries.
    fn folder(&mut self, n: usize) -> Vec<u128> {
        self.next += self.rng.below(64);
        (0..n)
            .map(|_| {
                if self.random || self.rng.below(10) < 2 {
                    // A record reused from anywhere, with a later sequence number.
                    reference(self.rng.below(self.span), 1 + self.rng.below(4000))
                } else {
                    self.next += 1;
                    reference(self.next, 1)
                }
            })
            .collect()
    }
}

fn row(id: u128, parent: u128, name: String, kind: LocalKind) -> LocalRow {
    LocalRow {
        file_id: id,
        parent_id: Some(parent),
        name,
        name_lossy: false,
        kind,
        size: 4096,
        mtime: 1,
        attributes: if kind == LocalKind::File { 0x20 } else { 0x10 },
        reparse_tag: 0,
        skip: Skip::None,
    }
}

struct Run {
    scan: Duration,
    index: Duration,
    bytes: u64,
    free: u64,
    compact: Duration,
    compacted: u64,
}

fn run(items: usize, ordered: bool, cache_mib: u32, batch: usize, random: bool) -> Run {
    let dir =
        std::env::temp_dir().join(format!("oneup-scan-cost-{}-{ordered}-{cache_mib}-{batch}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).unwrap();
    let db = dir.join("catalog.db");
    let mut s = Store::open(&db, cache_mib * 1024).unwrap();
    if ordered {
        s.use_ordered_keys().unwrap();
    }
    s.install_drive_search().unwrap();
    let mut records = Records { rng: Rng(0x9E37_79B9_7F4A_7C15), random, next: 1000, span: items as u64 * 4 };
    let root_id = reference(5, 5);
    let root = LocalRoot {
        path: r"\\?\T:\".into(),
        volume_serial: 1,
        root_file_id: root_id,
        rules: String::new(),
        journal: Some((1, 0)),
        started_at_ms: 0,
        finished_at_ms: None,
        sync_root_provider: None,
    };
    let root_row =
        LocalRow { parent_id: None, kind: LocalKind::Root, ..row(root_id, 0, String::new(), LocalKind::Root) };
    s.write(|o| o.local_begin(&root, &root_row)).unwrap();

    // The scanner's queue, deepest first.
    let mut queue: Vec<(u128, u32)> = vec![(root_id, 0)];
    let mut written = 0usize;
    let mut shape = Rng(42);
    let started = Instant::now();
    while !queue.is_empty() {
        queue.sort_by_key(|&(_, depth)| depth);
        // A step ends at `batch` folders or its entry cap: the drive
        // worker's 8,192 with 256 folders, the scanner's default 32,768
        // with 4,096 (`oneup_local::Config`).
        let cap = if batch <= 256 { 8192 } else { 32_768 };
        let mut step = Vec::new();
        let mut entries = 0;
        while let Some(next) = queue.pop() {
            step.push(next);
            entries += 20;
            if step.len() >= batch || entries >= cap {
                break;
            }
        }
        let mut found = Vec::new();
        s.write(|o| {
            for &(folder, depth) in &step {
                if written < items {
                    // About 20 entries a folder, one in eight of them a folder.
                    let n = if depth == 0 { 40 } else { 1 + shape.below(40) as usize };
                    for (i, id) in records.folder(n).into_iter().enumerate() {
                        let folder_kind = shape.below(8) == 0;
                        let kind = if folder_kind { LocalKind::Folder } else { LocalKind::File };
                        o.local_put(&row(id, folder, format!("entry {written} {i}.txt"), kind))?;
                        written += 1;
                        if folder_kind {
                            o.local_enqueue(id, depth + 1)?;
                            found.push((id, depth + 1));
                        }
                    }
                }
                o.local_dequeue(folder)?;
            }
            Ok(())
        })
        .unwrap();
        queue.extend(found);
    }
    s.write(|o| o.local_finish(1)).unwrap();
    let scan = started.elapsed();
    let stored = s.read(|o| o.local_stats()).unwrap();
    assert!(written >= items, "the simulated drive ran out of folders at {written} entries ({stored:?})");
    let started = Instant::now();
    while s.flush_drive_search(8192).unwrap() == 8192 {}
    let index = started.elapsed();
    s.checkpoint().unwrap();
    let bytes = s.files_size();
    let free = s.space().unwrap().free;
    let started = Instant::now();
    s.compact().unwrap();
    let compact = started.elapsed();
    let compacted = s.files_size();
    drop(s);
    let _ = std::fs::remove_dir_all(&dir);
    Run { scan, index, bytes, free, compact, compacted }
}

fn list(key: &str, default: &str) -> Vec<String> {
    std::env::var(key).unwrap_or_else(|_| default.into()).split(',').map(|s| s.trim().to_owned()).collect()
}

#[test]
#[ignore = "slow: simulates scans of a million items; run by hand in a release build"]
fn what_a_drive_scan_costs_the_database() {
    let items: usize = std::env::var("SCAN_COST_ITEMS").ok().and_then(|v| v.parse().ok()).unwrap_or(1_000_000);
    println!("{items} items");
    println!("alloc      order    cache  batch   scan s  items/s  index s  MiB   free MiB  compact s  compacted MiB");
    for alloc in list("SCAN_COST_ALLOC", "clustered") {
        for order in list("SCAN_COST_ORDER", "native,ordered") {
            for cache in list("SCAN_COST_CACHE_MIB", "4,64") {
                for batch in list("SCAN_COST_BATCH", "256,4096") {
                    let r = run(
                        items,
                        order == "ordered",
                        cache.parse().unwrap(),
                        batch.parse().unwrap(),
                        alloc == "random",
                    );
                    let mib = |b: u64| b as f64 / f64::from(1 << 20);
                    println!(
                        "{alloc:<10} {order:<8} {cache:>5}  {batch:>5}  {:>7.1}  {:>7.0}  {:>7.1}  {:>4.0}  {:>8.0}  {:>9.1}  {:>13.0}",
                        r.scan.as_secs_f64(),
                        items as f64 / r.scan.as_secs_f64(),
                        r.index.as_secs_f64(),
                        mib(r.bytes),
                        mib(r.free),
                        r.compact.as_secs_f64(),
                        mib(r.compacted),
                    );
                }
            }
        }
    }
}
