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
        // Where it was deleted from: where OneDrive would restore it.
        if let Some(parent) = item.parent_id() {
            ops.note_deleted_from(&item.id, parent)?;
        }
        ops.unmark_shared(&item.id)?;
        t.deleted += 1;
        return Ok(());
    }
    let Some(row) = to_row(item) else {
        t.ignored += 1;
        return Ok(());
    };
    let child_count = item.folder.as_ref().and_then(|f| f.child_count);
    let placed = place_row(ops, rules, mode, row, child_count, t)?;
    // Every entry says whether it's shared now (`ITEM_SELECT` asks for the
    // facet), so the mark follows each change: kept for an item stored, or
    // waiting as an orphan (whose mark goes if it's dropped or pruned when
    // placed), never for one discarded inside an excluded folder.
    match (placed, item.sharing()) {
        (Placed::Kept, Some(at)) => ops.mark_shared(&item.id, at),
        _ => ops.unmark_shared(&item.id),
    }
}

/// Where [`place_row`] put an entry.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Placed {
    /// Stored, or waiting for its parent in the orphan buffer.
    Kept,
    /// Dropped: inside an excluded folder, or not placeable.
    Discarded,
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
    let id = row.id.clone();
    if place_row(ops, rules, mode, row, child_count, t)? == Placed::Discarded {
        ops.unmark_shared(&id)?;
    }
    Ok(())
}

fn place_row(
    ops: &Ops<'_>,
    rules: &ExcludeSet,
    mode: Mode,
    row: RemoteRow,
    child_count: Option<i64>,
    t: &mut Tally,
) -> Result<Placed> {
    if row.kind == Kind::Root {
        ops.upsert_item(&row, false)?;
        t.stored += 1;
        return Ok(Placed::Kept);
    }
    let Some(parent) = row.parent_id.clone() else {
        t.ignored += 1;
        return Ok(Placed::Discarded);
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
            return Ok(Placed::Discarded);
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
                        // The folder may have moved under one of its own
                        // descendants (pages apply one at a time, so the
                        // store can hold that loop for a while): pruning
                        // its subtree took the new parent with it, and the
                        // folder is inside that pruned subtree now.
                        if ops.item_state(&parent)? != ItemState::Included {
                            ops.prune_subtree(&row.id, true)?;
                            ops.add_pruned(&row.id, &parent)?;
                            t.pruned += 1;
                            return Ok(Placed::Discarded);
                        }
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
                    // Descendants may have moved elsewhere and arrive later
                    // in this enumeration. Keep their tombstones so their
                    // own arrival still schedules the contents we dropped.
                    ops.remove_pruned_one(&row.id)?;
                }
                ops.upsert_item(&row, false)?;
                if needs_enum {
                    t.needs_enumeration.push(row.id.clone());
                }
                t.stored += 1;
            }
        }
    }
    Ok(Placed::Kept)
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
            ops.remove_pruned_one(&id)?;
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
    fn a_returning_folder_keeps_moved_descendants_due_for_enumeration() {
        let mut s = oneup_store::Store::open_in_memory().unwrap();
        s.write(|o| {
            let rules = oneup_rules::ExcludeSet::from_names(["node_modules"]);
            let mut t = super::Tally::default();
            for json in [
                r#"{"id":"R","name":"root","root":{},"folder":{}}"#,
                r#"{"id":"x","name":"node_modules","folder":{},"parentReference":{"id":"R"}}"#,
                r#"{"id":"a","name":"a","folder":{},"parentReference":{"id":"x"}}"#,
                r#"{"id":"b","name":"b","folder":{},"parentReference":{"id":"a"}}"#,
            ] {
                let item = serde_json::from_str::<DriveItem>(json).unwrap();
                super::apply(o, &rules, super::Mode::CatchUp, &item, &mut t)?;
            }
            // Both move out. Enumerating a no longer delivers b's contents.
            let mut t = super::Tally::default();
            for json in [
                r#"{"id":"a","name":"a","folder":{"childCount":1},"parentReference":{"id":"R"}}"#,
                r#"{"id":"b","name":"b","folder":{"childCount":1},"parentReference":{"id":"R"}}"#,
            ] {
                let item = serde_json::from_str::<DriveItem>(json).unwrap();
                super::apply(o, &rules, super::Mode::Enumerate, &item, &mut t)?;
            }
            assert_eq!(t.needs_enumeration, ["a", "b"]);
            Ok(())
        })
        .unwrap();
    }

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

    /// Delta's entry for a deleted item names the folder it was deleted
    /// from (measured), even one it was moved into that this PC never saw:
    /// where OneDrive would restore it.
    #[test]
    fn a_deletion_keeps_the_folder_onedrive_says_it_went_from() {
        let mut s = oneup_store::Store::open_in_memory().unwrap();
        let from = s
            .write(|o| {
                o.history_start(0)?;
                let mut t = super::Tally::default();
                // a.txt synced (only an item a record links is restored).
                let base = |sid, parent, name: &str, kind, remote: &str| oneup_sync::Base {
                    sid,
                    parent,
                    name: name.into(),
                    kind,
                    size: 0,
                    hash: None,
                    local: Some(oneup_sync::LocalId(u128::from(sid) + 100)),
                    local_mtime: 1,
                    remote: Some(oneup_sync::RemoteId(remote.into())),
                    remote_etag: "e".into(),
                };
                o.synced_put(&base(oneup_sync::ROOT, None, "", oneup_sync::NodeKind::Folder, "R"))?;
                o.synced_put(&base(2, Some(oneup_sync::ROOT), "a.txt", oneup_sync::NodeKind::File, "a"))?;
                for json in [
                    r#"{"id":"R","name":"root","root":{},"folder":{"childCount":1}}"#,
                    r#"{"id":"d","name":"docs","folder":{"childCount":1},"parentReference":{"id":"R"}}"#,
                    r#"{"id":"a","name":"a.txt","file":{},"parentReference":{"id":"d"}}"#,
                    r#"{"id":"a","deleted":{"state":"deleted"},"file":{},"parentReference":{"id":"trash"}}"#,
                ] {
                    let item = serde_json::from_str::<DriveItem>(json).unwrap();
                    super::apply(o, &oneup_rules::ExcludeSet::none(), super::Mode::CatchUp, &item, &mut t)?;
                }
                assert_eq!(t.deleted, 1);
                o.deleted_from(&oneup_sync::RemoteId("a".into()))
            })
            .unwrap();
        assert_eq!(from, Some((oneup_sync::RemoteId("trash".into()), "a.txt".into())));
    }

    /// Pages apply one at a time, so a move of two folders past each other
    /// can leave the store with `a` under `b` and `b` under `a` until the
    /// page with the second. A folder that is excluded as it moves under one
    /// of its own descendants took that new parent with it when its subtree
    /// was pruned, and must not stay behind with a parent that is gone.
    #[test]
    fn an_excluded_folder_moved_under_its_own_descendant_is_pruned() {
        let mut s = oneup_store::Store::open_in_memory().unwrap();
        s.write(|o| {
            let mut t = super::Tally::default();
            let rules = oneup_rules::ExcludeSet::from_names(["node_modules"]);
            // R > a > b; then OneDrive moves `a` (now node_modules) under `b`,
            // and `b` under an excluded folder `x`.
            for json in [
                r#"{"id":"R","name":"root","root":{},"folder":{"childCount":2}}"#,
                r#"{"id":"x","name":"node_modules","folder":{"childCount":1},"parentReference":{"id":"R"}}"#,
                r#"{"id":"a","name":"a","folder":{"childCount":1},"parentReference":{"id":"R"}}"#,
                r#"{"id":"b","name":"b","folder":{"childCount":0},"parentReference":{"id":"a"}}"#,
                r#"{"id":"a","name":"node_modules","folder":{"childCount":1},"parentReference":{"id":"b"}}"#,
                r#"{"id":"b","name":"b","folder":{"childCount":1},"parentReference":{"id":"x"}}"#,
            ] {
                let item = serde_json::from_str::<DriveItem>(json).unwrap();
                super::apply(o, &rules, super::Mode::CatchUp, &item, &mut t)?;
            }
            assert_eq!(o.check_invariants()?, Vec::<String>::new());
            assert_eq!(o.item_state("a")?, oneup_store::ItemState::Pruned);
            assert_eq!(o.item_state("b")?, oneup_store::ItemState::Pruned);
            Ok(())
        })
        .unwrap();
    }
}
