//! The Local tree: OneUpDrive's record of a scanned local folder.
//!
//! Filled by `oneup-local`'s read-only scanner (schema v3). Like the Remote
//! tree, every row is *derived* data: it can always be rebuilt by scanning
//! again, so a new scan simply replaces the previous one.
//!
//! Invariants (checked by [`Ops::check_invariants`]):
//! 1. at most one root row, and it matches `local_root.root_file_id`;
//! 2. every non-root row's parent is stored and is a folder whose contents
//!    were listed (`skip = 0`);
//! 3. only folders have a non-zero `skip`;
//! 4. every queued folder is stored, listable and not yet skipped.

use rusqlite::{OptionalExtension, Row, params};

use crate::{Error, Ops, Result};

/// Windows attribute bits the summary queries need. Duplicated from
/// `oneup-ntfs` on purpose: the store must not depend on the FFI crates.
const FILE_ATTRIBUTE_OFFLINE: i64 = 0x1000;
const FILE_ATTRIBUTE_PINNED: i64 = 0x0008_0000;
const FILE_ATTRIBUTE_RECALL_ON_DATA_ACCESS: i64 = 0x0040_0000;
/// `IO_REPARSE_TAG_CLOUD` and its 16 variants `CLOUD_1`..`CLOUD_F` differ
/// only in bits 12..16, so masking those out matches all of them.
const CLOUD_TAG_MASK: i64 = 0xFFFF_0FFF;
const IO_REPARSE_TAG_CLOUD: i64 = 0x9000_001A;

/// What a local entry is.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum LocalKind {
    File = 0,
    Folder = 1,
    /// The scanned folder itself.
    Root = 2,
    /// A symbolic link, junction or mount point. Never followed: its target
    /// lies outside the tree (or would loop back into it).
    Link = 3,
}

impl TryFrom<i64> for LocalKind {
    type Error = Error;
    fn try_from(v: i64) -> Result<Self> {
        Ok(match v {
            0 => LocalKind::File,
            1 => LocalKind::Folder,
            2 => LocalKind::Root,
            3 => LocalKind::Link,
            _ => return Err(Error::Corrupt(format!("local kind {v}"))),
        })
    }
}

/// Why a folder's contents were not listed. Rows with any reason other than
/// [`Skip::None`] have no children in `local_item`.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum Skip {
    /// Listed normally (or not a folder).
    None = 0,
    /// Matched an exclusion rule; stored as one opaque row, like
    /// `remote_item.is_excluded`.
    Excluded = 1,
    /// A cloud placeholder folder whose children haven't been downloaded yet.
    /// Listing it would make the cloud provider fetch them, so we don't.
    Unpopulated = 2,
    /// The folder couldn't be opened or listed (usually access denied).
    Unreadable = 3,
}

impl TryFrom<i64> for Skip {
    type Error = Error;
    fn try_from(v: i64) -> Result<Self> {
        Ok(match v {
            0 => Skip::None,
            1 => Skip::Excluded,
            2 => Skip::Unpopulated,
            3 => Skip::Unreadable,
            _ => return Err(Error::Corrupt(format!("local skip {v}"))),
        })
    }
}

/// One local entry's metadata, exactly as the directory listing reported it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct LocalRow {
    pub file_id: u128,
    pub parent_id: Option<u128>,
    pub name: String,
    /// The on-disk name isn't valid Unicode (an unpaired UTF-16 surrogate,
    /// legal on NTFS); `name` holds a lossy rendering that must never be used
    /// to address the file, and the item can't be synced as named.
    pub name_lossy: bool,
    pub kind: LocalKind,
    pub size: i64,
    /// Last write time as a Windows FILETIME (100 ns ticks since 1601).
    pub mtime: i64,
    pub attributes: u32,
    pub reparse_tag: u32,
    pub skip: Skip,
}

impl LocalRow {
    fn from_row(r: &Row<'_>) -> rusqlite::Result<(i64, i64, Self)> {
        let id: Vec<u8> = r.get("file_id")?;
        let parent: Option<Vec<u8>> = r.get("parent_id")?;
        Ok((
            r.get("kind")?,
            r.get("skip")?,
            Self {
                file_id: id_from_blob(&id),
                parent_id: parent.as_deref().map(id_from_blob),
                name: r.get("name")?,
                name_lossy: r.get::<_, i64>("name_lossy")? != 0,
                kind: LocalKind::File, // replaced by `fix_local` after validation
                size: r.get("size")?,
                mtime: r.get("mtime")?,
                attributes: r.get::<_, i64>("attributes")? as u32,
                reparse_tag: r.get::<_, i64>("reparse_tag")? as u32,
                skip: Skip::None, // replaced by `fix_local`
            },
        ))
    }
}

fn fix_local((kind, skip, mut row): (i64, i64, LocalRow)) -> Result<LocalRow> {
    row.kind = LocalKind::try_from(kind)?;
    row.skip = Skip::try_from(skip)?;
    Ok(row)
}

/// File IDs are stored as 16-byte little-endian BLOBs (the on-disk byte order
/// of `FILE_ID_128`). Malformed blobs (never written by us) decode as 0, which
/// matches no real entry, rather than panicking.
pub(crate) fn id_from_blob(b: &[u8]) -> u128 {
    <[u8; 16]>::try_from(b).map(u128::from_le_bytes).unwrap_or(0)
}

pub(crate) fn blob(id: u128) -> [u8; 16] {
    id.to_le_bytes()
}

/// The scanned folder and the circumstances of its scan.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct LocalRoot {
    /// The folder's normalized path, for display and for re-opening it.
    pub path: String,
    /// Identity of the folder (`data-safety.md` invariant 4). If either differs
    /// on a later run, it's a different folder (or a different disk) and the
    /// stored scan doesn't describe it.
    pub volume_serial: u64,
    pub root_file_id: u128,
    /// `ExcludeSet::fingerprint()` the scan applied.
    pub rules: String,
    /// The change journal (id, USN) up to which this tree is current: the
    /// position when the scan *started* (so changes made during the scan are
    /// replayed), then advanced by the observer. `None` on file systems
    /// without a journal.
    pub journal: Option<(u64, i64)>,
    pub started_at_ms: i64,
    /// `None` while the scan is unfinished.
    pub finished_at_ms: Option<i64>,
    /// Set if the folder is inside another sync engine's folder (e.g. the
    /// official OneDrive client). Such a folder may be read, never managed.
    pub sync_root_provider: Option<String>,
}

/// What [`Ops::local_put`] did.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PutOutcome {
    /// Inserted, or updated in place when the same entry is seen again (e.g.
    /// a folder re-listed after a resume).
    Stored,
    /// The file ID is already stored under a *different* parent or name: a
    /// hard link (one file, several names), or an entry moved while the scan
    /// ran. The first sighting is kept; the caller counts these.
    Duplicate,
}

/// Summary counts of the Local tree.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct LocalStats {
    pub files: i64,
    pub file_bytes: i64,
    /// Folders including the root.
    pub folders: i64,
    pub excluded_folders: i64,
    pub unpopulated_folders: i64,
    pub unreadable_folders: i64,
    pub links: i64,
    pub lossy_names: i64,
    /// Entries that are cloud-provider placeholders (any state).
    pub placeholders: i64,
    /// Files whose data is not on this PC (cloud-only placeholders).
    pub online_only_files: i64,
    pub online_only_bytes: i64,
    /// Files marked "Always keep on this device".
    pub pinned_files: i64,
    /// Folders still waiting to be listed.
    pub queued: i64,
}

/// Version of what a scan records, bumped when an older scan's rows can't be
/// trusted for some purpose. Kept in `meta`, so no schema change is needed.
///
/// * 2: placeholders are exposed while scanning. Version-1 scans ran with
///   Windows disguising placeholders, so hydrated cloud files were recorded
///   as ordinary files.
pub const LOCAL_SCAN_FORMAT: &str = "2";
const LOCAL_SCAN_FORMAT_KEY: &str = "local_scan_format";

const LOCAL_COLS: &str = "file_id, parent_id, name, name_lossy, kind, size, mtime, attributes, reparse_tag, skip";

/// `%needle%` for `LIKE … ESCAPE '\'`, with the needle's own `%`, `_` and
/// `\` taken literally.
pub(crate) fn like_pattern(needle: &str) -> String {
    let mut p = String::from("%");
    for c in needle.chars() {
        if matches!(c, '%' | '_' | '\\') {
            p.push('\\');
        }
        p.push(c);
    }
    p.push('%');
    p
}

impl Ops<'_> {
    /// The current scan's root, if any scan has started.
    pub fn local_root(&self) -> Result<Option<LocalRoot>> {
        let row = self
            .c
            .query_row(
                "SELECT path, volume_serial, root_file_id, rules, journal_id, start_usn, started_at,
                        finished_at, sync_root_provider FROM local_root WHERE id = 1",
                [],
                |r| {
                    let root: Vec<u8> = r.get(2)?;
                    let journal_id: Option<i64> = r.get(4)?;
                    let start_usn: Option<i64> = r.get(5)?;
                    Ok(LocalRoot {
                        path: r.get(0)?,
                        volume_serial: r.get::<_, i64>(1)? as u64,
                        root_file_id: id_from_blob(&root),
                        rules: r.get(3)?,
                        journal: journal_id.zip(start_usn).map(|(j, u)| (j as u64, u)),
                        started_at_ms: r.get(6)?,
                        finished_at_ms: r.get(7)?,
                        sync_root_provider: r.get(8)?,
                    })
                },
            )
            .optional()?;
        Ok(row)
    }

    /// Starts a new scan: forgets any previous Local tree, records `root`,
    /// stores the root folder's row and queues it for listing.
    pub fn local_begin(&self, root: &LocalRoot, root_row: &LocalRow) -> Result<()> {
        debug_assert_eq!(root_row.kind, LocalKind::Root);
        self.c.execute_batch("DELETE FROM local_scan_queue; DELETE FROM local_item; DELETE FROM local_root;")?;
        self.c.execute(
            "INSERT INTO local_root (id, path, volume_serial, root_file_id, rules, journal_id, start_usn,
                                     started_at, finished_at, sync_root_provider)
             VALUES (1, ?1, ?2, ?3, ?4, ?5, ?6, ?7, NULL, ?8)",
            params![
                root.path,
                root.volume_serial as i64,
                blob(root.root_file_id),
                root.rules,
                root.journal.map(|(j, _)| j as i64),
                root.journal.map(|(_, u)| u),
                root.started_at_ms,
                root.sync_root_provider,
            ],
        )?;
        self.local_put(root_row)?;
        self.meta_set(LOCAL_SCAN_FORMAT_KEY, LOCAL_SCAN_FORMAT)?;
        self.local_enqueue(root_row.file_id, 0)
    }

    /// Whether the stored scan was made by the current scanner (see
    /// [`LOCAL_SCAN_FORMAT`]). `false` means "scan again before relying on it".
    pub fn local_scan_is_current(&self) -> Result<bool> {
        Ok(self.meta_get(LOCAL_SCAN_FORMAT_KEY)?.as_deref() == Some(LOCAL_SCAN_FORMAT))
    }

    /// Marks the scan complete.
    pub fn local_finish(&self, finished_at_ms: i64) -> Result<()> {
        self.c.execute("UPDATE local_root SET finished_at = ?1 WHERE id = 1", [finished_at_ms])?;
        Ok(())
    }

    /// Stores an entry. Idempotent for the same (id, parent, name); see
    /// [`PutOutcome::Duplicate`] for the other case.
    pub fn local_put(&self, row: &LocalRow) -> Result<PutOutcome> {
        debug_assert!(row.skip == Skip::None || matches!(row.kind, LocalKind::Folder | LocalKind::Root));
        // One statement per entry (this is the scan's hot path): the upsert's
        // WHERE clause only lets the update through for the same parent and
        // name, so "0 rows changed" means a duplicate. `IS` compares NULL
        // parents (the root) as equal.
        let changed = self
            .c
            .prepare_cached(
                "INSERT INTO local_item (file_id, parent_id, name, name_lossy, kind, size, mtime, attributes,
                                         reparse_tag, skip)
                 VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10)
                 ON CONFLICT(file_id) DO UPDATE SET
                    name_lossy = excluded.name_lossy, kind = excluded.kind, size = excluded.size,
                    mtime = excluded.mtime, attributes = excluded.attributes,
                    reparse_tag = excluded.reparse_tag, skip = excluded.skip
                 WHERE local_item.parent_id IS excluded.parent_id AND local_item.name = excluded.name",
            )?
            .execute(params![
                blob(row.file_id),
                row.parent_id.map(blob),
                row.name,
                row.name_lossy as i64,
                row.kind as i64,
                row.size,
                row.mtime,
                i64::from(row.attributes),
                i64::from(row.reparse_tag),
                row.skip as i64,
            ])?;
        Ok(if changed == 0 { PutOutcome::Duplicate } else { PutOutcome::Stored })
    }

    /// Stores an entry as it is now, wherever it was before: unlike
    /// [`Ops::local_put`], a different parent or name is a move or rename,
    /// not a duplicate. Used by the change-journal observer, which knows the
    /// entry's current state.
    pub fn local_update(&self, row: &LocalRow) -> Result<()> {
        debug_assert!(row.skip == Skip::None || matches!(row.kind, LocalKind::Folder | LocalKind::Root));
        self.c
            .prepare_cached(
                "INSERT INTO local_item (file_id, parent_id, name, name_lossy, kind, size, mtime, attributes,
                                         reparse_tag, skip)
                 VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10)
                 ON CONFLICT(file_id) DO UPDATE SET
                    parent_id = excluded.parent_id, name = excluded.name, name_lossy = excluded.name_lossy,
                    kind = excluded.kind, size = excluded.size, mtime = excluded.mtime,
                    attributes = excluded.attributes, reparse_tag = excluded.reparse_tag, skip = excluded.skip",
            )?
            .execute(params![
                blob(row.file_id),
                row.parent_id.map(blob),
                row.name,
                row.name_lossy as i64,
                row.kind as i64,
                row.size,
                row.mtime,
                i64::from(row.attributes),
                i64::from(row.reparse_tag),
                row.skip as i64,
            ])?;
        Ok(())
    }

    /// Forgets `file_id` and everything stored below it (and any of them
    /// still queued for listing). Returns the number of rows removed.
    ///
    /// This only says "no longer in the scanned tree" (moved out, or now
    /// inside an excluded folder). It is **not** deletion evidence; the
    /// caller records that separately, and only for a USN delete record.
    pub fn local_remove_subtree(&self, file_id: u128) -> Result<usize> {
        self.remove_below(file_id, true)
    }

    /// Like [`Ops::local_remove_subtree`] but keeps `file_id` itself: a
    /// folder that has just become excluded keeps its own row.
    pub fn local_remove_children(&self, file_id: u128) -> Result<usize> {
        self.remove_below(file_id, false)
    }

    fn remove_below(&self, file_id: u128, include_self: bool) -> Result<usize> {
        // The subtree is collected once into a temp table, then removed from
        // both tables. Recursion follows parent links, so it's proportional
        // to the subtree's size.
        self.c.execute("DELETE FROM t_local_sub", [])?;
        self.c.execute(
            "INSERT INTO t_local_sub(id)
             WITH RECURSIVE s(id) AS (
                 SELECT ?1
                 UNION
                 SELECT c.file_id FROM local_item c JOIN s ON c.parent_id = s.id
             ) SELECT id FROM s",
            [blob(file_id)],
        )?;
        if !include_self {
            self.c.execute("DELETE FROM t_local_sub WHERE id = ?1", [blob(file_id)])?;
        }
        self.c.execute("DELETE FROM local_scan_queue WHERE file_id IN (SELECT id FROM t_local_sub)", [])?;
        Ok(self.c.execute("DELETE FROM local_item WHERE file_id IN (SELECT id FROM t_local_sub)", [])?)
    }

    /// Moves the journal position up to which the Local tree is current.
    /// (After a scan it's the position taken before the scan started; the
    /// observer advances it as it applies records.)
    pub fn local_set_journal_cursor(&self, usn: i64) -> Result<()> {
        self.c.execute("UPDATE local_root SET start_usn = ?1 WHERE id = 1", [usn])?;
        Ok(())
    }

    /// Records the name-only folder rules the Local tree now follows (after
    /// the rules changed and stored folders were re-evaluated).
    pub fn local_set_rules(&self, fingerprint: &str) -> Result<()> {
        self.c.execute("UPDATE local_root SET rules = ?1 WHERE id = 1", [fingerprint])?;
        Ok(())
    }

    /// Calls `f(id, parent, name, skip)` for every stored folder. One pass
    /// over the table: for re-evaluating the rules after they changed (a
    /// rare event), not for a round.
    pub fn for_each_local_folder(&self, mut f: impl FnMut(u128, Option<u128>, &str, Skip)) -> Result<()> {
        let mut st = self.c.prepare("SELECT file_id, parent_id, name, skip FROM local_item WHERE kind = ?1")?;
        let mut rows = st.query([LocalKind::Folder as i64])?;
        while let Some(r) = rows.next()? {
            let id = id_from_blob(&r.get::<_, Vec<u8>>(0)?);
            let parent = r.get::<_, Option<Vec<u8>>>(1)?.map(|p| id_from_blob(&p));
            let name: String = r.get(2)?;
            f(id, parent, &name, Skip::try_from(r.get::<_, i64>(3)?)?);
        }
        Ok(())
    }

    /// Records why a stored folder's contents weren't listed after all
    /// (e.g. it turned out to be unreadable when opened).
    pub fn local_set_skip(&self, file_id: u128, skip: Skip) -> Result<()> {
        self.c
            .prepare_cached("UPDATE local_item SET skip = ?2 WHERE file_id = ?1")?
            .execute(params![blob(file_id), skip as i64])?;
        Ok(())
    }

    pub fn local_enqueue(&self, file_id: u128, depth: u32) -> Result<()> {
        self.c
            .prepare_cached("INSERT OR IGNORE INTO local_scan_queue (file_id, depth) VALUES (?1, ?2)")?
            .execute(params![blob(file_id), depth])?;
        Ok(())
    }

    pub fn local_dequeue(&self, file_id: u128) -> Result<()> {
        self.c.prepare_cached("DELETE FROM local_scan_queue WHERE file_id = ?1")?.execute([blob(file_id)])?;
        Ok(())
    }

    /// Up to `limit` queued folders, deepest first. Deepest-first keeps the
    /// queue (and so the resume state) small: it grows with the tree's depth
    /// times its fan-out rather than with its total width.
    pub fn local_next_dirs(&self, limit: usize) -> Result<Vec<(u128, u32)>> {
        let mut st =
            self.c.prepare_cached("SELECT file_id, depth FROM local_scan_queue ORDER BY depth DESC LIMIT ?1")?;
        let v = st
            .query_map([limit as i64], |r| Ok((id_from_blob(&r.get::<_, Vec<u8>>(0)?), r.get(1)?)))?
            .collect::<rusqlite::Result<_>>()?;
        Ok(v)
    }

    pub fn local_get(&self, file_id: u128) -> Result<Option<LocalRow>> {
        let row = self
            .c
            .prepare_cached(&format!("SELECT {LOCAL_COLS} FROM local_item WHERE file_id = ?1"))?
            .query_row([blob(file_id)], LocalRow::from_row)
            .optional()?;
        row.map(fix_local).transpose()
    }

    /// The stored children of a folder (unordered).
    pub fn local_children(&self, parent_id: u128) -> Result<Vec<LocalRow>> {
        let mut st = self.c.prepare_cached(&format!("SELECT {LOCAL_COLS} FROM local_item WHERE parent_id = ?1"))?;
        let rows = st.query_map([blob(parent_id)], LocalRow::from_row)?;
        rows.map(|r| fix_local(r?)).collect()
    }

    /// Up to `limit` entries whose name contains `needle` (any case), for
    /// the settings window's search. A scan of the names: fine for a search
    /// someone typed, not for anything that runs by itself.
    pub fn local_find(&self, needle: &str, limit: usize) -> Result<Vec<LocalRow>> {
        let mut st = self.c.prepare_cached(&format!(
            "SELECT {LOCAL_COLS} FROM local_item WHERE kind != 2 AND name LIKE ?1 ESCAPE '\\' LIMIT ?2"
        ))?;
        let rows = st.query_map(rusqlite::params![like_pattern(needle), limit as i64], LocalRow::from_row)?;
        rows.map(|r| fix_local(r?)).collect()
    }

    /// Items set to always stay on this device (`FILE_ATTRIBUTE_PINNED`, as
    /// stored) whose folder isn't: the tops of what's pinned.
    pub fn local_pinned_tops(&self) -> Result<Vec<u128>> {
        let mut st = self.c.prepare(
            "SELECT c.file_id FROM local_item c LEFT JOIN local_item p ON p.file_id = c.parent_id
             WHERE (c.attributes & 524288) != 0 AND c.kind != 2 AND (p.file_id IS NULL OR (p.attributes & 524288) = 0)",
        )?;
        let rows = st.query_map([], |r| r.get::<_, Vec<u8>>(0))?;
        rows.map(|r| Ok(id_from_blob(&r?))).collect()
    }

    /// How many files were online-only when last looked at, and their
    /// total size (`FILE_ATTRIBUTE_RECALL_ON_DATA_ACCESS` as stored).
    pub fn local_online_only(&self) -> Result<(u64, i64)> {
        Ok(self.c.query_row(
            "SELECT count(*), COALESCE(SUM(size), 0) FROM local_item WHERE kind = 0 AND (attributes & 4194304) != 0",
            [],
            |r| Ok((r.get::<_, i64>(0)? as u64, r.get(1)?)),
        )?)
    }

    /// For each folder directly in `parent_id`: the total size of the files
    /// anywhere inside it. One pass over the folder's subtree (for the
    /// settings window; not for anything that runs by itself).
    pub fn local_folder_totals(&self, parent_id: u128) -> Result<Vec<(u128, i64)>> {
        let mut st = self.c.prepare_cached(
            "WITH RECURSIVE t(top, id) AS (
                 SELECT file_id, file_id FROM local_item WHERE parent_id = ?1 AND kind = 1
                 UNION ALL
                 SELECT t.top, c.file_id FROM local_item c JOIN t ON c.parent_id = t.id WHERE c.kind = 1)
             SELECT t.top, COALESCE(SUM(f.size), 0) FROM t
             LEFT JOIN local_item f ON f.parent_id = t.id AND f.kind = 0
             GROUP BY t.top",
        )?;
        let rows = st.query_map([blob(parent_id)], |r| Ok((id_from_blob(&r.get::<_, Vec<u8>>(0)?), r.get(1)?)))?;
        rows.collect::<rusqlite::Result<_>>().map_err(Into::into)
    }

    /// The cloud placeholders the scan found, files and folders: (file ID,
    /// is a folder, size). With no sync root holding the folder any more,
    /// what a release left behind (`oneup_app::moveover`); only ever
    /// called for that, not in steady state.
    pub fn local_cloud_items(&self) -> Result<Vec<(u128, bool, i64)>> {
        let mut st = self.c.prepare(&format!(
            "SELECT file_id, kind, size FROM local_item
             WHERE kind IN (0, 1) AND (reparse_tag & {CLOUD_TAG_MASK}) = {IO_REPARSE_TAG_CLOUD}"
        ))?;
        let rows = st.query_map([], |r| {
            let id: Vec<u8> = r.get(0)?;
            Ok((id_from_blob(&id), r.get::<_, i64>(1)? == LocalKind::Folder as i64, r.get(2)?))
        })?;
        rows.collect::<rusqlite::Result<_>>().map_err(Into::into)
    }

    /// Path of a stored entry relative to the scanned folder ("/" for the
    /// root, "/a/b.txt" below it), built from parent links.
    pub fn local_path_of(&self, file_id: u128) -> Result<Option<String>> {
        let mut parts = Vec::new();
        let mut cur = file_id;
        // Bounded walk: NTFS paths can't nest deeper than ~16k components, and
        // a cycle (impossible unless the DB is corrupt) must not hang us.
        for _ in 0..32_768 {
            let row: Option<(Option<Vec<u8>>, String, i64)> = self
                .c
                .prepare_cached("SELECT parent_id, name, kind FROM local_item WHERE file_id = ?1")?
                .query_row([blob(cur)], |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?)))
                .optional()?;
            match row {
                None => return Ok(None),
                Some((_, _, k)) if k == LocalKind::Root as i64 => {
                    parts.reverse();
                    return Ok(Some(format!("/{}", parts.join("/"))));
                }
                Some((Some(p), name, _)) => {
                    parts.push(name);
                    cur = id_from_blob(&p);
                }
                Some((None, _, _)) => return Ok(None),
            }
        }
        Err(Error::Corrupt(format!("local parent chain of {file_id:#x} too deep or cyclic")))
    }

    /// Relative paths ("/a/node_modules") of every excluded folder, sorted.
    /// Feeds the remote enumeration hints: the remote side only needs to list
    /// the folders *leading to* these to skip them without downloading.
    ///
    /// One recursive pass over folders only (files are never visited), so the
    /// cost is proportional to the number of folders.
    pub fn local_excluded_paths(&self) -> Result<Vec<String>> {
        let mut st = self.c.prepare(
            "WITH RECURSIVE t(id, path, skip) AS (
                 SELECT file_id, '', skip FROM local_item WHERE kind = 2
                 UNION ALL
                 SELECT c.file_id, t.path || '/' || c.name, c.skip
                 FROM local_item c JOIN t ON c.parent_id = t.id
                 WHERE c.kind = 1 AND t.skip = 0
             )
             SELECT path FROM t WHERE skip = 1 ORDER BY path",
        )?;
        let v = st.query_map([], |r| r.get(0))?.collect::<rusqlite::Result<_>>()?;
        Ok(v)
    }

    pub fn local_stats(&self) -> Result<LocalStats> {
        let one = |sql: &str| -> Result<i64> { Ok(self.c.query_row(sql, [], |r| r.get(0))?) };
        let online = FILE_ATTRIBUTE_RECALL_ON_DATA_ACCESS | FILE_ATTRIBUTE_OFFLINE;
        Ok(LocalStats {
            files: one("SELECT count(*) FROM local_item WHERE kind = 0")?,
            file_bytes: one("SELECT coalesce(sum(size), 0) FROM local_item WHERE kind = 0")?,
            folders: one("SELECT count(*) FROM local_item WHERE kind IN (1, 2)")?,
            excluded_folders: one("SELECT count(*) FROM local_item WHERE skip = 1")?,
            unpopulated_folders: one("SELECT count(*) FROM local_item WHERE skip = 2")?,
            unreadable_folders: one("SELECT count(*) FROM local_item WHERE skip = 3")?,
            links: one("SELECT count(*) FROM local_item WHERE kind = 3")?,
            lossy_names: one("SELECT count(*) FROM local_item WHERE name_lossy = 1")?,
            placeholders: one(&format!(
                "SELECT count(*) FROM local_item WHERE (reparse_tag & {CLOUD_TAG_MASK}) = {IO_REPARSE_TAG_CLOUD}"
            ))?,
            online_only_files: one(&format!(
                "SELECT count(*) FROM local_item WHERE kind = 0 AND (attributes & {online}) != 0"
            ))?,
            online_only_bytes: one(&format!(
                "SELECT coalesce(sum(size), 0) FROM local_item WHERE kind = 0 AND (attributes & {online}) != 0"
            ))?,
            pinned_files: one(&format!(
                "SELECT count(*) FROM local_item WHERE kind = 0 AND (attributes & {FILE_ATTRIBUTE_PINNED}) != 0"
            ))?,
            queued: one("SELECT count(*) FROM local_scan_queue")?,
        })
    }

    /// Local-tree invariant violations, each prefixed with "local:".
    pub(crate) fn local_problems(&self) -> Result<Vec<String>> {
        let one = |sql: &str| -> Result<i64> { Ok(self.c.query_row(sql, [], |r| r.get(0))?) };
        let mut problems = Vec::new();
        let roots = one("SELECT count(*) FROM local_item WHERE kind = 2")?;
        if roots > 1 {
            problems.push(format!("local: {roots} root rows"));
        }
        let mismatched_root = one("SELECT count(*) FROM local_item i JOIN local_root r ON r.id = 1
             WHERE i.kind = 2 AND i.file_id != r.root_file_id")?;
        if mismatched_root > 0 {
            problems.push("local: root row doesn't match the recorded root".into());
        }
        let dangling = one("SELECT count(*) FROM local_item c LEFT JOIN local_item p ON p.file_id = c.parent_id
             WHERE c.kind != 2 AND (p.file_id IS NULL OR p.kind NOT IN (1, 2) OR p.skip != 0)")?;
        if dangling > 0 {
            problems.push(format!("local: {dangling} rows whose parent is missing, not a folder, or skipped"));
        }
        let bad_skip = one("SELECT count(*) FROM local_item WHERE skip != 0 AND kind NOT IN (1, 2)")?;
        if bad_skip > 0 {
            problems.push(format!("local: {bad_skip} non-folders marked skipped"));
        }
        let bad_queue = one("SELECT count(*) FROM local_scan_queue q LEFT JOIN local_item i ON i.file_id = q.file_id
             WHERE i.file_id IS NULL OR i.kind NOT IN (1, 2) OR i.skip != 0")?;
        if bad_queue > 0 {
            problems.push(format!("local: {bad_queue} queued entries that aren't listable folders"));
        }
        Ok(problems)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::Store;

    fn row(id: u128, parent: Option<u128>, name: &str, kind: LocalKind) -> LocalRow {
        LocalRow {
            file_id: id,
            parent_id: parent,
            name: name.into(),
            name_lossy: false,
            kind,
            size: 10,
            mtime: 0,
            attributes: 0,
            reparse_tag: 0,
            skip: Skip::None,
        }
    }

    fn root() -> LocalRoot {
        LocalRoot {
            path: r"C:\t".into(),
            volume_serial: u64::MAX - 1, // must survive the i64 round trip
            root_file_id: 1,
            rules: "v1:folders=".into(),
            journal: Some((u64::MAX - 5, 42)),
            started_at_ms: 7,
            finished_at_ms: None,
            sync_root_provider: Some("OneDrive".into()),
        }
    }

    /// root(1) / a(2) / { f(3), node_modules(4, excluded) }, plus a link(5)
    /// at the top and a 128-bit id that needs all 16 bytes.
    fn scanned(s: &mut Store) {
        s.write(|o| {
            o.local_begin(&root(), &row(1, None, "", LocalKind::Root))?;
            assert_eq!(o.local_put(&row(2, Some(1), "a", LocalKind::Folder))?, PutOutcome::Stored);
            o.local_enqueue(2, 1)?;
            o.local_dequeue(1)?;
            o.local_put(&row(u128::MAX - 3, Some(2), "f.txt", LocalKind::File))?;
            o.local_put(&LocalRow { skip: Skip::Excluded, ..row(4, Some(2), "node_modules", LocalKind::Folder) })?;
            o.local_put(&row(5, Some(1), "link", LocalKind::Link))?;
            o.local_dequeue(2)?;
            o.local_finish(9)
        })
        .unwrap();
    }

    #[test]
    fn scan_roundtrip_paths_and_stats() {
        let mut s = Store::open_in_memory().unwrap();
        scanned(&mut s);
        s.read(|o| {
            let r = o.local_root()?.unwrap();
            assert_eq!(r, LocalRoot { finished_at_ms: Some(9), ..root() });
            assert_eq!(o.local_path_of(u128::MAX - 3)?.as_deref(), Some("/a/f.txt"));
            assert_eq!(o.local_path_of(1)?.as_deref(), Some("/"));
            assert_eq!(o.local_excluded_paths()?, ["/a/node_modules"]);
            let st = o.local_stats()?;
            assert_eq!((st.files, st.folders, st.excluded_folders, st.links, st.queued), (1, 3, 1, 1, 0));
            assert_eq!(o.local_children(2)?.len(), 2);
            assert!(o.local_scan_is_current()?);
            assert!(o.check_invariants()?.is_empty(), "{:?}", o.check_invariants()?);
            Ok(())
        })
        .unwrap();
    }

    #[test]
    fn puts_are_idempotent_and_hard_links_are_reported() {
        let mut s = Store::open_in_memory().unwrap();
        scanned(&mut s);
        s.write(|o| {
            let again = LocalRow { size: 99, ..row(u128::MAX - 3, Some(2), "f.txt", LocalKind::File) };
            assert_eq!(o.local_put(&again)?, PutOutcome::Stored);
            assert_eq!(o.local_get(u128::MAX - 3)?.unwrap().size, 99);
            // Same file ID under another name: kept where it was first seen.
            let link = row(u128::MAX - 3, Some(1), "hardlink.txt", LocalKind::File);
            assert_eq!(o.local_put(&link)?, PutOutcome::Duplicate);
            assert_eq!(o.local_path_of(u128::MAX - 3)?.as_deref(), Some("/a/f.txt"));
            // The root (NULL parent) re-put is an update, not a duplicate.
            assert_eq!(o.local_put(&row(1, None, "", LocalKind::Root))?, PutOutcome::Stored);
            Ok(())
        })
        .unwrap();
    }

    #[test]
    fn begin_replaces_the_previous_scan() {
        let mut s = Store::open_in_memory().unwrap();
        scanned(&mut s);
        s.write(|o| o.local_begin(&LocalRoot { root_file_id: 77, ..root() }, &row(77, None, "", LocalKind::Root)))
            .unwrap();
        s.read(|o| {
            let st = o.local_stats()?;
            assert_eq!((st.files, st.folders, st.queued), (0, 1, 1));
            assert_eq!(o.local_root()?.unwrap().finished_at_ms, None);
            assert_eq!(o.local_next_dirs(10)?, vec![(77, 0)]);
            Ok(())
        })
        .unwrap();
    }

    #[test]
    fn queue_is_deepest_first() {
        let mut s = Store::open_in_memory().unwrap();
        s.write(|o| {
            o.local_begin(&root(), &row(1, None, "", LocalKind::Root))?;
            o.local_put(&row(2, Some(1), "a", LocalKind::Folder))?;
            o.local_put(&row(3, Some(2), "b", LocalKind::Folder))?;
            o.local_enqueue(2, 1)?;
            o.local_enqueue(3, 2)?;
            assert_eq!(o.local_next_dirs(2)?, vec![(3, 2), (2, 1)]);
            Ok(())
        })
        .unwrap();
    }

    #[test]
    fn updates_move_entries_and_subtrees_are_removed_with_their_queue() {
        let mut s = Store::open_in_memory().unwrap();
        scanned(&mut s);
        s.write(|o| {
            // Move f.txt from /a to the root under a new name.
            o.local_update(&row(u128::MAX - 3, Some(1), "g.txt", LocalKind::File))?;
            assert_eq!(o.local_path_of(u128::MAX - 3)?.as_deref(), Some("/g.txt"));
            o.local_put(&row(6, Some(2), "b", LocalKind::Folder))?;
            o.local_enqueue(6, 2)?;
            // /a now holds b (queued) and node_modules: both go, and the queue empties.
            assert_eq!(o.local_remove_children(2)?, 2);
            assert!(o.local_get(2)?.is_some());
            assert_eq!(o.local_remove_subtree(2)?, 1);
            assert_eq!(o.local_stats()?.queued, 0);
            o.local_set_journal_cursor(77)?;
            assert_eq!(o.local_root()?.and_then(|r| r.journal), Some((u64::MAX - 5, 77)));
            assert!(o.check_invariants()?.is_empty(), "{:?}", o.check_invariants()?);
            Ok(())
        })
        .unwrap();
    }

    #[test]
    fn local_invariant_violations_are_reported() {
        let mut s = Store::open_in_memory().unwrap();
        scanned(&mut s);
        s.write(|o| {
            // A child under an excluded folder, and a skipped file.
            o.local_put(&row(10, Some(4), "inside", LocalKind::File))?;
            o.c.execute("UPDATE local_item SET skip = 3 WHERE file_id = ?1", [blob(5)])?;
            Ok(())
        })
        .unwrap();
        let problems = s.read(|o| o.check_invariants()).unwrap();
        assert_eq!(problems.len(), 2, "{problems:?}");
    }
}
