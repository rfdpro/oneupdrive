//! The Synced tree: what the local folder and OneDrive last agreed on, plus
//! the deletion evidence and content hashes the reconciler reads (schema v4,
//! `docs/architecture/sync-engine.md`).
//!
//! Unlike the Local and Remote trees, none of this is derived data. A mirror
//! rebuild ([`Ops::clear_remote`]) and a rescan ([`Ops::local_begin`]) leave
//! it alone, and they never write deletion evidence: an item that is simply
//! not there after a rebuild reads as [`Seen::Missing`], which the
//! reconciler holds instead of propagating (`data-safety.md` invariant 3).
//!
//! Tables:
//! * `synced_item`: one row per agreed item, keyed by our own `sid` (root =
//!   [`oneup_sync::ROOT`]), indexed by parent and by both linked IDs.
//! * `local_gone` / `remote_gone`: IDs removed with evidence (a USN delete
//!   record; a delta `deleted` facet on the item or an ancestor). Recorded
//!   only for IDs a Synced row links to, because those are the only ones
//!   the reconciler ever asks about. That keeps them as small as the Synced
//!   tree, and they're dropped when their row goes or is relinked.
//! * `local_hash`: QuickXorHashes computed for exactly one (file ID, size,
//!   mtime); a hash whose stamp no longer matches is ignored, not trusted.
//!
//! Invariants (checked by [`Ops::check_invariants`]):
//! 1. if any row exists, the root row exists and has no parent;
//! 2. every other row's parent exists and is a folder;
//! 3. no two rows link the same local or cloud item (unique indexes).

use oneup_sync::{Base, Hash, LocalId, LocalNode, NodeKind, ROOT, RemoteId, RemoteNode, Seen, Sid, Trees};
use rusqlite::{OptionalExtension, Row, params};

use crate::local::{blob, id_from_blob};
use crate::{Error, Ops, Result};

/// v3 -> v4: the Synced tree and its evidence (see the module docs).
///
/// The partial unique indexes enforce invariant 3 while allowing any number
/// of detached rows (`NULL` link), which the reconciler creates on purpose
/// when it renames a conflicting local copy away.
pub(crate) const MIGRATE_V4: &str = "
CREATE TABLE synced_item (
    sid         INTEGER PRIMARY KEY,
    parent_sid  INTEGER,
    name        TEXT NOT NULL,
    kind        INTEGER NOT NULL,
    size        INTEGER NOT NULL DEFAULT 0,
    qxh         BLOB,
    local_id    BLOB,
    local_mtime INTEGER NOT NULL DEFAULT 0,
    remote_id   TEXT,
    remote_etag TEXT NOT NULL DEFAULT ''
);
CREATE INDEX synced_item_parent ON synced_item(parent_sid);
CREATE UNIQUE INDEX synced_item_local ON synced_item(local_id) WHERE local_id IS NOT NULL;
CREATE UNIQUE INDEX synced_item_remote ON synced_item(remote_id) WHERE remote_id IS NOT NULL;

CREATE TABLE local_gone (file_id BLOB PRIMARY KEY) WITHOUT ROWID;
CREATE TABLE remote_gone (id TEXT PRIMARY KEY) WITHOUT ROWID;

CREATE TABLE local_hash (
    file_id BLOB PRIMARY KEY,
    size    INTEGER NOT NULL,
    mtime   INTEGER NOT NULL,
    qxh     BLOB NOT NULL
) WITHOUT ROWID;
";

/// v11 -> v12: an item written to the Local tree is there, so it can't be
/// deleted: its deletion evidence goes (`data-safety.md` invariant 3). A file
/// sent to the Recycle Bin counts as deleted (`oneup_app::recyclebin`);
/// restored before that reached OneDrive, it came back with its evidence,
/// which the consistency check rightly refused, stopping the agent
/// (reproduced on a test account). Stale evidence would also
/// turn a later disappearance without evidence into a deletion. Triggers,
/// as for the dirty marks, so every writer of `local_item` is covered. The
/// executor notes evidence *before* removing an item and doesn't write its
/// row in between, so its own sequence is unaffected.
pub(crate) const MIGRATE_V12: &str = "
DELETE FROM local_gone WHERE file_id IN (SELECT file_id FROM local_item);
CREATE TRIGGER local_back_ins AFTER INSERT ON local_item
BEGIN
    DELETE FROM local_gone WHERE file_id = NEW.file_id;
END;
CREATE TRIGGER local_back_upd AFTER UPDATE ON local_item
BEGIN
    DELETE FROM local_gone WHERE file_id = NEW.file_id;
END;
";

/// v14 -> v15: drops Synced records no longer reachable from the root
/// through folders, with their evidence: what [`Ops::synced_remove`] left
/// behind before it took a folder's whole subtree. The planner walks from
/// the root, so it never saw them; only the consistency check did, and
/// refused every round.
/// (The root is sid 0, [`ROOT`].)
pub(crate) const MIGRATE_V15: &str = "
CREATE TEMP TABLE reach(sid INTEGER PRIMARY KEY);
WITH RECURSIVE r(sid) AS (
    SELECT sid FROM synced_item WHERE sid = 0 AND parent_sid IS NULL
    UNION ALL
    SELECT c.sid FROM synced_item c JOIN r ON c.parent_sid = r.sid
    JOIN synced_item p ON p.sid = r.sid AND p.kind = 1)
INSERT INTO reach SELECT sid FROM r;
DELETE FROM local_gone WHERE EXISTS (SELECT 1 FROM reach) AND file_id IN
    (SELECT local_id FROM synced_item WHERE sid NOT IN reach AND local_id IS NOT NULL);
DELETE FROM remote_gone WHERE EXISTS (SELECT 1 FROM reach) AND id IN
    (SELECT remote_id FROM synced_item WHERE sid NOT IN reach AND remote_id IS NOT NULL);
DELETE FROM synced_item WHERE EXISTS (SELECT 1 FROM reach) AND sid NOT IN reach;
DROP TABLE reach;
";

/// v15 -> v16: [`MIGRATE_V12`] for the cloud side. An item written to the
/// Remote tree is there, so its deletion evidence goes. A folder's
/// deletion is evidence for everything stored below it, but delta may
/// report the folder deleted before a child it moved out, and an item
/// restored from the OneDrive recycle bin keeps its id. Kept, that evidence
/// turned the item's later disappearance without evidence (a rule prunes
/// it) into a deletion to bring to this PC. Evidence is noted for items
/// already gone from the Remote tree, or by [`Ops::delete_subtree`] as it
/// removes their rows, so no writer's own sequence is undone.
pub(crate) const MIGRATE_V16: &str = "
DELETE FROM remote_gone WHERE id IN (SELECT id FROM remote_item);
CREATE TRIGGER remote_back_ins AFTER INSERT ON remote_item
BEGIN
    DELETE FROM remote_gone WHERE id = NEW.id;
END;
CREATE TRIGGER remote_back_upd AFTER UPDATE ON remote_item
BEGIN
    DELETE FROM remote_gone WHERE id = NEW.id;
END;
";

/// Summary counts of the Synced tree.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct SyncedStats {
    /// Rows including the root.
    pub items: i64,
    pub local_tombstones: i64,
    pub remote_tombstones: i64,
    pub hashes: i64,
}

const BASE_COLS: &str = "sid, parent_sid, name, kind, size, qxh, local_id, local_mtime, remote_id, remote_etag";

/// The stored kind codes, shared with `local_item` and `remote_item`
/// (0 = file, 1 = folder, 2 = root).
pub(crate) fn node_kind(code: i64) -> Result<NodeKind> {
    match code {
        0 => Ok(NodeKind::File),
        1 => Ok(NodeKind::Folder),
        _ => Err(Error::Corrupt(format!("synced kind {code}"))),
    }
}

pub(crate) fn kind_code(k: NodeKind) -> i64 {
    match k {
        NodeKind::File => 0,
        NodeKind::Folder => 1,
    }
}

/// A BLOB that should hold a QuickXorHash. Anything but 20 bytes (never
/// written by us) reads as "no hash", which only ever makes the reconciler
/// ask for one; it never makes two different contents look equal.
pub(crate) fn hash_of(b: Option<Vec<u8>>) -> Option<Hash> {
    b.and_then(|v| <[u8; 20]>::try_from(v.as_slice()).ok())
}

/// Sids are `u64` in the reconciler and `INTEGER` (i64) in SQLite. They
/// are allocated by SQLite from 1 upwards, so the cast never wraps.
pub(crate) fn sid_out(v: i64) -> Sid {
    v as Sid
}

pub(crate) fn sid_in(s: Sid) -> i64 {
    s as i64
}

fn base_from_row(r: &Row<'_>) -> rusqlite::Result<(i64, Base)> {
    let local: Option<Vec<u8>> = r.get("local_id")?;
    Ok((
        r.get("kind")?,
        Base {
            sid: sid_out(r.get("sid")?),
            parent: r.get::<_, Option<i64>>("parent_sid")?.map(sid_out),
            name: r.get("name")?,
            kind: NodeKind::File, // replaced by `fix_base` after validation
            size: r.get("size")?,
            hash: hash_of(r.get("qxh")?),
            local: local.as_deref().map(|b| LocalId(id_from_blob(b))),
            local_mtime: r.get("local_mtime")?,
            remote: r.get::<_, Option<String>>("remote_id")?.map(RemoteId),
            remote_etag: r.get("remote_etag")?,
        },
    ))
}

fn fix_base((kind, mut b): (i64, Base)) -> Result<Base> {
    b.kind = node_kind(kind)?;
    Ok(b)
}

/// Local entries the reconciler may see: files and folders whose contents
/// were listed, with names that can address them. Links, excluded or
/// unreadable folders and lossy names are filtered out, and (per the
/// [`Trees`] contract) read as Missing when asked for by ID.
const LOCAL_NODE: &str = "SELECT i.file_id, i.parent_id, i.name, i.kind, i.size, i.mtime, h.qxh
     FROM local_item i
     LEFT JOIN local_hash h ON h.file_id = i.file_id AND h.size = i.size AND h.mtime = i.mtime";
const LOCAL_SYNCABLE: &str = "i.skip = 0 AND i.name_lossy = 0";
/// A local file the OneDrive app left standing in for a package in the
/// matching cloud folder: `<notebook>.url` for a OneNote notebook,
/// `<vault>.lnk` for the Personal Vault. Packages never sync, so their
/// stand-ins don't either (they'd upload as stray files after moving over).
/// Names compare as the planner compares them (`oneup_fold`, see `Store::init`).
const STAND_IN: &str = "(i.kind = 0 AND (i.name LIKE '%.url' OR i.name LIKE '%.lnk') AND EXISTS (
       SELECT 1 FROM synced_item s JOIN remote_item r ON r.parent_id = s.remote_id
       WHERE s.local_id = i.parent_id AND r.kind = 3
         AND oneup_fold(r.name) = oneup_fold(substr(i.name, 1, length(i.name) - 4))))";

fn local_node(r: &Row<'_>) -> rusqlite::Result<LocalNode> {
    let id: Vec<u8> = r.get(0)?;
    let parent: Option<Vec<u8>> = r.get(1)?;
    let file = r.get::<_, i64>(3)? == 0;
    // Folder sizes and times carry no meaning for sync (a folder's mtime
    // changes whenever a child does), so they're zeroed rather than compared.
    Ok(LocalNode {
        id: LocalId(id_from_blob(&id)),
        parent: parent.as_deref().map(|p| LocalId(id_from_blob(p))),
        name: r.get(2)?,
        kind: if file { NodeKind::File } else { NodeKind::Folder },
        size: if file { r.get(4)? } else { 0 },
        mtime: if file { r.get(5)? } else { 0 },
        hash: if file { hash_of(r.get(6)?) } else { None },
    })
}

/// Cloud items the reconciler may see: files, folders and the root.
/// Excluded folders and packages (OneNote notebooks) are filtered out.
const REMOTE_NODE: &str = "SELECT id, parent_id, name, kind, size, etag, qxh FROM remote_item";
const REMOTE_SYNCABLE: &str = "is_excluded = 0";

/// The children lists the reconciler reads ([`Trees`]), each from a parent
/// index.
fn base_children_sql() -> String {
    format!("SELECT {BASE_COLS} FROM synced_item WHERE parent_sid = ?1")
}
fn local_children_sql() -> String {
    format!("{LOCAL_NODE} WHERE i.parent_id = ?1 AND i.kind IN (0, 1) AND {LOCAL_SYNCABLE} AND NOT {STAND_IN}")
}
fn remote_children_sql() -> String {
    format!("{REMOTE_NODE} WHERE parent_id = ?1 AND kind IN (0, 1) AND {REMOTE_SYNCABLE}")
}
const SYNCED_HAS_CHILDREN: &str = "SELECT 1 FROM synced_item WHERE parent_sid = ?1 LIMIT 1";

/// Evidence for every Synced item in the subtree in `t_sub`.
pub(crate) const NOTE_SUBTREE_GONE: &str = "
INSERT OR IGNORE INTO remote_gone(id)
SELECT s.remote_id FROM synced_item s WHERE s.remote_id IN (SELECT id FROM t_sub)";

fn remote_node(r: &Row<'_>) -> rusqlite::Result<RemoteNode> {
    let file = r.get::<_, i64>(3)? == 0;
    Ok(RemoteNode {
        id: RemoteId(r.get(0)?),
        parent: r.get::<_, Option<String>>(1)?.map(RemoteId),
        name: r.get(2)?,
        kind: if file { NodeKind::File } else { NodeKind::Folder },
        // OneDrive reports a folder's size as the total of its contents.
        size: if file { r.get(4)? } else { 0 },
        hash: if file { hash_of(r.get(6)?) } else { None },
        etag: r.get::<_, Option<String>>(5)?.unwrap_or_default(),
    })
}

/// [`Ops::local_children_links`] (its plan is checked by
/// `children_queries_use_the_parent_indexes`, as the next one's).
const LOCAL_CHILDREN_LINKS: &str = "SELECT l.file_id, s.remote_id FROM local_item l
     JOIN synced_item s ON s.local_id = l.file_id WHERE l.parent_id = ?1";
/// [`Ops::remote_children_linked`].
const REMOTE_CHILDREN_LINKED: &str =
    "SELECT r.id FROM remote_item r JOIN synced_item s ON s.remote_id = r.id WHERE r.parent_id = ?1";

impl Ops<'_> {
    pub fn synced_get(&self, sid: Sid) -> Result<Option<Base>> {
        let row = self
            .c
            .prepare_cached(&format!("SELECT {BASE_COLS} FROM synced_item WHERE sid = ?1"))?
            .query_row([sid_in(sid)], base_from_row)
            .optional()?;
        row.map(fix_base).transpose()
    }

    /// Rows whose agreed parent is `sid` (unordered).
    pub fn synced_children(&self, sid: Sid) -> Result<Vec<Base>> {
        let mut st = self.c.prepare_cached(&base_children_sql())?;
        let rows = st.query_map([sid_in(sid)], base_from_row)?;
        rows.map(|r| fix_base(r?)).collect()
    }

    /// Whether any row's agreed parent is `sid`: one probe of the parent index.
    pub fn synced_has_children(&self, sid: Sid) -> Result<bool> {
        Ok(self.c.prepare_cached(SYNCED_HAS_CHILDREN)?.exists([sid_in(sid)])?)
    }

    /// Up to `limit` Synced rows after `after` (by sid) on both sides whose
    /// local item isn't a Cloud Files placeholder (its recorded reparse tag
    /// isn't `IO_REPARSE_TAG_CLOUD` or a variant): what turning Files
    /// On-Demand on has left to convert. Pages by the primary key, joining
    /// on `local_item`'s, so finding few left costs one pass of the table.
    pub fn synced_not_placeholders(&self, after: Sid, limit: usize) -> Result<Vec<Base>> {
        let cols = BASE_COLS.split(", ").map(|c| format!("s.{c} AS {c}")).collect::<Vec<_>>().join(", ");
        let mut st = self.c.prepare_cached(&format!(
            "SELECT {cols} FROM synced_item s JOIN local_item l ON l.file_id = s.local_id
             WHERE s.sid > ?1 AND s.remote_id IS NOT NULL
               AND (l.reparse_tag & 4294905855) != 2415919130
             ORDER BY s.sid LIMIT ?2"
        ))?;
        let rows = st.query_map(rusqlite::params![sid_in(after), limit as i64], base_from_row)?;
        rows.map(|r| fix_base(r?)).collect()
    }

    pub fn synced_by_local(&self, id: u128) -> Result<Option<Base>> {
        let row = self
            .c
            .prepare_cached(&format!("SELECT {BASE_COLS} FROM synced_item WHERE local_id = ?1"))?
            .query_row([blob(id)], base_from_row)
            .optional()?;
        row.map(fix_base).transpose()
    }

    /// The children of local folder `parent` that have a Synced row, each
    /// with the cloud item it links (if any): one query, by the parent and
    /// link indexes.
    pub fn local_children_links(&self, parent: u128) -> Result<Vec<(u128, Option<String>)>> {
        let mut st = self.c.prepare_cached(LOCAL_CHILDREN_LINKS)?;
        let rows = st.query_map([blob(parent)], |r| Ok((id_from_blob(&r.get::<_, Vec<u8>>(0)?), r.get(1)?)))?;
        rows.collect::<rusqlite::Result<_>>().map_err(Into::into)
    }

    /// The children of cloud folder `parent` that have a Synced row: one
    /// query, by the parent and link indexes.
    pub fn remote_children_linked(&self, parent: &str) -> Result<Vec<String>> {
        let mut st = self.c.prepare_cached(REMOTE_CHILDREN_LINKED)?;
        let rows = st.query_map([parent], |r| r.get(0))?;
        rows.collect::<rusqlite::Result<_>>().map_err(Into::into)
    }

    pub fn synced_by_remote(&self, id: &str) -> Result<Option<Base>> {
        let row = self
            .c
            .prepare_cached(&format!("SELECT {BASE_COLS} FROM synced_item WHERE remote_id = ?1"))?
            .query_row([id], base_from_row)
            .optional()?;
        row.map(fix_base).transpose()
    }

    /// Inserts or updates the row `b.sid`. Linking a local or cloud item
    /// that another row already links fails (invariant 3) instead of
    /// silently replacing that row, which is why this is an upsert on `sid`
    /// alone and not `INSERT OR REPLACE`.
    pub fn synced_put(&self, b: &Base) -> Result<()> {
        if let Some(old) = self.synced_get(b.sid)? {
            self.drop_evidence(&old, Some(b))?;
        }
        self.c
            .prepare_cached(
                "INSERT INTO synced_item (sid, parent_sid, name, kind, size, qxh, local_id, local_mtime,
                                          remote_id, remote_etag)
                 VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10)
                 ON CONFLICT(sid) DO UPDATE SET
                    parent_sid = excluded.parent_sid, name = excluded.name, kind = excluded.kind,
                    size = excluded.size, qxh = excluded.qxh, local_id = excluded.local_id,
                    local_mtime = excluded.local_mtime, remote_id = excluded.remote_id,
                    remote_etag = excluded.remote_etag",
            )?
            .execute(params![
                sid_in(b.sid),
                b.parent.map(sid_in),
                b.name,
                kind_code(b.kind),
                b.size,
                b.hash.map(|h| h.to_vec()),
                b.local.map(|l| blob(l.0)),
                b.local_mtime,
                b.remote.as_ref().map(|r| r.0.as_str()),
                b.remote_etag,
            ])?;
        Ok(())
    }

    /// Inserts `b` as a new row (its `sid` is ignored) and returns the sid
    /// SQLite allocated: one more than the largest ever used, so sids are
    /// never reused and a stale plan can't address a newer row by accident.
    pub fn synced_insert(&self, b: &Base) -> Result<Sid> {
        let next: i64 = self.c.query_row(
            "SELECT max(coalesce((SELECT max(sid) FROM synced_item), 0),
                        coalesce((SELECT CAST(value AS INTEGER) FROM meta WHERE key = 'synced_last_sid'), 0)) + 1",
            [],
            |r| r.get(0),
        )?;
        let sid = sid_out(next);
        self.synced_put(&Base { sid, ..b.clone() })?;
        self.meta_set("synced_last_sid", &next.to_string())?;
        Ok(sid)
    }

    /// Removes a row, the rows below it, and the evidence recorded for
    /// their items. Returns whether it existed.
    ///
    /// The planner forgets a folder's record once it sees no records below
    /// it, but it sees through the user's rules, which hide the records of
    /// excluded items gone from both sides. Those used to stay behind,
    /// pointing at a parent that no longer existed, and the consistency
    /// check then refused every round (a `node_modules` record, seen in
    /// use). A record only says what was last in
    /// sync; dropping one never deletes anything (deletions need evidence,
    /// `data-safety.md` invariant 3).
    pub fn synced_remove(&self, sid: Sid) -> Result<bool> {
        if self.synced_get(sid)?.is_none() {
            return Ok(false);
        }
        const SUB: &str = "WITH RECURSIVE sub(sid) AS (
                               SELECT ?1 UNION ALL
                               SELECT c.sid FROM synced_item c JOIN sub ON c.parent_sid = sub.sid)";
        for sql in [
            "DELETE FROM local_gone WHERE file_id IN
                 (SELECT local_id FROM synced_item WHERE sid IN sub AND local_id IS NOT NULL)",
            "DELETE FROM remote_gone WHERE id IN
                 (SELECT remote_id FROM synced_item WHERE sid IN sub AND remote_id IS NOT NULL)",
            "DELETE FROM synced_item WHERE sid IN sub",
        ] {
            self.c.prepare_cached(&format!("{SUB} {sql}"))?.execute([sid_in(sid)])?;
        }
        Ok(true)
    }

    /// Forgets deletion evidence for the items `old` links and `new` no
    /// longer does: nothing else can ask about them (invariant 3).
    fn drop_evidence(&self, old: &Base, new: Option<&Base>) -> Result<()> {
        if let Some(l) = old.local.filter(|l| new.is_none_or(|n| n.local != Some(*l))) {
            self.c.prepare_cached("DELETE FROM local_gone WHERE file_id = ?1")?.execute([blob(l.0)])?;
        }
        if let Some(r) = old.remote.as_ref().filter(|r| new.is_none_or(|n| n.remote.as_ref() != Some(*r))) {
            self.c.prepare_cached("DELETE FROM remote_gone WHERE id = ?1")?.execute([r.0.as_str()])?;
        }
        Ok(())
    }

    /// Records that local item `id` was deleted, with evidence (a USN
    /// delete record). Only Synced items need it; returns whether `id` is
    /// one. Never call this for "not found by a rescan": that isn't evidence.
    pub fn note_local_deleted(&self, id: u128) -> Result<bool> {
        Ok(self
            .c
            .prepare_cached(
                "INSERT OR IGNORE INTO local_gone(file_id)
                 SELECT local_id FROM synced_item WHERE local_id = ?1",
            )?
            .execute([blob(id)])?
            > 0)
    }

    /// Withdraws local deletion evidence for `id`. Only for the executor,
    /// which records evidence *before* an effect that removes an item from
    /// the tree (so a crash in between can't leave it "missing without
    /// evidence") and withdraws it when the effect didn't happen and the
    /// item is still there.
    pub fn unnote_local_deleted(&self, id: u128) -> Result<()> {
        self.c.prepare_cached("DELETE FROM local_gone WHERE file_id = ?1")?.execute([blob(id)])?;
        Ok(())
    }

    /// Withdraws cloud deletion evidence for `id`. Only for the executor's
    /// settling of a vault move cut short (`oneup_exec::Executor::settle_moves`):
    /// the item was moved under a new ID that can't be found yet, so its
    /// old ID's removal is no evidence of a deletion.
    pub fn unnote_remote_deleted(&self, id: &str) -> Result<()> {
        self.c.prepare_cached("DELETE FROM remote_gone WHERE id = ?1")?.execute([id])?;
        Ok(())
    }

    /// Records that cloud item `id` was deleted, with evidence (a delta
    /// `deleted` facet). See [`Ops::note_local_deleted`].
    pub fn note_remote_deleted(&self, id: &str) -> Result<bool> {
        Ok(self
            .c
            .prepare_cached(
                "INSERT OR IGNORE INTO remote_gone(id)
                 SELECT remote_id FROM synced_item WHERE remote_id = ?1",
            )?
            .execute([id])?
            > 0)
    }

    /// Records evidence for every Synced item in the mirror's `t_sub`
    /// (a subtree a delta `deleted` facet removed). Called by
    /// [`Ops::delete_subtree`] before it drops the rows, so a deleted folder
    /// is evidence for everything inside it.
    pub(crate) fn note_remote_subtree_deleted(&self) -> Result<()> {
        self.c.execute(NOTE_SUBTREE_GONE, [])?;
        Ok(())
    }

    /// Stores the content hash computed for local file `id` while it had
    /// exactly this `size` and `mtime`.
    pub fn local_hash_put(&self, id: u128, size: i64, mtime: i64, hash: &Hash) -> Result<()> {
        self.c
            .prepare_cached(
                "INSERT INTO local_hash(file_id, size, mtime, qxh) VALUES (?1, ?2, ?3, ?4)
                 ON CONFLICT(file_id) DO UPDATE SET size = excluded.size, mtime = excluded.mtime, qxh = excluded.qxh",
            )?
            .execute(params![blob(id), size, mtime, hash.to_vec()])?;
        Ok(())
    }

    /// The hash recorded for local file `id` if it was computed for exactly
    /// this `size` and `mtime`.
    pub fn local_hash_get(&self, id: u128, size: i64, mtime: i64) -> Result<Option<Hash>> {
        let v: Option<Vec<u8>> = self
            .c
            .prepare_cached("SELECT qxh FROM local_hash WHERE file_id = ?1 AND size = ?2 AND mtime = ?3")?
            .query_row(params![blob(id), size, mtime], |r| r.get(0))
            .optional()?;
        Ok(v.and_then(|b| b.try_into().ok()))
    }

    /// The Synced rows whose local item is gone (deleted with evidence, or
    /// detached), deepest first: what a one-way (backup) pair kept in the
    /// cloud after this PC deleted it.
    pub fn synced_gone_locally(&self) -> Result<Vec<Sid>> {
        let gone: std::collections::HashSet<i64> = self
            .c
            .prepare_cached(
                "SELECT s.sid FROM synced_item s
                 WHERE s.local_id IS NULL OR s.local_id IN (SELECT file_id FROM local_gone)",
            )?
            .query_map([], |r| r.get::<_, i64>(0))?
            .collect::<std::result::Result<_, _>>()?;
        if gone.is_empty() {
            return Ok(Vec::new());
        }
        let mut out: Vec<Sid> =
            self.synced_all()?.into_iter().map(|b| b.sid).filter(|s| gone.contains(&sid_in(*s))).collect();
        out.reverse();
        Ok(out)
    }

    /// Every Synced row, parents before children (breadth-first from the
    /// root): for one-off walks such as turning Files On-Demand on.
    pub fn synced_all(&self) -> Result<Vec<Base>> {
        let mut out = Vec::new();
        let mut i = 0;
        if let Some(root) = self.synced_get(ROOT)? {
            out.push(root);
        }
        while i < out.len() {
            let kids = self.synced_children(out[i].sid)?;
            out.extend(kids);
            i += 1;
        }
        Ok(out)
    }

    pub fn synced_stats(&self) -> Result<SyncedStats> {
        let one = |sql: &str| -> Result<i64> { Ok(self.c.query_row(sql, [], |r| r.get(0))?) };
        Ok(SyncedStats {
            items: one("SELECT count(*) FROM synced_item")?,
            local_tombstones: one("SELECT count(*) FROM local_gone")?,
            remote_tombstones: one("SELECT count(*) FROM remote_gone")?,
            hashes: one("SELECT count(*) FROM local_hash")?,
        })
    }

    /// Synced-tree invariant violations, each prefixed with "synced:".
    pub(crate) fn synced_problems(&self) -> Result<Vec<String>> {
        let one = |sql: &str| -> Result<i64> { Ok(self.c.query_row(sql, [], |r| r.get(0))?) };
        let mut problems = Vec::new();
        let rows = one("SELECT count(*) FROM synced_item")?;
        let root_ok = one(&format!("SELECT count(*) FROM synced_item WHERE sid = {ROOT} AND parent_sid IS NULL"))?;
        if rows > 0 && root_ok != 1 {
            problems.push("synced: rows exist but the root row is missing or has a parent".into());
        }
        let dangling = one(&format!(
            "SELECT count(*) FROM synced_item c LEFT JOIN synced_item p ON p.sid = c.parent_sid
             WHERE c.sid != {ROOT} AND (p.sid IS NULL OR p.kind != 1)"
        ))?;
        if dangling > 0 {
            problems.push(format!("synced: {dangling} rows whose parent is missing or not a folder"));
        }
        // Evidence for an item that's still there would turn a later
        // disappearance without evidence into a deletion.
        let stale = one("SELECT count(*) FROM local_gone g JOIN local_item i ON i.file_id = g.file_id")?;
        if stale > 0 {
            problems.push(format!("synced: {stale} local items both present and noted deleted"));
        }
        let stale = one("SELECT count(*) FROM remote_gone g JOIN remote_item r ON r.id = g.id")?;
        if stale > 0 {
            problems.push(format!("synced: {stale} cloud items both present and noted deleted"));
        }
        Ok(problems)
    }
}

/// The reconciler's view of the store. Reads only; every method is one
/// indexed query (children lists: one per folder), so a round costs what
/// the folders it visits contain, not the size of the trees.
impl Trees for Ops<'_> {
    type Error = Error;

    fn base(&self, sid: Sid) -> Result<Option<Base>> {
        self.synced_get(sid)
    }

    fn base_children(&self, sid: Sid) -> Result<Vec<Base>> {
        self.synced_children(sid)
    }

    /// Through the index on (folder, name): only the matches are read.
    /// SQLite's `NOCASE` folds ASCII only, so a name with other letters,
    /// not found that way, is looked for as the planner folds names, among
    /// the folder's children in SQLite (still only matches come back).
    fn base_named(&self, parent: Sid, name: &str) -> Result<Vec<Base>> {
        let mut st = self.c.prepare_cached(&format!(
            "SELECT {BASE_COLS} FROM synced_item INDEXED BY synced_item_name
             WHERE parent_sid = ?1 AND name = ?2 COLLATE NOCASE"
        ))?;
        let rows = st.query_map(rusqlite::params![sid_in(parent), name], base_from_row)?;
        let mut out = rows.map(|r| fix_base(r?)).collect::<Result<Vec<_>>>()?;
        if out.is_empty() && !name.is_ascii() {
            let mut st = self.c.prepare_cached(&format!(
                "SELECT {BASE_COLS} FROM synced_item
                 WHERE parent_sid = ?1 AND length(name) = ?2 AND oneup_fold(name) = ?3"
            ))?;
            let params = rusqlite::params![sid_in(parent), crate::char_count(name), oneup_sync::fold(name)];
            out = st.query_map(params, base_from_row)?.map(|r| fix_base(r?)).collect::<Result<Vec<_>>>()?;
        }
        out.retain(|b| oneup_sync::same_name(&b.name, name));
        Ok(out)
    }

    fn base_by_local(&self, id: LocalId) -> Result<Option<Base>> {
        self.synced_by_local(id.0)
    }

    fn base_by_remote(&self, id: &RemoteId) -> Result<Option<Base>> {
        self.synced_by_remote(&id.0)
    }

    fn local(&self, id: LocalId) -> Result<Seen<LocalNode>> {
        let node = self
            .c
            .prepare_cached(&format!("{LOCAL_NODE} WHERE i.file_id = ?1 AND i.kind IN (0, 1, 2) AND {LOCAL_SYNCABLE}"))?
            .query_row([blob(id.0)], local_node)
            .optional()?;
        if let Some(n) = node {
            return Ok(Seen::Present(n));
        }
        // Stored but filtered out (a link, an excluded folder...): there,
        // but not syncable, so Missing. Only an absent item can be Deleted.
        let stored = self.c.prepare_cached("SELECT 1 FROM local_item WHERE file_id = ?1")?.exists([blob(id.0)])?;
        let gone =
            !stored && self.c.prepare_cached("SELECT 1 FROM local_gone WHERE file_id = ?1")?.exists([blob(id.0)])?;
        Ok(if gone { Seen::Deleted } else { Seen::Missing })
    }

    fn remote(&self, id: &RemoteId) -> Result<Seen<RemoteNode>> {
        let node = self
            .c
            .prepare_cached(&format!("{REMOTE_NODE} WHERE id = ?1 AND kind IN (0, 1, 2) AND {REMOTE_SYNCABLE}"))?
            .query_row([id.0.as_str()], remote_node)
            .optional()?;
        if let Some(n) = node {
            return Ok(Seen::Present(n));
        }
        let stored = self.c.prepare_cached("SELECT 1 FROM remote_item WHERE id = ?1")?.exists([id.0.as_str()])?;
        let gone =
            !stored && self.c.prepare_cached("SELECT 1 FROM remote_gone WHERE id = ?1")?.exists([id.0.as_str()])?;
        Ok(if gone { Seen::Deleted } else { Seen::Missing })
    }

    fn local_children(&self, id: LocalId) -> Result<Vec<LocalNode>> {
        let mut st = self.c.prepare_cached(&local_children_sql())?;
        let v = st.query_map([blob(id.0)], local_node)?.collect::<rusqlite::Result<_>>()?;
        Ok(v)
    }

    fn remote_children(&self, id: &RemoteId) -> Result<Vec<RemoteNode>> {
        let mut st = self.c.prepare_cached(&remote_children_sql())?;
        let v = st.query_map([id.0.as_str()], remote_node)?.collect::<rusqlite::Result<_>>()?;
        Ok(v)
    }

    // The same queries, stepped a row at a time: SQLite reads no further
    // than the first child `f` accepts.

    fn any_base_child(&self, sid: Sid, f: &mut dyn FnMut(&Base) -> Result<bool>) -> Result<bool> {
        let mut st = self.c.prepare_cached(&base_children_sql())?;
        let mut rows = st.query([sid_in(sid)])?;
        while let Some(r) = rows.next()? {
            if f(&fix_base(base_from_row(r)?)?)? {
                return Ok(true);
            }
        }
        Ok(false)
    }

    fn any_local_child(&self, id: LocalId, f: &mut dyn FnMut(&LocalNode) -> Result<bool>) -> Result<bool> {
        let mut st = self.c.prepare_cached(&local_children_sql())?;
        let mut rows = st.query([blob(id.0)])?;
        while let Some(r) = rows.next()? {
            if f(&local_node(r)?)? {
                return Ok(true);
            }
        }
        Ok(false)
    }

    fn any_remote_child(&self, id: &RemoteId, f: &mut dyn FnMut(&RemoteNode) -> Result<bool>) -> Result<bool> {
        let mut st = self.c.prepare_cached(&remote_children_sql())?;
        let mut rows = st.query([id.0.as_str()])?;
        while let Some(r) = rows.next()? {
            if f(&remote_node(r)?)? {
                return Ok(true);
            }
        }
        Ok(false)
    }

    fn has_base_children(&self, sid: Sid) -> Result<bool> {
        self.synced_has_children(sid)
    }

    // Through the name index: one lookup, whatever the folder holds.
    fn local_has_file(&self, id: LocalId, name: &str) -> Result<bool> {
        Ok(self.local_named(id.0, name)?.iter().any(|r| r.kind == crate::LocalKind::File))
    }

    fn remote_has_file(&self, id: &RemoteId, name: &str) -> Result<bool> {
        Ok(self.remote_named(&id.0, name)?.iter().any(|r| r.kind == crate::Kind::File))
    }
}

#[cfg(test)]
pub(crate) mod tests {
    use super::*;
    use crate::{Kind, LocalKind, LocalRow, RemoteRow, Skip, Store};

    pub(crate) fn base(sid: Sid, parent: Option<Sid>, name: &str, kind: NodeKind, local: u128, remote: &str) -> Base {
        Base {
            sid,
            parent,
            name: name.into(),
            kind,
            size: 0,
            hash: None,
            local: Some(LocalId(local)),
            local_mtime: 0,
            remote: Some(RemoteId(remote.into())),
            remote_etag: "e".into(),
        }
    }

    pub(crate) fn lrow(id: u128, parent: Option<u128>, name: &str, kind: LocalKind) -> LocalRow {
        LocalRow {
            file_id: id,
            parent_id: parent,
            name: name.into(),
            name_lossy: false,
            kind,
            size: 3,
            mtime: 9,
            attributes: 0,
            reparse_tag: 0,
            skip: Skip::None,
        }
    }

    pub(crate) fn rrow(id: &str, parent: Option<&str>, name: &str, kind: Kind) -> RemoteRow {
        RemoteRow {
            id: id.into(),
            parent_id: parent.map(Into::into),
            name: name.into(),
            kind,
            size: 3,
            etag: Some("e".into()),
            qxh: None,
            mtime_ms: None,
        }
    }

    /// Root(1 / "root") with a synced file f (2 / "f") and folder d (3 / "d").
    pub(crate) fn synced(s: &mut Store) {
        s.write(|o| {
            o.local_put(&lrow(1, None, "", LocalKind::Root))?;
            o.local_put(&lrow(2, Some(1), "f", LocalKind::File))?;
            o.local_put(&lrow(3, Some(1), "d", LocalKind::Folder))?;
            o.upsert_item(&rrow("root", None, "root", Kind::Root), false)?;
            o.upsert_item(&rrow("f", Some("root"), "f", Kind::File), false)?;
            o.upsert_item(&rrow("d", Some("root"), "d", Kind::Folder), false)?;
            o.synced_put(&base(ROOT, None, "", NodeKind::Folder, 1, "root"))?;
            assert_eq!(o.synced_insert(&base(0, Some(ROOT), "f", NodeKind::File, 2, "f"))?, 1);
            assert_eq!(o.synced_insert(&base(0, Some(ROOT), "d", NodeKind::Folder, 3, "d"))?, 2);
            Ok(())
        })
        .unwrap();
    }

    #[test]
    fn a_folders_links_come_in_one_query_per_side() {
        let mut s = Store::open_in_memory().unwrap();
        synced(&mut s);
        s.write(|o| {
            // Unlinked on both sides, and one linked inside `d`.
            o.local_put(&lrow(4, Some(1), "new", LocalKind::File))?;
            o.upsert_item(&rrow("n", Some("root"), "new", Kind::File), false)?;
            o.local_put(&lrow(5, Some(3), "in", LocalKind::File))?;
            o.upsert_item(&rrow("i", Some("d"), "in", Kind::File), false)?;
            o.synced_insert(&base(0, Some(2), "in", NodeKind::File, 5, "i")).map(|_| ())
        })
        .unwrap();
        let mut local = s.read(|o| o.local_children_links(1)).unwrap();
        local.sort();
        assert_eq!(local, [(2, Some("f".to_string())), (3, Some("d".to_string()))]);
        let mut remote = s.read(|o| o.remote_children_linked("root")).unwrap();
        remote.sort();
        assert_eq!(remote, ["d", "f"]);
        assert_eq!(s.read(|o| o.remote_children_linked("d")).unwrap(), ["i"]);
    }

    /// A file restored from the Recycle Bin before its deletion reached
    /// OneDrive: back in the Local tree, it's no longer deleted.
    #[test]
    fn an_item_back_in_the_tree_loses_its_deletion_evidence() {
        let mut s = Store::open_in_memory().unwrap();
        synced(&mut s);
        s.write(|o| {
            o.local_remove_subtree(2)?;
            assert!(o.note_local_deleted(2)?);
            assert_eq!(o.local(LocalId(2))?, Seen::Deleted);
            o.local_put(&lrow(2, Some(1), "f", LocalKind::File))?;
            assert!(matches!(o.local(LocalId(2))?, Seen::Present(_)));
            assert!(o.check_invariants()?.is_empty(), "{:?}", o.check_invariants()?);
            // Gone again without a delete record: missing, not deleted.
            o.local_remove_subtree(2)?;
            assert_eq!(o.local(LocalId(2))?, Seen::Missing);
            Ok(())
        })
        .unwrap();
    }

    /// A child delta reports moved out of a folder only after the folder's
    /// deletion (or an item restored from the OneDrive recycle bin): back in
    /// the Remote tree, it's no longer deleted, and leaving it again without
    /// evidence (a rule prunes it) reads as Missing.
    #[test]
    fn a_cloud_item_back_in_the_tree_loses_its_deletion_evidence() {
        let mut s = Store::open_in_memory().unwrap();
        synced(&mut s);
        s.write(|o| {
            o.upsert_item(&rrow("g", Some("d"), "g", Kind::File), false)?;
            o.local_put(&lrow(4, Some(3), "g", LocalKind::File))?;
            o.synced_insert(&base(0, Some(2), "g", NodeKind::File, 4, "g"))?;
            o.delete_subtree("d")?;
            assert_eq!(o.remote(&RemoteId("g".into()))?, Seen::Deleted);
            o.upsert_item(&rrow("g", Some("root"), "g", Kind::File), false)?;
            assert!(matches!(o.remote(&RemoteId("g".into()))?, Seen::Present(_)));
            assert!(o.check_invariants()?.is_empty(), "{:?}", o.check_invariants()?);
            o.prune_subtree("g", true)?;
            assert_eq!(o.remote(&RemoteId("g".into()))?, Seen::Missing);
            assert_eq!(o.remote(&RemoteId("d".into()))?, Seen::Deleted, "the folder's own evidence stays");
            Ok(())
        })
        .unwrap();
    }

    /// Upgrading a v15 database drops the evidence older builds left for
    /// items still stored (which the consistency check reports), and keeps
    /// the rest.
    #[test]
    fn v16_drops_cloud_evidence_for_stored_items() {
        let dir = std::env::temp_dir().join(format!("oneup-store-v16-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("state.db");
        {
            let mut s = Store::open(&path, 1024).unwrap();
            synced(&mut s);
            // Outside `write`, which would look for v18's tables at its end.
            s.read(|o| {
                // What a v15 database looks like.
                o.c.execute_batch(crate::pins::UNDO_V22)?;
                o.c.execute_batch(crate::shared::UNDO_V21)?;
                o.c.execute_batch(crate::dupes::UNDO_V20)?;
                o.c.execute_batch(crate::choose::UNDO_V19)?;
                o.c.execute_batch(crate::search::UNDO_V18)?;
                o.c.execute_batch(crate::history::UNDO_V23)?;
                o.c.execute_batch(
                    "DROP TRIGGER remote_back_ins; DROP TRIGGER remote_back_upd;
                     DROP INDEX enum_unit_pending; DROP INDEX remote_item_root; DROP TABLE retry;
                     INSERT INTO remote_gone(id) VALUES ('f'), ('gone');
                     INSERT INTO synced_item(sid, parent_sid, name, kind, remote_id) VALUES (9, 0, 'gone', 0, 'gone');
                     PRAGMA user_version = 15;",
                )?;
                let problems = o.check_invariants()?;
                assert!(
                    problems.iter().any(|p| p.contains("cloud items both present and noted deleted")),
                    "{problems:?}"
                );
                Ok(())
            })
            .unwrap();
        }
        let s = Store::open(&path, 1024).unwrap();
        s.read(|o| {
            assert!(o.check_invariants()?.is_empty(), "{:?}", o.check_invariants()?);
            assert!(matches!(o.remote(&RemoteId("f".into()))?, Seen::Present(_)));
            assert_eq!(o.remote(&RemoteId("gone".into()))?, Seen::Deleted, "evidence for an absent item stays");
            Ok(())
        })
        .unwrap();
        drop(s);
        std::fs::remove_dir_all(&dir).unwrap();
    }

    /// Forgetting a folder's record takes the records below it, and their
    /// evidence; v15 repairs what the old behaviour left (records hanging
    /// from a missing parent or from a file), keeping the rest.
    #[test]
    fn a_folder_record_goes_with_its_subtree_and_v15_drops_what_hung_loose() {
        assert_eq!(ROOT, 0, "MIGRATE_V15 starts from sid 0");
        let mut s = Store::open_in_memory().unwrap();
        synced(&mut s);
        s.write(|o| {
            let deep = o.synced_insert(&base(0, Some(2), "node_modules", NodeKind::Folder, 4, "nm"))?;
            o.synced_insert(&base(0, Some(deep), "dep.js", NodeKind::File, 5, "dep"))?;
            o.c.execute("INSERT INTO local_gone(file_id) VALUES (?1)", [blob(5)])?;
            assert!(o.synced_remove(2)?);
            assert_eq!(o.synced_children(ROOT)?.len(), 1, "only f is left");
            assert!(o.synced_by_local(5)?.is_none() && o.synced_by_remote("nm")?.is_none());
            assert_eq!(o.synced_stats()?.local_tombstones, 0, "their evidence went too");
            assert!(o.check_invariants()?.is_empty(), "{:?}", o.check_invariants()?);

            // What older builds left: a record whose parent is gone, one
            // below it, and one under a file.
            o.c.execute_batch(
                "INSERT INTO synced_item(sid, parent_sid, name, kind) VALUES (50, 49, 'node_modules', 1);
                 INSERT INTO synced_item(sid, parent_sid, name, kind) VALUES (51, 50, 'dep.js', 0);
                 INSERT INTO synced_item(sid, parent_sid, name, kind) VALUES (52, 1, 'x', 0);",
            )?;
            assert!(!o.check_invariants()?.is_empty());
            o.c.execute_batch(MIGRATE_V15)?;
            assert!(o.check_invariants()?.is_empty(), "{:?}", o.check_invariants()?);
            assert!(o.synced_get(50)?.is_none() && o.synced_get(51)?.is_none() && o.synced_get(52)?.is_none());
            assert!(o.synced_get(1)?.is_some() && o.synced_get(ROOT)?.is_some(), "the rest stays");
            Ok(())
        })
        .unwrap();
    }

    #[test]
    fn rows_roundtrip_and_links_are_unique() {
        let mut s = Store::open_in_memory().unwrap();
        synced(&mut s);
        s.write(|o| {
            let f = o.synced_by_local(2)?.unwrap();
            assert_eq!(f, Base { sid: 1, ..base(0, Some(ROOT), "f", NodeKind::File, 2, "f") });
            assert_eq!(o.synced_by_remote("d")?.map(|b| b.sid), Some(2));
            assert_eq!(o.synced_children(ROOT)?.len(), 2);
            // A second row linking an already linked item is refused.
            assert!(o.synced_put(&base(7, Some(ROOT), "x", NodeKind::File, 2, "other")).is_err());
            assert!(o.check_invariants()?.is_empty(), "{:?}", o.check_invariants()?);
            Ok(())
        })
        .unwrap();
    }

    /// What's left to convert: synced on both sides, not a placeholder by
    /// its recorded tag (any `CLOUD_*` variant counts), by sid, after the
    /// cursor; the root never.
    #[test]
    fn synced_items_that_are_not_placeholders_are_paged_by_sid() {
        let mut s = Store::open_in_memory().unwrap();
        synced(&mut s);
        let left = |s: &Store, after| -> Vec<Sid> {
            s.read(|o| o.synced_not_placeholders(after, 10)).unwrap().into_iter().map(|b| b.sid).collect()
        };
        assert_eq!(left(&s, ROOT), [1, 2]);
        assert_eq!(left(&s, 1), [2]);
        s.write(|o| o.local_update(&LocalRow { reparse_tag: 0x9000_601A, ..lrow(2, Some(1), "f", LocalKind::File) }))
            .unwrap();
        assert_eq!(left(&s, ROOT), [2], "a placeholder (CLOUD_6) is done");
        s.write(|o| o.local_update(&LocalRow { reparse_tag: 0xA000_000C, ..lrow(3, Some(1), "d", LocalKind::Folder) }))
            .unwrap();
        assert_eq!(left(&s, ROOT), [2], "a symbolic link isn't a placeholder");
    }

    #[test]
    fn sids_are_never_reused() {
        let mut s = Store::open_in_memory().unwrap();
        synced(&mut s);
        s.write(|o| {
            assert!(o.synced_remove(2)?);
            assert_eq!(o.synced_insert(&base(0, Some(ROOT), "g", NodeKind::File, 4, "g"))?, 3);
            Ok(())
        })
        .unwrap();
    }

    #[test]
    fn evidence_is_kept_only_for_synced_items_and_read_back() {
        let mut s = Store::open_in_memory().unwrap();
        synced(&mut s);
        s.write(|o| {
            // The file is gone on both sides with evidence; "x" was never synced.
            o.c.execute("DELETE FROM local_item WHERE file_id = ?1", [blob(2)])?;
            o.c.execute("DELETE FROM remote_item WHERE id = 'f'", [])?;
            assert!(o.note_local_deleted(2)?);
            assert!(o.note_remote_deleted("f")?);
            assert!(!o.note_remote_deleted("x")?);
            assert_eq!(o.local(LocalId(2))?, Seen::Deleted);
            assert_eq!(o.remote(&RemoteId("f".into()))?, Seen::Deleted);
            // Gone without evidence reads as Missing, never Deleted.
            o.c.execute("DELETE FROM remote_item WHERE id = 'd'", [])?;
            assert_eq!(o.remote(&RemoteId("d".into()))?, Seen::Missing);
            // Removing the row drops its evidence.
            o.synced_remove(1)?;
            assert_eq!(o.synced_stats()?.remote_tombstones + o.synced_stats()?.local_tombstones, 0);
            Ok(())
        })
        .unwrap();
    }

    #[test]
    fn a_deleted_cloud_folder_is_evidence_for_its_contents() {
        let mut s = Store::open_in_memory().unwrap();
        synced(&mut s);
        s.write(|o| {
            o.upsert_item(&rrow("g", Some("d"), "g", Kind::File), false)?;
            o.local_put(&lrow(4, Some(3), "g", LocalKind::File))?;
            o.synced_insert(&base(0, Some(2), "g", NodeKind::File, 4, "g"))?;
            o.delete_subtree("d")?;
            assert_eq!(o.remote(&RemoteId("d".into()))?, Seen::Deleted);
            assert_eq!(o.remote(&RemoteId("g".into()))?, Seen::Deleted);
            // A mirror rebuild forgets the Remote tree but keeps the evidence
            // and the Synced rows.
            o.clear_remote()?;
            assert_eq!(o.remote(&RemoteId("g".into()))?, Seen::Deleted);
            assert_eq!(o.remote(&RemoteId("f".into()))?, Seen::Missing);
            assert_eq!(o.synced_stats()?.items, 4);
            Ok(())
        })
        .unwrap();
    }

    #[test]
    fn unsyncable_items_read_as_missing_and_hashes_need_a_matching_stamp() {
        let mut s = Store::open_in_memory().unwrap();
        synced(&mut s);
        s.write(|o| {
            o.local_hash_put(2, 3, 9, &[7; 20])?;
            assert_eq!(o.local(LocalId(2))?.present().and_then(|n| n.hash), Some([7; 20]));
            o.local_hash_put(2, 3, 8, &[7; 20])?; // computed for an older version
            assert_eq!(o.local(LocalId(2))?.present().and_then(|n| n.hash), None);
            o.local_set_skip(3, Skip::Excluded)?;
            o.upsert_item(&rrow("d", Some("root"), "d", Kind::Folder), true)?;
            assert_eq!(o.local(LocalId(3))?, Seen::Missing);
            assert_eq!(o.remote(&RemoteId("d".into()))?, Seen::Missing);
            // Called through the trait: `Ops` has inherent methods of the
            // same names that return raw rows, unfiltered.
            assert_eq!(Trees::local_children(o, LocalId(1))?.len(), 1);
            assert_eq!(Trees::remote_children(o, &RemoteId("root".into()))?.len(), 1);
            // Folders and the root read as plain folders without size.
            let root = o.remote(&RemoteId("root".into()))?;
            assert!(root.present().is_some_and(|n| n.kind == NodeKind::Folder && n.size == 0));
            Ok(())
        })
        .unwrap();
    }

    #[test]
    fn the_onedrive_apps_stand_ins_for_packages_dont_sync() {
        let mut s = Store::open_in_memory().unwrap();
        synced(&mut s);
        s.write(|o| {
            o.upsert_item(&rrow("v", Some("root"), "Personal Vault", Kind::Package), false)?;
            o.upsert_item(&rrow("n", Some("d"), "Notes", Kind::Package), false)?;
            o.local_put(&lrow(4, Some(1), "Personal Vault.lnk", LocalKind::File))?;
            o.local_put(&lrow(5, Some(1), "Other.lnk", LocalKind::File))?;
            // A notebook's shortcut stands in only beside its own notebook.
            o.local_put(&lrow(6, Some(1), "notes.URL", LocalKind::File))?;
            o.local_put(&lrow(7, Some(3), "notes.URL", LocalKind::File))?;
            // Case beyond ASCII folds too, as Windows folds it.
            o.upsert_item(&rrow("g", Some("d"), "Σημειώσεις", Kind::Package), false)?;
            o.local_put(&lrow(8, Some(3), "ΣΗΜΕΙΏΣΕΙΣ.url", LocalKind::File))?;
            let names = |id| -> Result<Vec<String>> {
                Ok(Trees::local_children(o, LocalId(id))?.into_iter().map(|n| n.name).collect())
            };
            assert_eq!(names(1)?.len(), 4, "f, d, Other.lnk and notes.URL: {:?}", names(1)?);
            assert!(!names(1)?.contains(&"Personal Vault.lnk".to_string()));
            assert!(names(3)?.is_empty(), "{:?}", names(3)?);
            Ok(())
        })
        .unwrap();
    }

    /// The reconciler checks a deleted folder for children every round its
    /// parent is planned; the folder may hold hundreds of thousands.
    #[test]
    fn emptiness_checks_agree_with_the_lists_and_stop_at_the_first_child() {
        let mut s = Store::open_in_memory().unwrap();
        synced(&mut s);
        s.write(|o| {
            let (root, d) = (RemoteId("root".into()), RemoteId("d".into()));
            assert!(o.has_base_children(ROOT)? && !o.has_base_children(2)?);
            assert!(o.has_local_children(LocalId(1))? && !o.has_local_children(LocalId(3))?);
            assert!(o.has_remote_children(&root)? && !o.has_remote_children(&d)?);
            // Children the lists filter out don't count.
            o.local_put(&lrow(4, Some(3), "x", LocalKind::File))?;
            o.local_set_skip(4, Skip::Excluded)?;
            o.upsert_item(&rrow("x", Some("d"), "x", Kind::Package), false)?;
            assert!(!o.has_local_children(LocalId(3))? && !o.has_remote_children(&d)?);
            // A row after the first is never read: this one would fail to.
            o.c.execute("INSERT INTO synced_item(sid, parent_sid, name, kind) VALUES (99, 0, 'bad', 7)", [])?;
            assert!(o.synced_children(ROOT).is_err());
            assert!(o.any_base_child(ROOT, &mut |_| Ok(true))?);
            Ok(())
        })
        .unwrap();
    }

    #[test]
    fn children_queries_use_the_parent_indexes() {
        let s = Store::open_in_memory().unwrap();
        for sql in [
            base_children_sql(),
            local_children_sql(),
            remote_children_sql(),
            SYNCED_HAS_CHILDREN.into(),
            LOCAL_CHILDREN_LINKS.into(),
            REMOTE_CHILDREN_LINKED.into(),
        ] {
            let mut st = s.conn.prepare(&format!("EXPLAIN QUERY PLAN {sql}")).unwrap();
            let plan: Vec<String> =
                st.query_map([rusqlite::types::Null], |r| r.get(3)).unwrap().map(|r| r.unwrap()).collect();
            assert!(!plan.iter().any(|step| step.starts_with("SCAN")), "{sql}\n{plan:#?}");
        }
    }
}
