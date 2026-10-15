//! Turning Files On-Demand on and off for a folder pair
//! (`docs/architecture/files-on-demand.md` § Turning it on and off). Both
//! need the data folder to themselves (the agent stopped).
//!
//! * **On:** the synced folder is registered as OneUpDrive's sync root
//!   (identity: the pair's token), and every synced folder and every
//!   unchanged synced file becomes a placeholder that **keeps its
//!   content**, in sync. Nothing is dehydrated by turning it on.
//! * **Off:** refused unless the disk has room for every online-only file.
//!   Then each one is downloaded (the hydration service runs in this
//!   process meanwhile), every placeholder is reverted to an ordinary file,
//!   and only then is the sync root unregistered: Windows removes the
//!   placeholders still dehydrated when it is (measured), so
//!   their files would vanish from this PC (they stay in OneDrive).

use std::path::{Path, PathBuf};
use std::sync::Arc;

use anyhow::{Context, Result, bail};
use oneup_cfapi::identity::Identity;
use oneup_cfapi::provider;
use oneup_effects::Version;
use oneup_store::{LocalKind, Store};
use oneup_sync::{LocalPre, NodeKind, ROOT};

use crate::sync::{open_paired, pairing};

/// Whether the pair's folder is OneUpDrive's sync root for this pair.
pub fn is_on(root: &Path, token: &str) -> bool {
    provider::registration(root)
        .is_some_and(|(name, identity)| name == oneup_cfapi::PROVIDER_NAME && identity == token.as_bytes())
}

fn token(store: &Store) -> Result<String> {
    store.read(|o| o.meta_get("sync.token"))?.context("this data folder has no synced folder yet (run `oneup sync`)")
}

/// What turning it on did.
#[derive(Debug, Default)]
pub struct On {
    pub folders: u64,
    pub files: u64,
    /// Synced files that changed since their last sync (converted after
    /// their next upload).
    pub left: u64,
}

pub fn turn_on(store: &Store) -> Result<On> {
    let pair = pairing(store)?;
    let token = token(store)?;
    if let Some(other) = oneup_cfapi::other_sync_root(&pair.local) {
        bail!("{} is managed by {}; OneUpDrive won't take it over", pair.local.display(), other.provider_name);
    }
    if !is_on(&pair.local, &token) {
        crate::syncroot::register(&pair.local, &token, "OneUpDrive")?;
    }
    let fs = open_paired(store)?;
    if !fs.on_demand() {
        bail!("the folder was registered, but isn't recognised as OneUpDrive's; nothing was converted");
    }
    let mut done = On::default();
    // Parents first, where sids allow; what fails for want of a converted
    // parent goes on the next pass.
    for _ in 0..PASSES {
        let before = done.folders + done.files;
        let mut left = 0;
        let mut after = ROOT;
        loop {
            let page = store.read(|o| o.synced_not_placeholders(after, PAGE))?;
            let Some(last) = page.last() else { break };
            after = last.sid;
            for b in &page {
                match convert(&fs, store, b)? {
                    Converted::Folder => done.folders += 1,
                    Converted::File => done.files += 1,
                    Converted::Left => left += 1,
                }
            }
        }
        done.left = left;
        if left == 0 || done.folders + done.files == before {
            break;
        }
    }
    Ok(done)
}

/// Rows read at a time when converting.
const PAGE: usize = 1_000;
/// Passes over what's left at most (a folder's sid can come after its
/// children's).
const PASSES: usize = 3;

/// What [`convert`] did with one synced item.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Converted {
    Folder,
    File,
    /// Not converted: changed since its last sync (converted after its
    /// next upload), or refused (tried again later).
    Left,
}

/// Turns the synced item `b`, already here and not a placeholder, into a
/// placeholder in sync that keeps its content: a folder always, a file
/// only if it's unchanged since its last sync (same size, and the hash on
/// record for exactly its size and time is the synced one; checked again
/// under a share-none handle by `convert_in_sync`).
pub fn convert(fs: &oneup_effects::LocalFs, store: &Store, b: &oneup_sync::Base) -> Result<Converted> {
    let (Some(local), Some(remote)) = (b.local, b.remote.clone()) else { return Ok(Converted::Left) };
    if b.sid == ROOT {
        return Ok(Converted::Left);
    }
    if b.kind == NodeKind::Folder {
        let v = Version {
            identity: Identity { item: remote.0, etag: b.remote_etag.clone(), size: 0, hash: [0; 20] },
            modified: 0,
        };
        let ok = fs.convert_in_sync(&LocalPre { id: local, stamp: None }, &v).is_ok();
        return Ok(if ok { Converted::Folder } else { Converted::Left });
    }
    let Some(row) = store.read(|o| o.local_get(local.0))? else { return Ok(Converted::Left) };
    let hash = store.read(|o| o.local_hash_get(local.0, row.size, row.mtime))?;
    let unchanged = row.kind == LocalKind::File && row.size == b.size && hash.is_some() && hash == b.hash;
    let (true, Some(hash)) = (unchanged, hash) else { return Ok(Converted::Left) };
    let v = Version {
        identity: Identity { item: remote.0, etag: b.remote_etag.clone(), size: row.size, hash },
        modified: row.mtime,
    };
    Ok(match fs.convert_in_sync(&LocalPre { id: local, stamp: Some((row.size, row.mtime)) }, &v) {
        Ok(()) => Converted::File,
        Err(_) => Converted::Left,
    })
}

/// A placeholder found when turning it off.
struct Found {
    path: PathBuf,
    folder: bool,
    depth: usize,
    size: i64,
}

/// What turning it off needs: the placeholders, and how many bytes must be
/// downloaded.
fn placeholders(store: &Store, root: &Path) -> Result<(Vec<Found>, u64)> {
    let local_root = store.read(|o| o.local_root())?.context("no local scan")?;
    let mut out = Vec::new();
    let mut need = 0u64;
    let mut todo = vec![(local_root.root_file_id, root.to_path_buf(), 0usize)];
    while let Some((id, dir, depth)) = todo.pop() {
        for c in store.read(|o| o.local_children(id))? {
            if depth == 0 && oneup_rules::is_engine_dir(&c.name) {
                continue;
            }
            let path = dir.join(&c.name);
            let folder = c.kind == LocalKind::Folder;
            if let Ok(f) = provider::open(&path)
                && let Ok(Some(i)) = provider::info(&f)
            {
                if !folder {
                    need += u64::try_from(c.size - i.on_disk.min(c.size)).unwrap_or(0);
                }
                out.push(Found { path: path.clone(), folder, depth: depth + 1, size: c.size });
            }
            if folder {
                todo.push((c.file_id, path, depth + 1));
            }
        }
    }
    Ok((out, need))
}

/// What turning it off would download, and the disk's free space.
pub fn off_needs(store: &Store) -> Result<(u64, u64)> {
    let pair = pairing(store)?;
    let (_, need) = placeholders(store, &pair.local)?;
    let free = oneup_sys::disk::free_bytes(&pair.local)?;
    Ok((need, free))
}

/// Turns it off (see the module docs). `graph`: for downloading what's
/// online-only. Run on a multi-threaded runtime (the hydration it waits for
/// is served on another of its threads).
pub async fn turn_off(data_dir: &Path, store: Store, graph: Arc<oneup_graph::Graph>) -> Result<u64> {
    let pair = pairing(&store)?;
    let token = token(&store)?;
    if !is_on(&pair.local, &token) {
        bail!("Files On-Demand isn't on for this folder");
    }
    let (found, need) = placeholders(&store, &pair.local)?;
    let free = oneup_sys::disk::free_bytes(&pair.local)?;
    // Some room to spare, so the disk isn't left completely full.
    let margin = 1 << 30;
    if need.saturating_add(margin) > free {
        bail!(
            "turning Files On-Demand off downloads {} of online-only files, but the disk has {} free; \
             free up space first (nothing was changed)",
            crate::versions::size(need),
            crate::versions::size(free)
        );
    }
    // Serve the downloads from this process while it runs.
    let h = oneup_exec::hydrate::Hydrator::new(
        oneup_exec::GraphCloud::new(graph),
        crate::setup::open_store(data_dir, 4 * 1024)?,
        data_dir.join("hydration"),
        &pair.local,
        tokio::runtime::Handle::current(),
        Box::new(|| {}),
    )?;
    let conn = provider::connect(&pair.local, Arc::new(oneup_exec::hydrate::Serve(h)))?;
    let mut files: Vec<&Found> = found.iter().filter(|f| !f.folder).collect();
    files.sort_by_key(|f| f.path.clone());
    let mut downloaded = 0u64;
    for f in files {
        let path = f.path.clone();
        let size = f.size;
        let got = tokio::task::spawn_blocking(move || -> Result<i64> {
            let file = provider::open(&path)?;
            let before = provider::info(&file)?.map_or(size, |i| i.on_disk);
            if before < size {
                provider::hydrate(&file).with_context(|| format!("downloading {}", path.display()))?;
            }
            provider::revert(&file, size).with_context(|| format!("making {} an ordinary file", path.display()))?;
            Ok(size - before)
        })
        .await??;
        downloaded += u64::try_from(got).unwrap_or(0);
    }
    // Folders last, deepest first.
    let mut folders: Vec<&Found> = found.iter().filter(|f| f.folder).collect();
    folders.sort_by_key(|f| std::cmp::Reverse(f.depth));
    for f in folders {
        let file = provider::open(&f.path)?;
        // An excluded folder's mark is never reverted: that removed an
        // online-only file the OneDrive app had left inside one
        // (`LocalFs::unmark_excluded_folder`). Unregistering below makes it
        // an ordinary folder, its contents as they are.
        if provider::info(&file)?.is_some_and(|i| i.identity == oneup_effects::EXCLUDED_MARK) {
            continue;
        }
        provider::revert(&file, 0).with_context(|| format!("making {} an ordinary folder", f.path.display()))?;
    }
    drop(conn);
    crate::syncroot::unregister(&pair.local, &token)?;
    Ok(downloaded)
}
