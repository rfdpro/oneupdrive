//! What applying pin rules keeps (schema v22, `oneup_app::pinrules`):
//!
//! * `pin_owned`: the files and folders a pin rule pinned (or that took
//!   the pin from a folder a rule pinned), so removing the rule unpins
//!   those and never one the user pinned.
//! * `pin_retry`: items whose pin couldn't be set yet (not a placeholder
//!   yet, say), tried again after a back-off. At most [`PIN_RETRY_MAX`].
//! * `pin_changed`: items a rule pinned that lost their pin since (File
//!   Explorer's "Free up space"), queued by a trigger when the change
//!   journal's attribute change reaches the Local tree, so the agent finds
//!   them without any sync activity. Never more rows than `pin_owned`.
//!
//! **Bound to the Local row.** A file ID can be reused once its file is
//! gone, so these rows go with their Local row, in the same transaction
//! (triggers on `local_item`): when the row is deleted, or its kind
//! changes. A later item with the same ID starts with none.
//!
//! Also read for the rules: a folder's children a page at a time in the
//! parent index's own order (a walk never holds a whole folder), and the
//! activity log after a given entry.

use crate::local::{LOCAL_COLS, blob, fix_local, id_from_blob};
use crate::{LocalRow, Ops, Result};

/// `FILE_ATTRIBUTE_PINNED`.
const PINNED: i64 = 0x0008_0000;

pub(crate) fn migrate_v22() -> String {
    let gone = "DELETE FROM pin_owned WHERE file_id = OLD.file_id;
        DELETE FROM pin_retry WHERE file_id = OLD.file_id;
        DELETE FROM pin_changed WHERE file_id = OLD.file_id;";
    format!(
        "CREATE TABLE pin_owned (file_id BLOB PRIMARY KEY) WITHOUT ROWID;
         CREATE TABLE pin_retry (
             file_id BLOB PRIMARY KEY,
             tries   INTEGER NOT NULL,
             next_ms INTEGER NOT NULL
         ) WITHOUT ROWID;
         CREATE INDEX pin_retry_next ON pin_retry(next_ms);
         CREATE TABLE pin_changed (file_id BLOB PRIMARY KEY) WITHOUT ROWID;
         CREATE TRIGGER pin_item_gone AFTER DELETE ON local_item BEGIN {gone} END;
         CREATE TRIGGER pin_item_kind AFTER UPDATE OF kind ON local_item WHEN OLD.kind != NEW.kind
         BEGIN {gone} END;
         CREATE TRIGGER pin_owned_lost AFTER UPDATE OF attributes ON local_item
             WHEN (OLD.attributes & {PINNED}) != 0 AND (NEW.attributes & {PINNED}) = 0
              AND EXISTS (SELECT 1 FROM pin_owned WHERE file_id = NEW.file_id)
         BEGIN
             DELETE FROM pin_changed WHERE file_id = NEW.file_id;
             INSERT INTO pin_changed(file_id) VALUES (NEW.file_id);
         END;"
    )
}

/// Takes [`migrate_v22`] back out, for tests that make an older database.
#[cfg(test)]
pub(crate) const UNDO_V22: &str = "DROP TRIGGER pin_item_gone; DROP TRIGGER pin_item_kind;
    DROP TRIGGER pin_owned_lost; DROP TABLE pin_owned; DROP TABLE pin_retry; DROP TABLE pin_changed;";

/// At most this many items wait for a pin to be retried.
pub const PIN_RETRY_MAX: usize = 10_000;

/// [`Ops::local_children_page`] (its plan is checked by a test).
fn children_page_sql() -> String {
    format!("SELECT {LOCAL_COLS} FROM local_item WHERE parent_id = ?1 AND file_id > ?2 ORDER BY file_id LIMIT ?3")
}

/// [`Ops::activity_done_after`]: a range of the primary key.
const DONE_AFTER: &str = "SELECT seq, path, detail FROM activity WHERE seq > ?1 AND outcome = 0 AND path != ''
     ORDER BY seq LIMIT ?2";

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

    /// Notes that a pin rule pinned item `id` (it must be in the Local tree:
    /// the note goes with its row).
    pub fn pin_owned_add(&self, id: u128) -> Result<()> {
        self.c
            .prepare_cached(
                "INSERT OR IGNORE INTO pin_owned(file_id) SELECT file_id FROM local_item WHERE file_id = ?1",
            )?
            .execute([blob(id)])?;
        Ok(())
    }

    pub fn pin_owned_remove(&self, id: u128) -> Result<()> {
        self.c.prepare_cached("DELETE FROM pin_owned WHERE file_id = ?1")?.execute([blob(id)])?;
        Ok(())
    }

    pub fn pin_owned_has(&self, id: u128) -> Result<bool> {
        let mut st = self.c.prepare_cached("SELECT 1 FROM pin_owned WHERE file_id = ?1")?;
        Ok(st.exists([blob(id)])?)
    }

    /// Up to `limit` items a pin rule pinned, after `after`, by ID.
    pub fn pin_owned_page(&self, after: Option<u128>, limit: usize) -> Result<Vec<u128>> {
        let after: Vec<u8> = after.map(|a| blob(a).to_vec()).unwrap_or_default();
        let ids: Vec<Vec<u8>> = self
            .c
            .prepare_cached("SELECT file_id FROM pin_owned WHERE file_id > ?1 ORDER BY file_id LIMIT ?2")?
            .query_map(rusqlite::params![after, limit as i64], |r| r.get(0))?
            .collect::<rusqlite::Result<_>>()?;
        Ok(ids.iter().map(|b| id_from_blob(b)).collect())
    }

    /// Item `id` waits for its pin to be tried again at `next_ms` (its
    /// `tries`th failure). `false` if [`PIN_RETRY_MAX`] items wait already
    /// (and this one isn't among them), or it isn't in the Local tree.
    pub fn pin_retry_put(&self, id: u128, tries: u32, next_ms: i64) -> Result<bool> {
        let there = self.c.prepare_cached("SELECT 1 FROM pin_retry WHERE file_id = ?1")?.exists([blob(id)])?;
        if !there && self.pin_retry_count()? >= PIN_RETRY_MAX {
            return Ok(false);
        }
        let n = self
            .c
            .prepare_cached(
                "INSERT OR REPLACE INTO pin_retry(file_id, tries, next_ms)
                 SELECT file_id, ?2, ?3 FROM local_item WHERE file_id = ?1",
            )?
            .execute(rusqlite::params![blob(id), tries, next_ms])?;
        Ok(n > 0)
    }

    /// Up to `limit` items whose retry is due at `now_ms`: (ID, tries so far).
    pub fn pin_retry_due(&self, now_ms: i64, limit: usize) -> Result<Vec<(u128, u32)>> {
        let rows: Vec<(Vec<u8>, u32)> = self
            .c
            .prepare_cached("SELECT file_id, tries FROM pin_retry WHERE next_ms <= ?1 ORDER BY next_ms LIMIT ?2")?
            .query_map(rusqlite::params![now_ms, limit as i64], |r| Ok((r.get(0)?, r.get(1)?)))?
            .collect::<rusqlite::Result<_>>()?;
        Ok(rows.into_iter().map(|(b, t)| (id_from_blob(&b), t)).collect())
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

    /// Takes up to `limit` items a rule pinned that lost their pin since
    /// (queued by the `pin_owned_lost` trigger).
    pub fn pin_changed_take(&self, limit: usize) -> Result<Vec<u128>> {
        let ids: Vec<Vec<u8>> = self
            .c
            .prepare_cached("SELECT file_id FROM pin_changed LIMIT ?1")?
            .query_map([limit as i64], |r| r.get(0))?
            .collect::<rusqlite::Result<_>>()?;
        let mut del = self.c.prepare_cached("DELETE FROM pin_changed WHERE file_id = ?1")?;
        for b in &ids {
            del.execute([b])?;
        }
        Ok(ids.iter().map(|b| id_from_blob(b)).collect())
    }
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used)]
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
            o.pin_owned_add(7)?;
            assert!(o.pin_retry_put(8, 1, 0)?);
            o.pin_owned_add(99)?;
            assert!(!o.pin_retry_put(98, 1, 0)?, "not in the Local tree");
            assert!(o.pin_owned_has(7)? && !o.pin_owned_has(99)?);
            o.local_remove_subtree(7)?;
            o.local_remove_subtree(8)?;
            assert!(!o.pin_owned_has(7)? && o.pin_retry_count()? == 0, "gone with their rows");
            // The ID comes back, for another file.
            o.local_put(&row(7, 1, LocalKind::File, PINNED as u32))?;
            assert!(!o.pin_owned_has(7)?, "a reused ID isn't the rule's");
            o.pin_owned_add(7)?;
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
            o.pin_owned_add(7)?;
            o.local_update(&row(7, 1, LocalKind::File, 0x0010_0000))?;
            o.local_update(&row(8, 1, LocalKind::File, 0x0010_0000))?;
            assert_eq!(o.pin_changed_take(10)?, [7]);
            assert!(o.pin_changed_take(10)?.is_empty(), "taken");
            Ok(())
        })
        .unwrap();
    }

    #[test]
    fn retries_are_bounded() {
        let mut s = store();
        s.write(|o| {
            for id in 0..=PIN_RETRY_MAX as u128 {
                o.local_put(&row(1000 + id, 1, LocalKind::File, 0))?;
            }
            for id in 0..PIN_RETRY_MAX as u128 {
                assert!(o.pin_retry_put(1000 + id, 1, 0)?);
            }
            assert!(!o.pin_retry_put(1000 + PIN_RETRY_MAX as u128, 1, 0)?, "full");
            assert!(o.pin_retry_put(1000, 2, 10)?, "one already there is updated");
            assert_eq!(o.pin_retry_due(5, 3)?.len(), 3);
            Ok(())
        })
        .unwrap();
    }
}
