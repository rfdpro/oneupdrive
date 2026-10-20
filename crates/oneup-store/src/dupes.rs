//! Duplicates: files with the same content (size and QuickXorHash).
//!
//! Cloud files come from the cloud files by size index
//! (`remote_item_copyable`, schema v9: files of at least [`COPY_MIN`] with a
//! hash), so grouping reads that index once and never the whole Remote
//! tree, and a group's copies are one search of it by size. Files on this
//! PC that aren't in OneDrive are hashed only when the user asks
//! (`local_only_hash`, schema v20), never by sync itself; such a row counts
//! only while the Local tree still has the file with the size and
//! last-write time it was hashed at, and no Synced record links it (once
//! synced, its cloud copy stands for it).
//!
//! A synced file is one copy: its cloud row. Groups are ordered by the space
//! removing all but one copy would free, largest first, and read a page at a
//! time ([`Ops::dup_groups`]).

use oneup_sync::Hash;
use rusqlite::{OptionalExtension as _, params};

use crate::local::{blob, id_from_blob};
use crate::{COPY_MIN, ITEM_COLS, LocalRow, Ops, RemoteRow, Result, fix_kind};

/// v19 -> v20: hashes of files on this PC that aren't in OneDrive, made on
/// request for finding duplicates (see the module docs). It touches only
/// its own table and triggers. Bounded by the
/// caller ([`LOCAL_ONLY_MAX`]). Triggers drop a file's hash when the Local
/// tree sees it anew, with another size, time or kind, or gone, and every
/// hash when the synced folder is scanned again or is another one: a hash
/// stands for one file at one stamp, never a later file that took its ID.
/// Queries check the stamp as well.
pub(crate) const MIGRATE_V20: &str = "
CREATE TABLE local_only_hash (
    file_id BLOB PRIMARY KEY,
    size    INTEGER NOT NULL,
    mtime   INTEGER NOT NULL,
    qxh     BLOB NOT NULL
) WITHOUT ROWID;
CREATE INDEX local_only_hash_content ON local_only_hash(size, qxh);
CREATE TRIGGER local_only_hash_new AFTER INSERT ON local_item
BEGIN
    DELETE FROM local_only_hash WHERE file_id = NEW.file_id;
END;
CREATE TRIGGER local_only_hash_changed AFTER UPDATE ON local_item
WHEN OLD.size IS NOT NEW.size OR OLD.mtime IS NOT NEW.mtime OR OLD.kind IS NOT NEW.kind
BEGIN
    DELETE FROM local_only_hash WHERE file_id = OLD.file_id;
END;
CREATE TRIGGER local_only_hash_gone AFTER DELETE ON local_item
BEGIN
    DELETE FROM local_only_hash WHERE file_id = OLD.file_id;
END;
CREATE TRIGGER local_only_hash_root_new AFTER INSERT ON local_root
BEGIN
    DELETE FROM local_only_hash;
END;
CREATE TRIGGER local_only_hash_root_moved AFTER UPDATE ON local_root
WHEN OLD.path IS NOT NEW.path OR OLD.volume_serial IS NOT NEW.volume_serial OR OLD.root_file_id IS NOT NEW.root_file_id
BEGIN
    DELETE FROM local_only_hash;
END;
CREATE TRIGGER local_only_hash_root_gone AFTER DELETE ON local_root
BEGIN
    DELETE FROM local_only_hash;
END;
";

/// What [`MIGRATE_V20`] adds, removed: for tests that make an older store.
#[cfg(test)]
pub(crate) const UNDO_V20: &str = "
DROP TRIGGER local_only_hash_new; DROP TRIGGER local_only_hash_changed; DROP TRIGGER local_only_hash_gone;
DROP TRIGGER local_only_hash_root_new; DROP TRIGGER local_only_hash_root_moved;
DROP TRIGGER local_only_hash_root_gone; DROP TABLE local_only_hash;
";

/// At most this many local-only hashes are kept: the table stays small
/// enough to read whole when grouping.
pub const LOCAL_ONLY_MAX: i64 = 100_000;

/// Every copy: the indexed cloud files, and the local-only files whose hash
/// still stands. The literal sizes and `kind` repeat the partial index's
/// condition, which is what lets SQLite use it.
const COPIES: &str = "
copies(size, qxh) AS (
    SELECT size, qxh FROM remote_item INDEXED BY remote_item_copyable
     WHERE kind = 0 AND size >= 1048576 AND qxh IS NOT NULL
    UNION ALL
    SELECT h.size, h.qxh FROM local_only_hash h
      JOIN local_item i ON i.file_id = h.file_id AND i.size = h.size AND i.mtime = h.mtime
     WHERE NOT EXISTS (SELECT 1 FROM synced_item s WHERE s.local_id = h.file_id)),
groups(size, qxh, n) AS (
    SELECT size, qxh, count(*) FROM copies GROUP BY size, qxh HAVING count(*) > 1)";

pub(crate) fn groups_sql() -> String {
    format!(
        "WITH {COPIES}
         SELECT size, qxh, n FROM groups
          WHERE ?1 IS NULL OR ((n - 1) * size, size, qxh) < (?1, ?2, ?3)
          ORDER BY (n - 1) * size DESC, size DESC, qxh DESC LIMIT ?4"
    )
}

pub(crate) fn summary_sql() -> String {
    format!("WITH {COPIES} SELECT count(*), coalesce(sum(n - 1), 0), coalesce(sum((n - 1) * size), 0) FROM groups")
}

pub(crate) fn cloud_copies_sql() -> String {
    format!(
        "SELECT {ITEM_COLS} FROM remote_item INDEXED BY remote_item_copyable
          WHERE kind = 0 AND size >= 1048576 AND qxh IS NOT NULL AND size = ?1 AND qxh = ?2
          ORDER BY id LIMIT ?3"
    )
}

pub(crate) const LOCAL_COPIES: &str = "
SELECT h.file_id FROM local_only_hash h
  JOIN local_item i ON i.file_id = h.file_id AND i.size = h.size AND i.mtime = h.mtime
 WHERE h.size = ?1 AND h.qxh = ?2
   AND NOT EXISTS (SELECT 1 FROM synced_item s WHERE s.local_id = h.file_id)
 ORDER BY h.file_id LIMIT ?3";

/// Files of the Local tree from a file ID on, by ID: one pass over the
/// tree's primary key, a page at a time, only when the user asks.
pub(crate) const LOCAL_FILES_FROM: &str = "
SELECT file_id FROM local_item WHERE file_id > ?1 AND kind = 0 AND size >= ?2 ORDER BY file_id LIMIT ?3";

/// Content that more than one file has.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct DupGroup {
    pub size: i64,
    pub hash: Hash,
    /// How many copies (at least 2).
    pub copies: i64,
}

impl DupGroup {
    /// What removing all but one copy frees.
    pub fn saved(&self) -> i64 {
        (self.copies - 1).saturating_mul(self.size)
    }

    /// Where the next page starts ([`Ops::dup_groups`]).
    pub fn cursor(&self) -> DupCursor {
        DupCursor { saved: self.saved(), size: self.size, hash: self.hash }
    }
}

/// The last group of a page.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct DupCursor {
    pub saved: i64,
    pub size: i64,
    pub hash: Hash,
}

/// All groups together.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct DupSummary {
    pub groups: i64,
    /// Copies beyond the first of each group.
    pub files: i64,
    /// What removing them frees.
    pub bytes: i64,
}

fn hash_of(v: Vec<u8>) -> Option<Hash> {
    <[u8; 20]>::try_from(v.as_slice()).ok()
}

impl Ops<'_> {
    /// How many groups there are, and what removing every extra copy frees.
    pub fn dup_summary(&self) -> Result<DupSummary> {
        Ok(self
            .c
            .prepare_cached(&summary_sql())?
            .query_row([], |r| Ok(DupSummary { groups: r.get(0)?, files: r.get(1)?, bytes: r.get(2)? }))?)
    }

    /// Up to `limit` groups after `after` (`None`: from the first), most
    /// space saved first.
    pub fn dup_groups(&self, after: Option<DupCursor>, limit: usize) -> Result<Vec<DupGroup>> {
        let (saved, size, hash) = match after {
            Some(c) => (Some(c.saved), Some(c.size), Some(c.hash.to_vec())),
            None => (None, None, None),
        };
        let mut st = self.c.prepare_cached(&groups_sql())?;
        let rows = st.query_map(params![saved, size, hash, limit as i64], |r| {
            Ok((r.get::<_, i64>(0)?, r.get::<_, Vec<u8>>(1)?, r.get::<_, i64>(2)?))
        })?;
        let mut out = Vec::new();
        for row in rows {
            let (size, hash, copies) = row?;
            // The index holds 20-byte hashes only; anything else is skipped.
            if let Some(hash) = hash_of(hash) {
                out.push(DupGroup { size, hash, copies });
            }
        }
        Ok(out)
    }

    /// Up to `limit` cloud files with this content, by ID.
    pub fn dup_cloud_copies(&self, size: i64, hash: &Hash, limit: usize) -> Result<Vec<RemoteRow>> {
        if size < COPY_MIN {
            return Ok(Vec::new());
        }
        let rows = self
            .c
            .prepare_cached(&cloud_copies_sql())?
            .query_map(params![size, hash.to_vec(), limit as i64], RemoteRow::from_row)?
            .collect::<rusqlite::Result<Vec<_>>>()?;
        rows.into_iter().map(fix_kind).collect()
    }

    /// Up to `limit` local-only files with this content whose hash still
    /// stands (see the module docs), by ID.
    pub fn dup_local_copies(&self, size: i64, hash: &Hash, limit: usize) -> Result<Vec<LocalRow>> {
        let ids: Vec<Vec<u8>> = self
            .c
            .prepare_cached(LOCAL_COPIES)?
            .query_map(params![size, hash.to_vec(), limit as i64], |r| r.get(0))?
            .collect::<rusqlite::Result<_>>()?;
        let mut out = Vec::with_capacity(ids.len());
        for id in ids {
            if let Some(row) = self.local_get(id_from_blob(&id))? {
                out.push(row);
            }
        }
        Ok(out)
    }

    /// The hash recorded for local-only file `id`, if it was made for this
    /// size and last-write time.
    pub fn local_only_hash_get(&self, id: u128, size: i64, mtime: i64) -> Result<Option<Hash>> {
        let v: Option<Vec<u8>> = self
            .c
            .prepare_cached("SELECT qxh FROM local_only_hash WHERE file_id = ?1 AND size = ?2 AND mtime = ?3")?
            .query_row(params![blob(id), size, mtime], |r| r.get(0))
            .optional()?;
        Ok(v.and_then(hash_of))
    }

    /// Records the hash of local-only file `id` made at this size and
    /// last-write time. Returns `false`, recording nothing, once
    /// [`LOCAL_ONLY_MAX`] are kept.
    pub fn local_only_hash_put(&self, id: u128, size: i64, mtime: i64, hash: &Hash) -> Result<bool> {
        let known = self.c.prepare_cached("SELECT 1 FROM local_only_hash WHERE file_id = ?1")?.exists([blob(id)])?;
        if !known && self.local_only_hash_count()? >= LOCAL_ONLY_MAX {
            return Ok(false);
        }
        self.c
            .prepare_cached(
                "INSERT INTO local_only_hash(file_id, size, mtime, qxh) VALUES (?1, ?2, ?3, ?4)
                 ON CONFLICT(file_id) DO UPDATE SET size = excluded.size, mtime = excluded.mtime, qxh = excluded.qxh",
            )?
            .execute(params![blob(id), size, mtime, hash.to_vec()])?;
        Ok(true)
    }

    /// Forgets the local-only hash of file `id`: its data was written
    /// (the change journal says so), whatever its size and time are now.
    pub fn local_only_hash_forget(&self, id: u128) -> Result<()> {
        self.c.prepare_cached("DELETE FROM local_only_hash WHERE file_id = ?1")?.execute([blob(id)])?;
        Ok(())
    }

    pub fn local_only_hash_count(&self) -> Result<i64> {
        Ok(self.c.prepare_cached("SELECT count(*) FROM local_only_hash")?.query_row([], |r| r.get(0))?)
    }

    /// Drops the local-only hashes that no longer stand: the file is gone
    /// or changed, or synced now. Returns how many.
    pub fn local_only_hash_trim(&self) -> Result<usize> {
        Ok(self.c.execute(
            "DELETE FROM local_only_hash WHERE NOT EXISTS (
                 SELECT 1 FROM local_item i WHERE i.file_id = local_only_hash.file_id
                    AND i.size = local_only_hash.size AND i.mtime = local_only_hash.mtime)
               OR EXISTS (SELECT 1 FROM synced_item s WHERE s.local_id = local_only_hash.file_id)",
            [],
        )?)
    }

    /// Forgets every local-only hash.
    pub fn local_only_hash_clear(&self) -> Result<()> {
        self.c.execute("DELETE FROM local_only_hash", [])?;
        Ok(())
    }

    /// Up to `limit` files of the Local tree after file ID `after`, by ID,
    /// of at least `min_size` bytes: candidates for hashing, read a page at
    /// a time.
    pub fn local_files_from(&self, after: u128, min_size: i64, limit: usize) -> Result<Vec<LocalRow>> {
        let ids: Vec<Vec<u8>> = self
            .c
            .prepare_cached(LOCAL_FILES_FROM)?
            .query_map(params![blob(after), min_size, limit as i64], |r| r.get(0))?
            .collect::<rusqlite::Result<_>>()?;
        let mut out = Vec::with_capacity(ids.len());
        for id in ids {
            if let Some(row) = self.local_get(id_from_blob(&id))? {
                out.push(row);
            }
        }
        Ok(out)
    }
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used)]
    use super::*;
    use crate::{Kind, LocalKind, Skip, Store};

    const MIB: i64 = 1 << 20;

    fn file(id: &str, size: i64, h: u8) -> RemoteRow {
        RemoteRow {
            id: id.into(),
            parent_id: Some("root".into()),
            name: format!("{id}.bin"),
            kind: Kind::File,
            size,
            etag: Some("e".into()),
            qxh: Some([h; 20]),
            mtime_ms: None,
        }
    }

    fn local(id: u128, size: i64, mtime: i64) -> LocalRow {
        LocalRow {
            file_id: id,
            parent_id: Some(1),
            name: format!("l{id}"),
            name_lossy: false,
            kind: LocalKind::File,
            size,
            mtime,
            attributes: 0,
            reparse_tag: 0,
            skip: Skip::None,
        }
    }

    fn store() -> Store {
        let mut s = Store::open_in_memory().unwrap();
        s.write(|o| {
            let root = RemoteRow { kind: Kind::Root, parent_id: None, qxh: None, ..file("root", 0, 0) };
            o.upsert_item(&root, false)?;
            o.local_put(&LocalRow { kind: LocalKind::Root, parent_id: None, ..local(1, 0, 0) })?;
            // Three copies of 5 MiB, two of 8 MiB, one of 9 MiB, and small
            // files (below the index) that never count.
            for (id, size, h) in [
                ("a1", 5 * MIB, 1),
                ("a2", 5 * MIB, 1),
                ("a3", 5 * MIB, 1),
                ("b1", 8 * MIB, 2),
                ("b2", 8 * MIB, 2),
                ("c1", 9 * MIB, 3),
                ("s1", 10, 4),
                ("s2", 10, 4),
            ] {
                o.upsert_item(&file(id, size, h), false)?;
            }
            Ok(())
        })
        .unwrap();
        s
    }

    #[test]
    fn groups_come_most_space_saved_first_a_page_at_a_time() {
        let s = store();
        let all = s.read(|o| o.dup_groups(None, 10)).unwrap();
        let shape: Vec<(i64, i64)> = all.iter().map(|g| (g.size / MIB, g.copies)).collect();
        assert_eq!(shape, [(5, 3), (8, 2)], "10 MiB saved, then 8; single copies and small files never");
        let first = s.read(|o| o.dup_groups(None, 1)).unwrap();
        let next = s.read(|o| o.dup_groups(Some(first[0].cursor()), 1)).unwrap();
        assert_eq!(next, [all[1]]);
        assert!(s.read(|o| o.dup_groups(Some(next[0].cursor()), 1)).unwrap().is_empty());
        let sum = s.read(|o| o.dup_summary()).unwrap();
        assert_eq!(sum, DupSummary { groups: 2, files: 3, bytes: 18 * MIB });
        let copies = s.read(|o| o.dup_cloud_copies(5 * MIB, &[1; 20], 10)).unwrap();
        assert_eq!(copies.iter().map(|r| r.id.as_str()).collect::<Vec<_>>(), ["a1", "a2", "a3"]);
    }

    #[test]
    fn local_only_hashes_count_while_they_stand() {
        let mut s = store();
        s.write(|o| {
            o.local_put(&local(10, 9 * MIB, 100))?;
            o.local_put(&local(11, 9 * MIB, 100))?;
            assert!(o.local_only_hash_put(10, 9 * MIB, 100, &[3; 20])?);
            // Hashed at another last-write time: doesn't stand.
            o.local_only_hash_put(11, 9 * MIB, 99, &[3; 20])
        })
        .unwrap();
        let group = |s: &Store| s.read(|o| o.dup_groups(None, 10)).unwrap().into_iter().find(|g| g.hash == [3; 20]);
        assert_eq!(group(&s).map(|g| g.copies), Some(2), "the cloud copy and one here");
        let here = s.read(|o| o.dup_local_copies(9 * MIB, &[3; 20], 10)).unwrap();
        assert_eq!(here.iter().map(|r| r.file_id).collect::<Vec<_>>(), [10]);

        // Synced now: its cloud copy stands for it.
        s.write(|o| {
            let b = |sid, parent, local: u128| oneup_sync::Base {
                sid,
                parent,
                name: String::new(),
                kind: oneup_sync::NodeKind::Folder,
                size: 0,
                hash: None,
                local: Some(oneup_sync::LocalId(local)),
                local_mtime: 0,
                remote: None,
                remote_etag: String::new(),
            };
            o.synced_put(&b(oneup_sync::ROOT, None, 1))?;
            o.synced_put(&oneup_sync::Base { kind: oneup_sync::NodeKind::File, ..b(5, Some(oneup_sync::ROOT), 10) })
        })
        .unwrap();
        assert_eq!(group(&s), None);
        assert_eq!(s.write(|o| o.local_only_hash_trim()).unwrap(), 2, "synced, and stale");
        assert_eq!(s.read(|o| o.local_only_hash_count()).unwrap(), 0);
    }

    /// A v19 store, from before the table, upgrades: the table and its
    /// triggers arrive, the trees it had stay.
    #[test]
    fn a_v19_store_upgrades_to_hashing_local_only_files() {
        let dir = std::env::temp_dir().join(format!("oneup-store-dupes-v19-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("state.db");
        let table = |s: &Store| {
            s.read(|o| {
                let sql = "SELECT count(*) FROM sqlite_master WHERE name = 'local_only_hash'";
                Ok(o.c.query_row(sql, [], |r| r.get::<_, i64>(0))?)
            })
            .unwrap()
        };
        {
            let mut s = Store::open(&path, 1024).unwrap();
            s.write(|o| {
                o.local_put(&LocalRow { kind: LocalKind::Root, parent_id: None, ..local(1, 0, 0) })?;
                o.local_put(&local(10, 9 * MIB, 100))
            })
            .unwrap();
            s.read(|o| {
                let undo = format!("{} {UNDO_V20} PRAGMA user_version = 19;", crate::shared::UNDO_V21);
                Ok(o.c.execute_batch(&undo)?)
            })
            .unwrap();
            assert_eq!(table(&s), 0, "a v19 store");
        }
        let mut s = Store::open(&path, 1024).unwrap();
        let version = s.read(|o| Ok(o.c.query_row("PRAGMA user_version", [], |r| r.get::<_, i64>(0))?)).unwrap();
        assert_eq!(version, crate::SCHEMA_VERSION, "upgraded to the latest");
        assert_eq!(table(&s), 1);
        assert!(s.write(|o| o.local_only_hash_put(10, 9 * MIB, 100, &[3; 20])).unwrap());
        assert_eq!(s.read(|o| o.local_only_hash_count()).unwrap(), 1);
        s.write(|o| o.local_put(&local(10, 9 * MIB, 101))).unwrap();
        assert_eq!(s.read(|o| o.local_only_hash_count()).unwrap(), 0, "its triggers came too");
        drop(s);
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// A hash goes when its file changes, goes, comes back under the same
    /// ID, or the synced folder is scanned again.
    #[test]
    fn a_hash_goes_with_its_file_and_its_folder() {
        let mut s = store();
        let count = |s: &Store| s.read(|o| o.local_only_hash_count()).unwrap();
        let put = |s: &mut Store, id: u128| {
            s.write(|o| o.local_only_hash_put(id, 9 * MIB, 100, &[3; 20]).map(|_| ())).unwrap()
        };
        s.write(|o| {
            for id in [10u128, 11, 12] {
                o.local_put(&local(id, 9 * MIB, 100))?;
            }
            Ok(())
        })
        .unwrap();
        for id in [10, 11, 12] {
            put(&mut s, id);
        }
        // Same size, another time (an edit that kept the size).
        s.write(|o| o.local_update(&local(10, 9 * MIB, 101))).unwrap();
        assert_eq!(count(&s), 2);
        // Renamed only: it stands.
        s.write(|o| o.local_update(&LocalRow { name: "moved".into(), ..local(11, 9 * MIB, 100) })).unwrap();
        assert_eq!(count(&s), 2);
        // Gone, and another file under the same ID with the same stamp.
        s.write(|o| {
            o.c.execute("DELETE FROM local_item WHERE file_id = ?1", [blob(11)])?;
            o.local_put(&local(11, 9 * MIB, 100)).map(|_| ())
        })
        .unwrap();
        assert_eq!(count(&s), 1);
        assert!(s.read(|o| o.local_only_hash_get(11, 9 * MIB, 100)).unwrap().is_none());
        // The folder scanned again: none stands.
        let scan = "INSERT INTO local_root (id, path, volume_serial, root_file_id, rules, started_at)
                    VALUES (1, 'C:/x', 1, x'00', '', 0)";
        s.write(|o| o.c.execute_batch(scan).map_err(Into::into)).unwrap();
        assert_eq!(count(&s), 0);
    }

    #[test]
    fn candidates_for_hashing_come_by_id_a_page_at_a_time() {
        let mut s = store();
        s.write(|o| {
            for id in [20u128, 21, 22] {
                o.local_put(&local(id, 2 * MIB, 1))?;
            }
            o.local_put(&local(23, 10, 1))
        })
        .unwrap();
        let ids = |v: Vec<LocalRow>| v.into_iter().map(|r| r.file_id).collect::<Vec<_>>();
        assert_eq!(ids(s.read(|o| o.local_files_from(0, COPY_MIN, 2)).unwrap()), [20, 21]);
        assert_eq!(ids(s.read(|o| o.local_files_from(21, COPY_MIN, 2)).unwrap()), [22]);
    }

    /// Grouping reads the cloud files by size index once and the small
    /// local-only table, never the whole Remote or Local tree; a group's
    /// copies are searches, not scans.
    #[test]
    fn grouping_uses_the_size_index_and_never_scans_a_tree() {
        let s = Store::open_in_memory().unwrap();
        let plan = |sql: &str| -> Vec<String> {
            let mut st = s.conn.prepare(&format!("EXPLAIN QUERY PLAN {sql}")).unwrap();
            let nulls = std::iter::repeat_n(rusqlite::types::Null, st.parameter_count());
            st.query_map(rusqlite::params_from_iter(nulls), |r| r.get(3)).unwrap().map(|r| r.unwrap()).collect()
        };
        for sql in [groups_sql(), summary_sql()] {
            let steps = plan(&sql);
            assert!(
                steps.iter().any(|s| s.contains("remote_item USING INDEX remote_item_copyable")),
                "{sql}\n{steps:#?}"
            );
            // Besides the index, only the local-only table (bounded) and
            // what the query made itself are read whole.
            for step in steps.iter().filter(|s| s.starts_with("SCAN")) {
                let ok = step == "SCAN h" || step == "SCAN groups" || step == "SCAN copies";
                assert!(ok, "{step}\n{sql}\n{steps:#?}");
            }
        }
        for sql in [cloud_copies_sql(), LOCAL_COPIES.to_string(), LOCAL_FILES_FROM.to_string()] {
            let steps = plan(&sql);
            assert!(!steps.iter().any(|s| s.starts_with("SCAN")), "{sql}\n{steps:#?}");
        }
    }
}
