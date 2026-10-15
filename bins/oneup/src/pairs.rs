//! More folder pairs under one sign-in (`docs/architecture/pairs.md`):
//! any OneDrive folder with any folder on this PC, a folder someone shared
//! with you, or one of Windows' own folders (Desktop, Documents, ...) as
//! **PC folder backup**.
//!
//! * Each extra pair has its own data folder, `<data folder>\pairs\<name>`,
//!   marked by [`MARKER`], with its own state database, `config.toml`,
//!   agent (`oneupd`), log and lock, as a vault pair has. It signs in as
//!   the main data folder does (`setup::auth` follows
//!   `vaultpair::main_dir`).
//! * Its cloud listing covers only its own folder: its agent's Graph client
//!   is scoped to it ([`scoped`], `Graph::scoped`), so the folder stands in
//!   for the drive's root and the change feed is folder-scoped. For a
//!   shared folder that's the only way to list it at all (it lives in the
//!   other person's drive).
//! * A pair syncs both ways or, as a **backup**, one way
//!   (`crate::flow`): Windows' folders are offered as backups by default.
//! * Overlaps are refused: no two pairs share local folders, and within
//!   the account's own drive no pair's OneDrive folder is inside another
//!   extra pair's. One inside the main pair's OneDrive folder is left to
//!   its own pair by a rule the main pair gets (`rulefiles::add_separate`).
//! * Nothing is moved or redirected: PC folder backup syncs Windows'
//!   folders where they are (the OneDrive app instead moves them into its
//!   folder and redirects Windows there).

use std::path::{Path, PathBuf};
use std::sync::Arc;

use anyhow::{Context, Result, bail};
use oneup_graph::Graph;

use crate::flow::Flow;

/// In an extra pair's data folder: what its OneDrive side is, for people.
pub const MARKER: &str = "folder-pair.txt";
/// Where extra pairs' data folders are, inside the main data folder.
pub const PAIRS: &str = "pairs";
/// Meta keys in an extra pair's store.
pub const KEY_KIND: &str = "pair.kind";
/// The drive holding a shared folder (absent: the account's own).
pub const KEY_DRIVE: &str = "pair.drive";
/// The OneDrive folder's ID.
pub const KEY_ROOT: &str = "pair.root";

pub fn is_pair_dir(dir: &Path) -> bool {
    dir.join(MARKER).is_file()
}

/// What an extra pair syncs.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Kind {
    /// A folder of the account's own OneDrive.
    Folder,
    /// A folder someone shared, in their drive.
    Shared { drive: String, owner: Option<String> },
    /// One of Windows' folders, where it is (PC folder backup).
    Known(oneup_sys::known::Known),
}

impl Kind {
    fn encode(&self) -> String {
        match self {
            Kind::Folder => "folder".into(),
            Kind::Shared { owner, .. } => format!("shared:{}", owner.as_deref().unwrap_or_default()),
            Kind::Known(k) => format!("known:{}", k.name()),
        }
    }

    fn decode(s: &str, drive: Option<String>) -> Kind {
        if let Some(k) = s.strip_prefix("known:").and_then(oneup_sys::known::Known::parse) {
            return Kind::Known(k);
        }
        match (s.strip_prefix("shared:"), drive) {
            (Some(owner), Some(drive)) => {
                Kind::Shared { drive, owner: Some(owner.to_string()).filter(|o| !o.is_empty()) }
            }
            _ => Kind::Folder,
        }
    }

    /// For lists: "Folder", "Shared by Sam", "Documents (PC folder backup)".
    pub fn label(&self) -> String {
        match self {
            Kind::Folder => "OneDrive folder".into(),
            Kind::Shared { owner: Some(o), .. } => format!("shared by {o}"),
            Kind::Shared { owner: None, .. } => "shared with you".into(),
            Kind::Known(k) => format!("{} (PC folder backup)", k.name()),
        }
    }
}

/// An extra pair of a main data folder.
#[derive(Debug, Clone)]
pub struct Entry {
    pub name: String,
    pub dir: PathBuf,
    /// The OneDrive side, for people: a path in the account's own drive, or
    /// a shared folder's name.
    pub remote: String,
}

/// The extra pairs of the main data folder `main`.
pub fn list(main: &Path) -> Vec<Entry> {
    let mut out: Vec<Entry> = std::fs::read_dir(main.join(PAIRS))
        .into_iter()
        .flatten()
        .flatten()
        .filter_map(|e| {
            let dir = e.path();
            let remote = std::fs::read_to_string(dir.join(MARKER)).ok()?;
            Some(Entry { name: e.file_name().to_string_lossy().into_owned(), dir, remote: remote.trim().to_string() })
        })
        .collect();
    out.sort_by_key(|e| e.name.to_lowercase());
    out
}

/// Finds a pair by its name or its OneDrive side.
pub fn find(main: &Path, name: &str) -> Result<Entry> {
    list(main)
        .into_iter()
        .find(|e| e.name.eq_ignore_ascii_case(name) || e.remote.eq_ignore_ascii_case(name))
        .with_context(|| format!("no folder pair called {name} here (`oneup pair list`)"))
}

/// Everything about an extra pair a list shows.
#[derive(Debug, Clone)]
pub struct Described {
    pub local: PathBuf,
    pub kind: Kind,
    pub flow: Flow,
    pub on_demand: bool,
}

pub fn describe(e: &Entry) -> Result<Described> {
    let store = crate::setup::open_store(&e.dir, 1024)?;
    let get = |k: &str| store.read(|o| o.meta_get(k));
    let local = PathBuf::from(get(crate::sync::KEY_LOCAL)?.unwrap_or_default());
    let token = get(crate::sync::KEY_TOKEN)?.unwrap_or_default();
    let kind = Kind::decode(&get(KEY_KIND)?.unwrap_or_default(), get(KEY_DRIVE)?);
    Ok(Described { on_demand: crate::ondemand::is_on(&local, &token), local, kind, flow: crate::flow::get(&store)? })
}

/// For an extra pair's data folder `dir`: `graph` scoped to its OneDrive
/// folder. Anything else: `graph` as it is.
pub fn scoped(dir: &Path, graph: Graph) -> Result<Graph> {
    if !is_pair_dir(dir) {
        return Ok(graph);
    }
    let store = crate::setup::open_store(dir, 1024)?;
    let root = store.read(|o| o.meta_get(KEY_ROOT))?.context("this folder pair has no OneDrive folder recorded")?;
    let drive = store.read(|o| o.meta_get(KEY_DRIVE))?;
    Ok(graph.scoped(drive.as_deref(), &root))
}

/// A data folder name for a pair, not yet used.
fn new_name(main: &Path, label: &str) -> String {
    let base: String =
        label.chars().map(|c| if c.is_alphanumeric() || c == '-' || c == '_' { c } else { '-' }).take(40).collect();
    let base = base.trim_matches('-').to_string();
    let base = if base.is_empty() { "pair".to_string() } else { base };
    let taken = list(main);
    (1..)
        .map(|n| if n == 1 { base.clone() } else { format!("{base}-{n}") })
        .find(|n| taken.iter().all(|e| !e.name.eq_ignore_ascii_case(n)) && !main.join(PAIRS).join(n).exists())
        .unwrap_or(base)
}

// ----------------------------------------------------------- overlaps

/// Every folder on this PC that a pair of `main` syncs (the main pair's,
/// vaults', extra pairs'), with what syncs it; `except` is left out.
pub fn local_folders(main: &Path, except: Option<&Path>) -> Result<Vec<(PathBuf, String)>> {
    let mut out = Vec::new();
    let mut add = |dir: &Path, what: String| -> Result<()> {
        if except.is_some_and(|x| crate::test_account::same_dir(x, dir)) || !dir.join("state.db").exists() {
            return Ok(());
        }
        if let Some(l) = crate::setup::open_store(dir, 1024)?.read(|o| o.meta_get(crate::sync::KEY_LOCAL))? {
            out.push((PathBuf::from(l), what));
        }
        Ok(())
    };
    add(main, "your main synced folder".into())?;
    for v in crate::vaultpair::list(main) {
        add(&v.dir, format!("the vault {}", v.name))?;
    }
    for p in list(main) {
        add(&p.dir, format!("the folder pair {}", p.name))?;
    }
    Ok(out)
}

/// Refuses `folder` if a pair of `main` already syncs it, a folder in it,
/// or a folder around it.
pub fn check_local(main: &Path, folder: &Path, except: Option<&Path>) -> Result<()> {
    for (l, what) in local_folders(main, except)? {
        let l = std::fs::canonicalize(&l).unwrap_or(l);
        if oneup_local::coexist::is_within(folder, &l) || oneup_local::coexist::is_within(&l, folder) {
            bail!("{} overlaps {} ({}); pick a folder apart from it", folder.display(), l.display(), what);
        }
    }
    Ok(())
}

/// `a` is `b` or inside it, as OneDrive paths (case-insensitive).
fn remote_within(a: &str, b: &str) -> bool {
    let (a, b) = (a.trim_end_matches('/').to_lowercase(), b.trim_end_matches('/').to_lowercase());
    a == b || b.is_empty() || a.starts_with(&format!("{b}/"))
}

/// Refuses OneDrive folder `remote` (in the account's own drive) if it
/// holds the main pair's folder, a vault, or another extra pair's folder,
/// or is inside one of the latter two. Inside the main pair's folder is
/// fine: the main pair then leaves it to its own pair.
pub fn check_remote(main: &Path, remote: &str, except: Option<&Path>) -> Result<()> {
    let store = crate::setup::open_store(main, 1024)?;
    if let Some(m) = store.read(|o| o.meta_get(crate::sync::KEY_REMOTE))?
        && remote_within(&m, remote)
    {
        bail!("OneDrive {remote} holds your main synced folder ({m}); pick a folder inside it or apart from it");
    }
    for v in crate::vaultpair::list(main) {
        if remote_within(&v.remote, remote) || remote_within(remote, &v.remote) {
            bail!("OneDrive {remote} overlaps the vault {} ({}); pick a folder apart from it", v.name, v.remote);
        }
    }
    for p in list(main) {
        if except.is_some_and(|x| crate::test_account::same_dir(x, &p.dir)) {
            continue;
        }
        let own = crate::setup::open_store(&p.dir, 1024)?.read(|o| o.meta_get(KEY_DRIVE))?.is_none();
        if own && (remote_within(&p.remote, remote) || remote_within(remote, &p.remote)) {
            bail!("OneDrive {remote} overlaps the folder pair {} ({}); pick a folder apart from it", p.name, p.remote);
        }
    }
    Ok(())
}

/// The OneDrive folders of `main`'s extra pairs in the account's own drive,
/// as paths (what the main pair leaves alone; `rulefiles::add_separate`).
pub fn own_drive_folders(main: &Path) -> Vec<String> {
    list(main)
        .into_iter()
        .filter(|p| {
            crate::setup::open_store(&p.dir, 1024)
                .and_then(|s| Ok(s.read(|o| o.meta_get(KEY_DRIVE))?.is_none()))
                .unwrap_or(false)
        })
        .map(|p| p.remote)
        .collect()
}

// ------------------------------------------------------- adding a pair

/// Which OneDrive folder a new pair syncs.
#[derive(Debug, Clone)]
pub enum Cloud {
    /// A path in the account's own drive (created if missing).
    Path(String),
    /// A folder someone shared ([`Graph::shared_folders`]).
    Shared(oneup_graph::SharedFolder),
}

/// What a new pair is.
#[derive(Debug, Clone)]
pub struct Request {
    pub cloud: Cloud,
    pub local: PathBuf,
    pub flow: Flow,
    pub on_demand: bool,
    /// Set for PC folder backup (the local folder is then Windows' own).
    pub known: Option<oneup_sys::known::Known>,
}

/// For PC folder backup: the request that backs Windows' folder `k` up
/// into OneDrive folder `/<name>` (as the OneDrive app names them).
pub fn known_request(k: oneup_sys::known::Known, flow: Flow) -> Result<Request> {
    let local = k.path().with_context(|| format!("Windows doesn't say where your {} folder is", k.name()))?;
    Ok(Request { cloud: Cloud::Path(format!("/{}", k.name())), local, flow, on_demand: false, known: Some(k) })
}

/// The OneDrive folder at `path` in the account's own drive, made if
/// missing (each level with `conflictBehavior=fail`, so nothing that exists
/// is ever replaced).
pub(crate) async fn ensure_folder(graph: &Graph, path: &str) -> Result<oneup_graph::DriveItem> {
    if let Some(it) = graph.item_by_path(path).await? {
        if !it.is_folder() {
            bail!("OneDrive {path} is a file, not a folder");
        }
        return Ok(it);
    }
    let mut parent = graph.root().await?;
    let mut so_far = String::new();
    for part in path.split('/').filter(|s| !s.is_empty()) {
        so_far = format!("{so_far}/{part}");
        parent = match graph.item_by_path(&so_far).await? {
            Some(it) if it.is_folder() => it,
            Some(_) => bail!("OneDrive {so_far} is a file, not a folder"),
            None => match graph.create_folder(&parent.id, part).await {
                Ok(it) => it,
                // Made meanwhile (another device): use it.
                Err(e) if e.status() == Some(409) => {
                    graph.item_by_path(&so_far).await?.with_context(|| format!("OneDrive {so_far} vanished"))?
                }
                Err(e) => return Err(e.into()),
            },
        };
    }
    Ok(parent)
}

/// Pairs a folder on this PC with a OneDrive folder in a new data folder of
/// `main`. Both may already hold files: the first round matches equal
/// files and keeps both copies of any that differ, as a first sync always
/// does. Nothing is transferred here; the pair's agent does that.
pub async fn add(main: &Path, http: reqwest::Client, auth: Arc<oneup_graph::Auth>, req: &Request) -> Result<Entry> {
    crate::setup::require_test_account(main)?;
    if crate::vaultpair::is_child_dir(main) {
        bail!("give the main data folder, not a vault's or a folder pair's");
    }
    let flow = req.flow;
    let on_demand = req.on_demand && flow == Flow::TwoWay;

    // ---- the local folder: there, not a drive's top, not anyone else's
    std::fs::create_dir_all(&req.local).with_context(|| format!("creating {}", req.local.display()))?;
    let local = std::fs::canonicalize(&req.local)?;
    if local.components().count() < 3 {
        bail!("refusing a drive's top folder; pick a folder inside it");
    }
    if crate::test_account::same_dir(&local, main) || local.starts_with(std::fs::canonicalize(main)?) {
        bail!("that's inside OneUpDrive's own data folder");
    }
    if let Some(other) = oneup_cfapi::other_sync_root(&local) {
        bail!(
            "{} is managed by {}; OneUpDrive won't sync it{}",
            local.display(),
            other.provider_name,
            if req.known.is_some() { " (it's backed up by that app already)" } else { "" }
        );
    }
    check_local(main, &local, None)?;

    // ---- the OneDrive folder
    let plain = Graph::new(http.clone(), Arc::clone(&auth), oneup_graph::Aimd::new(4, 8));
    let (root, drive, remote, kind) = match &req.cloud {
        Cloud::Path(p) => {
            let p = crate::sync::normalize_remote(p)?;
            check_remote(main, &p, None)?;
            let it = ensure_folder(&plain, &p).await?;
            let kind = req.known.map_or(Kind::Folder, Kind::Known);
            (it.id, None, p, kind)
        }
        Cloud::Shared(s) => {
            let label = match &s.owner {
                Some(o) => format!("{} (shared by {o})", s.name),
                None => format!("{} (shared)", s.name),
            };
            if list(main).iter().any(|e| e.remote.eq_ignore_ascii_case(&label)) {
                bail!("{label} is already synced here");
            }
            (
                s.id.clone(),
                Some(s.drive_id.clone()),
                label,
                Kind::Shared { drive: s.drive_id.clone(), owner: s.owner.clone() },
            )
        }
    };

    // ---- the pair's data folder
    let name = new_name(main, remote.rsplit('/').find(|s| !s.is_empty()).unwrap_or(&remote));
    let dir = main.join(PAIRS).join(&name);
    std::fs::create_dir_all(&dir)?;
    let token = crate::sync::random_token();
    let had_engine_dir = local.join(oneup_rules::ENGINE_DIR).exists();
    let made = async {
        // Same rules as the main pair to start with (its presets and own
        // rules); each pair's config.toml is its own from here on.
        if main.join(crate::config::FILE).is_file() {
            std::fs::copy(main.join(crate::config::FILE), dir.join(crate::config::FILE))?;
        }
        let preset = crate::setup::open_store(main, 1024)?
            .read(|o| o.meta_get(crate::setup::KEY_PRESET))?
            .unwrap_or_else(|| "none".into());
        let mut store = crate::setup::open_store(&dir, 4 * 1024)?;
        store.write(|o| {
            o.meta_set(crate::setup::KEY_PRESET, &preset)?;
            o.meta_set(KEY_KIND, &kind.encode())?;
            o.meta_set(KEY_ROOT, &root)?;
            if let Some(d) = &drive {
                o.meta_set(KEY_DRIVE, d)?;
            }
            Ok(())
        })?;
        crate::flow::set(&mut store, flow)?;
        let rules = crate::setup::preset_rules(&preset).unwrap_or_else(oneup_rules::ExcludeSet::none);
        let cfg = oneup_local::Config::new(rules.clone());
        let mut scan = oneup_local::Scan::open(store, &local, cfg)?;
        if let Some(other) = scan.other_engine() {
            bail!("{} is managed by {}; OneUpDrive won't sync it", local.display(), other.label());
        }
        let summary = tokio::task::spawn_blocking(move || scan.run().map(|s| (s, scan))).await??;
        if !summary.0.finished {
            bail!("the folder's scan was interrupted");
        }
        let store = summary.1.into_store();
        let graph = Arc::new(plain.scoped(drive.as_deref(), &root));
        let mut store = crate::sync::mirror(&graph, store, &rules).await?;
        let lroot = store.read(|o| o.local_root())?.context("no local scan")?;
        let rroot = store.read(|o| o.remote_root())?.context("the OneDrive folder wasn't listed")?;
        if !store.write(|o| oneup_exec::link_roots(o, lroot.root_file_id, &rroot.id))? {
            bail!("the new data folder isn't empty");
        }
        if on_demand {
            crate::syncroot::register(&local, &token, &format!("OneUpDrive {}", remote.trim_start_matches('/')))?;
        }
        store.write(|o| {
            o.meta_set(crate::sync::KEY_LOCAL, &local.to_string_lossy())?;
            o.meta_set(crate::sync::KEY_REMOTE, &remote)?;
            o.meta_set(crate::sync::KEY_TOKEN, &token)
        })?;
        oneup_effects::LocalFs::open(Path::new(&lroot.path), lroot.volume_serial, lroot.root_file_id, &token, true)
            .context("opening the folder")?;
        store.write(|o| o.meta_set(crate::sync::KEY_MARKER, "1"))?;
        if on_demand {
            // Files already here stay as they are; what comes from OneDrive
            // arrives online-only.
            crate::ondemand::turn_on(&store)?;
        }
        // Last: until it's there, this isn't a folder pair.
        std::fs::write(dir.join(MARKER), &remote)?;
        Ok(())
    }
    .await;
    if let Err(e) = made {
        // Our own half-made data folder and registration, and our own
        // folder in the local one if this made it; the files there stay.
        if on_demand {
            let _ = crate::syncroot::unregister(&local, &token);
        }
        let _ = std::fs::remove_dir_all(&dir);
        if !had_engine_dir {
            let _ = std::fs::remove_dir_all(local.join(oneup_rules::ENGINE_DIR));
        }
        return Err(e);
    }
    Ok(Entry { name, dir, remote })
}

/// Removes the pair (its agent must be stopped): the folder stops being
/// synced and the pair's data folder goes. Both folders keep what they
/// hold; online-only files leave the folder (Windows removes them; they're
/// in OneDrive).
pub fn remove(e: &Entry) -> Result<Described> {
    let d = describe(e)?;
    if d.on_demand {
        let token = crate::setup::open_store(&e.dir, 1024)?
            .read(|o| o.meta_get(crate::sync::KEY_TOKEN))?
            .context("pairing token missing")?;
        crate::syncroot::unregister(&d.local, &token).context("releasing the folder")?;
    }
    std::fs::remove_dir_all(&e.dir).with_context(|| format!("removing {}", e.dir.display()))?;
    Ok(d)
}

/// Stops the main data folder `dir` syncing its folder, as [`remove`] does
/// for an extra pair: the folder is released (Windows removes only the
/// online-only files, which are in OneDrive, and keeps every other file as
/// an ordinary one), then the pairing is forgotten (OneUpDrive's own
/// database for it). The sign-in, settings, vaults and extra pairs stay;
/// the settings window offers setting up again. Run with the agent stopped.
/// Returns the folder that was synced.
pub fn unpair(dir: &Path) -> Result<PathBuf> {
    let store = crate::setup::open_store(dir, 1024)?;
    let (local, token) = (
        store.read(|o| o.meta_get(crate::sync::KEY_LOCAL))?.context("this folder doesn't sync anything")?,
        store.read(|o| o.meta_get(crate::sync::KEY_TOKEN))?.unwrap_or_default(),
    );
    drop(store);
    let local = PathBuf::from(local);
    if crate::ondemand::is_on(&local, &token) {
        crate::syncroot::unregister(&local, &token).context("releasing the folder")?;
    }
    // Ours (the data folder's database), not user content.
    for name in ["state.db", "state.db-wal", "state.db-shm"] {
        match std::fs::remove_file(dir.join(name)) {
            Err(e) if e.kind() != std::io::ErrorKind::NotFound => {
                return Err(e).with_context(|| format!("removing {name}"));
            }
            _ => {}
        }
    }
    Ok(local)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn kinds_round_trip() {
        for k in [
            Kind::Folder,
            Kind::Known(oneup_sys::known::Known::Pictures),
            Kind::Shared { drive: "d1".into(), owner: Some("Sam".into()) },
            Kind::Shared { drive: "d1".into(), owner: None },
        ] {
            let drive = match &k {
                Kind::Shared { drive, .. } => Some(drive.clone()),
                _ => None,
            };
            assert_eq!(Kind::decode(&k.encode(), drive), k);
        }
        assert_eq!(Kind::decode("shared:Sam", None), Kind::Folder, "a shared folder needs its drive");
    }

    #[test]
    fn remote_folders_nest_case_insensitively() {
        assert!(remote_within("/Photos/2024", "/photos"));
        assert!(remote_within("/Photos", "/Photos/"));
        assert!(!remote_within("/Photos2", "/Photos"));
        assert!(remote_within("/anything", ""), "the whole drive holds everything");
    }
}
