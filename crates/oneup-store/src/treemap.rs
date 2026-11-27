//! Bounded readings of the database's size metadata for the Files treemap.

use rusqlite::{Row, params};

use crate::{Ops, Result};

/// The names and sizes the map can display, with the total including smaller items.
pub struct Level<I> {
    pub bytes: u64,
    pub items: Vec<Item<I>>,
}

pub struct Item<I> {
    pub id: I,
    pub name: String,
    pub bytes: u64,
    pub folder: bool,
    pub expandable: bool,
}

impl Ops<'_> {
    /// Fully resident files below a folder, grouped by its immediate children.
    /// Each folder and file is visited once, only on request. The grouping
    /// stays in SQLite and only the largest entries reach the window.
    pub fn local_treemap(&self, parent: u128, limit: usize) -> Result<Level<u128>> {
        const RECALL: i64 = 0x0044_1000; // RECALL_ON_DATA_ACCESS | RECALL_ON_OPEN | OFFLINE
        let mut query = self.c.prepare_cached(
            "WITH RECURSIVE
               tops AS MATERIALIZED (
                   SELECT file_id, name, kind, size, attributes FROM local_item
                   WHERE parent_id = ?1 AND kind IN (0, 1)),
               dirs(top, id) AS (
                   SELECT file_id, file_id FROM tops WHERE kind = 1
                   UNION ALL
                   SELECT d.top, c.file_id FROM dirs d JOIN local_item c ON c.parent_id = d.id WHERE c.kind = 1),
               totals AS (
                   SELECT d.top, SUM(f.size) AS bytes
                   FROM dirs d JOIN local_item f ON f.parent_id = d.id
                   WHERE f.kind = 0 AND (f.attributes & ?3) = 0 GROUP BY d.top),
               weights AS (
                   SELECT t.file_id AS id, t.name, t.kind,
                          CASE WHEN t.kind = 1 THEN COALESCE(s.bytes, 0)
                               WHEN (t.attributes & ?3) = 0 THEN t.size ELSE 0 END AS bytes
                   FROM tops t LEFT JOIN totals s ON s.top = t.file_id)
             SELECT id, name, kind, bytes, kind = 1, SUM(bytes) OVER ()
             FROM weights WHERE bytes > 0 ORDER BY bytes DESC, id LIMIT ?2",
        )?;
        let mut rows = query.query(params![self.keys.blob(parent), limit as i64, RECALL])?;
        to_level(&mut rows, |row| Ok(self.keys.id(&row.get::<_, Vec<u8>>(0)?)))
    }

    /// OneDrive stores a folder's reported size on its row. Use it directly,
    /// including for excluded folders whose descendants aren't mirrored.
    pub fn remote_treemap(&self, parent: &str, limit: usize) -> Result<Level<String>> {
        let mut query = self.c.prepare_cached(
            "SELECT p.id, p.name, p.kind, p.size,
                    p.kind = 1 AND EXISTS (SELECT 1 FROM remote_item WHERE parent_id = p.id AND size > 0),
                    p.total
             FROM (
                 SELECT id, name, kind, size, SUM(size) OVER () AS total
                 FROM remote_item WHERE parent_id = ?1 AND size > 0
                 ORDER BY size DESC, id LIMIT ?2
             ) p ORDER BY p.size DESC, p.id",
        )?;
        let mut rows = query.query(params![parent, limit as i64])?;
        to_level(&mut rows, |row| row.get(0))
    }
}

fn to_level<I>(rows: &mut rusqlite::Rows<'_>, id: impl Fn(&Row<'_>) -> rusqlite::Result<I>) -> Result<Level<I>> {
    let mut level = Level { bytes: 0, items: Vec::new() };
    while let Some(row) = rows.next()? {
        level.bytes = u64::try_from(row.get::<_, i64>(5)?).unwrap_or_default();
        level.items.push(Item {
            id: id(row)?,
            name: row.get(1)?,
            folder: row.get::<_, i64>(2)? == 1,
            bytes: u64::try_from(row.get::<_, i64>(3)?).unwrap_or_default(),
            expandable: row.get(4)?,
        });
    }
    Ok(level)
}

#[cfg(test)]
mod tests {
    use crate::{Kind, LocalKind, LocalRow, RemoteRow, Skip, Store};

    #[test]
    fn the_disk_map_counts_only_resident_content_and_keeps_the_omitted_total() {
        let mut store = Store::open_in_memory().unwrap();
        store
            .write(|o| {
                for (id, parent_id, kind, size, attributes) in [
                    (1, None, LocalKind::Root, 0, 0),
                    (2, Some(1), LocalKind::Folder, 0, 0),
                    (3, Some(2), LocalKind::Folder, 0, 0),
                    (4, Some(3), LocalKind::File, 9, 0),
                    (5, Some(2), LocalKind::File, 3, 0),
                    (6, Some(2), LocalKind::File, 800, 0x0040_0000),
                    (7, Some(1), LocalKind::File, 700, 0x0004_0000),
                    (8, Some(1), LocalKind::File, 600, 0x0000_1000),
                    (9, Some(1), LocalKind::File, 5, 0),
                    (10, Some(1), LocalKind::Link, 100, 0),
                ] {
                    o.local_put(&LocalRow {
                        file_id: id,
                        parent_id,
                        name: id.to_string(),
                        name_lossy: false,
                        kind,
                        size,
                        attributes,
                        mtime: 0,
                        reparse_tag: 0,
                        skip: Skip::None,
                    })?;
                }
                Ok(())
            })
            .unwrap();

        let root = store.read(|o| o.local_treemap(1, 1)).unwrap();
        assert_eq!(root.bytes, 17);
        assert_eq!(root.items.len(), 1);
        assert_eq!((root.items[0].id, root.items[0].bytes, root.items[0].expandable), (2, 12, true));
        let nested = store.read(|o| o.local_treemap(2, 8)).unwrap();
        assert_eq!(nested.bytes, 12);
        assert_eq!(nested.items.iter().map(|i| (i.id, i.bytes)).collect::<Vec<_>>(), [(3, 9), (5, 3)]);
    }

    #[test]
    fn the_drive_map_uses_reported_folder_sizes_with_or_without_children() {
        let mut store = Store::open_in_memory().unwrap();
        store
            .write(|o| {
                for (id, parent, kind, size, excluded) in [
                    ("root", None, Kind::Root, 0, false),
                    ("a", Some("root"), Kind::Folder, 5000, true),
                    ("b", Some("root"), Kind::Folder, 2000, false),
                    ("c", Some("b"), Kind::File, 1500, false),
                    ("d", Some("root"), Kind::File, 1300, false),
                ] {
                    o.upsert_item(
                        &RemoteRow {
                            id: id.into(),
                            parent_id: parent.map(str::to_owned),
                            name: id.into(),
                            kind,
                            size,
                            etag: None,
                            qxh: None,
                            mtime_ms: None,
                        },
                        excluded,
                    )?;
                }
                Ok(())
            })
            .unwrap();

        let root = store.read(|o| o.remote_treemap("root", 2)).unwrap();
        assert_eq!(root.bytes, 8300);
        assert_eq!(root.items.len(), 2);
        assert_eq!((root.items[0].bytes, root.items[0].expandable), (5000, false));
        assert_eq!((root.items[1].bytes, root.items[1].expandable), (2000, true));
    }
}
