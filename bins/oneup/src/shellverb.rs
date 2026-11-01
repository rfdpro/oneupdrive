//! `oneup shell-verb VERB PATH` (hidden): what Explorer's OneUpDrive
//! commands do for a file or folder in a synced folder (the shell extension,
//! `crates/oneup-shellext`, only starts this program with the path). The
//! data folder is found from the path (`oneup_app::datafolders`).
//!
//! * `explain`: why the item syncs or not, and `activity`: what sync did
//!   with it lately, on the settings window's Files page.
//! * `history`: the Files page's History section, with activity and recovery copies.
//! * `first`: transfer it before anything else (the agent must be running).
//! * `online`: open it on onedrive.com.
//! * `versions`, `share`: the settings window's "Versions & sharing" page
//!   for it.
//! * `office`: open it in Word, Excel or PowerPoint from OneDrive, with
//!   AutoSave (`oneup_app::item::office_uri`).
//!
//! A problem shows in a message box: there's no console to print it in.

use std::path::Path;
use std::sync::Arc;

use anyhow::{Context, Result, bail};
use oneup_graph::{Aimd, Graph};

pub async fn run(verb: &str, path: &Path) -> Result<()> {
    let result = async {
        let (dir, rel) = oneup_app::datafolders::for_path(path)
            .with_context(|| format!("{} isn't in a folder OneUpDrive syncs", path.display()))?;
        match verb {
            "history" => item_page(&dir, path, "itemhistory"),
            "explain" => item_page(&dir, path, "itemwhy"),
            "activity" => item_page(&dir, path, "itemactivity"),
            "first" => crate::agent::sync_now(&dir, Some(rel.clone()).filter(|r| !r.is_empty())).await,
            "online" => online(&dir, &rel).await,
            "versions" | "share" => item_page(&dir, path, "item"),
            "office" => oneup_app::item::launch(&oneup_app::item::office_uri(path).await?),
            other => bail!("unknown command {other}"),
        }
    }
    .await;
    // Explorer starts this without a console: a problem needs a window.
    if let Err(e) = &result {
        oneup_sys::tray::error_box("OneUpDrive", &format!("{e:#}"));
    }
    result
}

/// The settings window's Files page with `path` selected and one of its
/// sections open: `item` (Versions), `itemhistory`, `itemwhy` or `itemactivity`.
fn item_page(dir: &Path, path: &Path, page: &str) -> Result<()> {
    // Its own main data folder's window (a pair's items show there too).
    let main = oneup_app::vaultpair::main_dir(dir);
    std::process::Command::new(oneup_app::launch::sibling("oneup-settings.exe")?)
        .arg("--data-dir")
        .arg(&main)
        .args(["--page", page, "--item"])
        .arg(path)
        .spawn()
        .context("opening the settings window")?;
    Ok(())
}

/// Opens the item's page on onedrive.com.
async fn online(dir: &Path, rel: &str) -> Result<()> {
    if oneup_app::vaultpair::is_vault_dir(dir) {
        oneup_sys::tray::error_box("OneUpDrive", "This is in a vault: OneDrive's website shows only encrypted files.");
        return Ok(());
    }
    let store = oneup_app::setup::open_store(dir, 4 * 1024)?;
    let root = store.read(|o| o.local_root())?.context("no local scan")?;
    // Down the Local tree by name, then to the cloud item it's synced with.
    let mut id = root.root_file_id;
    for part in rel.split('/').filter(|p| !p.is_empty()) {
        id = store
            .read(|o| o.local_children(id))?
            .into_iter()
            .find(|c| oneup_sync::same_name(&c.name, part))
            .with_context(|| format!("/{rel} isn't known to OneUpDrive yet"))?
            .file_id;
    }
    let remote = store
        .read(|o| o.synced_by_local(id))?
        .and_then(|b| b.remote)
        .with_context(|| format!("/{rel} isn't in OneDrive yet"))?;
    drop(store);
    let http = oneup_app::setup::http()?;
    let graph = Graph::new(http.clone(), oneup_app::setup::auth(dir, None, &http)?, Aimd::new(2, 4));
    let url = Arc::new(graph).web_url(&remote.0).await?.context("OneDrive has no web page for it")?;
    if !url.starts_with("https://") {
        bail!("unexpected address from OneDrive");
    }
    std::process::Command::new("rundll32").args(["url.dll,FileProtocolHandler", &url]).spawn()?;
    Ok(())
}
