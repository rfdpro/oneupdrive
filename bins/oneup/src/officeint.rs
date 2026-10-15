//! **Experimental** Office integration: telling Microsoft Office that files
//! in OneUpDrive's folders are in OneDrive, so that double-clicking one may
//! open it from OneDrive with AutoSave and co-authoring, as with the OneDrive
//! app.
//!
//! Microsoft documents how Office finds a third-party sync program (its Cloud
//! Storage Partner Program, "sync client integration"): a key under
//! `HKCU\Software\SyncEngines\Providers\<engine>` names a COM handler and a
//! hash algorithm, and a sub-key per synced folder names its `MountPoint`.
//! For a file under a mount point, Office asks the handler
//! (`IStorageProviderHandler`, served by `oneup_shell.dll`, which runs
//! `oneup office-props`) for the file's address, whether it's in sync, and its
//! hash. What's *not* documented is Office opening a OneDrive address that a
//! third party hands it (the documented case is a partner's WOPI service,
//! with an ID Microsoft assigns); the sub-key also carries `UrlNamespace`,
//! as the OneDrive app's own entry does. Whether Office then turns AutoSave
//! on is what trying it shows. Off unless turned on; turning it off removes
//! everything it wrote.

use std::path::Path;
use std::sync::Arc;

use anyhow::{Context, Result};

/// Where Office looks for sync programs.
const KEY: &str = r"Software\SyncEngines\Providers\OneUpDrive";
/// The handler's class (`oneup_shell::CLSID_OFFICE`).
const HANDLER: &str = "{7D3E5C1A-0B5E-4F6B-9A41-6F2C1D8E9A30}";

pub fn is_on(_main: &Path) -> bool {
    oneup_sys::user_registry::get(KEY, "Handler").ok().flatten().is_some()
}

/// The synced folders it applies to: the main one and the extra pairs (not
/// vaults: OneDrive only has their encrypted files).
fn folders(main: &Path) -> Vec<std::path::PathBuf> {
    let mut out = vec![main.to_path_buf()];
    out.extend(crate::pairs::list(main).into_iter().map(|p| p.dir));
    out
}

/// Writes the keys for every synced folder of `main`; returns how many.
pub async fn turn_on(main: &Path) -> Result<usize> {
    if !crate::config::load_experimental(main)? {
        anyhow::bail!(
            "the Office integration is experimental: turn on experimental options first (`oneup settings --experimental on`)"
        );
    }
    crate::setup::require_test_account(main)?;
    let mut n = 0;
    oneup_sys::user_registry::set(KEY, "Handler", HANDLER)?;
    oneup_sys::user_registry::set(KEY, "HashAlgorithm", "SHA256")?;
    oneup_sys::user_registry::set(KEY, "DisplayNameResource", "OneUpDrive")?;
    for dir in folders(main) {
        let store = crate::setup::open_store(&dir, 1024)?;
        let (Some(local), Some(token)) =
            (store.read(|o| o.meta_get(crate::sync::KEY_LOCAL))?, store.read(|o| o.meta_get(crate::sync::KEY_TOKEN))?)
        else {
            continue;
        };
        let root = store.read(|o| o.local_root())?.context("no local scan")?;
        let remote = store.read(|o| o.synced_by_local(root.root_file_id))?.and_then(|b| b.remote);
        drop(store);
        let local = local.strip_prefix(r"\\?\").unwrap_or(&local).to_string();
        let sub = format!(r"{KEY}\{}", &token[..token.len().min(16)]);
        oneup_sys::user_registry::set(&sub, "MountPoint", &local)?;
        if let Some(r) = remote {
            let http = crate::setup::http()?;
            let auth = crate::setup::auth(&dir, None, &http)?;
            let graph = Arc::new(crate::pairs::scoped(
                &dir,
                oneup_graph::Graph::new(http, auth, oneup_graph::Aimd::new(2, 4)),
            )?);
            if let Some(dav) = graph.web_dav_url(&r.0).await? {
                oneup_sys::user_registry::set(&sub, "UrlNamespace", &format!("{}/", dav.trim_end_matches('/')))?;
            }
        }
        n += 1;
    }
    Ok(n)
}

pub fn turn_off(_main: &Path) -> Result<()> {
    oneup_sys::user_registry::remove_key(KEY)?;
    Ok(())
}
