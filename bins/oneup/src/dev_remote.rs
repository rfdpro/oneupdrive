//! `oneup dev-remote` (hidden, developers only): makes changes in the
//! **test account's** OneDrive directly, the way another device or the web
//! would, so `oneup sync` can be tried in the cloud → PC direction without
//! a second machine. Uses the same conditional writes as the executor
//! (`If-Match` on the current eTag, `conflictBehavior=fail`).

use std::sync::Arc;

use anyhow::{Context, Result, bail};
use oneup_graph::Graph;

#[derive(clap::Subcommand)]
pub enum DevRemote {
    /// Create or overwrite a small file with the given text
    Put { path: String, text: String },
    /// Create a folder
    Mkdir { path: String },
    /// Rename an item within its folder
    Rename { path: String, name: String },
    /// Delete an item (to the test account's recycle bin)
    Rm { path: String },
    /// Measure whether Graph's restoreVersion honours If-Match (it restores
    /// the previous version if it doesn't)
    RestoreCheck { path: String },
}

pub async fn run(graph: Arc<Graph>, cmd: &DevRemote) -> Result<()> {
    let split = |p: &str| -> Result<(String, String)> {
        let p = p.trim_end_matches('/');
        let (parent, name) = p.rsplit_once('/').context("use a path like /folder/name")?;
        Ok((if parent.is_empty() { "/".into() } else { parent.into() }, name.into()))
    };
    let parent_id = |parent: String| {
        let g = Arc::clone(&graph);
        async move {
            match g.item_by_path(&parent).await? {
                Some(it) => Ok::<_, anyhow::Error>(it.id),
                None => bail!("no folder {parent}"),
            }
        }
    };
    match cmd {
        DevRemote::Put { path, text } => {
            let existing = graph.item_by_path(path).await?;
            let (parent, name) = split(path)?;
            let replace = existing.as_ref().map(|it| (it.id.as_str(), it.e_tag.as_deref().unwrap_or_default()));
            graph.upload_small(&parent_id(parent).await?, &name, replace, text.as_bytes()).await?;
        }
        DevRemote::Mkdir { path } => {
            let (parent, name) = split(path)?;
            graph.create_folder(&parent_id(parent).await?, &name).await?;
        }
        DevRemote::Rename { path, name } => {
            let it = graph.item_by_path(path).await?.context("no such item")?;
            let parent = it.parent_id().context("can't rename the drive root")?.to_string();
            graph.move_item(&it.id, it.e_tag.as_deref().unwrap_or_default(), &parent, name).await?;
        }
        DevRemote::RestoreCheck { path } => {
            let it = graph.item_by_path(path).await?.context("no such item")?;
            let versions = graph.versions(&it.id).await?;
            let older = versions.get(1).context("it needs at least two versions")?;
            let stale = format!("{}x", it.e_tag.clone().unwrap_or_default());
            // Measures Graph's restoreVersion (OneUpDrive doesn't use it:
            // OneDrive Personal ignores If-Match).
            match graph.restore_version(&it.id, &older.id, &stale).await {
                Err(e) if e.status() == Some(412) => println!("stale eTag refused (412)"),
                other => println!("stale eTag NOT refused: {other:?}"),
            }
        }
        DevRemote::Rm { path } => {
            let it = graph.item_by_path(path).await?.context("no such item")?;
            graph.delete_item(&it.id, it.e_tag.as_deref().unwrap_or_default()).await?;
        }
    }
    println!("done");
    Ok(())
}
