//! What applying pin rules keeps (schema v22, `oneup_app::pinrules`):
//!
//! * `pin_owned`: the files and folders a pin rule pinned, with the
//!   item's creation time read on the handle that pinned it (`created`)
//!   and the activity-log entry at the time (`at_seq`). It's written before
//!   the pin is set (an intent: a crash in between leaves it the rule's).
//!   A rule only ever unpins an item whose file ID and creation time, read
//!   again on the handle that unpins it, are the ones recorded; anything
//!   else is the user's. A placeholder that takes its pin from a rule's
//!   folder counts as the rule's only if it came later.
//! * `pin_retry`: pins and unpins that couldn't be set yet (not a
//!   placeholder yet, say), each with its operation, tried again after a
//!   back-off. At most [`PIN_RETRY_MAX`].
//! * `pin_changed`: items a rule pinned that lost their pin since (File
//!   Explorer's "Free up space"), queued by a trigger when the change
//!   journal's attribute change reaches the Local tree. A row leaves only
//!   once its item was pinned again or handed to `pin_retry`.
//! * `pin_subtree`: folders moved (or newly pinned) since, whose contents
//!   the rules must be checked against.
//! * `pin_released`: files freed on request by name ("Free up space" on
//!   the file itself) that a pin rule matches: the rule leaves them freed
//!   until someone keeps them on this device again (a trigger drops the
//!   row when the file is pinned again).
//! * `pin_user` also holds the folders the user pinned (a trigger notes a
//!   folder that gets the pin without a rule's note, and the first pass
//!   after a scan notes the folders it found pinned), with the creation
//!   time read on a handle while it's pinned (queued in `pin_changed` to
//!   read it); one that loses its pin is queued in `pin_changed`, so a
//!   "Free up space" on a folder above doesn't undo it. Its pin is put back
//!   only on the item with that creation time: after a rescan, an ID
//!   another folder reuses isn't pinned.
//! * `pin_user`: items found pinned already in a folder a rule pinned,
//!   when the rules first looked at them: the user's own, never taken as
//!   having inherited the folder's pin.
//!
//! **Bound to the Local row.** A file ID can be reused once its file is
//! gone, so these rows go with their Local row, in the same transaction
//! (triggers on `local_item`): when the row is deleted, or its kind
//! changes. A later item with the same ID starts with none.
//!
//! **A rescan keeps them.** A new scan of the same folder (after the change
//! journal was lost, say) deletes every Local row, which takes these rows
//! along; `local_begin` first sets the rules' own pins, retries and
//! releases and the user's items aside ([`Ops::pins_hold_for_rescan`]) and forgets that the
//! rules were applied, so the agent walks again and puts back those whose
//! IDs the new scan found ([`Ops::pins_rebind`]). An ID alone proves
//! nothing (it can be reused): an ownership put back is used only through
//! the creation-time check above, so a replacement that took the ID is
//! the user's; a release put back only keeps a rule from pinning.
//!
//! Also read for the rules: a folder's children a page at a time in the
//! parent index's own order (a walk never holds a whole folder), and the
//! activity log after a given entry.

use crate::local::{LOCAL_COLS, blob, fix_local, id_from_blob};
use crate::{LocalRow, Ops, Result};

/// `FILE_ATTRIBUTE_PINNED`.
const PINNED: i64 = 0x0008_0000;

/// The meta key `oneup_app::pinrules` stores the applied rules under; a
/// rescan forgets it so they are applied again.
pub const KEY_APPLIED: &str = "pins.applied";

/// Set by a rescan until [`Ops::pins_rebind`] has run: what it set aside
/// is put back, and the folders it found pinned are noted, once.
const KEY_REBIND: &str = "pins.rebind";

/// What a retry is for.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PinOp {
    Pin,
    Unpin,
}

impl PinOp {
    fn code(self) -> i64 {
        match self {
            PinOp::Pin => 0,
            PinOp::Unpin => 1,
        }
    }

    fn from_code(c: i64) -> Self {
        if c == 1 { PinOp::Unpin } else { PinOp::Pin }
    }
}

pub(crate) fn migrate_v22() -> String {
    let gone = "DELETE FROM pin_owned WHERE file_id = OLD.file_id;
        DELETE FROM pin_retry WHERE file_id = OLD.file_id;
        DELETE FROM pin_changed WHERE file_id = OLD.file_id;
        DELETE FROM pin_subtree WHERE file_id = OLD.file_id;
        DELETE FROM pin_user WHERE file_id = OLD.file_id;
        DELETE FROM pin_released WHERE file_id = OLD.file_id;";
    format!(
        "CREATE TABLE pin_owned (
             file_id BLOB PRIMARY KEY,
             created INTEGER NOT NULL DEFAULT 0,
             at_seq  INTEGER NOT NULL DEFAULT 0
         ) WITHOUT ROWID;
         CREATE TABLE pin_retry (
             file_id BLOB PRIMARY KEY,
             op      INTEGER NOT NULL DEFAULT 0,
             tries   INTEGER NOT NULL,
             next_ms INTEGER NOT NULL
         ) WITHOUT ROWID;
         CREATE INDEX pin_retry_next ON pin_retry(next_ms);
         CREATE TABLE pin_changed (file_id BLOB PRIMARY KEY, seen INTEGER NOT NULL DEFAULT 0) WITHOUT ROWID;
         CREATE TABLE pin_subtree (file_id BLOB PRIMARY KEY) WITHOUT ROWID;
         CREATE TABLE pin_user (file_id BLOB PRIMARY KEY, created INTEGER) WITHOUT ROWID;
         CREATE TABLE pin_released (file_id BLOB PRIMARY KEY) WITHOUT ROWID;
         CREATE TABLE pin_owned_hold (file_id BLOB PRIMARY KEY, created INTEGER NOT NULL, at_seq INTEGER NOT NULL) WITHOUT ROWID;
         CREATE TABLE pin_released_hold (file_id BLOB PRIMARY KEY) WITHOUT ROWID;
         CREATE TABLE pin_retry_hold (file_id BLOB PRIMARY KEY, op INTEGER NOT NULL) WITHOUT ROWID;
         CREATE TABLE pin_user_hold (file_id BLOB PRIMARY KEY, waiting INTEGER NOT NULL, created INTEGER) WITHOUT ROWID;
         CREATE TRIGGER pin_item_gone AFTER DELETE ON local_item BEGIN {gone} END;
         CREATE TRIGGER pin_item_kind AFTER UPDATE OF kind ON local_item WHEN OLD.kind != NEW.kind
         BEGIN {gone} END;
         CREATE TRIGGER pin_owned_lost AFTER UPDATE OF attributes ON local_item
             WHEN (OLD.attributes & {PINNED}) != 0 AND (NEW.attributes & {PINNED}) = 0
              AND EXISTS (SELECT 1 FROM pin_owned WHERE file_id = NEW.file_id)
         BEGIN
             DELETE FROM pin_changed WHERE file_id = NEW.file_id;
             INSERT INTO pin_changed(file_id) VALUES (NEW.file_id);
         END;
         CREATE TRIGGER pin_user_folder AFTER UPDATE OF attributes ON local_item
             WHEN NEW.kind = 1 AND (OLD.attributes & {PINNED}) = 0 AND (NEW.attributes & {PINNED}) != 0
              AND NOT EXISTS (SELECT 1 FROM pin_owned WHERE file_id = NEW.file_id)
         BEGIN
             DELETE FROM pin_user WHERE file_id = NEW.file_id;
             INSERT INTO pin_user(file_id) VALUES (NEW.file_id);
             DELETE FROM pin_changed WHERE file_id = NEW.file_id;
             INSERT INTO pin_changed(file_id) VALUES (NEW.file_id);
         END;
         CREATE TRIGGER pin_user_folder_lost AFTER UPDATE OF attributes ON local_item
             WHEN NEW.kind = 1 AND (OLD.attributes & {PINNED}) != 0 AND (NEW.attributes & {PINNED}) = 0
              AND EXISTS (SELECT 1 FROM pin_user WHERE file_id = NEW.file_id)
         BEGIN
             DELETE FROM pin_changed WHERE file_id = NEW.file_id;
             INSERT INTO pin_changed(file_id) VALUES (NEW.file_id);
         END;
         CREATE TRIGGER pin_released_pinned AFTER UPDATE OF attributes ON local_item
             WHEN (OLD.attributes & {PINNED}) = 0 AND (NEW.attributes & {PINNED}) != 0
         BEGIN
             DELETE FROM pin_released WHERE file_id = NEW.file_id;
         END;"
    )
}

/// Takes [`migrate_v22`] back out, for tests that make an older database.
#[cfg(test)]
pub(crate) const UNDO_V22: &str = "DROP TRIGGER pin_item_gone; DROP TRIGGER pin_item_kind;
    DROP TRIGGER pin_owned_lost; DROP TRIGGER pin_released_pinned; DROP TRIGGER pin_user_folder;
    DROP TRIGGER pin_user_folder_lost; DROP TABLE pin_released; DROP TABLE pin_owned; DROP TABLE pin_retry; DROP TABLE pin_changed;
    DROP TABLE pin_subtree; DROP TABLE pin_user; DROP TABLE pin_owned_hold; DROP TABLE pin_retry_hold;
    DROP TABLE pin_released_hold; DROP TABLE pin_user_hold;";

/// At most this many items wait for a pin to be retried.
pub const PIN_RETRY_MAX: usize = 10_000;

/// [`Ops::local_children_page`] (its plan is checked by a test).
fn children_page_sql() -> String {
    format!("SELECT {LOCAL_COLS} FROM local_item WHERE parent_id = ?1 AND file_id > ?2 ORDER BY file_id LIMIT ?3")
}

/// [`Ops::activity_done_after`]: a range of the primary key.
const DONE_AFTER: &str = "SELECT seq, path, detail FROM activity WHERE seq > ?1 AND outcome = 0 AND path != ''
     ORDER BY seq LIMIT ?2";

fn ids(rows: Vec<Vec<u8>>) -> Vec<u128> {
    rows.iter().map(|b| id_from_blob(b)).collect()
}

impl Ops<'_> {
    /// Up to `limit` children of `parent` after the child `after` (`None`:
    /// from the first), in the parent index's order.
    pub fn local_children_page(&self, parent: u128, after: Option<u128>, limit: usize) -> Result<Vec<LocalRow>> {
        let after: Vec<u8> = after.map(|a| blob(a).to_vec()).unwrap_or_default();
        let mut st = self.c.prepare_cached(&children_page_sql())?;
        let rows = st.query_map(rusqlite::params![blob(parent), after, limit as i64], LocalRow::from_row)?;
        rows.map(|r| fix_local(r?)).collect()
    }

    /// Up to `limit` activity entries that did something, after entry
    /// `seq`, oldest first: (seq, path, detail).
    pub fn activity_done_after(&self, seq: i64, limit: usize) -> Result<Vec<(i64, String, String)>> {
        let rows = self
            .c
            .prepare_cached(DONE_AFTER)?
            .query_map(rusqlite::params![seq, limit as i64], |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?)))?
            .collect::<rusqlite::Result<Vec<_>>>()?;
        Ok(rows)
    }

    /// The latest activity entry's number (0: none yet).
    pub fn activity_last_seq(&self) -> Result<i64> {
        Ok(self.c.query_row("SELECT COALESCE(max(seq), 0) FROM activity", [], |r| r.get(0))?)
    }

    /// Notes that a pin rule pins item `id`, created at `created` (read on
    /// the handle that pins it), when the activity log's last entry was
    /// `at_seq`; written before the pin is set. It must be in the Local tree
    /// (the note goes with its row). An existing note is replaced. Nothing
    /// is written for an item the user claimed (`pin_user`): returns
    /// whether the note was written.
    pub fn pin_owned_add(&self, id: u128, created: i64, at_seq: i64) -> Result<bool> {
        let n = self
            .c
            .prepare_cached(
                "INSERT OR REPLACE INTO pin_owned(file_id, created, at_seq)
                 SELECT file_id, ?2, ?3 FROM local_item WHERE file_id = ?1
                  AND NOT EXISTS (SELECT 1 FROM pin_user WHERE file_id = ?1)",
            )?
            .execute(rusqlite::params![blob(id), created, at_seq])?;
        Ok(n > 0)
    }

    /// The creation time recorded when a rule pinned item `id`, if one did.
    pub fn pin_owned_created(&self, id: u128) -> Result<Option<i64>> {
        use rusqlite::OptionalExtension as _;
        Ok(self
            .c
            .prepare_cached("SELECT created FROM pin_owned WHERE file_id = ?1")?
            .query_row([blob(id)], |r| r.get(0))
            .optional()?)
    }

    /// The person chose to keep item `id` on this device themselves: it's
    /// theirs from now on (no rule's note, retry or release on it).
    pub fn pin_claim_for_user(&self, id: u128) -> Result<()> {
        self.pin_owned_remove(id)?;
        self.pin_retry_remove(id)?;
        self.pin_released_remove(id)?;
        self.pin_user_add(id)
    }

    pub fn pin_owned_remove(&self, id: u128) -> Result<()> {
        self.c.prepare_cached("DELETE FROM pin_owned WHERE file_id = ?1")?.execute([blob(id)])?;
        Ok(())
    }

    pub fn pin_owned_has(&self, id: u128) -> Result<bool> {
        Ok(self.pin_owned_at(id)?.is_some())
    }

    /// When a rule pinned item `id` (the activity log's last entry then),
    /// if one did.
    pub fn pin_owned_at(&self, id: u128) -> Result<Option<i64>> {
        use rusqlite::OptionalExtension as _;
        Ok(self
            .c
            .prepare_cached("SELECT at_seq FROM pin_owned WHERE file_id = ?1")?
            .query_row([blob(id)], |r| r.get(0))
            .optional()?)
    }

    /// Up to `limit` items a pin rule pinned, after `after`, by ID.
    pub fn pin_owned_page(&self, after: Option<u128>, limit: usize) -> Result<Vec<u128>> {
        let after: Vec<u8> = after.map(|a| blob(a).to_vec()).unwrap_or_default();
        let rows: Vec<Vec<u8>> = self
            .c
            .prepare_cached("SELECT file_id FROM pin_owned WHERE file_id > ?1 ORDER BY file_id LIMIT ?2")?
            .query_map(rusqlite::params![after, limit as i64], |r| r.get(0))?
            .collect::<rusqlite::Result<_>>()?;
        Ok(ids(rows))
    }

    /// Item `id` waits for `op` to be tried again at `next_ms` (its
    /// `tries`th failure). `false` if [`PIN_RETRY_MAX`] items wait already
    /// (and this one isn't among them), or it isn't in the Local tree.
    pub fn pin_retry_put(&self, id: u128, op: PinOp, tries: u32, next_ms: i64) -> Result<bool> {
        let there = self.c.prepare_cached("SELECT 1 FROM pin_retry WHERE file_id = ?1")?.exists([blob(id)])?;
        if !there && self.pin_retry_count()? >= PIN_RETRY_MAX {
            return Ok(false);
        }
        let n = self
            .c
            .prepare_cached(
                "INSERT OR REPLACE INTO pin_retry(file_id, op, tries, next_ms)
                 SELECT file_id, ?2, ?3, ?4 FROM local_item WHERE file_id = ?1",
            )?
            .execute(rusqlite::params![blob(id), op.code(), tries, next_ms])?;
        Ok(n > 0)
    }

    /// Up to `limit` retries due at `now_ms`: (ID, operation, tries so far).
    pub fn pin_retry_due(&self, now_ms: i64, limit: usize) -> Result<Vec<(u128, PinOp, u32)>> {
        let rows: Vec<(Vec<u8>, i64, u32)> = self
            .c
            .prepare_cached("SELECT file_id, op, tries FROM pin_retry WHERE next_ms <= ?1 ORDER BY next_ms LIMIT ?2")?
            .query_map(rusqlite::params![now_ms, limit as i64], |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?)))?
            .collect::<rusqlite::Result<_>>()?;
        Ok(rows.into_iter().map(|(b, op, t)| (id_from_blob(&b), PinOp::from_code(op), t)).collect())
    }

    pub fn pin_retry_remove(&self, id: u128) -> Result<()> {
        self.c.prepare_cached("DELETE FROM pin_retry WHERE file_id = ?1")?.execute([blob(id)])?;
        Ok(())
    }

    /// How many items wait for a pin to be retried.
    pub fn pin_retry_count(&self) -> Result<usize> {
        let n: i64 = self.c.query_row("SELECT count(*) FROM pin_retry", [], |r| r.get(0))?;
        Ok(usize::try_from(n).unwrap_or(0))
    }

    /// Up to `limit` items a rule pinned that lost their pin since (queued
    /// by the `pin_owned_lost` trigger). They stay queued until
    /// [`Ops::pin_changed_done`].
    pub fn pin_changed_page(&self, limit: usize) -> Result<Vec<u128>> {
        let rows: Vec<Vec<u8>> = self
            .c
            .prepare_cached("SELECT file_id FROM pin_changed LIMIT ?1")?
            .query_map([limit as i64], |r| r.get(0))?
            .collect::<rusqlite::Result<_>>()?;
        Ok(ids(rows))
    }

    /// Item `id` was pinned again, or handed to `pin_retry`, or needs
    /// nothing: off the queue.
    /// Whether queued item `id` was looked at once already and left
    /// queued ([`Ops::pin_changed_seen_mark`]).
    pub fn pin_changed_seen(&self, id: u128) -> Result<bool> {
        Ok(self.c.prepare_cached("SELECT 1 FROM pin_changed WHERE file_id = ?1 AND seen != 0")?.exists([blob(id)])?)
    }

    /// Queued item `id` was looked at and left queued for a later pass.
    pub fn pin_changed_seen_mark(&self, id: u128) -> Result<()> {
        self.c.prepare_cached("UPDATE pin_changed SET seen = 1 WHERE file_id = ?1")?.execute([blob(id)])?;
        Ok(())
    }

    pub fn pin_changed_done(&self, id: u128) -> Result<()> {
        self.c.prepare_cached("DELETE FROM pin_changed WHERE file_id = ?1")?.execute([blob(id)])?;
        Ok(())
    }

    /// Folder `id` moved: its contents are checked against the rules at
    /// its new place (by `oneup_app::pinrules`).
    pub fn pin_subtree_add(&self, id: u128) -> Result<()> {
        self.c
            .prepare_cached(
                "INSERT OR IGNORE INTO pin_subtree(file_id) SELECT file_id FROM local_item WHERE file_id = ?1",
            )?
            .execute([blob(id)])?;
        Ok(())
    }

    /// The next moved folder to check, if any.
    pub fn pin_subtree_next(&self) -> Result<Option<u128>> {
        let rows: Vec<Vec<u8>> = self
            .c
            .prepare_cached("SELECT file_id FROM pin_subtree LIMIT 1")?
            .query_map([], |r| r.get(0))?
            .collect::<rusqlite::Result<_>>()?;
        Ok(ids(rows).pop())
    }

    pub fn pin_subtree_has(&self, id: u128) -> Result<bool> {
        Ok(self.c.prepare_cached("SELECT 1 FROM pin_subtree WHERE file_id = ?1")?.exists([blob(id)])?)
    }

    /// File `id` was freed on request by name: no pin rule keeps it until
    /// it's pinned again (the rule's note on it goes).
    pub fn pin_released_add(&self, id: u128) -> Result<()> {
        self.c
            .prepare_cached(
                "INSERT OR IGNORE INTO pin_released(file_id) SELECT file_id FROM local_item WHERE file_id = ?1",
            )?
            .execute([blob(id)])?;
        self.pin_owned_remove(id)
    }

    pub fn pin_released_has(&self, id: u128) -> Result<bool> {
        Ok(self.c.prepare_cached("SELECT 1 FROM pin_released WHERE file_id = ?1")?.exists([blob(id)])?)
    }

    pub fn pin_released_remove(&self, id: u128) -> Result<()> {
        self.c.prepare_cached("DELETE FROM pin_released WHERE file_id = ?1")?.execute([blob(id)])?;
        Ok(())
    }

    /// Item `id` was found pinned already where a rule pins: the user's.
    pub fn pin_user_add(&self, id: u128) -> Result<()> {
        self.c
            .prepare_cached(
                "INSERT OR IGNORE INTO pin_user(file_id) SELECT file_id FROM local_item WHERE file_id = ?1",
            )?
            .execute([blob(id)])?;
        Ok(())
    }

    pub fn pin_user_remove(&self, id: u128) -> Result<()> {
        self.c.prepare_cached("DELETE FROM pin_user WHERE file_id = ?1")?.execute([blob(id)])?;
        Ok(())
    }

    /// The creation time of the user's item `id`, read on a handle while
    /// it was pinned, if one was.
    pub fn pin_user_created(&self, id: u128) -> Result<Option<i64>> {
        use rusqlite::OptionalExtension as _;
        Ok(self
            .c
            .prepare_cached("SELECT created FROM pin_user WHERE file_id = ?1")?
            .query_row([blob(id)], |r| r.get::<_, Option<i64>>(0))
            .optional()?
            .flatten())
    }

    /// Records the creation time of the user's item `id`.
    pub fn pin_user_set_created(&self, id: u128, created: i64) -> Result<()> {
        self.c
            .prepare_cached("UPDATE pin_user SET created = ?2 WHERE file_id = ?1")?
            .execute(rusqlite::params![blob(id), created])?;
        Ok(())
    }

    pub fn pin_user_has(&self, id: u128) -> Result<bool> {
        Ok(self.c.prepare_cached("SELECT 1 FROM pin_user WHERE file_id = ?1")?.exists([blob(id)])?)
    }

    pub fn pin_subtree_done(&self, id: u128) -> Result<()> {
        self.c.prepare_cached("DELETE FROM pin_subtree WHERE file_id = ?1")?.execute([blob(id)])?;
        Ok(())
    }

    /// Before a rescan deletes every Local row (`local_begin`): for the same
    /// folder (`same`), the rules' own pins and retries are set aside, as
    /// file IDs of the same volume stay the same; either way the rules count
    /// as not applied, so the agent walks again.
    pub(crate) fn pins_hold_for_rescan(&self, same: bool) -> Result<()> {
        if same {
            self.c.execute_batch(
                "INSERT OR IGNORE INTO pin_owned_hold(file_id, created, at_seq) SELECT file_id, created, at_seq FROM pin_owned;
                 INSERT OR IGNORE INTO pin_retry_hold(file_id, op) SELECT file_id, op FROM pin_retry;
                 INSERT OR IGNORE INTO pin_released_hold(file_id) SELECT file_id FROM pin_released;
                 INSERT OR IGNORE INTO pin_user_hold(file_id, waiting, created)
                     SELECT u.file_id, EXISTS (SELECT 1 FROM pin_changed c WHERE c.file_id = u.file_id), u.created
                     FROM pin_user u;",
            )?;
        } else {
            self.c.execute_batch(
                "DELETE FROM pin_owned_hold; DELETE FROM pin_retry_hold; DELETE FROM pin_released_hold;
                 DELETE FROM pin_user_hold;",
            )?;
        }
        self.meta_set(KEY_REBIND, "1")?;
        self.meta_delete(KEY_APPLIED)
    }

    /// Puts back what [`Ops::pins_hold_for_rescan`] set aside, for the items
    /// the new scan found; drops the rest. Returns how many of the rules'
    /// own were put back. Then notes the folders the scan found pinned that
    /// aren't the rules' as the user's (the scan inserts rows, which no
    /// trigger watches), and queues again a user's folder that was waiting
    /// for its pin to be restored. Once per rescan (one pass over
    /// the folders), nothing otherwise.
    pub fn pins_rebind(&self) -> Result<usize> {
        if self.meta_get(KEY_REBIND)?.is_none() {
            return Ok(0);
        }
        let n = self.c.execute(
            "INSERT OR IGNORE INTO pin_owned(file_id, created, at_seq)
             SELECT h.file_id, h.created, h.at_seq FROM pin_owned_hold h JOIN local_item l ON l.file_id = h.file_id",
            [],
        )?;
        self.c.execute(
            "INSERT OR IGNORE INTO pin_released(file_id)
             SELECT h.file_id FROM pin_released_hold h JOIN local_item l ON l.file_id = h.file_id",
            [],
        )?;
        self.c.execute(
            "INSERT OR IGNORE INTO pin_retry(file_id, op, tries, next_ms)
             SELECT h.file_id, h.op, 1, 0 FROM pin_retry_hold h JOIN local_item l ON l.file_id = h.file_id",
            [],
        )?;
        self.c.execute_batch(&format!(
            "INSERT OR IGNORE INTO pin_user(file_id, created)
                 SELECT h.file_id, h.created FROM pin_user_hold h JOIN local_item l ON l.file_id = h.file_id
                 WHERE h.created IS NOT NULL;
             INSERT OR IGNORE INTO pin_user(file_id)
                 SELECT l.file_id FROM local_item l WHERE l.kind = 1 AND (l.attributes & {PINNED}) != 0
                  AND NOT EXISTS (SELECT 1 FROM pin_owned o WHERE o.file_id = l.file_id);
             INSERT OR IGNORE INTO pin_changed(file_id)
                 SELECT h.file_id FROM pin_user_hold h JOIN local_item l ON l.file_id = h.file_id
                 WHERE h.waiting != 0 AND h.created IS NOT NULL AND l.kind = 1 AND (l.attributes & {PINNED}) = 0;
             INSERT OR IGNORE INTO pin_changed(file_id)
                 SELECT u.file_id FROM pin_user u JOIN local_item l ON l.file_id = u.file_id
                 WHERE u.created IS NULL AND l.kind = 1 AND (l.attributes & {PINNED}) != 0;
             DELETE FROM pin_owned_hold; DELETE FROM pin_retry_hold; DELETE FROM pin_released_hold;
             DELETE FROM pin_user_hold;"
        ))?;
        self.meta_delete(KEY_REBIND)?;
        Ok(n)
    }
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used)]
    use super::PinOp;
    use super::*;
    use crate::{LocalKind, LocalRoot, Skip, Store};

    fn row(id: u128, parent: u128, kind: LocalKind, attributes: u32) -> LocalRow {
        LocalRow {
            file_id: id,
            parent_id: Some(parent),
            name: format!("n{id}"),
            name_lossy: false,
            kind,
            size: 1,
            mtime: 0,
            attributes,
            reparse_tag: 0,
            skip: Skip::None,
        }
    }

    fn store() -> Store {
        let mut s = Store::open_in_memory().unwrap();
        s.write(|o| {
            let root = LocalRow { parent_id: None, ..row(1, 0, LocalKind::Root, 0x10) };
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
            o.local_begin(&info, &root)
        })
        .unwrap();
        s
    }

    fn plan(s: &Store, sql: &str) -> String {
        let mut st = s.conn.prepare(&format!("EXPLAIN QUERY PLAN {sql}")).unwrap();
        let params: Vec<rusqlite::types::Null> = vec![rusqlite::types::Null; st.parameter_count()];
        let rows = st.query_map(rusqlite::params_from_iter(params), |r| r.get::<_, String>(3)).unwrap();
        rows.map(|r| r.unwrap()).collect::<Vec<_>>().join("; ")
    }

    #[test]
    fn pages_and_the_log_are_read_by_their_indexes() {
        let s = store();
        let p = plan(&s, &children_page_sql());
        assert!(p.contains("USING INDEX local_item_parent") && !p.contains("TEMP B-TREE"), "{p}");
        let p = plan(&s, DONE_AFTER);
        assert!(p.contains("USING INTEGER PRIMARY KEY") && !p.contains("TEMP B-TREE"), "{p}");
    }

    /// A file ID can be reused once its file is gone: ownership and
    /// retries go with the Local row, in the same transaction, so a new
    /// item with the old ID starts with neither.
    #[test]
    fn notes_go_with_their_local_row_and_a_reused_id_starts_clean() {
        let mut s = store();
        s.write(|o| {
            o.local_put(&row(7, 1, LocalKind::File, PINNED as u32))?;
            o.local_put(&row(8, 1, LocalKind::File, 0))?;
            o.pin_owned_add(7, 0, 0)?;
            assert!(o.pin_retry_put(8, PinOp::Pin, 1, 0)?);
            o.pin_owned_add(99, 0, 0)?;
            assert!(!o.pin_retry_put(98, PinOp::Pin, 1, 0)?, "not in the Local tree");
            assert!(o.pin_owned_has(7)? && !o.pin_owned_has(99)?);
            o.local_remove_subtree(7)?;
            o.local_remove_subtree(8)?;
            assert!(!o.pin_owned_has(7)? && o.pin_retry_count()? == 0, "gone with their rows");
            // The ID comes back, for another file.
            o.local_put(&row(7, 1, LocalKind::File, PINNED as u32))?;
            assert!(!o.pin_owned_has(7)?, "a reused ID isn't the rule's");
            o.pin_owned_add(7, 0, 0)?;
            o.local_update(&row(7, 1, LocalKind::Folder, PINNED as u32))?;
            assert!(!o.pin_owned_has(7)?, "a change of kind is another item");
            Ok(())
        })
        .unwrap();
    }

    /// Losing the pin (File Explorer's "Free up space", recorded from the
    /// journal) queues an item a rule pinned; an item the rule didn't pin
    /// isn't queued.
    #[test]
    fn an_owned_item_losing_its_pin_is_queued() {
        let mut s = store();
        s.write(|o| {
            o.local_put(&row(7, 1, LocalKind::File, PINNED as u32))?;
            o.local_put(&row(8, 1, LocalKind::File, PINNED as u32))?;
            o.pin_owned_add(7, 0, 0)?;
            o.local_update(&row(7, 1, LocalKind::File, 0x0010_0000))?;
            o.local_update(&row(8, 1, LocalKind::File, 0x0010_0000))?;
            assert_eq!(o.pin_changed_page(10)?, [7]);
            assert_eq!(o.pin_changed_page(10)?, [7], "kept until done");
            o.pin_changed_done(7)?;
            assert!(o.pin_changed_page(10)?.is_empty());
            Ok(())
        })
        .unwrap();
    }

    /// A rescan of the same folder deletes every Local row (and so the
    /// rules' notes); they're set aside first and put back for the items the
    /// new scan finds, and the rules count as not applied.
    #[test]
    fn a_rescan_of_the_same_folder_keeps_the_rules_pins() {
        let mut s = store();
        s.write(|o| {
            o.local_put(&row(7, 1, LocalKind::File, PINNED as u32))?;
            o.local_put(&row(8, 1, LocalKind::File, 0))?;
            o.local_put(&row(9, 1, LocalKind::File, PINNED as u32))?;
            o.pin_owned_add(7, 70, 5)?;
            o.pin_owned_add(9, 90, 5)?;
            assert!(o.pin_retry_put(8, PinOp::Unpin, 3, 99)?);
            o.pin_released_add(8)?;
            o.meta_set(KEY_APPLIED, "p2:x")
        })
        .unwrap();
        // A rescan: same root, rows back except 9.
        s.write(|o| {
            let root = LocalRow { parent_id: None, ..row(1, 0, LocalKind::Root, 0x10) };
            let info = LocalRoot {
                path: r"C:\Sync".into(),
                volume_serial: 1,
                root_file_id: 1,
                rules: String::new(),
                journal: None,
                started_at_ms: 0,
                finished_at_ms: None,
                sync_root_provider: None,
            };
            o.local_begin(&info, &root)?;
            assert!(!o.pin_owned_has(7)?, "gone with the rows for now");
            assert!(o.meta_get(KEY_APPLIED)?.is_none(), "applied again");
            o.local_put(&row(7, 1, LocalKind::File, PINNED as u32))?;
            o.local_put(&row(8, 1, LocalKind::File, 0))?;
            assert_eq!(o.pins_rebind()?, 1);
            assert_eq!((o.pin_owned_at(7)?, o.pin_owned_created(7)?), (Some(5), Some(70)));
            assert!(!o.pin_owned_has(9)?, "not found again: dropped");
            assert_eq!(o.pin_retry_due(0, 10)?, [(8, PinOp::Unpin, 1)]);
            assert!(o.pin_released_has(8)?, "a file freed by name stays released");
            assert_eq!(o.pins_rebind()?, 0, "set aside once");
            assert!(o.meta_get(KEY_REBIND)?.is_none());
            Ok(())
        })
        .unwrap();
    }

    /// A rescan keeps the user's items, notes the folders it finds pinned
    /// (not the rules') as the user's, and queues a user's folder whose pin
    /// was taken, to be restored.
    #[test]
    fn a_rescan_keeps_and_notes_the_users_folders() {
        let mut s = store();
        let folder = |id, attributes| row(id, 1, LocalKind::Folder, 0x10 | attributes);
        s.write(|o| {
            o.local_put(&folder(2, 0))?;
            o.local_put(&folder(3, PINNED as u32))?;
            o.local_update(&folder(2, PINNED as u32))?;
            assert!(o.pin_user_has(2)?, "pinned: the trigger notes it");
            assert_eq!(o.pin_changed_page(10)?, [2], "queued for its creation time");
            o.pin_user_set_created(2, 20)?;
            o.pin_changed_done(2)?;
            // 5 too, its creation time never read.
            o.local_put(&folder(5, 0))?;
            o.local_update(&folder(5, PINNED as u32))?;
            // A "Free up space" above took their pins; not restored yet.
            o.local_update(&folder(2, 0x0010_0000))?;
            o.local_update(&folder(5, 0x0010_0000))?;
            o.pin_owned_add(3, 30, 1).map(|_| ())
        })
        .unwrap();
        s.write(|o| {
            let root = LocalRow { parent_id: None, ..row(1, 0, LocalKind::Root, 0x10) };
            let info = LocalRoot {
                path: r"C:\Sync".into(),
                volume_serial: 1,
                root_file_id: 1,
                rules: String::new(),
                journal: None,
                started_at_ms: 0,
                finished_at_ms: None,
                sync_root_provider: None,
            };
            o.local_begin(&info, &root)?;
            o.local_put(&folder(2, 0x0010_0000))?;
            o.local_put(&folder(3, PINNED as u32))?;
            o.local_put(&folder(4, PINNED as u32))?;
            o.local_put(&folder(5, 0x0010_0000))?;
            assert!(!o.pin_user_has(4)?, "an insert notes nothing");
            o.pins_rebind()?;
            assert!(o.pin_user_has(2)?, "held across the rescan");
            assert_eq!(o.pin_user_created(2)?, Some(20));
            assert!(!o.pin_user_has(5)?, "unproven: its ID may be another folder's");
            assert!(o.pin_user_has(4)?, "found pinned: the user's");
            assert!(!o.pin_user_has(3)?, "the rules'");
            let mut queued = o.pin_changed_page(10)?;
            queued.sort_unstable();
            assert_eq!(queued, [2, 4], "to be restored; to read its creation time");
            o.pin_user_remove(4)?;
            o.pins_rebind()?;
            assert!(!o.pin_user_has(4)?, "once per rescan");
            Ok(())
        })
        .unwrap();
    }

    #[test]
    fn moved_folders_wait_in_a_queue() {
        let mut s = store();
        s.write(|o| {
            o.local_put(&row(7, 1, LocalKind::Folder, 0x10))?;
            o.pin_subtree_add(7)?;
            o.pin_subtree_add(70)?;
            assert_eq!(o.pin_subtree_next()?, Some(7), "only items in the Local tree");
            o.pin_subtree_done(7)?;
            assert_eq!(o.pin_subtree_next()?, None);
            Ok(())
        })
        .unwrap();
    }

    /// Keeping an item on this device oneself makes it the user's: no rule's
    /// note, retry or release stays on it.
    #[test]
    fn keeping_it_oneself_makes_it_the_users() {
        let mut s = store();
        s.write(|o| {
            o.local_put(&row(7, 1, LocalKind::File, PINNED as u32))?;
            o.pin_owned_add(7, 70, 1)?;
            assert!(o.pin_retry_put(7, PinOp::Unpin, 1, 0)?);
            o.pin_claim_for_user(7)?;
            assert!(!o.pin_owned_has(7)? && o.pin_retry_count()? == 0 && o.pin_user_has(7)?);
            assert!(!o.pin_owned_add(7, 70, 2)?, "the user's: no note");
            assert!(!o.pin_owned_has(7)?);
            Ok(())
        })
        .unwrap();
    }

    /// A file freed by name stays released from the rules until it's pinned
    /// again (the trigger), and the release goes with its row.
    #[test]
    fn a_release_lasts_until_the_file_is_pinned_again() {
        let mut s = store();
        s.write(|o| {
            o.local_put(&row(7, 1, LocalKind::File, PINNED as u32))?;
            o.pin_owned_add(7, 70, 1)?;
            o.pin_released_add(7)?;
            assert!(o.pin_released_has(7)? && !o.pin_owned_has(7)?, "no longer the rule's");
            o.local_update(&row(7, 1, LocalKind::File, 0x0010_0000))?;
            assert!(o.pin_released_has(7)?, "freed: still released");
            o.local_update(&row(7, 1, LocalKind::File, PINNED as u32))?;
            assert!(!o.pin_released_has(7)?, "pinned again: the release ends");
            o.pin_released_add(7)?;
            o.local_remove_subtree(7)?;
            assert!(!o.pin_released_has(7)?);
            Ok(())
        })
        .unwrap();
    }

    /// A database at v19 (choose-what-syncs, before duplicates' v20 and the
    /// pin tables) gets them on opening, triggers included.
    #[test]
    fn a_v19_database_upgrades() {
        let dir = std::env::temp_dir().join(format!("oneup-store-pins-up-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("state.db");
        {
            let s = Store::open(&path, 1024).unwrap();
            s.read(|o| {
                Ok(o.c.execute_batch(&format!(
                    "{UNDO_V22} {} {} PRAGMA user_version = 19;",
                    crate::shared::UNDO_V21,
                    crate::dupes::UNDO_V20
                ))?)
            })
            .unwrap();
        }
        let mut s = Store::open(&path, 1024).unwrap();
        let v: i64 = s.conn.query_row("PRAGMA user_version", [], |r| r.get(0)).unwrap();
        assert_eq!(v, crate::SCHEMA_VERSION);
        s.write(|o| {
            o.local_put(&row(7, 1, LocalKind::File, PINNED as u32))?;
            o.pin_owned_add(7, 70, 1)?;
            assert!(o.pin_owned_has(7)?);
            o.local_remove_subtree(7)?;
            assert!(!o.pin_owned_has(7)?, "the triggers are there");
            Ok(())
        })
        .unwrap();
        drop(s);
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn retries_are_bounded() {
        let mut s = store();
        s.write(|o| {
            for id in 0..=PIN_RETRY_MAX as u128 {
                o.local_put(&row(1000 + id, 1, LocalKind::File, 0))?;
            }
            for id in 0..PIN_RETRY_MAX as u128 {
                assert!(o.pin_retry_put(1000 + id, PinOp::Pin, 1, 0)?);
            }
            assert!(!o.pin_retry_put(1000 + PIN_RETRY_MAX as u128, PinOp::Pin, 1, 0)?, "full");
            assert!(o.pin_retry_put(1000, PinOp::Unpin, 2, 10)?, "one already there is updated");
            assert_eq!(o.pin_retry_due(5, 3)?.len(), 3);
            Ok(())
        })
        .unwrap();
    }
}
