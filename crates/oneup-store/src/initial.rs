//! Durable choices for items found on only one side during setup.
//! These are exclusions, never deletion evidence. They survive rescans.

use crate::{Ops, Result};
use rusqlite::{OptionalExtension, params};

pub(crate) const MIGRATE_V26: &str = "
CREATE TABLE IF NOT EXISTS initial_choice (
    key TEXT PRIMARY KEY, path TEXT NOT NULL, folder INTEGER NOT NULL,
    selected INTEGER NOT NULL, active INTEGER NOT NULL DEFAULT 1
) WITHOUT ROWID;
CREATE INDEX IF NOT EXISTS initial_choice_path ON initial_choice(path);
";

#[derive(Debug)]
pub struct InitialChoice {
    pub key: String,
    pub path: String,
    pub folder: bool,
    pub selected: bool,
}

pub fn local_key(id: u128) -> String {
    let hex: String = id.to_le_bytes().iter().map(|b| format!("{b:02x}")).collect();
    format!("l:{hex}")
}
pub fn local_id(key: &str) -> Option<u128> {
    let hex = key.strip_prefix("l:")?;
    (hex.len() == 32).then(|| u128::from_str_radix(hex, 16).ok().map(u128::swap_bytes)).flatten()
}

impl Ops<'_> {
    pub fn initial_begin(&self) -> Result<()> {
        self.require_durable()?;
        self.c.execute("UPDATE initial_choice SET active=0 WHERE substr(key,1,2) IN ('l:','r:')", [])?;
        Ok(())
    }
    pub fn initial_end(&self) -> Result<()> {
        self.require_durable()?;
        self.c.execute("DELETE FROM initial_choice WHERE active=0", [])?;
        Ok(())
    }
    pub fn initial_path(&self, key: &str) -> Result<Option<String>> {
        if let Some(id) = local_id(key) {
            return self.local_path_of(id);
        }
        if let Some(id) = key.strip_prefix("r:") {
            let path = self.path_of(id)?;
            let root = self.meta_get("sync.remote_path")?.unwrap_or_else(|| "/".into());
            return Ok(path.and_then(|p| p.strip_prefix(root.trim_end_matches('/')).map(str::to_owned)));
        }
        Ok(None)
    }
    pub fn initial_clear_missing(&self) -> Result<()> {
        self.c.execute("DELETE FROM initial_choice WHERE key >= 'm:' AND key < 'm;'", [])?;
        Ok(())
    }
    pub fn initial_held(&self, key: &str) -> Result<bool> {
        Ok(self.c.prepare_cached("SELECT 1 FROM initial_choice WHERE key=?1 AND selected=0")?.exists([key])?)
    }
    pub fn initial_key_at(&self, side: &str, path: &str) -> Result<Option<String>> {
        Ok(self
            .c
            .prepare_cached("SELECT key FROM initial_choice WHERE path=?1 AND substr(key,1,2)=?2")?
            .query_row(params![path, format!("{side}:")], |r| r.get(0))
            .optional()?)
    }
    pub fn initial_put(&self, key: &str, path: &str, folder: bool, selected: bool) -> Result<()> {
        if key.starts_with("l:") || key.starts_with("r:") {
            self.require_durable()?;
        }
        self.c
            .prepare_cached(
                "INSERT INTO initial_choice(key,path,folder,selected,active) VALUES (?1, ?2, ?3, ?4,1)
            ON CONFLICT(key) DO UPDATE SET path=excluded.path, folder=excluded.folder,active=1",
            )?
            .execute(params![key, path, folder, selected])?;
        Ok(())
    }

    pub fn initial_page(&self, side: &str, after: &str, limit: usize) -> Result<Vec<InitialChoice>> {
        let start = format!("{side}:");
        let end = format!("{side};");
        let mut q = self.c.prepare_cached(
            "SELECT key,path,folder,selected FROM initial_choice
            WHERE key >= ?1 AND key < ?2 AND key > ?3 ORDER BY key LIMIT ?4",
        )?;
        Ok(q.query_map(params![start, end, after, limit.min(201) as i64], |r| {
            Ok(InitialChoice { key: r.get(0)?, path: r.get(1)?, folder: r.get(2)?, selected: r.get(3)? })
        })?
        .collect::<rusqlite::Result<_>>()?)
    }

    /// Selecting a file includes its containing folders. A folder's choice
    /// covers its existing children only; future files follow ordinary sync.
    pub fn initial_select(&self, key: &str, selected: bool) -> Result<bool> {
        if key.starts_with("l:") || key.starts_with("r:") {
            self.require_durable()?;
        }
        let row: Option<(String, bool)> = self
            .c
            .prepare_cached("SELECT path,folder FROM initial_choice WHERE key=?1")?
            .query_row([key], |r| Ok((r.get(0)?, r.get(1)?)))
            .optional()?;
        let Some((_path, folder)) = row else { return Ok(false) };
        if !selected {
            let linked = match (local_id(key), key.strip_prefix("r:")) {
                (Some(id), _) => self.synced_by_local(id)?.is_some(),
                (_, Some(id)) => self.synced_by_remote(id)?.is_some(),
                _ => false,
            };
            if linked {
                return Ok(false);
            }
        }
        let mut changed = self
            .c
            .execute("UPDATE initial_choice SET selected=?2 WHERE key=?1 AND selected<>?2", params![key, selected])?;
        if let Some(id) = local_id(key) {
            let bytes = crate::local::blob(id);
            if folder {
                changed += self.c.execute(
                    "WITH RECURSIVE sub(id) AS (SELECT ?1 UNION
                    SELECT i.file_id FROM local_item i JOIN sub s ON i.parent_id=s.id)
                    UPDATE initial_choice SET selected=?2 WHERE selected<>?2 AND key IN
                    (SELECT 'l:'||lower(hex(id)) FROM sub)",
                    params![bytes, selected],
                )?;
            }
            if selected {
                changed += self.c.execute(
                    "WITH RECURSIVE parents(id) AS (SELECT ?1 UNION
                    SELECT i.parent_id FROM local_item i JOIN parents p ON i.file_id=p.id WHERE i.parent_id IS NOT NULL)
                    UPDATE initial_choice SET selected=1 WHERE selected<>1 AND key IN
                    (SELECT 'l:'||lower(hex(id)) FROM parents)",
                    [bytes],
                )?;
            }
        } else if let Some(id) = key.strip_prefix("r:") {
            if folder {
                changed += self.c.execute(
                    "WITH RECURSIVE sub(id) AS (SELECT ?1 UNION
                    SELECT i.id FROM remote_item i JOIN sub s ON i.parent_id=s.id)
                    UPDATE initial_choice SET selected=?2 WHERE selected<>?2 AND key IN (SELECT 'r:'||id FROM sub)",
                    params![id, selected],
                )?;
            }
            if selected {
                changed += self.c.execute(
                    "WITH RECURSIVE parents(id) AS (SELECT ?1 UNION
                    SELECT i.parent_id FROM remote_item i JOIN parents p ON i.id=p.id WHERE i.parent_id IS NOT NULL)
                    UPDATE initial_choice SET selected=1 WHERE selected<>1 AND key IN (SELECT 'r:'||id FROM parents)",
                    [id],
                )?;
            }
        }
        // Acknowledgements never affect planning. Repeating an existing
        // choice must not trigger another full planning pass either.
        if changed == 0 || !(key.starts_with("l:") || key.starts_with("r:")) {
            return Ok(true);
        }
        // Changes to a choice must be considered even when no file changed.
        self.meta_delete("plan.full_pass_version")?;
        let revision = self.meta_get("initial.revision")?.and_then(|s| s.parse::<u64>().ok()).unwrap_or(0);
        self.meta_set("initial.revision", &revision.saturating_add(1).to_string())?;
        self.dirty_mark_synced(&[oneup_sync::ROOT])?;
        Ok(true)
    }
}
