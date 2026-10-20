//! Instant search: an index of the Remote tree's names (schema v18).
//!
//! OneDrive's own listing is already mirrored here, so every name in the
//! drive can be found without asking OneDrive or downloading anything,
//! including online-only files Windows Search can't see.
//!
//! **The index.** `remote_name` is an FTS5 table with the trigram
//! tokenizer: any run of three or more characters of a name finds it, in
//! any case. It is *contentless* (the names stay in `remote_item` only),
//! keeps no positions (`detail=none`) and no column sizes, and stores per
//! entry just the item's ID, packed ([`pack`]): measured at about 42 bytes
//! per item in all with OneDrive's numeric IDs and 57 with its newer
//! `s<GUID>` IDs, against 134 for a plain trigram table. Without positions,
//! a search asks for items holding *all* the needle's trigrams, which can
//! over-match (`abc` and `bcd` don't make `abcd`), so every candidate is
//! checked against its stored name ([`Ops::search_names`]). Names are
//! indexed between a start and an end mark ([`indexed`]), so exact names
//! and names starting with the needle have selective queries of their own
//! and always come first.
//!
//! **Keeping it current.** Like the dirty marks ([`crate::dirty`]),
//! triggers on `remote_item` see every write, whoever makes it. They don't
//! write the index themselves: FTS5 flushes its pending writes at every
//! statement inside a trigger, which made each mirrored item cost ten times
//! as much. They queue the change (`search_queue`), and every write path
//! applies the queue just before it commits, in the same transaction. An
//! item's entry is keyed by `remote_item.sk`, a number the insert trigger
//! takes from the queue's own sequence (never reused, so an old entry can't
//! be mistaken for a new one).
//!
//! **Never in sync's way.** The contentless table must be told the exact
//! name an entry was made with to remove it, and removing an entry that
//! isn't there is an error ("database disk image is malformed"). So every
//! change is applied only if the entry is (or isn't) there, and if applying
//! the queue fails anyway, the queue is dropped, the index marked for a
//! rebuild, and the caller's write goes ahead: a broken index costs search,
//! never sync.
//!
//! **Built once.** A store from before v18 has items without a number.
//! [`Store::search_build`] numbers and indexes them in steps (each its own
//! short transaction, in key order from where the last one stopped), after
//! which nothing reads the whole tree again: the triggers keep the index
//! current at the cost of the changes.
//!
//! **Limits.** Only the Remote tree is indexed: what's in OneDrive. A
//! folder that's excluded is one row whose contents were never listed
//! (`remote_item.is_excluded`), so it can be found but nothing inside it
//! can; files on this PC that haven't been uploaded yet aren't found
//! either.

use rusqlite::OptionalExtension;
use rusqlite::types::{Value, ValueRef};

use crate::{ITEM_COLS, Ops, RemoteRow, Result, Store, fix_kind};

/// v17 -> v18: the name index, its queue and the triggers that fill the
/// queue. A store with no items yet has nothing to build.
pub(crate) const MIGRATE_V18: &str = "
ALTER TABLE remote_item ADD COLUMN sk INTEGER;

CREATE TABLE search_queue (
    seq  INTEGER PRIMARY KEY AUTOINCREMENT,
    -- 0: add the entry, 1: remove it.
    op   INTEGER NOT NULL DEFAULT 0,
    -- The entry's key; NULL: this row's own `seq`.
    sk   INTEGER,
    name TEXT NOT NULL,
    rid  TEXT NOT NULL
);

CREATE VIRTUAL TABLE remote_name USING fts5(
    name, item UNINDEXED,
    content='', contentless_unindexed=1, columnsize=0, detail=none, tokenize='trigram'
);

CREATE TRIGGER search_remote_ins AFTER INSERT ON remote_item WHEN NEW.kind != 2
BEGIN
    INSERT INTO search_queue(name, rid) VALUES (NEW.name, NEW.id);
    UPDATE remote_item SET sk = last_insert_rowid() WHERE id = NEW.id;
END;

CREATE TRIGGER search_remote_name AFTER UPDATE OF name ON remote_item
WHEN OLD.sk IS NOT NULL AND OLD.name IS NOT NEW.name
BEGIN
    INSERT INTO search_queue(op, sk, name, rid) VALUES (1, OLD.sk, OLD.name, OLD.id);
    INSERT INTO search_queue(op, sk, name, rid) VALUES (0, OLD.sk, NEW.name, NEW.id);
END;

-- Not while the whole tree is cleared (`clear_remote`), which empties the
-- index at once instead.
CREATE TRIGGER search_remote_del AFTER DELETE ON remote_item
WHEN OLD.sk IS NOT NULL AND NOT EXISTS (SELECT 1 FROM meta WHERE key = 'search.clearing')
BEGIN
    INSERT INTO search_queue(op, sk, name, rid) VALUES (1, OLD.sk, OLD.name, OLD.id);
END;

INSERT INTO meta(key, value) SELECT 'search.built', '1'
    WHERE NOT EXISTS (SELECT 1 FROM remote_item WHERE kind != 2);
";

/// Test support: turns a v18 database back into what v17 had, to test
/// upgrades.
#[cfg(test)]
pub(crate) const UNDO_V18: &str = "
DROP TRIGGER search_remote_ins; DROP TRIGGER search_remote_name; DROP TRIGGER search_remote_del;
DROP TABLE remote_name; DROP TABLE search_queue;
-- FTS5 leaves a contentless_unindexed table's content table behind.
DROP TABLE IF EXISTS remote_name_content;
ALTER TABLE remote_item DROP COLUMN sk;
DELETE FROM meta WHERE key LIKE 'search.%';
";

/// `meta` keys: the index holds every item (`1`); how far the build got
/// (the last key it numbered); why applying the queue failed; the whole
/// Remote tree is being cleared (the delete trigger queues nothing).
pub(crate) const CLEARING: &str = "search.clearing";
const BUILT: &str = "search.built";
const BUILD_AFTER: &str = "search.build_after";
const BROKEN: &str = "search.broken";

/// Shortest needle the index can answer: trigrams are three characters.
pub const MIN_CHARS: usize = 3;

/// Items a build step adds ([`Store::search_build`]): a fraction of a
/// second each, so other connections' writes never wait long.
pub const BUILD_STEP: usize = 20_000;

/// Queued changes applied per read of the queue (bounds memory when a
/// large folder is deleted or excluded).
const APPLY_BATCH: i64 = 10_000;

/// Most candidates one search checks against their names ([`Found::too_many`]). Over-matching
/// is rare, so this only ends searches for very common runs of letters
/// early (each candidate costs one lookup by key).
const MAX_CANDIDATES: usize = 20_000;

/// The queued changes, oldest first.
pub(crate) const QUEUED: &str = "SELECT seq, op, sk, name, rid FROM search_queue WHERE seq > ?1 ORDER BY seq LIMIT ?2";
const HAS_ENTRY: &str = "SELECT 1 FROM remote_name_content WHERE id = ?1";
const ADD_ENTRY: &str = "INSERT INTO remote_name(rowid, name, item) VALUES (?1, ?2, ?3)";
const REMOVE_ENTRY: &str = "INSERT INTO remote_name(remote_name, rowid, name) VALUES ('delete', ?1, ?2)";
/// What every ID of the drive starts with: the root's, up to its `!`.
const ID_PREFIX: &str = "SELECT substr(id, 1, instr(id, '!')) FROM remote_item WHERE kind = 2 LIMIT 1";
/// Marks around each name as the index holds it ([`indexed`]), so a query
/// can ask for names that start with the needle, or are it: no name holds
/// either character (Windows and OneDrive refuse control characters).
const START: char = '\u{1}';
const END: char = '\u{2}';

/// A name as the index holds it: between [`START`] and [`END`]. Adding and
/// removing an entry both go through this, so they always agree.
fn indexed(name: &str) -> String {
    format!("{START}{name}{END}")
}

/// The candidates for a needle, the latest entries first.
pub(crate) const CANDIDATES: &str =
    "SELECT rowid, item FROM remote_name WHERE remote_name MATCH ?1 ORDER BY rowid DESC";
/// Items the build hasn't seen yet, in key order from where it stopped.
pub(crate) const TO_BUILD: &str =
    "SELECT id, name, sk FROM remote_item WHERE id > ?1 AND kind != 2 ORDER BY id LIMIT ?2";

/// An item ID as the index stores it: without the drive's prefix (`<16 hex
/// digits>!`, the same for every item), and the rest as a number or 16
/// bytes where it is one (`…!123456`, `…!s<32 hex digits>`), so it costs
/// 3 to 17 bytes instead of 23 to 50. Anything else is kept whole.
pub(crate) fn pack(id: &str, prefix: &str) -> Value {
    let rest = match id.strip_prefix(prefix) {
        Some(r) if !prefix.is_empty() && !r.contains('!') => r,
        _ => return Value::Text(id.to_string()),
    };
    let digits = rest.len() <= 18 && rest.bytes().all(|b| b.is_ascii_digit()) && !rest.starts_with('0');
    if digits && let Ok(n) = rest.parse::<i64>() {
        return Value::Integer(n);
    }
    if let Some(hex) = rest.strip_prefix('s')
        && hex.len() == 32
        && hex.bytes().all(|b| b.is_ascii_digit() || (b'a'..=b'f').contains(&b))
    {
        let bytes = (0..16).map(|i| u8::from_str_radix(&hex[2 * i..2 * i + 2], 16)).collect();
        if let Ok(b) = bytes {
            return Value::Blob(b);
        }
    }
    Value::Text(rest.to_string())
}

/// [`pack`] undone. Text with a `!` was kept whole.
pub(crate) fn unpack(v: ValueRef<'_>, prefix: &str) -> Option<String> {
    match v {
        ValueRef::Integer(n) => Some(format!("{prefix}{n}")),
        ValueRef::Blob(b) if b.len() == 16 => {
            let hex: String = b.iter().map(|x| format!("{x:02x}")).collect();
            Some(format!("{prefix}s{hex}"))
        }
        ValueRef::Text(t) => {
            let t = std::str::from_utf8(t).ok()?;
            Some(if prefix.is_empty() || t.contains('!') { t.to_string() } else { format!("{prefix}{t}") })
        }
        _ => None,
    }
}

/// The index query for `needle`: every trigram of it (as FTS5 strings),
/// all required. `None` if it has none (too short).
pub(crate) fn match_expr(needle: &str) -> Option<String> {
    let chars: Vec<char> = needle.chars().collect();
    let mut grams: Vec<String> = chars.windows(MIN_CHARS).map(|w| w.iter().collect()).collect();
    grams.sort();
    grams.dedup();
    if grams.is_empty() {
        return None;
    }
    Some(grams.iter().map(|g| format!("\"{}\"", g.replace('"', "\"\""))).collect::<Vec<_>>().join(" AND "))
}

/// One item a search found.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Hit {
    pub row: RemoteRow,
    /// An excluded folder: its contents aren't in the mirror.
    pub excluded: bool,
}

/// What a search found.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Found {
    /// Exact names first, then names starting with the needle, then the
    /// rest; the latest added to the index first within each. Every exact
    /// name comes before the rest, however many others match.
    pub hits: Vec<Hit>,
    /// More items match than were returned.
    pub more: bool,
    /// A query stopped after reading [`MAX_CANDIDATES`] candidates without
    /// finding enough, so some matches may be missing, maybe all: the
    /// needle's trigrams are common apart but rarely together. Someone has
    /// to type more.
    pub too_many: bool,
}

/// Where the index stands.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum IndexState {
    /// Holds every item.
    Ready,
    /// Items from before the index still need adding ([`Store::search_build`]).
    Building,
    /// Applying changes failed; it must be built again.
    Broken,
}

impl Ops<'_> {
    pub fn search_state(&self) -> Result<IndexState> {
        if self.meta_get(BROKEN)?.is_some() {
            return Ok(IndexState::Broken);
        }
        Ok(if self.meta_get(BUILT)?.is_some() { IndexState::Ready } else { IndexState::Building })
    }

    fn id_prefix(&self) -> Result<String> {
        let p: Option<String> = self.c.prepare_cached(ID_PREFIX)?.query_row([], |r| r.get(0)).optional()?;
        Ok(p.unwrap_or_default())
    }

    /// Applies the queued changes to the index (see the module docs). Never
    /// fails the caller's transaction for the index's sake: on an error the
    /// index is marked for a rebuild instead.
    pub(crate) fn search_apply(&self) -> Result<()> {
        let queued: bool =
            self.c.prepare_cached("SELECT EXISTS (SELECT 1 FROM search_queue)")?.query_row([], |r| r.get(0))?;
        if !queued {
            return Ok(());
        }
        self.c.execute_batch("SAVEPOINT search_apply")?;
        match self.apply_queued() {
            Ok(()) => self.c.execute_batch("RELEASE search_apply")?,
            Err(e) => {
                self.c.execute_batch("ROLLBACK TO search_apply; RELEASE search_apply")?;
                // A full disk or a lock fails the write itself too; anything
                // else is the index's own trouble.
                if e.is_disk_full() || e.is_busy() {
                    return Err(e);
                }
                self.c.execute("DELETE FROM search_queue", [])?;
                self.meta_set(BROKEN, &e.to_string())?;
            }
        }
        Ok(())
    }

    fn apply_queued(&self) -> Result<()> {
        let prefix = self.id_prefix()?;
        let mut after = 0i64;
        loop {
            type Queued = (i64, i64, Option<i64>, String, String);
            let rows: Vec<Queued> = self
                .c
                .prepare_cached(QUEUED)?
                .query_map([after, APPLY_BATCH], |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?, r.get(3)?, r.get(4)?)))?
                .collect::<rusqlite::Result<_>>()?;
            let Some(last) = rows.last().map(|r| r.0) else { break };
            let mut has = self.c.prepare_cached(HAS_ENTRY)?;
            for (seq, op, sk, name, rid) in rows {
                let key = sk.unwrap_or(seq);
                let there = has.exists([key])?;
                if op == 1 && there {
                    self.c.prepare_cached(REMOVE_ENTRY)?.execute(rusqlite::params![key, indexed(&name)])?;
                } else if op == 0 && !there {
                    let item = pack(&rid, &prefix);
                    self.c.prepare_cached(ADD_ENTRY)?.execute(rusqlite::params![key, indexed(&name), item])?;
                }
            }
            self.c.prepare_cached("DELETE FROM search_queue WHERE seq <= ?1")?.execute([last])?;
            after = last;
        }
        Ok(())
    }

    /// Adds up to `limit` items the index doesn't have yet (from a store
    /// older than it), continuing from where the last step stopped. Returns
    /// how many it looked at; fewer than `limit` means the index is now
    /// complete. The caller's [`Store::write`] applies them.
    pub fn search_build_step(&self, limit: usize) -> Result<usize> {
        let after = self.meta_get(BUILD_AFTER)?.unwrap_or_default();
        let rows: Vec<(String, String, Option<i64>)> = self
            .c
            .prepare_cached(TO_BUILD)?
            .query_map(rusqlite::params![after, limit as i64], |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?)))?
            .collect::<rusqlite::Result<_>>()?;
        for (id, name, sk) in &rows {
            match sk {
                // Numbered since the index came: queued again in case the
                // index was emptied for a rebuild (applying skips it if not).
                Some(sk) => {
                    self.c
                        .prepare_cached("INSERT INTO search_queue(sk, name, rid) VALUES (?1, ?2, ?3)")?
                        .execute(rusqlite::params![sk, name, id])?;
                }
                None => {
                    self.c
                        .prepare_cached("INSERT INTO search_queue(name, rid) VALUES (?1, ?2)")?
                        .execute([name, id])?;
                    let sk = self.c.last_insert_rowid();
                    self.c
                        .prepare_cached("UPDATE remote_item SET sk = ?1 WHERE id = ?2")?
                        .execute(rusqlite::params![sk, id])?;
                }
            }
        }
        match rows.last() {
            Some((id, _, _)) if rows.len() == limit => self.meta_set(BUILD_AFTER, id)?,
            _ => {
                self.meta_delete(BUILD_AFTER)?;
                self.meta_set(BUILT, "1")?;
            }
        }
        Ok(rows.len())
    }

    /// Empties the index for a rebuild ([`IndexState::Broken`]).
    pub fn search_reset(&self) -> Result<()> {
        self.c.execute_batch(
            "INSERT INTO remote_name(remote_name) VALUES ('delete-all');
             DELETE FROM search_queue;",
        )?;
        for k in [BUILT, BUILD_AFTER, BROKEN] {
            self.meta_delete(k)?;
        }
        Ok(())
    }

    /// Clears the Remote tree's rows with `clear` ([`Ops::clear_remote`])
    /// and every index entry with them, at once: the delete trigger queues
    /// nothing meanwhile (on a large drive that would be millions of rows
    /// thrown away). What's listed again is added as it comes.
    pub(crate) fn search_clear_with(&self, clear: impl FnOnce() -> Result<()>) -> Result<()> {
        self.meta_set(CLEARING, "1")?;
        clear()?;
        self.meta_delete(CLEARING)?;
        self.search_reset()?;
        self.meta_set(BUILT, "1")
    }

    /// Up to `limit` stored items whose name contains `needle` (as
    /// [`oneup_sync::fold`] compares names; at least [`MIN_CHARS`]
    /// characters, else nothing), the drive's root aside. Cost: the index
    /// lookup, and one lookup by key per candidate.
    pub fn search_names(&self, needle: &str, limit: usize) -> Result<Found> {
        self.search_names_within(needle, limit, MAX_CANDIDATES)
    }

    /// [`Ops::search_names`], reading at most `cap` candidates.
    fn search_names_within(&self, needle: &str, limit: usize, cap: usize) -> Result<Found> {
        let Some(anywhere) = match_expr(needle) else { return Ok(Found::default()) };
        let id_prefix = self.id_prefix()?;
        let folded = oneup_sync::fold(needle);
        // Exact names, then names starting with the needle, then the rest,
        // each a query of its own (the start and end marks make the first
        // two selective), the latest entries first within each. Each stops
        // once enough are found, so a common needle costs what it lists.
        type Fits = fn(&str, &str) -> bool;
        let passes: [(Option<String>, Fits); 3] = [
            (match_expr(&format!("{START}{needle}{END}")), |name, n| name == n),
            (match_expr(&format!("{START}{needle}")), |name, n| name.starts_with(n)),
            (Some(anywhere), |name, n| name.contains(n)),
        ];
        let mut taken: Vec<String> = Vec::new();
        let mut taken_ids = std::collections::HashSet::new();
        let mut too_many = false;
        let mut st = self.c.prepare_cached(CANDIDATES)?;
        let mut name_of = self.c.prepare_cached("SELECT name FROM remote_item WHERE id = ?1")?;
        for (expr, fits) in passes {
            let Some(expr) = expr else { continue };
            let mut rows = st.query([&expr])?;
            let mut read = 0;
            while taken.len() <= limit {
                let Some(r) = rows.next()? else { break };
                read += 1;
                if read > cap {
                    too_many = true;
                    break;
                }
                let Some(id) = unpack(r.get_ref(1)?, &id_prefix) else { continue };
                if taken_ids.contains(&id) {
                    continue;
                }
                let Some(name) = name_of.query_row([&id], |r| r.get::<_, String>(0)).optional()? else { continue };
                if fits(&oneup_sync::fold(&name), &folded) {
                    taken_ids.insert(id.clone());
                    taken.push(id);
                }
            }
            if taken.len() > limit {
                break;
            }
        }
        let more = too_many || taken.len() > limit;
        taken.truncate(limit);
        let mut get =
            self.c.prepare_cached(&format!("SELECT {ITEM_COLS}, is_excluded FROM remote_item WHERE id = ?1"))?;
        let mut hits = Vec::with_capacity(taken.len());
        for id in taken {
            let found = get
                .query_row([&id], |r| Ok((RemoteRow::from_row(r)?, r.get::<_, i64>("is_excluded")? != 0)))
                .optional()?;
            if let Some((row, excluded)) = found {
                hits.push(Hit { row: fix_kind(row)?, excluded });
            }
        }
        Ok(Found { hits, more, too_many })
    }
}

impl Ops<'_> {
    /// What's wrong with a built index: items without an entry, entries
    /// for no item, changes not applied. For verification and tests: it
    /// reads every item and entry. Not part of [`Ops::check_invariants`],
    /// which stops sync, as the index never should.
    pub fn search_problems(&self) -> Result<Vec<String>> {
        if self.search_state()? != IndexState::Ready {
            return Ok(vec![format!("index {:?}", self.search_state()?)]);
        }
        let one = |sql: &str| -> Result<i64> { Ok(self.c.query_row(sql, [], |r| r.get(0))?) };
        let mut problems = Vec::new();
        let queued = one("SELECT count(*) FROM search_queue")?;
        if queued > 0 {
            problems.push(format!("{queued} changes not applied"));
        }
        let missing = one("SELECT count(*) FROM remote_item r WHERE r.kind != 2
             AND (r.sk IS NULL OR NOT EXISTS (SELECT 1 FROM remote_name_content c WHERE c.id = r.sk))")?;
        if missing > 0 {
            problems.push(format!("{missing} items not in the index"));
        }
        let prefix = self.id_prefix()?;
        let mut st = self.c.prepare("SELECT id, c1 FROM remote_name_content")?;
        let mut rows = st.query([])?;
        let mut stray = 0;
        while let Some(r) = rows.next()? {
            let key: i64 = r.get(0)?;
            let ok = match unpack(r.get_ref(1)?, &prefix) {
                Some(id) => self
                    .c
                    .prepare_cached("SELECT 1 FROM remote_item WHERE id = ?1 AND sk = ?2")?
                    .exists(rusqlite::params![id, key])?,
                None => false,
            };
            stray += i64::from(!ok);
        }
        if stray > 0 {
            problems.push(format!("{stray} index entries for no item"));
        }
        Ok(problems)
    }
}

impl Store {
    /// Makes the index complete, `step` items per transaction (so other
    /// connections' writes wait at most one step). Before each step it asks
    /// `go_on`, with the number added so far: `false` stops the build there
    /// (before the first step: nothing is done), to go on from that point
    /// next time. Starts over if the index is [`IndexState::Broken`].
    /// Returns how many items it looked at (0 once built: nothing is read
    /// then, and `go_on` isn't asked).
    pub fn search_build(&mut self, step: usize, mut go_on: impl FnMut(u64) -> bool) -> Result<u64> {
        let step = step.max(1);
        let mut done = 0u64;
        // Without taking the write lock: a search asks first, every time.
        if self.read(|o| o.search_state())? == IndexState::Ready {
            return Ok(0);
        }
        loop {
            if !go_on(done) {
                return Ok(done);
            }
            let n = self.write(|o| match o.search_state()? {
                IndexState::Ready => Ok(None),
                IndexState::Broken => {
                    o.search_reset()?;
                    Ok(Some(0))
                }
                IndexState::Building => o.search_build_step(step).map(Some),
            })?;
            match n {
                None => return Ok(done),
                Some(n) => done += n as u64,
            }
        }
    }
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used)]
    use rusqlite::types::Value;

    use super::*;
    use crate::synced::tests::rrow;
    use crate::{Kind, Store};

    const P: &str = "a1b2c3d4e5f60708!";

    fn id(n: u32) -> String {
        format!("{P}{n}")
    }

    fn names(s: &Store, needle: &str) -> Vec<String> {
        s.read(|o| o.search_names(needle, 50)).unwrap().hits.into_iter().map(|h| h.row.name).collect()
    }

    fn put(s: &mut Store, n: u32, parent: u32, name: &str, kind: Kind) {
        s.write(|o| o.upsert_item(&rrow(&id(n), Some(&id(parent)), name, kind), false)).unwrap();
    }

    /// A drive with OneDrive's numeric IDs: root, two folders, files.
    fn drive(s: &mut Store) {
        s.write(|o| {
            o.upsert_item(&rrow(&id(1), None, "root", Kind::Root), false)?;
            o.upsert_item(&rrow(&id(2), Some(&id(1)), "Reports", Kind::Folder), false)?;
            o.upsert_item(&rrow(&id(3), Some(&id(2)), "Quarterly report.docx", Kind::File), false)?;
            o.upsert_item(&rrow(&id(4), Some(&id(2)), "REPORT", Kind::File), false)?;
            o.upsert_item(&rrow(&id(5), Some(&id(1)), "report.txt", Kind::File), false)?;
            o.upsert_item(&rrow(&id(6), Some(&id(1)), "node_modules", Kind::Folder), true)?;
            o.upsert_item(&rrow(&id(7), Some(&id(1)), "Ρεπορτ ΟΔΟΣ.pdf", Kind::File), false)
        })
        .unwrap();
    }

    #[test]
    fn ids_are_stored_small_and_come_back_whole() {
        let guid = format!("{P}s0123456789abcdef0123456789abcdef");
        let cases = [
            (format!("{P}123456"), Some(Value::Integer(123_456))),
            (guid.clone(), None),
            (format!("{P}s0123456789ABCDEF0123456789ABCDEF"), None),
            (format!("{P}0123"), Some(Value::Text("0123".into()))),
            (format!("{P}0"), Some(Value::Text("0".into()))),
            (format!("{P}1234567890123456789"), Some(Value::Text("1234567890123456789".into()))),
            (format!("{P}12!34"), None),
            (P.to_string(), Some(Value::Text(String::new()))),
            ("other-drive!5".to_string(), None),
        ];
        for (id, want) in cases {
            let v = pack(&id, P);
            if let Some(w) = want {
                assert_eq!(v, w, "{id}");
            }
            let back = match &v {
                Value::Integer(n) => unpack(ValueRef::Integer(*n), P),
                Value::Blob(b) => unpack(ValueRef::Blob(b), P),
                Value::Text(t) => unpack(ValueRef::Text(t.as_bytes()), P),
                _ => None,
            };
            assert_eq!(back.as_deref(), Some(id.as_str()), "{id} as {v:?}");
        }
        assert!(matches!(pack(&guid, P), Value::Blob(b) if b.len() == 16));
        // A drive whose IDs have no `!` (the tests' fake OneDrive): kept whole.
        assert_eq!(pack("f1", ""), Value::Text("f1".into()));
        assert_eq!(unpack(ValueRef::Text(b"f1"), "").as_deref(), Some("f1"));
    }

    #[test]
    fn the_query_asks_for_every_trigram() {
        assert_eq!(match_expr("ab"), None);
        assert_eq!(match_expr("abcd").as_deref(), Some(r#""abc" AND "bcd""#));
        assert_eq!(match_expr("aaaa").as_deref(), Some(r#""aaa""#));
        assert_eq!(match_expr(r#"a"bc"#).as_deref(), Some(r#""""bc" AND "a""b""#));
    }

    #[test]
    fn any_part_of_a_name_finds_it_in_any_case_best_matches_first() {
        let mut s = Store::open_in_memory().unwrap();
        drive(&mut s);
        assert_eq!(s.read(|o| o.search_state()).unwrap(), IndexState::Ready, "a new store needs no build");
        assert_eq!(names(&s, "report"), ["REPORT", "report.txt", "Reports", "Quarterly report.docx"]);
        assert_eq!(names(&s, "ORT.D"), ["Quarterly report.docx"]);
        assert_eq!(names(&s, "οδοσ"), ["Ρεπορτ ΟΔΟΣ.pdf"], "folded as names are compared");
        assert!(names(&s, "roo").is_empty(), "the drive's root isn't an item to find");
        assert!(names(&s, "re").is_empty(), "too short");
        assert!(names(&s, "reportx").is_empty());
        let x = s.read(|o| o.search_names("node_mod", 5)).unwrap();
        assert!(x.hits[0].excluded, "an excluded folder is found, and says so");
        let one = s.read(|o| o.search_names("report", 1)).unwrap();
        assert_eq!((one.hits.len(), one.more), (1, true));
    }

    /// The executor's write that doesn't wait for the database applies
    /// the index's queue like the others: every write path ends in one
    /// commit.
    #[test]
    fn a_write_that_does_not_wait_reaches_the_index_too() {
        let mut s = Store::open_in_memory().unwrap();
        drive(&mut s);
        let r = s.try_write_now(|o| -> Result<()> {
            o.upsert_item(&rrow(&id(9), Some(&id(1)), "fresh.txt", Kind::File), false)?;
            o.upsert_item(&rrow(&id(5), Some(&id(1)), "renamed.txt", Kind::File), false)
        });
        assert!(matches!(r, Ok(Ok(()))));
        assert_eq!(names(&s, "fresh"), ["fresh.txt"]);
        assert_eq!(names(&s, "renamed"), ["renamed.txt"]);
        assert!(!names(&s, "report").contains(&"report.txt".to_string()));
        assert!(s.read(|o| o.search_problems()).unwrap().is_empty());
    }

    /// Names holding a needle's trigrams apart, but not the needle, can be
    /// so many that the search gives up reading them: it says so, rather
    /// than "nothing found".
    #[test]
    fn too_many_candidates_ask_for_more_typing() {
        let mut s = Store::open_in_memory().unwrap();
        drive(&mut s);
        s.write(|o| {
            // The real match first, so the newest-first read reaches it last.
            o.upsert_item(&rrow(&id(20), Some(&id(1)), "xabcdx", Kind::File), false)?;
            for n in 21..30 {
                o.upsert_item(&rrow(&id(n), Some(&id(1)), &format!("abc {n} bcd"), Kind::File), false)?;
            }
            Ok(())
        })
        .unwrap();
        let few = s.read(|o| o.search_names_within("abcd", 50, 5)).unwrap();
        assert!(few.hits.is_empty() && few.too_many && few.more, "{few:?}");
        let all = s.read(|o| o.search_names_within("abcd", 50, 100)).unwrap();
        assert_eq!((all.hits.len(), all.too_many), (1, false));
    }

    /// An exact name comes first even when hundreds of newer names merely
    /// contain it: every candidate is looked at, only the best are kept.
    #[test]
    fn an_exact_name_comes_first_however_many_newer_matches_there_are() {
        let mut s = Store::open_in_memory().unwrap();
        drive(&mut s);
        s.write(|o| {
            o.upsert_item(&rrow(&id(100), Some(&id(1)), "Budget", Kind::File), false)?;
            o.upsert_item(&rrow(&id(101), Some(&id(1)), "budget plan", Kind::File), false)?;
            for n in 200..500 {
                o.upsert_item(&rrow(&id(n), Some(&id(1)), &format!("old budget {n}.xlsx"), Kind::File), false)?;
            }
            Ok(())
        })
        .unwrap();
        let f = s.read(|o| o.search_names("budget", 50)).unwrap();
        assert_eq!(f.hits.len(), 50);
        assert_eq!((f.hits[0].row.name.as_str(), f.hits[1].row.name.as_str()), ("Budget", "budget plan"));
        // Within a rank, the latest entries first.
        assert_eq!(f.hits[2].row.name, "old budget 499.xlsx");
        assert!(f.more && !f.too_many);
        let one = s.read(|o| o.search_names("budget", 1)).unwrap();
        assert_eq!((one.hits[0].row.name.as_str(), one.more), ("Budget", true));
    }

    /// Clearing the whole Remote tree (listing OneDrive again) empties the
    /// index at once, without queueing a removal per item.
    #[test]
    fn clearing_the_remote_tree_queues_nothing() {
        let mut s = Store::open_in_memory().unwrap();
        drive(&mut s);
        let seq = |s: &Store| -> i64 {
            s.read(|o| {
                Ok(o.c.query_row("SELECT seq FROM sqlite_sequence WHERE name = 'search_queue'", [], |r| r.get(0))?)
            })
            .unwrap()
        };
        let before = seq(&s);
        s.write(|o| {
            o.clear_remote()?;
            let queued: i64 = o.c.query_row("SELECT count(*) FROM search_queue", [], |r| r.get(0))?;
            assert_eq!(queued, 0);
            assert_eq!(o.meta_get(CLEARING)?, None, "the flag goes with the clear");
            Ok(())
        })
        .unwrap();
        assert_eq!(seq(&s), before, "no queue row was ever written");
        assert!(names(&s, "report").is_empty());
        // Removing items one by one still reaches the index.
        drive(&mut s);
        s.write(|o| o.delete_subtree(&id(5)).map(drop)).unwrap();
        assert!(!names(&s, "report").contains(&"report.txt".to_string()));
        assert!(s.read(|o| o.search_problems()).unwrap().is_empty());
    }

    #[test]
    fn renames_deletions_and_exclusions_keep_the_index_current() {
        let mut s = Store::open_in_memory().unwrap();
        drive(&mut s);
        put(&mut s, 5, 2, "minutes.txt", Kind::File);
        assert_eq!(names(&s, "minutes"), ["minutes.txt"]);
        assert!(!names(&s, "report").contains(&"report.txt".to_string()));
        // Renamed twice, back to where it was, in one transaction.
        s.write(|o| {
            o.upsert_item(&rrow(&id(5), Some(&id(2)), "a.txt", Kind::File), false)?;
            o.upsert_item(&rrow(&id(5), Some(&id(2)), "minutes.txt", Kind::File), false)
        })
        .unwrap();
        assert_eq!(names(&s, "minutes"), ["minutes.txt"]);
        // Added and removed in one transaction.
        s.write(|o| {
            o.upsert_item(&rrow(&id(8), Some(&id(1)), "fleeting", Kind::File), false)?;
            o.delete_subtree(&id(8)).map(drop)
        })
        .unwrap();
        assert!(names(&s, "fleeting").is_empty());
        // A folder becomes excluded: its contents go, it stays.
        s.write(|o| {
            o.prune_subtree(&id(2), false)?;
            o.upsert_item(&rrow(&id(2), Some(&id(1)), "Reports", Kind::Folder), true)
        })
        .unwrap();
        assert_eq!(names(&s, "report"), ["Reports"]);
        s.write(|o| o.delete_subtree(&id(2)).map(drop)).unwrap();
        assert!(names(&s, "report").is_empty());
        s.write(|o| o.clear_remote()).unwrap();
        assert!(names(&s, "pdf").is_empty());
        assert_eq!(s.read(|o| o.search_state()).unwrap(), IndexState::Ready);
        drive(&mut s);
        assert_eq!(names(&s, "pdf"), ["Ρεπορτ ΟΔΟΣ.pdf"]);
        s.read(|o| {
            let entries: i64 = o.c.query_row("SELECT count(*) FROM remote_name_content", [], |r| r.get(0))?;
            assert_eq!(entries, 6, "one entry per item, the root aside");
            assert!(o.search_problems()?.is_empty(), "{:?}", o.search_problems()?);
            Ok(())
        })
        .unwrap();
    }

    /// A store from before the index gets it in steps, while it keeps
    /// changing in between; the items it had are found once built.
    #[test]
    fn an_older_store_builds_its_index_in_steps() {
        let dir = std::env::temp_dir().join(format!("oneup-store-search-up-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("state.db");
        {
            let mut s = Store::open(&path, 1024).unwrap();
            drive(&mut s);
            s.read(|o| {
                let undo = format!(
                    "{} {} {} {} {UNDO_V18} PRAGMA user_version = 17;",
                    crate::pins::UNDO_V22,
                    crate::shared::UNDO_V21,
                    crate::dupes::UNDO_V20,
                    crate::choose::UNDO_V19
                );
                Ok(o.c.execute_batch(&undo)?)
            })
            .unwrap();
        }
        let mut s = Store::open(&path, 1024).unwrap();
        assert_eq!(s.read(|o| o.search_state()).unwrap(), IndexState::Building);
        assert!(names(&s, "report").is_empty(), "not built yet");
        // Changes before and during the build: a new item, a rename of an
        // item the build hasn't reached, a deletion.
        put(&mut s, 9, 1, "new report", Kind::File);
        put(&mut s, 7, 1, "renamed.pdf", Kind::File);
        assert_eq!(names(&s, "report"), ["new report"], "new items are found at once");
        s.write(|o| o.search_build_step(2).map(drop)).unwrap();
        s.write(|o| o.delete_subtree(&id(4)).map(drop)).unwrap();
        let mut seen = Vec::new();
        // Told to stop before it starts: nothing done.
        let cursor = |s: &Store| s.read(|o| o.meta_get(BUILD_AFTER)).unwrap();
        let before = cursor(&s);
        assert_eq!(s.search_build(2, |_| false).unwrap(), 0);
        assert_eq!(cursor(&s), before);
        // Stopped after a step, then carried on.
        let first = s.search_build(2, |done| done == 0).unwrap();
        assert_eq!((first, s.read(|o| o.search_state()).unwrap()), (2, IndexState::Building));
        let n = s
            .search_build(2, |n| {
                seen.push(n);
                true
            })
            .unwrap();
        assert!(n > 0 && !seen.is_empty());
        assert_eq!(s.read(|o| o.search_state()).unwrap(), IndexState::Ready);
        // The build's entries are newer than the item added before it.
        assert_eq!(names(&s, "report"), ["report.txt", "Reports", "Quarterly report.docx", "new report"]);
        assert_eq!(names(&s, "renamed"), ["renamed.pdf"]);
        assert!(names(&s, "ΟΔΟΣ").is_empty());
        assert_eq!(s.search_build(2, |_| true).unwrap(), 0, "once built, nothing is read");
        assert!(s.read(|o| o.search_problems()).unwrap().is_empty());
        drop(s);
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// Applying changes to the index can fail; the write it belongs to
    /// still goes ahead, and the index is built again later.
    #[test]
    fn a_failing_index_never_fails_a_write_and_is_rebuilt() {
        let mut s = Store::open_in_memory().unwrap();
        drive(&mut s);
        s.read(|o| {
            Ok(o.c.execute_batch(
                "CREATE TRIGGER boom BEFORE INSERT ON remote_name_content BEGIN SELECT RAISE(ABORT, 'boom'); END;",
            )?)
        })
        .unwrap();
        put(&mut s, 9, 1, "kept.txt", Kind::File);
        assert!(s.read(|o| o.get_item(&id(9))).unwrap().is_some(), "the write went ahead");
        assert_eq!(s.read(|o| o.search_state()).unwrap(), IndexState::Broken);
        // Later writes don't fail either.
        s.write(|o| o.delete_subtree(&id(5)).map(drop)).unwrap();
        s.read(|o| Ok(o.c.execute_batch("DROP TRIGGER boom;")?)).unwrap();
        s.search_build(3, |_| true).unwrap();
        assert_eq!(s.read(|o| o.search_state()).unwrap(), IndexState::Ready);
        assert_eq!(names(&s, "kept"), ["kept.txt"]);
        assert!(s.read(|o| o.search_problems()).unwrap().is_empty());
        assert_eq!(names(&s, "report"), ["REPORT", "Reports", "Quarterly report.docx"]);
    }

    /// Once built, the index costs what changes: items added, moved and
    /// renamed, and asking the build again, read no table that grows with
    /// the drive.
    #[cfg(feature = "statement-log")]
    #[test]
    fn once_built_upkeep_reads_no_whole_table() {
        let mut s = Store::open_in_memory().unwrap();
        drive(&mut s);
        s.log_statements();
        put(&mut s, 9, 2, "new.txt", Kind::File);
        put(&mut s, 3, 1, "moved and renamed.docx", Kind::File);
        // (Removing subtrees walks them: `subtree_queries_never_scan`.)
        assert_eq!(s.search_build(1000, |_| true).unwrap(), 0);
        let statements = Store::take_statements();
        assert!(statements.iter().any(|q| q.contains("remote_name")), "{statements:#?}");
        let scans = s.tree_scans(&statements).unwrap();
        assert!(scans.is_empty(), "{scans:#?}");
    }

    /// Every statement the index runs, in the triggers, when applying the
    /// queue, building and searching, looks rows up by key or through the
    /// index: none reads a table that grows with the drive.
    #[test]
    fn index_upkeep_and_search_never_scan_the_tree() {
        let s = Store::open_in_memory().unwrap();
        let plan = |sql: &str| -> Vec<String> {
            let mut st = s.conn.prepare(&format!("EXPLAIN QUERY PLAN {sql}")).unwrap();
            let nulls = std::iter::repeat_n(rusqlite::types::Null, st.parameter_count());
            st.query_map(rusqlite::params_from_iter(nulls), |r| r.get(3)).unwrap().map(|r| r.unwrap()).collect()
        };
        for sql in [
            QUEUED,
            HAS_ENTRY,
            ID_PREFIX,
            TO_BUILD,
            "DELETE FROM search_queue WHERE seq <= ?1",
            "SELECT 1 FROM meta WHERE key = ?1",
            "SELECT name FROM remote_item WHERE id = ?1",
            // The triggers' statements, with parameters for NEW and OLD.
            "UPDATE remote_item SET sk = ?1 WHERE id = ?2",
            "SELECT id, parent_id, name, kind, size, etag, qxh, mtime, is_excluded FROM remote_item WHERE id = ?1",
        ] {
            let p = plan(sql);
            assert!(!p.iter().any(|step| step.starts_with("SCAN")), "{sql}\n{p:#?}");
        }
        let p = plan(CANDIDATES);
        assert!(p.iter().all(|step| step.contains("VIRTUAL TABLE INDEX")), "{p:#?}");
    }
}
