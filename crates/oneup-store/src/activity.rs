//! The activity log (schema v6): what sync did, item by item, so the user
//! can see what happened to a file and when (`data-safety.md` § Recovery, §7
//! "activity log").
//!
//! Bounded: only the latest [`KEEP`] entries are kept; older ones are
//! dropped as new ones arrive, by sequence number (cheap: the primary key).
//! Entries hold paths relative to the synced folders, which the database
//! already holds anyway; they never hold file contents.

use rusqlite::OptionalExtension as _;

use crate::{Ops, Result};

/// How many entries are kept.
pub const KEEP: i64 = 50_000;

/// How an entry ended.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Outcome {
    Done,
    Skipped,
    Failed,
    /// Waiting: for the user (a safety stop, a full version store) or a pause.
    Waiting,
    /// Something that happened to sync itself (trimming, a decision).
    Note,
}

impl Outcome {
    fn code(self) -> i64 {
        match self {
            Outcome::Done => 0,
            Outcome::Skipped => 1,
            Outcome::Failed => 2,
            Outcome::Waiting => 3,
            Outcome::Note => 4,
        }
    }

    fn from_code(c: i64) -> Self {
        match c {
            0 => Outcome::Done,
            1 => Outcome::Skipped,
            2 => Outcome::Failed,
            3 => Outcome::Waiting,
            _ => Outcome::Note,
        }
    }
}

/// One entry.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Entry {
    /// Assigned by the store (0 when adding).
    pub seq: i64,
    pub at_ms: i64,
    pub outcome: Outcome,
    /// What was done (`upload`, `download`, `recycle`, `safety stop`, …).
    pub action: String,
    /// The item, relative to the synced folders (`/docs/a.txt`), or empty.
    pub path: String,
    /// Why, or what else to know.
    pub detail: String,
}

pub(crate) const MIGRATE_V6: &str = "
CREATE TABLE activity (
    seq     INTEGER PRIMARY KEY AUTOINCREMENT,
    at_ms   INTEGER NOT NULL,
    outcome INTEGER NOT NULL,
    action  TEXT NOT NULL,
    path    TEXT NOT NULL COLLATE NOCASE,
    detail  TEXT NOT NULL
);
CREATE INDEX activity_path ON activity(path, seq);
";

fn entry(r: &rusqlite::Row<'_>) -> rusqlite::Result<Entry> {
    Ok(Entry {
        seq: r.get(0)?,
        at_ms: r.get(1)?,
        outcome: Outcome::from_code(r.get(2)?),
        action: r.get(3)?,
        path: r.get(4)?,
        detail: r.get(5)?,
    })
}

const COLS: &str = "seq, at_ms, outcome, action, path, detail";

impl Ops<'_> {
    /// Adds entries (in order), then drops the oldest beyond [`KEEP`].
    pub fn activity_add(&self, entries: &[Entry]) -> Result<()> {
        if entries.is_empty() {
            return Ok(());
        }
        let mut ins = self
            .c
            .prepare_cached("INSERT INTO activity(at_ms, outcome, action, path, detail) VALUES (?1, ?2, ?3, ?4, ?5)")?;
        for e in entries {
            ins.execute(rusqlite::params![e.at_ms, e.outcome.code(), e.action, e.path, e.detail])?;
        }
        let last: Option<i64> =
            self.c.query_row("SELECT max(seq) FROM activity", [], |r| r.get(0)).optional()?.flatten();
        if let Some(last) = last {
            // An entry a pending sharing change will settle stays: its
            // outcome must have somewhere to go.
            self.c
                .prepare_cached("DELETE FROM activity WHERE seq <= ?1 AND seq NOT IN (SELECT seq FROM sharing_intent)")?
                .execute([last - KEEP])?;
        }
        Ok(())
    }

    /// Adds one entry for something about to be done, before it's done, and
    /// returns its sequence number for [`Ops::activity_settle`]: if the
    /// process dies in between, the record of the attempt stays.
    pub fn activity_begin(&self, e: &Entry) -> Result<i64> {
        self.activity_add(std::slice::from_ref(e))?;
        Ok(self.c.query_row("SELECT max(seq) FROM activity", [], |r| r.get(0))?)
    }

    /// How an entry [`Ops::activity_begin`] added ended. Returns whether
    /// the entry was there to settle.
    pub fn activity_settle(&self, seq: i64, outcome: Outcome, detail: &str) -> Result<bool> {
        let n = self
            .c
            .prepare_cached("UPDATE activity SET outcome = ?2, detail = ?3 WHERE seq = ?1")?
            .execute(rusqlite::params![seq, outcome.code(), detail])?;
        Ok(n == 1)
    }

    /// The latest `limit` entries, newest first; only those for `path` (the
    /// item itself, or anything inside it if it's a folder) if given. Paths
    /// compare case-insensitively (ASCII), as Windows names do.
    pub fn activity_recent(&self, path: Option<&str>, limit: usize) -> Result<Vec<Entry>> {
        let limit = i64::try_from(limit).unwrap_or(i64::MAX);
        let rows = match path {
            None => self
                .c
                .prepare_cached(&format!("SELECT {COLS} FROM activity ORDER BY seq DESC LIMIT ?1"))?
                .query_map([limit], entry)?
                .collect::<rusqlite::Result<Vec<_>>>()?,
            Some(p) => {
                let p = p.trim_end_matches('/');
                // `path` itself, or under `path/` (a range on the index; `0`
                // is the character after `/`).
                self.c
                    .prepare_cached(&format!(
                        "SELECT {COLS} FROM activity
                         WHERE path = ?1 OR (path >= ?1 || '/' AND path < ?1 || '0')
                         ORDER BY seq DESC LIMIT ?2"
                    ))?
                    .query_map(rusqlite::params![p, limit], entry)?
                    .collect::<rusqlite::Result<Vec<_>>>()?
            }
        };
        Ok(rows)
    }

    /// The latest `limit` entries that did `action` (done, not skipped or
    /// failed) at `since_ms` or later, newest first; with `before`, only
    /// those older than that sequence number (the next page). Reads the
    /// table from the newest entry back (no index on the action): for a
    /// command someone runs, not for anything that runs by itself.
    pub fn activity_done(&self, action: &str, since_ms: i64, before: Option<i64>, limit: usize) -> Result<Vec<Entry>> {
        let limit = i64::try_from(limit).unwrap_or(i64::MAX);
        Ok(self
            .c
            .prepare_cached(&format!(
                "SELECT {COLS} FROM activity WHERE action = ?1 AND outcome = ?2 AND at_ms >= ?3 AND seq < ?4
                 ORDER BY seq DESC LIMIT ?5"
            ))?
            .query_map(
                rusqlite::params![action, Outcome::Done.code(), since_ms, before.unwrap_or(i64::MAX), limit],
                entry,
            )?
            .collect::<rusqlite::Result<Vec<_>>>()?)
    }
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used)]
    use super::*;
    use crate::Store;

    fn e(at: i64, path: &str) -> Entry {
        Entry {
            seq: 0,
            at_ms: at,
            outcome: Outcome::Done,
            action: "upload".into(),
            path: path.into(),
            detail: String::new(),
        }
    }

    #[test]
    fn entries_come_back_newest_first_and_by_path() {
        let mut s = Store::open_in_memory().unwrap();
        s.write(|o| o.activity_add(&[e(1, "/docs/a.txt"), e(2, "/docs2/b.txt"), e(3, "/docs"), e(4, "/x.txt")]))
            .unwrap();
        let all = s.read(|o| o.activity_recent(None, 10)).unwrap();
        assert_eq!(all.iter().map(|e| e.at_ms).collect::<Vec<_>>(), [4, 3, 2, 1]);
        let docs = s.read(|o| o.activity_recent(Some("/docs/"), 10)).unwrap();
        // The folder and what's inside it, not a sibling that shares the prefix.
        assert_eq!(docs.iter().map(|e| e.path.as_str()).collect::<Vec<_>>(), ["/docs", "/docs/a.txt"]);
        // Names compare case-insensitively, as on Windows.
        assert_eq!(s.read(|o| o.activity_recent(Some("/DOCS"), 10)).unwrap().len(), 2);
        assert_eq!(s.read(|o| o.activity_recent(None, 1)).unwrap().len(), 1);
    }

    #[test]
    fn done_entries_of_one_action_since_a_time() {
        let mut s = Store::open_in_memory().unwrap();
        let keep = |at, outcome| Entry { action: "keep both".into(), outcome, ..e(at, "/a.txt") };
        s.write(|o| {
            o.activity_add(&[keep(1, Outcome::Done), e(2, "/b"), keep(3, Outcome::Failed), keep(4, Outcome::Done)])
        })
        .unwrap();
        let got = s.read(|o| o.activity_done("keep both", 0, None, 10)).unwrap();
        assert_eq!(got.iter().map(|e| e.at_ms).collect::<Vec<_>>(), [4, 1]);
        assert_eq!(s.read(|o| o.activity_done("keep both", 2, None, 10)).unwrap().len(), 1);
        let first = s.read(|o| o.activity_done("keep both", 0, None, 1)).unwrap();
        assert_eq!(first[0].at_ms, 4);
        // The next page starts after the last entry of this one.
        let next = s.read(|o| o.activity_done("keep both", 0, Some(first[0].seq), 1)).unwrap();
        assert_eq!(next[0].at_ms, 1);
        assert!(s.read(|o| o.activity_done("keep both", 0, Some(next[0].seq), 1)).unwrap().is_empty());
    }

    #[test]
    fn an_entry_begun_first_is_settled_in_place() {
        let mut s = Store::open_in_memory().unwrap();
        let seq = s.write(|o| o.activity_begin(&Entry { outcome: Outcome::Waiting, ..e(1, "/a") })).unwrap();
        s.write(|o| o.activity_add(&[e(2, "/b")])).unwrap();
        assert_eq!(s.read(|o| o.activity_recent(Some("/a"), 1)).unwrap()[0].outcome, Outcome::Waiting);
        s.write(|o| o.activity_settle(seq, Outcome::Done, "done")).unwrap();
        let a = &s.read(|o| o.activity_recent(Some("/a"), 1)).unwrap()[0];
        assert_eq!((a.outcome, a.detail.as_str(), a.at_ms), (Outcome::Done, "done", 1));
    }

    #[test]
    fn only_the_latest_entries_are_kept() {
        let mut s = Store::open_in_memory().unwrap();
        let many: Vec<Entry> = (0..KEEP + 10).map(|i| e(i, "/f")).collect();
        s.write(|o| o.activity_add(&many)).unwrap();
        let n: i64 = s.read(|o| Ok(o.c.query_row("SELECT count(*) FROM activity", [], |r| r.get(0))?)).unwrap();
        assert_eq!(n, KEEP);
        assert_eq!(s.read(|o| o.activity_recent(None, 1)).unwrap()[0].at_ms, KEEP + 9);
    }
}
