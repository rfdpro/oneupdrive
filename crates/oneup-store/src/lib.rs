//! OneUpDrive's local state database.
//!
//! It holds the **Remote tree**, a metadata mirror of the OneDrive drive,
//! plus the bookkeeping that keeps it correct and resumable:
//!
//! * `remote_item`: every *included* item, plus excluded folders as single
//!   opaque rows (`is_excluded = 1`) whose contents are never stored.
//! * `remote_pruned`: tombstones (id, parent) for folders found *inside*
//!   excluded subtrees, so later delta entries beneath them are recognised
//!   and dropped regardless of arrival order.
//! * `remote_orphan`: items whose parent hasn't been seen yet (out-of-order
//!   delivery); resolved once the parent arrives or is fetched.
//! * `enum_unit`: resumable progress of the initial enumeration.
//! * `cursor`: the delta cursor (deltaLink).
//!
//! Since schema v3 it also holds the **Local tree**, a read-only scan of a
//! local folder (see [`local`]), and since v4 the **Synced tree** with the
//! deletion evidence the reconciler relies on (see [`synced`]), and since v5
//! the marks of folders that need planning again (see [`dirty`]).
//!
//! The Remote and Local trees are *derived metadata*: rows can always be
//! rebuilt from OneDrive or by rescanning. The Synced tree is not: it is the
//! record of what both sides agreed on, so rebuilds and rescans never touch
//! it. User file content is never touched by this crate.
//!
//! Remote-tree invariants (checked by [`Ops::check_invariants`] and in tests,
//! together with the Local tree's own):
//! 1. at most one root row;
//! 2. every non-root row's parent exists in `remote_item` and is not excluded;
//! 3. no id is both stored and pruned;
//! 4. only folders are marked excluded.

use std::path::Path;

use rusqlite::{Connection, OptionalExtension, Row, TransactionBehavior, params};

pub mod local;
pub use local::{LOCAL_SCAN_FORMAT, LocalKind, LocalRoot, LocalRow, LocalStats, PutOutcome, Skip};
pub mod synced;
pub use synced::SyncedStats;
pub mod dirty;
pub use dirty::DirtyScope;
pub mod activity;
mod rulefile;
pub mod usage;
pub use activity::{Entry as ActivityEntry, Outcome as ActivityOutcome};
pub use usage::{AppFile, AppUse, UseKind};

#[derive(Debug, thiserror::Error)]
pub enum Error {
    #[error(transparent)]
    Sqlite(#[from] rusqlite::Error),
    #[error("database schema version {found} is newer than this build supports ({supported})")]
    TooNew { found: i64, supported: i64 },
    #[error("corrupt row: {0}")]
    Corrupt(String),
}

impl Error {
    /// Another connection (the agent's hydration service, the settings
    /// window, a command) held the database longer than [`BUSY_WAIT`]:
    /// nothing was changed, and trying again soon is the cure.
    pub fn is_busy(&self) -> bool {
        matches!(
            self,
            Error::Sqlite(rusqlite::Error::SqliteFailure(e, _))
                if matches!(e.code, rusqlite::ErrorCode::DatabaseBusy | rusqlite::ErrorCode::DatabaseLocked)
        )
    }

    /// The database itself is damaged, or doesn't fit this build.
    pub fn is_damaged(&self) -> bool {
        match self {
            Error::Corrupt(_) | Error::TooNew { .. } => true,
            Error::Sqlite(rusqlite::Error::SqliteFailure(e, _)) => {
                matches!(e.code, rusqlite::ErrorCode::DatabaseCorrupt | rusqlite::ErrorCode::NotADatabase)
            }
            _ => false,
        }
    }

    /// The disk the database is on is full.
    pub fn is_disk_full(&self) -> bool {
        matches!(self, Error::Sqlite(rusqlite::Error::SqliteFailure(e, _)) if e.code == rusqlite::ErrorCode::DiskFull)
    }
}

/// How long a connection waits for another one's write to finish before
/// giving up with "database is locked". Several processes share the
/// database (the agent, its hydration service, the settings window, the
/// CLI), and some writes are long (applying changed rules to a big tree).
pub const BUSY_WAIT: std::time::Duration = std::time::Duration::from_secs(60);

pub type Result<T> = std::result::Result<T, Error>;

const SCHEMA_VERSION: i64 = 15;

/// v1 -> v2: orphans remember whether they arrived during catch-up. Existing
/// orphans default to "yes", the conservative choice (their folders get
/// fully enumerated when placed).
const MIGRATE_V2: &str = "ALTER TABLE remote_orphan ADD COLUMN catch_up INTEGER NOT NULL DEFAULT 1;";

/// v2 -> v3: the Local tree (a read-only scan of a local folder).
///
/// * `local_root`: the one scanned folder and its identity (volume serial +
///   root file ID, `data-safety.md` invariant 4), the rules it was scanned with,
///   and the change-journal position taken *before* the scan started, so a
///   later observer can replay everything that changed during the scan.
/// * `local_item`: one row per entry, keyed by the 128-bit file ID
///   (little-endian BLOB). Folders whose contents were not listed say why in
///   `skip` (excluded, unpopulated placeholder, unreadable).
/// * `local_scan_queue`: folders still to be listed; a scan resumes from it.
const MIGRATE_V3: &str = "
CREATE TABLE local_root (
    id                 INTEGER PRIMARY KEY CHECK (id = 1),
    path               TEXT NOT NULL,
    volume_serial      INTEGER NOT NULL,
    root_file_id       BLOB NOT NULL,
    rules              TEXT NOT NULL,
    journal_id         INTEGER,
    start_usn          INTEGER,
    started_at         INTEGER NOT NULL,
    finished_at        INTEGER,
    sync_root_provider TEXT
);

CREATE TABLE local_item (
    file_id     BLOB PRIMARY KEY,
    parent_id   BLOB,
    name        TEXT NOT NULL,
    name_lossy  INTEGER NOT NULL DEFAULT 0,
    kind        INTEGER NOT NULL,
    size        INTEGER NOT NULL DEFAULT 0,
    mtime       INTEGER NOT NULL DEFAULT 0,
    attributes  INTEGER NOT NULL DEFAULT 0,
    reparse_tag INTEGER NOT NULL DEFAULT 0,
    skip        INTEGER NOT NULL DEFAULT 0
) WITHOUT ROWID;
CREATE INDEX local_item_parent ON local_item(parent_id);

CREATE TABLE local_scan_queue (
    file_id BLOB PRIMARY KEY,
    depth   INTEGER NOT NULL
) WITHOUT ROWID;
CREATE INDEX local_scan_queue_depth ON local_scan_queue(depth);
";

const SCHEMA_V1: &str = "
CREATE TABLE meta (
    key   TEXT PRIMARY KEY,
    value TEXT NOT NULL
) WITHOUT ROWID;

CREATE TABLE remote_item (
    id          TEXT PRIMARY KEY,
    parent_id   TEXT,
    name        TEXT NOT NULL,
    kind        INTEGER NOT NULL,
    size        INTEGER NOT NULL DEFAULT 0,
    etag        TEXT,
    qxh         BLOB,
    mtime       INTEGER,
    is_excluded INTEGER NOT NULL DEFAULT 0
) WITHOUT ROWID;
CREATE INDEX remote_item_parent ON remote_item(parent_id);

CREATE TABLE remote_pruned (
    id        TEXT PRIMARY KEY,
    parent_id TEXT
) WITHOUT ROWID;
CREATE INDEX remote_pruned_parent ON remote_pruned(parent_id);

CREATE TABLE remote_orphan (
    id        TEXT PRIMARY KEY,
    parent_id TEXT NOT NULL,
    name      TEXT NOT NULL,
    kind      INTEGER NOT NULL,
    size      INTEGER NOT NULL DEFAULT 0,
    etag      TEXT,
    qxh       BLOB,
    mtime     INTEGER
) WITHOUT ROWID;
CREATE INDEX remote_orphan_parent ON remote_orphan(parent_id);

CREATE TABLE enum_unit (
    folder_id TEXT PRIMARY KEY,
    path      TEXT NOT NULL,
    kind      INTEGER NOT NULL,
    depth     INTEGER NOT NULL,
    state     INTEGER NOT NULL,
    next_link TEXT,
    items     INTEGER NOT NULL DEFAULT 0
) WITHOUT ROWID;

CREATE TABLE cursor (
    scope      TEXT PRIMARY KEY,
    delta_link TEXT NOT NULL,
    taken_at   INTEGER NOT NULL,
    state      INTEGER NOT NULL
) WITHOUT ROWID;
";

/// What a remote item is.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum Kind {
    File = 0,
    Folder = 1,
    Root = 2,
    /// A package such as a OneNote notebook: opaque, never descended into.
    Package = 3,
}

impl TryFrom<i64> for Kind {
    type Error = Error;
    fn try_from(v: i64) -> Result<Self> {
        Ok(match v {
            0 => Kind::File,
            1 => Kind::Folder,
            2 => Kind::Root,
            3 => Kind::Package,
            _ => return Err(Error::Corrupt(format!("kind {v}"))),
        })
    }
}

/// One remote item's metadata.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RemoteRow {
    pub id: String,
    pub parent_id: Option<String>,
    pub name: String,
    pub kind: Kind,
    pub size: i64,
    pub etag: Option<String>,
    /// QuickXorHash (files only).
    pub qxh: Option<[u8; 20]>,
    /// Last modified, Unix milliseconds.
    pub mtime_ms: Option<i64>,
}

impl RemoteRow {
    fn from_row(r: &Row<'_>) -> rusqlite::Result<(i64, Self)> {
        let qxh: Option<Vec<u8>> = r.get("qxh")?;
        let kind: i64 = r.get("kind")?;
        Ok((
            kind,
            Self {
                id: r.get("id")?,
                parent_id: r.get("parent_id")?,
                name: r.get("name")?,
                kind: Kind::File, // replaced by the caller after validation
                size: r.get("size")?,
                etag: r.get("etag")?,
                qxh: qxh.and_then(|v| <[u8; 20]>::try_from(v.as_slice()).ok()),
                mtime_ms: r.get("mtime")?,
            },
        ))
    }
}

/// Where an id currently stands in the mirror.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ItemState {
    Absent,
    /// Stored and included.
    Included,
    /// Stored as an excluded folder (contents not stored).
    Excluded,
    /// Known to be inside an excluded subtree.
    Pruned,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum UnitKind {
    /// List the folder's direct children (shallow levels of the initial enumeration).
    List = 0,
    /// Folder-scoped delta over the whole subtree.
    Delta = 1,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Unit {
    pub folder_id: String,
    pub path: String,
    pub kind: UnitKind,
    pub depth: u32,
    pub next_link: Option<String>,
    pub items: i64,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum CursorState {
    /// Taken before the initial enumeration; must still be replayed.
    Pending = 0,
    /// The mirror is consistent with this cursor.
    Current = 1,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Cursor {
    pub delta_link: String,
    pub taken_at_ms: i64,
    pub state: CursorState,
}

#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Stats {
    pub files: i64,
    pub folders: i64,
    pub packages: i64,
    pub excluded_folders: i64,
    pub pruned_folders: i64,
    pub orphans: i64,
    pub units_pending: i64,
    pub units_done: i64,
    pub file_bytes: i64,
    pub db_bytes: i64,
}

pub struct Store {
    conn: Connection,
}

/// Free pages [`Space::worth_compacting`] leaves alone: compacting costs a
/// full copy, worth it only for a lot of space.
pub const COMPACT_MIN_FREE: u64 = 64 << 20;

/// How the database file's pages are used.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Space {
    /// All pages, in bytes.
    pub total: u64,
    /// Pages deleted rows left free, in bytes.
    pub free: u64,
}

impl Space {
    /// Free pages are kept inside the file, which then stays at its largest
    /// size for good (one was 80% free). Compacting is worth its full copy
    /// when at least [`COMPACT_MIN_FREE`] and a quarter of the file are
    /// free.
    pub fn worth_compacting(self) -> bool {
        self.free >= COMPACT_MIN_FREE && self.free * 4 >= self.total
    }

    /// What a compacted copy holds.
    pub fn used(self) -> u64 {
        self.total - self.free
    }
}

/// What [`Store::compact`] did: the file's size on disk before and after.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Compacted {
    pub before: u64,
    pub after: u64,
    /// Another connection was reading, so the rewrite is still in the WAL:
    /// the file shrinks at a later checkpoint, once nobody reads the old
    /// pages.
    pub pending: bool,
}

/// What connections with [`Store::log_statements`] on have run.
#[cfg(feature = "statement-log")]
static STATEMENTS: std::sync::Mutex<Vec<String>> = std::sync::Mutex::new(Vec::new());

/// Tables small enough to read whole at every round: what a round may
/// scan ([`Store::tree_scans`]). Everything else grows with the tree.
#[cfg(feature = "statement-log")]
const SMALL_TABLES: &[&str] = &["meta", "cursor", "enum_unit", "local_root", "remote_orphan", "dirty", "t_sub"];

#[cfg(feature = "statement-log")]
impl Store {
    /// Test support: records every statement this connection runs from now
    /// on (as written, parameters unbound), until [`Store::take_statements`].
    pub fn log_statements(&mut self) {
        self.conn.trace_v2(
            rusqlite::trace::TraceEventCodes::SQLITE_TRACE_STMT,
            Some(|e| {
                if let rusqlite::trace::TraceEvent::Stmt(_, sql) = e {
                    let mut all = STATEMENTS.lock().unwrap_or_else(|p| p.into_inner());
                    if !all.iter().any(|s| s == sql) {
                        all.push(sql.to_string());
                    }
                }
            }),
        );
    }

    /// The statements recorded so far (by every logging connection),
    /// clearing the record.
    pub fn take_statements() -> Vec<String> {
        std::mem::take(&mut *STATEMENTS.lock().unwrap_or_else(|p| p.into_inner()))
    }

    /// The steps of `statements`' query plans that read a whole table other
    /// than the [`SMALL_TABLES`], each with its statement: what makes a
    /// query's cost follow the size of the tree instead of the changes.
    /// Parameters are bound to NULL, which doesn't change a plan's shape. A
    /// statement that can't be explained is an error, not skipped.
    pub fn tree_scans(&self, statements: &[String]) -> Result<Vec<String>> {
        let mut out = Vec::new();
        // Trigger bodies are logged as comments; their plans aren't shown.
        for sql in statements.iter().filter(|s| !s.trim_start().starts_with("--")) {
            let mut st = self.conn.prepare(&format!("EXPLAIN QUERY PLAN {sql}"))?;
            let nulls = std::iter::repeat_n(rusqlite::types::Null, st.parameter_count());
            let steps: Vec<String> =
                st.query_map(rusqlite::params_from_iter(nulls), |r| r.get(3))?.collect::<rusqlite::Result<_>>()?;
            for step in steps {
                let Some(name) = step.strip_prefix("SCAN ").and_then(|t| t.split(' ').next()) else { continue };
                if !SMALL_TABLES.contains(&table_of(sql, name)) && !step.contains("CONSTANT ROW") {
                    out.push(format!("{step}  <-  {sql}"));
                }
            }
        }
        Ok(out)
    }
}

/// The table `name` stands for in `sql`: itself, or the table it's an alias
/// of (`FROM remote_orphan o`, `JOIN remote_item AS r`).
#[cfg(feature = "statement-log")]
fn table_of<'a>(sql: &'a str, name: &'a str) -> &'a str {
    let words: Vec<&str> =
        sql.split(|c: char| c.is_whitespace() || c == ',' || c == '(' || c == ')').filter(|w| !w.is_empty()).collect();
    for i in 0..words.len().saturating_sub(2) {
        if !(words[i].eq_ignore_ascii_case("FROM") || words[i].eq_ignore_ascii_case("JOIN")) {
            continue;
        }
        let alias = if words[i + 2].eq_ignore_ascii_case("AS") { words.get(i + 3) } else { words.get(i + 2) };
        if alias == Some(&name) {
            return words[i + 1];
        }
    }
    name
}

impl Store {
    /// Opens (creating if needed) the database. `cache_kib` bounds SQLite's
    /// page cache, which is how the memory profile reaches the store.
    pub fn open(path: &Path, cache_kib: u32) -> Result<Self> {
        Self::init(Connection::open(path)?, cache_kib)
    }

    pub fn open_in_memory() -> Result<Self> {
        Self::init(Connection::open_in_memory()?, 2048)
    }

    fn init(conn: Connection, cache_kib: u32) -> Result<Self> {
        conn.busy_timeout(BUSY_WAIT)?;
        // The mirror is rebuildable from the cloud, so NORMAL durability (WAL)
        // is sufficient here. The future op journal will use FULL.
        conn.execute_batch(&format!(
            "PRAGMA journal_mode = WAL;
             PRAGMA synchronous = NORMAL;
             PRAGMA foreign_keys = OFF;
             PRAGMA cache_size = -{cache_kib};
             PRAGMA temp_store = FILE;"
        ))?;
        let version: i64 = conn.query_row("PRAGMA user_version", [], |r| r.get(0))?;
        if version > SCHEMA_VERSION {
            return Err(Error::TooNew { found: version, supported: SCHEMA_VERSION });
        }
        // Each step runs in its own transaction, so an interrupted upgrade
        // resumes from the last completed version.
        if version < 1 {
            conn.execute_batch(&format!("BEGIN; {SCHEMA_V1} PRAGMA user_version = 1; COMMIT;"))?;
        }
        if version < 2 {
            conn.execute_batch(&format!("BEGIN; {MIGRATE_V2} PRAGMA user_version = 2; COMMIT;"))?;
        }
        if version < 3 {
            conn.execute_batch(&format!("BEGIN; {MIGRATE_V3} PRAGMA user_version = 3; COMMIT;"))?;
        }
        if version < 4 {
            conn.execute_batch(&format!("BEGIN; {} PRAGMA user_version = 4; COMMIT;", synced::MIGRATE_V4))?;
        }
        if version < 5 {
            conn.execute_batch(&format!("BEGIN; {} PRAGMA user_version = 5; COMMIT;", dirty::migrate_v5()))?;
        }
        if version < 6 {
            conn.execute_batch(&format!("BEGIN; {} PRAGMA user_version = 6; COMMIT;", activity::MIGRATE_V6))?;
        }
        if version < 7 {
            conn.execute_batch(&format!("BEGIN; {} PRAGMA user_version = 7; COMMIT;", rulefile::MIGRATE_V7))?;
        }
        if version < 8 {
            conn.execute_batch(&format!("BEGIN; {} PRAGMA user_version = 8; COMMIT;", rulefile::MIGRATE_V8))?;
        }
        if version < 9 {
            conn.execute_batch(&format!("BEGIN; {MIGRATE_V9} PRAGMA user_version = 9; COMMIT;"))?;
        }
        if version < 10 {
            conn.execute_batch(&format!("BEGIN; {} PRAGMA user_version = 10; COMMIT;", usage::MIGRATE_V10))?;
        }
        if version < 11 {
            conn.execute_batch(&format!("BEGIN; {} PRAGMA user_version = 11; COMMIT;", usage::MIGRATE_V11))?;
        }
        if version < 12 {
            conn.execute_batch(&format!("BEGIN; {} PRAGMA user_version = 12; COMMIT;", synced::MIGRATE_V12))?;
        }
        if version < 13 {
            conn.execute_batch(&format!("BEGIN; {} PRAGMA user_version = 13; COMMIT;", dirty::migrate_v13()))?;
        }
        if version < 14 {
            conn.execute_batch(&format!("BEGIN; {} PRAGMA user_version = 14; COMMIT;", rulefile::MIGRATE_V14))?;
        }
        if version < 15 {
            conn.execute_batch(&format!("BEGIN; {} PRAGMA user_version = 15; COMMIT;", synced::MIGRATE_V15))?;
        }
        conn.execute_batch(
            "CREATE TEMP TABLE IF NOT EXISTS t_sub (id TEXT PRIMARY KEY) WITHOUT ROWID;
             CREATE TEMP TABLE IF NOT EXISTS t_local_sub (id BLOB PRIMARY KEY) WITHOUT ROWID;",
        )?;
        Ok(Self { conn })
    }

    /// Runs `f` in one transaction; commits only if it returns `Ok`.
    ///
    /// The transaction takes the write lock when it starts (`IMMEDIATE`),
    /// waiting up to [`BUSY_WAIT`] for another connection's write. A
    /// deferred one would read first and only then ask for the lock, and in
    /// WAL mode SQLite fails that at once ("database is locked", without
    /// waiting) when another connection committed in between, which has
    /// stopped the agent.
    pub fn write<T>(&mut self, f: impl FnOnce(&Ops<'_>) -> Result<T>) -> Result<T> {
        let tx = self.conn.transaction_with_behavior(TransactionBehavior::Immediate)?;
        let out = f(&Ops { c: &tx })?;
        tx.commit()?;
        Ok(out)
    }

    /// [`Store::write`] for callers with their own error type: any `Err`
    /// (theirs or the store's) rolls the whole transaction back.
    pub fn try_write<T, E: From<Error>>(
        &mut self,
        f: impl FnOnce(&Ops<'_>) -> std::result::Result<T, E>,
    ) -> std::result::Result<T, E> {
        let tx = self.conn.transaction_with_behavior(TransactionBehavior::Immediate).map_err(Error::from)?;
        let out = f(&Ops { c: &tx })?;
        tx.commit().map_err(Error::from)?;
        Ok(out)
    }

    /// Reads this connection's pages through a memory map of up to `bytes`
    /// of the file, instead of one system call per 4 KiB page: a walk of a
    /// large tree measured no read calls and a fifth less time this way.
    /// For the agent's own connection only. Windows can't shrink a file
    /// another process has mapped, so the other connections (the tray, the
    /// settings window, the CLI) don't map, and [`Store::compact`] can
    /// return the space. A disk error in a mapped page ends the process
    /// instead of failing a query.
    pub fn map_memory(&self, bytes: u64) -> Result<()> {
        self.conn.execute_batch(&format!("PRAGMA mmap_size = {bytes};"))?;
        Ok(())
    }

    /// How the file's pages are used.
    pub fn space(&self) -> Result<Space> {
        let pages = |sql: &str| -> Result<u64> {
            Ok(u64::try_from(self.conn.query_row(sql, [], |r| r.get::<_, i64>(0))?).unwrap_or(0))
        };
        let size = pages("PRAGMA page_size")?;
        Ok(Space { total: pages("PRAGMA page_count")? * size, free: pages("PRAGMA freelist_count")? * size })
    }

    /// Rewrites the database without its free pages, and returns the file's
    /// size on disk before and after. The file shrinks only once the rewrite
    /// is checkpointed ([`Compacted::pending`] otherwise) and no other
    /// process has the file mapped; until then `after` is no smaller
    /// (nothing is lost either way: SQLite reuses the space). Needs room on
    /// the disk for up to twice the file ([`Store::files_size`]).
    pub fn compact(&self) -> Result<Compacted> {
        let on_disk = || self.conn.path().and_then(|p| std::fs::metadata(p).ok()).map_or(0, |m| m.len());
        let before = on_disk();
        self.conn.execute_batch("PRAGMA mmap_size = 0; VACUUM;")?;
        let pending = !self.checkpoint()?;
        Ok(Compacted { before, after: on_disk(), pending })
    }

    /// The database's files on disk: the database and its WAL.
    pub fn files_size(&self) -> u64 {
        let Some(path) = self.conn.path() else { return 0 };
        let len = |p: &str| std::fs::metadata(p).map_or(0, |m| m.len());
        len(path) + len(&format!("{path}-wal"))
    }

    /// Folds the write-ahead log into the database and empties it, as far
    /// as other connections' reads allow (they keep what they still read).
    /// Returns whether it all went (`false`: a reader held some back).
    pub fn checkpoint(&self) -> Result<bool> {
        let busy: i64 = self.conn.query_row("PRAGMA wal_checkpoint(TRUNCATE)", [], |r| r.get(0))?;
        Ok(busy == 0)
    }

    /// Read-only access outside an explicit transaction.
    pub fn read<T>(&self, f: impl FnOnce(&Ops<'_>) -> Result<T>) -> Result<T> {
        f(&Ops { c: &self.conn })
    }
}

/// Operations on an open connection or transaction.
pub struct Ops<'a> {
    c: &'a Connection,
}

const ITEM_COLS: &str = "id, parent_id, name, kind, size, etag, qxh, mtime";

/// Cloud files at least this large are worth copying in OneDrive instead of
/// uploading when a new local file has the same content (server-side
/// copy). Smaller ones upload in about the time a copy takes.
pub const COPY_MIN: i64 = 1 << 20;

/// v9: cloud files by size, for finding content OneDrive already has
/// (only files of at least [`COPY_MIN`] with a hash, so the index stays
/// a fraction of the table).
const MIGRATE_V9: &str = "
CREATE INDEX remote_item_copyable ON remote_item(size)
    WHERE kind = 0 AND size >= 1048576 AND qxh IS NOT NULL;
";

/// The folders of the subtree in `t_sub` become tombstones.
const PRUNE_TOMBSTONES: &str = "
INSERT OR REPLACE INTO remote_pruned(id, parent_id)
SELECT r.id, r.parent_id FROM remote_item r
WHERE r.id IN (SELECT id FROM t_sub) AND r.kind = 1";

impl Ops<'_> {
    // ------------------------------------------------------------ meta

    pub fn meta_get(&self, key: &str) -> Result<Option<String>> {
        Ok(self.c.query_row("SELECT value FROM meta WHERE key = ?1", [key], |r| r.get(0)).optional()?)
    }

    pub fn meta_set(&self, key: &str, value: &str) -> Result<()> {
        self.c.execute(
            "INSERT INTO meta(key, value) VALUES (?1, ?2) ON CONFLICT(key) DO UPDATE SET value = excluded.value",
            [key, value],
        )?;
        Ok(())
    }

    pub fn meta_delete(&self, key: &str) -> Result<()> {
        self.c.execute("DELETE FROM meta WHERE key = ?1", [key])?;
        Ok(())
    }

    // ------------------------------------------------------------ items

    pub fn item_state(&self, id: &str) -> Result<ItemState> {
        let excluded: Option<i64> = self
            .c
            .prepare_cached("SELECT is_excluded FROM remote_item WHERE id = ?1")?
            .query_row([id], |r| r.get(0))
            .optional()?;
        if let Some(x) = excluded {
            return Ok(if x != 0 { ItemState::Excluded } else { ItemState::Included });
        }
        let pruned = self.c.prepare_cached("SELECT 1 FROM remote_pruned WHERE id = ?1")?.exists([id])?;
        Ok(if pruned { ItemState::Pruned } else { ItemState::Absent })
    }

    /// Up to `limit` cloud files of exactly `size` bytes (at least
    /// [`COPY_MIN`]) with a hash: candidates for a server-side copy.
    pub fn remote_files_of_size(&self, size: i64, limit: usize) -> Result<Vec<RemoteRow>> {
        if size < COPY_MIN {
            return Ok(Vec::new());
        }
        let rows = self
            .c
            .prepare_cached(&format!(
                "SELECT {ITEM_COLS} FROM remote_item
                 WHERE kind = 0 AND size >= 1048576 AND qxh IS NOT NULL AND size = ?1 LIMIT ?2"
            ))?
            .query_map(params![size, limit as i64], RemoteRow::from_row)?
            .collect::<rusqlite::Result<Vec<_>>>()?;
        rows.into_iter().map(fix_kind).collect()
    }

    pub fn get_item(&self, id: &str) -> Result<Option<RemoteRow>> {
        let row = self
            .c
            .prepare_cached(&format!("SELECT {ITEM_COLS} FROM remote_item WHERE id = ?1"))?
            .query_row([id], RemoteRow::from_row)
            .optional()?;
        row.map(fix_kind).transpose()
    }

    /// Inserts or replaces an item (idempotent: delta may repeat entries).
    pub fn upsert_item(&self, row: &RemoteRow, excluded: bool) -> Result<()> {
        debug_assert!(!excluded || row.kind == Kind::Folder, "only folders can be excluded");
        self.c
            .prepare_cached(
                "INSERT INTO remote_item (id, parent_id, name, kind, size, etag, qxh, mtime, is_excluded)
                 VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9)
                 ON CONFLICT(id) DO UPDATE SET
                    parent_id = excluded.parent_id, name = excluded.name, kind = excluded.kind,
                    size = excluded.size, etag = excluded.etag, qxh = excluded.qxh,
                    mtime = excluded.mtime, is_excluded = excluded.is_excluded",
            )?
            .execute(params![
                row.id,
                row.parent_id,
                row.name,
                row.kind as i64,
                row.size,
                row.etag,
                row.qxh.map(|h| h.to_vec()),
                row.mtime_ms,
                excluded as i64
            ])?;
        Ok(())
    }

    /// Loads `id` and all its stored descendants into the temp `t_sub` table.
    /// How many items are stored below `id` (not counting it).
    pub fn remote_subtree_count(&self, id: &str) -> Result<u64> {
        self.load_subtree(id)?;
        let n: i64 = self.c.query_row("SELECT count(*) FROM t_sub", [], |r| r.get(0))?;
        Ok(u64::try_from(n - 1).unwrap_or(0))
    }

    fn load_subtree(&self, id: &str) -> Result<()> {
        // Everything that reads `t_sub` back must look rows up from it (`IN`),
        // not join it: with no table statistics SQLite drives such a join
        // from the big table, a full scan per call (`subtree_queries_never_scan`).
        self.c.execute("DELETE FROM t_sub", [])?;
        self.c.execute(
            "INSERT INTO t_sub(id)
             WITH RECURSIVE sub(id) AS (
                 SELECT ?1
                 UNION
                 SELECT r.id FROM remote_item r JOIN sub ON r.parent_id = sub.id
             ) SELECT id FROM sub",
            [id],
        )?;
        Ok(())
    }

    /// Removes `id` and everything below it (stored rows, tombstones,
    /// orphans waiting on them). Used for remote deletions only: it records
    /// the deletion as evidence for every Synced item in the subtree, so it
    /// must never be used to "tidy up" rows that are merely unaccounted for.
    /// Returns the number of stored rows removed.
    pub fn delete_subtree(&self, id: &str) -> Result<usize> {
        self.load_subtree(id)?;
        self.note_remote_subtree_deleted()?;
        self.c.execute(
            "DELETE FROM remote_pruned WHERE id IN (
                 WITH RECURSIVE ps(id) AS (
                     SELECT id FROM t_sub
                     UNION
                     SELECT p.id FROM remote_pruned p JOIN ps ON p.parent_id = ps.id
                 ) SELECT id FROM ps)",
            [],
        )?;
        self.c.execute("DELETE FROM remote_orphan WHERE id IN (SELECT id FROM t_sub)", [])?;
        Ok(self.c.execute("DELETE FROM remote_item WHERE id IN (SELECT id FROM t_sub)", [])?)
    }

    /// Moves `id`'s stored subtree into tombstones: folders become
    /// `remote_pruned` rows and all stored rows are removed. With
    /// `include_self = false`, `id` itself stays stored (it is becoming an
    /// excluded folder). Returns the number of stored rows removed.
    pub fn prune_subtree(&self, id: &str, include_self: bool) -> Result<usize> {
        self.load_subtree(id)?;
        if !include_self {
            self.c.execute("DELETE FROM t_sub WHERE id = ?1", [id])?;
        }
        self.c.execute(PRUNE_TOMBSTONES, [])?;
        Ok(self.c.execute("DELETE FROM remote_item WHERE id IN (SELECT id FROM t_sub)", [])?)
    }

    pub fn add_pruned(&self, id: &str, parent_id: &str) -> Result<()> {
        self.c
            .prepare_cached("INSERT OR REPLACE INTO remote_pruned(id, parent_id) VALUES (?1, ?2)")?
            .execute([id, parent_id])?;
        Ok(())
    }

    /// Forgets the tombstones for `id` and everything recorded below it.
    pub fn unprune_subtree(&self, id: &str) -> Result<usize> {
        Ok(self.c.execute(
            "DELETE FROM remote_pruned WHERE id IN (
                 WITH RECURSIVE ps(id) AS (
                     SELECT ?1
                     UNION
                     SELECT p.id FROM remote_pruned p JOIN ps ON p.parent_id = ps.id
                 ) SELECT id FROM ps)",
            [id],
        )?)
    }

    /// Forgets the tombstone for `id` alone (its descendants' tombstones stay).
    pub fn remove_pruned_one(&self, id: &str) -> Result<()> {
        self.c.prepare_cached("DELETE FROM remote_pruned WHERE id = ?1")?.execute([id])?;
        Ok(())
    }

    /// Every stored item with its excluded flag, ordered by id. Intended for
    /// verification and tests; it reads the whole table.
    pub fn all_items(&self) -> Result<Vec<(RemoteRow, bool)>> {
        let mut st = self.c.prepare(&format!("SELECT {ITEM_COLS}, is_excluded FROM remote_item ORDER BY id"))?;
        let rows = st.query_map([], |r| Ok((RemoteRow::from_row(r)?, r.get::<_, i64>("is_excluded")? != 0)))?;
        rows.map(|r| {
            let (row, x) = r?;
            Ok((fix_kind(row)?, x))
        })
        .collect()
    }

    /// The drive's root row, if the mirror has one.
    pub fn remote_root(&self) -> Result<Option<RemoteRow>> {
        let row = self
            .c
            .prepare_cached(&format!("SELECT {ITEM_COLS} FROM remote_item WHERE kind = 2 LIMIT 1"))?
            .query_row([], RemoteRow::from_row)
            .optional()?;
        row.map(fix_kind).transpose()
    }

    /// Up to `limit` stored cloud items whose name contains `needle` (any
    /// case), for the settings window's search (a scan of the names).
    pub fn remote_find(&self, needle: &str, limit: usize) -> Result<Vec<RemoteRow>> {
        let mut st = self.c.prepare_cached(&format!(
            "SELECT {ITEM_COLS} FROM remote_item WHERE kind != 2 AND name LIKE ?1 ESCAPE '\\' LIMIT ?2"
        ))?;
        let rows = st.query_map(rusqlite::params![local::like_pattern(needle), limit as i64], RemoteRow::from_row)?;
        rows.map(|r| fix_kind(r?)).collect()
    }

    /// For each folder directly in cloud folder `parent_id`: the total size
    /// of the files anywhere inside it, as listed (see
    /// [`Ops::local_folder_totals`]).
    pub fn remote_folder_totals(&self, parent_id: &str) -> Result<Vec<(String, i64)>> {
        let mut st = self.c.prepare_cached(
            "WITH RECURSIVE t(top, id) AS (
                 SELECT id, id FROM remote_item WHERE parent_id = ?1 AND kind = 1
                 UNION ALL
                 SELECT t.top, c.id FROM remote_item c JOIN t ON c.parent_id = t.id WHERE c.kind = 1)
             SELECT t.top, COALESCE(SUM(f.size), 0) FROM t
             LEFT JOIN remote_item f ON f.parent_id = t.id AND f.kind IN (0, 3)
             GROUP BY t.top",
        )?;
        let rows = st.query_map([parent_id], |r| Ok((r.get(0)?, r.get(1)?)))?;
        rows.collect::<rusqlite::Result<_>>().map_err(Into::into)
    }

    /// The stored children of a remote folder with their excluded flags (unordered).
    pub fn remote_children(&self, parent_id: &str) -> Result<Vec<(RemoteRow, bool)>> {
        let mut st =
            self.c.prepare_cached(&format!("SELECT {ITEM_COLS}, is_excluded FROM remote_item WHERE parent_id = ?1"))?;
        let rows =
            st.query_map([parent_id], |r| Ok((RemoteRow::from_row(r)?, r.get::<_, i64>("is_excluded")? != 0)))?;
        rows.map(|r| {
            let (row, x) = r?;
            Ok((fix_kind(row)?, x))
        })
        .collect()
    }

    /// Calls `f(id, name, is_excluded)` for every stored folder.
    pub fn for_each_folder(&self, mut f: impl FnMut(&str, &str, bool)) -> Result<()> {
        let mut st = self.c.prepare("SELECT id, name, is_excluded FROM remote_item WHERE kind = 1")?;
        let mut rows = st.query([])?;
        while let Some(r) = rows.next()? {
            let (id, name, x): (String, String, i64) = (r.get(0)?, r.get(1)?, r.get(2)?);
            f(&id, &name, x != 0);
        }
        Ok(())
    }

    // ------------------------------------------------------------ orphans

    /// Buffers an entry whose parent is unknown. `catch_up` records whether
    /// it came from the incremental feed (where a folder's contents are *not*
    /// delivered alongside it).
    pub fn put_orphan(&self, row: &RemoteRow, catch_up: bool) -> Result<()> {
        let parent = row.parent_id.as_deref().ok_or_else(|| Error::Corrupt("orphan without parent".into()))?;
        self.c
            .prepare_cached(
                "INSERT OR REPLACE INTO remote_orphan (id, parent_id, name, kind, size, etag, qxh, mtime, catch_up)
                 VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9)",
            )?
            .execute(params![
                row.id,
                parent,
                row.name,
                row.kind as i64,
                row.size,
                row.etag,
                row.qxh.map(|h| h.to_vec()),
                row.mtime_ms,
                catch_up as i64
            ])?;
        Ok(())
    }

    /// Removes and returns orphans whose parent is now known (stored or
    /// pruned), each with its `catch_up` flag.
    pub fn take_resolvable_orphans(&self) -> Result<Vec<(RemoteRow, bool)>> {
        let rows: Vec<(RemoteRow, bool)> = {
            let mut st = self.c.prepare(&format!(
                "SELECT {ITEM_COLS}, catch_up FROM remote_orphan o
                 WHERE EXISTS (SELECT 1 FROM remote_item r WHERE r.id = o.parent_id)
                    OR EXISTS (SELECT 1 FROM remote_pruned p WHERE p.id = o.parent_id)"
            ))?;
            st.query_map([], |r| Ok((RemoteRow::from_row(r)?, r.get::<_, i64>("catch_up")? != 0)))?
                .map(|r| {
                    let (row, cu) = r?;
                    Ok((fix_kind(row)?, cu))
                })
                .collect::<Result<_>>()?
        };
        for (r, _) in &rows {
            self.c.prepare_cached("DELETE FROM remote_orphan WHERE id = ?1")?.execute([&r.id])?;
        }
        Ok(rows)
    }

    /// Parents that nothing in the mirror (or orphan buffer) knows about:
    /// they must be fetched to place the orphans waiting on them.
    pub fn orphan_missing_parents(&self) -> Result<Vec<String>> {
        let mut st = self.c.prepare(
            "SELECT DISTINCT o.parent_id FROM remote_orphan o
             WHERE NOT EXISTS (SELECT 1 FROM remote_orphan x WHERE x.id = o.parent_id)
               AND NOT EXISTS (SELECT 1 FROM remote_item r WHERE r.id = o.parent_id)
               AND NOT EXISTS (SELECT 1 FROM remote_pruned p WHERE p.id = o.parent_id)",
        )?;
        let v = st.query_map([], |r| r.get(0))?.collect::<rusqlite::Result<_>>()?;
        Ok(v)
    }

    /// Drops orphans waiting on `parent_id` (used when the parent turns out
    /// to no longer exist). Returns how many were dropped.
    pub fn drop_orphans_of(&self, parent_id: &str) -> Result<usize> {
        Ok(self.c.execute("DELETE FROM remote_orphan WHERE parent_id = ?1", [parent_id])?)
    }

    // ------------------------------------------------------------ enumeration units

    /// Adds a unit unless one already exists for that folder.
    pub fn put_unit(&self, u: &Unit) -> Result<()> {
        self.c
            .prepare_cached(
                "INSERT OR IGNORE INTO enum_unit (folder_id, path, kind, depth, state, next_link, items)
                 VALUES (?1, ?2, ?3, ?4, 0, ?5, ?6)",
            )?
            .execute(params![u.folder_id, u.path, u.kind as i64, u.depth, u.next_link, u.items])?;
        Ok(())
    }

    /// Adds a unit, or resets an existing one (finished or not) to start over.
    pub fn requeue_unit(&self, u: &Unit) -> Result<()> {
        self.c
            .prepare_cached(
                "INSERT INTO enum_unit (folder_id, path, kind, depth, state, next_link, items)
                 VALUES (?1, ?2, ?3, ?4, 0, NULL, 0)
                 ON CONFLICT(folder_id) DO UPDATE SET path = excluded.path, kind = excluded.kind,
                     depth = excluded.depth, state = 0, next_link = NULL",
            )?
            .execute(params![u.folder_id, u.path, u.kind as i64, u.depth])?;
        Ok(())
    }

    pub fn set_unit_progress(&self, folder_id: &str, next_link: &str, added: i64) -> Result<()> {
        self.c
            .prepare_cached("UPDATE enum_unit SET next_link = ?2, items = items + ?3 WHERE folder_id = ?1")?
            .execute(params![folder_id, next_link, added])?;
        Ok(())
    }

    pub fn finish_unit(&self, folder_id: &str, added: i64) -> Result<()> {
        self.c
            .prepare_cached(
                "UPDATE enum_unit SET state = 1, next_link = NULL, items = items + ?2 WHERE folder_id = ?1",
            )?
            .execute(params![folder_id, added])?;
        Ok(())
    }

    /// Finished units with the most downloaded entries, largest first
    /// (diagnostics: shows where enumeration cost goes).
    pub fn heaviest_units(&self, limit: usize) -> Result<Vec<Unit>> {
        let mut st = self.c.prepare(
            "SELECT folder_id, path, kind, depth, next_link, items FROM enum_unit ORDER BY items DESC LIMIT ?1",
        )?;
        let v = st
            .query_map([limit as i64], |r| {
                let kind: i64 = r.get(2)?;
                Ok(Unit {
                    folder_id: r.get(0)?,
                    path: r.get(1)?,
                    kind: if kind == 0 { UnitKind::List } else { UnitKind::Delta },
                    depth: r.get(3)?,
                    next_link: r.get(4)?,
                    items: r.get(5)?,
                })
            })?
            .collect::<rusqlite::Result<_>>()?;
        Ok(v)
    }

    /// Total entries downloaded per unit kind: (list, delta).
    pub fn unit_totals(&self) -> Result<(i64, i64)> {
        Ok(self.c.query_row(
            "SELECT coalesce(sum(CASE WHEN kind = 0 THEN items END), 0),
                    coalesce(sum(CASE WHEN kind = 1 THEN items END), 0) FROM enum_unit",
            [],
            |r| Ok((r.get(0)?, r.get(1)?)),
        )?)
    }

    pub fn pending_units(&self) -> Result<Vec<Unit>> {
        let mut st = self.c.prepare(
            "SELECT folder_id, path, kind, depth, next_link, items FROM enum_unit WHERE state = 0 ORDER BY depth, path",
        )?;
        let v = st
            .query_map([], |r| {
                let kind: i64 = r.get(2)?;
                Ok(Unit {
                    folder_id: r.get(0)?,
                    path: r.get(1)?,
                    kind: if kind == 0 { UnitKind::List } else { UnitKind::Delta },
                    depth: r.get(3)?,
                    next_link: r.get(4)?,
                    items: r.get(5)?,
                })
            })?
            .collect::<rusqlite::Result<_>>()?;
        Ok(v)
    }

    pub fn clear_units(&self) -> Result<()> {
        self.c.execute("DELETE FROM enum_unit", [])?;
        Ok(())
    }

    /// Forgets the whole Remote tree (items, tombstones, orphans, units, the
    /// cursor and the rules it was built with), so the next mirror run starts
    /// from scratch. The Local tree and user settings in `meta` are kept.
    pub fn clear_remote(&self) -> Result<()> {
        self.c.execute_batch(
            "DELETE FROM remote_item; DELETE FROM remote_pruned; DELETE FROM remote_orphan;
             DELETE FROM enum_unit; DELETE FROM cursor; DELETE FROM meta WHERE key = 'rules';",
        )?;
        Ok(())
    }

    // ------------------------------------------------------------ cursor

    pub fn cursor(&self) -> Result<Option<Cursor>> {
        let row: Option<(String, i64, i64)> = self
            .c
            .query_row("SELECT delta_link, taken_at, state FROM cursor WHERE scope = 'root'", [], |r| {
                Ok((r.get(0)?, r.get(1)?, r.get(2)?))
            })
            .optional()?;
        Ok(row.map(|(delta_link, taken_at_ms, s)| Cursor {
            delta_link,
            taken_at_ms,
            state: if s == 1 { CursorState::Current } else { CursorState::Pending },
        }))
    }

    pub fn set_cursor(&self, c: &Cursor) -> Result<()> {
        self.c.execute(
            "INSERT INTO cursor(scope, delta_link, taken_at, state) VALUES ('root', ?1, ?2, ?3)
             ON CONFLICT(scope) DO UPDATE SET delta_link = excluded.delta_link,
                 taken_at = excluded.taken_at, state = excluded.state",
            params![c.delta_link, c.taken_at_ms, c.state as i64],
        )?;
        Ok(())
    }

    // ------------------------------------------------------------ queries

    /// Full path of a stored item, built from parent links (root = "/").
    pub fn path_of(&self, id: &str) -> Result<Option<String>> {
        let mut parts = Vec::new();
        let mut cur = id.to_string();
        for _ in 0..4096 {
            let row: Option<(Option<String>, String, i64)> = self
                .c
                .prepare_cached("SELECT parent_id, name, kind FROM remote_item WHERE id = ?1")?
                .query_row([&cur], |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?)))
                .optional()?;
            match row {
                None => return Ok(None),
                Some((_, _, 2)) => {
                    parts.reverse();
                    return Ok(Some(format!("/{}", parts.join("/"))));
                }
                Some((Some(p), name, _)) => {
                    parts.push(name);
                    cur = p;
                }
                Some((None, _, _)) => return Ok(None),
            }
        }
        Err(Error::Corrupt(format!("parent chain of {id} too deep or cyclic")))
    }

    /// Drive paths ("/Code/app/node_modules") of every excluded folder in the
    /// mirror, sorted. Used as an enumeration hint before a rebuild discards
    /// them. One recursive pass over folders only.
    pub fn remote_excluded_paths(&self) -> Result<Vec<String>> {
        let mut st = self.c.prepare(
            "WITH RECURSIVE t(id, path, x) AS (
                 SELECT id, '', is_excluded FROM remote_item WHERE kind = 2
                 UNION ALL
                 SELECT c.id, t.path || '/' || c.name, c.is_excluded
                 FROM remote_item c JOIN t ON c.parent_id = t.id
                 WHERE c.kind = 1 AND t.x = 0
             )
             SELECT path FROM t WHERE x = 1 ORDER BY path",
        )?;
        let v = st.query_map([], |r| r.get(0))?.collect::<rusqlite::Result<_>>()?;
        Ok(v)
    }

    /// Whether enumeration units are still waiting, for every round: it reads
    /// only the small `enum_unit` table (unlike [`Ops::stats`], which counts
    /// whole trees).
    pub fn units_pending(&self) -> Result<bool> {
        Ok(self.c.query_row("SELECT EXISTS (SELECT 1 FROM enum_unit WHERE state = 0)", [], |r| r.get(0))?)
    }

    pub fn stats(&self) -> Result<Stats> {
        let one = |sql: &str| -> Result<i64> { Ok(self.c.query_row(sql, [], |r| r.get(0))?) };
        Ok(Stats {
            files: one("SELECT count(*) FROM remote_item WHERE kind = 0")?,
            folders: one("SELECT count(*) FROM remote_item WHERE kind IN (1, 2)")?,
            packages: one("SELECT count(*) FROM remote_item WHERE kind = 3")?,
            excluded_folders: one("SELECT count(*) FROM remote_item WHERE is_excluded = 1")?,
            pruned_folders: one("SELECT count(*) FROM remote_pruned")?,
            orphans: one("SELECT count(*) FROM remote_orphan")?,
            units_pending: one("SELECT count(*) FROM enum_unit WHERE state = 0")?,
            units_done: one("SELECT count(*) FROM enum_unit WHERE state = 1")?,
            file_bytes: one("SELECT coalesce(sum(size), 0) FROM remote_item WHERE kind = 0")?,
            db_bytes: one("SELECT page_count * page_size FROM pragma_page_count(), pragma_page_size()")?,
        })
    }

    /// Returns a description of every violated invariant (empty = healthy).
    pub fn check_invariants(&self) -> Result<Vec<String>> {
        let one = |sql: &str| -> Result<i64> { Ok(self.c.query_row(sql, [], |r| r.get(0))?) };
        let mut problems = Vec::new();
        let roots = one("SELECT count(*) FROM remote_item WHERE kind = 2")?;
        if roots > 1 {
            problems.push(format!("{roots} root rows"));
        }
        let dangling = one("SELECT count(*) FROM remote_item c LEFT JOIN remote_item p ON p.id = c.parent_id
             WHERE c.kind != 2 AND (p.id IS NULL OR p.is_excluded = 1)")?;
        if dangling > 0 {
            problems.push(format!("{dangling} rows whose parent is missing or excluded"));
        }
        let both = one("SELECT count(*) FROM remote_item r JOIN remote_pruned p ON p.id = r.id")?;
        if both > 0 {
            problems.push(format!("{both} ids both stored and pruned"));
        }
        let bad_excl = one("SELECT count(*) FROM remote_item WHERE is_excluded = 1 AND kind != 1")?;
        if bad_excl > 0 {
            problems.push(format!("{bad_excl} non-folders marked excluded"));
        }
        problems.extend(self.local_problems()?);
        problems.extend(self.synced_problems()?);
        Ok(problems)
    }
}

fn fix_kind((kind, mut row): (i64, RemoteRow)) -> Result<RemoteRow> {
    row.kind = Kind::try_from(kind)?;
    Ok(row)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    #[allow(clippy::unwrap_used)]
    fn compacting_is_worth_it_for_a_lot_of_space_that_is_a_quarter_of_the_file() {
        let mib = 1 << 20;
        let worth = |total: u64, free: u64| Space { total: total * mib, free: free * mib }.worth_compacting();
        assert!(worth(1_400, 1_100));
        assert!(worth(256, 64), "exactly a quarter and the minimum");
        assert!(!worth(257, 64), "just under a quarter");
        assert!(!worth(200, 63), "a quarter, but too little to be worth a copy");
        assert!(!worth(0, 0));
    }

    #[test]
    #[allow(clippy::unwrap_used)]
    fn compacting_returns_the_free_space_to_the_disk_and_keeps_the_rest() {
        let path = std::env::temp_dir().join(format!("oneup-store-compact-{}.db", std::process::id()));
        let _ = std::fs::remove_file(&path);
        let mut s = Store::open(&path, 1024).unwrap();
        // The agent's connection maps the file; compacting still shrinks it
        // (it unmaps first).
        s.map_memory(64 << 20).unwrap();
        s.write(|o| {
            for i in 0..4_000 {
                o.meta_set(&format!("k{i}"), &"x".repeat(1_000))?;
            }
            Ok(())
        })
        .unwrap();
        assert!(s.checkpoint().unwrap());
        assert_eq!(s.space().unwrap().free, 0);
        s.write(|o| Ok(o.c.execute("DELETE FROM meta WHERE key >= 'k1'", [])?)).unwrap();
        assert!(s.checkpoint().unwrap());
        let space = s.space().unwrap();
        assert!(space.free * 2 > space.total, "{space:?}");
        let on_disk = std::fs::metadata(&path).unwrap().len();
        assert!(s.files_size() >= on_disk);
        let c = s.compact().unwrap();
        assert_eq!(c.before, on_disk, "measured on disk");
        assert!(!c.pending);
        assert!(c.after * 2 < c.before, "{c:?}");
        assert_eq!(c.after, std::fs::metadata(&path).unwrap().len());
        assert_eq!(s.space().unwrap().free, 0);
        assert_eq!(s.read(|o| o.meta_get("k0")).unwrap().map(|v| v.len()), Some(1_000), "what's left is kept");
        drop(s);
        for ext in ["", "-wal", "-shm"] {
            let _ = std::fs::remove_file(format!("{}{ext}", path.display()));
        }
    }

    /// Another connection in the middle of a read keeps the rewrite in the
    /// WAL: the compaction says so, and the file shrinks at the next
    /// checkpoint once the read is over.
    #[test]
    #[allow(clippy::unwrap_used)]
    fn a_reader_leaves_the_compaction_pending_until_it_is_done() {
        let path = std::env::temp_dir().join(format!("oneup-store-compact-read-{}.db", std::process::id()));
        let _ = std::fs::remove_file(&path);
        let mut s = Store::open(&path, 1024).unwrap();
        s.write(|o| {
            for i in 0..4_000 {
                o.meta_set(&format!("k{i}"), &"x".repeat(1_000))?;
            }
            Ok(())
        })
        .unwrap();
        s.write(|o| Ok(o.c.execute("DELETE FROM meta WHERE key >= 'k1'", [])?)).unwrap();
        assert!(s.checkpoint().unwrap());
        let reader = Store::open(&path, 1024).unwrap();
        reader.conn.execute_batch("BEGIN; SELECT count(*) FROM meta;").unwrap();
        s.conn.busy_timeout(std::time::Duration::from_millis(50)).unwrap();
        let c = s.compact().unwrap();
        assert!(c.pending, "{c:?}");
        assert!(c.after >= c.before, "not shrunk yet: {c:?}");
        reader.conn.execute_batch("COMMIT").unwrap();
        drop(reader);
        assert!(s.checkpoint().unwrap());
        assert!(std::fs::metadata(&path).unwrap().len() * 2 < c.before, "shrunk at the next checkpoint");
        assert_eq!(s.read(|o| o.meta_get("k0")).unwrap().map(|v| v.len()), Some(1_000));
        drop(s);
        for ext in ["", "-wal", "-shm"] {
            let _ = std::fs::remove_file(format!("{}{ext}", path.display()));
        }
    }

    /// The guard behind `an_update_reads_no_whole_table` (oneup-remote)
    /// sees through parameters and aliases, and leaves small tables and
    /// indexed lookups alone.
    #[cfg(feature = "statement-log")]
    #[test]
    fn tree_scans_are_found_with_parameters_and_aliases() {
        let mut s = Store::open_in_memory().unwrap();
        s.log_statements();
        let scans = |sql: &str| s.tree_scans(&[sql.to_string()]).unwrap();
        assert_eq!(scans("SELECT count(*) FROM remote_item WHERE size > ?1").len(), 1);
        assert_eq!(scans("SELECT 1 FROM remote_item r WHERE r.size = ?1 AND r.kind = ?2").len(), 1);
        assert_eq!(scans("SELECT * FROM synced_item AS s JOIN meta m ON m.key = s.name").len(), 1);
        assert!(scans("SELECT * FROM remote_item WHERE id = ?1").is_empty(), "indexed");
        assert!(scans("SELECT * FROM remote_item WHERE parent_id = ?1").is_empty(), "indexed");
        assert!(scans("SELECT * FROM enum_unit WHERE state = ?1").is_empty(), "small table");
        assert!(s.tree_scans(&["SELECT * FROM no_such_table".to_string()]).is_err(), "not skipped");
        assert!(scans("-- TRIGGER t").is_empty());
    }

    /// Two connections that each read and then write, at the same time (the
    /// agent and its hydration service, say): every write waits its turn.
    /// With deferred transactions, some failed at once with "database is
    /// locked" (checked: this test fails that way without `IMMEDIATE`).
    #[test]
    #[allow(clippy::unwrap_used)]
    fn concurrent_writers_wait_for_each_other_instead_of_failing() {
        let path = std::env::temp_dir().join(format!("oneup-store-busy-{}.db", std::process::id()));
        let _ = std::fs::remove_file(&path);
        drop(Store::open(&path, 1024).unwrap());
        let writer = |key: &'static str| {
            let path = path.clone();
            std::thread::spawn(move || {
                let mut s = Store::open(&path, 1024).unwrap();
                for _ in 0..300 {
                    s.write(|o| {
                        let n: u64 = o.meta_get(key)?.and_then(|v| v.parse().ok()).unwrap_or(0);
                        o.meta_set(key, &(n + 1).to_string())
                    })
                    .unwrap();
                }
            })
        };
        let (a, b) = (writer("a"), writer("b"));
        a.join().unwrap();
        b.join().unwrap();
        let s = Store::open(&path, 1024).unwrap();
        assert_eq!(s.read(|o| o.meta_get("a")).unwrap().as_deref(), Some("300"));
        assert_eq!(s.read(|o| o.meta_get("b")).unwrap().as_deref(), Some("300"));
        drop(s);
        for ext in ["", "-wal", "-shm"] {
            let _ = std::fs::remove_file(format!("{}{ext}", path.display()));
        }
    }

    /// Pruning or deleting a subtree costs what the subtree holds, not what
    /// the mirror holds. As joins, these walked every row of the big table
    /// once per call: newly excluded folders kept a 2.8M-item agent at
    /// "starting" for hours (checked: this test fails that way).
    #[test]
    #[allow(clippy::unwrap_used)]
    fn subtree_queries_never_scan() {
        let s = Store::open_in_memory().unwrap();
        for sql in [PRUNE_TOMBSTONES, synced::NOTE_SUBTREE_GONE] {
            let mut st = s.conn.prepare(&format!("EXPLAIN QUERY PLAN {sql}")).unwrap();
            let plan: Vec<String> = st.query_map([], |r| r.get(3)).unwrap().map(|r| r.unwrap()).collect();
            assert!(!plan.iter().any(|step| step.starts_with("SCAN")), "{sql}\n{plan:#?}");
        }
    }

    fn row(id: &str, parent: Option<&str>, name: &str, kind: Kind) -> RemoteRow {
        RemoteRow {
            id: id.into(),
            parent_id: parent.map(Into::into),
            name: name.into(),
            kind,
            size: 1,
            etag: Some(format!("e-{id}")),
            qxh: None,
            mtime_ms: Some(0),
        }
    }

    fn tree(s: &mut Store) {
        s.write(|o| {
            o.upsert_item(&row("r", None, "root", Kind::Root), false)?;
            o.upsert_item(&row("a", Some("r"), "a", Kind::Folder), false)?;
            o.upsert_item(&row("b", Some("a"), "b", Kind::Folder), false)?;
            o.upsert_item(&row("f1", Some("b"), "f1.txt", Kind::File), false)?;
            o.upsert_item(&row("f2", Some("a"), "f2.txt", Kind::File), false)
        })
        .unwrap();
    }

    #[test]
    fn upsert_is_idempotent_and_updates() {
        let mut s = Store::open_in_memory().unwrap();
        tree(&mut s);
        tree(&mut s);
        s.write(|o| o.upsert_item(&row("f2", Some("b"), "renamed.txt", Kind::File), false)).unwrap();
        s.read(|o| {
            assert_eq!(o.stats()?.files, 2);
            assert_eq!(o.remote_root()?.map(|r| r.id).as_deref(), Some("r"));
            let mut kids: Vec<String> = o.remote_children("b")?.into_iter().map(|(r, _)| r.name).collect();
            kids.sort();
            assert_eq!(kids, ["f1.txt", "renamed.txt"]);
            assert_eq!(o.path_of("f2")?.as_deref(), Some("/a/b/renamed.txt"));
            assert!(o.check_invariants()?.is_empty());
            Ok(())
        })
        .unwrap();
    }

    #[test]
    fn folder_totals_count_files_at_any_depth() {
        let mut s = Store::open_in_memory().unwrap();
        tree(&mut s);
        s.write(|o| o.upsert_item(&row("e", Some("r"), "empty", Kind::Folder), false)).unwrap();
        let mut t = s.read(|o| o.remote_folder_totals("r")).unwrap();
        t.sort();
        // a holds f2 and, inside b, f1 (1 byte each); "empty" holds nothing.
        assert_eq!(t, [("a".to_string(), 2), ("e".to_string(), 0)]);
        assert_eq!(s.read(|o| o.remote_folder_totals("b")).unwrap(), []);
    }

    #[test]
    fn delete_subtree_removes_rows_and_tombstones() {
        let mut s = Store::open_in_memory().unwrap();
        tree(&mut s);
        s.write(|o| {
            o.add_pruned("junk", "b")?;
            o.add_pruned("junk2", "junk")?;
            assert_eq!(o.delete_subtree("a")?, 4);
            Ok(())
        })
        .unwrap();
        s.read(|o| {
            let st = o.stats()?;
            assert_eq!((st.files, st.folders, st.pruned_folders), (0, 1, 0));
            assert!(o.check_invariants()?.is_empty());
            Ok(())
        })
        .unwrap();
    }

    #[test]
    fn prune_keeps_self_as_excluded_and_tombstones_folders() {
        let mut s = Store::open_in_memory().unwrap();
        tree(&mut s);
        s.write(|o| {
            assert_eq!(o.prune_subtree("a", false)?, 3);
            o.upsert_item(&row("a", Some("r"), "a", Kind::Folder), true)
        })
        .unwrap();
        s.read(|o| {
            assert_eq!(o.item_state("a")?, ItemState::Excluded);
            assert_eq!(o.item_state("b")?, ItemState::Pruned);
            assert_eq!(o.item_state("f1")?, ItemState::Absent);
            assert_eq!(o.remote_excluded_paths()?, ["/a"]);
            assert!(o.check_invariants()?.is_empty());
            Ok(())
        })
        .unwrap();
        s.write(|o| o.unprune_subtree("b").map(|_| ())).unwrap();
        s.read(|o| {
            assert_eq!(o.item_state("b")?, ItemState::Absent);
            Ok(())
        })
        .unwrap();
    }

    #[test]
    fn orphans_resolve_when_parent_arrives() {
        let mut s = Store::open_in_memory().unwrap();
        s.write(|o| {
            o.upsert_item(&row("r", None, "root", Kind::Root), false)?;
            o.put_orphan(&row("c", Some("p2"), "c", Kind::File), false)?;
            o.put_orphan(&row("p2", Some("p1"), "p2", Kind::Folder), true)?;
            assert_eq!(o.orphan_missing_parents()?, vec!["p1".to_string()]);
            assert!(o.take_resolvable_orphans()?.is_empty());
            o.upsert_item(&row("p1", Some("r"), "p1", Kind::Folder), false)?;
            let first = o.take_resolvable_orphans()?;
            assert_eq!(first.len(), 1);
            assert_eq!(first[0].0.id, "p2");
            assert!(first[0].1, "catch-up flag is kept");
            o.upsert_item(&first[0].0, false)?;
            assert_eq!(o.take_resolvable_orphans()?.len(), 1);
            Ok(())
        })
        .unwrap();
    }

    #[test]
    fn clear_remote_keeps_local_tree_and_settings() {
        let mut s = Store::open_in_memory().unwrap();
        tree(&mut s);
        s.write(|o| {
            o.add_pruned("junk", "b")?;
            o.set_cursor(&Cursor { delta_link: "dl".into(), taken_at_ms: 1, state: CursorState::Current })?;
            o.meta_set("rules", "v1:folders=x")?;
            o.meta_set("exclude_preset", "dev")?;
            let root = LocalRoot {
                path: "C:\\x".into(),
                volume_serial: 1,
                root_file_id: 1,
                rules: "r".into(),
                journal: None,
                started_at_ms: 0,
                finished_at_ms: None,
                sync_root_provider: None,
            };
            let row = LocalRow {
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
            o.local_begin(&root, &row)?;
            o.clear_remote()
        })
        .unwrap();
        s.read(|o| {
            let st = o.stats()?;
            assert_eq!((st.files, st.folders, st.pruned_folders), (0, 0, 0));
            assert!(o.cursor()?.is_none());
            assert_eq!(o.meta_get("rules")?, None);
            assert_eq!(o.meta_get("exclude_preset")?.as_deref(), Some("dev"));
            assert_eq!(o.local_stats()?.folders, 1);
            Ok(())
        })
        .unwrap();
    }

    #[test]
    fn invariant_violations_are_reported() {
        let mut s = Store::open_in_memory().unwrap();
        s.write(|o| {
            o.upsert_item(&row("r", None, "root", Kind::Root), false)?;
            o.upsert_item(&row("x", Some("missing"), "x", Kind::File), false)?;
            o.upsert_item(&row("e", Some("r"), "e", Kind::Folder), true)?;
            o.upsert_item(&row("y", Some("e"), "y", Kind::File), false)?;
            o.add_pruned("e", "r")
        })
        .unwrap();
        let problems = s.read(|o| o.check_invariants()).unwrap();
        assert_eq!(problems.len(), 2, "{problems:?}");
    }

    #[test]
    fn units_and_cursor_roundtrip() {
        let mut s = Store::open_in_memory().unwrap();
        s.write(|o| {
            let u = Unit {
                folder_id: "a".into(),
                path: "/a".into(),
                kind: UnitKind::Delta,
                depth: 2,
                next_link: None,
                items: 0,
            };
            o.put_unit(&u)?;
            o.put_unit(&u)?;
            o.set_unit_progress("a", "next", 10)?;
            assert_eq!(o.pending_units()?[0].next_link.as_deref(), Some("next"));
            o.finish_unit("a", 5)?;
            assert!(o.pending_units()?.is_empty());
            o.set_cursor(&Cursor { delta_link: "dl".into(), taken_at_ms: 7, state: CursorState::Pending })?;
            o.set_cursor(&Cursor { delta_link: "dl2".into(), taken_at_ms: 8, state: CursorState::Current })?;
            assert_eq!(o.cursor()?.unwrap().state, CursorState::Current);
            assert_eq!(o.stats()?.units_done, 1);
            Ok(())
        })
        .unwrap();
    }

    #[test]
    fn migrates_v1_database() {
        let dir = std::env::temp_dir().join(format!("oneup-store-migrate-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("state.db");
        {
            let c = Connection::open(&path).unwrap();
            c.execute_batch(&format!("{SCHEMA_V1} PRAGMA user_version = 1;")).unwrap();
            c.execute("INSERT INTO remote_orphan (id, parent_id, name, kind) VALUES ('o', 'p', 'o', 1)", []).unwrap();
        }
        let mut s = Store::open(&path, 1024).unwrap();
        s.write(|o| {
            o.upsert_item(&row("p", None, "root", Kind::Root), false)?;
            let got = o.take_resolvable_orphans()?;
            assert_eq!(got.len(), 1);
            assert!(got[0].1, "pre-v2 orphans default to catch-up (conservative)");
            Ok(())
        })
        .unwrap();
        let v: i64 = s.conn.query_row("PRAGMA user_version", [], |r| r.get(0)).unwrap();
        assert_eq!(v, SCHEMA_VERSION);
        // v3's Local tree exists and starts empty.
        assert_eq!(s.read(|o| o.local_root()).unwrap(), None);
        assert_eq!(s.read(|o| o.local_stats()).unwrap(), LocalStats::default());
        drop(s);
        std::fs::remove_dir_all(&dir).unwrap();
    }

    #[test]
    fn reopening_keeps_data() {
        let dir = std::env::temp_dir().join(format!("oneup-store-test-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("state.db");
        {
            let mut s = Store::open(&path, 1024).unwrap();
            tree(&mut s);
        }
        let s = Store::open(&path, 1024).unwrap();
        assert_eq!(s.read(|o| o.stats()).unwrap().files, 2);
        drop(s);
        std::fs::remove_dir_all(&dir).unwrap();
    }
}
