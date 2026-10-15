//! `oneup upload-bench` (hidden, developers only): measures upload speed on
//! the **test account**, behind the upload-size choices in `oneup-graph`: one-request
//! uploads at several sizes, and upload sessions with several fragment
//! sizes. Synthetic content only, inside a new folder at the top of the
//! test drive that is removed afterwards (into the test account's recycle
//! bin).

use std::sync::Arc;
use std::time::Instant;

use anyhow::{Context, Result};
use oneup_exec::{Cloud, GraphCloud, UploadMemory};
use oneup_graph::Graph;
use oneup_sync::RemoteId;

use crate::cloud_check::{content, guard, rand_u32};

const MIB: usize = 1024 * 1024;
const UNIT: usize = 320 * 1024;

/// `smart_mib`: instead of the fixed sizes, upload files of this size
/// through the executor's uploader (adaptive fragments), per memory profile.
pub async fn run(
    dir: &std::path::Path,
    graph: Arc<Graph>,
    session_mib: usize,
    rounds: u32,
    smart_mib: Option<usize>,
) -> Result<()> {
    guard(dir, &graph).await?;
    let root = graph.root().await?;
    let name = format!("oneup-upload-bench-{:08x}", rand_u32());
    let top = graph.create_folder(&root.id, &name).await?;
    println!("Working in /{name}");
    let result = match smart_mib {
        Some(mib) => smart(&graph, &top.id, mib, rounds).await,
        None => measure(&graph, &top.id, session_mib, rounds).await,
    };
    let cleanup = match graph.item(&top.id).await {
        Ok(Some(now)) => graph.delete_item(&now.id, now.e_tag.as_deref().unwrap_or_default()).await.map_err(Into::into),
        Ok(None) => Ok(()),
        Err(e) => Err(anyhow::Error::from(e)),
    };
    result?;
    cleanup.context("the measurements finished, but removing the folder failed; remove it by hand")?;
    println!("/{name} was removed.");
    Ok(())
}

/// Files of `mib` through `GraphCloud`, as the executor sends them; one
/// uploader per profile, so later files start from what earlier ones learned.
async fn smart(graph: &Arc<Graph>, top: &str, mib: usize, rounds: u32) -> Result<()> {
    let parent = RemoteId(top.to_string());
    for profile in ["minimal", "balanced", "performance"] {
        let cloud = GraphCloud::with_memory(Arc::clone(graph), UploadMemory::for_profile(profile));
        for r in 0..rounds {
            let data = content(mib * MIB, 0x5eed_0000 + u64::from(r));
            let start = cloud.fragment_size();
            let t = Instant::now();
            let (row, _) = cloud
                .upload(&parent, &format!("smart-{profile}-{r}.bin"), None, data.len() as i64, &mut data.as_slice())
                .await
                .map_err(|e| anyhow::anyhow!("{e}"))?;
            let s = t.elapsed().as_secs_f64();
            anyhow::ensure!(row.size == data.len() as i64, "uploaded size differs");
            // The same content again, as a server-side copy.
            let pre =
                oneup_sync::RemotePre { id: RemoteId(row.id.clone()), etag: row.etag.clone().unwrap_or_default() };
            let hash = row.qxh.context("no hash")?;
            let c = Instant::now();
            cloud
                .copy(&pre, &hash, &parent, &format!("copy-{profile}-{r}.bin"))
                .await
                .map_err(|e| anyhow::anyhow!("{e}"))?;
            println!("  {profile:<11} server-side copy of it: {:.2} s", c.elapsed().as_secs_f64());
            println!(
                "  {profile:<11} {mib} MiB  {s:6.2} s  {}  (fragments: started {:.2} MiB, now {:.2} MiB)",
                rate(data.len(), s),
                start as f64 / MIB as f64,
                cloud.fragment_size() as f64 / MIB as f64
            );
        }
    }
    Ok(())
}

fn rate(bytes: usize, secs: f64) -> String {
    format!("{:6.2} MiB/s", bytes as f64 / MIB as f64 / secs.max(1e-9))
}

async fn measure(graph: &Graph, top: &str, session_mib: usize, rounds: u32) -> Result<()> {
    // Per-request cost: a 1-byte upload.
    let mut small = Vec::new();
    for i in 0..5 {
        let t = Instant::now();
        graph.upload_small(top, &format!("tiny-{i}.bin"), None, b"x").await?;
        small.push(t.elapsed().as_secs_f64());
    }
    small.sort_by(f64::total_cmp);
    println!("1-byte upload (one request): median {:.0} ms, min {:.0} ms", small[2] * 1e3, small[0] * 1e3);

    println!("\nOne-request uploads (simple PUT):");
    for mib in [1usize, 4, 8, 16, 32] {
        for r in 0..rounds {
            let data = content(mib * MIB, ((mib as u64) << 8) | u64::from(r));
            let t = Instant::now();
            graph.upload_small(top, &format!("simple-{mib}-{r}.bin"), None, &data).await?;
            let s = t.elapsed().as_secs_f64();
            println!("  {mib:>3} MiB  {s:6.2} s  {}", rate(data.len(), s));
        }
    }

    println!("\nUpload sessions, {session_mib} MiB file:");
    let total = session_mib * MIB;
    for units in [16usize, 32, 64, 128, 191] {
        let frag = units * UNIT;
        for r in 0..rounds {
            let data = content(total, ((units as u64) << 16) | u64::from(r));
            let t = Instant::now();
            let url = graph.upload_session(top, &format!("session-{units}-{r}.bin"), None).await?;
            let opened = t.elapsed().as_secs_f64();
            let mut start = 0usize;
            let mut per = Vec::new();
            while start < total {
                let end = (start + frag).min(total);
                let f = Instant::now();
                let _ = graph.upload_fragment(&url, start as u64, total as u64, &data[start..end]).await?;
                per.push(format!("{:.1}", (end - start) as f64 / MIB as f64 / f.elapsed().as_secs_f64()));
                start = end;
            }
            let s = t.elapsed().as_secs_f64();
            println!(
                "  fragment {:5.2} MiB  {s:6.2} s  {}  (session {:.0} ms; per fragment MiB/s: {})",
                frag as f64 / MIB as f64,
                rate(total, s),
                opened * 1e3,
                per.join(" ")
            );
        }
    }
    Ok(())
}
