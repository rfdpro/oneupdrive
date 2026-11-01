//! The name index at scale (`oneup_store::search`): what it adds per item
//! and how long a substring search takes. Slow, so not run by default:
//!
//! ```text
//! cargo test -p oneup-store --release --test search_scale -- --ignored --nocapture
//! ```
//!
//! `SEARCH_SCALE_ITEMS` sets the size (default 1,000,000); `SEARCH_SCALE_IDS=guid`
//! uses OneDrive's newer `…!s<32 hex digits>` IDs instead of numeric ones.
//! Items go in through the store's own writes, a thousand per transaction
//! as the mirror stores a page, so the triggers' cost is measured too.

#![allow(clippy::unwrap_used)]

use std::time::{Duration, Instant};

use oneup_store::{Kind, RemoteRow, Store};

/// Budgets the index must meet (`docs/architecture/search.md`).
const MAX_BYTES_PER_ITEM: f64 = 64.0;
const MAX_SEARCH: Duration = Duration::from_millis(100);

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

const WORDS: &[&str] = &[
    "report",
    "budget",
    "invoice",
    "meeting",
    "notes",
    "draft",
    "final",
    "summary",
    "plan",
    "project",
    "contract",
    "letter",
    "photo",
    "holiday",
    "family",
    "receipt",
    "tax",
    "statement",
    "bank",
    "manual",
    "presentation",
    "review",
    "agenda",
    "quarterly",
    "annual",
    "sales",
    "design",
    "schedule",
    "expenses",
    "travel",
    "school",
    "essay",
    "recipe",
    "house",
    "medical",
    "passport",
    "application",
    "mortgage",
    "salary",
    "client",
    "order",
    "analysis",
    "export",
    "backup",
    "archive",
    "copy",
    "version",
    "results",
    "template",
    "logo",
    "music",
    "Rechnung",
    "Steuer",
    "Vertrag",
    "facture",
    "informe",
    "Übersicht",
    "résumé",
];
const CODE: &[&str] = &[
    "index.js",
    "package.json",
    "README.md",
    "LICENSE",
    ".gitignore",
    "mod.rs",
    "lib.rs",
    "main.py",
    "__init__.py",
    "Makefile",
    "tsconfig.json",
    "Dockerfile",
    "style.css",
    "main.go",
    "App.java",
];
const EXT: &[&str] = &[".docx", ".pdf", ".xlsx", ".pptx", ".txt", ".csv", ".js", ".ts", ".py", ".rs"];

fn pick<'a>(r: &mut Rng, v: &[&'a str]) -> &'a str {
    v[r.below(v.len() as u64) as usize]
}

/// A name from a mixed drive: photos, code, documents, media, cache-like
/// names.
fn name(r: &mut Rng, folder: bool) -> String {
    if folder {
        return format!("{} {}", pick(r, WORDS), r.below(100));
    }
    match r.below(100) {
        0..25 => format!(
            "IMG_{}{:02}{:02}_{:06}.jpg",
            2010 + r.below(16),
            1 + r.below(12),
            1 + r.below(28),
            r.below(1_000_000)
        ),
        25..35 => pick(r, CODE).to_string(),
        35..50 => format!("{}_{}{}", pick(r, WORDS), r.below(10_000), pick(r, EXT)),
        50..75 => {
            let n = 1 + r.below(3);
            let words: Vec<&str> = (0..n).map(|_| pick(r, WORDS)).collect();
            format!("{} {}{}", words.join(" "), 2000 + r.below(26), pick(r, EXT))
        }
        75..88 => format!("{:02} {} - {}.mp3", 1 + r.below(20), pick(r, WORDS), pick(r, WORDS)),
        _ => format!("{:016x}{:08x}.bin", r.next(), r.next() as u32),
    }
}

#[test]
#[ignore = "slow: measures the index at a million items"]
fn the_index_meets_its_budgets_at_scale() {
    let n: usize = std::env::var("SEARCH_SCALE_ITEMS").ok().and_then(|v| v.parse().ok()).unwrap_or(1_000_000);
    let guid = std::env::var("SEARCH_SCALE_IDS").is_ok_and(|v| v == "guid");
    let dir = std::env::temp_dir().join(format!("oneup-search-scale-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).unwrap();
    let path = dir.join("state.db");
    let mut s = Store::open(&path, 64 * 1024).unwrap();
    // `SEARCH_SCALE_BASELINE=1`: without the index's triggers, to tell
    // what they add to storing items (and nothing else).
    let baseline = std::env::var("SEARCH_SCALE_BASELINE").is_ok_and(|v| v == "1");
    if baseline {
        rusqlite::Connection::open(&path)
            .unwrap()
            .execute_batch(
                "DROP TRIGGER search_remote_ins; DROP TRIGGER search_remote_name; DROP TRIGGER search_remote_del;",
            )
            .unwrap();
    }
    let mut r = Rng(0x9E37_79B9_7F4A_7C15);
    let drive = "a1b2c3d4e5f60708!";
    let id = |r: &mut Rng, k: usize| {
        if guid { format!("{drive}s{:016x}{:016x}", r.next(), r.next()) } else { format!("{drive}{}", 100 + k) }
    };
    let root = id(&mut r, 0);
    let row = |id: String, parent: Option<String>, name: String, kind: Kind| RemoteRow {
        id,
        parent_id: parent,
        name,
        kind,
        size: 1,
        etag: Some("{0123456789ABCDEF0123456789ABCDEF},1".into()),
        qxh: None,
        mtime_ms: Some(1_700_000_000_000),
    };
    s.write(|o| o.upsert_item(&row(root.clone(), None, "root".into(), Kind::Root), false)).unwrap();
    let started = Instant::now();
    let mut folders = vec![root];
    let mut made = 1;
    while made < n {
        s.write(|o| {
            for _ in 0..1000.min(n - made) {
                let parent = folders[folders.len() - 1 - r.below(folders.len().min(50) as u64) as usize].clone();
                let folder = r.below(100) < 8;
                let me = id(&mut r, made);
                let kind = if folder { Kind::Folder } else { Kind::File };
                o.upsert_item(&row(me.clone(), Some(parent), name(&mut r, folder), kind), false)?;
                if folder {
                    folders.push(me);
                }
                made += 1;
            }
            Ok(())
        })
        .unwrap();
    }
    let stored = started.elapsed();
    s.checkpoint().unwrap();
    drop(s);
    if baseline {
        println!(
            "{n} items stored without the index in {stored:.1?} ({:.1} µs each)",
            stored.as_micros() as f64 / n as f64
        );
        let _ = std::fs::remove_dir_all(&dir);
        return;
    }

    // Searches from a fresh connection with a small cache, as the settings
    // window and `oneup find` open one.
    let s = Store::open(&path, 4 * 1024).unwrap();
    let mut slowest = Duration::ZERO;
    for needle in [
        "report",
        "budget 20",
        "img_2023",
        "package.json",
        "zzq",
        "quarterly budget summary",
        "mortgage",
        "jpg",
        "Übersicht",
    ] {
        let t = Instant::now();
        let found = s.read(|o| o.search_names(needle, 50)).unwrap();
        let took = t.elapsed();
        slowest = slowest.max(took);
        let mark = if found.too_many {
            " (too many)"
        } else if found.more {
            "+"
        } else {
            ""
        };
        println!("  {needle:>26}: {:>2}{mark} found in {took:.1?}", found.hits.len());
    }
    drop(s);

    // What the index's tables take, page by page, as written and once
    // compacted, and the item number each row carries (the difference
    // removing it makes to the compacted table).
    let c = rusqlite::Connection::open(&path).unwrap();
    let pages = |sql: &str| -> f64 { c.query_row(sql, [], |r| r.get::<_, i64>(0)).unwrap() as f64 / n as f64 };
    const INDEX: &str = "SELECT sum(pgsize) FROM dbstat WHERE name LIKE 'remote_name%' OR name = 'search_queue'";
    const ITEMS: &str = "SELECT sum(pgsize) FROM dbstat WHERE name = 'remote_item'";
    let written = pages(INDEX);
    c.execute_batch("VACUUM;").unwrap();
    let (index, with) = (pages(INDEX), pages(ITEMS));
    c.execute_batch("UPDATE remote_item SET sk = NULL; VACUUM;").unwrap();
    let numbers = with - pages(ITEMS);
    drop(c);
    let per_item = written.max(index) + numbers;
    println!(
        "{n} items ({} IDs) stored in {stored:.1?} ({:.1} µs each); index {written:.1} B/item as written, \
         {index:.1} compacted; item numbers {numbers:.1} B/item; total {per_item:.1} B/item; slowest search {slowest:.1?}",
        if guid { "guid" } else { "numeric" },
        stored.as_micros() as f64 / n as f64,
    );
    // A store from before the index (no item numbers, as left above):
    // built in steps, as the agent does on its first start after an upgrade.
    let c = rusqlite::Connection::open(&path).unwrap();
    c.execute_batch(
        "INSERT INTO remote_name(remote_name) VALUES ('delete-all'); DELETE FROM meta WHERE key = 'search.built';",
    )
    .unwrap();
    drop(c);
    let mut s = Store::open(&path, 64 * 1024).unwrap();
    let t = Instant::now();
    let built = s.search_build(oneup_store::search::BUILD_STEP, |_| true).unwrap();
    println!("built from scratch in steps: {built} items in {:.1?}", t.elapsed());
    assert_eq!(s.read(|o| o.search_names("report", 1)).unwrap().hits.len(), 1);
    drop(s);
    let _ = std::fs::remove_dir_all(&dir);
    assert!(slowest < MAX_SEARCH, "slowest search {slowest:?}");
    assert!(per_item <= MAX_BYTES_PER_ITEM, "{per_item:.1} B/item");
}
