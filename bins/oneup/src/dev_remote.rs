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
    /// Print a file's time, size and tags, and its version history
    /// (read-only)
    Versions { path: String },
    /// Measure how the version history lists a file's current version: in
    /// a new folder `folder`, upload a file, replace it with a small upload
    /// and with an upload session of `mib` MiB, and read the item and its
    /// history after each, at once and later; then delete the folder (to
    /// the recycle bin)
    VersionsProbe {
        folder: String,
        #[arg(long, default_value_t = 6)]
        mib: usize,
    },
}

/// The item's and its history's facts, as Graph gives them (`after`: the
/// moment, for the record).
async fn show_versions(graph: &Graph, id: &str, after: &str) -> Result<()> {
    let url = format!("{}?$select=id,size,lastModifiedDateTime,eTag,cTag,fileSystemInfo", graph.item_url(id));
    let it: serde_json::Value = graph.get(&url).await?;
    println!(
        "[{after}] item: size {} lastModified {} fsModified {} eTag {} cTag {}",
        it["size"], it["lastModifiedDateTime"], it["fileSystemInfo"]["lastModifiedDateTime"], it["eTag"], it["cTag"]
    );
    if let Some((_, cur)) = graph.current_version(id).await? {
        println!(
            "[{after}] current version, as OneUpDrive reads it: {} size {:?} at {:?} ms",
            cur.id, cur.size, cur.modified_ms
        );
    }
    let v: serde_json::Value = graph.get(&format!("{}/versions", graph.item_url(id))).await?;
    for e in v["value"].as_array().into_iter().flatten() {
        println!("[{after}]   version {} size {} lastModified {}", e["id"], e["size"], e["lastModifiedDateTime"]);
    }
    Ok(())
}

/// Reads at once, after 3 s and after 15 s.
async fn show_over_time(graph: &Graph, id: &str, what: &str) -> Result<()> {
    show_versions(graph, id, &format!("{what}, at once")).await?;
    for s in [3u64, 15] {
        tokio::time::sleep(std::time::Duration::from_secs(s)).await;
        show_versions(graph, id, &format!("{what}, +{s} s")).await?;
    }
    Ok(())
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
            if parent == "/" {
                return Ok::<_, anyhow::Error>(g.root().await?.id);
            }
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
        DevRemote::Versions { path } => {
            let it = graph.item_by_path(path).await?.context("no such item")?;
            show_versions(&graph, &it.id, "now").await?;
        }
        DevRemote::VersionsProbe { folder, mib } => {
            let (parent, name) = split(folder)?;
            if graph.item_by_path(folder).await?.is_some() {
                bail!("{folder} exists; give a new folder");
            }
            let dir = graph.create_folder(&parent_id(parent).await?, &name).await?;
            let up = graph.upload_small(&dir.id, "probe.txt", None, b"version 1").await?;
            println!("[uploaded] answer: eTag {:?} cTag {:?}", up.e_tag, up.c_tag);
            show_over_time(&graph, &up.id, "uploaded").await?;
            let now = graph.item(&up.id).await?.context("gone")?;
            let tag = now.e_tag.clone().unwrap_or_default();
            let got =
                graph.upload_small(&dir.id, "probe.txt", Some((&up.id, &tag)), b"version 2, a little longer").await?;
            println!("[replaced (small)] answer: eTag {:?} cTag {:?}", got.e_tag, got.c_tag);
            show_over_time(&graph, &up.id, "replaced (small)").await?;
            let now = graph.item(&up.id).await?.context("gone")?;
            let tag = now.e_tag.clone().unwrap_or_default();
            let total = mib * 1024 * 1024;
            let data: Vec<u8> = (0..total).map(|i| (i % 251) as u8).collect();
            let url = graph.upload_session(&dir.id, "probe.txt", Some((&up.id, &tag))).await?;
            // 10 MiB fragments, a multiple of 320 KiB.
            let frag = 32 * 320 * 1024;
            let mut start = 0;
            while start < total {
                let end = (start + frag).min(total);
                let f = graph.upload_fragment(&url, start as u64, total as u64, &data[start..end]).await?;
                if let Some(it) = f.item {
                    println!("[replaced (session)] answer: eTag {:?} cTag {:?}", it.e_tag, it.c_tag);
                }
                start = end;
            }
            show_over_time(&graph, &up.id, &format!("replaced (session, {mib} MiB)")).await?;
            // A rename changes the eTag but not the content: what then?
            let now = graph.item(&up.id).await?.context("gone")?;
            graph.move_item(&up.id, now.e_tag.as_deref().unwrap_or_default(), &dir.id, "renamed.txt").await?;
            show_versions(&graph, &up.id, "renamed").await?;
            let now = graph.item(&up.id).await?.context("gone")?;
            let tag = now.e_tag.clone().unwrap_or_default();
            let got = graph.upload_small(&dir.id, "renamed.txt", Some((&up.id, &tag)), b"version 4").await?;
            println!("[replaced after the rename] answer: eTag {:?} cTag {:?}", got.e_tag, got.c_tag);
            show_versions(&graph, &up.id, "replaced after the rename").await?;
            let dir_now = graph.item(&dir.id).await?.context("folder gone")?;
            graph.delete_item(&dir.id, dir_now.e_tag.as_deref().unwrap_or_default()).await?;
            println!("deleted {folder} (recycle bin)");
        }
        DevRemote::Rm { path } => {
            let it = graph.item_by_path(path).await?.context("no such item")?;
            graph.delete_item(&it.id, it.e_tag.as_deref().unwrap_or_default()).await?;
        }
    }
    println!("done");
    Ok(())
}
