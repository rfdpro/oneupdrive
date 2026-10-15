//! How files are used (schema v10): which programs change and open files in
//! the synced folder, when a file was last opened, and what the largest
//! files are. All bounded and indexed, so cost follows activity, not the
//! number of files:
//!
//! * `app_use`: counts per hour, program and kind; [`Ops::app_use_prune`]
//!   drops old hours.
//! * `local_used`: the last time a file's content was used, for the files
//!   OneUpDrive saw being opened (one row each, replaced in place).
//! * `local_by_age`: the files whose content is here and could be freed,
//!   oldest first, for the automatic space policies.

use rusqlite::{OptionalExtension as _, params};

use crate::local::{blob, id_from_blob};
use crate::{ITEM_COLS, LocalRow, Ops, RemoteRow, Result, fix_kind};

pub(crate) const MIGRATE_V10: &str = "
CREATE TABLE app_use (
    hour  INTEGER NOT NULL,
    app   TEXT NOT NULL,
    kind  INTEGER NOT NULL,
    files INTEGER NOT NULL,
    bytes INTEGER NOT NULL,
    PRIMARY KEY (hour, app, kind)
) WITHOUT ROWID;
CREATE TABLE local_used (
    file_id BLOB PRIMARY KEY,
    at_ms   INTEGER NOT NULL
) WITHOUT ROWID;
CREATE INDEX local_by_age ON local_item(mtime, file_id)
    WHERE kind = 0 AND (attributes & 1024) != 0 AND (attributes & 4194304) = 0
      AND (attributes & 524288) = 0 AND (attributes & 1048576) = 0;
";

/// v10 -> v11: which files each program used, latest first, bounded
/// ([`APP_FILES_KEEP`]): the counts alone said which programs, never what
/// they did. Paths relative to the synced
/// folder, as the activity log keeps them; never contents.
pub(crate) const MIGRATE_V11: &str = "
CREATE TABLE app_file (
    seq   INTEGER PRIMARY KEY AUTOINCREMENT,
    at_ms INTEGER NOT NULL,
    app   TEXT NOT NULL,
    kind  INTEGER NOT NULL,
    path  TEXT NOT NULL,
    bytes INTEGER NOT NULL
);
CREATE INDEX app_file_app ON app_file(app, seq);
";

/// How many entries of `app_file` are kept.
pub const APP_FILES_KEEP: i64 = 5_000;

/// One file a program used.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct AppFile {
    pub at_ms: i64,
    /// As in [`AppUse::app`].
    pub app: String,
    pub kind: UseKind,
    /// Relative to the synced folder (`/docs/a.txt`).
    pub path: String,
    pub bytes: i64,
}

/// The condition `local_by_age` indexes: placeholders whose content is
/// here (not dehydrated), neither pinned nor already set to free up space.
const FREEABLE: &str = "kind = 0 AND (attributes & 1024) != 0 AND (attributes & 4194304) = 0 \
                        AND (attributes & 524288) = 0 AND (attributes & 1048576) = 0";

/// What a program did with files.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum UseKind {
    /// Wrote to a file in the synced folder.
    Changed,
    /// Opened an online-only file, which was downloaded for it.
    Opened,
}

impl UseKind {
    fn code(self) -> i64 {
        match self {
            UseKind::Changed => 0,
            UseKind::Opened => 1,
        }
    }

    fn from_code(c: i64) -> Self {
        if c == 1 { UseKind::Opened } else { UseKind::Changed }
    }
}

/// Counts for one hour, program and kind.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct AppUse {
    /// Hours since the Unix epoch.
    pub hour: i64,
    /// The program's file name (`WINWORD.EXE`), or `""` when it couldn't
    /// be told.
    pub app: String,
    pub kind: UseKind,
    pub files: i64,
    pub bytes: i64,
}

impl Ops<'_> {
    /// Adds to the counts (they add up for the same hour, program, kind).
    pub fn app_use_add(&self, uses: &[AppUse]) -> Result<()> {
        let mut st = self.c.prepare_cached(
            "INSERT INTO app_use (hour, app, kind, files, bytes) VALUES (?1, ?2, ?3, ?4, ?5)
             ON CONFLICT (hour, app, kind) DO UPDATE SET files = files + ?4, bytes = bytes + ?5",
        )?;
        for u in uses {
            st.execute(params![u.hour, u.app, u.kind.code(), u.files, u.bytes])?;
        }
        Ok(())
    }

    /// Everything from hour `since` on, oldest first.
    pub fn app_use_since(&self, since: i64) -> Result<Vec<AppUse>> {
        Ok(self
            .c
            .prepare_cached("SELECT hour, app, kind, files, bytes FROM app_use WHERE hour >= ?1 ORDER BY hour")?
            .query_map([since], |r| {
                Ok(AppUse {
                    hour: r.get(0)?,
                    app: r.get(1)?,
                    kind: UseKind::from_code(r.get(2)?),
                    files: r.get(3)?,
                    bytes: r.get(4)?,
                })
            })?
            .collect::<rusqlite::Result<Vec<_>>>()?)
    }

    /// Adds entries, then drops the oldest beyond [`APP_FILES_KEEP`].
    pub fn app_files_add(&self, files: &[AppFile]) -> Result<()> {
        if files.is_empty() {
            return Ok(());
        }
        let mut st = self
            .c
            .prepare_cached("INSERT INTO app_file (at_ms, app, kind, path, bytes) VALUES (?1, ?2, ?3, ?4, ?5)")?;
        for f in files {
            st.execute(params![f.at_ms, f.app, f.kind.code(), f.path, f.bytes])?;
        }
        let last: Option<i64> = self.c.query_row("SELECT max(seq) FROM app_file", [], |r| r.get(0))?;
        if let Some(last) = last {
            self.c.prepare_cached("DELETE FROM app_file WHERE seq <= ?1")?.execute([last - APP_FILES_KEEP])?;
        }
        Ok(())
    }

    /// The latest `limit` files used, newest first; only `app`'s if given.
    pub fn app_files(&self, app: Option<&str>, limit: usize) -> Result<Vec<AppFile>> {
        let limit = i64::try_from(limit).unwrap_or(i64::MAX);
        let row = |r: &rusqlite::Row<'_>| {
            Ok(AppFile {
                at_ms: r.get(0)?,
                app: r.get(1)?,
                kind: UseKind::from_code(r.get(2)?),
                path: r.get(3)?,
                bytes: r.get(4)?,
            })
        };
        let cols = "at_ms, app, kind, path, bytes";
        let rows = match app {
            Some(a) => self
                .c
                .prepare_cached(&format!("SELECT {cols} FROM app_file WHERE app = ?1 ORDER BY seq DESC LIMIT ?2"))?
                .query_map(params![a, limit], row)?
                .collect::<rusqlite::Result<Vec<_>>>()?,
            None => self
                .c
                .prepare_cached(&format!("SELECT {cols} FROM app_file ORDER BY seq DESC LIMIT ?1"))?
                .query_map([limit], row)?
                .collect::<rusqlite::Result<Vec<_>>>()?,
        };
        Ok(rows)
    }

    /// Drops the hours before `before`.
    pub fn app_use_prune(&self, before: i64) -> Result<usize> {
        Ok(self.c.prepare_cached("DELETE FROM app_use WHERE hour < ?1")?.execute([before])?)
    }

    /// Records that a file's content was used at `at_ms`.
    pub fn local_used_set(&self, file_id: u128, at_ms: i64) -> Result<()> {
        self.c
            .prepare_cached(
                "INSERT INTO local_used (file_id, at_ms) VALUES (?1, ?2)
                 ON CONFLICT (file_id) DO UPDATE SET at_ms = max(at_ms, ?2)",
            )?
            .execute(params![blob(file_id).as_slice(), at_ms])?;
        Ok(())
    }

    /// When a file's content was last seen used, if ever.
    pub fn local_used_get(&self, file_id: u128) -> Result<Option<i64>> {
        Ok(self
            .c
            .prepare_cached("SELECT at_ms FROM local_used WHERE file_id = ?1")?
            .query_row([blob(file_id).as_slice()], |r| r.get(0))
            .optional()?)
    }

    /// Forgets the use times of files no longer in the Local tree (bounded
    /// work per call).
    pub fn local_used_trim(&self, limit: usize) -> Result<usize> {
        Ok(self
            .c
            .prepare_cached(
                "DELETE FROM local_used WHERE file_id IN (
                     SELECT u.file_id FROM local_used u LEFT JOIN local_item l ON l.file_id = u.file_id
                     WHERE l.file_id IS NULL LIMIT ?1)",
            )?
            .execute([limit as i64])?)
    }

    /// Files whose content is here and could be freed (see the module docs),
    /// last written before `before_mtime` (a FILETIME), oldest first,
    /// continuing after `after` (the last row of the previous page). Empty
    /// files have nothing to free (and freeing one is always refused: it
    /// looks freed already), so they're left out.
    pub fn local_by_age(&self, before_mtime: i64, after: Option<(i64, u128)>, limit: usize) -> Result<Vec<LocalRow>> {
        let (am, aid) = after.map_or((i64::MIN, Vec::new()), |(m, id)| (m, blob(id).to_vec()));
        let ids: Vec<Vec<u8>> = self
            .c
            .prepare_cached(&format!(
                "SELECT file_id FROM local_item INDEXED BY local_by_age
                 WHERE {FREEABLE} AND size > 0 AND mtime < ?1 AND (mtime, file_id) > (?2, ?3)
                 ORDER BY mtime, file_id LIMIT ?4"
            ))?
            .query_map(params![before_mtime, am, aid, limit as i64], |r| r.get(0))?
            .collect::<rusqlite::Result<_>>()?;
        let mut out = Vec::with_capacity(ids.len());
        for id in ids {
            if let Some(row) = self.local_get(id_from_blob(&id))? {
                out.push(row);
            }
        }
        Ok(out)
    }

    /// The `limit` largest files in OneDrive (that the mirror lists),
    /// largest first. Uses the index of files of 1 MiB or more; a drive
    /// with fewer than `limit` of those is small enough to read whole.
    pub fn remote_largest(&self, limit: usize) -> Result<Vec<RemoteRow>> {
        let query = |sql: &str| -> Result<Vec<RemoteRow>> {
            let rows = self
                .c
                .prepare_cached(sql)?
                .query_map([limit as i64], RemoteRow::from_row)?
                .collect::<rusqlite::Result<Vec<_>>>()?;
            rows.into_iter().map(fix_kind).collect()
        };
        let big = query(&format!(
            "SELECT {ITEM_COLS} FROM remote_item INDEXED BY remote_item_copyable
             WHERE kind = 0 AND size >= 1048576 AND qxh IS NOT NULL ORDER BY size DESC LIMIT ?1"
        ))?;
        if big.len() >= limit {
            return Ok(big);
        }
        query(&format!("SELECT {ITEM_COLS} FROM remote_item WHERE kind = 0 ORDER BY size DESC LIMIT ?1"))
    }
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used)]
    use super::*;
    use crate::Store;

    #[test]
    fn app_use_adds_up_and_ages_out() {
        let mut s = Store::open_in_memory().unwrap();
        let u = |hour, app: &str, kind, files| AppUse { hour, app: app.into(), kind, files, bytes: files * 10 };
        s.write(|o| {
            o.app_use_add(&[u(1, "a.exe", UseKind::Changed, 2), u(2, "a.exe", UseKind::Opened, 1)])?;
            o.app_use_add(&[u(1, "a.exe", UseKind::Changed, 3)])
        })
        .unwrap();
        let all = s.read(|o| o.app_use_since(0)).unwrap();
        assert_eq!(all, vec![u(1, "a.exe", UseKind::Changed, 5), u(2, "a.exe", UseKind::Opened, 1)]);
        s.write(|o| o.app_use_prune(2)).unwrap();
        assert_eq!(s.read(|o| o.app_use_since(0)).unwrap().len(), 1);
    }

    #[test]
    fn files_used_come_newest_first_per_program_and_stay_bounded() {
        let mut s = Store::open_in_memory().unwrap();
        let f = |at, app: &str, path: &str| AppFile {
            at_ms: at,
            app: app.into(),
            kind: UseKind::Changed,
            path: path.into(),
            bytes: 1,
        };
        s.write(|o| o.app_files_add(&[f(1, "a.exe", "/x"), f(2, "b.exe", "/y"), f(3, "a.exe", "/z")])).unwrap();
        let paths = |v: Vec<AppFile>| v.into_iter().map(|f| f.path).collect::<Vec<_>>();
        assert_eq!(paths(s.read(|o| o.app_files(Some("a.exe"), 10)).unwrap()), ["/z", "/x"]);
        assert_eq!(paths(s.read(|o| o.app_files(None, 2)).unwrap()), ["/z", "/y"]);
        let many: Vec<AppFile> = (0..APP_FILES_KEEP + 5).map(|i| f(i, "c.exe", "/c")).collect();
        s.write(|o| o.app_files_add(&many)).unwrap();
        assert_eq!(s.read(|o| o.app_files(None, usize::MAX)).unwrap().len() as i64, APP_FILES_KEEP);
    }

    #[test]
    fn freeable_files_come_oldest_first_in_pages() {
        use crate::{LocalKind, Skip};
        let mut s = Store::open_in_memory().unwrap();
        let file = |id: u128, mtime: i64, attributes: u32| LocalRow {
            file_id: id,
            parent_id: Some(1),
            name: format!("f{id}"),
            name_lossy: false,
            kind: LocalKind::File,
            size: 10,
            mtime,
            attributes,
            reparse_tag: 0,
            skip: Skip::None,
        };
        const HERE: u32 = 1024; // a placeholder with its content here
        s.write(|o| {
            o.local_put(&LocalRow { kind: LocalKind::Root, parent_id: None, name: "r".into(), ..file(1, 0, 0) })?;
            o.local_put(&file(10, 300, HERE))?;
            o.local_put(&file(11, 100, HERE))?;
            o.local_put(&file(12, 200, HERE))?;
            o.local_put(&file(13, 50, HERE | 4_194_304))?; // dehydrated: nothing to free
            o.local_put(&file(14, 50, HERE | 524_288))?; // pinned: always kept
            o.local_put(&file(15, 50, 0))?; // an ordinary file: never freed
            o.local_put(&file(16, 900, HERE))?; // too new
            o.local_put(&LocalRow { size: 0, ..file(17, 150, HERE) })?; // empty: nothing to free
            Ok(())
        })
        .unwrap();
        let ids = |v: Vec<LocalRow>| v.iter().map(|r| r.file_id).collect::<Vec<_>>();
        let first = s.read(|o| o.local_by_age(500, None, 2)).unwrap();
        assert_eq!(ids(first.clone()), vec![11, 12]);
        let last = first.last().map(|r| (r.mtime, r.file_id));
        assert_eq!(ids(s.read(|o| o.local_by_age(500, last, 2)).unwrap()), vec![10]);
    }

    #[test]
    fn use_times_only_move_forward() {
        let mut s = Store::open_in_memory().unwrap();
        s.write(|o| {
            o.local_used_set(7, 100)?;
            o.local_used_set(7, 50)
        })
        .unwrap();
        assert_eq!(s.read(|o| o.local_used_get(7)).unwrap(), Some(100));
        assert_eq!(s.read(|o| o.local_used_get(8)).unwrap(), None);
        // File 7 isn't in the Local tree: forgotten.
        assert_eq!(s.write(|o| o.local_used_trim(10)).unwrap(), 1);
    }
}
