//! Dirty folders: which Synced folders need planning again (schema v5).
//!
//! The reconciler decides one folder's *children* at a time. A full pass
//! plans every Synced folder, which costs time proportional to the whole
//! tree; the background agent instead plans only folders whose contents may
//! have changed since it last looked (`AGENTS.md`: cost scales with changes).
//!
//! **Why triggers.** Every writer of the three trees (scanner, change-journal
//! observer, mirror, executor) would otherwise have to report what it
//! touched, and one forgotten call would leave a change unplanned until the
//! next full pass. SQLite triggers on `local_item`, `remote_item`,
//! `synced_item` and `local_hash` see every write, whoever makes it, inside
//! the same transaction. A missed mark would mean "change not synced yet",
//! never data loss (the reconciler and executor re-check everything), but
//! it's still a bug the triggers rule out by construction.
//!
//! **What is marked.** For any row that appears, disappears or changes:
//! its parent before and after (the folders whose children changed), and
//! the row itself if it is a folder (its contents may now need planning:
//! a folder that just entered a tree, or was just linked in the Synced
//! tree). Marks are IDs of the tree that changed (`side`); [`Ops::dirty_scope`]
//! maps them to Synced folders. A mark that maps to no Synced folder (say,
//! a new local folder) is dropped: its parent was marked too, planning the
//! parent creates the folder on the other side, and *that* write marks the
//! new Synced row, whose contents are then planned in the next round.
//!
//! Updates that change nothing (the observer's echo of the executor's own
//! write, a delta entry repeated) mark nothing, so a quiet folder stays
//! quiet. Over-marking is harmless (a folder is planned and nothing comes
//! of it); under-marking isn't, so when in doubt a trigger marks.
//!
//! Marking only happens once a Synced tree exists. Before the first sync
//! there is nothing to plan incrementally, and a first scan of millions of
//! items shouldn't pay for marks nobody reads.
//!
//! **Clearing.** Each mark has a sequence number (`AUTOINCREMENT`, so never
//! reused). A round reads the marks up to some number, plans, executes, and
//! then clears only up to that number: anything marked meanwhile, including
//! the executor's own writes, survives for the next round. Re-marking a
//! folder gives it a new number, so a folder marked again during a round is
//! never cleared by that round.

use oneup_sync::{LocalId, RemoteId, Sid};
use rusqlite::OptionalExtension;

use crate::{Ops, Result};

/// The `side` codes in the `dirty` table.
const LOCAL: i64 = 0;
const REMOTE: i64 = 1;
const SYNCED: i64 = 2;

/// One mark inside a trigger body: the folder `id` (an SQL expression over
/// `NEW`/`OLD`) of tree `side`, when `cond` holds.
///
/// It is a delete followed by an insert rather than `INSERT OR REPLACE`,
/// because SQLite lets the *outer* statement's conflict handling override
/// a trigger's, and the stores' upserts (`ON CONFLICT DO UPDATE`) would turn
/// the replace into a constraint error. Deleting first also gives a
/// re-marked folder a new, higher sequence number (see the module docs).
fn mark(side: i64, id: &str, cond: &str) -> String {
    format!(
        "DELETE FROM dirty WHERE side = {side} AND id = {id} AND ({cond});
         INSERT INTO dirty(side, id) SELECT {side}, {id} WHERE {id} IS NOT NULL AND ({cond});\n"
    )
}

/// A trigger `name` on `event` of `table` (`WHEN` clause `when`), whose body
/// is the given marks.
fn trigger(name: &str, event: &str, table: &str, when: &str, marks: &[String]) -> String {
    format!("CREATE TRIGGER {name} AFTER {event} ON {table} WHEN {when}\nBEGIN\n{}END;\n", marks.concat())
}

/// v12 -> v13: a Synced row removed also marks its grandparent folder. A
/// folder is deleted on the other side only once it's empty, and that is
/// decided when *its* parent is planned; removing its last child marked
/// only the folder itself, so the deletion waited for the next full plan
/// (an agent restart): a folder tree deleted here left its empty folders
/// in OneDrive, one level going per restart (found by the `everyday_`
/// tests).
pub(crate) fn migrate_v13() -> String {
    let grandparent = "(SELECT parent_sid FROM synced_item WHERE sid = OLD.parent_sid)";
    trigger("dirty_synced_del_up", "DELETE", "synced_item", "1", &[mark(SYNCED, grandparent, SYNCING)])
}

/// Marking only starts once a Synced tree exists (see the module docs).
const SYNCING: &str = "EXISTS (SELECT 1 FROM synced_item)";

/// v4 -> v5: the `dirty` table and the triggers that fill it.
///
/// Kind codes: folders are 1 in all three trees, and 2 is the root in the
/// Local and Remote trees (the Synced root is an ordinary folder row).
/// `IS NOT` compares NULLs as values, so "changed" includes "became NULL".
pub(crate) fn migrate_v5() -> String {
    let folder = |row: &str| format!("{row}.kind IN (1, 2)");
    let always = "1";
    // Any column that the reconciler reads (or that changes what it reads).
    let local_changed = "OLD.parent_id IS NOT NEW.parent_id OR OLD.name IS NOT NEW.name
        OR OLD.name_lossy IS NOT NEW.name_lossy OR OLD.kind IS NOT NEW.kind OR OLD.size IS NOT NEW.size
        OR OLD.mtime IS NOT NEW.mtime OR OLD.attributes IS NOT NEW.attributes
        OR OLD.reparse_tag IS NOT NEW.reparse_tag OR OLD.skip IS NOT NEW.skip";
    let remote_changed = "OLD.parent_id IS NOT NEW.parent_id OR OLD.name IS NOT NEW.name OR OLD.kind IS NOT NEW.kind
        OR OLD.size IS NOT NEW.size OR OLD.etag IS NOT NEW.etag OR OLD.qxh IS NOT NEW.qxh
        OR OLD.mtime IS NOT NEW.mtime OR OLD.is_excluded IS NOT NEW.is_excluded";
    let synced_changed = "OLD.parent_sid IS NOT NEW.parent_sid OR OLD.name IS NOT NEW.name OR OLD.kind IS NOT NEW.kind
        OR OLD.size IS NOT NEW.size OR OLD.qxh IS NOT NEW.qxh OR OLD.local_id IS NOT NEW.local_id
        OR OLD.local_mtime IS NOT NEW.local_mtime OR OLD.remote_id IS NOT NEW.remote_id
        OR OLD.remote_etag IS NOT NEW.remote_etag";
    let hash_changed = "OLD.size IS NOT NEW.size OR OLD.mtime IS NOT NEW.mtime OR OLD.qxh IS NOT NEW.qxh";
    // A new hash lets the reconciler compare contents it couldn't before:
    // the hashed file's folder.
    let hashed_parent = "(SELECT parent_id FROM local_item WHERE file_id = NEW.file_id)";

    let mut sql = String::from(
        "CREATE TABLE dirty (
            seq  INTEGER PRIMARY KEY AUTOINCREMENT,
            side INTEGER NOT NULL,
            id   NOT NULL,
            UNIQUE (side, id)
        );\n",
    );
    for (table, id, parent, changed, side) in [
        ("local_item", "file_id", "parent_id", local_changed, LOCAL),
        ("remote_item", "id", "parent_id", remote_changed, REMOTE),
    ] {
        let short = &table[..table.len() - 5];
        sql += &trigger(
            &format!("dirty_{short}_ins"),
            "INSERT",
            table,
            SYNCING,
            &[mark(side, &format!("NEW.{parent}"), always), mark(side, &format!("NEW.{id}"), &folder("NEW"))],
        );
        sql += &trigger(
            &format!("dirty_{short}_upd"),
            "UPDATE",
            table,
            &format!("{SYNCING} AND ({changed})"),
            &[
                mark(side, &format!("OLD.{parent}"), always),
                mark(side, &format!("NEW.{parent}"), always),
                mark(side, &format!("NEW.{id}"), &folder("NEW")),
            ],
        );
        sql += &trigger(
            &format!("dirty_{short}_del"),
            "DELETE",
            table,
            SYNCING,
            &[mark(side, &format!("OLD.{parent}"), always), mark(side, &format!("OLD.{id}"), &folder("OLD"))],
        );
    }
    // The Synced tree marks unconditionally: its first row is what starts
    // marking. Its folders are kind 1 (the root included).
    sql += &trigger(
        "dirty_synced_ins",
        "INSERT",
        "synced_item",
        always,
        &[mark(SYNCED, "NEW.parent_sid", always), mark(SYNCED, "NEW.sid", "NEW.kind = 1")],
    );
    sql += &trigger(
        "dirty_synced_upd",
        "UPDATE",
        "synced_item",
        synced_changed,
        &[
            mark(SYNCED, "OLD.parent_sid", always),
            mark(SYNCED, "NEW.parent_sid", always),
            mark(SYNCED, "NEW.sid", "NEW.kind = 1"),
        ],
    );
    sql += &trigger("dirty_synced_del", "DELETE", "synced_item", always, &[mark(SYNCED, "OLD.parent_sid", always)]);
    sql += &trigger("dirty_hash_ins", "INSERT", "local_hash", SYNCING, &[mark(LOCAL, hashed_parent, always)]);
    sql += &trigger(
        "dirty_hash_upd",
        "UPDATE",
        "local_hash",
        &format!("{SYNCING} AND ({hash_changed})"),
        &[mark(LOCAL, hashed_parent, always)],
    );
    sql
}

/// The folders to plan in one round, and how far to clear afterwards.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct DirtyScope {
    /// Synced folders whose children may have changed, ascending (sids
    /// grow over time, so parents mostly come before their children).
    pub folders: Vec<Sid>,
    /// The highest mark read; pass it to [`Ops::dirty_clear`] once the
    /// round's outcome makes the marks unnecessary.
    pub upto: i64,
}

impl Ops<'_> {
    /// Maps the current marks to Synced folders (see the module docs).
    /// Reads only; nothing is cleared.
    ///
    /// Cost: one indexed lookup per mark. A mark whose ID no Synced folder
    /// links is skipped (see the module docs for why that's complete).
    pub fn dirty_scope(&self) -> Result<DirtyScope> {
        let upto: i64 = self.c.query_row("SELECT coalesce(max(seq), 0) FROM dirty", [], |r| r.get(0))?;
        let mut st = self.c.prepare_cached(
            "SELECT s.sid FROM dirty d JOIN synced_item s ON s.local_id = d.id
                 WHERE d.side = ?1 AND d.seq <= ?4 AND s.kind = 1
             UNION
             SELECT s.sid FROM dirty d JOIN synced_item s ON s.remote_id = d.id
                 WHERE d.side = ?2 AND d.seq <= ?4 AND s.kind = 1
             UNION
             SELECT s.sid FROM dirty d JOIN synced_item s ON s.sid = d.id
                 WHERE d.side = ?3 AND d.seq <= ?4 AND s.kind = 1
             ORDER BY 1",
        )?;
        let folders = st
            .query_map([LOCAL, REMOTE, SYNCED, upto], |r| r.get::<_, i64>(0))?
            .map(|r| r.map(|v| v as Sid))
            .collect::<rusqlite::Result<Vec<_>>>()?;
        Ok(DirtyScope { folders, upto })
    }

    /// Drops the marks up to `upto` (from [`Ops::dirty_scope`]). Marks made
    /// after that read have higher numbers and stay.
    pub fn dirty_clear(&self, upto: i64) -> Result<usize> {
        Ok(self.c.prepare_cached("DELETE FROM dirty WHERE seq <= ?1")?.execute([upto])?)
    }

    /// Drops the marks up to `upto` like [`Ops::dirty_clear`], but marks
    /// again (with new numbers) the folders of what still needs a retry:
    /// the folders holding the given items, and the given items themselves
    /// where they're folders. For a round in which some operations were
    /// skipped (a file open elsewhere): keeping *every* mark for their sake
    /// made each later round plan the whole tree again (measured on a
    /// 300k-file drive: about 10 minutes per round, for 7
    /// skipped files). Marking more than needed only plans a folder for
    /// nothing.
    pub fn dirty_clear_except(&self, upto: i64, local: &[LocalId], remote: &[RemoteId], synced: &[Sid]) -> Result<()> {
        self.dirty_clear(upto)?;
        let mut mark = self.c.prepare_cached(
            "INSERT INTO dirty(side, id) SELECT ?1, ?2 WHERE ?2 IS NOT NULL
             ON CONFLICT(side, id) DO NOTHING",
        )?;
        for l in local {
            let id = crate::local::blob(l.0);
            let parent: Option<Vec<u8>> = self
                .c
                .prepare_cached("SELECT parent_id FROM local_item WHERE file_id = ?1")?
                .query_row([&id[..]], |r| r.get(0))
                .optional()?
                .flatten();
            mark.execute(rusqlite::params![LOCAL, &id[..]])?;
            mark.execute(rusqlite::params![LOCAL, parent])?;
        }
        for r in remote {
            let parent: Option<String> = self
                .c
                .prepare_cached("SELECT parent_id FROM remote_item WHERE id = ?1")?
                .query_row([&r.0], |row| row.get(0))
                .optional()?
                .flatten();
            mark.execute(rusqlite::params![REMOTE, &r.0])?;
            mark.execute(rusqlite::params![REMOTE, parent])?;
        }
        for s in synced {
            let parent: Option<i64> = self
                .c
                .prepare_cached("SELECT parent_sid FROM synced_item WHERE sid = ?1")?
                .query_row([*s as i64], |row| row.get(0))
                .optional()?
                .flatten();
            mark.execute(rusqlite::params![SYNCED, *s as i64])?;
            mark.execute(rusqlite::params![SYNCED, parent])?;
        }
        Ok(())
    }

    /// Marks the Synced items `synced` and their folders for planning (new
    /// evidence about them that no tree write recorded).
    pub fn dirty_mark_synced(&self, synced: &[Sid]) -> Result<()> {
        let mut mark = self.c.prepare_cached(
            "INSERT INTO dirty(side, id) SELECT ?1, ?2 WHERE ?2 IS NOT NULL
             ON CONFLICT(side, id) DO NOTHING",
        )?;
        for s in synced {
            let parent: Option<i64> = self
                .c
                .prepare_cached("SELECT parent_sid FROM synced_item WHERE sid = ?1")?
                .query_row([*s as i64], |row| row.get(0))
                .optional()?
                .flatten();
            mark.execute(rusqlite::params![SYNCED, *s as i64])?;
            mark.execute(rusqlite::params![SYNCED, parent])?;
        }
        Ok(())
    }

    /// The highest mark number so far (0 if none). It only grows, so a
    /// higher value later means something was marked in between, even a
    /// folder that was already marked.
    pub fn dirty_last(&self) -> Result<i64> {
        // `sqlite_sequence` keeps AUTOINCREMENT's high-water mark even after
        // the rows are cleared.
        Ok(self.c.query_row("SELECT coalesce((SELECT seq FROM sqlite_sequence WHERE name = 'dirty'), 0)", [], |r| {
            r.get(0)
        })?)
    }

    /// Number of marks (for status displays).
    pub fn dirty_count(&self) -> Result<i64> {
        Ok(self.c.query_row("SELECT count(*) FROM dirty", [], |r| r.get(0))?)
    }
}

#[cfg(test)]
mod tests {
    use oneup_sync::{NodeKind, ROOT};

    use crate::synced::tests::{base, lrow, rrow, synced};
    use crate::{Kind, LocalKind, Store};

    /// The fixture's folder "d": local 3, cloud "d", sid 2.
    const D: u64 = 2;

    /// Reads the scope and clears it, as a round does.
    fn take(s: &mut Store) -> Vec<u64> {
        s.write(|o| {
            let scope = o.dirty_scope()?;
            o.dirty_clear(scope.upto)?;
            Ok(scope.folders)
        })
        .unwrap()
    }

    /// Removing the last item of a folder marks the folder *and* its
    /// parent, which decides whether the now empty folder goes too (v13).
    #[test]
    fn removing_an_item_marks_its_folder_and_the_folder_above() {
        let mut s = Store::open_in_memory().unwrap();
        synced(&mut s);
        // d (sid 2) gets a subfolder e (sid 3) holding a file x (sid 4).
        s.write(|o| {
            o.local_put(&lrow(5, Some(3), "e", LocalKind::Folder))?;
            o.local_put(&lrow(6, Some(5), "x", LocalKind::File))?;
            assert_eq!(o.synced_insert(&base(0, Some(D), "e", NodeKind::Folder, 5, "e"))?, 3);
            assert_eq!(o.synced_insert(&base(0, Some(3), "x", NodeKind::File, 6, "x"))?, 4);
            Ok(())
        })
        .unwrap();
        take(&mut s);
        s.write(|o| o.synced_remove(4).map(drop)).unwrap();
        let mut marked = take(&mut s);
        marked.sort_unstable();
        assert_eq!(marked, [D, 3], "e itself, and d, where e's own fate is decided");
        // At the top, the grandparent is the root.
        s.write(|o| o.synced_remove(3).map(drop)).unwrap();
        let mut marked = take(&mut s);
        marked.sort_unstable();
        assert_eq!(marked, [ROOT, D]);
    }

    #[test]
    fn a_retry_keeps_only_the_folders_of_what_is_retried() {
        use oneup_sync::{LocalId, RemoteId};
        let mut s = Store::open_in_memory().unwrap();
        synced(&mut s);
        s.write(|o| {
            o.local_put(&lrow(4, Some(3), "g", LocalKind::File))?;
            o.upsert_item(&rrow("h", Some("d"), "h", Kind::File), false).map(drop)
        })
        .unwrap();
        let clear_except = |s: &mut Store, local: &[LocalId], remote: &[RemoteId], synced: &[u64]| {
            s.write(|o| {
                let upto = o.dirty_scope()?.upto;
                o.dirty_clear_except(upto, local, remote, synced)
            })
            .unwrap();
            take(s)
        };
        // Everything was marked; a file in d was skipped: only d is left.
        assert_eq!(clear_except(&mut s, &[LocalId(4)], &[], &[]), [D]);
        // A cloud file in d.
        assert_eq!(clear_except(&mut s, &[], &[RemoteId("h".into())], &[]), [D]);
        // A Synced folder: itself and the folder holding it.
        assert_eq!(clear_except(&mut s, &[], &[], &[D]), [ROOT, D]);
        // Nothing to retry: nothing left.
        assert_eq!(clear_except(&mut s, &[], &[], &[]), Vec::<u64>::new());
    }

    #[test]
    fn nothing_is_marked_before_the_first_sync() {
        let mut s = Store::open_in_memory().unwrap();
        s.write(|o| {
            o.local_put(&lrow(1, None, "", LocalKind::Root))?;
            o.local_put(&lrow(2, Some(1), "f", LocalKind::File))?;
            o.upsert_item(&rrow("root", None, "root", Kind::Root), false)?;
            assert_eq!(o.dirty_count()?, 0);
            Ok(())
        })
        .unwrap();
    }

    #[test]
    fn changes_mark_the_folders_whose_children_changed() {
        let mut s = Store::open_in_memory().unwrap();
        synced(&mut s);
        // Building the Synced tree marked the root and the folder in it.
        assert_eq!(take(&mut s), [ROOT, D]);
        assert_eq!(take(&mut s), Vec::<u64>::new());

        // Rewriting rows as they are (an echo, a repeated delta entry) marks nothing.
        s.write(|o| {
            o.local_update(&lrow(2, Some(1), "f", LocalKind::File))?;
            o.upsert_item(&rrow("f", Some("root"), "f", Kind::File), false)?;
            o.synced_put(&base(1, Some(ROOT), "f", NodeKind::File, 2, "f"))
        })
        .unwrap();
        assert_eq!(take(&mut s), Vec::<u64>::new());

        // A new local file in d: d.
        s.write(|o| o.local_put(&lrow(4, Some(3), "g", LocalKind::File)).map(drop)).unwrap();
        assert_eq!(take(&mut s), [D]);
        // A cloud rename at the top: the root.
        s.write(|o| o.upsert_item(&rrow("f", Some("root"), "F", Kind::File), false)).unwrap();
        assert_eq!(take(&mut s), [ROOT]);
        // A local move from the top into d: both.
        s.write(|o| o.local_update(&lrow(2, Some(3), "f", LocalKind::File))).unwrap();
        assert_eq!(take(&mut s), [ROOT, D]);
        // A cloud deletion inside d: d.
        s.write(|o| {
            o.upsert_item(&rrow("h", Some("d"), "h", Kind::File), false)?;
            o.dirty_clear(i64::MAX)?;
            o.delete_subtree("h").map(drop)
        })
        .unwrap();
        assert_eq!(take(&mut s), [D]);
        // A new hash for a file in d lets d's plan compare contents: d.
        s.write(|o| o.local_hash_put(4, 3, 9, &[1; 20])).unwrap();
        assert_eq!(take(&mut s), [D]);
    }

    #[test]
    fn a_new_folder_is_planned_once_it_is_linked() {
        let mut s = Store::open_in_memory().unwrap();
        synced(&mut s);
        take(&mut s);
        // Not synced yet: only its parent can be planned (which creates it
        // on the other side).
        s.write(|o| o.local_put(&lrow(5, Some(1), "n", LocalKind::Folder)).map(drop)).unwrap();
        assert_eq!(take(&mut s), [ROOT]);
        // The executor links it; now its own contents get planned.
        let sid = s.write(|o| o.synced_insert(&base(0, Some(ROOT), "n", NodeKind::Folder, 5, "n"))).unwrap();
        assert_eq!(take(&mut s), [ROOT, sid]);
    }

    #[test]
    fn marks_made_during_a_round_survive_its_clear() {
        let mut s = Store::open_in_memory().unwrap();
        synced(&mut s);
        let scope = s.read(|o| o.dirty_scope()).unwrap();
        assert_eq!(scope.folders, [ROOT, D]);
        // During the round, d is marked again and a file appears at the top.
        s.write(|o| {
            o.local_put(&lrow(4, Some(3), "g", LocalKind::File))?;
            o.local_put(&lrow(6, Some(1), "k", LocalKind::File)).map(drop)
        })
        .unwrap();
        s.write(|o| o.dirty_clear(scope.upto)).unwrap();
        assert_eq!(take(&mut s), [ROOT, D]);
    }

    #[test]
    fn the_last_mark_number_only_grows() {
        let mut s = Store::open_in_memory().unwrap();
        assert_eq!(s.read(|o| o.dirty_last()).unwrap(), 0);
        synced(&mut s);
        let before = s.read(|o| o.dirty_last()).unwrap();
        take(&mut s);
        assert_eq!(s.read(|o| o.dirty_last()).unwrap(), before, "clearing keeps the number");
        // Re-marking an already marked folder still moves it on.
        s.write(|o| o.local_put(&lrow(4, Some(3), "g", LocalKind::File)).map(drop)).unwrap();
        let once = s.read(|o| o.dirty_last()).unwrap();
        s.write(|o| o.local_put(&lrow(5, Some(3), "h", LocalKind::File)).map(drop)).unwrap();
        assert!(once > before && s.read(|o| o.dirty_last()).unwrap() > once);
    }
}
