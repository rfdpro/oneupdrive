//! Vault pairs: a Cryptomator vault in OneDrive kept as a decrypted folder
//! on this PC (`docs/architecture/vaults.md` § Vault pairs).
//!
//! * A vault pair has its own data folder, `<data folder>\vaults\<name>`,
//!   marked by [`MARKER`]. It uses the main data folder's sign-in and cloud
//!   listing: the vault's encrypted items are read from the main listing
//!   (which the vault agent brings up to date itself, with the main pair's
//!   listing rules), and its Remote tree is their decrypted view
//!   (`oneup_vaultsync::project`).
//! * Its agent is its own `oneupd` process: unlocking starts it with the
//!   vault's keys handed over its standard input (never written to disk),
//!   locking stops it. "Remember on this PC" keeps the keys DPAPI-protected
//!   in the vault pair's data folder ([`KEY_FILE`]), so it can start
//!   without asking.
//! * Files On-Demand by default (the decrypted files are online-only until
//!   opened), or full files.
//!
//! The main pair leaves a vault inside its OneDrive folder alone (rules,
//! see `rulefiles`), and a vault's folder may not overlap any synced
//! folder: decrypted files must never be uploaded as they are.

use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex, RwLock};

use anyhow::{Context, Result, bail};
use oneup_exec::{Cloud, CloudError, GraphCloud, Meta, MetaDone, VersionMark};
use oneup_graph::Graph;
use oneup_store::{RemoteRow, Store};
use oneup_sync::{RemoteId, RemotePre};
use oneup_vault::key::Masterkey;
use oneup_vaultsync::{Nodes, Source, Vault, VaultCloud};
use zeroize::Zeroizing;

/// In a vault pair's data folder: the vault's OneDrive path.
pub const MARKER: &str = "vault-pair.txt";
/// The vault's keys, DPAPI-protected ("remember on this PC").
pub const KEY_FILE: &str = "vault-key.dpapi";
/// Where vault pairs' data folders are, inside the main data folder.
pub const VAULTS: &str = "vaults";
/// Meta keys in a vault pair's store.
pub const KEY_ROOT: &str = "vault.root";
pub const KEY_THRESHOLD: &str = "vault.threshold";

pub fn is_vault_dir(dir: &Path) -> bool {
    dir.join(MARKER).is_file()
}

/// A vault pair's or an extra folder pair's data folder (`crate::pairs`):
/// one that signs in as its main data folder does.
pub fn is_child_dir(dir: &Path) -> bool {
    is_vault_dir(dir) || crate::pairs::is_pair_dir(dir)
}

/// The data folder whose sign-in `dir` uses (and, for a vault pair, whose
/// cloud listing): its own, or for a vault or extra pair the main one.
pub fn main_dir(dir: &Path) -> PathBuf {
    if is_child_dir(dir)
        && let Some(main) = dir.parent().and_then(Path::parent)
    {
        return main.to_path_buf();
    }
    dir.to_path_buf()
}

/// A vault pair of a main data folder.
#[derive(Debug, Clone)]
pub struct Entry {
    pub name: String,
    pub dir: PathBuf,
    /// The vault's OneDrive path.
    pub remote: String,
}

/// [`list`], for safety checks: only a missing folder or marker means
/// "none"; any other error is returned rather than skipped.
pub fn list_strict(main: &Path) -> std::io::Result<Vec<Entry>> {
    let not_found = |e: &std::io::Error| e.kind() == std::io::ErrorKind::NotFound;
    let entries = match std::fs::read_dir(main.join(VAULTS)) {
        Ok(e) => e,
        Err(e) if not_found(&e) => return Ok(Vec::new()),
        Err(e) => return Err(e),
    };
    let mut out = Vec::new();
    for e in entries {
        let e = e?;
        let dir = e.path();
        let remote = match std::fs::read_to_string(dir.join(MARKER)) {
            Ok(r) => r,
            Err(e) if not_found(&e) => continue,
            Err(e) => return Err(e),
        };
        out.push(Entry { name: e.file_name().to_string_lossy().into_owned(), dir, remote: remote.trim().to_string() });
    }
    out.sort_by_key(|e| e.name.to_lowercase());
    Ok(out)
}

/// The vault pairs of the main data folder `main`.
pub fn list(main: &Path) -> Vec<Entry> {
    let mut out: Vec<Entry> = std::fs::read_dir(main.join(VAULTS))
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

/// A data folder name for the vault at OneDrive path `remote`, not yet used.
pub fn new_name(main: &Path, remote: &str) -> String {
    let last = remote.rsplit('/').find(|s| !s.is_empty()).unwrap_or("vault");
    let base: String =
        last.chars().map(|c| if c.is_alphanumeric() || c == '-' || c == '_' { c } else { '-' }).take(40).collect();
    let base = if base.trim_matches('-').is_empty() { "vault".to_string() } else { base };
    let taken = list(main);
    (1..)
        .map(|n| if n == 1 { base.clone() } else { format!("{base}-{n}") })
        .find(|n| taken.iter().all(|e| !oneup_sync::same_name(&e.name, n)) && !main.join(VAULTS).join(n).exists())
        .unwrap_or(base)
}

// ------------------------------------------------------------------ keys

pub fn save_key(dir: &Path, key: &Masterkey) -> Result<()> {
    let sealed = oneup_sys::dpapi::protect(key.export().as_ref()).context("protecting the vault's keys")?;
    crate::ownfile::write(&dir.join(KEY_FILE), &sealed)?;
    Ok(())
}

pub fn load_key(dir: &Path) -> Result<Option<Masterkey>> {
    let Ok(sealed) = std::fs::read(dir.join(KEY_FILE)) else { return Ok(None) };
    let raw = oneup_sys::dpapi::unprotect(&sealed).context("reading the remembered vault keys")?;
    let raw: &[u8; 64] = raw.as_slice().try_into().context("the remembered vault keys are damaged")?;
    Ok(Some(Masterkey::import(raw)))
}

pub fn forget_key(dir: &Path) -> Result<bool> {
    match std::fs::remove_file(dir.join(KEY_FILE)) {
        Ok(()) => Ok(true),
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(false),
        Err(e) => Err(e.into()),
    }
}

/// The vault's keys sealed with Windows Hello's secret
/// (`oneup_vault::sealed`, `oneup_sys::hello`): unlocking then asks for
/// your face, fingerprint or PIN instead of the password.
pub const HELLO_FILE: &str = "vault-key.hello";

/// The Windows Hello credential of the vault pair in `dir`.
#[cfg(windows)]
fn hello_name(dir: &Path) -> Result<String> {
    let token = crate::setup::open_store(dir, 1024)?
        .read(|o| o.meta_get(crate::sync::KEY_TOKEN))?
        .context("not a vault pair")?;
    Ok(format!("OneUpDrive vault {token}"))
}

/// Sets Windows Hello up for the vault pair in `dir` (shows its prompt).
#[cfg(windows)]
pub fn save_key_hello(dir: &Path, key: &Masterkey) -> Result<()> {
    let challenge = oneup_vault::sealed::new_challenge()?;
    let mut secret = Zeroizing::new([0u8; 32]);
    oneup_sys::hello::secret(&hello_name(dir)?, &challenge, true, &mut secret)?;
    let blob = oneup_vault::sealed::seal(key, &secret, &challenge)?;
    crate::ownfile::write(&dir.join(HELLO_FILE), &blob)?;
    Ok(())
}

/// The keys, through Windows Hello (shows its prompt); `None` if it isn't
/// set up for this vault.
#[cfg(windows)]
pub fn load_key_hello(dir: &Path) -> Result<Option<Masterkey>> {
    let Ok(blob) = std::fs::read(dir.join(HELLO_FILE)) else { return Ok(None) };
    let challenge = oneup_vault::sealed::challenge(&blob)?;
    let mut secret = Zeroizing::new([0u8; 32]);
    oneup_sys::hello::secret(&hello_name(dir)?, &challenge, false, &mut secret)?;
    Ok(Some(oneup_vault::sealed::open(&blob, &secret)?))
}

/// Turns Windows Hello off for the vault pair in `dir`.
#[cfg(windows)]
pub fn forget_hello(dir: &Path) -> Result<bool> {
    if let Ok(name) = hello_name(dir) {
        oneup_sys::hello::delete(&name);
    }
    match std::fs::remove_file(dir.join(HELLO_FILE)) {
        Ok(()) => Ok(true),
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(false),
        Err(e) => Err(e.into()),
    }
}

/// The length of [`key_line`]: 64 bytes in hex and a newline.
const KEY_LINE_LEN: usize = 129;

/// The keys as one line of hex, for handing them to the agent. Written
/// into a string of its final size, so a string that grows leaves no
/// partial copy behind.
pub fn key_line(key: &Masterkey) -> Zeroizing<String> {
    const HEX: &[u8; 16] = b"0123456789abcdef";
    let mut line = Zeroizing::new(String::with_capacity(KEY_LINE_LEN));
    for b in key.export().iter() {
        line.push(char::from(HEX[usize::from(b >> 4)]));
        line.push(char::from(HEX[usize::from(b & 15)]));
    }
    line.push('\n');
    line
}

/// The agent's side of [`key_line`]: one line from standard input, read
/// straight from its handle. Rust's own `stdin()` keeps a buffer for the
/// life of the process, which would hold the keys long after they're used.
pub fn key_from_stdin() -> Result<Masterkey> {
    #[cfg(windows)]
    let handle = {
        use std::os::windows::io::AsHandle as _;
        std::io::stdin().as_handle().try_clone_to_owned()
    };
    #[cfg(not(windows))]
    let handle = {
        use std::os::fd::AsFd as _;
        std::io::stdin().as_fd().try_clone_to_owned()
    };
    read_key_line(&mut std::fs::File::from(handle.context("reading the vault's keys")?))
}

/// One [`key_line`] from `input`, read into a fixed, zeroed-on-drop buffer
/// (no buffered reader or growing string keeps a copy). The line must end
/// with its newline: input that stops before it (the sender died mid-write)
/// is refused rather than guessed at.
fn read_key_line(input: &mut impl std::io::Read) -> Result<Masterkey> {
    let mut line = Zeroizing::new([0u8; KEY_LINE_LEN + 1]);
    let mut n = 0;
    while n < line.len() && !line[..n].contains(&b'\n') {
        match input.read(&mut line[n..]) {
            Ok(0) => break,
            Ok(k) => n += k,
            Err(e) if e.kind() == std::io::ErrorKind::Interrupted => {}
            Err(e) => return Err(e).context("reading the vault's keys"),
        }
    }
    if !line[..n].contains(&b'\n') {
        bail!("no vault keys on standard input");
    }
    keys_from_line(&line[..n])
}

/// The keys in a [`key_line`].
fn keys_from_line(line: &[u8]) -> Result<Masterkey> {
    let end = line.iter().position(|&b| b == b'\n').unwrap_or(line.len());
    let hex = line[..end].strip_suffix(b"\r").unwrap_or(&line[..end]);
    if hex.len() != 128 {
        bail!("no vault keys on standard input");
    }
    let nibble = |c: u8| match c {
        b'0'..=b'9' => Ok(c - b'0'),
        b'a'..=b'f' => Ok(c - b'a' + 10),
        b'A'..=b'F' => Ok(c - b'A' + 10),
        _ => Err(anyhow::anyhow!("the vault keys aren't hex")),
    };
    let mut raw = Zeroizing::new([0u8; 64]);
    for (b, pair) in raw.iter_mut().zip(hex.as_chunks::<2>().0) {
        *b = (nibble(pair[0])? << 4) | nibble(pair[1])?;
    }
    Ok(Masterkey::import(&raw))
}

/// A cloud file's content (small files: the vault's key and config).
async fn fetch(cloud: &GraphCloud, r: &RemoteRow) -> Result<Vec<u8>> {
    let mut out = Vec::new();
    let pre = RemotePre { id: RemoteId(r.id.clone()), etag: r.etag.clone().unwrap_or_default() };
    cloud.download(&pre, &mut out).await.map_err(|e| anyhow::anyhow!("{e}"))?;
    Ok(out)
}

/// Unlocks the vault whose folder is `root` in the listing `main`: its keys
/// from `masterkey.cryptomator` and the password, then its configuration,
/// verified with them. Returns the keys and the name-shortening threshold.
pub async fn unlock(graph: &Arc<Graph>, main: &Store, root: &str, password: &str) -> Result<(Masterkey, usize)> {
    let kids = children(main, root)?;
    unlock_in(graph, kids, password).await
}

fn children(main: &Store, root: &str) -> Result<Vec<RemoteRow>> {
    Ok(main.read(|o| o.remote_children(root))?.into_iter().map(|(r, _)| r).collect())
}

/// [`unlock`] from the vault folder's listed items (no database held while
/// waiting on OneDrive).
async fn unlock_in(graph: &Arc<Graph>, kids: Vec<RemoteRow>, password: &str) -> Result<(Masterkey, usize)> {
    let find = |n: &str| kids.iter().find(|r| oneup_sync::same_name(&r.name, n)).cloned();
    let cfg = find(oneup_vault::config::FILE).context("this OneDrive folder isn't a vault (no vault.cryptomator)")?;
    let cloud = GraphCloud::new(Arc::clone(graph));
    let token = String::from_utf8(fetch(&cloud, &cfg).await?).context("vault.cryptomator isn't text")?;
    // Only a masterkey file next to it (never a path elsewhere, never Hub).
    let key_name = oneup_vault::config::key_file(&token)?;
    let key_file = find(&key_name).with_context(|| format!("the vault's {key_name} is missing"))?;
    let json = fetch(&cloud, &key_file).await?;
    let key = Masterkey::from_file(&json, password)?;
    let config = oneup_vault::config::Config::verify(&token, &key)?;
    Ok((key, config.shortening_threshold))
}

// ---------------------------------------------------------- the listing

/// The vault's encrypted items, from the main listing; small files from
/// OneDrive.
pub struct Listing {
    pub store: Arc<Mutex<Store>>,
    pub cloud: GraphCloud,
}

impl Source for Listing {
    fn children(&self, id: &str) -> oneup_vaultsync::Result<Vec<RemoteRow>> {
        let s = self.store.lock().unwrap_or_else(|p| p.into_inner());
        Ok(s.read(|o| o.remote_children(id))?.into_iter().map(|(r, _)| r).collect())
    }

    fn item(&self, id: &str) -> oneup_vaultsync::Result<Option<RemoteRow>> {
        Ok(self.store.lock().unwrap_or_else(|p| p.into_inner()).read(|o| o.get_item(id))?)
    }

    async fn fetch(&self, item: &RemoteRow) -> oneup_vaultsync::Result<Vec<u8>> {
        fetch(&self.cloud, item).await.map_err(|e| oneup_vaultsync::Error::Fetch(format!("{e:#}")))
    }
}

/// Where a vault pair's Remote tree comes from (see the module docs).
pub struct VaultRemote {
    main: PathBuf,
    graph: Arc<Graph>,
    pub vault: Vault,
    pub nodes: Arc<RwLock<Nodes>>,
    listing: Listing,
    cache: tokio::sync::Mutex<oneup_vaultsync::project::Cache>,
    rule_files: Mutex<crate::rulefiles::Cache>,
    /// Problems already reported, so each is said once.
    reported: Mutex<Vec<String>>,
    /// The main listing's invariants were checked once this run (see
    /// `Engine::remote`).
    verified: std::sync::atomic::AtomicBool,
}

impl VaultRemote {
    /// For the vault pair in data folder `dir`, unlocked with `key`.
    pub fn open(dir: &Path, store: &Store, graph: Arc<Graph>, key: Masterkey) -> Result<Self> {
        let root = store.read(|o| o.meta_get(KEY_ROOT))?.context("not a vault pair's data folder")?;
        let threshold = store
            .read(|o| o.meta_get(KEY_THRESHOLD))?
            .and_then(|t| t.parse().ok())
            .unwrap_or(oneup_vault::config::SHORTENING_THRESHOLD);
        let main = main_dir(dir);
        let listing = Arc::new(Mutex::new(crate::setup::open_store(&main, 4 * 1024)?));
        Ok(Self {
            main,
            vault: Vault { key: Arc::new(key), threshold, root },
            nodes: Arc::new(RwLock::new(Nodes::new())),
            listing: Listing { store: listing, cloud: GraphCloud::new(Arc::clone(&graph)) },
            graph,
            cache: tokio::sync::Mutex::new(oneup_vaultsync::project::Cache::default()),
            rule_files: Mutex::new(crate::rulefiles::Cache::default()),
            reported: Mutex::new(Vec::new()),
            verified: std::sync::atomic::AtomicBool::new(false),
        })
    }

    /// The cloud a vault pair's executor (or hydration) uses.
    pub fn cloud(&self, inner: GraphCloud) -> VaultCloud<GraphCloud> {
        VaultCloud::new(inner, self.vault.clone(), Arc::clone(&self.nodes), Arc::clone(&self.listing.store))
    }

    /// Brings the main listing up to date (with the main pair's listing
    /// rules), then the vault pair's Remote tree from it.
    pub async fn refresh(&self, store: Store) -> Result<Store> {
        let main = crate::setup::open_store(&self.main, 4 * 1024)?;
        let pruning = {
            let mut c = self.rule_files.lock().unwrap_or_else(|p| p.into_inner());
            crate::sync::pruning_for(&self.main, &main, &mut c)?
        };
        let verify = !self.verified.load(std::sync::atomic::Ordering::Relaxed);
        drop(crate::sync::mirror_with(&self.graph, main, &pruning, verify).await?);
        self.verified.store(true, std::sync::atomic::Ordering::Relaxed);
        let mut cache = self.cache.lock().await;
        let p = oneup_vaultsync::project(&self.listing, &self.vault, &mut cache).await?;
        drop(cache);
        {
            let mut seen = self.reported.lock().unwrap_or_else(|p| p.into_inner());
            for why in &p.problems {
                if !seen.contains(why) {
                    println!("Vault: {why}");
                    seen.push(why.clone());
                }
            }
        }
        *self.nodes.write().unwrap_or_else(|p| p.into_inner()) = p.nodes.clone();
        let mut store = store;
        oneup_vaultsync::apply(&mut store, &self.listing, &p)?;
        Ok(store)
    }
}

// --------------------------------------------------------- pair's cloud

/// The cloud a folder pair's executor talks to: OneDrive as it is, or a
/// vault in it.
pub enum PairCloud {
    Plain(GraphCloud),
    Vault(VaultCloud<GraphCloud>),
}

type Hash = [u8; 20];

impl Cloud for PairCloud {
    async fn download(
        &self,
        pre: &RemotePre,
        out: &mut (dyn std::io::Write + Send),
    ) -> Result<Option<Hash>, CloudError> {
        match self {
            PairCloud::Plain(c) => c.download(pre, out).await,
            PairCloud::Vault(c) => c.download(pre, out).await,
        }
    }

    async fn upload(
        &self,
        parent: &RemoteId,
        name: &str,
        replace: Option<&RemotePre>,
        size: i64,
        src: &mut dyn oneup_exec::Source,
    ) -> Result<(RemoteRow, Option<Hash>), CloudError> {
        match self {
            PairCloud::Plain(c) => c.upload(parent, name, replace, size, src).await,
            PairCloud::Vault(c) => c.upload(parent, name, replace, size, src).await,
        }
    }

    async fn version_mark(&self, pre: &RemotePre, size: i64) -> Result<Option<VersionMark>, CloudError> {
        match self {
            PairCloud::Plain(c) => c.version_mark(pre, size).await,
            PairCloud::Vault(c) => c.version_mark(pre, size).await,
        }
    }

    async fn replaced_other(&self, id: &RemoteId, mark: &VersionMark) -> Result<Option<VersionMark>, CloudError> {
        match self {
            PairCloud::Plain(c) => c.replaced_other(id, mark).await,
            PairCloud::Vault(c) => c.replaced_other(id, mark).await,
        }
    }

    async fn download_version(
        &self,
        id: &RemoteId,
        version: &str,
        out: &mut (dyn std::io::Write + Send),
    ) -> Result<u64, CloudError> {
        match self {
            PairCloud::Plain(c) => c.download_version(id, version, out).await,
            PairCloud::Vault(c) => c.download_version(id, version, out).await,
        }
    }

    fn upcoming(&self, downloads: &[RemotePre]) {
        match self {
            PairCloud::Plain(c) => c.upcoming(downloads),
            // A vault downloads each file's encrypted content by its own ID.
            PairCloud::Vault(_) => {}
        }
    }

    fn concurrent(&self) -> bool {
        match self {
            PairCloud::Plain(c) => c.concurrent(),
            PairCloud::Vault(c) => c.concurrent(),
        }
    }

    fn can_copy(&self) -> bool {
        match self {
            PairCloud::Plain(c) => c.can_copy(),
            PairCloud::Vault(c) => c.can_copy(),
        }
    }

    async fn copy(
        &self,
        source: &RemotePre,
        hash: &Hash,
        parent: &RemoteId,
        name: &str,
    ) -> Result<RemoteRow, CloudError> {
        match self {
            PairCloud::Plain(c) => c.copy(source, hash, parent, name).await,
            PairCloud::Vault(c) => c.copy(source, hash, parent, name).await,
        }
    }

    async fn mkdir(&self, parent: &RemoteId, name: &str) -> Result<RemoteRow, CloudError> {
        match self {
            PairCloud::Plain(c) => c.mkdir(parent, name).await,
            PairCloud::Vault(c) => c.mkdir(parent, name).await,
        }
    }

    async fn move_item(&self, pre: &RemotePre, to: &RemoteId, name: &str) -> Result<RemoteRow, CloudError> {
        match self {
            PairCloud::Plain(c) => c.move_item(pre, to, name).await,
            PairCloud::Vault(c) => c.move_item(pre, to, name).await,
        }
    }

    async fn delete(&self, pre: &RemotePre) -> Result<(), CloudError> {
        match self {
            PairCloud::Plain(c) => c.delete(pre).await,
            PairCloud::Vault(c) => c.delete(pre).await,
        }
    }

    async fn prune(&self, pre: &RemotePre) -> Result<(), CloudError> {
        match self {
            PairCloud::Plain(c) => c.prune(pre).await,
            PairCloud::Vault(c) => c.prune(pre).await,
        }
    }

    async fn item(&self, id: &RemoteId) -> Result<Option<RemoteRow>, CloudError> {
        match self {
            PairCloud::Plain(c) => c.item(id).await,
            PairCloud::Vault(c) => c.inner().item(id).await,
        }
    }

    async fn children(&self, id: &RemoteId) -> Result<Vec<RemoteRow>, CloudError> {
        match self {
            PairCloud::Plain(c) => c.children(id).await,
            PairCloud::Vault(c) => c.children(id).await,
        }
    }

    async fn batch(&self, ops: &[Meta]) -> Vec<Result<MetaDone, CloudError>> {
        match self {
            PairCloud::Plain(c) => c.batch(ops).await,
            PairCloud::Vault(c) => c.batch(ops).await,
        }
    }
}

// ----------------------------------------------------------- overlaps

fn within(a: &Path, b: &Path) -> bool {
    oneup_local::coexist::is_within(a, b)
}

/// Refuses a vault folder that overlaps a synced folder (the main pair's or
/// another vault's): decrypted files must never be uploaded as they are.
pub fn check_no_overlap(main: &Store, main_dir: &Path, folder: &Path, except: Option<&Path>) -> Result<()> {
    let mut synced: Vec<PathBuf> = Vec::new();
    if let Some(l) = main.read(|o| o.meta_get("sync.local_path"))? {
        synced.push(PathBuf::from(l));
    }
    for e in list(main_dir) {
        if except.is_some_and(|x| crate::test_account::same_dir(x, &e.dir)) {
            continue;
        }
        if let Ok(s) = crate::setup::open_store(&e.dir, 1024)
            && let Some(l) = s.read(|o| o.meta_get("sync.local_path"))?
        {
            synced.push(PathBuf::from(l));
        }
    }
    for s in synced {
        if within(folder, &s) || within(&s, folder) {
            bail!(
                "{} overlaps {}, which is already synced: a vault's decrypted files must stay out of other synced \
                 folders (and the other way round). Pick a folder elsewhere.",
                folder.display(),
                s.display()
            );
        }
    }
    Ok(())
}

// ------------------------------------------------------- adding a pair

/// `/A/B` with single slashes; the drive's top is refused.
fn remote_path(p: &str) -> Result<String> {
    let parts: Vec<&str> = p.split(['/', '\\']).filter(|s| !s.is_empty()).collect();
    if parts.is_empty() || parts.iter().any(|s| *s == "." || *s == "..") {
        bail!("give the vault's folder in OneDrive, e.g. /Vault");
    }
    Ok(format!("/{}", parts.join("/")))
}

/// Whether OneDrive paths `a` and `b` (as [`remote_path`] gives them) name
/// one folder, as OneDrive compares names.
fn same_remote(a: &str, b: &str) -> bool {
    oneup_sync::fold(a) == oneup_sync::fold(b)
}

/// What [`add`] made.
pub struct Added {
    pub entry: Entry,
    pub key: Masterkey,
    pub on_demand: bool,
}

/// Pairs the vault at OneDrive path `remote` with the empty (or new) local
/// `folder`, in a new vault pair data folder of `main`. Nothing is synced
/// yet: the vault's agent does that once started.
pub async fn add(
    main: &Path,
    graph: Arc<Graph>,
    remote: &str,
    folder: &Path,
    on_demand: bool,
    password: &str,
    remember: bool,
) -> Result<Added> {
    crate::setup::require_test_account(main)?;
    if is_vault_dir(main) {
        bail!("give the main data folder, not a vault pair's");
    }
    let remote = remote_path(remote)?;
    if list(main).iter().any(|e| same_remote(&e.remote, &remote)) {
        bail!("the vault at {remote} is already paired");
    }

    // ---- the vault in the cloud listing, and its keys
    let store = crate::setup::open_store(main, 4 * 1024)?;
    let pruning = crate::sync::pruning_for(main, &store, &mut crate::rulefiles::Cache::default())?;
    let store = crate::sync::mirror(&graph, store, &pruning).await?;
    let root = crate::sync::find_remote(&store, &remote)?.with_context(|| format!("no folder {remote} in OneDrive"))?;
    let (key, threshold) = unlock(&graph, &store, &root, password).await?;

    // ---- the local folder: empty, and apart from every synced folder
    std::fs::create_dir_all(folder).with_context(|| format!("creating {}", folder.display()))?;
    let folder = std::fs::canonicalize(folder)?;
    if folder.components().count() < 3 {
        bail!("refusing a drive's top folder; pick a folder inside it");
    }
    if std::fs::read_dir(&folder)?.next().is_some() {
        bail!("{} isn't empty: a vault is paired with a new or empty folder", folder.display());
    }
    check_no_overlap(&store, main, &folder, None)?;
    if let Some(other) = oneup_cfapi::other_sync_root(&folder) {
        bail!("{} is managed by {}; OneUpDrive won't use it", folder.display(), other.provider_name);
    }
    drop(store);

    // ---- the pair's data folder
    let name = new_name(main, &remote);
    let dir = main.join(VAULTS).join(&name);
    std::fs::create_dir_all(&dir)?;
    let token = crate::sync::random_token();
    let made = (|| -> Result<()> {
        if on_demand {
            crate::syncroot::register(
                &folder,
                &token,
                &format!("OneUpDrive vault {}", remote.trim_start_matches('/')),
            )?;
        }
        let mut store = crate::setup::open_store(&dir, 4 * 1024)?;
        store.write(|o| {
            o.meta_set(crate::setup::KEY_PRESET, "none")?;
            o.meta_set(KEY_ROOT, &root)?;
            o.meta_set(KEY_THRESHOLD, &threshold.to_string())
        })?;
        let mut scan =
            oneup_local::Scan::open(store, &folder, oneup_local::Config::new(oneup_rules::ExcludeSet::none()))?;
        if !scan.run()?.finished {
            bail!("the folder's scan was interrupted");
        }
        let mut store = scan.into_store();
        let local = store.read(|o| o.local_root())?.context("no local scan")?;
        if !store.write(|o| oneup_exec::link_roots(o, local.root_file_id, &root))? {
            bail!("the new data folder isn't empty");
        }
        store.write(|o| {
            o.meta_set(crate::sync::KEY_LOCAL, &folder.to_string_lossy())?;
            o.meta_set(crate::sync::KEY_REMOTE, &remote)?;
            o.meta_set(crate::sync::KEY_TOKEN, &token)
        })?;
        oneup_effects::LocalFs::open(Path::new(&local.path), local.volume_serial, local.root_file_id, &token, true)
            .context("opening the folder")?;
        store.write(|o| o.meta_set(crate::sync::KEY_MARKER, "1"))?;
        if remember {
            save_key(&dir, &key)?;
        }
        // Last: until it's there, this isn't a vault pair.
        std::fs::write(dir.join(MARKER), &remote)?;
        Ok(())
    })();
    if let Err(e) = made {
        // Our own half-made data folder and registration; the local folder
        // was empty and stays.
        if on_demand {
            let _ = crate::syncroot::unregister(&folder, &token);
        }
        let _ = std::fs::remove_dir_all(&dir);
        return Err(e);
    }
    Ok(Added { entry: Entry { name, dir, remote }, key, on_demand })
}

/// Makes a new, empty vault in OneDrive folder `remote` (new, or empty),
/// locked with `password`, then pairs it like [`add`]. The vault is a
/// standard Cryptomator vault (format 8). Show the person its recovery
/// key ([`oneup_vault::recovery`]) straight away.
pub async fn create(
    main: &Path,
    graph: Arc<Graph>,
    remote: &str,
    folder: &Path,
    on_demand: bool,
    password: &str,
    remember: bool,
) -> Result<Added> {
    crate::setup::require_test_account(main)?;
    if crate::vaultpair::is_child_dir(main) {
        bail!("give the main data folder, not a vault's or a folder pair's");
    }
    if password.chars().count() < 8 {
        bail!("use a password of at least 8 characters");
    }
    let remote = remote_path(remote)?;
    // Refused before anything is made: the folder on this PC must be new
    // or empty (as `add` requires), and apart from every synced folder.
    if folder.exists() && std::fs::read_dir(folder)?.next().is_some() {
        bail!("{} isn't empty: a vault is paired with a new or empty folder", folder.display());
    }
    let top = crate::pairs::ensure_folder(&graph, &remote).await?;
    let page = graph.children_page(&top.id, None).await?;
    if !page.value.is_empty() {
        bail!("OneDrive {remote} isn't empty; a new vault goes into a new or empty folder");
    }
    let key = Masterkey::generate()?;
    let key_file = key.to_file(password)?;
    let config = oneup_vault::config::Config::new()?.sign(&key)?;
    let cloud = GraphCloud::new(Arc::clone(&graph));
    let at = RemoteId(top.id.clone());
    let put = |name: &'static str, data: Vec<u8>| {
        let (cloud, at) = (&cloud, &at);
        async move {
            let len = i64::try_from(data.len()).unwrap_or(i64::MAX);
            cloud.upload(at, name, None, len, &mut data.as_slice()).await.map_err(|e| anyhow::anyhow!("{name}: {e}"))
        }
    };
    put(oneup_vault::key::FILE, key_file).await?;
    put(oneup_vault::config::FILE, config.into_bytes()).await?;
    // `d` and the top folder's storage in it (`d/XX/…`), as Cryptomator
    // makes them: it expects them, even in an empty vault.
    let path = oneup_vault::names::dir_path(&key, oneup_vault::names::ROOT_DIR_ID)?;
    let mut parent = at.clone();
    for part in path.split('/') {
        parent = RemoteId(cloud.mkdir(&parent, part).await.map_err(|e| anyhow::anyhow!("{part}: {e}"))?.id);
    }
    add(main, graph, &remote, folder, on_demand, password, remember).await
}

/// The vault's configuration (`vault.cryptomator`), read from OneDrive.
async fn config_token(graph: &Arc<Graph>, kids: &[RemoteRow]) -> Result<String> {
    let cfg = kids
        .iter()
        .find(|r| oneup_sync::same_name(&r.name, oneup_vault::config::FILE))
        .context("this OneDrive folder isn't a vault (no vault.cryptomator)")?;
    String::from_utf8(fetch(&GraphCloud::new(Arc::clone(graph)), cfg).await?).context("vault.cryptomator isn't text")
}

/// The vault pair's top folder's listed items (the main listing brought up
/// to date first).
async fn top_items(main: &Path, graph: &Arc<Graph>, e: &Entry) -> Result<(String, Vec<RemoteRow>)> {
    let root = crate::setup::open_store(&e.dir, 1024)?.read(|o| o.meta_get(KEY_ROOT))?.context("not a vault pair")?;
    let store = crate::setup::open_store(main, 4 * 1024)?;
    let pruning = crate::sync::pruning_for(main, &store, &mut crate::rulefiles::Cache::default())?;
    let store = crate::sync::mirror(graph, store, &pruning).await?;
    let kids = children(&store, &root)?;
    Ok((root, kids))
}

/// The vault's keys from its recovery key, checked against the vault's
/// signed configuration (so a recovery key of another vault is refused).
pub async fn keys_from_recovery(main: &Path, graph: &Arc<Graph>, e: &Entry, recovery: &str) -> Result<Masterkey> {
    let key = oneup_vault::recovery::from_recovery_key(recovery)?;
    let (_, kids) = top_items(main, graph, e).await?;
    let token = config_token(graph, &kids).await?;
    oneup_vault::config::Config::verify(&token, &key)
        .map_err(|_| anyhow::anyhow!("that recovery key belongs to another vault"))?;
    Ok(key)
}

/// Locks the vault with a new password: a new `masterkey.cryptomator`
/// wrapping the same keys, sent as a new version of the old one (If-Match;
/// OneDrive's version history keeps the old one). Nothing else changes:
/// files aren't re-encrypted, and keys remembered on this PC stay valid.
pub async fn set_password(main: &Path, graph: Arc<Graph>, e: &Entry, key: &Masterkey, password: &str) -> Result<()> {
    if password.chars().count() < 8 {
        bail!("use a password of at least 8 characters");
    }
    let (root, kids) = top_items(main, &graph, e).await?;
    // The right vault's keys, or nothing is written.
    let token = config_token(&graph, &kids).await?;
    oneup_vault::config::Config::verify(&token, key)?;
    let file = kids
        .iter()
        .find(|r| oneup_sync::same_name(&r.name, oneup_vault::key::FILE))
        .context("the vault's masterkey.cryptomator is missing")?;
    let json = key.to_file(password)?;
    let pre = RemotePre { id: RemoteId(file.id.clone()), etag: file.etag.clone().unwrap_or_default() };
    let len = i64::try_from(json.len()).unwrap_or(i64::MAX);
    GraphCloud::new(graph)
        .upload(&RemoteId(root), oneup_vault::key::FILE, Some(&pre), len, &mut json.as_slice())
        .await
        .map_err(|err| anyhow::anyhow!("saving the new password: {err}"))?;
    Ok(())
}

/// A vault pair's local folder and whether it's on demand.
pub fn describe(e: &Entry) -> Result<(PathBuf, bool)> {
    let store = crate::setup::open_store(&e.dir, 1024)?;
    let local = PathBuf::from(store.read(|o| o.meta_get(crate::sync::KEY_LOCAL))?.unwrap_or_default());
    let token = store.read(|o| o.meta_get(crate::sync::KEY_TOKEN))?.unwrap_or_default();
    let on = crate::ondemand::is_on(&local, &token);
    Ok((local, on))
}

/// Re-reads the keys with `password` (for unlocking a pair that isn't
/// remembered).
pub async fn keys(main: &Path, graph: &Arc<Graph>, e: &Entry, password: &str) -> Result<Masterkey> {
    // Listed afresh: the password may have changed on another PC.
    let (_, kids) = top_items(main, graph, e).await?;
    Ok(unlock_in(graph, kids, password).await?.0)
}

/// Removes the pair (its agent must be stopped): the folder stops being
/// OneUpDrive's, and the pair's data folder goes. The vault in OneDrive and
/// the files with content in the folder stay; online-only files leave the
/// folder (Windows removes them; they're in the vault).
pub fn remove(e: &Entry) -> Result<(PathBuf, bool)> {
    let (local, on) = describe(e)?;
    #[cfg(windows)]
    let _ = forget_hello(&e.dir);
    if on {
        let token = crate::setup::open_store(&e.dir, 1024)?
            .read(|o| o.meta_get(crate::sync::KEY_TOKEN))?
            .context("pairing token missing")?;
        crate::syncroot::unregister(&local, &token).context("releasing the folder")?;
    }
    std::fs::remove_dir_all(&e.dir).with_context(|| format!("removing {}", e.dir.display()))?;
    Ok((local, on))
}

#[cfg(test)]
mod tests {
    #[test]
    fn a_vault_is_already_paired_under_a_name_windows_takes_for_the_same() {
        use super::same_remote;
        assert!(same_remote("/Vault", "/VAULT"));
        assert!(same_remote("/ΟΔΟΣ", "/οδος"));
        assert!(same_remote("/ΟΔΟΣ", "/οδοσ"), "final sigma folds as sigma");
        assert!(!same_remote("/\u{212A}it", "/kit"), "the Kelvin sign is its own letter");
        assert!(!same_remote("/Vault", "/Vault2"));
    }

    #[test]
    #[allow(clippy::unwrap_used)]
    fn keys_go_to_the_agent_as_one_exact_line() {
        let key = oneup_vault::key::Masterkey::generate().unwrap();
        let line = super::key_line(&key);
        assert_eq!((line.len(), line.capacity()), (super::KEY_LINE_LEN, super::KEY_LINE_LEN));
        let back = super::keys_from_line(line.as_bytes()).unwrap();
        assert_eq!(*back.export(), *key.export());
        let crlf = line.trim_end().to_uppercase() + "\r\n";
        assert_eq!(*super::keys_from_line(crlf.as_bytes()).unwrap().export(), *key.export());
        // Not hex, too short, or non-ASCII of the right length: refused, never a panic.
        assert!(super::keys_from_line(b"").is_err());
        assert!(super::keys_from_line(&[b'g'; 128]).is_err());
        assert!(super::keys_from_line(&line.as_bytes()[..100]).is_err());
        assert!(super::keys_from_line("é".repeat(64).as_bytes()).is_err());
    }

    /// A reader that hands out its data in steps: `Ok(bytes)` gives at most
    /// that many bytes (one call each), `Err(kind)` fails that call once.
    struct Steps {
        data: Vec<u8>,
        at: usize,
        steps: std::collections::VecDeque<Result<usize, std::io::ErrorKind>>,
    }

    impl std::io::Read for Steps {
        fn read(&mut self, buf: &mut [u8]) -> std::io::Result<usize> {
            let max = match self.steps.pop_front() {
                Some(Err(kind)) => return Err(kind.into()),
                Some(Ok(max)) => max,
                None => usize::MAX,
            };
            let n = buf.len().min(max).min(self.data.len() - self.at);
            buf[..n].copy_from_slice(&self.data[self.at..self.at + n]);
            self.at += n;
            Ok(n)
        }
    }

    fn steps(data: &[u8], steps: Vec<Result<usize, std::io::ErrorKind>>) -> Steps {
        Steps { data: data.to_vec(), at: 0, steps: steps.into() }
    }

    #[test]
    #[allow(clippy::unwrap_used)]
    fn the_agent_reads_its_key_line_however_the_pipe_delivers_it() {
        use std::io::ErrorKind::{BrokenPipe, Interrupted};
        let key = oneup_vault::key::Masterkey::generate().unwrap();
        let line = super::key_line(&key);
        let bytes = line.as_bytes();

        // One byte at a time, with interruptions along the way (retried).
        let mut one = Vec::new();
        for i in 0..bytes.len() {
            if i % 10 == 0 {
                one.push(Err(Interrupted));
            }
            one.push(Ok(1));
        }
        let got = super::read_key_line(&mut steps(bytes, one)).unwrap();
        assert_eq!(*got.export(), *key.export());
        // All at once.
        assert_eq!(*super::read_key_line(&mut steps(bytes, vec![])).unwrap().export(), *key.export());

        // Ends before its newline: half a line, or all the hex but no newline.
        assert!(super::read_key_line(&mut steps(&bytes[..64], vec![])).is_err());
        assert!(super::read_key_line(&mut steps(&bytes[..128], vec![])).is_err());
        assert!(super::read_key_line(&mut steps(b"", vec![])).is_err());
        // Another I/O error: returned, not retried, no panic.
        let e = super::read_key_line(&mut steps(bytes, vec![Ok(5), Err(BrokenPipe)])).err().unwrap();
        assert!(e.chain().any(|c| c.downcast_ref::<std::io::Error>().is_some_and(|io| io.kind() == BrokenPipe)));
        // Too much on the line: refused.
        let long = [&bytes[..128], b"00\n"].concat();
        assert!(super::read_key_line(&mut steps(&long, vec![])).is_err());
    }
}
