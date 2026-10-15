//! Where the `.oneupignore` files are (schema v7), so loading the rules
//! costs what the rule files cost, not a search of the whole Local tree.
//! Triggers keep the list current whoever writes `local_item` (scanner,
//! observer, executor), as for the dirty marks (`dirty.rs`). The list holds
//! candidates; readers check each row's current name and kind.

use crate::local::{blob, id_from_blob};
use crate::{LocalKind, LocalRow, Ops, Result};

/// v6 -> v7. `lower()` folds ASCII only, which is all the name needs.
pub(crate) const MIGRATE_V7: &str = "
CREATE TABLE local_rule_file (file_id BLOB PRIMARY KEY) WITHOUT ROWID;
INSERT INTO local_rule_file SELECT file_id FROM local_item WHERE lower(name) = '.oneupignore';
CREATE TRIGGER rule_file_ins AFTER INSERT ON local_item WHEN lower(NEW.name) = '.oneupignore'
BEGIN
    DELETE FROM local_rule_file WHERE file_id = NEW.file_id;
    INSERT INTO local_rule_file(file_id) VALUES (NEW.file_id);
END;
CREATE TRIGGER rule_file_named AFTER UPDATE OF name ON local_item WHEN lower(NEW.name) = '.oneupignore'
BEGIN
    DELETE FROM local_rule_file WHERE file_id = NEW.file_id;
    INSERT INTO local_rule_file(file_id) VALUES (NEW.file_id);
END;
CREATE TRIGGER rule_file_renamed AFTER UPDATE OF name ON local_item
    WHEN lower(OLD.name) = '.oneupignore' AND lower(NEW.name) != '.oneupignore'
BEGIN
    DELETE FROM local_rule_file WHERE file_id = OLD.file_id;
END;
CREATE TRIGGER rule_file_del AFTER DELETE ON local_item WHEN lower(OLD.name) = '.oneupignore'
BEGIN
    DELETE FROM local_rule_file WHERE file_id = OLD.file_id;
END;
";

/// v7 -> v8: where Files On-Demand's placement work is (pin states are
/// attributes): files "always kept" but not downloaded, and files set to
/// "free up space" but still downloaded. Partial indexes, so they cost
/// what those few rows cost, not the whole table.
pub(crate) const MIGRATE_V8: &str = "
CREATE INDEX local_to_fetch ON local_item(file_id)
    WHERE kind = 0 AND (attributes & 524288) != 0 AND (attributes & 4194304) != 0;
CREATE INDEX local_to_free ON local_item(file_id)
    WHERE kind = 0 AND (attributes & 1048576) != 0 AND (attributes & 4194304) = 0 AND (attributes & 1024) != 0;
";

/// v13 -> v14: the folders to mark excluded for File Explorer: those left out by name (`skip` 1), and those carrying both pin
/// bits (pinned *excluded*), to unmark once synced again. Partial indexes:
/// a thousand rows, not the whole table.
pub(crate) const MIGRATE_V14: &str = "
CREATE INDEX local_excluded_folders ON local_item(file_id) WHERE kind = 1 AND skip = 1;
CREATE INDEX local_pinned_excluded ON local_item(file_id) WHERE kind = 1 AND (attributes & 1572864) = 1572864;
";

impl Ops<'_> {
    /// Up to `limit` folders left out by name (`node_modules`, `.git`...).
    pub fn local_excluded_folders(&self, limit: usize) -> Result<Vec<LocalRow>> {
        self.rows_where("kind = 1 AND skip = 1", limit)
    }

    /// Up to `limit` listed folders (not left out) pinned *excluded*:
    /// marks to undo, now that the folder syncs again.
    pub fn local_marked_but_synced(&self, limit: usize) -> Result<Vec<LocalRow>> {
        self.rows_where("kind = 1 AND (attributes & 1572864) = 1572864 AND skip = 0", limit)
    }

    /// Up to `limit` files pinned ("always keep on this device") whose
    /// content isn't here.
    pub fn local_to_fetch(&self, limit: usize) -> Result<Vec<LocalRow>> {
        self.rows_where("kind = 0 AND (attributes & 524288) != 0 AND (attributes & 4194304) != 0", limit)
    }

    /// Up to `limit` placeholders set to "free up space" whose content is
    /// still here. Not empty files: freeing one is always refused (it looks
    /// freed already), so they'd fill every batch for good.
    pub fn local_to_free(&self, limit: usize) -> Result<Vec<LocalRow>> {
        self.rows_where(
            "kind = 0 AND (attributes & 1048576) != 0 AND (attributes & 4194304) = 0 AND (attributes & 1024) != 0 \
             AND size > 0",
            limit,
        )
    }

    /// Up to `limit` files that were online-only (content not on this PC)
    /// when last seen. Scans the table until it has them: for rare checks.
    pub fn local_online_only_sample(&self, limit: usize) -> Result<Vec<u128>> {
        let ids: Vec<Vec<u8>> = self
            .c
            .prepare_cached("SELECT file_id FROM local_item WHERE kind = 0 AND (attributes & 4194304) != 0 LIMIT ?1")?
            .query_map([limit as i64], |r| r.get(0))?
            .collect::<std::result::Result<_, _>>()?;
        Ok(ids.iter().map(|b| id_from_blob(b)).collect())
    }

    /// SQLite's query plan for `sql` (tests prove index use with it).
    #[cfg(test)]
    pub(crate) fn conn_explain(&self, sql: &str) -> Result<String> {
        let mut st = self.c.prepare(&format!("EXPLAIN QUERY PLAN {sql}"))?;
        let rows = st.query_map([], |r| r.get::<_, String>(3))?;
        Ok(rows.collect::<std::result::Result<Vec<_>, _>>()?.join("; "))
    }

    fn rows_where(&self, cond: &str, limit: usize) -> Result<Vec<LocalRow>> {
        let ids: Vec<Vec<u8>> = self
            .c
            .prepare_cached(&format!("SELECT file_id FROM local_item WHERE {cond} LIMIT ?1"))?
            .query_map([limit as i64], |r| r.get(0))?
            .collect::<std::result::Result<_, _>>()?;
        let mut out = Vec::with_capacity(ids.len());
        for id in ids {
            if let Some(row) = self.local_get(id_from_blob(&id))? {
                out.push(row);
            }
        }
        Ok(out)
    }

    /// The Local tree's `.oneupignore` files (ordinary-looking rows named
    /// so; the reader still checks each file before reading it).
    pub fn local_rule_files(&self) -> Result<Vec<LocalRow>> {
        let ids: Vec<Vec<u8>> = self
            .c
            .prepare_cached("SELECT file_id FROM local_rule_file")?
            .query_map([], |r| r.get(0))?
            .collect::<std::result::Result<_, _>>()?;
        let mut out = Vec::with_capacity(ids.len());
        for id in ids {
            if let Some(row) = self.local_get(id_from_blob(&id))?
                && row.kind == LocalKind::File
                && row.name.eq_ignore_ascii_case(".oneupignore")
            {
                out.push(row);
            }
        }
        Ok(out)
    }

    /// Whether `file_id` is listed (for tests).
    pub fn is_rule_file(&self, file_id: u128) -> Result<bool> {
        Ok(self.c.prepare_cached("SELECT 1 FROM local_rule_file WHERE file_id = ?1")?.exists([blob(file_id)])?)
    }
}

#[cfg(test)]
mod tests {
    use crate::synced::tests::lrow;
    use crate::{LocalKind, Store};

    #[test]
    fn rule_files_are_tracked_through_every_write() {
        let mut s = Store::open_in_memory().unwrap();
        s.write(|o| {
            o.local_put(&lrow(1, None, "", LocalKind::Root))?;
            o.local_put(&lrow(2, Some(1), ".OneUpIgnore", LocalKind::File))?;
            o.local_put(&lrow(3, Some(1), "notes.txt", LocalKind::File))?;
            o.local_put(&lrow(4, Some(1), ".oneupignore", LocalKind::Folder))?;
            Ok(())
        })
        .unwrap();
        let ids = |s: &Store| -> Vec<u128> {
            let mut v: Vec<u128> = s.read(|o| o.local_rule_files()).unwrap().iter().map(|r| r.file_id).collect();
            v.sort_unstable();
            v
        };
        assert_eq!(ids(&s), [2], "a folder of that name isn't a rule file");
        s.write(|o| {
            o.local_update(&lrow(2, Some(1), "old-rules.txt", LocalKind::File))?;
            o.local_update(&lrow(3, Some(1), ".oneupignore", LocalKind::File))
        })
        .unwrap();
        assert_eq!(ids(&s), [3]);
        assert!(!s.read(|o| o.is_rule_file(2)).unwrap());
        s.write(|o| o.local_remove_subtree(3).map(|_| ())).unwrap();
        assert!(ids(&s).is_empty() && !s.read(|o| o.is_rule_file(3)).unwrap());
    }
}

#[cfg(test)]
mod placement_tests {
    use crate::synced::tests::lrow;
    use crate::{LocalKind, LocalRow, Store};

    #[test]
    fn placement_work_is_found_by_pin_and_recall_attributes() {
        let mut s = Store::open_in_memory().unwrap();
        let with = |id, attrs| LocalRow { attributes: attrs, ..lrow(id, Some(1), &format!("f{id}"), LocalKind::File) };
        s.write(|o| {
            o.local_put(&lrow(1, None, "", LocalKind::Root))?;
            o.local_put(&with(2, 0x0008_0000 | 0x0040_0000 | 0x400))?; // pinned, not here
            o.local_put(&with(3, 0x0008_0000 | 0x400))?; // pinned, here
            o.local_put(&with(4, 0x0010_0000 | 0x400))?; // unpinned, here
            o.local_put(&with(5, 0x0010_0000 | 0x0040_0000 | 0x400))?; // unpinned, freed
            o.local_put(&with(6, 0x20))?; // ordinary
            o.local_put(&LocalRow { size: 0, ..with(11, 0x0010_0000 | 0x400) })?; // unpinned, empty
            Ok(())
        })
        .unwrap();
        let ids = |v: Vec<LocalRow>| v.into_iter().map(|r| r.file_id).collect::<Vec<_>>();
        assert_eq!(ids(s.read(|o| o.local_to_fetch(10)).unwrap()), [2]);
        assert_eq!(ids(s.read(|o| o.local_to_free(10)).unwrap()), [4]);
        assert_eq!(s.read(|o| o.local_online_only_sample(10)).unwrap(), [2, 5], "not here, pinned or not");
        // Excluded folders to mark, and marks to undo once a folder syncs.
        s.write(|o| {
            let folder = |id, skip| LocalRow { skip, ..lrow(id, Some(1), &format!("d{id}"), LocalKind::Folder) };
            o.local_put(&folder(7, crate::Skip::Excluded))?;
            o.local_put(&LocalRow { attributes: 0x0018_0410, ..folder(8, crate::Skip::None) })?; // marked, listed
            o.local_put(&LocalRow { attributes: 0x0018_0410, ..folder(9, crate::Skip::Excluded) })?; // marked
            o.local_put(&folder(10, crate::Skip::None))
        })
        .unwrap();
        let mut excluded = ids(s.read(|o| o.local_excluded_folders(10)).unwrap());
        excluded.sort_unstable();
        assert_eq!(excluded, [7, 9]);
        assert_eq!(ids(s.read(|o| o.local_marked_but_synced(10)).unwrap()), [8]);
        let plan = s
            .read(|o| {
                o.conn_explain(
                    "SELECT file_id FROM local_item WHERE kind = 0 AND (attributes & 524288) != 0 AND (attributes & 4194304) != 0 LIMIT 1",
                )
            })
            .unwrap();
        assert!(plan.contains("local_to_fetch"), "uses the partial index: {plan}");
        let plan = s
            .read(|o| {
                o.conn_explain(
                    "SELECT file_id FROM local_item WHERE kind = 0 AND (attributes & 1048576) != 0 \
                     AND (attributes & 4194304) = 0 AND (attributes & 1024) != 0 AND size > 0 LIMIT 1",
                )
            })
            .unwrap();
        assert!(plan.contains("local_to_free"), "uses the partial index: {plan}");
    }
}
