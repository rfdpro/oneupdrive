//! `oneup cloud-check` (hidden, developers only): exercises every cloud
//! write the executor uses against the **test account**, and checks that
//! each precondition really is enforced by OneDrive: stale eTags are
//! refused, taken names are refused, non-empty folders are not deleted.
//!
//! Everything happens inside a new folder `oneup-cloud-check-<random>` at
//! the top of the test drive, which is removed afterwards (into the test
//! account's recycle bin), whether the check passed or not. Nothing else
//! in the drive is touched.

use std::sync::Arc;

use anyhow::{Context, Result, bail, ensure};
use oneup_exec::{Cloud, CloudError, GraphCloud, Meta, MetaDone};
use oneup_graph::Graph;
use oneup_graph::client::{FRAGMENT, SIMPLE_UPLOAD_MAX};
use oneup_store::RemoteRow;
use oneup_sync::{RemoteId, RemotePre};

use crate::test_account;

/// Runs the check. `dir` must be a test-account data folder whose sign-in
/// still reaches the drive recorded in its marker.
pub async fn run(dir: &std::path::Path, graph: Arc<Graph>) -> Result<()> {
    guard(dir, &graph).await?;

    let root = graph.root().await?;
    let cloud = GraphCloud::new(Arc::clone(&graph));
    let name = format!("oneup-cloud-check-{:08x}", rand_u32());
    let top = cloud.mkdir(&RemoteId(root.id.clone()), &name).await.map_err(|e| anyhow::anyhow!("{e}"))?;
    println!("Working in /{name}");

    let result = steps(&graph, &cloud, &top).await;

    // Clean up whatever the steps left. The check folder is ours and was
    // created moments ago, so deleting it with its contents is intended;
    // it goes to the test account's recycle bin.
    let cleanup = match graph.item(&top.id).await {
        Ok(Some(now)) => match graph.delete_item(&now.id, now.e_tag.as_deref().unwrap_or_default()).await {
            Ok(()) => Ok(()),
            Err(e) => Err(anyhow::anyhow!("removing /{name}: {e}")),
        },
        Ok(None) => Ok(()),
        Err(e) => Err(anyhow::anyhow!("looking up /{name}: {e}")),
    };
    match (result, cleanup) {
        (Ok(()), Ok(())) => {
            println!("All cloud writes behaved as expected; /{name} was removed.");
            Ok(())
        }
        (Err(e), Ok(())) => Err(e.context(format!("cloud-check failed (/{name} was removed)"))),
        (Ok(()), Err(c)) => Err(c.context("the checks passed, but cleanup failed; remove the folder by hand")),
        (Err(e), Err(c)) => Err(e.context(format!("cloud-check failed, and cleanup failed too: {c}"))),
    }
}

/// Refuses unless `dir` is a test-account data folder (not the default
/// one) whose sign-in reaches the test drive it recorded, and not the
/// personal drive.
pub(crate) async fn guard(dir: &std::path::Path, graph: &Graph) -> Result<()> {
    ensure!(
        !test_account::same_dir(dir, &test_account::default_dir()?),
        "developer cloud commands never run on the default data folder"
    );
    let marker = test_account::load(dir)?.context("this command needs a test-account data folder (--data-dir)")?;
    let drive = graph.drive().await?;
    ensure!(
        test_account::is_same_drive(&drive.id, &marker.drive_id),
        "this sign-in reaches a different drive than the test account recorded; refusing"
    );
    if let Some(personal) = test_account::personal_drive(&test_account::default_dir()?) {
        ensure!(!test_account::is_same_drive(&drive.id, &personal), "this is the personal drive; refusing");
    }
    Ok(())
}

async fn steps(graph: &Graph, cloud: &GraphCloud, top: &RemoteRow) -> Result<()> {
    let top_id = RemoteId(top.id.clone());
    let pre = |r: &RemoteRow| RemotePre { id: RemoteId(r.id.clone()), etag: r.etag.clone().unwrap_or_default() };

    // Folders: create, and refuse a taken name.
    let sub = cloud.mkdir(&top_id, "sub").await.map_err(other)?;
    expect(cloud.mkdir(&top_id, "sub").await, "creating a folder whose name is taken", is_taken)?;
    step("create folder; refuse a taken name");

    // Batched metadata changes ($batch): one request, each change
    // with its own precondition and outcome.
    let mk = |name: &str| Meta::Mkdir { parent: top_id.clone(), name: name.into() };
    let r = cloud.batch(&[mk("b1"), mk("b2"), mk("sub")]).await;
    let row = |r: &Result<MetaDone, CloudError>| match r {
        Ok(MetaDone::Item(row)) => Ok(row.clone()),
        other => Err(anyhow::anyhow!("expected an item, got {other:?}")),
    };
    let (b1, b2) = (row(&r[0])?, row(&r[1])?);
    ensure!(matches!(r[2], Err(CloudError::NameTaken)), "batched create on a taken name: {:?}", r[2]);
    // A new folder's eTag changes moments after it's made (measured:
    // `,1` then `,2`); the engine plans from the listing, which
    // has the later one.
    let (b1, b2) = (refetch(graph, &b1).await?, refetch(graph, &b2).await?);
    let r = cloud
        .batch(&[
            Meta::Move { pre: pre(&b1), to: top_id.clone(), name: "b1 renamed".into() },
            Meta::Move { pre: stale(&pre(&b2)), to: top_id.clone(), name: "b2 renamed".into() },
        ])
        .await;
    let b1 = row(&r[0])?;
    ensure!(b1.name == "b1 renamed", "batched rename went wrong");
    ensure!(matches!(r[1], Err(CloudError::Changed)), "batched move with a stale eTag: {:?}", r[1]);
    upload(cloud, &RemoteId(b2.id.clone()), "inside.bin", None, &content(10, 9)).await?;
    let b2 = refetch(graph, &b2).await?;
    let r = cloud.batch(&[Meta::Delete { pre: pre(&b2) }, Meta::Delete { pre: pre(&b1) }]).await;
    ensure!(matches!(r[0], Err(CloudError::Changed)), "batched delete of a non-empty folder: {:?}", r[0]);
    ensure!(matches!(r[1], Ok(MetaDone::Deleted)), "batched delete: {:?}", r[1]);
    ensure!(graph.item(&b2.id).await?.is_some() && graph.item(&b1.id).await?.is_none(), "batched deletes went wrong");
    step("batched create, rename and delete; refuse a taken name, a stale eTag and a non-empty folder");

    // Small upload (one request), then a taken name.
    let small = content(1000, 1);
    let a = upload(cloud, &top_id, "small.bin", None, &small).await?;
    expect(upload(cloud, &top_id, "small.bin", None, &small).await, "uploading over a taken name", is_taken_any)?;
    step("small upload; refuse a taken name");

    // Large upload (upload session, several fragments, uneven tail).
    // Above the one-request size, so it goes in several fragments.
    let large = content(SIMPLE_UPLOAD_MAX + FRAGMENT + 7, 2);
    let b = upload(cloud, &top_id, "large.bin", None, &large).await?;
    step("large upload in fragments");

    // Server-side copy of content OneDrive has: verified, and
    // refused onto a taken name or from a stale source.
    let mut h = oneup_hash::QuickXorHash::new();
    h.update(&large);
    let hash = h.finalize();
    let t = std::time::Instant::now();
    let c = cloud.copy(&pre(&b), &hash, &top_id, "large copy.bin").await.map_err(other)?;
    ensure!(c.qxh == Some(hash) && c.size == b.size && c.id != b.id, "the copy differs from its source");
    println!("      (copy took {:.1} s)", t.elapsed().as_secs_f64());
    expect(cloud.copy(&pre(&b), &hash, &top_id, "large copy.bin").await, "copying onto a taken name", is_taken)?;
    expect(cloud.copy(&stale(&pre(&b)), &hash, &top_id, "other.bin").await, "copying a stale source", is_changed)?;
    step("server-side copy; refuse a taken name and a stale source");

    // Download what was uploaded, byte for byte.
    let mut got = Vec::new();
    cloud.download(&pre(&b), &mut got).await.map_err(other)?;
    ensure!(got == large, "downloaded content differs from the upload");
    expect(cloud.download(&stale(&pre(&b)), &mut Vec::new()).await, "downloading with a stale eTag", is_changed)?;
    step("download; refuse a stale eTag");

    // Replace (new version, same item) for both sizes, then refuse the
    // now-stale eTags.
    let small2 = content(1500, 3);
    let a2 = upload(cloud, &top_id, "small.bin", Some(&pre(&a)), &small2).await?;
    ensure!(a2.id == a.id, "replacing a small file changed its ID");
    expect(
        upload(cloud, &top_id, "small.bin", Some(&pre(&a)), &small).await,
        "replacing with a stale eTag",
        is_changed_any,
    )?;
    let large2 = content(SIMPLE_UPLOAD_MAX + 11, 4);
    let b2 = upload(cloud, &top_id, "large.bin", Some(&pre(&b)), &large2).await?;
    ensure!(b2.id == b.id, "replacing a large file changed its ID");
    expect(
        upload(cloud, &top_id, "large.bin", Some(&pre(&b)), &large).await,
        "replacing (upload session) with a stale eTag",
        is_changed_any,
    )?;
    step("replace small and large; refuse stale eTags");

    // Move + rename; stale eTag and taken name refused.
    let sub_id = RemoteId(sub.id.clone());
    expect(cloud.move_item(&pre(&a), &sub_id, "moved.bin").await, "moving with a stale eTag", is_changed)?;
    let moved = cloud.move_item(&pre(&a2), &sub_id, "moved.bin").await.map_err(other)?;
    ensure!(moved.parent_id.as_deref() == Some(sub.id.as_str()) && moved.name == "moved.bin", "move went wrong");
    let b2_now = cloud.move_item(&pre(&b2), &top_id, "large-renamed.bin").await.map_err(other)?;
    expect(cloud.move_item(&pre(&b2_now), &sub_id, "moved.bin").await, "moving onto a taken name", is_taken)?;
    step("move and rename; refuse stale eTags and taken names");

    // Deletes: a folder with contents is refused, a stale eTag is refused.
    let sub_now = refetch(graph, &sub).await?;
    expect(cloud.delete(&pre(&sub_now)).await, "deleting a folder that has contents", is_changed)?;
    expect(cloud.delete(&pre(&a2)).await, "deleting with a stale eTag", is_changed)?;
    cloud.delete(&pre(&moved)).await.map_err(other)?;
    let sub_now = refetch(graph, &sub).await?;
    cloud.delete(&pre(&sub_now)).await.map_err(other)?;
    step("delete; refuse non-empty folders and stale eTags");
    Ok(())
}

/// An item's current row: a folder's eTag changes whenever its contents
/// do, so the check re-reads it the way the delta feed would.
async fn refetch(graph: &Graph, row: &RemoteRow) -> Result<RemoteRow> {
    let item = graph.item(&row.id).await?.context("the item vanished")?;
    oneup_remote::ingest::to_row(&item).context("item without a name")
}

async fn upload(
    cloud: &GraphCloud,
    parent: &RemoteId,
    name: &str,
    replace: Option<&RemotePre>,
    bytes: &[u8],
) -> Result<RemoteRow, CloudError> {
    let (row, _) = cloud.upload(parent, name, replace, bytes.len() as i64, &mut &bytes[..]).await?;
    let mut h = oneup_hash::QuickXorHash::new();
    h.update(bytes);
    if row.qxh != Some(h.finalize()) || row.size != bytes.len() as i64 {
        return Err(CloudError::Other(format!("{name}: OneDrive's hash or size doesn't match what was uploaded")));
    }
    Ok(row)
}

fn expect<T>(r: Result<T, CloudError>, what: &str, ok: fn(&CloudError) -> bool) -> Result<()> {
    match r {
        Err(e) if ok(&e) => Ok(()),
        Err(e) => bail!("{what}: refused, but with the wrong error: {e}"),
        Ok(_) => bail!("{what}: OneDrive accepted it, but should have refused"),
    }
}

fn is_taken(e: &CloudError) -> bool {
    matches!(e, CloudError::NameTaken)
}
/// Uploads to a taken name: OneDrive answers 409, which may surface either
/// way depending on the path (simple PUT vs. session).
fn is_taken_any(e: &CloudError) -> bool {
    matches!(e, CloudError::NameTaken | CloudError::Changed)
}
fn is_changed(e: &CloudError) -> bool {
    matches!(e, CloudError::Changed)
}
fn is_changed_any(e: &CloudError) -> bool {
    matches!(e, CloudError::Changed | CloudError::NameTaken)
}

fn stale(p: &RemotePre) -> RemotePre {
    RemotePre { id: p.id.clone(), etag: format!("{}-stale", p.etag) }
}

fn other(e: CloudError) -> anyhow::Error {
    anyhow::anyhow!("{e}")
}

fn step(what: &str) {
    println!("  ok  {what}");
}

/// Deterministic, incompressible-looking test content.
pub(crate) fn content(len: usize, seed: u64) -> Vec<u8> {
    let mut x = seed.wrapping_mul(0x9E37_79B9_7F4A_7C15) | 1;
    (0..len)
        .map(|_| {
            x ^= x << 13;
            x ^= x >> 7;
            x ^= x << 17;
            x as u8
        })
        .collect()
}

pub(crate) fn rand_u32() -> u32 {
    let t = std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).map(|d| d.as_nanos()).unwrap_or(0);
    (t as u32) ^ std::process::id().rotate_left(16)
}
