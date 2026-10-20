//! Automatic space policies (`config.toml` `[space]`, `oneup space`, the
//! settings window's Files On-Demand page): with Files On-Demand on, the
//! agent frees files by itself, as Windows' Storage Sense does for the
//! OneDrive app:
//!
//! * **files not used for N days** become online-only;
//! * **when the disk runs low** (less free than `low_disk_percent`), the
//!   files unused for longest (at least a day) are freed until it has 5
//!   points more free again.
//!
//! "Used" is the latest of: the file's last write, its last access as
//! Windows records it (where the volume keeps access times), and the last
//! time OneUpDrive downloaded it for a program (the `local_used` table).
//! Pinned files ("Always keep on this device") are never freed, and
//! freeing is the same [`Op::Dehydrate`] as "Free up space": through the
//! gate and the effects layer, which frees a file only if its content
//! provably is OneDrive's (`data-safety.md` invariant 11). Each pass frees a
//! bounded batch and looks at a bounded number of files, continuing where
//! the last pass stopped.

use std::path::Path;

use anyhow::Result;
use oneup_store::{LocalRow, Store};
use oneup_sync::{LocalId, LocalPre, Op};

use crate::config::Space;

/// At most this many files freed per pass...
pub const BATCH: usize = 64;
/// ...and looked at.
const LOOK: usize = 2_000;
const DAY_MS: i64 = 24 * 3_600_000;
/// Where the last pass stopped (`mtime file_id`), so files skipped as
/// recently used aren't looked at again and again.
const KEY_CURSOR: &str = "space.cursor";

/// Why files are freed in a pass.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Why {
    Unused { days: u32 },
    LowDisk,
}

/// What a pass would free.
#[derive(Debug, Default)]
pub struct Selection {
    pub ops: Vec<Op>,
    pub bytes: u64,
    pub why: Option<Why>,
}

/// The disk the synced folder is on.
#[derive(Debug, Clone, Copy)]
pub struct Disk {
    pub free: u64,
    pub total: u64,
}

/// Unix milliseconds → FILETIME (100 ns ticks since 1601).
pub fn filetime(unix_ms: i64) -> i64 {
    (unix_ms + 11_644_473_600_000).saturating_mul(10_000)
}

/// FILETIME → Unix milliseconds.
pub fn unix_ms(filetime: i64) -> i64 {
    filetime / 10_000 - 11_644_473_600_000
}

/// What this pass should free, if anything. `accessed` gives a file's last
/// access time (Unix ms) from its path relative to the synced folder;
/// `kept` says whether a pin rule keeps it on this PC (`crate::pinrules`),
/// which wins over every policy here, even before the agent has set the
/// file's pin.
pub fn select(
    store: &mut Store,
    s: &Space,
    disk: Option<Disk>,
    now_ms: i64,
    accessed: &dyn Fn(&str) -> Option<i64>,
    kept: &dyn Fn(&str) -> bool,
) -> Result<Selection> {
    let low = disk.filter(|_| s.free_when_low).and_then(|d| {
        let want = d.total / 100 * u64::from(s.low_disk_percent);
        (d.free < want).then(|| d.total / 100 * u64::from(s.low_disk_percent + 5) - d.free)
    });
    let (cutoff, why, need) = match (low, s.free_unused_days) {
        // Low disk: anything unused for a day will do, oldest first.
        (Some(need), _) => (now_ms - DAY_MS, Why::LowDisk, Some(need)),
        (None, 0) => return Ok(Selection::default()),
        (None, days) => (now_ms - i64::from(days) * DAY_MS, Why::Unused { days }, None),
    };

    let mut after = store.read(|o| o.meta_get(KEY_CURSOR))?.and_then(|c| {
        let (m, id) = c.split_once(' ')?;
        Some((m.parse().ok()?, id.parse().ok()?))
    });
    let mut out = Selection { why: Some(why), ..Selection::default() };
    let mut looked = 0;
    let mut wrapped = after.is_none();
    'pages: loop {
        let page = store.read(|o| o.local_by_age(filetime(cutoff), after, 256))?;
        if page.is_empty() {
            if wrapped {
                after = None;
                break;
            }
            // Start over from the oldest once.
            wrapped = true;
            after = None;
            continue;
        }
        for r in page {
            looked += 1;
            after = Some((r.mtime, r.file_id));
            if let Some(op) = freeable(store, &r, cutoff, accessed, kept)? {
                out.bytes += u64::try_from(r.size).unwrap_or(0);
                out.ops.push(op);
            }
            let enough = match need {
                Some(n) => out.bytes >= n,
                None => false,
            };
            if enough || out.ops.len() >= BATCH || looked >= LOOK {
                break 'pages;
            }
        }
    }
    store.write(|o| match after {
        Some((m, id)) => o.meta_set(KEY_CURSOR, &format!("{m} {id}")),
        None => o.meta_delete(KEY_CURSOR),
    })?;
    Ok(out)
}

/// The operation freeing `r`, if it hasn't been used since `cutoff` and
/// its content's hash is on record (the effects layer checks it).
fn freeable(
    store: &Store,
    r: &LocalRow,
    cutoff: i64,
    accessed: &dyn Fn(&str) -> Option<i64>,
    kept: &dyn Fn(&str) -> bool,
) -> Result<Option<Op>> {
    if unix_ms(r.mtime) >= cutoff {
        return Ok(None);
    }
    let (used, hash, path) = store.read(|o| {
        Ok((o.local_used_get(r.file_id)?, o.local_hash_get(r.file_id, r.size, r.mtime)?, o.local_path_of(r.file_id)?))
    })?;
    if used.is_some_and(|u| u >= cutoff) {
        return Ok(None);
    }
    let Some(path) = path else { return Ok(None) };
    if kept(&path) {
        return Ok(None);
    }
    if accessed(&path).is_some_and(|a| a >= cutoff) {
        return Ok(None);
    }
    Ok(hash
        .map(|hash| Op::Dehydrate { local: LocalPre { id: LocalId(r.file_id), stamp: Some((r.size, r.mtime)) }, hash }))
}

/// A file's last access time as Windows records it, for [`select`].
pub fn last_access(root: &Path, rel: &str) -> Option<i64> {
    let p = rel.trim_start_matches('/').split('/').fold(root.to_path_buf(), |p, x| p.join(x));
    let t = std::fs::metadata(p).ok()?.accessed().ok()?;
    let ms = t.duration_since(std::time::UNIX_EPOCH).ok()?.as_millis();
    i64::try_from(ms).ok()
}

/// One line for the activity log and `oneup status`.
pub fn describe(sel: &Selection) -> String {
    let size = crate::versions::size(sel.bytes);
    match sel.why {
        Some(Why::Unused { days }) => {
            format!("{} files ({size}) not used for {days} days made online-only", sel.ops.len())
        }
        Some(Why::LowDisk) => format!("disk low: {} files ({size}) unused longest made online-only", sel.ops.len()),
        None => String::new(),
    }
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used)]
    use super::*;
    use oneup_store::{LocalKind, Skip};

    const NOW: i64 = 1_800_000_000_000;

    fn file(id: u128, days_old: i64) -> LocalRow {
        LocalRow {
            file_id: id,
            parent_id: Some(1),
            name: format!("f{id}"),
            name_lossy: false,
            kind: LocalKind::File,
            size: 1000,
            mtime: filetime(NOW - days_old * DAY_MS),
            attributes: 1024,
            reparse_tag: 0,
            skip: Skip::None,
        }
    }

    fn store(files: &[LocalRow]) -> Store {
        let mut s = Store::open_in_memory().unwrap();
        s.write(|o| {
            let root = LocalRow { kind: LocalKind::Root, parent_id: None, name: "r".into(), ..file(1, 0) };
            o.local_put(&root)?;
            for f in files {
                o.local_put(f)?;
                o.local_hash_put(f.file_id, f.size, f.mtime, &[7; 20])?;
            }
            Ok(())
        })
        .unwrap();
        s
    }

    fn freed(sel: &Selection) -> Vec<u128> {
        let mut v: Vec<u128> = sel
            .ops
            .iter()
            .filter_map(|o| if let Op::Dehydrate { local, .. } = o { Some(local.id.0) } else { None })
            .collect();
        v.sort();
        v
    }

    #[test]
    fn files_unused_for_long_enough_are_freed_unless_used_since() {
        let mut s = store(&[file(10, 40), file(11, 5), file(12, 60), file(13, 90)]);
        // 13 was opened through OneUpDrive yesterday; 12 read by a program.
        s.write(|o| o.local_used_set(13, NOW - DAY_MS)).unwrap();
        let accessed = |p: &str| (p == "/f12").then_some(NOW - 2 * DAY_MS);
        let space = Space { free_unused_days: 30, ..Space::default() };
        let sel = select(&mut s, &space, None, NOW, &accessed, &|_| false).unwrap();
        assert_eq!(freed(&sel), vec![10]);
        assert_eq!(sel.why, Some(Why::Unused { days: 30 }));
    }

    #[test]
    fn nothing_happens_unless_asked() {
        let mut s = store(&[file(10, 400)]);
        let sel =
            select(&mut s, &Space::default(), Some(Disk { free: 1, total: 1000 }), NOW, &|_| None, &|_| false).unwrap();
        assert!(sel.ops.is_empty());
    }

    #[test]
    fn a_low_disk_frees_oldest_first_until_there_is_room() {
        let mut s = store(&[file(10, 3), file(11, 10), file(12, 2), file(13, 0)]);
        let space = Space { free_when_low: true, low_disk_percent: 10, ..Space::default() };
        // 1 MB disk, 50 KB free: 100 KB is the line, 150 KB the goal.
        let disk = Disk { free: 50_000, total: 1_000_000 };
        let sel = select(&mut s, &space, Some(disk), NOW, &|_| None, &|_| false).unwrap();
        // Each file is 1 KB, so all three old enough go (13 is from today).
        assert_eq!(freed(&sel), vec![10, 11, 12]);
        assert_eq!(sel.why, Some(Why::LowDisk));
        // With room enough, nothing.
        let fine = Disk { free: 500_000, total: 1_000_000 };
        assert!(select(&mut s, &space, Some(fine), NOW, &|_| None, &|_| false).unwrap().ops.is_empty());
    }

    /// A pin rule wins over every space policy: what it matches is never
    /// chosen, though the disk is low and the file unused for a year, and
    /// though the agent hasn't set its pin yet (the index only leaves out
    /// files already pinned).
    #[test]
    fn a_pin_rule_keeps_files_whatever_the_policy() {
        let mut s = store(&[file(10, 400), file(11, 400), file(12, 400)]);
        let pins = oneup_rules::PinSet::new(&["/f11", "*12"]);
        let kept = |p: &str| pins.pins(p, false);
        let space = Space { free_unused_days: 1, free_when_low: true, low_disk_percent: 10 };
        let disk = Disk { free: 1, total: 1_000_000 };
        let sel = select(&mut s, &space, Some(disk), NOW, &|_| None, &kept).unwrap();
        assert_eq!(freed(&sel), vec![10]);
    }
}
