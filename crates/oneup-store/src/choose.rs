//! Choosing what syncs (schema v19): what the file tree's checkboxes and
//! the rule suggestions read. Every query here is bounded by an index or a
//! bounded table, never a pass over the whole tree:
//!
//! * `remote_folder_size`: cloud folders by size. OneDrive gives a folder's
//!   size as the total of everything inside it, so the largest folders are
//!   the first rows of this index.
//! * the activity log ([`crate::activity::KEEP`] entries at most), newest
//!   first by its primary key, for the folders that changed most lately.
//! * one folder's subtree totals, for the preview of excluding it: as
//!   large as that folder, asked for when someone unticks it.
//! * which children of a listed folder hold a `.oneupignore` at any depth:
//!   up from the rule files (`local_rule_file`) by the primary key, as far
//!   as the listed folder, never down through the folders' contents.

use crate::local::{blob, id_from_blob};
use crate::{ITEM_COLS, Ops, RemoteRow, Result, fix_kind};

pub(crate) const MIGRATE_V19: &str = "
CREATE INDEX remote_folder_size ON remote_item(size) WHERE kind = 1;
";

/// Takes [`MIGRATE_V19`] back out, for tests that make an older database.
#[cfg(test)]
pub(crate) const UNDO_V19: &str = "DROP INDEX remote_folder_size;";

/// [`Ops::local_children_with_rule_files`] (its plan is checked by a test):
/// from each rule file's folder up its parents, one step at a time by the
/// primary key, stopping at a child of `?1`; `UNION` so folders shared by
/// several rule files' paths are walked once. `CROSS JOIN` keeps the rule
/// files first (left alone, SQLite reads every row of `local_item`).
const CHILDREN_WITH_RULE_FILES: &str = "WITH RECURSIVE up(id, parent) AS (
         SELECT d.file_id, d.parent_id FROM local_rule_file r
         CROSS JOIN local_item f ON f.file_id = r.file_id
         CROSS JOIN local_item d ON d.file_id = f.parent_id
         UNION
         SELECT p.file_id, p.parent_id FROM up JOIN local_item p ON p.file_id = up.parent
         WHERE up.parent IS NOT ?1)
     SELECT DISTINCT id FROM up WHERE parent = ?1";

/// [`Ops::remote_largest_folders_after`].
fn largest_folders_after_sql() -> String {
    format!(
        "SELECT {ITEM_COLS} FROM remote_item INDEXED BY remote_folder_size
         WHERE kind = 1 AND (size < ?1 OR (size = ?1 AND id < ?2)) ORDER BY size DESC, id DESC LIMIT ?3"
    )
}

/// [`Ops::remote_largest_folders`] (its plan is checked by a test).
fn largest_folders_sql() -> String {
    format!(
        "SELECT {ITEM_COLS} FROM remote_item INDEXED BY remote_folder_size WHERE kind = 1 ORDER BY size DESC LIMIT ?1"
    )
}

/// [`Ops::activity_done_since`].
const DONE_SINCE: &str = "SELECT path FROM activity WHERE outcome = 0 AND path != '' AND at_ms >= ?1
     ORDER BY seq DESC LIMIT ?2";

/// What's inside one folder on one side: files and their bytes, and of
/// those, the files whose content isn't on this PC (online-only).
/// `skipped`: folders inside stored as one row, their contents not listed
/// (left out by name, or not readable), so the counts are at least these.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct Totals {
    pub files: u64,
    pub folders: u64,
    pub bytes: u64,
    pub online_files: u64,
    pub online_bytes: u64,
    pub skipped: u64,
}

impl Ops<'_> {
    /// The `limit` largest cloud folders, largest first (their size: all
    /// that's inside them, as OneDrive reports it).
    pub fn remote_largest_folders(&self, limit: usize) -> Result<Vec<RemoteRow>> {
        let rows = self
            .c
            .prepare_cached(&largest_folders_sql())?
            .query_map([limit as i64], RemoteRow::from_row)?
            .collect::<rusqlite::Result<Vec<_>>>()?;
        rows.into_iter().map(fix_kind).collect()
    }

    /// [`Ops::remote_largest_folders`] continuing after `(size, id)`, the
    /// last row of the previous page: the index read in pages, so a caller
    /// filtering rows (to one pair's folder) reads as far as it needs and no
    /// further.
    pub fn remote_largest_folders_after(&self, after: Option<(i64, &str)>, limit: usize) -> Result<Vec<RemoteRow>> {
        let (size, id) = after.unwrap_or((i64::MAX, "\u{10ffff}"));
        let rows = self
            .c
            .prepare_cached(&largest_folders_after_sql())?
            .query_map(rusqlite::params![size, id, limit as i64], RemoteRow::from_row)?
            .collect::<rusqlite::Result<Vec<_>>>()?;
        rows.into_iter().map(fix_kind).collect()
    }

    /// The paths of the latest `limit` activity entries that did something
    /// (outcome done) since `since_ms`, newest first.
    pub fn activity_done_since(&self, since_ms: i64, limit: usize) -> Result<Vec<String>> {
        let rows = self
            .c
            .prepare_cached(DONE_SINCE)?
            .query_map(rusqlite::params![since_ms, limit as i64], |r| r.get(0))?
            .collect::<rusqlite::Result<Vec<String>>>()?;
        Ok(rows)
    }

    /// The folders directly in `parent` that hold a rule file (a
    /// `.oneupignore`) themselves or anywhere below. Costs the rule files'
    /// paths up to `parent`, never the folders' contents.
    pub fn local_children_with_rule_files(&self, parent: u128) -> Result<Vec<u128>> {
        let ids: Vec<Vec<u8>> = self
            .c
            .prepare_cached(CHILDREN_WITH_RULE_FILES)?
            .query_map([blob(parent)], |r| r.get(0))?
            .collect::<rusqlite::Result<_>>()?;
        Ok(ids.iter().map(|b| id_from_blob(b)).collect())
    }

    /// Everything inside the local folder `folder`, at any depth. One pass
    /// over that folder's subtree: for a preview someone asked for.
    pub fn local_subtree_totals(&self, folder: u128) -> Result<Totals> {
        type Row = (i64, i64, i64, i64, i64, i64);
        let (files, folders, bytes, online_files, online_bytes, skipped): Row = self.c.query_row(
            "WITH RECURSIVE t(id) AS (
                 SELECT ?1
                 UNION ALL
                 SELECT c.file_id FROM local_item c JOIN t ON c.parent_id = t.id WHERE c.kind = 1)
             SELECT COALESCE(SUM(f.kind = 0), 0), COALESCE(SUM(f.kind = 1), 0),
                    COALESCE(SUM(CASE WHEN f.kind = 0 THEN f.size END), 0),
                    COALESCE(SUM(f.kind = 0 AND (f.attributes & 4194304) != 0), 0),
                    COALESCE(SUM(CASE WHEN f.kind = 0 AND (f.attributes & 4194304) != 0 THEN f.size END), 0),
                    COALESCE(SUM(f.kind = 1 AND f.skip != 0), 0)
             FROM t JOIN local_item f ON f.parent_id = t.id",
            [blob(folder)],
            |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?, r.get(3)?, r.get(4)?, r.get(5)?)),
        )?;
        let n = |v: i64| u64::try_from(v).unwrap_or(0);
        Ok(Totals {
            files: n(files),
            folders: n(folders),
            bytes: n(bytes),
            online_files: n(online_files),
            online_bytes: n(online_bytes),
            skipped: n(skipped),
        })
    }

    /// Everything inside the cloud folder `folder`, at any depth (packages
    /// count as files). Online-only doesn't apply. A folder stored as one
    /// row (left out by name) counts with the size OneDrive gives for all
    /// that's inside it, its files unknown (`skipped`).
    pub fn remote_subtree_totals(&self, folder: &str) -> Result<Totals> {
        let (files, folders, bytes, skipped): (i64, i64, i64, i64) = self.c.query_row(
            "WITH RECURSIVE t(id) AS (
                 SELECT ?1
                 UNION ALL
                 SELECT c.id FROM remote_item c JOIN t ON c.parent_id = t.id WHERE c.kind = 1 AND c.is_excluded = 0)
             SELECT COALESCE(SUM(f.kind IN (0, 3)), 0), COALESCE(SUM(f.kind = 1), 0),
                    COALESCE(SUM(CASE WHEN f.kind IN (0, 3) OR (f.kind = 1 AND f.is_excluded != 0) THEN f.size END), 0),
                    COALESCE(SUM(f.kind = 1 AND f.is_excluded != 0), 0)
             FROM t JOIN remote_item f ON f.parent_id = t.id",
            [folder],
            |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?, r.get(3)?)),
        )?;
        let n = |v: i64| u64::try_from(v).unwrap_or(0);
        Ok(Totals { files: n(files), folders: n(folders), bytes: n(bytes), skipped: n(skipped), ..Totals::default() })
    }
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used)]
    use super::*;
    use crate::{ActivityEntry, ActivityOutcome, Kind, Store};

    fn plan(s: &Store, sql: &str) -> String {
        let mut st = s.conn.prepare(&format!("EXPLAIN QUERY PLAN {sql}")).unwrap();
        let n = st.parameter_count();
        let params: Vec<rusqlite::types::Null> = vec![rusqlite::types::Null; n];
        let rows = st.query_map(rusqlite::params_from_iter(params), |r| r.get::<_, String>(3)).unwrap();
        rows.map(|r| r.unwrap()).collect::<Vec<_>>().join("; ")
    }

    /// The suggestions read the first rows of an index or the bounded
    /// activity log by its key: never the whole Remote tree, never a sort.
    #[test]
    fn suggestion_queries_are_bounded_by_an_index() {
        let s = Store::open_in_memory().unwrap();
        let p = plan(&s, &largest_folders_sql());
        assert!(p.contains("USING INDEX remote_folder_size"), "{p}");
        assert!(!p.contains("TEMP B-TREE"), "no sort: {p}");
        let p = plan(&s, &largest_folders_after_sql());
        assert!(p.contains("USING INDEX remote_folder_size") && !p.contains("TEMP B-TREE"), "pages of the index: {p}");
        // Up from the rule files by the key: no pass over local_item.
        let p = plan(&s, CHILDREN_WITH_RULE_FILES);
        assert!(!p.contains("SCAN local_item") && !p.contains("SCAN f") && !p.contains("SCAN d"), "{p}");
        assert!(!p.contains("SCAN p"), "{p}");
        let p = plan(&s, DONE_SINCE);
        assert!(!p.contains("TEMP B-TREE"), "newest first by its key: {p}");
        assert!(p.contains("activity"), "{p}");
    }

    #[test]
    fn the_largest_folders_come_first_and_only_folders() {
        let mut s = Store::open_in_memory().unwrap();
        let row = |id: &str, kind, size| RemoteRow {
            id: id.into(),
            parent_id: Some("root".into()),
            name: id.into(),
            kind,
            size,
            etag: None,
            qxh: None,
            mtime_ms: None,
        };
        s.write(|o| {
            o.upsert_item(&RemoteRow { parent_id: None, ..row("root", Kind::Root, 900) }, false)?;
            o.upsert_item(&row("small", Kind::Folder, 10), false)?;
            o.upsert_item(&row("big", Kind::Folder, 500), false)?;
            o.upsert_item(&row("file", Kind::File, 800), false)?;
            o.upsert_item(&row("mid", Kind::Folder, 100), false)
        })
        .unwrap();
        let got: Vec<String> = s.read(|o| o.remote_largest_folders(2)).unwrap().into_iter().map(|r| r.id).collect();
        assert_eq!(got, ["big", "mid"]);
    }

    #[test]
    fn largest_folders_come_in_pages() {
        let mut s = Store::open_in_memory().unwrap();
        let row = |id: &str, size| RemoteRow {
            id: id.into(),
            parent_id: Some("root".into()),
            name: id.into(),
            kind: Kind::Folder,
            size,
            etag: None,
            qxh: None,
            mtime_ms: None,
        };
        s.write(|o| {
            for (id, size) in [("a", 50), ("b", 40), ("c", 40), ("d", 10)] {
                o.upsert_item(&row(id, size), false)?;
            }
            Ok(())
        })
        .unwrap();
        let ids = |v: Vec<RemoteRow>| v.into_iter().map(|r| r.id).collect::<Vec<_>>();
        let first = s.read(|o| o.remote_largest_folders_after(None, 2)).unwrap();
        assert_eq!(ids(first.clone()), ["a", "c"]);
        let last = first.last().unwrap();
        let next = s.read(|o| o.remote_largest_folders_after(Some((last.size, &last.id)), 5)).unwrap();
        assert_eq!(ids(next), ["b", "d"]);
    }

    #[test]
    fn done_entries_since_a_time_newest_first() {
        let mut s = Store::open_in_memory().unwrap();
        let e = |at_ms, outcome, path: &str| ActivityEntry {
            seq: 0,
            at_ms,
            outcome,
            action: "upload".into(),
            path: path.into(),
            detail: String::new(),
        };
        s.write(|o| {
            o.activity_add(&[
                e(1, ActivityOutcome::Done, "/old"),
                e(10, ActivityOutcome::Done, "/a"),
                e(11, ActivityOutcome::Failed, "/failed"),
                e(12, ActivityOutcome::Note, ""),
                e(13, ActivityOutcome::Done, "/b"),
            ])
        })
        .unwrap();
        assert_eq!(s.read(|o| o.activity_done_since(5, 100)).unwrap(), ["/b", "/a"]);
        assert_eq!(s.read(|o| o.activity_done_since(5, 1)).unwrap(), ["/b"]);
    }

    #[test]
    fn subtree_totals_count_everything_inside_one_folder() {
        use crate::{LocalKind, LocalRoot, LocalRow, Skip};
        let mut s = Store::open_in_memory().unwrap();
        let row = |id: u128, parent: u128, kind, size, attributes| LocalRow {
            file_id: id,
            parent_id: Some(parent),
            name: format!("n{id}"),
            name_lossy: false,
            kind,
            size,
            mtime: 0,
            attributes,
            reparse_tag: 0,
            skip: Skip::None,
        };
        s.write(|o| {
            let root = LocalRow { parent_id: None, ..row(1, 0, LocalKind::Root, 0, 0x10) };
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
            o.local_put(&row(2, 1, LocalKind::Folder, 0, 0x10))?;
            o.local_put(&row(3, 2, LocalKind::File, 5, 0))?;
            o.local_put(&row(4, 2, LocalKind::Folder, 0, 0x10))?;
            o.local_put(&row(5, 4, LocalKind::File, 7, 0x0040_0000))?;
            o.local_put(&row(6, 1, LocalKind::File, 100, 0))?;
            o.local_put(&LocalRow { skip: Skip::Excluded, ..row(7, 2, LocalKind::Folder, 0, 0x10) })?;
            Ok(())
        })
        .unwrap();
        let t = s.read(|o| o.local_subtree_totals(2)).unwrap();
        assert_eq!(t, Totals { files: 2, folders: 2, bytes: 12, online_files: 1, online_bytes: 7, skipped: 1 });
        // A rule file in 4 (inside 2): 2 holds one, 6 (a file) and 7 don't.
        s.write(|o| {
            o.local_put(&row(8, 4, LocalKind::File, 1, 0))?;
            o.local_update(&LocalRow { name: ".oneupignore".into(), ..row(8, 4, LocalKind::File, 1, 0) })
        })
        .unwrap();
        assert_eq!(s.read(|o| o.local_children_with_rule_files(1)).unwrap(), [2]);
        assert_eq!(s.read(|o| o.local_children_with_rule_files(2)).unwrap(), [4]);
        assert!(
            s.read(|o| o.local_children_with_rule_files(4)).unwrap().is_empty(),
            "the file itself isn't a folder in 4"
        );
    }

    /// A folder left out by name is one row in OneDrive's listing: it
    /// counts with OneDrive's size for everything inside it.
    #[test]
    fn a_folder_left_out_by_name_counts_with_onedrives_size() {
        let mut s = Store::open_in_memory().unwrap();
        let row = |id: &str, parent: &str, kind, size| RemoteRow {
            id: id.into(),
            parent_id: Some(parent.into()),
            name: id.into(),
            kind,
            size,
            etag: None,
            qxh: None,
            mtime_ms: None,
        };
        s.write(|o| {
            o.upsert_item(&row("top", "root", Kind::Folder, 0), false)?;
            o.upsert_item(&row("f", "top", Kind::File, 5), false)?;
            o.upsert_item(&row("node_modules", "top", Kind::Folder, 1000), true)
        })
        .unwrap();
        let t = s.read(|o| o.remote_subtree_totals("top")).unwrap();
        assert_eq!((t.files, t.bytes, t.skipped), (1, 1005, 1));
    }
}
