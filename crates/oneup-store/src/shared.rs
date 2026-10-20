//! Which cloud items are shared (schema v21), for the sharing audit.
//!
//! The mirror keeps Graph's `shared` facet as it arrives with delta, in a
//! side table holding a row only for shared items: an unshared item costs
//! nothing, where a column on `remote_item` would cost every row a byte or
//! more. Listing what's shared then needs no request to OneDrive; who it's
//! shared with is read on demand, for those items only.
//!
//! Graph reports the facet on an item that inherits its sharing from a
//! folder too, so a shared folder's contents are marked as well, and only
//! the item's permissions tell whether it's shared on its own (a permission
//! without `inheritedFrom`). The audit therefore reads every marked item, a
//! bounded page at a time ([`Ops::shared_page`], keyset on the ID), and
//! drops the marks of items with no permission of their own: the cost of
//! telling them apart is paid once per item, and again only when it
//! changes (and is marked again).

use crate::{ActivityEntry, ActivityOutcome, ITEM_COLS, Ops, RemoteRow, Result, fix_kind};

/// v21: the shared marks, and the sharing changes asked of OneDrive whose
/// outcome isn't recorded yet (a write-ahead intent per change, keyed by
/// its activity entry). Each mark carries a revision (`rev`, random), new
/// every time it's written, so the audit removes only a mark it read
/// ([`Ops::unmark_shared_read`]), never one the mirror wrote since. New
/// tables only: it applies the same on a v19 or a v20 database.
pub(crate) const MIGRATE_V21: &str = "
CREATE TABLE remote_shared (
    id  TEXT PRIMARY KEY,
    at  INTEGER,
    rev INTEGER NOT NULL
) WITHOUT ROWID;

CREATE TABLE sharing_intent (
    seq        INTEGER PRIMARY KEY,
    item       TEXT NOT NULL,
    permission TEXT NOT NULL,
    expires    INTEGER,
    detail     TEXT NOT NULL,
    at_ms      INTEGER NOT NULL
);
";

/// Takes [`MIGRATE_V21`] back out, for tests that make an older database.
#[cfg(test)]
pub(crate) const UNDO_V21: &str = "DROP TABLE remote_shared; DROP TABLE sharing_intent;";

/// A shared item as the mirror has it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SharedRow {
    pub item: RemoteRow,
    /// When it was shared (Unix milliseconds), if OneDrive said.
    pub at_ms: Option<i64>,
    /// The mark's revision when read ([`Ops::unmark_shared_read`]).
    pub rev: i64,
}

/// One page of marks ([`Ops::shared_page`]).
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct MarkPage {
    /// Marked items the mirror holds: the audit's candidates.
    pub rows: Vec<SharedRow>,
    /// Marks for items the mirror holds neither as a row nor as an orphan
    /// waiting for its folder: nothing will ever list them. With each
    /// one's revision ([`Ops::unmark_shared_read`]).
    pub unheld: Vec<(String, i64)>,
    /// The last mark read: where the next page starts.
    pub last: Option<String>,
    /// How many marks were read (listed, not held, or waiting as orphans).
    pub read: usize,
}

/// A sharing change asked of OneDrive ([`Ops::intent_begin`]).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum IntendedChange {
    Stop,
    /// The permission's expiry set to this time (Unix milliseconds).
    Expire(i64),
}

/// A sharing change whose outcome isn't recorded yet.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Intent {
    /// Its activity entry.
    pub seq: i64,
    pub item: String,
    pub permission: String,
    pub change: IntendedChange,
    /// What the entry says about the permission (never who, nor the link).
    pub detail: String,
    /// When it was asked (Unix milliseconds).
    pub at_ms: i64,
}

impl Ops<'_> {
    /// Records that item `id` is shared (since `at_ms`, if known).
    pub fn mark_shared(&self, id: &str, at_ms: Option<i64>) -> Result<()> {
        self.c
            .prepare_cached(
                "INSERT INTO remote_shared(id, at, rev) VALUES (?1, ?2, random())
                 ON CONFLICT(id) DO UPDATE SET at = excluded.at, rev = excluded.rev",
            )?
            .execute(rusqlite::params![id, at_ms])?;
        Ok(())
    }

    /// [`Ops::mark_shared`], only if the mirror holds the item (stored, or
    /// waiting for its folder): for marks from outside the mirror's own
    /// listing (`oneup shared --scan`). Returns whether it marked it.
    pub fn mark_shared_if_held(&self, id: &str, at_ms: Option<i64>) -> Result<bool> {
        let n = self
            .c
            .prepare_cached(
                "INSERT INTO remote_shared(id, at, rev) SELECT ?1, ?2, random()
                 WHERE EXISTS (SELECT 1 FROM remote_item WHERE id = ?1)
                    OR EXISTS (SELECT 1 FROM remote_orphan WHERE id = ?1)
                 ON CONFLICT(id) DO UPDATE SET at = excluded.at, rev = excluded.rev",
            )?
            .execute(rusqlite::params![id, at_ms])?;
        Ok(n > 0)
    }

    /// Records that item `id` isn't shared (one lookup in a small table:
    /// it runs for every entry the mirror stores).
    pub fn unmark_shared(&self, id: &str) -> Result<()> {
        self.c.prepare_cached("DELETE FROM remote_shared WHERE id = ?1")?.execute([id])?;
        Ok(())
    }

    /// Removes item `id`'s mark only if it's still the one read (revision
    /// `rev`): a mark the mirror wrote meanwhile (the item shared again)
    /// stays. Returns whether it removed it.
    pub fn unmark_shared_read(&self, id: &str, rev: i64) -> Result<bool> {
        let n = self
            .c
            .prepare_cached("DELETE FROM remote_shared WHERE id = ?1 AND rev = ?2")?
            .execute(rusqlite::params![id, rev])?;
        Ok(n > 0)
    }

    /// Whether item `id` is marked shared.
    pub fn is_marked_shared(&self, id: &str) -> Result<bool> {
        Ok(self.c.prepare_cached("SELECT 1 FROM remote_shared WHERE id = ?1")?.exists([id])?)
    }

    /// Forgets the shared marks of the items in `t_sub` (a subtree being
    /// removed from the mirror).
    pub(crate) fn unmark_shared_subtree(&self) -> Result<()> {
        self.c.execute("DELETE FROM remote_shared WHERE id IN (SELECT id FROM t_sub)", [])?;
        Ok(())
    }

    /// How many items are marked shared (contents of shared folders not
    /// checked yet included). Counts the marks' own table only.
    pub fn shared_count(&self) -> Result<u64> {
        let n: i64 = self.c.query_row("SELECT count(*) FROM remote_shared", [], |r| r.get(0))?;
        Ok(u64::try_from(n).unwrap_or(0))
    }

    /// Up to `limit` marks after `after` (`None`: from the start), by ID:
    /// one page of the audit's candidates, read along the marks' primary
    /// key however many there are, with the marks nothing will list (for
    /// the caller to drop). The drive's root is never a candidate
    /// (OneDrive Personal doesn't share it).
    pub fn shared_page(&self, after: Option<&str>, limit: usize) -> Result<MarkPage> {
        let cols = ITEM_COLS.split(", ").map(|c| format!("r.{c}")).collect::<Vec<_>>().join(", ");
        let mut st = self.c.prepare_cached(&format!(
            "SELECT s.id AS mark, s.at AS at, s.rev AS rev,
                    EXISTS (SELECT 1 FROM remote_orphan o WHERE o.id = s.id) AS waiting, {cols}
             FROM remote_shared s LEFT JOIN remote_item r ON r.id = s.id
             WHERE s.id > ?1 ORDER BY s.id LIMIT ?2"
        ))?;
        let limit = i64::try_from(limit).unwrap_or(i64::MAX);
        let mut out = MarkPage::default();
        let mut rows = st.query(rusqlite::params![after.unwrap_or(""), limit])?;
        while let Some(r) = rows.next()? {
            let mark: String = r.get("mark")?;
            let held: Option<String> = r.get("id")?;
            if held.is_some() {
                let row = fix_kind(RemoteRow::from_row(r)?)?;
                if row.kind != crate::Kind::Root {
                    out.rows.push(SharedRow { item: row, at_ms: r.get("at")?, rev: r.get("rev")? });
                }
            } else if !r.get::<_, bool>("waiting")? {
                out.unheld.push((mark.clone(), r.get("rev")?));
            }
            out.last = Some(mark);
            out.read += 1;
        }
        Ok(out)
    }

    /// Records, before OneDrive is asked, a sharing change and its activity
    /// entry (outcome *waiting*); returns the entry's number. If the outcome
    /// is never recorded ([`Ops::intent_settle`]), the change stays in
    /// [`Ops::intents`], to be read back from OneDrive later. Only in a
    /// durable transaction ([`crate::Store::write_durable`]): the record
    /// must survive a power cut that follows the request.
    pub fn intent_begin(
        &self,
        entry: &ActivityEntry,
        item: &str,
        permission: &str,
        change: IntendedChange,
        detail: &str,
    ) -> Result<i64> {
        self.require_durable()?;
        let seq = self.activity_begin(entry)?;
        let expires = match change {
            IntendedChange::Stop => None,
            IntendedChange::Expire(at) => Some(at),
        };
        self.c
            .prepare_cached(
                "INSERT INTO sharing_intent(seq, item, permission, expires, detail, at_ms)
                 VALUES (?1, ?2, ?3, ?4, ?5, ?6)",
            )?
            .execute(rusqlite::params![seq, item, permission, expires, detail, entry.at_ms])?;
        Ok(seq)
    }

    /// The sharing changes whose outcome isn't recorded, oldest first (a
    /// handful at most: one per change interrupted).
    pub fn intents(&self) -> Result<Vec<Intent>> {
        let mut st = self.c.prepare_cached(
            "SELECT seq, item, permission, expires, detail, at_ms FROM sharing_intent ORDER BY seq LIMIT 1000",
        )?;
        let rows = st.query_map([], |r| {
            let expires: Option<i64> = r.get(3)?;
            Ok(Intent {
                seq: r.get(0)?,
                item: r.get(1)?,
                permission: r.get(2)?,
                change: expires.map_or(IntendedChange::Stop, IntendedChange::Expire),
                detail: r.get(4)?,
                at_ms: r.get(5)?,
            })
        })?;
        Ok(rows.collect::<rusqlite::Result<_>>()?)
    }

    /// Records how a change [`Ops::intent_begin`] began ended: its activity
    /// entry settled, the intent gone. Only if exactly that entry is there
    /// (trimming keeps it, see `activity_add`): otherwise nothing changes,
    /// the intent stays, and this returns `false` for the caller to report.
    pub fn intent_settle(&self, seq: i64, outcome: ActivityOutcome, detail: &str) -> Result<bool> {
        if !self.activity_settle(seq, outcome, detail)? {
            return Ok(false);
        }
        self.c.prepare_cached("DELETE FROM sharing_intent WHERE seq = ?1")?.execute([seq])?;
        Ok(true)
    }
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used)]
    use crate::{Kind, RemoteRow, Store};

    fn row(id: &str, parent: Option<&str>, kind: Kind) -> RemoteRow {
        RemoteRow {
            id: id.into(),
            parent_id: parent.map(str::to_string),
            name: id.to_lowercase(),
            kind,
            size: 0,
            etag: None,
            qxh: None,
            mtime_ms: None,
        }
    }

    fn all(s: &Store) -> Vec<String> {
        let mut out = Vec::new();
        let mut after: Option<String> = None;
        loop {
            let page = s.read(|o| o.shared_page(after.as_deref(), 2)).unwrap();
            let Some(last) = page.last else { return out };
            after = Some(last);
            out.extend(page.rows.into_iter().map(|r| r.item.id));
        }
    }

    #[test]
    fn every_marked_item_is_a_candidate_one_page_at_a_time() {
        let mut s = Store::open_in_memory().unwrap();
        s.write(|o| {
            o.upsert_item(&row("R", None, Kind::Root), false)?;
            o.upsert_item(&row("A", Some("R"), Kind::Folder), false)?;
            o.upsert_item(&row("A1", Some("A"), Kind::File), false)?;
            o.upsert_item(&row("A2", Some("A"), Kind::File), false)?;
            o.upsert_item(&row("B", Some("R"), Kind::File), false)?;
            o.upsert_item(&row("C", Some("R"), Kind::File), false)?;
            // A shared folder and what inherits it, with the same time, one
            // with none: whether they're shared on their own is for their
            // permissions to say, so all are candidates.
            o.mark_shared("A", Some(100))?;
            o.mark_shared("A1", Some(100))?;
            o.mark_shared("A2", None)?;
            o.mark_shared("B", None)
        })
        .unwrap();
        assert_eq!(all(&s), ["A", "A1", "A2", "B"]);
        assert!(s.read(|o| o.is_marked_shared("A1")).unwrap());
        // Deleted from OneDrive or pruned: the marks go with the items.
        s.write(|o| o.delete_subtree("A").map(|_| ())).unwrap();
        assert_eq!(all(&s), ["B"]);
        s.write(|o| o.prune_subtree("B", true).map(|_| ())).unwrap();
        assert!(all(&s).is_empty());
        s.write(|o| {
            o.mark_shared("C", Some(5))?;
            o.clear_remote()
        })
        .unwrap();
        assert_eq!(s.read(|o| o.shared_count()).unwrap(), 0);
    }

    /// Marks from outside the mirror's listing are kept only for items it
    /// holds; a mark nothing will list is reported, for the audit to drop,
    /// but an orphan's waits for its folder.
    #[test]
    fn marks_are_for_items_the_mirror_holds() {
        let mut s = Store::open_in_memory().unwrap();
        s.write(|o| {
            o.upsert_item(&row("R", None, Kind::Root), false)?;
            o.upsert_item(&row("A", Some("R"), Kind::File), false)?;
            o.put_orphan(&row("O", Some("LATER"), Kind::File), true)
        })
        .unwrap();
        let marked = s
            .write(|o| {
                Ok((
                    o.mark_shared_if_held("A", None)?,
                    o.mark_shared_if_held("O", None)?,
                    o.mark_shared_if_held("X", None)?,
                ))
            })
            .unwrap();
        assert_eq!(marked, (true, true, false), "an item the mirror doesn't hold isn't marked");
        assert!(!s.read(|o| o.is_marked_shared("X")).unwrap());
        // A mark left without its item (an older build's): reported.
        s.write(|o| o.mark_shared("Z", None)).unwrap();
        let page = s.read(|o| o.shared_page(None, 10)).unwrap();
        assert_eq!(page.rows.iter().map(|r| r.item.id.as_str()).collect::<Vec<_>>(), ["A"]);
        assert_eq!(page.unheld.iter().map(|(id, _)| id.as_str()).collect::<Vec<_>>(), ["Z"]);
        assert_eq!(page.last.as_deref(), Some("Z"));
        // The limit counts every mark read, listed or not.
        let first = s.read(|o| o.shared_page(None, 2)).unwrap();
        assert_eq!((first.rows.len(), first.unheld.len(), first.last.as_deref(), first.read), (1, 0, Some("O"), 2));
    }

    #[test]
    fn dropped_orphans_take_their_marks_with_them() {
        let mut s = Store::open_in_memory().unwrap();
        s.write(|o| {
            o.put_orphan(&row("O", Some("GONE"), Kind::File), true)?;
            o.mark_shared("O", None)?;
            o.drop_orphans_of("GONE").map(|_| ())
        })
        .unwrap();
        assert!(!s.read(|o| o.is_marked_shared("O")).unwrap());
    }

    /// A change is on record, with what was asked, before OneDrive is
    /// asked; settling it records the outcome and forgets the intent.
    #[test]
    fn an_intent_stays_until_its_outcome_is_recorded() {
        use crate::{ActivityEntry, ActivityOutcome, IntendedChange};
        let mut s = Store::open_in_memory().unwrap();
        let entry = |outcome| ActivityEntry {
            seq: 0,
            at_ms: 1,
            outcome,
            action: "set expiry".into(),
            path: "/a".into(),
            detail: "Specific people, can view; asked OneDrive".into(),
        };
        let seq = s
            .write_durable(|o| {
                o.intent_begin(
                    &entry(ActivityOutcome::Waiting),
                    "I!1",
                    "p",
                    IntendedChange::Expire(9_000),
                    "Specific people, can view",
                )
            })
            .unwrap();
        let pending = s.read(|o| o.intents()).unwrap();
        assert_eq!(pending.len(), 1);
        assert_eq!(
            (pending[0].seq, pending[0].item.as_str(), pending[0].change),
            (seq, "I!1", IntendedChange::Expire(9_000))
        );
        assert_eq!(s.read(|o| o.activity_recent(None, 1)).unwrap()[0].outcome, ActivityOutcome::Waiting);
        assert!(s.write(|o| o.intent_settle(seq, ActivityOutcome::Done, "Specific people, can view")).unwrap());
        assert!(s.read(|o| o.intents()).unwrap().is_empty());
        assert_eq!(s.read(|o| o.activity_recent(None, 1)).unwrap()[0].outcome, ActivityOutcome::Done);
    }

    /// The activity log's trimming keeps an entry a pending change will
    /// settle; an entry gone anyway leaves the intent, reported.
    #[test]
    fn a_pending_changes_entry_outlives_trimming() {
        use crate::activity::KEEP;
        use crate::{ActivityEntry, ActivityOutcome, IntendedChange};
        let mut s = Store::open_in_memory().unwrap();
        let entry = |at_ms, outcome| ActivityEntry {
            seq: 0,
            at_ms,
            outcome,
            action: "stop sharing".into(),
            path: "/a".into(),
            detail: String::new(),
        };
        let seq = s
            .write_durable(|o| o.intent_begin(&entry(0, ActivityOutcome::Waiting), "I", "p", IntendedChange::Stop, ""))
            .unwrap();
        let many: Vec<ActivityEntry> = (1..=KEEP + 10).map(|i| entry(i, ActivityOutcome::Done)).collect();
        s.write(|o| o.activity_add(&many)).unwrap();
        let n: i64 = s
            .read(|o| Ok(o.c.query_row("SELECT count(*) FROM activity WHERE seq = ?1", [seq], |r| r.get(0))?))
            .unwrap();
        assert_eq!(n, 1, "kept while its change is pending");
        assert!(s.write(|o| o.intent_settle(seq, ActivityOutcome::Done, "")).unwrap());
        // Settled: trimmed like any other entry from then on.
        s.write(|o| o.activity_add(&[entry(KEEP + 11, ActivityOutcome::Done)])).unwrap();
        let n: i64 = s
            .read(|o| Ok(o.c.query_row("SELECT count(*) FROM activity WHERE seq = ?1", [seq], |r| r.get(0))?))
            .unwrap();
        assert_eq!(n, 0);

        // An entry gone (removed by hand, an older build): nothing settled,
        // the intent stays.
        let seq = s
            .write_durable(|o| o.intent_begin(&entry(1, ActivityOutcome::Waiting), "I", "p", IntendedChange::Stop, ""))
            .unwrap();
        s.write(|o| Ok(o.c.execute("DELETE FROM activity WHERE seq = ?1", [seq]).map(|_| ())?)).unwrap();
        assert!(!s.write(|o| o.intent_settle(seq, ActivityOutcome::Done, "")).unwrap());
        assert_eq!(s.read(|o| o.intents()).unwrap().len(), 1);
    }

    /// A store at version `from` (made by undoing `undo`) upgrades to the
    /// latest (v21 and the versions after it)
    /// with its tree as it was, nothing marked and nothing pending, and
    /// marks and intents work at once.
    fn upgrades_from(from: i64, undo: &str) {
        use crate::{ActivityEntry, ActivityOutcome, IntendedChange};
        let dir = std::env::temp_dir().join(format!("oneup-store-shared-up{from}-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("state.db");
        {
            let mut s = Store::open(&path, 1024).unwrap();
            s.write(|o| {
                o.upsert_item(&row("R", None, Kind::Root), false)?;
                o.upsert_item(&row("A", Some("R"), Kind::File), false)
            })
            .unwrap();
            s.read(|o| Ok(o.c.execute_batch(&format!("{undo} PRAGMA user_version = {from};"))?)).unwrap();
        }
        let mut s = Store::open(&path, 1024).unwrap();
        let v: i64 = s.read(|o| Ok(o.c.query_row("PRAGMA user_version", [], |r| r.get(0))?)).unwrap();
        assert_eq!(v, crate::SCHEMA_VERSION);
        assert!(s.read(|o| o.get_item("A")).unwrap().is_some(), "the tree is kept");
        assert_eq!(s.read(|o| o.shared_count()).unwrap(), 0);
        assert!(s.read(|o| o.intents()).unwrap().is_empty());
        s.write(|o| o.mark_shared("A", None)).unwrap();
        assert_eq!(s.read(|o| o.shared_page(None, 10)).unwrap().rows.len(), 1);
        let e = ActivityEntry {
            seq: 0,
            at_ms: 1,
            outcome: ActivityOutcome::Waiting,
            action: "stop sharing".into(),
            path: "/a".into(),
            detail: String::new(),
        };
        s.write_durable(|o| o.intent_begin(&e, "A", "p", IntendedChange::Stop, "")).unwrap();
        assert_eq!(s.read(|o| o.intents()).unwrap().len(), 1);
        drop(s);
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn a_v19_store_upgrades() {
        upgrades_from(
            19,
            &format!(
                "{} {} {} {}",
                crate::history::UNDO_V23,
                crate::pins::UNDO_V22,
                super::UNDO_V21,
                crate::dupes::UNDO_V20
            ),
        );
    }

    #[test]
    fn a_v20_store_upgrades() {
        upgrades_from(20, &format!("{} {} {}", crate::history::UNDO_V23, crate::pins::UNDO_V22, super::UNDO_V21));
    }

    /// A mark written again after it was read (the item shared again)
    /// isn't removed by the audit's late decision about the old one.
    #[test]
    fn only_the_mark_read_is_removed() {
        let mut s = Store::open_in_memory().unwrap();
        s.write(|o| {
            o.upsert_item(&row("R", None, Kind::Root), false)?;
            o.upsert_item(&row("A", Some("R"), Kind::File), false)?;
            o.mark_shared("A", Some(1))
        })
        .unwrap();
        let read = s.read(|o| o.shared_page(None, 10)).unwrap().rows.remove(0);
        // The mirror marks it again meanwhile.
        s.write(|o| o.mark_shared("A", Some(2))).unwrap();
        assert!(!s.write(|o| o.unmark_shared_read("A", read.rev)).unwrap());
        assert!(s.read(|o| o.is_marked_shared("A")).unwrap(), "the newer mark stays");
        let again = s.read(|o| o.shared_page(None, 10)).unwrap().rows.remove(0);
        assert!(s.write(|o| o.unmark_shared_read("A", again.rev)).unwrap());
        assert!(!s.read(|o| o.is_marked_shared("A")).unwrap());
    }

    /// An intent is written only in a durable transaction.
    #[test]
    fn an_intent_needs_a_durable_commit() {
        use crate::{ActivityEntry, ActivityOutcome, Error, IntendedChange};
        let mut s = Store::open_in_memory().unwrap();
        let e = ActivityEntry {
            seq: 0,
            at_ms: 1,
            outcome: ActivityOutcome::Waiting,
            action: "stop sharing".into(),
            path: "/a".into(),
            detail: String::new(),
        };
        let r = s.write(|o| o.intent_begin(&e, "A", "p", IntendedChange::Stop, ""));
        assert!(matches!(r, Err(Error::NotDurable)), "{r:?}");
        assert!(s.read(|o| o.intents()).unwrap().is_empty());
        let before = s.read(|o| o.durable_commits()).unwrap();
        s.write_durable(|o| o.intent_begin(&e, "A", "p", IntendedChange::Stop, "")).unwrap();
        assert_eq!(s.read(|o| o.durable_commits()).unwrap(), before + 1);
    }

    /// A page is read along the marks' key, whatever the size of the tree.
    #[cfg(feature = "statement-log")]
    #[test]
    fn a_page_reads_no_whole_table() {
        let mut s = Store::open_in_memory().unwrap();
        s.write(|o| o.upsert_item(&row("R", None, Kind::Root), false)).unwrap();
        s.log_statements();
        Store::take_statements();
        s.read(|o| o.shared_page(Some("A"), 10)).unwrap();
        s.write(|o| o.mark_shared_if_held("A", None)).unwrap();
        let scans = s.tree_scans(&Store::take_statements()).unwrap();
        assert!(scans.is_empty(), "{scans:?}");
    }
}
