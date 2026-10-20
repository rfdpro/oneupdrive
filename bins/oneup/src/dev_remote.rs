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
    /// Measure what restoring a deleted item does when its name is taken,
    /// and when it isn't, in a new folder `folder` (made, then deleted into
    /// the recycle bin afterwards): prints each answer and what the folder
    /// holds
    RestoreProbe { folder: String },
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
        DevRemote::RestoreProbe { folder } => {
            let (parent, name) = split(folder)?;
            if graph.item_by_path(folder).await?.is_some() {
                bail!("{folder} exists already; pick a new name");
            }
            let top = if parent == "/" { graph.root().await?.id } else { parent_id(parent).await? };
            let dir = graph.create_folder(&top, &name).await?;
            let list = |label: &'static str| {
                let (g, id) = (Arc::clone(&graph), dir.id.clone());
                async move {
                    let page = g.children_page(&id, None).await?;
                    println!("{label}:");
                    for it in page.value {
                        println!("  {:?} (id {}, {} bytes)", it.name, it.id, it.size.unwrap_or_default());
                    }
                    Ok::<_, anyhow::Error>(())
                }
            };
            let restore = |id: String, label: &'static str, as_name: &'static str| {
                let (g, to) = (Arc::clone(&graph), dir.id.clone());
                async move {
                    match g.restore_deleted(&id, &to, as_name).await {
                        Ok(it) => {
                            println!("{label}: restored as {:?} (id {}, same id: {})", it.name, it.id, it.id == id)
                        }
                        Err(e) => println!("{label}: refused, status {:?}, code {:?}: {e}", e.status(), e.code()),
                    }
                }
            };
            // The name taken: a.txt deleted, a new a.txt made, the first
            // restored under the same name.
            let first = graph.upload_small(&dir.id, "a.txt", None, b"first").await?;
            graph.delete_item(&first.id, first.e_tag.as_deref().unwrap_or_default()).await?;
            graph.upload_small(&dir.id, "a.txt", None, b"second, the new one").await?;
            restore(first.id.clone(), "restore with the name taken", "a.txt").await;
            list("the folder after it").await?;
            // Under another name, free: is the name asked for honoured?
            let second = graph.upload_small(&dir.id, "c.txt", None, b"third").await?;
            graph.delete_item(&second.id, second.e_tag.as_deref().unwrap_or_default()).await?;
            restore(second.id.clone(), "restore as a free name other than its own", "c-restored.txt").await;
            list("the folder after it").await?;
            // Its own name, free.
            let third = graph.upload_small(&dir.id, "d.txt", None, b"fourth").await?;
            graph.delete_item(&third.id, third.e_tag.as_deref().unwrap_or_default()).await?;
            restore(third.id.clone(), "restore under its own name, free", "d.txt").await;
            list("the folder after it").await?;
            // Into another folder than the one it was deleted from.
            let sub = graph.create_folder(&dir.id, "sub").await?;
            let fourth = graph.upload_small(&dir.id, "e.txt", None, b"fifth").await?;
            graph.delete_item(&fourth.id, fourth.e_tag.as_deref().unwrap_or_default()).await?;
            match graph.restore_deleted(&fourth.id, &sub.id, "e.txt").await {
                Ok(it) => println!(
                    "restore into another folder: restored as {:?} in {:?} (asked {})",
                    it.name,
                    it.parent_id(),
                    sub.id
                ),
                Err(e) => {
                    println!("restore into another folder: refused, status {:?}, code {:?}: {e}", e.status(), e.code())
                }
            }
            list("the folder after it").await?;
            // Its folder deleted after it: what does restoring it do?
            let x = graph.create_folder(&dir.id, "x").await?;
            let fifth = graph.upload_small(&x.id, "f.txt", None, b"sixth").await?;
            graph.delete_item(&fifth.id, fifth.e_tag.as_deref().unwrap_or_default()).await?;
            let x_now = graph.item_by_path(&format!("{folder}/x")).await?.context("x")?;
            graph.delete_item(&x_now.id, x_now.e_tag.as_deref().unwrap_or_default()).await?;
            match graph.restore_deleted(&fifth.id, &dir.id, "f.txt").await {
                Ok(it) => println!(
                    "restore with its folder deleted after it: restored as {:?} in {:?}",
                    it.name,
                    it.parent_id()
                ),
                Err(e) => println!(
                    "restore with its folder deleted after it: refused, status {:?}, code {:?}: {e}",
                    e.status(),
                    e.code()
                ),
            }
            println!("x now: {:?}", graph.item_by_path(&format!("{folder}/x")).await?.map(|i| i.id));
            list("the folder after it").await?;
            // Deleted with its folder (the folder deleted whole).
            let y = graph.create_folder(&dir.id, "y").await?;
            let sixth = graph.upload_small(&y.id, "g.txt", None, b"seventh").await?;
            let y_now = graph.item_by_path(&format!("{folder}/y")).await?.context("y")?;
            graph.delete_item(&y_now.id, y_now.e_tag.as_deref().unwrap_or_default()).await?;
            match graph.restore_deleted(&sixth.id, &dir.id, "g.txt").await {
                Ok(it) => println!(
                    "restore of an item deleted with its folder: restored as {:?} in {:?}",
                    it.name,
                    it.parent_id()
                ),
                Err(e) => println!(
                    "restore of an item deleted with its folder: refused, status {:?}, code {:?}: {e}",
                    e.status(),
                    e.code()
                ),
            }
            println!("y now: {:?}", graph.item_by_path(&format!("{folder}/y")).await?.map(|i| i.id));
            list("the folder after it").await?;
            // Clean up: the probe folder goes to the recycle bin.
            let d = graph.item_by_path(folder).await?.context("the probe folder")?;
            graph.delete_item(&d.id, d.e_tag.as_deref().unwrap_or_default()).await?;
            println!("{folder} deleted (in the recycle bin)");
        }
        DevRemote::Rm { path } => {
            let it = graph.item_by_path(path).await?.context("no such item")?;
            graph.delete_item(&it.id, it.e_tag.as_deref().unwrap_or_default()).await?;
        }
    }
    println!("done");
    Ok(())
}
