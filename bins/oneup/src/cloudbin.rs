//! Items deleted from OneDrive, as far as the Synced tree's history
//! reaches (`oneup recycle-bin`, the settings window's **Recovery** page).
//!
//! OneDrive Personal's recycle bin can't be listed through documented
//! Graph: the `recycleBin` resource is SharePoint's (sites and SharePoint
//! Embedded containers), and a drive has no such relationship. A deleted
//! item can be brought back by its ID (`POST /items/{id}/restore`,
//! documented for OneDrive Personal only). So this lists the items whose
//! records went with their OneDrive IDs, which OneDrive no longer lists:
//! deleted on either side and synced, or removed by a rewind. What was
//! deleted before the history began, or never synced, isn't listed (the
//! recycle bin on OneDrive's website has it). Bringing one back changes
//! only OneDrive, and takes a free name; this PC gets it by syncing.

use anyhow::{Context as _, Result, bail};
use oneup_store::Ops;
use oneup_sync::rewind::path_then;
use oneup_sync::{NodeKind, RemoteId};
use serde_json::{Value, json};

/// Listed at most.
pub const MAX: usize = 1_000;

/// A deleted item.
#[derive(Debug, Clone)]
pub struct Deleted {
    /// Its place in the history: what `--restore` names.
    pub seq: i64,
    pub at_ms: i64,
    /// Where it was, relative to the synced folder (`/docs/a.txt`).
    pub path: String,
    pub name: String,
    pub kind: NodeKind,
    pub size: i64,
    pub id: RemoteId,
    /// The OneDrive folder it goes back into, if that's still there.
    pub folder: Option<RemoteId>,
}

/// Items deleted from OneDrive, newest first.
pub fn list(o: &Ops<'_>, limit: usize) -> oneup_store::Result<Vec<Deleted>> {
    let mut out = Vec::new();
    for r in o.history_removed(limit.min(MAX), None)? {
        let Some(id) = r.was.remote.clone() else { continue };
        // Its folder now: the one it was in, if that's still synced.
        let folder = match r.was.parent {
            Some(p) => o.synced_get(p)?.and_then(|b| b.remote),
            None => None,
        };
        let parent_path = match r.was.parent {
            Some(p) => path_then(o, o, p, r.at_ms - 1)?,
            None => String::new(),
        };
        out.push(Deleted {
            seq: r.seq,
            at_ms: r.at_ms,
            path: format!("{parent_path}/{}", r.was.name),
            name: r.was.name.clone(),
            kind: r.was.kind,
            size: r.was.size,
            id,
            folder,
        });
    }
    Ok(out)
}

pub fn to_json(d: &Deleted) -> Value {
    json!({
        "seq": d.seq,
        "at_ms": d.at_ms,
        "path": d.path,
        "folder": d.kind == NodeKind::Folder,
        "size": d.size,
        "restorable": d.folder.is_some(),
    })
}

/// Brings back the deleted item `seq` names, from OneDrive's recycle bin,
/// into the folder it was in, under its name or `name (restored N)` if
/// that's taken. Returns where it went (relative to the synced folder).
pub async fn restore(dir: &std::path::Path, seq: i64) -> Result<String> {
    crate::setup::require_test_account(dir)?;
    if crate::vaultpair::is_vault_dir(dir) {
        bail!("this is an encrypted vault: OneDrive only has its encrypted files");
    }
    let store = crate::setup::open_store(dir, 4 * 1024)?;
    let d = store
        .read(|o| list(o, MAX))?
        .into_iter()
        .find(|d| d.seq == seq)
        .context("that item isn't listed as deleted any more")?;
    let folder = d.folder.clone().context("the folder it was in is gone too: bring that back first")?;
    drop(store);
    let http = crate::setup::http()?;
    let auth = crate::setup::auth(dir, None, &http)?;
    let graph = std::sync::Arc::new(crate::pairs::scoped(
        dir,
        oneup_graph::Graph::new(http, auth, oneup_graph::Aimd::new(2, 4)),
    )?);
    let cloud = oneup_exec::GraphCloud::new(graph);
    let row =
        oneup_exec::recover::restore_deleted(&cloud, &d.id, &folder, &d.name, d.kind).await.map_err(|e| match e {
            oneup_exec::CloudError::Changed => {
                anyhow::anyhow!("OneDrive no longer has it: its recycle bin was emptied, or kept it past its time")
            }
            e => anyhow::anyhow!("{e}"),
        })?;
    let at = d.path.rsplit_once('/').map_or("", |(p, _)| p);
    Ok(format!("{at}/{}", row.name))
}
