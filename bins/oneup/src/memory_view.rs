//! Size and location metadata for the Files page's Memory view.

use std::path::{Path, PathBuf};

use anyhow::{Context, Result, ensure};
use oneup_store::treemap::{Item, Level};
use oneup_store::{Kind, LocalKind, Store};
use serde_json::{Value, json};

const TILES: usize = 32;
const PREVIEWS: usize = 6;
const PREVIEW_TILES: usize = 16;

fn map_json<I>(level: Level<I>, reported_bytes: u64, tile: impl FnMut(Item<I>) -> Result<Value>) -> Result<Value> {
    let bytes = level.bytes.max(reported_bytes);
    let other = bytes.saturating_sub(level.items.iter().map(|i| i.bytes).sum());
    let items = level.items.into_iter().map(tile).collect::<Result<Vec<_>>>()?;
    Ok(json!({ "bytes": bytes, "other": other, "items": items }))
}

/// Only the main pair's Local tree or the account's Remote tree is read.
pub fn read(dir: &Path, on_disk: bool, folder: Option<&str>) -> Result<Value> {
    let store = crate::setup::open_store(dir, 4 * 1024)?;
    if on_disk {
        let root = store.read(|o| o.local_root())?.context("This folder has not been scanned yet.")?;
        let id = folder.map(str::parse).transpose()?.unwrap_or(root.root_file_id);
        let row = store.read(|o| o.local_get(id))?.context("Folder has moved or is no longer listed.")?;
        ensure!(matches!(row.kind, LocalKind::Folder | LocalKind::Root), "Choose a folder.");
        let rel = store.read(|o| o.local_path_of(id))?.context("Folder has moved or is no longer listed.")?;
        let at = PathBuf::from(root.path).join(crate::filetree::clean(&rel).replace('/', "\\"));
        local(&store, id, &at, true)
    } else {
        let row = match folder {
            Some(id) => store.read(|o| o.get_item(id))?,
            None => store.read(|o| o.remote_root())?,
        }
        .context("This OneDrive folder has not been listed yet.")?;
        ensure!(matches!(row.kind, Kind::Folder | Kind::Root), "Choose a folder.");
        let reported_bytes = if row.kind == Kind::Root { 0 } else { u64::try_from(row.size).unwrap_or_default() };
        remote(&store, &row.id, reported_bytes, true)
    }
}

fn local(store: &Store, parent: u128, at: &Path, overview: bool) -> Result<Value> {
    let level = store.read(|o| o.local_treemap(parent, if overview { TILES } else { PREVIEW_TILES }))?;
    let mut previews = 0;
    map_json(level, 0, |item| {
        let path = at.join(&item.name);
        let preview = if overview && item.expandable && previews < PREVIEWS {
            previews += 1;
            Some(local(store, item.id, &path, false)?)
        } else {
            None
        };
        Ok(json!({
            "id": item.id.to_string(),
            "name": item.name,
            "bytes": item.bytes,
            "folder": item.folder,
            "expandable": item.expandable,
            "location": path.to_string_lossy().trim_start_matches(r"\\?\"),
            "preview": preview,
        }))
    })
}

fn remote(store: &Store, parent: &str, reported_bytes: u64, overview: bool) -> Result<Value> {
    let level = store.read(|o| o.remote_treemap(parent, if overview { TILES } else { PREVIEW_TILES }))?;
    let mut previews = 0;
    map_json(level, reported_bytes, |item| {
        let preview = if overview && item.expandable && previews < PREVIEWS {
            previews += 1;
            Some(remote(store, &item.id, item.bytes, false)?)
        } else {
            None
        };
        Ok(json!({
            "location": if item.folder { item.id.as_str() } else { parent },
            "id": item.id,
            "name": item.name,
            "bytes": item.bytes,
            "folder": item.folder,
            "expandable": item.expandable,
            "preview": preview,
        }))
    })
}
