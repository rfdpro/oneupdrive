//! Rule suggestions (`oneup rules --suggest`, the settings window's
//! **Exclude** view): the largest folders in OneDrive and the folders that
//! changed most lately, as candidates to exclude. Suggestions only: nothing
//! is excluded unless someone chooses it, and excluding goes through the
//! same preview and rule change as unticking a folder in the file tree
//! (`crate::filetree::preview`, `crate::filetree::set_sync`).
//!
//! Bounded, never a pass over the tree (`oneup_store::choose`):
//! * **Largest**: the first [`LOOK`] rows of the cloud folders' size index
//!   (OneDrive gives a folder's size as all that's inside it).
//! * **Busiest**: the activity log's done entries of the last [`DAYS`]
//!   days (the log keeps at most `oneup_store::activity::KEEP` entries),
//!   counted for each folder above the item.
//!
//! A folder that a folder inside it accounts for most of isn't suggested:
//! the one inside is ([`pick`]). Folders the rules already exclude aren't
//! either.

use std::collections::HashMap;
use std::path::Path;

use anyhow::Result;
use oneup_rules::{ENGINE_DIR, RuleSet};
use oneup_store::{RemoteRow, Store};
use serde_json::{Value, json};

/// Cloud folders in the pair's folder looked at, largest first...
const LOOK: usize = 300;
/// ...out of at most this many rows of the drive-wide index.
const LOOK_MAX: usize = 20_000;

/// Whether cloud item `id` is strictly inside folder `root`, by its
/// parents; `known` remembers folders already decided on the way, so rows
/// in the same folders cost one lookup each.
fn is_inside(
    o: &oneup_store::Ops<'_>,
    id: &str,
    root: &str,
    known: &mut HashMap<String, bool>,
) -> oneup_store::Result<bool> {
    let mut path = Vec::new();
    let mut cur = o.get_item(id)?.and_then(|r| r.parent_id);
    let answer = loop {
        let Some(p) = cur else { break false };
        if p == root {
            break true;
        }
        if let Some(&k) = known.get(&p) {
            break k;
        }
        if path.len() > 4096 {
            break false;
        }
        cur = o.get_item(&p)?.and_then(|r| r.parent_id);
        path.push(p);
    };
    for p in path {
        known.insert(p, answer);
    }
    Ok(answer)
}
/// The window for the busiest folders.
pub const DAYS: i64 = 7;
/// A folder is left for one inside it holding at least this share (3/4).
const MOSTLY: (u64, u64) = (3, 4);

/// One suggested folder (relative to the synced folder) and its weight:
/// bytes for the largest, changes for the busiest.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Suggestion {
    pub rel: String,
    pub weight: u64,
}

#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Suggestions {
    pub largest: Vec<Suggestion>,
    pub busiest: Vec<Suggestion>,
}

fn folded_parts(rel: &str) -> Vec<String> {
    rel.split('/').filter(|p| !p.is_empty()).map(oneup_sync::fold).collect()
}

/// Reads rows a page at a time (`page(after the last row, at most n)`),
/// keeping what `keep` makes a suggestion of: never more than `look_max`
/// rows read (the last page asks only for what's left) nor more than
/// `look` kept (the rest of a page is left). Returns the kept and the
/// number of rows read.
fn read_bounded<R, E>(
    mut page: impl FnMut(Option<&R>, usize) -> Result<Vec<R>, E>,
    mut keep: impl FnMut(&R) -> Result<Option<Suggestion>, E>,
    look: usize,
    look_max: usize,
    page_size: usize,
) -> Result<(Vec<Suggestion>, usize), E> {
    let mut v = Vec::new();
    let mut read = 0;
    let mut last: Option<R> = None;
    while v.len() < look && read < look_max {
        let rows = page(last.as_ref(), page_size.min(look_max - read))?;
        let Some(end) = rows.len().checked_sub(1) else { break };
        read += rows.len();
        let mut rows = rows;
        last = rows.drain(end..).next();
        for r in rows.iter().chain(last.as_ref()) {
            if v.len() >= look {
                break;
            }
            if let Some(s) = keep(r)? {
                v.push(s);
            }
        }
    }
    Ok((v, read))
}

/// Whether `inner` is strictly inside `outer` (folded parts).
fn inside(inner: &[String], outer: &[String]) -> bool {
    inner.len() > outer.len() && inner.starts_with(outer)
}

/// Pure: up to `n` of `candidates`, heaviest first, leaving out a folder
/// that one inside it mostly accounts for, and any folder inside or above
/// one already picked.
pub fn pick(mut candidates: Vec<Suggestion>, n: usize) -> Vec<Suggestion> {
    candidates.sort_by(|a, b| b.weight.cmp(&a.weight).then_with(|| a.rel.cmp(&b.rel)));
    let parts: Vec<Vec<String>> = candidates.iter().map(|c| folded_parts(&c.rel)).collect();
    let mut taken: Vec<usize> = Vec::new();
    for (i, c) in candidates.iter().enumerate() {
        if taken.len() >= n {
            break;
        }
        let mostly_one_inside = candidates.iter().zip(&parts).any(|(d, dp)| {
            inside(dp, &parts[i]) && d.weight.saturating_mul(MOSTLY.1) >= c.weight.saturating_mul(MOSTLY.0)
        });
        let near = taken.iter().any(|&t| inside(&parts[i], &parts[t]) || inside(&parts[t], &parts[i]));
        if !mostly_one_inside && !near && !parts[i].is_empty() {
            taken.push(i);
        }
    }
    taken.into_iter().map(|i| candidates[i].clone()).collect()
}

/// Pure: changes per folder above each changed item (`/a/b/c.txt` counts
/// for `a` and `a/b`), names as first seen.
pub fn busiest_folders(paths: &[String]) -> Vec<Suggestion> {
    let mut counts: HashMap<Vec<String>, (String, u64)> = HashMap::new();
    for p in paths {
        let parts: Vec<&str> = p.split('/').filter(|x| !x.is_empty()).collect();
        for depth in 1..parts.len() {
            let rel = parts[..depth].join("/");
            counts.entry(folded_parts(&rel)).or_insert((rel, 0)).1 += 1;
        }
    }
    counts.into_values().map(|(rel, weight)| Suggestion { rel, weight }).collect()
}

/// Whether a suggestion for `rel` makes sense: not the engine's folder,
/// not already excluded.
fn worth(set: &RuleSet, rel: &str) -> bool {
    !rel.is_empty()
        && !rel.split('/').next().is_some_and(|f| oneup_sync::same_name(f, ENGINE_DIR))
        && !set.excludes(rel, true)
}

/// The suggestions for data folder `data_dir`, up to `n` of each.
pub fn suggest(data_dir: &Path, store: &Store, n: usize, now_ms: i64) -> Result<Suggestions> {
    let (set, _) = crate::rulefiles::load(data_dir, store, &mut crate::rulefiles::Cache::default())?;
    let remote = store.read(|o| o.meta_get(crate::sync::KEY_REMOTE))?.unwrap_or_default();
    let top = remote.trim_end_matches('/').to_string();
    // The pair's own OneDrive folder (`None`: the whole drive).
    let pair_root = if top.is_empty() { None } else { crate::sync::find_remote(store, &top)? };
    if !top.is_empty() && pair_root.is_none() {
        return Ok(Suggestions::default());
    }
    let largest: Vec<Suggestion> = store.read(|o| {
        // Pages of the size index, drive-wide, keeping the pair's: as far as
        // it takes to find [`LOOK`] of them, and at most [`LOOK_MAX`] rows.
        let mut inside: HashMap<String, bool> = HashMap::new();
        let page = |after: Option<&RemoteRow>, limit: usize| {
            o.remote_largest_folders_after(after.map(|r| (r.size, r.id.as_str())), limit)
        };
        let keep = |r: &RemoteRow| -> oneup_store::Result<Option<Suggestion>> {
            if let Some(root) = &pair_root
                && !is_inside(o, &r.id, root, &mut inside)?
            {
                return Ok(None);
            }
            let Some(path) = o.path_of(&r.id)? else { return Ok(None) };
            let rest = if top.is_empty() {
                Some(path.as_str())
            } else {
                oneup_sync::strip_folded_prefix(&path, &top).filter(|rest| rest.starts_with('/'))
            };
            let Some(rest) = rest else { return Ok(None) };
            let rel = crate::filetree::clean(rest);
            Ok(worth(&set, &rel).then(|| Suggestion { rel, weight: u64::try_from(r.size).unwrap_or(0) }))
        };
        Ok(read_bounded(page, keep, LOOK, LOOK_MAX, 256)?.0)
    })?;
    let since = now_ms - DAYS * 24 * 3_600_000;
    let paths = store.read(|o| o.activity_done_since(since, oneup_store::activity::KEEP as usize))?;
    let root = store.read(|o| o.local_root())?;
    let busy: Vec<Suggestion> = busiest_folders(&paths).into_iter().filter(|s| worth(&set, &s.rel)).collect();
    // Still here (activity names folders that may be gone since).
    let mut busiest = Vec::new();
    for s in pick(busy, n * 4) {
        let here = match &root {
            Some(r) => store.read(|o| crate::filetree::row_at(o, r.root_file_id, &s.rel))?.is_some(),
            None => false,
        };
        if here && busiest.len() < n {
            busiest.push(s);
        }
    }
    Ok(Suggestions { largest: pick(largest, n), busiest })
}

/// [`suggest`] as the window and `--json` show it.
pub fn to_json(s: &Suggestions) -> Value {
    let list = |v: &[Suggestion], key: &str| -> Vec<Value> {
        v.iter()
            .map(|s| json!({ "rel": s.rel, key: s.weight, "rule": crate::filetree::rule_for(&s.rel, true) }))
            .collect()
    };
    json!({ "largest": list(&s.largest, "bytes"), "busiest": list(&s.busiest, "changes"), "days": DAYS })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn s(rel: &str, weight: u64) -> Suggestion {
        Suggestion { rel: rel.into(), weight }
    }

    #[test]
    fn a_folder_mostly_made_of_one_inside_it_gives_way_to_that_one() {
        let got = pick(
            vec![
                s("Projects", 100),
                s("Projects/app", 95),
                s("Projects/app/node_modules", 90),
                s("Videos", 60),
                s("Videos/2019", 20),
                s("Music", 10),
            ],
            3,
        );
        assert_eq!(got, [s("Projects/app/node_modules", 90), s("Videos", 60), s("Music", 10)]);
    }

    #[test]
    fn nothing_inside_or_above_a_pick_and_names_compare_as_windows_does() {
        let got = pick(vec![s("A", 100), s("a/B", 30), s("a/c", 30)], 5);
        assert_eq!(got, [s("A", 100)]);
        assert!(pick(vec![s("", 5)], 5).is_empty(), "never the synced folder itself");
    }

    /// From the database alone: the largest cloud folders inside the pair's
    /// OneDrive folder that the rules don't exclude, and the folders whose
    /// items changed most in the window that are still here.
    #[test]
    #[allow(clippy::unwrap_used)]
    fn suggestions_come_from_the_size_index_and_the_activity_log() {
        use oneup_store::{ActivityEntry, ActivityOutcome, Kind, LocalKind, LocalRoot, LocalRow, RemoteRow, Skip};
        let d = std::env::temp_dir().join(format!("oneup-suggest-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&d);
        std::fs::create_dir_all(&d).unwrap();
        std::fs::write(crate::config::path(&d), "[rules]\nexclude = [\"/Old/\"]\n").unwrap();
        let mut store = Store::open_in_memory().unwrap();
        let folder = |id: &str, parent: &str, size| RemoteRow {
            id: id.into(),
            parent_id: Some(parent.into()),
            name: id.into(),
            kind: Kind::Folder,
            size,
            etag: None,
            qxh: None,
            mtime_ms: None,
        };
        let local = |id: u128, parent: u128, name: &str| LocalRow {
            file_id: id,
            parent_id: Some(parent),
            name: name.into(),
            name_lossy: false,
            kind: LocalKind::Folder,
            size: 0,
            mtime: 0,
            attributes: 0x10,
            reparse_tag: 0,
            skip: Skip::None,
        };
        let now = 100 * 24 * 3_600_000;
        let e = |at_ms, path: &str| ActivityEntry {
            seq: 0,
            at_ms,
            outcome: ActivityOutcome::Done,
            action: "upload".into(),
            path: path.into(),
            detail: String::new(),
        };
        store
            .write(|o| {
                o.meta_set(crate::sync::KEY_REMOTE, "/Sync")?;
                o.upsert_item(&RemoteRow { parent_id: None, kind: Kind::Root, ..folder("root", "", 0) }, false)?;
                o.upsert_item(&folder("Sync", "root", 1000), false)?;
                o.upsert_item(&folder("Elsewhere", "root", 5000), false)?;
                o.upsert_item(&folder("Videos", "Sync", 600), false)?;
                o.upsert_item(&folder("Old", "Sync", 300), false)?;
                o.upsert_item(&folder("Docs", "Sync", 50), false)?;
                let root = LocalRow { parent_id: None, kind: LocalKind::Root, ..local(1, 0, "") };
                let info = LocalRoot {
                    path: r"C:\Sync".into(),
                    volume_serial: 1,
                    root_file_id: 1,
                    rules: String::new(),
                    journal: None,
                    started_at_ms: 0,
                    finished_at_ms: Some(0),
                    sync_root_provider: None,
                };
                o.local_begin(&info, &root)?;
                o.local_put(&local(2, 1, "build"))?;
                o.local_put(&local(3, 2, "out"))?;
                o.local_put(&local(4, 1, "Docs"))?;
                o.activity_add(&[
                    e(0, "/Docs/ancient.txt"),
                    e(now - 1000, "/build/out/a.o"),
                    e(now - 1000, "/build/out/b.o"),
                    e(now - 1000, "/build/out/c.o"),
                    e(now - 1000, "/Docs/a.txt"),
                    e(now - 1000, "/gone/x.txt"),
                    e(now - 1000, "/gone/y.txt"),
                    e(now - 1000, "/gone/z.txt"),
                    e(now - 1000, "/gone/w.txt"),
                ])
            })
            .unwrap();
        let s = suggest(&d, &store, 5, now).unwrap();
        let rels = |v: &[Suggestion]| v.iter().map(|x| x.rel.clone()).collect::<Vec<_>>();
        assert_eq!(rels(&s.largest), ["Videos", "Docs"], "inside the pair, not excluded");
        assert_eq!(s.largest[0].weight, 600);
        assert_eq!(rels(&s.busiest), ["build/out", "Docs"], "gone folders and old changes left out");
        let j = to_json(&s);
        assert_eq!(j["largest"][0]["rule"], "/Videos/");
        let _ = std::fs::remove_dir_all(&d);
    }

    /// A small pair under a drive whose largest folders are all elsewhere
    /// still gets its own: the index is read past the drive's top rows.
    #[test]
    #[allow(clippy::unwrap_used)]
    fn a_small_pair_gets_its_own_largest_folders() {
        use oneup_store::{Kind, RemoteRow};
        let d = std::env::temp_dir().join(format!("oneup-suggest-small-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&d);
        std::fs::create_dir_all(&d).unwrap();
        let mut store = Store::open_in_memory().unwrap();
        let folder = |id: String, parent: &str, size| RemoteRow {
            id: id.clone(),
            parent_id: Some(parent.into()),
            name: id,
            kind: Kind::Folder,
            size,
            etag: None,
            qxh: None,
            mtime_ms: None,
        };
        store
            .write(|o| {
                o.meta_set(crate::sync::KEY_REMOTE, "/Small")?;
                o.upsert_item(&RemoteRow { parent_id: None, kind: Kind::Root, ..folder("root".into(), "", 0) }, false)?;
                o.upsert_item(&folder("Big".into(), "root", 1 << 40), false)?;
                for i in 0..(LOOK as i64 + 500) {
                    o.upsert_item(&folder(format!("big{i}"), "Big", 1_000_000 + i), false)?;
                }
                o.upsert_item(&folder("Small".into(), "root", 100), false)?;
                o.upsert_item(&folder("Photos".into(), "Small", 60), false)
            })
            .unwrap();
        let s = suggest(&d, &store, 5, 0).unwrap();
        assert_eq!(s.largest, [Suggestion { rel: "Photos".into(), weight: 60 }]);
        let _ = std::fs::remove_dir_all(&d);
    }

    /// Never more rows read than the bound (the last page asks for what's
    /// left), nor more kept than asked for (the rest of a page is left).
    #[test]
    fn reading_keeps_to_both_bounds() {
        let rows: Vec<u64> = (0..1_000).collect();
        let mut asked = Vec::new();
        let page = |after: Option<&u64>, n: usize| -> Result<Vec<u64>, ()> {
            asked.push(n);
            let from = after.map_or(0, |a| *a as usize + 1);
            Ok(rows.iter().copied().skip(from).take(n).collect())
        };
        // Keeps every other row: a page can bring more than what's left.
        let keep = |r: &u64| Ok(r.is_multiple_of(2).then(|| s(&format!("f{r}"), *r)));
        let (kept, read) = read_bounded(page, keep, 10_000, 600, 256).unwrap();
        assert_eq!(read, 600, "not 768");
        assert_eq!(asked, [256, 256, 88]);
        assert_eq!(kept.len(), 300);
        let keep = |r: &u64| Ok::<_, ()>(Some(s(&format!("f{r}"), *r)));
        let page = |after: Option<&u64>, n: usize| -> Result<Vec<u64>, ()> {
            let from = after.map_or(0, |a| *a as usize + 1);
            Ok(rows.iter().copied().skip(from).take(n).collect())
        };
        let (kept, read) = read_bounded(page, keep, 300, 20_000, 256).unwrap();
        assert_eq!((kept.len(), read), (300, 512), "300 kept, not 512");
    }

    #[test]
    fn changes_count_for_every_folder_above_the_item() {
        let mut got =
            busiest_folders(&["/a/b/x.txt".into(), "/A/b/y.txt".into(), "/a/z.txt".into(), "/top.txt".into()]);
        got.sort_by_key(|x| x.rel.to_lowercase());
        assert_eq!(got, [s("a", 3), s("a/b", 2)]);
    }
}
