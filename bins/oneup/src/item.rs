//! One file or folder in a synced folder, as Explorer's commands and the
//! settings window's **Version history and sharing** page see it: its
//! OneDrive item, its versions (list, open a copy, restore one), and
//! sharing links.
//!
//! Restoring a version changes the file in OneDrive: the old version is
//! downloaded and sent as a new version, conditional on the file's current
//! eTag (`If-Match`), and checked by its hash. The version it replaces
//! stays in the history. (Graph's own `restoreVersion` ignores `If-Match`
//! on OneDrive Personal, measured with `oneup dev-remote restore-check`,
//! so it isn't used.) This PC gets the restored content by
//! syncing, through the safety gate as any change from OneDrive. Opening a version writes a copy outside the synced
//! folder. Nothing here works inside an encrypted vault (OneDrive's
//! versions and links there are of encrypted files).

use std::path::{Path, PathBuf};
use std::sync::Arc;

use anyhow::{Context, Result, bail};
use oneup_graph::{Aimd, Graph};
use serde_json::{Value, json};

/// An item in a synced folder.
pub struct Item {
    /// The data folder syncing it.
    pub dir: PathBuf,
    /// Its path inside the synced folder (`/`-separated, no leading `/`).
    pub rel: String,
    /// Its OneDrive item.
    pub remote: String,
    pub folder: bool,
    /// Its local file ID.
    pub local: u128,
}

impl Item {
    /// The item at `path`, which must be synced already.
    pub fn at(path: &Path) -> Result<Item> {
        let (dir, rel) = crate::datafolders::for_path(path)
            .with_context(|| format!("{} isn't in a folder OneUpDrive syncs", path.display()))?;
        Item::in_dir(dir, rel)
    }

    /// The item at `rel` in data folder `dir`'s synced folder.
    pub fn in_dir(dir: PathBuf, rel: String) -> Result<Item> {
        if crate::vaultpair::is_vault_dir(&dir) {
            bail!(
                "this is in an encrypted vault: OneDrive only has its encrypted files, so it can't show versions or share it"
            );
        }
        let store = crate::setup::open_store(&dir, 4 * 1024)?;
        let root = store.read(|o| o.local_root())?.context("no local scan")?;
        // Down the Local tree by name, then to the cloud item it's synced with.
        let mut id = root.root_file_id;
        let mut folder = true;
        for part in rel.split('/').filter(|p| !p.is_empty()) {
            let row = store
                .read(|o| o.local_children(id))?
                .into_iter()
                .find(|c| oneup_sync::same_name(&c.name, part))
                .with_context(|| format!("/{rel} isn't known to OneUpDrive yet"))?;
            folder = row.kind != oneup_store::LocalKind::File;
            id = row.file_id;
        }
        let remote = store
            .read(|o| o.synced_by_local(id))?
            .and_then(|b| b.remote)
            .with_context(|| format!("/{rel} isn't in OneDrive yet"))?;
        Ok(Item { dir, rel, remote: remote.0, folder, local: id })
    }

    /// Whether the file at `path` (this item) is unchanged since it was last
    /// synced: same size and last-write time as its Synced row.
    pub fn unchanged(&self, path: &Path) -> Result<bool> {
        use std::os::windows::fs::MetadataExt as _;
        let store = crate::setup::open_store(&self.dir, 1024)?;
        let base = store.read(|o| o.synced_by_local(self.local))?.context("not synced yet")?;
        drop(store);
        let meta = std::fs::metadata(path)?;
        Ok(i64::try_from(meta.len()).ok() == Some(base.size)
            && i64::try_from(meta.last_write_time()).ok() == Some(base.local_mtime))
    }

    /// Whether its pair is a backup, which puts this PC's version back over
    /// changes made in OneDrive.
    pub fn in_backup(&self) -> Result<bool> {
        let store = crate::setup::open_store(&self.dir, 1024)?;
        Ok(crate::flow::get(&store)? == crate::flow::Flow::Backup)
    }

    pub fn name(&self) -> &str {
        self.rel.rsplit('/').next().unwrap_or(&self.rel)
    }

    /// Graph as the item's pair addresses it (a shared folder's drive).
    pub fn graph(&self) -> Result<Arc<Graph>> {
        let http = crate::setup::http()?;
        let auth = crate::setup::auth(&self.dir, None, &http)?;
        Ok(Arc::new(crate::pairs::scoped(&self.dir, Graph::new(http, auth, Aimd::new(2, 4)))?))
    }
}

/// The item and its versions, for the window.
pub async fn versions_json(path: &Path) -> Result<Value> {
    let it = Item::at(path)?;
    let versions = if it.folder { Vec::new() } else { it.graph()?.versions(&it.remote).await? };
    Ok(json!({
        "name": it.name(),
        "path": format!("/{}", it.rel),
        "folder": it.folder,
        "versions": versions.iter().enumerate().map(|(i, v)| json!({
            "id": v.id,
            "modified_ms": v.modified_ms,
            "size": v.size,
            "by": v.by,
            "current": i == 0,
        })).collect::<Vec<_>>(),
    }))
}

/// Saves version `version` of the file at `path` as a copy outside the
/// synced folder (`%TEMP%\OneUpDrive versions`) and returns where.
pub async fn open_version(path: &Path, version: &str, label: &str) -> Result<PathBuf> {
    let it = Item::at(path)?;
    if it.folder {
        bail!("folders have no versions");
    }
    let dir = std::env::temp_dir().join("OneUpDrive versions");
    std::fs::create_dir_all(&dir)?;
    let name = it.name();
    let (stem, ext) = match name.rfind('.') {
        Some(i) if i > 0 => name.split_at(i),
        _ => (name, ""),
    };
    let clean: String =
        label.chars().map(|c| if c.is_alphanumeric() || c == '-' || c == ' ' { c } else { '-' }).collect();
    let mut to = dir.join(format!("{stem} (version {clean}){ext}"));
    let mut n = 2;
    while to.exists() {
        to = dir.join(format!("{stem} (version {clean}, {n}){ext}"));
        n += 1;
    }
    let mut f = std::fs::OpenOptions::new().write(true).create_new(true).open(&to)?;
    if let Err(e) = it.graph()?.download_version(&it.remote, version, &mut f).await {
        drop(f);
        let _ = std::fs::remove_file(&to);
        return Err(e.into());
    }
    Ok(to)
}

/// Makes version `version` the file's current one in OneDrive (see the
/// module docs).
pub async fn restore_version(path: &Path, version: &str) -> Result<()> {
    use oneup_exec::Cloud as _;
    let it = Item::at(path)?;
    crate::setup::require_test_account(&it.dir)?;
    if it.folder {
        bail!("folders have no versions");
    }
    let graph = it.graph()?;
    let now = graph.item(&it.remote).await?.context("it's no longer in OneDrive")?;
    let etag = now.e_tag.clone().context("OneDrive gave no version tag")?;
    let parent = now.parent_id().context("it has no folder in OneDrive")?.to_string();
    let name = now.name.clone().context("it has no name in OneDrive")?;
    // Downloaded into the data folder (OneUpDrive's own), never the synced one.
    let staged = it.dir.join(format!("restore-{}.tmp", std::process::id()));
    let result = async {
        let mut f = std::fs::OpenOptions::new().read(true).write(true).create(true).truncate(true).open(&staged)?;
        let mut hashing = HashingWrite { inner: &mut f, hash: oneup_hash::QuickXorHash::new() };
        let len = graph.download_version(&it.remote, version, &mut hashing).await?;
        let hash = hashing.hash.finalize();
        std::io::Seek::rewind(&mut f)?;
        let size = i64::try_from(len)?;
        let pre = oneup_sync::RemotePre { id: oneup_sync::RemoteId(it.remote.clone()), etag };
        let cloud = oneup_exec::GraphCloud::new(Arc::clone(&graph));
        let (row, _) = cloud.upload(&oneup_sync::RemoteId(parent), &name, Some(&pre), size, &mut f).await.map_err(
            |e| match e {
                oneup_exec::CloudError::Changed => anyhow::anyhow!("it changed in OneDrive meanwhile; look again"),
                e => anyhow::anyhow!("{e}"),
            },
        )?;
        if row.qxh != Some(hash) || row.size != size {
            bail!("OneDrive's copy doesn't match the version sent; look at the file's versions again");
        }
        Ok(())
    }
    .await;
    let _ = std::fs::remove_file(&staged);
    result
}

/// Hashes what's written through it.
struct HashingWrite<'a> {
    inner: &'a mut std::fs::File,
    hash: oneup_hash::QuickXorHash,
}

impl std::io::Write for HashingWrite<'_> {
    fn write(&mut self, buf: &[u8]) -> std::io::Result<usize> {
        let n = self.inner.write(buf)?;
        self.hash.update(&buf[..n]);
        Ok(n)
    }
    fn flush(&mut self) -> std::io::Result<()> {
        self.inner.flush()
    }
}

/// The Office program for a file name (Microsoft's Office URI schemes),
/// or `None` for files Office doesn't edit with AutoSave.
pub fn office_scheme(name: &str) -> Option<&'static str> {
    let ext = name.rsplit_once('.')?.1.to_ascii_lowercase();
    Some(match ext.as_str() {
        "docx" | "docm" | "dotx" | "dotm" | "doc" | "dot" | "rtf" | "odt" => "ms-word",
        "xlsx" | "xlsm" | "xlsb" | "xltx" | "xltm" | "xls" | "xlt" | "ods" => "ms-excel",
        "pptx" | "pptm" | "potx" | "potm" | "ppsx" | "ppsm" | "ppt" | "pps" | "odp" => "ms-powerpoint",
        _ => return None,
    })
}

/// The address that opens the file at `path` in its Office program, from
/// OneDrive, for editing: Office then saves to OneDrive itself as you work
/// (AutoSave, co-authoring), and this PC gets each saved version by
/// syncing. Office must be signed in with the same Microsoft account.
pub async fn office_uri(path: &Path) -> Result<String> {
    let it = Item::at(path)?;
    office_uri_of(&it).await
}

/// [`office_uri`] for an item already found.
pub async fn office_uri_of(it: &Item) -> Result<String> {
    let scheme = office_scheme(it.name()).context("that isn't a Word, Excel or PowerPoint file")?;
    if it.in_backup()? {
        bail!(
            "this folder is a backup: OneUpDrive puts this PC's version back over changes made in OneDrive, \
             so Office's AutoSave edits there would be undone; open it on this PC instead"
        );
    }
    let dav = it.graph()?.web_dav_url(&it.remote).await?.context("OneDrive has no address for it")?;
    if !dav.starts_with("https://") || dav.contains(['|', '"', ' ']) {
        bail!("unexpected address from OneDrive");
    }
    Ok(format!("{scheme}:ofe|u|{dav}"))
}

/// What Office asks a sync program about a file before opening it from
/// OneDrive (Microsoft's Cloud Storage Partner Program, "sync client
/// integration"; `oneup office-props`, called by the shell extension):
/// its address in OneDrive, whether the file here is the version OneDrive
/// has, and the SHA-256 of its content (Office compares it with its own).
pub struct OfficeProps {
    pub uri: String,
    pub in_sync: bool,
    pub sha256: [u8; 32],
}

pub async fn office_props(path: &Path) -> Result<OfficeProps> {
    use sha2::Digest as _;
    let it = Item::at(path)?;
    if it.folder {
        bail!("a folder");
    }
    let in_sync = it.unchanged(path)?;
    let dav = it.graph()?.web_dav_url(&it.remote).await?.context("OneDrive has no address for it")?;
    let mut h = sha2::Sha256::new();
    std::io::copy(&mut std::fs::File::open(path)?, &mut h)?;
    Ok(OfficeProps { uri: dav, in_sync, sha256: h.finalize().into() })
}

/// Hands an address (`ms-word:…`, `https://…`) to the program Windows has
/// for it.
pub fn launch(uri: &str) -> Result<()> {
    std::process::Command::new("rundll32").args(["url.dll,FileProtocolHandler", uri]).spawn()?;
    Ok(())
}

/// A sharing link for the item at `path`.
pub async fn share(path: &Path, edit: bool) -> Result<String> {
    let it = Item::at(path)?;
    crate::setup::require_test_account(&it.dir)?;
    let link = it.graph()?.share_link(&it.remote, edit).await?;
    if !link.starts_with("https://") {
        bail!("unexpected link from OneDrive");
    }
    Ok(link)
}

#[cfg(test)]
mod tests {
    #[test]
    fn office_files_open_in_their_program() {
        assert_eq!(super::office_scheme("Report.DOCX"), Some("ms-word"));
        assert_eq!(super::office_scheme("a.b.xlsx"), Some("ms-excel"));
        assert_eq!(super::office_scheme("deck.pptx"), Some("ms-powerpoint"));
        assert_eq!(super::office_scheme("notes.txt"), None);
        assert_eq!(super::office_scheme("docx"), None);
    }
}
