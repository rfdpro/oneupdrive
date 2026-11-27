//! Optional file-name catalog for an isolated, read-only volume scan.

use rusqlite::params;

use crate::{Ops, Result, Store};

/// The triggers queue a changed row only if it isn't queued already, rather
/// than `INSERT OR IGNORE`: SQLite lets the outer statement's conflict
/// handling override a trigger's, and the Local tree's upserts (`ON
/// CONFLICT DO UPDATE`) turned the ignore into a constraint error whenever
/// a queued row changed again before the queue was flushed (a rename the
/// journal reports during a scan). They're made again each time a catalog
/// is opened, so catalogs made with the earlier triggers get these.
const SCHEMA: &str = "
CREATE TABLE IF NOT EXISTS drive_dirty(file_id BLOB PRIMARY KEY) WITHOUT ROWID;
CREATE TABLE IF NOT EXISTS drive_key(seq INTEGER PRIMARY KEY, file_id BLOB UNIQUE NOT NULL);
CREATE VIRTUAL TABLE IF NOT EXISTS drive_name USING fts5(name, content='', contentless_delete=1, detail=none, tokenize='trigram');
DROP TRIGGER IF EXISTS drive_insert;
DROP TRIGGER IF EXISTS drive_rename;
DROP TRIGGER IF EXISTS drive_delete;
CREATE TRIGGER drive_insert AFTER INSERT ON local_item WHEN NEW.kind IN (0, 1)
BEGIN INSERT INTO drive_dirty SELECT NEW.file_id
    WHERE NOT EXISTS (SELECT 1 FROM drive_dirty WHERE file_id = NEW.file_id); END;
CREATE TRIGGER drive_rename AFTER UPDATE OF name,kind,name_lossy,skip ON local_item
WHEN OLD.name IS NOT NEW.name OR OLD.kind IS NOT NEW.kind OR OLD.name_lossy IS NOT NEW.name_lossy
    OR OLD.skip IS NOT NEW.skip
BEGIN INSERT INTO drive_dirty SELECT NEW.file_id
    WHERE NOT EXISTS (SELECT 1 FROM drive_dirty WHERE file_id = NEW.file_id); END;
CREATE TRIGGER drive_delete AFTER DELETE ON local_item WHEN OLD.kind IN (0, 1)
BEGIN INSERT INTO drive_dirty SELECT OLD.file_id
    WHERE NOT EXISTS (SELECT 1 FROM drive_dirty WHERE file_id = OLD.file_id); END;";

impl Store {
    /// Install the catalog before scanning. Ordinary sync stores never use these triggers.
    pub fn install_drive_search(&mut self) -> Result<()> {
        self.write(|o| {
            o.c.execute_batch(SCHEMA)?;
            Ok(())
        })
    }

    /// Apply one bounded batch. The queue survives an interruption after the scan commits.
    pub fn flush_drive_search(&mut self, limit: usize) -> Result<usize> {
        self.write(|o| {
            // Resolve the entire batch in SQLite. The first build feeds the
            // trigram index without crossing the SQLite API for every name.
            o.c.execute_batch(
                "CREATE TEMP TABLE IF NOT EXISTS drive_batch (
                    file_id BLOB PRIMARY KEY, old_seq INTEGER, name TEXT
                ) WITHOUT ROWID;",
            )?;
            let count = o.c.prepare_cached(
                "INSERT INTO drive_batch
                 SELECT d.file_id,k.seq,l.name FROM drive_dirty d
                 LEFT JOIN drive_key k ON k.file_id=d.file_id
                 LEFT JOIN local_item l ON l.file_id=d.file_id AND l.kind IN (0,1) AND l.name_lossy=0 AND l.skip=0
                 ORDER BY d.file_id LIMIT ?1",
            )?.execute([limit as i64])?;
            if count != 0 {
                o.c.execute_batch(
                    "DELETE FROM drive_name WHERE rowid IN (SELECT old_seq FROM drive_batch WHERE old_seq IS NOT NULL);
                     DELETE FROM drive_key WHERE seq IN (SELECT old_seq FROM drive_batch WHERE name IS NULL);
                     INSERT INTO drive_key(file_id) SELECT file_id FROM drive_batch WHERE old_seq IS NULL AND name IS NOT NULL;
                     INSERT INTO drive_name(rowid,name)
                        SELECT k.seq,b.name FROM drive_batch b CROSS JOIN drive_key k ON k.file_id=b.file_id
                        WHERE b.name IS NOT NULL ORDER BY k.seq;
                     DELETE FROM drive_dirty WHERE file_id IN (SELECT file_id FROM drive_batch);
                     DELETE FROM drive_batch;",
                )?;
            }
            Ok(count)
        })
    }
}

impl Ops<'_> {
    /// Fetch candidate IDs from the trigram index and check the current name before emitting.
    /// A visitor can stop immediately after filling its page.
    pub fn visit_drive_names(
        &self,
        text: &str,
        words: bool,
        folder: Option<bool>,
        mut visit: impl FnMut(crate::LocalRow) -> Result<bool>,
    ) -> Result<bool> {
        let terms = if words { text.split_whitespace().collect::<Vec<_>>() } else { vec![text] };
        let Some(expr) = crate::search::match_terms(terms.iter().copied()) else { return Ok(false) };
        let terms: Vec<_> = terms.into_iter().map(oneup_sync::fold).collect();
        let mut st = self.c.prepare_cached(
            "SELECT l.* FROM drive_name n CROSS JOIN drive_key k ON k.seq=n.rowid
             CROSS JOIN local_item l ON l.file_id=k.file_id
             WHERE drive_name MATCH ?1 AND l.kind IN (0,1) AND l.skip=0 AND l.name_lossy=0
             AND (?2 IS NULL OR (l.kind=1)=?2) ORDER BY n.rowid DESC LIMIT 20001",
        )?;
        let mut rows = st.query(params![expr, folder])?;
        let mut seen = 0;
        while let Some(row) = rows.next()? {
            seen += 1;
            if seen > 20_000 {
                return Ok(true);
            }
            let name = oneup_sync::fold(&row.get::<_, String>("name")?);
            if terms.iter().all(|term| name.contains(term))
                && !visit(crate::local::fix_local(crate::LocalRow::from_row(self.keys, row)?)?)?
            {
                break;
            }
        }
        Ok(false)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{LocalKind, LocalRoot, LocalRow, Skip};

    #[test]
    fn catalog_tracks_128_bit_ids_through_rename_and_removal() {
        let mut store = Store::open_in_memory().unwrap();
        store.install_drive_search().unwrap();
        let root = LocalRoot {
            path: "C:/fixture".into(),
            volume_serial: 1,
            root_file_id: 1,
            rules: String::new(),
            journal: None,
            started_at_ms: 0,
            finished_at_ms: None,
            sync_root_provider: None,
        };
        let mut row = LocalRow {
            file_id: 1,
            parent_id: None,
            name: String::new(),
            name_lossy: false,
            kind: LocalKind::Root,
            size: 0,
            mtime: 0,
            attributes: 0,
            reparse_tag: 0,
            skip: Skip::None,
        };
        store.write(|o| o.local_begin(&root, &row)).unwrap();
        row.file_id = u128::MAX - 4;
        row.parent_id = Some(1);
        row.kind = LocalKind::File;
        row.name = "Invoice-24.pdf".into();
        store.write(|o| o.local_put(&row)).unwrap();
        assert_eq!(store.flush_drive_search(2).unwrap(), 1);
        let query = |store: &Store, needle: &str, words| {
            let mut found = Vec::new();
            store
                .read(|o| {
                    o.visit_drive_names(needle, words, Some(false), |row| {
                        found.push(row.file_id);
                        Ok(true)
                    })
                })
                .unwrap();
            found
        };
        assert_eq!(query(&store, "PDF invoice 24", true), [row.file_id]);
        row.name = "Receipt-24.pdf".into();
        store.write(|o| o.local_update(&row)).unwrap();
        assert!(query(&store, "invoice", false).is_empty(), "an unflushed rename cannot return an old name");
        assert_eq!(store.flush_drive_search(1).unwrap(), 1);
        assert_eq!(query(&store, "receipt", false), [row.file_id]);
        store.write(|o| o.local_remove_subtree(row.file_id)).unwrap();
        assert_eq!(store.flush_drive_search(2).unwrap(), 1);
        assert!(query(&store, "receipt", false).is_empty());
        assert_eq!(store.flush_drive_search(2).unwrap(), 0);
        assert!(store.read(|o| o.remote_root()).unwrap().is_none());
    }

    /// A row the queue holds already can change again before a flush: a
    /// scan queues every row it lists, and following the journal renames
    /// some of them. The upserts must not turn that into a constraint error.
    #[test]
    fn a_queued_row_changes_again_before_the_flush() {
        let mut store = Store::open_in_memory().unwrap();
        // As made by an earlier build: installing replaces its triggers.
        store
            .write(|o| {
                o.c.execute_batch(
                    "CREATE TABLE drive_dirty(file_id BLOB PRIMARY KEY) WITHOUT ROWID;
                     CREATE TRIGGER drive_rename AFTER UPDATE OF name ON local_item
                     BEGIN INSERT OR IGNORE INTO drive_dirty VALUES (NEW.file_id); END;",
                )?;
                Ok(())
            })
            .unwrap();
        store.install_drive_search().unwrap();
        let mut row = LocalRow {
            file_id: 7,
            parent_id: Some(1),
            name: "Draft.txt".into(),
            name_lossy: false,
            kind: LocalKind::File,
            size: 0,
            mtime: 0,
            attributes: 0,
            reparse_tag: 0,
            skip: Skip::None,
        };
        store.write(|o| o.local_put(&row).map(|_| ())).unwrap();
        for name in ["Report.txt", "Final report.txt"] {
            row.name = name.into();
            store.write(|o| o.local_put(&row).map(|_| ()).and_then(|()| o.local_update(&row))).unwrap();
        }
        store.write(|o| o.local_remove_subtree(7).map(|_| ())).unwrap();
        row.name = "Back.txt".into();
        store.write(|o| o.local_update(&row)).unwrap();
        assert_eq!(store.flush_drive_search(8).unwrap(), 1, "one queued row, whatever happened to it");
    }
}
