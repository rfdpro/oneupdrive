//! Applying delta/children entries to the Remote tree.
//!
//! This is the only place that decides how a remote entry changes the mirror.
//! It runs inside a store transaction and does no network I/O, so it is
//! deterministic and fully testable.
//!
//! Rules:
//! * Entries may repeat and may arrive in any order. Upserts are idempotent,
//!   and an entry whose parent is unknown waits in the orphan buffer.
//! * An excluded folder is stored as one opaque row. Anything found beneath
//!   it is dropped, and folders beneath it become tombstones so later entries
//!   under them are recognised immediately.
//! * During catch-up, a folder that appears from nowhere (moved in from an
//!   excluded or unknown area, or newly included by a rule change) is
//!   reported as `needs_enumeration`, because Graph does not re-send the
//!   contents of a moved folder.

use oneup_graph::DriveItem;
use oneup_rules::ExcludeSet;
use oneup_store::{ItemState, Kind, Ops, RemoteRow, Result};

/// Where entries come from, which changes how unknown folders are treated.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Mode {
    /// Initial enumeration: subtree contents are being delivered anyway.
    Enumerate,
    /// Incremental changes: folders that appear with content need their own enumeration.
    CatchUp,
}

#[derive(Debug, Default, Clone, PartialEq, Eq)]
pub struct Tally {
    pub stored: u64,
    pub excluded: u64,
    pub pruned: u64,
    pub orphaned: u64,
    pub deleted: u64,
    pub ignored: u64,
    /// Folder ids whose subtree must be enumerated.
    pub needs_enumeration: Vec<String>,
}

impl Tally {
    pub fn add(&mut self, o: &Tally) {
        self.stored += o.stored;
        self.excluded += o.excluded;
        self.pruned += o.pruned;
        self.orphaned += o.orphaned;
        self.deleted += o.deleted;
        self.ignored += o.ignored;
        self.needs_enumeration.extend(o.needs_enumeration.iter().cloned());
    }
}

/// Converts a Graph item to a store row. `None` for entries without a name
/// (only deletions legitimately lack one).
pub fn to_row(item: &DriveItem) -> Option<RemoteRow> {
    let kind = if item.is_root() {
        Kind::Root
    } else if item.is_package() {
        Kind::Package
    } else if item.is_folder() {
        Kind::Folder
    } else if item.file.is_some() {
        Kind::File
    } else {
        // Neither a file nor a folder: the Personal Vault (measured:
        // no facet at all, size 0) or a shortcut to a folder
        // shared with the account (`remoteItem`). Like a package, it stays
        // in OneDrive; downloading it would make an empty file of it.
        Kind::Package
    };
    Some(RemoteRow {
        id: item.id.clone(),
        parent_id: if kind == Kind::Root { None } else { item.parent_id().map(str::to_string) },
        name: item.name.clone()?,
        kind,
        size: item.size.unwrap_or(0),
        etag: item.e_tag.clone(),
        qxh: item.quick_xor_hash(),
        mtime_ms: item.modified_ms(),
    })
}

/// Applies one Graph entry.
pub fn apply(ops: &Ops<'_>, rules: &ExcludeSet, mode: Mode, item: &DriveItem, t: &mut Tally) -> Result<()> {
    if item.is_deleted() {
        ops.delete_subtree(&item.id)?;
        t.deleted += 1;
        return Ok(());
    }
    let Some(row) = to_row(item) else {
        t.ignored += 1;
        return Ok(());
    };
    let child_count = item.folder.as_ref().and_then(|f| f.child_count);
    apply_row(ops, rules, mode, row, child_count, t)
}

/// Applies one row (from Graph or from the orphan buffer). `child_count` is
/// `None` when unknown, which is treated conservatively as "has content".
pub fn apply_row(
    ops: &Ops<'_>,
    rules: &ExcludeSet,
    mode: Mode,
    row: RemoteRow,
    child_count: Option<i64>,
    t: &mut Tally,
) -> Result<()> {
    if row.kind == Kind::Root {
        ops.upsert_item(&row, false)?;
        t.stored += 1;
        return Ok(());
    }
    let Some(parent) = row.parent_id.clone() else {
        t.ignored += 1;
        return Ok(());
    };
    let was = ops.item_state(&row.id)?;
    match ops.item_state(&parent)? {
        ItemState::Excluded | ItemState::Pruned => {
            // Inside an excluded subtree: drop it, and remember folders as tombstones.
            if matches!(was, ItemState::Included | ItemState::Excluded) {
                ops.prune_subtree(&row.id, true)?;
            }
            if row.kind == Kind::Folder {
                ops.add_pruned(&row.id, &parent)?;
            }
            t.pruned += 1;
        }
        ItemState::Absent => {
            ops.put_orphan(&row, mode == Mode::CatchUp)?;
            t.orphaned += 1;
        }
        ItemState::Included => {
            let exclude = row.kind == Kind::Folder && excludes_folder(ops, rules, &row.name, &parent)?;
            if exclude {
                match was {
                    ItemState::Included => {
                        ops.prune_subtree(&row.id, false)?;
                    }
                    // Its own tombstone goes; tombstones below it stay valid.
                    ItemState::Pruned => ops.remove_pruned_one(&row.id)?,
                    ItemState::Excluded | ItemState::Absent => {}
                }
                ops.upsert_item(&row, true)?;
                t.excluded += 1;
            } else {
                let has_content = child_count != Some(0);
                let needs_enum = row.kind == Kind::Folder
                    && has_content
                    && match was {
                        ItemState::Pruned | ItemState::Excluded => true,
                        ItemState::Absent => mode == Mode::CatchUp,
                        ItemState::Included => false,
                    };
                if matches!(was, ItemState::Pruned | ItemState::Excluded) {
                    ops.unprune_subtree(&row.id)?;
                }
                ops.upsert_item(&row, false)?;
                if needs_enum {
                    t.needs_enumeration.push(row.id.clone());
                }
                t.stored += 1;
            }
        }
    }
    Ok(())
}

/// Whether a folder named `name` in folder `parent` is excluded: by the
/// rules, or because it is OneUpDrive's own folder in the drive root
/// ([`oneup_rules::ENGINE_DIR`]), which mirrors the local sync folder's and
/// must never be merged with it.
fn excludes_folder(ops: &Ops<'_>, rules: &ExcludeSet, name: &str, parent: &str) -> Result<bool> {
    if rules.excludes_folder(name) {
        return Ok(true);
    }
    if !oneup_rules::is_engine_dir(name) {
        return Ok(false);
    }
    Ok(ops.get_item(parent)?.is_some_and(|p| p.kind == Kind::Root))
}

/// Re-applies buffered orphans whose parents are now known, repeatedly,
/// until no more can be placed.
///
/// Each orphan is re-applied in the mode it arrived in: an orphan from the
/// catch-up feed may be a folder moved in from elsewhere whose contents no
/// stream will deliver, even if it is placed later by an enumeration run
/// (e.g. after an interrupted catch-up).
pub fn resolve_local_orphans(ops: &Ops<'_>, rules: &ExcludeSet, t: &mut Tally) -> Result<()> {
    loop {
        let ready = ops.take_resolvable_orphans()?;
        if ready.is_empty() {
            return Ok(());
        }
        for (row, catch_up) in ready {
            t.orphaned = t.orphaned.saturating_sub(1);
            let cc = if row.kind == Kind::Folder { None } else { Some(0) };
            let mode = if catch_up { Mode::CatchUp } else { Mode::Enumerate };
            apply_row(ops, rules, mode, row, cc, t)?;
        }
    }
}

/// Re-evaluates stored folders after the rules changed. Newly excluded
/// folders lose their contents; newly included ones need enumeration.
pub fn reapply_rules(ops: &Ops<'_>, rules: &ExcludeSet) -> Result<Tally> {
    let mut to_exclude = Vec::new();
    let mut to_include = Vec::new();
    let mut engine_named = Vec::new();
    ops.for_each_folder(|id, name, excluded| {
        // The engine folder rule depends on the parent, which this walk
        // doesn't carry; such folders (rare) are settled just below.
        if oneup_rules::is_engine_dir(name) {
            engine_named.push((id.to_string(), name.to_string(), excluded));
            return;
        }
        match (excluded, rules.excludes_folder(name)) {
            (false, true) => to_exclude.push(id.to_string()),
            (true, false) => to_include.push(id.to_string()),
            _ => {}
        }
    })?;
    for (id, name, excluded) in engine_named {
        let parent = ops.get_item(&id)?.and_then(|r| r.parent_id);
        let want = match parent {
            Some(p) => excludes_folder(ops, rules, &name, &p)?,
            None => false,
        };
        match (excluded, want) {
            (false, true) => to_exclude.push(id),
            (true, false) => to_include.push(id),
            _ => {}
        }
    }
    let mut t = Tally::default();
    for id in to_exclude {
        // A folder already pruned as part of an earlier iteration is gone.
        if let Some(row) = ops.get_item(&id)? {
            ops.prune_subtree(&id, false)?;
            ops.upsert_item(&row, true)?;
            t.excluded += 1;
        }
    }
    for id in to_include {
        if let Some(row) = ops.get_item(&id)? {
            ops.unprune_subtree(&id)?;
            ops.upsert_item(&row, false)?;
            t.needs_enumeration.push(id);
            t.stored += 1;
        }
    }
    Ok(t)
}

#[cfg(test)]
mod tests {
    use oneup_graph::DriveItem;
    use oneup_store::Kind;

    #[test]
    fn items_that_are_neither_files_nor_folders_stay_in_onedrive() {
        let kind = |json: &str| super::to_row(&serde_json::from_str::<DriveItem>(json).unwrap()).unwrap().kind;
        assert_eq!(kind(r#"{"id":"a","name":"a.txt","file":{}}"#), Kind::File);
        assert_eq!(kind(r#"{"id":"b","name":"B","folder":{}}"#), Kind::Folder);
        // The Personal Vault, as the test account's delta listed it.
        assert_eq!(kind(r#"{"id":"v","name":"Personal Vault","size":0}"#), Kind::Package);
        // A shortcut to a folder someone shared.
        assert_eq!(kind(r#"{"id":"s","name":"Shared","remoteItem":{"id":"x"}}"#), Kind::Package);
    }
}
