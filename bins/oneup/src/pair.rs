//! `oneup pair …`: more folder pairs under this sign-in (any OneDrive
//! folder, a folder shared with you, PC folder backup), and `oneup flow`
//! (both ways or backup). The work is in `oneup_app::pairs` and
//! `oneup_app::flow`.

use std::path::Path;
use std::sync::Arc;

use anyhow::{Context, Result, bail};
use oneup_app::flow::{self, Flow};
use oneup_app::pairs::{self, Cloud, Request};

pub struct Add<'a> {
    pub cloud: Cloud,
    pub folder: &'a Path,
    pub backup: bool,
    pub on_demand: bool,
    pub start: bool,
    pub known: Option<oneup_sys::known::Known>,
}

pub async fn add(main: &Path, http: reqwest::Client, auth: Arc<oneup_graph::Auth>, a: Add<'_>) -> Result<()> {
    if !auth.is_signed_in()? {
        bail!("not signed in; run `oneup login --test-account` first");
    }
    let req = Request {
        cloud: a.cloud,
        local: a.folder.to_path_buf(),
        flow: if a.backup { Flow::Backup } else { Flow::TwoWay },
        on_demand: a.on_demand,
        known: a.known,
    };
    let e = pairs::add(main, http, auth, &req).await?;
    let d = pairs::describe(&e)?;
    println!("Paired {} with OneDrive {} as \"{}\" ({}).", d.local.display(), e.remote, e.name, flow::name(d.flow));
    if d.on_demand {
        println!("Files from OneDrive arrive online-only and download when opened.");
    }
    if a.start {
        crate::agent::start(&e.dir).await
    } else {
        println!("`oneup start` starts syncing it (with everything else).");
        Ok(())
    }
}

/// `oneup pair backup-pc FOLDER…`: Windows' own folders, each a pair.
pub async fn backup_pc(
    main: &Path,
    http: reqwest::Client,
    auth: Arc<oneup_graph::Auth>,
    folders: &[String],
    two_way: bool,
    start: bool,
) -> Result<()> {
    let flow = if two_way { Flow::TwoWay } else { Flow::Backup };
    for name in folders {
        let k = oneup_sys::known::Known::parse(name)
            .with_context(|| format!("{name} isn't one of Desktop, Documents, Pictures, Music, Videos"))?;
        let req = pairs::known_request(k, flow)?;
        let a = Add { cloud: req.cloud, folder: &req.local, backup: !two_way, on_demand: false, start, known: Some(k) };
        add(main, http.clone(), Arc::clone(&auth), a).await.with_context(|| format!("backing up {}", k.name()))?;
    }
    Ok(())
}

pub async fn list(main: &Path) -> Result<()> {
    let all = pairs::list(main);
    if all.is_empty() {
        println!(
            "No extra folder pairs. `oneup pair add /Path/In/OneDrive C:\\Some\\Folder` adds one; \
             `oneup pair backup-pc Documents` backs a Windows folder up."
        );
        return Ok(());
    }
    for e in all {
        let d = pairs::describe(&e)?;
        let running = crate::agent::current(&e.dir).await.ok().flatten().is_some();
        println!("{}", e.name);
        println!("  {}  <->  OneDrive {}", d.local.display(), e.remote);
        println!(
            "  {}; {}; {}{}",
            d.kind.label(),
            flow::name(d.flow),
            if running { "syncing" } else { "not running" },
            if d.on_demand { "; online-only until opened" } else { "" }
        );
    }
    Ok(())
}

/// `oneup pair shared`: what others shared with this account.
pub async fn shared(graph: &oneup_graph::Graph) -> Result<()> {
    let found = graph.shared_folders().await.map_err(|e| match e.status() {
        Some(401 | 403) => anyhow::anyhow!(
            "OneDrive didn't allow listing what's shared with you: that needs the permission to read files \
             others share (`oneup login --shared-folders` asks for it)"
        ),
        _ => e.into(),
    })?;
    if found.is_empty() {
        println!("Nobody has shared a folder with this account.");
    }
    for f in found {
        println!("{}{}", f.name, f.owner.map(|o| format!("  (shared by {o})")).unwrap_or_default());
    }
    Ok(())
}

/// The shared folder called `name` (as `oneup pair shared` lists it).
pub async fn find_shared(graph: &oneup_graph::Graph, name: &str) -> Result<oneup_graph::SharedFolder> {
    let found = graph.shared_folders().await?;
    let mut hits: Vec<_> = found.into_iter().filter(|f| oneup_sync::same_name(&f.name, name)).collect();
    match hits.len() {
        0 => bail!("no folder called {name} is shared with you (`oneup pair shared` lists them)"),
        1 => Ok(hits.remove(0)),
        _ => bail!("several folders called {name} are shared with you; the settings window lets you pick one"),
    }
}

pub async fn remove(main: &Path, name: &str, yes: bool) -> Result<()> {
    let e = pairs::find(main, name)?;
    if crate::agent::current(&e.dir).await.ok().flatten().is_some() {
        bail!("{} is syncing; stop it first (`oneup --data-dir \"{}\" stop`)", e.name, e.dir.display());
    }
    let d = pairs::describe(&e)?;
    if !yes {
        println!("This stops syncing {} with OneDrive {}.", d.local.display(), e.remote);
        println!("Both keep what they hold now.");
        if d.on_demand {
            println!("Online-only files leave the folder on this PC (they're in OneDrive).");
        }
        println!("Run again with --yes to do it.");
        return Ok(());
    }
    pairs::remove(&e)?;
    println!("{} is no longer synced; {} stays as it is now.", e.name, d.local.display());
    Ok(())
}

/// `oneup unpair`: the main folder stops syncing ([`pairs::unpair`]).
pub async fn unpair(dir: &Path, yes: bool) -> Result<()> {
    if crate::agent::current(dir).await.ok().flatten().is_some() {
        bail!("OneUpDrive is syncing; stop it first (`oneup stop`)");
    }
    let store = oneup_app::setup::open_store(dir, 1024)?;
    let pair = oneup_app::sync::pairing(&store).context("this data folder doesn't sync anything")?;
    let token = store.read(|o| o.meta_get("sync.token"))?.unwrap_or_default();
    drop(store);
    if !yes {
        println!("This stops syncing {} with OneDrive {}.", pair.local.display(), pair.remote);
        println!("Both keep what they hold now; your sign-in and settings stay.");
        if oneup_app::ondemand::is_on(&pair.local, &token) {
            println!("Online-only files leave the folder on this PC (they're in OneDrive).");
        }
        println!("Run again with --yes to do it.");
        return Ok(());
    }
    // Takes the data folder's lock itself.
    let local = pairs::unpair(dir).context("stop the background agent first (`oneup stop`)")?;
    println!("{} is no longer synced; it stays as it is now.", local.display());
    Ok(())
}

/// `oneup flow [both|backup]` for a data folder (the main one, or a pair's
/// with `--data-dir`).
pub fn set_flow(dir: &Path, to: Option<Flow>) -> Result<()> {
    let mut store = oneup_app::setup::open_store(dir, 4 * 1024)?;
    let Some(to) = to else {
        println!("Changes travel {}.", flow::name(flow::get(&store)?));
        return Ok(());
    };
    oneup_app::setup::require_test_account(dir)?;
    let _lock = oneup_app::setup::DirLock::acquire(dir)
        .context("stop the background agent first (`oneup stop`), then change this")?;
    if to == Flow::Backup {
        let token = store.read(|o| o.meta_get("sync.token"))?.unwrap_or_default();
        if let Ok(pair) = oneup_app::sync::pairing(&store)
            && oneup_app::ondemand::is_on(&pair.local, &token)
        {
            bail!("a backup keeps every file on this PC; turn Files On-Demand off first (`oneup on-demand off`)");
        }
    }
    let released = flow::set(&mut store, to)?;
    println!("Changes now travel {}.", flow::name(to));
    match to {
        Flow::Backup => println!(
            "What you add or change here goes to OneDrive; deleting here never deletes there, and nothing from \
             OneDrive changes this PC."
        ),
        Flow::TwoWay if released > 0 => println!(
            "{released} items the backup kept in OneDrive after you deleted them here come back to this PC \
             (delete them again to remove them from OneDrive too)."
        ),
        Flow::TwoWay => {}
    }
    Ok(())
}
