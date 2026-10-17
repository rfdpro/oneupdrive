//! When failed operations are tried again (schema v17).
//!
//! A failed operation keeps its folder marked (`dirty`), so it isn't
//! forgotten; without a record of when to try it again, every round (any
//! change in the folder, every timed check) ran it again at once. Each
//! failure now records when its item may be tried next: [`FIRST_MS`] after
//! the first failure, twice as long after each further one in a row, at most
//! [`MAX_MS`].
//!
//! The record names the operation as it was planned (`version`, which covers
//! the item's local stamp and cloud eTag). An operation planned differently
//! (the file changed on either side) isn't held by it, and its first failure
//! starts over at [`FIRST_MS`]. Success removes the record.
//!
//! Bounded by the items failing: records go on success, or [`STALE_MS`] after
//! their time if the item was never tried again (it went away). Every query
//! is by key or by the `until_ms` index, never a scan.

use oneup_sync::{LocalId, RemoteId};
use rusqlite::OptionalExtension as _;

use crate::{Ops, Result};

/// The wait after the first failure.
pub const FIRST_MS: i64 = 60_000;
/// The longest wait.
pub const MAX_MS: i64 = 3_600_000;
/// How long after its time a record is kept for an item not tried again.
pub const STALE_MS: i64 = 24 * 3_600_000;
/// The most of an error message a record keeps (for `oneup explain`).
const MAX_ERROR: usize = 500;

pub(crate) const MIGRATE_V17: &str = "
CREATE TABLE retry (
    item     TEXT PRIMARY KEY,
    version  TEXT NOT NULL,
    failures INTEGER NOT NULL,
    until_ms INTEGER NOT NULL,
    error    TEXT NOT NULL
) WITHOUT ROWID;
CREATE INDEX retry_until ON retry(until_ms);
";

/// The item an operation acts on: a file or folder on this PC, or in OneDrive.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Item<'a> {
    Local(LocalId),
    Remote(&'a RemoteId),
}

impl Item<'_> {
    fn key(self) -> String {
        match self {
            Item::Local(id) => format!("l:{:032x}", id.0),
            Item::Remote(id) => format!("r:{}", id.0),
        }
    }
}

/// An item's failures in a row.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Retry {
    /// The operation that failed, as planned.
    pub version: String,
    pub failures: u32,
    /// When it may be tried again (Unix ms).
    pub until_ms: i64,
    /// Why it last failed.
    pub error: String,
}

impl Retry {
    /// Whether operation `version` waits at `now_ms`: it's the one that
    /// failed, and its time hasn't come.
    pub fn holds(&self, version: &str, now_ms: i64) -> bool {
        self.version == version && self.until_ms > now_ms
    }
}

/// The wait after `failures` failures in a row: [`FIRST_MS`], doubling, at
/// most [`MAX_MS`].
pub fn wait_ms(failures: u32) -> i64 {
    let doublings = failures.saturating_sub(1).min(16);
    (FIRST_MS << doublings).min(MAX_MS)
}

fn bounded(error: &str) -> &str {
    if error.len() <= MAX_ERROR {
        return error;
    }
    let mut end = MAX_ERROR;
    while !error.is_char_boundary(end) {
        end -= 1;
    }
    &error[..end]
}

impl Ops<'_> {
    pub fn retry_get(&self, item: Item<'_>) -> Result<Option<Retry>> {
        Ok(self
            .c
            .prepare_cached("SELECT version, failures, until_ms, error FROM retry WHERE item = ?1")?
            .query_row([item.key()], |r| {
                Ok(Retry {
                    version: r.get(0)?,
                    failures: u32::try_from(r.get::<_, i64>(1)?).unwrap_or(u32::MAX),
                    until_ms: r.get(2)?,
                    error: r.get(3)?,
                })
            })
            .optional()?)
    }

    /// Records that `item`'s operation `version` failed at `now_ms` with
    /// `error`, and returns when it may be tried again. Another failure of
    /// the same operation doubles the wait; a different operation starts
    /// over.
    pub fn retry_failed(&self, item: Item<'_>, version: &str, error: &str, now_ms: i64) -> Result<Retry> {
        let failures = match self.retry_get(item)? {
            Some(r) if r.version == version => r.failures.saturating_add(1),
            _ => 1,
        };
        let r = Retry {
            version: version.to_string(),
            failures,
            until_ms: now_ms.saturating_add(wait_ms(failures)),
            error: bounded(error).to_string(),
        };
        self.c
            .prepare_cached(
                "INSERT INTO retry(item, version, failures, until_ms, error) VALUES (?1, ?2, ?3, ?4, ?5)
                 ON CONFLICT(item) DO UPDATE SET version = ?2, failures = ?3, until_ms = ?4, error = ?5",
            )?
            .execute(rusqlite::params![item.key(), r.version, i64::from(r.failures), r.until_ms, r.error])?;
        Ok(r)
    }

    /// `item` synced: its failures are over.
    pub fn retry_clear(&self, item: Item<'_>) -> Result<()> {
        self.c.prepare_cached("DELETE FROM retry WHERE item = ?1")?.execute([item.key()])?;
        Ok(())
    }

    /// Whether any item has failures recorded (read through the index).
    pub fn retry_any(&self) -> Result<bool> {
        let first: Option<i64> =
            self.c.prepare_cached("SELECT min(until_ms) FROM retry")?.query_row([], |r| r.get(0))?;
        Ok(first.is_some())
    }

    /// The items still waiting at `now_ms`: how many, and when the first one
    /// may be tried again.
    pub fn retry_waiting(&self, now_ms: i64) -> Result<(u64, Option<i64>)> {
        let (n, first): (i64, Option<i64>) = self
            .c
            .prepare_cached("SELECT count(*), min(until_ms) FROM retry WHERE until_ms > ?1")?
            .query_row([now_ms], |r| Ok((r.get(0)?, r.get(1)?)))?;
        Ok((u64::try_from(n).unwrap_or(0), first))
    }

    /// Forgets the items whose time came more than [`STALE_MS`] before
    /// `now_ms` and that weren't tried since (they went away, or the agent
    /// was off: the next failure starts over). Returns how many.
    pub fn retry_expire(&self, now_ms: i64) -> Result<usize> {
        Ok(self
            .c
            .prepare_cached("DELETE FROM retry WHERE until_ms < ?1")?
            .execute([now_ms.saturating_sub(STALE_MS)])?)
    }
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used)]
    use super::*;
    use crate::Store;

    #[test]
    fn the_wait_doubles_from_a_minute_to_an_hour() {
        let waits: Vec<i64> = (1..=9).map(|n| wait_ms(n) / 60_000).collect();
        assert_eq!(waits, [1, 2, 4, 8, 16, 32, 60, 60, 60]);
        assert_eq!(wait_ms(u32::MAX), MAX_MS);
    }

    #[test]
    fn failures_in_a_row_wait_longer_and_success_or_a_change_starts_over() {
        let mut s = Store::open_in_memory().unwrap();
        let file = Item::Local(LocalId(7));
        let r = s.write(|o| o.retry_failed(file, "upload v1", "HTTP 500", 1_000)).unwrap();
        assert_eq!((r.failures, r.until_ms), (1, 61_000));
        assert!(r.holds("upload v1", 60_999) && !r.holds("upload v1", 61_000), "until its time");
        assert!(!r.holds("upload v2", 2_000), "the file changed: not held");
        let r = s.write(|o| o.retry_failed(file, "upload v1", "HTTP 500", 61_000)).unwrap();
        assert_eq!((r.failures, r.until_ms), (2, 181_000), "twice as long");
        // The file changed (a new stamp or eTag): its first failure again.
        let r = s.write(|o| o.retry_failed(file, "upload v2", "HTTP 500", 200_000)).unwrap();
        assert_eq!((r.failures, r.until_ms), (1, 260_000));
        assert_eq!(s.read(|o| o.retry_get(file)).unwrap(), Some(r));
        // Another item is unaffected, and success removes the record.
        let other = RemoteId("r!1".into());
        assert_eq!(s.read(|o| o.retry_get(Item::Remote(&other))).unwrap(), None);
        s.write(|o| o.retry_clear(file)).unwrap();
        assert_eq!(s.read(|o| o.retry_get(file)).unwrap(), None);
        assert!(!s.read(|o| o.retry_any()).unwrap());
    }

    #[test]
    fn waiting_items_are_counted_and_stale_ones_forgotten() {
        let mut s = Store::open_in_memory().unwrap();
        let (a, b) = (RemoteId("a".into()), RemoteId("b".into()));
        s.write(|o| {
            o.retry_failed(Item::Remote(&a), "download", "x", 0)?;
            o.retry_failed(Item::Local(LocalId(1)), "upload", &"é".repeat(400), 30_000)
        })
        .unwrap();
        assert_eq!(s.read(|o| o.retry_waiting(1_000)).unwrap(), (2, Some(60_000)));
        assert_eq!(s.read(|o| o.retry_waiting(70_000)).unwrap(), (1, Some(90_000)), "a's time came");
        let kept = s.read(|o| o.retry_get(Item::Local(LocalId(1)))).unwrap().unwrap().error;
        assert!(kept.len() <= MAX_ERROR && kept.starts_with('é'), "bounded, on a character boundary");
        // b never failed; a was never tried again and goes a day after its time.
        assert_eq!(s.write(|o| o.retry_expire(60_000 + STALE_MS)).unwrap(), 0);
        assert_eq!(s.write(|o| o.retry_expire(60_001 + STALE_MS)).unwrap(), 1);
        assert_eq!(s.read(|o| o.retry_get(Item::Remote(&a))).unwrap(), None);
        assert_eq!(s.read(|o| o.retry_get(Item::Remote(&b))).unwrap(), None);
        assert!(s.read(|o| o.retry_any()).unwrap());
    }

    /// Kept across restarts: it's in the database.
    #[test]
    fn the_wait_survives_a_restart() {
        let path = std::env::temp_dir().join(format!("oneup-store-retry-{}.db", std::process::id()));
        let _ = std::fs::remove_file(&path);
        let mut s = Store::open(&path, 1024).unwrap();
        s.write(|o| o.retry_failed(Item::Local(LocalId(9)), "v", "x", 5)).unwrap();
        drop(s);
        let s = Store::open(&path, 1024).unwrap();
        assert_eq!(s.read(|o| o.retry_get(Item::Local(LocalId(9)))).unwrap().map(|r| r.until_ms), Some(60_005));
        drop(s);
        for ext in ["", "-wal", "-shm"] {
            let _ = std::fs::remove_file(format!("{}{ext}", path.display()));
        }
    }

    /// Every query reads by key or through the `until_ms` index, so a round
    /// costs the same however many items failed.
    #[cfg(feature = "statement-log")]
    #[test]
    fn no_query_scans_the_table() {
        let mut s = Store::open_in_memory().unwrap();
        let sql = [
            "SELECT version, failures, until_ms, error FROM retry WHERE item = ?1",
            "SELECT min(until_ms) FROM retry",
            "SELECT count(*), min(until_ms) FROM retry WHERE until_ms > ?1",
            "DELETE FROM retry WHERE until_ms < ?1",
            "DELETE FROM retry WHERE item = ?1",
        ];
        let scans = s.tree_scans(&sql.map(String::from)).unwrap();
        assert!(scans.is_empty(), "{scans:?}");
        // The guard sees this table.
        assert_eq!(s.tree_scans(&["SELECT count(*) FROM retry".to_string()]).unwrap().len(), 1);
        s.write(|o| o.retry_clear(Item::Local(LocalId(1)))).unwrap();
    }
}
