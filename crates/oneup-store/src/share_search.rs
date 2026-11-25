//! Flat path catalogs for shares that provide notifications but no journal.

use rusqlite::params;

use crate::{Ops, Result, Store};

const SCHEMA: &str = "
CREATE TABLE IF NOT EXISTS share_item (
    id INTEGER PRIMARY KEY, path TEXT UNIQUE NOT NULL, name TEXT NOT NULL, folder INTEGER NOT NULL
);
CREATE TABLE IF NOT EXISTS share_pending (path TEXT PRIMARY KEY) WITHOUT ROWID;
CREATE VIRTUAL TABLE IF NOT EXISTS share_name USING fts5(name, content='', contentless_delete=1, detail=none, tokenize='trigram');
CREATE TRIGGER IF NOT EXISTS share_add AFTER INSERT ON share_item
BEGIN INSERT INTO share_name(rowid,name) VALUES (NEW.id,NEW.name); END;
CREATE TRIGGER IF NOT EXISTS share_remove AFTER DELETE ON share_item
BEGIN DELETE FROM share_name WHERE rowid=OLD.id; END;";

impl Store {
    /// Each watch session starts from a clean snapshot.
    pub fn reset_share_search(&mut self) -> Result<()> {
        self.write(|o| {
            o.c.execute_batch(
                "DROP TABLE IF EXISTS share_item; DROP TABLE IF EXISTS share_pending;
                 DROP TABLE IF EXISTS share_name;",
            )?;
            o.c.execute_batch(SCHEMA)?;
            o.share_enqueue("")?;
            Ok(())
        })
    }
}

impl Ops<'_> {
    pub fn share_put(&self, path: &str, name: &str, folder: bool) -> Result<()> {
        self.c
            .prepare_cached("INSERT OR IGNORE INTO share_item(path,name,folder) VALUES (?1,?2,?3)")?
            .execute(params![path, name, folder])?;
        Ok(())
    }

    /// The range is exact because all stored paths use slash separators.
    pub fn share_remove(&self, path: &str) -> Result<()> {
        let lower = format!("{path}/");
        let upper = format!("{path}0");
        self.c
            .prepare_cached("DELETE FROM share_item WHERE path >= ?1 AND path < ?2")?
            .execute(params![lower, upper])?;
        self.c.prepare_cached("DELETE FROM share_item WHERE path=?1")?.execute([path])?;
        Ok(())
    }

    pub fn share_enqueue(&self, path: &str) -> Result<()> {
        self.c.prepare_cached("INSERT OR IGNORE INTO share_pending VALUES (?1)")?.execute([path])?;
        Ok(())
    }

    pub fn share_next(&self) -> Result<Vec<String>> {
        let mut statement = self.c.prepare_cached("SELECT path FROM share_pending ORDER BY path LIMIT 64")?;
        Ok(statement.query_map([], |r| r.get(0))?.collect::<rusqlite::Result<_>>()?)
    }

    pub fn share_dequeue(&self, path: &str) -> Result<()> {
        self.c.prepare_cached("DELETE FROM share_pending WHERE path=?1")?.execute([path])?;
        Ok(())
    }

    pub fn visit_share_names(
        &self,
        text: &str,
        words: bool,
        folder: Option<bool>,
        mut visit: impl FnMut(&str, &str, bool) -> Result<bool>,
    ) -> Result<bool> {
        let terms = if words { text.split_whitespace().collect::<Vec<_>>() } else { vec![text] };
        let Some(expr) = crate::search::match_terms(terms.iter().copied()) else { return Ok(false) };
        let terms: Vec<_> = terms.into_iter().map(oneup_sync::fold).collect();
        let mut st = self.c.prepare_cached(
            "SELECT i.path,i.name,i.folder FROM share_name n CROSS JOIN share_item i ON i.id=n.rowid
             WHERE share_name MATCH ?1 AND (?2 IS NULL OR i.folder=?2) ORDER BY n.rowid DESC LIMIT 20001",
        )?;
        let mut rows = st.query(params![expr, folder])?;
        let mut seen = 0;
        while let Some(row) = rows.next()? {
            seen += 1;
            if seen > 20_000 {
                return Ok(true);
            }
            let name: String = row.get(1)?;
            let folded = oneup_sync::fold(&name);
            if terms.iter().all(|term| folded.contains(term)) {
                let path: String = row.get(0)?;
                if !visit(&path, &name, row.get(2)?)? {
                    break;
                }
            }
        }
        Ok(false)
    }
}
