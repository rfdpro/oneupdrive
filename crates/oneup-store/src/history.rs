//! The Synced tree's history (schema v23): for each change to a Synced row,
//! what the row agreed on until then, and when. It's what a rewind
//! reconstructs a folder's past from, and what lists items deleted from
//! OneDrive by ID (the OneDrive Personal recycle bin can't be listed
//! through documented Graph; `POST /items/{id}/restore` takes an ID).
//!
//! Written by triggers on `synced_item`, as the dirty marks are, so every
//! writer is covered by construction, inside the same transaction. A row
//! holds the item's agreed place and content before the change (its
//! folder, name, kind, size, hash and OneDrive ID), not its local stamps or
//! eTag, which change without anything the user would rewind. An update
//! that changes none of those columns writes nothing.
//!
//! Bounded ([`Ops::history_prune`]): rows older than a number of days go,
//! and the oldest beyond a number of rows. What's covered starts at
//! [`Ops::history_since`]: the schema's upgrade, or the newest row pruned.
//! Cost grows with changes, never with the size of the tree.

use oneup_sync::rewind::{History, Was};
use oneup_sync::{RemoteId, Sid};
use rusqlite::OptionalExtension as _;

use crate::synced::{hash_of, node_kind, sid_in, sid_out};
use crate::{Error, Ops, Result};

/// Now, in Unix ms, as SQLite tells it (millisecond resolution).
const NOW_MS: &str = "CAST((julianday('now') - 2440587.5) * 86400000.0 AS INTEGER)";

/// Where coverage starts (Unix ms, as text).
pub const SINCE: &str = "history.since";

/// Set once the history is kept ([`Ops::history_start`]).
pub const ON: &str = "history.on";

/// What a history row records about its change.
const CREATED: i64 = 0;
const CHANGED: i64 = 1;
const REMOVED: i64 = 2;

/// v23: the history table, its indexes and its triggers, the indexes on
/// Synced names and the item marks. A name with letters SQLite's `NOCASE`
/// doesn't fold (anything but ASCII) also keeps its folded form
/// (`synced_item.fold`, written with the row by `Ops::synced_put`, filled
/// here once with the store's own `oneup_fold`, a query, not part of the
/// schema), indexed by folder, so a lookup by name never reads a folder's
/// other children. It touches only `synced_item`,
/// `local_item`, `remote_item` and `meta`, all there since v9, so it runs
/// on v19 as on any version after it. It doesn't start the history: an
/// upgrade can come in the middle of a first sync, whose records are of
/// items that were there before it, not created then. Every folder's is
/// started when a sequence of rounds next settles; a new one from the end of its
/// first sync ([`Ops::history_start`]): the records a first sync makes
/// are of items that were there before it, not created then, and a large
/// folder's would fill the history at once.
pub(crate) fn migrate_v23() -> String {
    let on = format!("EXISTS (SELECT 1 FROM meta WHERE key = '{ON}')");
    let unsettled = unsettled_triggers(&on);
    format!(
        "
CREATE TABLE synced_history (
    seq        INTEGER PRIMARY KEY AUTOINCREMENT,
    at_ms      INTEGER NOT NULL,
    sid        INTEGER NOT NULL,
    change     INTEGER NOT NULL,
    parent_sid INTEGER,
    name       TEXT,
    kind       INTEGER,
    size       INTEGER,
    qxh        BLOB,
    remote_id  TEXT
);
CREATE INDEX synced_history_sid ON synced_history(sid, seq);
CREATE INDEX synced_history_parent ON synced_history(parent_sid, at_ms) WHERE parent_sid IS NOT NULL;
CREATE INDEX synced_history_at ON synced_history(at_ms);
CREATE INDEX synced_history_remote ON synced_history(remote_id) WHERE remote_id IS NOT NULL;
CREATE INDEX synced_item_name ON synced_item(parent_sid, name COLLATE NOCASE);
ALTER TABLE synced_item ADD COLUMN fold TEXT;
UPDATE synced_item SET fold = oneup_fold(name) WHERE name GLOB '*[^ -~]*';
CREATE INDEX synced_item_fold ON synced_item(parent_sid, fold) WHERE fold IS NOT NULL;
CREATE TABLE synced_unsettled (sid INTEGER PRIMARY KEY);
{unsettled}
CREATE TRIGGER synced_history_ins AFTER INSERT ON synced_item WHEN {on}
BEGIN
    INSERT INTO synced_history(at_ms, sid, change) VALUES ({NOW_MS}, NEW.sid, {CREATED});
END;
CREATE TRIGGER synced_history_upd AFTER UPDATE ON synced_item
WHEN (OLD.parent_sid IS NOT NEW.parent_sid OR OLD.name IS NOT NEW.name OR OLD.kind IS NOT NEW.kind
    OR OLD.size IS NOT NEW.size OR OLD.qxh IS NOT NEW.qxh OR OLD.remote_id IS NOT NEW.remote_id) AND {on}
BEGIN
    INSERT INTO synced_history(at_ms, sid, change, parent_sid, name, kind, size, qxh, remote_id)
    VALUES ({NOW_MS}, OLD.sid, {CHANGED}, OLD.parent_sid, OLD.name, OLD.kind, OLD.size, OLD.qxh, OLD.remote_id);
END;
CREATE TRIGGER synced_history_del AFTER DELETE ON synced_item WHEN {on}
BEGIN
    INSERT INTO synced_history(at_ms, sid, change, parent_sid, name, kind, size, qxh, remote_id)
    VALUES ({NOW_MS}, OLD.sid, {REMOVED}, OLD.parent_sid, OLD.name, OLD.kind, OLD.size, OLD.qxh, OLD.remote_id);
END;
"
    )
}

/// Whether record `s` (parent `p`) differs from its items `l` and `r`:
/// gone or unlinked, another folder or name, a file's size or last-write
/// time (here) or eTag (in OneDrive). More rather than fewer: one that
/// doesn't differ is only read for nothing.
const DIFFERS: &str = "(
        l.file_id IS NULL OR l.skip != 0 OR l.name_lossy != 0
        OR l.parent_id IS NOT p.local_id OR l.name IS NOT s.name
        OR (s.kind = 0 AND (l.size IS NOT s.size OR l.mtime IS NOT s.local_mtime))
        OR r.id IS NULL OR r.parent_id IS NOT p.remote_id OR r.name IS NOT s.name
        OR (s.kind = 0 AND r.etag IS NOT s.remote_etag))";

/// The marked records and their items, by mark.
const MARKED: &str = "FROM synced_unsettled u
    JOIN synced_item s ON s.sid = u.sid
    LEFT JOIN synced_item p ON p.sid = s.parent_sid
    LEFT JOIN local_item l ON l.file_id = s.local_id
    LEFT JOIN remote_item r ON r.id = s.remote_id";

/// Item marks for what may differ from its record (`synced_unsettled`):
/// a local or cloud item a record links that appears, changes or goes,
/// and a record whose links or stamps change. Each is one probe of the
/// unique index on the link. What a rewind reads for changes not synced
/// yet ([`Ops::history_unsettled`]); a mark goes once its item agrees with
/// its record again ([`Ops::history_settle_marks`]), and any later change
/// marks it again. Kept only while the history is (`on`).
fn unsettled_triggers(on: &str) -> String {
    let local_changed = "OLD.parent_id IS NOT NEW.parent_id OR OLD.name IS NOT NEW.name
        OR OLD.name_lossy IS NOT NEW.name_lossy OR OLD.kind IS NOT NEW.kind OR OLD.size IS NOT NEW.size
        OR OLD.mtime IS NOT NEW.mtime OR OLD.skip IS NOT NEW.skip";
    let remote_changed = "OLD.parent_id IS NOT NEW.parent_id OR OLD.name IS NOT NEW.name OR OLD.kind IS NOT NEW.kind
        OR OLD.size IS NOT NEW.size OR OLD.etag IS NOT NEW.etag OR OLD.qxh IS NOT NEW.qxh";
    let synced_changed = "OLD.local_id IS NOT NEW.local_id OR OLD.remote_id IS NOT NEW.remote_id
        OR OLD.local_mtime IS NOT NEW.local_mtime OR OLD.remote_etag IS NOT NEW.remote_etag";
    // Not `OR IGNORE`: an outer upsert's conflict handling would override
    // it (see `dirty::mark`).
    let mark = |by: &str| {
        format!(
            "INSERT INTO synced_unsettled(sid) SELECT s.sid FROM synced_item s WHERE s.{by}
             AND NOT EXISTS (SELECT 1 FROM synced_unsettled u WHERE u.sid = s.sid);"
        )
    };
    let t = |name: &str, event: &str, table: &str, when: &str, body: String| {
        format!("CREATE TRIGGER {name} AFTER {event} ON {table} WHEN ({when}) AND {on}\nBEGIN\n    {body}\nEND;\n")
    };
    [
        t("unsettled_local_ins", "INSERT", "local_item", "1", mark("local_id = NEW.file_id")),
        t("unsettled_local_upd", "UPDATE", "local_item", local_changed, mark("local_id = NEW.file_id")),
        t("unsettled_local_del", "DELETE", "local_item", "1", mark("local_id = OLD.file_id")),
        t("unsettled_remote_ins", "INSERT", "remote_item", "1", mark("remote_id = NEW.id")),
        t("unsettled_remote_upd", "UPDATE", "remote_item", remote_changed, mark("remote_id = NEW.id")),
        t("unsettled_remote_del", "DELETE", "remote_item", "1", mark("remote_id = OLD.id")),
        t(
            "unsettled_synced_upd",
            "UPDATE",
            "synced_item",
            synced_changed,
            "INSERT INTO synced_unsettled(sid) SELECT NEW.sid
             WHERE NOT EXISTS (SELECT 1 FROM synced_unsettled WHERE sid = NEW.sid);"
                .into(),
        ),
    ]
    .concat()
}

/// Undoes [`migrate_v23`], for tests that make an older store.
#[cfg(test)]
pub(crate) const UNDO_V23: &str = "DROP TRIGGER synced_history_ins; DROP TRIGGER synced_history_upd;
    DROP TRIGGER synced_history_del; DROP TABLE synced_history; DROP INDEX synced_item_name;
    DROP INDEX synced_item_fold; ALTER TABLE synced_item DROP COLUMN fold;
    DROP TRIGGER unsettled_local_ins; DROP TRIGGER unsettled_local_upd; DROP TRIGGER unsettled_local_del;
    DROP TRIGGER unsettled_remote_ins; DROP TRIGGER unsettled_remote_upd; DROP TRIGGER unsettled_remote_del;
    DROP TRIGGER unsettled_synced_upd; DROP TABLE synced_unsettled;";

/// An item whose record went, with its OneDrive ID: deleted (or set aside)
/// at `at_ms`, as it was until then.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Removed {
    pub seq: i64,
    pub at_ms: i64,
    pub sid: Sid,
    pub was: Was,
}

/// What [`Ops::history_prune`] keeps.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Keep {
    /// Rows older than this many days go (0: none for age).
    pub days: u32,
    /// At most this many rows stay (0: no limit).
    pub rows: u64,
}

impl Default for Keep {
    fn default() -> Self {
        Keep { days: 30, rows: 1_000_000 }
    }
}

fn was_of(r: &rusqlite::Row<'_>, at: usize) -> rusqlite::Result<Option<Raw>> {
    let kind: Option<i64> = r.get(at + 3)?;
    let Some(kind) = kind else { return Ok(None) };
    Ok(Some((
        r.get(at)?,
        r.get(at + 1)?,
        r.get::<_, Option<String>>(at + 2)?.unwrap_or_default(),
        kind,
        r.get::<_, Option<i64>>(at + 4)?.unwrap_or(0),
        r.get(at + 5)?,
        r.get(at + 6)?,
    )))
}

type Raw = (i64, Option<i64>, String, i64, i64, Option<Vec<u8>>, Option<String>);

fn to_was((_, parent, name, kind, size, qxh, remote): Raw) -> Result<Was> {
    Ok(Was {
        parent: parent.map(sid_out),
        name,
        kind: node_kind(kind)?,
        size,
        hash: hash_of(qxh),
        remote: remote.map(RemoteId),
    })
}

impl Ops<'_> {
    /// Where the history's coverage starts (Unix ms): a rewind can't go
    /// further back. `i64::MAX`: none kept yet.
    pub fn history_since(&self) -> Result<i64> {
        Ok(self.meta_get(SINCE)?.and_then(|v| v.parse().ok()).unwrap_or(i64::MAX))
    }

    /// Starts keeping the history, at `now_ms`, if it isn't kept yet: once a
    /// first sync has settled. Returns whether it started now.
    pub fn history_start(&self, now_ms: i64) -> Result<bool> {
        if self.meta_get(ON)?.is_some() {
            return Ok(false);
        }
        self.meta_set(ON, "1")?;
        self.meta_set(SINCE, &now_ms.to_string())?;
        Ok(true)
    }

    /// See [`History::change_after`].
    pub fn history_change_after(&self, sid: Sid, at: i64) -> Result<Option<Option<Was>>> {
        let row = self
            .c
            .prepare_cached(
                "SELECT change, parent_sid, name, kind, size, qxh, remote_id FROM synced_history
                 WHERE sid = ?1 AND at_ms > ?2 ORDER BY seq LIMIT 1",
            )?
            .query_row(rusqlite::params![sid_in(sid), at], |r| {
                let change: i64 = r.get(0)?;
                Ok((change, was_of(r, 0)?))
            })
            .optional()?;
        match row {
            None => Ok(None),
            Some((CREATED, _)) => Ok(Some(None)),
            Some((_, Some(raw))) => Ok(Some(Some(to_was(raw)?))),
            Some((c, None)) => Err(Error::Corrupt(format!("history row ({c}) without a kind"))),
        }
    }

    /// See [`History::changed_after`]: through the index on the time, so
    /// it reads the rows since the moment only.
    pub fn history_changed_after(&self, at: i64, limit: usize) -> Result<Option<Vec<Sid>>> {
        let over = i64::try_from(limit.saturating_add(1)).unwrap_or(i64::MAX);
        let mut st = self.c.prepare_cached(
            "SELECT DISTINCT sid FROM synced_history INDEXED BY synced_history_at WHERE at_ms > ?1 LIMIT ?2",
        )?;
        let rows = st.query_map(rusqlite::params![at, over], |r| r.get::<_, i64>(0))?;
        let out = rows.map(|r| Ok(sid_out(r?))).collect::<Result<Vec<_>>>()?;
        Ok((out.len() <= limit).then_some(out))
    }

    /// See [`History::unsettled`]: the marked records whose item differs
    /// from them now, read by mark (item-level, through the marks' key and
    /// the links' unique indexes), however large their folders.
    pub fn history_unsettled(&self, limit: usize) -> Result<Option<Vec<Sid>>> {
        let over = i64::try_from(limit.saturating_add(1)).unwrap_or(i64::MAX);
        let mut st = self.c.prepare_cached(&format!("SELECT s.sid {MARKED} WHERE {DIFFERS} LIMIT ?1"))?;
        let out = st.query_map([over], |r| r.get::<_, i64>(0))?.map(|r| Ok(sid_out(r?))).collect::<Result<Vec<_>>>()?;
        Ok((out.len() <= limit).then_some(out))
    }

    /// Drops at most `budget` marks of records whose items agree with them
    /// again, or that went (the history has those): each round, so the
    /// marks stay as few as what waits to sync. A change after this marks
    /// the record again.
    pub fn history_settle_marks(&self, budget: usize) -> Result<usize> {
        let budget = i64::try_from(budget).unwrap_or(i64::MAX);
        Ok(self
            .c
            .prepare_cached(&format!(
                "DELETE FROM synced_unsettled WHERE sid IN (
                SELECT u.sid FROM synced_unsettled u WHERE NOT EXISTS (SELECT 1 FROM synced_item WHERE sid = u.sid)
                UNION ALL
                SELECT u.sid {MARKED} WHERE NOT {DIFFERS}
                LIMIT ?1)"
            ))?
            .execute([budget])?)
    }

    /// See [`History::children_after`].
    pub fn history_children_after(&self, parent: Sid, at: i64) -> Result<Vec<Sid>> {
        let mut st =
            self.c.prepare_cached("SELECT DISTINCT sid FROM synced_history WHERE parent_sid = ?1 AND at_ms > ?2")?;
        let rows = st.query_map(rusqlite::params![sid_in(parent), at], |r| r.get::<_, i64>(0))?;
        rows.map(|r| Ok(sid_out(r?))).collect()
    }

    /// Records removed with a OneDrive ID that OneDrive no longer lists,
    /// newest first, at most `limit`, those before `before` (a `seq`, for
    /// the next page) if given: items deleted from OneDrive, as far as the
    /// history reaches.
    pub fn history_removed(&self, limit: usize, before: Option<i64>) -> Result<Vec<Removed>> {
        let limit = i64::try_from(limit).unwrap_or(i64::MAX);
        let mut st = self.c.prepare_cached(
            "SELECT h.seq, h.at_ms, h.sid, h.change, h.parent_sid, h.name, h.kind, h.size, h.qxh, h.remote_id
             FROM synced_history h
             WHERE h.change = 2 AND h.remote_id IS NOT NULL AND h.seq < ?1
               AND NOT EXISTS (SELECT 1 FROM remote_item r WHERE r.id = h.remote_id)
             ORDER BY h.seq DESC LIMIT ?2",
        )?;
        let rows = st.query_map(rusqlite::params![before.unwrap_or(i64::MAX), limit], |r| {
            Ok((r.get::<_, i64>(0)?, r.get::<_, i64>(1)?, r.get::<_, i64>(2)?, was_of(r, 3)?))
        })?;
        let mut out = Vec::new();
        for row in rows {
            let (seq, at_ms, sid, raw) = row?;
            let Some(raw) = raw else { continue };
            out.push(Removed { seq, at_ms, sid: sid_out(sid), was: to_was(raw)? });
        }
        Ok(out)
    }

    /// Drops rows older than `keep.days` before `now_ms`, then the oldest
    /// beyond `keep.rows`, and moves the coverage start up to the newest row
    /// dropped. Returns how many went. Cheap when there's nothing to drop:
    /// two index probes.
    pub fn history_prune(&self, keep: Keep, now_ms: i64) -> Result<usize> {
        self.history_prune_some(keep, now_ms, None, usize::MAX)
    }

    /// [`Ops::history_prune`], dropping at most `budget` rows (so each call
    /// costs a bounded amount, whatever came before), and none written
    /// after `needed_after`: a rewind under way to that moment reads the
    /// first change after it of every item.
    pub fn history_prune_some(
        &self,
        keep: Keep,
        now_ms: i64,
        needed_after: Option<i64>,
        budget: usize,
    ) -> Result<usize> {
        let mut upto: Option<i64> = None;
        if keep.days > 0 {
            let cutoff = now_ms.saturating_sub(i64::from(keep.days).saturating_mul(86_400_000));
            let seq: Option<i64> = self
                .c
                .prepare_cached("SELECT max(seq) FROM synced_history WHERE at_ms < ?1")?
                .query_row([cutoff], |r| r.get(0))?;
            upto = seq;
        }
        if keep.rows > 0 {
            let last: Option<i64> =
                self.c.prepare_cached("SELECT max(seq) FROM synced_history")?.query_row([], |r| r.get(0))?;
            let rows = i64::try_from(keep.rows).unwrap_or(i64::MAX);
            if let Some(last) = last
                && last > rows
            {
                upto = Some(upto.unwrap_or(0).max(last - rows));
            }
        }
        let Some(mut upto) = upto else { return Ok(0) };
        if let Some(t) = needed_after {
            let before: Option<i64> = self
                .c
                .prepare_cached("SELECT min(seq) FROM synced_history WHERE at_ms > ?1")?
                .query_row([t], |r| r.get(0))?;
            if let Some(first_needed) = before {
                upto = upto.min(first_needed - 1);
            }
        }
        let first: Option<i64> =
            self.c.prepare_cached("SELECT min(seq) FROM synced_history")?.query_row([], |r| r.get(0))?;
        let Some(first) = first else { return Ok(0) };
        let budget = i64::try_from(budget).unwrap_or(i64::MAX).max(1);
        upto = upto.min(first.saturating_add(budget - 1));
        if upto < first {
            return Ok(0);
        }
        let newest: Option<i64> = self
            .c
            .prepare_cached("SELECT max(at_ms) FROM synced_history WHERE seq <= ?1")?
            .query_row([upto], |r| r.get(0))?;
        let n = self.c.prepare_cached("DELETE FROM synced_history WHERE seq <= ?1")?.execute([upto])?;
        if let Some(at) = newest {
            let since = self.history_since()?;
            if since == i64::MAX || at > since {
                self.meta_set(SINCE, &at.to_string())?;
            }
        }
        Ok(n)
    }

    /// Replaces the whole history with `rows` (when, which item, what it
    /// was until then; `None`: created then), oldest first. For the
    /// simulation, which keeps a history of its own and checks that the
    /// store reads it back the same way.
    #[doc(hidden)]
    pub fn history_replace(&self, rows: &[(i64, Sid, Option<Was>)]) -> Result<()> {
        self.c.execute("DELETE FROM synced_history", [])?;
        let mut st = self.c.prepare_cached(
            "INSERT INTO synced_history(at_ms, sid, change, parent_sid, name, kind, size, qxh, remote_id)
             VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9)",
        )?;
        for (at, sid, was) in rows {
            match was {
                None => st.execute(rusqlite::params![
                    at,
                    sid_in(*sid),
                    CREATED,
                    None::<i64>,
                    None::<String>,
                    None::<i64>,
                    None::<i64>,
                    None::<Vec<u8>>,
                    None::<String>
                ])?,
                Some(w) => st.execute(rusqlite::params![
                    at,
                    sid_in(*sid),
                    CHANGED,
                    w.parent.map(sid_in),
                    w.name,
                    crate::synced::kind_code(w.kind),
                    w.size,
                    w.hash.map(|h| h.to_vec()),
                    w.remote.as_ref().map(|r| r.0.as_str()),
                ])?,
            };
        }
        self.meta_set(ON, "1")?;
        self.meta_set(SINCE, "0")
    }

    /// Stops keeping the history and drops it (rewinding turned off).
    pub fn history_stop(&self) -> Result<()> {
        if self.meta_get(ON)?.is_none() && self.meta_get(SINCE)?.is_none() {
            return Ok(());
        }
        self.c.execute("DELETE FROM synced_history", [])?;
        self.c.execute("DELETE FROM synced_unsettled", [])?;
        self.meta_delete(ON)?;
        self.meta_delete(SINCE)
    }

    /// How many rows the history holds.
    pub fn history_rows(&self) -> Result<i64> {
        Ok(self.c.query_row("SELECT count(*) FROM synced_history", [], |r| r.get(0))?)
    }
}

impl History for Ops<'_> {
    type Error = Error;

    fn change_after(&self, sid: Sid, at: i64) -> Result<Option<Option<Was>>> {
        self.history_change_after(sid, at)
    }

    fn children_after(&self, parent: Sid, at: i64) -> Result<Vec<Sid>> {
        self.history_children_after(parent, at)
    }

    fn changed_after(&self, at: i64, limit: usize) -> Result<Option<Vec<Sid>>> {
        self.history_changed_after(at, limit)
    }

    fn unsettled(&self, limit: usize) -> Result<Option<Vec<Sid>>> {
        self.history_unsettled(limit)
    }

    fn last_seen(&self, id: &RemoteId) -> Result<Option<Was>> {
        let row = self
            .c
            .prepare_cached(
                "SELECT change, parent_sid, name, kind, size, qxh, remote_id FROM synced_history
                 WHERE remote_id = ?1 ORDER BY seq DESC LIMIT 1",
            )?
            .query_row([id.0.as_str()], |r| was_of(r, 0))
            .optional()?
            .flatten();
        row.map(to_was).transpose()
    }
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used)]
    use oneup_sync::{Base, LocalId, NodeKind, ROOT};

    use super::*;
    use crate::Store;

    fn base(sid: Sid, parent: Sid, name: &str, size: i64) -> Base {
        Base {
            sid,
            parent: Some(parent),
            name: name.into(),
            kind: NodeKind::File,
            size,
            hash: Some([size as u8; 20]),
            local: Some(LocalId(u128::from(sid) + 100)),
            local_mtime: 1,
            remote: Some(RemoteId(format!("r{sid}"))),
            remote_etag: "e1".into(),
        }
    }

    fn root() -> Base {
        Base {
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
        }
    }

    /// A store at v19, or at v20 to v22 (the versions landing before this
    /// one), with a synced folder whose first sync may not have finished,
    /// comes up to v23 without starting the history: a record made by that
    /// sync after the upgrade isn't taken as created since. Once a sequence
    /// of rounds settles it starts, and from then on a change is recorded,
    /// an item that changes is marked, and a child is found by name through
    /// the new index.
    #[test]
    fn a_store_from_before_is_upgraded_without_starting_the_history() {
        for from in [19, 20, 21, 22] {
            upgraded_from(from);
        }
    }

    fn upgraded_from(from: i64) {
        let dir = std::env::temp_dir().join(format!("oneup-store-history-up{from}-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("state.db");
        {
            let mut s = Store::open(&path, 1024).unwrap();
            s.write(|o| {
                o.synced_put(&root())?;
                o.synced_put(&base(1, ROOT, "a.txt", 3))
            })
            .unwrap();
            let undo = format!("{UNDO_V23} DELETE FROM meta; PRAGMA user_version = {from};");
            s.read(|o| Ok(o.c.execute_batch(&undo)?)).unwrap();
        }
        let mut s = Store::open(&path, 1024).unwrap();
        let v: i64 = s.read(|o| Ok(o.c.query_row("PRAGMA user_version", [], |r| r.get(0))?)).unwrap();
        assert_eq!(v, 23, "from v{from}");
        assert_eq!(s.read(|o| o.history_since()).unwrap(), i64::MAX, "from v{from}: not started by the upgrade");
        // The first sync goes on: what it records isn't history.
        s.write(|o| o.synced_put(&base(2, ROOT, "found later.txt", 1))).unwrap();
        assert_eq!(s.read(|o| o.history_rows()).unwrap(), 0, "from v{from}");
        // The rounds settle: kept from then on.
        assert!(s.write(|o| o.history_start(100)).unwrap());
        s.write(|o| o.synced_put(&base(1, ROOT, "a.txt", 4))).unwrap();
        assert_eq!(s.read(|o| o.history_rows()).unwrap(), 1, "from v{from}: a change is recorded");
        let named = s.read(|o| oneup_sync::Trees::base_named(o, ROOT, "A.TXT")).unwrap();
        assert_eq!(named.iter().map(|b| b.sid).collect::<Vec<_>>(), [1], "found by name");
        s.write(|o| {
            o.upsert_item(
                &crate::RemoteRow {
                    id: "r1".into(),
                    parent_id: Some("root".into()),
                    name: "a.txt".into(),
                    kind: crate::Kind::File,
                    size: 9,
                    etag: Some("e2".into()),
                    qxh: None,
                    mtime_ms: None,
                },
                false,
            )
            .map(drop)
        })
        .unwrap();
        let marked: Vec<i64> = s
            .read(|o| {
                Ok(o.c
                    .prepare("SELECT sid FROM synced_unsettled")?
                    .query_map([], |r| r.get(0))?
                    .collect::<rusqlite::Result<_>>()?)
            })
            .unwrap();
        assert_eq!(marked, [1], "an item that changes is marked");
        drop(s);
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// Moves every row's time to `at`, so tests can tell moments apart.
    fn stamp(s: &mut Store, at: i64) {
        s.write(|o| Ok(o.c.execute("UPDATE synced_history SET at_ms = ?1 WHERE at_ms > 1000000000", [at])?)).unwrap();
    }

    #[test]
    fn each_change_keeps_what_the_item_was_until_then() {
        let mut s = Store::open_in_memory().unwrap();
        assert_eq!(s.read(|o| o.history_since()).unwrap(), i64::MAX, "nothing kept before a first sync");
        s.write(|o| o.synced_put(&root())).unwrap();
        assert_eq!(s.read(|o| o.history_rows()).unwrap(), 0, "a first sync writes no history");
        assert!(s.write(|o| o.history_start(50)).unwrap());
        assert!(!s.write(|o| o.history_start(60)).unwrap(), "started once");
        assert_eq!(s.read(|o| o.history_since()).unwrap(), 50);
        s.write(|o| o.synced_put(&base(1, ROOT, "a.txt", 3))).unwrap();
        stamp(&mut s, 100);
        // Only the local stamp and the eTag: nothing to rewind.
        s.write(|o| o.synced_put(&Base { local_mtime: 9, remote_etag: "e2".into(), ..base(1, ROOT, "a.txt", 3) }))
            .unwrap();
        assert_eq!(s.read(|o| o.history_rows()).unwrap(), 1);
        s.write(|o| o.synced_put(&base(1, ROOT, "b.txt", 5))).unwrap();
        stamp(&mut s, 200);
        s.write(|o| o.synced_remove(1).map(|_| ())).unwrap();
        stamp(&mut s, 300);

        let at = |t| s.read(|o| o.history_change_after(1, t)).unwrap();
        assert_eq!(at(50), Some(None), "created at 100");
        assert_eq!(at(150).unwrap().unwrap().name, "a.txt");
        let w = at(250).unwrap().unwrap();
        assert_eq!((w.name.as_str(), w.size), ("b.txt", 5));
        assert_eq!(at(300), None, "nothing after");
        assert_eq!(s.read(|o| o.history_children_after(ROOT, 150)).unwrap(), vec![1]);
        assert!(s.read(|o| o.history_children_after(ROOT, 300)).unwrap().is_empty());
        // Gone from OneDrive too: listed as deleted.
        let removed = s.read(|o| o.history_removed(10, None)).unwrap();
        assert_eq!(removed.len(), 1);
        assert_eq!((removed[0].sid, removed[0].at_ms, removed[0].was.name.as_str()), (1, 300, "b.txt"));
    }

    #[test]
    fn pruning_drops_the_oldest_and_moves_the_start_up() {
        let mut s = Store::open_in_memory().unwrap();
        s.write(|o| {
            o.history_start(0)?;
            o.synced_put(&root())
        })
        .unwrap();
        for i in 1..=10 {
            s.write(|o| o.synced_put(&base(i, ROOT, "f", 1))).unwrap();
            let t = i64::try_from(i).unwrap() * 1000;
            stamp(&mut s, t);
        }
        // By count: the newest 4 stay.
        let gone = s.write(|o| o.history_prune(Keep { days: 0, rows: 4 }, 0)).unwrap();
        assert_eq!(gone, 7);
        assert_eq!(s.read(|o| o.history_since()).unwrap(), 6000);
        // By age: everything older than a day before now.
        let gone = s.write(|o| o.history_prune(Keep { days: 1, rows: 0 }, 86_400_000 + 8500)).unwrap();
        assert_eq!(gone, 2);
        assert_eq!(s.read(|o| o.history_since()).unwrap(), 8000);
        assert_eq!(s.write(|o| o.history_prune(Keep { days: 1, rows: 0 }, 0)).unwrap(), 0);
    }

    #[test]
    fn pruning_is_bounded_and_keeps_what_a_rewind_under_way_reads() {
        let mut s = Store::open_in_memory().unwrap();
        s.write(|o| {
            o.history_start(0)?;
            o.synced_put(&root())
        })
        .unwrap();
        for i in 1..=20 {
            s.write(|o| o.synced_put(&base(i, ROOT, "f", 1))).unwrap();
            stamp(&mut s, i64::try_from(i).unwrap() * 1000);
        }
        let all = s.read(|o| o.history_rows()).unwrap();
        // Over its limit of 2 rows: each call drops at most 5.
        let keep = Keep { days: 0, rows: 2 };
        assert_eq!(s.write(|o| o.history_prune_some(keep, 0, None, 5)).unwrap(), 5);
        assert_eq!(s.read(|o| o.history_rows()).unwrap(), all - 5);
        // A rewind to 9000 under way: the rows after it stay, whatever the limit.
        while s.write(|o| o.history_prune_some(keep, 0, Some(9000), 5)).unwrap() > 0 {}
        let left = s.read(|o| o.history_rows()).unwrap();
        assert_eq!(left, 11, "rows after 9000 kept: 10..=20");
        assert_eq!(s.read(|o| o.history_change_after(10, 9000)).unwrap(), Some(None), "still known as created then");
        // Done with it: down to the limit.
        while s.write(|o| o.history_prune_some(keep, 0, None, 5)).unwrap() > 0 {}
        assert_eq!(s.read(|o| o.history_rows()).unwrap(), 2);
    }
}
