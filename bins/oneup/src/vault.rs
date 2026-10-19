//! `oneup vault …`: tools for Cryptomator vaults in a local folder
//! (`oneup_vault::folder`), to check that OneUpDrive and Cryptomator's apps
//! read each other's vaults. They never change or remove what exists.
//!
//! The password is asked for (not shown), or read from
//! `ONEUP_VAULT_PASSWORD` for scripts and tests (a recovery key from
//! `ONEUP_VAULT_RECOVERY_KEY`).

use std::path::Path;

use anyhow::{Context, Result, bail};
use oneup_vault::folder::{Dir, Folder, Kind};
use zeroize::Zeroizing;

const PASSWORD_ENV: &str = "ONEUP_VAULT_PASSWORD";

/// Wiped from memory when dropped (the environment variable's copy, for
/// scripts and tests, stays in the process's environment).
fn password(confirm: bool) -> Result<Zeroizing<String>> {
    if let Ok(p) = std::env::var(PASSWORD_ENV) {
        return Ok(Zeroizing::new(p));
    }
    let p = Zeroizing::new(rpassword::prompt_password("Vault password: ").context("reading the password")?);
    if confirm {
        if p.chars().count() < 8 {
            bail!("use at least 8 characters (a long passphrase is best)");
        }
        let again = Zeroizing::new(rpassword::prompt_password("Again: ").context("reading the password")?);
        if again != p {
            bail!("the passwords don't match");
        }
    }
    Ok(p)
}

fn prepare() -> Result<()> {
    // Online-only files must be recognisable, so they're never read.
    oneup_cfapi::expose_placeholders().context("preparing to recognise online-only files")?;
    Ok(())
}

fn open(folder: &Path) -> Result<Folder> {
    prepare()?;
    Folder::open(folder, &password(false)?).with_context(|| format!("opening the vault in {}", folder.display()))
}

pub fn create(folder: &Path) -> Result<()> {
    prepare()?;
    let pw = password(true)?;
    let v = Folder::create(folder, &pw).with_context(|| format!("creating a vault in {}", folder.display()))?;
    println!("Created a vault in {} (Cryptomator format 8, ID {}).", folder.display(), v.config().jti);
    println!("Keep the password safe: without it, nobody can read what's in the vault, OneUpDrive included.");
    Ok(())
}

fn split(path: &str) -> (String, String) {
    let p = path.trim_matches(['/', '\\']).replace('\\', "/");
    match p.rsplit_once('/') {
        Some((parent, name)) => (parent.to_string(), name.to_string()),
        None => (String::new(), p),
    }
}

fn dir_at(v: &Folder, path: &str) -> Result<Dir> {
    match v.find(path)? {
        Some(e) => match e.kind {
            Kind::Dir(d) => Ok(d),
            _ => bail!("{path} isn't a folder in the vault"),
        },
        None => bail!("{path} isn't in the vault"),
    }
}

pub fn ls(folder: &Path, path: Option<&str>, recursive: bool) -> Result<()> {
    let v = open(folder)?;
    let start = path.unwrap_or("");
    let mut todo = vec![(start.trim_matches(['/', '\\']).to_string(), dir_at(&v, start)?)];
    let mut problems = 0;
    while let Some((at, dir)) = todo.pop() {
        let l = v.list(&dir)?;
        for e in &l.entries {
            let full = if at.is_empty() { e.name.clone() } else { format!("{at}/{}", e.name) };
            match &e.kind {
                Kind::File { size } => println!("  {full}  ({})", oneup_app::versions::size(*size)),
                Kind::Dir(d) => {
                    println!("  {full}/");
                    if recursive {
                        todo.push((full, d.clone()));
                    }
                }
                Kind::Symlink => println!("  {full}  (link)"),
            }
        }
        for p in &l.problems {
            problems += 1;
            println!("  ! in /{at}: {p}");
        }
    }
    if problems > 0 {
        bail!("{problems} entries couldn't be read");
    }
    Ok(())
}

pub fn get(folder: &Path, path: &str, dest: &Path) -> Result<()> {
    let v = open(folder)?;
    let Some(e) = v.find(path)? else { bail!("{path} isn't in the vault") };
    // Our own new file: never over an existing one.
    let out = std::fs::OpenOptions::new()
        .write(true)
        .create_new(true)
        .open(dest)
        .with_context(|| format!("creating {} (it mustn't exist yet)", dest.display()))?;
    let mut w = std::io::BufWriter::new(out);
    match v.read(&e, &mut w) {
        Ok(n) => {
            drop(w);
            println!("Decrypted {path} to {} ({}).", dest.display(), oneup_app::versions::size(n));
            Ok(())
        }
        Err(err) => {
            drop(w);
            // Created just now, and only partly written.
            let _ = std::fs::remove_file(dest);
            Err(err).with_context(|| format!("decrypting {path}"))
        }
    }
}

pub fn put(folder: &Path, src: &Path, path: &str) -> Result<()> {
    let v = open(folder)?;
    let (parent, name) = split(path);
    let dir = dir_at(&v, &parent)?;
    let input = std::fs::File::open(src).with_context(|| format!("opening {}", src.display()))?;
    let n = v.create_file(&dir, &name, std::io::BufReader::new(input))?;
    println!(
        "Encrypted {} into the vault as /{} ({}).",
        src.display(),
        path.trim_matches('/'),
        oneup_app::versions::size(n)
    );
    Ok(())
}

pub fn mkdir(folder: &Path, path: &str) -> Result<()> {
    let v = open(folder)?;
    let (parent, name) = split(path);
    v.create_dir(&dir_at(&v, &parent)?, &name)?;
    println!("Created /{} in the vault.", path.trim_matches('/'));
    Ok(())
}

/// Reads every file in the vault (decrypting into nothing) and reports
/// what fails.
pub fn check(folder: &Path) -> Result<()> {
    let v = open(folder)?;
    let (mut files, mut folders, mut bytes, mut problems) = (0u64, 0u64, 0u64, Vec::new());
    let mut todo = vec![(String::new(), Dir::root())];
    while let Some((at, dir)) = todo.pop() {
        folders += 1;
        let l = v.list(&dir)?;
        problems.extend(l.problems.iter().map(|p| format!("/{at}: {p}")));
        for e in l.entries {
            let full = format!("{at}/{}", e.name);
            match &e.kind {
                Kind::File { .. } => match v.read(&e, std::io::sink()) {
                    Ok(n) => {
                        files += 1;
                        bytes += n;
                    }
                    Err(err) => problems.push(format!("{full}: {err}")),
                },
                Kind::Dir(d) => todo.push((full.trim_start_matches('/').to_string(), d.clone())),
                Kind::Symlink => {}
            }
        }
    }
    println!("{files} file(s) ({}) in {folders} folder(s) read and verified.", oneup_app::versions::size(bytes));
    if !problems.is_empty() {
        for p in &problems {
            println!("  ! {p}");
        }
        bail!("{} problems", problems.len());
    }
    Ok(())
}

// ------------------------------------------------- vaults synced here

use std::sync::Arc;

use oneup_app::vaultpair::{self, Entry};
use oneup_graph::Graph;

fn find(main: &Path, name: &str) -> Result<Entry> {
    vaultpair::list(main)
        .into_iter()
        .find(|e| oneup_sync::same_name(&e.name, name) || oneup_sync::same_name(&e.remote, name))
        .with_context(|| format!("no vault called {name} here (`oneup vault list`)"))
}

pub async fn add(
    main: &Path,
    graph: Arc<Graph>,
    remote: &str,
    folder: &Path,
    on_demand: bool,
    remember: bool,
    start: bool,
) -> Result<()> {
    let pw = password(false)?;
    let added = vaultpair::add(main, graph, remote, folder, on_demand, &pw, remember).await?;
    let (local, _) = vaultpair::describe(&added.entry)?;
    println!(
        "Paired the vault {} with {} as \"{}\"{}.",
        added.entry.remote,
        local.display(),
        added.entry.name,
        if added.on_demand { " (files online-only until opened)" } else { " (all files kept decrypted here)" }
    );
    if remember {
        println!("Its keys are remembered on this PC (protected by your Windows sign-in).");
    }
    if start {
        crate::agent::start_with(&added.entry.dir, Some(&added.key)).await?;
    } else {
        println!("`oneup vault unlock {}` starts syncing it.", added.entry.name);
    }
    Ok(())
}

/// Prints a recovery key with what to do with it.
fn show_recovery(key: &oneup_vault::key::Masterkey) {
    println!("\nRecovery key (write it down or print it, and keep it somewhere safe):\n");
    println!("  {}\n", oneup_vault::recovery::recovery_key(key).as_str());
    println!("With it you can set a new password if you forget yours (`oneup vault change-password NAME");
    println!("--recovery-key`). Anyone who has it can read the vault. It's OneUpDrive's own: Cryptomator's");
    println!("apps don't read it.");
}

/// `oneup vault new`: a new vault in OneDrive, synced with a folder here.
pub async fn new(
    main: &Path,
    graph: Arc<Graph>,
    remote: &str,
    folder: &Path,
    on_demand: bool,
    remember: bool,
    start: bool,
) -> Result<()> {
    let pw = password(true)?;
    let added = vaultpair::create(main, graph, remote, folder, on_demand, &pw, remember).await?;
    let (local, _) = vaultpair::describe(&added.entry)?;
    println!(
        "Created the vault {} in OneDrive and paired it with {} as \"{}\".",
        added.entry.remote,
        local.display(),
        added.entry.name
    );
    show_recovery(&added.key);
    if start { crate::agent::start_with(&added.entry.dir, Some(&added.key)).await } else { Ok(()) }
}

/// The keys of a synced vault: remembered, or from its password.
async fn keys_of(main: &Path, graph: &Arc<Graph>, e: &Entry) -> Result<oneup_vault::key::Masterkey> {
    match vaultpair::load_key(&e.dir)? {
        Some(k) => Ok(k),
        None => vaultpair::keys(main, graph, e, &password(false)?).await,
    }
}

pub async fn recovery_key(main: &Path, graph: Arc<Graph>, name: &str) -> Result<()> {
    let e = find(main, name)?;
    show_recovery(&keys_of(main, &graph, &e).await?);
    Ok(())
}

pub async fn change_password(main: &Path, graph: Arc<Graph>, name: &str, with_recovery: bool) -> Result<()> {
    let e = find(main, name)?;
    let key = if with_recovery {
        let text = Zeroizing::new(match std::env::var("ONEUP_VAULT_RECOVERY_KEY") {
            Ok(t) => t,
            Err(_) => rpassword::prompt_password("Recovery key: ").context("reading the recovery key")?,
        });
        vaultpair::keys_from_recovery(main, &graph, &e, &text).await?
    } else {
        println!("Current password:");
        keys_of(main, &graph, &e).await?
    };
    println!("New password:");
    let pw = password(true)?;
    vaultpair::set_password(main, graph, &e, &key, &pw).await?;
    println!("The vault's password is changed. Cryptomator's apps and other PCs use the new one from now on.");
    Ok(())
}

pub async fn list(main: &Path) -> Result<()> {
    let all = vaultpair::list(main);
    if all.is_empty() {
        println!(r"No vaults synced here. `oneup vault add /Path/In/OneDrive C:\Some\Folder` adds one.");
        return Ok(());
    }
    for e in all {
        let (local, on) = vaultpair::describe(&e)?;
        let running = crate::agent::current(&e.dir).await.ok().flatten().is_some();
        let remembered = e.dir.join(vaultpair::KEY_FILE).exists();
        println!("{}", e.name);
        println!("  OneDrive {}  <->  {}", e.remote, local.display());
        println!(
            "  {}; {}; {}",
            if on { "online-only until opened" } else { "all files here" },
            if running { "unlocked, syncing" } else { "locked" },
            if remembered { "keys remembered on this PC" } else { "password needed to unlock" }
        );
    }
    Ok(())
}

pub async fn unlock(main: &Path, graph: Arc<Graph>, name: &str, remember: bool) -> Result<()> {
    let e = find(main, name)?;
    if crate::agent::current(&e.dir).await.ok().flatten().is_some() {
        println!("{} is already unlocked.", e.name);
        return Ok(());
    }
    let key = match vaultpair::load_key(&e.dir)? {
        Some(k) => k,
        None => match vaultpair::load_key_hello(&e.dir)? {
            Some(k) => k,
            None => vaultpair::keys(main, &graph, &e, &password(false)?).await?,
        },
    };
    if remember {
        vaultpair::save_key(&e.dir, &key)?;
    }
    crate::agent::start_with(&e.dir, Some(&key)).await
}

/// `oneup vault hello NAME on|off`: unlock with Windows Hello instead of
/// the password.
pub async fn hello(main: &Path, graph: Arc<Graph>, name: &str, on: Option<bool>) -> Result<()> {
    let e = find(main, name)?;
    let set_up = e.dir.join(vaultpair::HELLO_FILE).exists();
    match on {
        None => println!(
            "Windows Hello {} for {} ({}).",
            if set_up { "unlocks" } else { "doesn't unlock" },
            e.name,
            if oneup_sys::hello::available() { "available on this PC" } else { "not set up on this PC" }
        ),
        Some(true) => {
            if !oneup_sys::hello::available() {
                bail!("Windows Hello isn't set up on this PC (Settings > Accounts > Sign-in options)");
            }
            let key = keys_of(main, &graph, &e).await?;
            println!("Confirm with Windows Hello…");
            vaultpair::save_key_hello(&e.dir, &key)?;
            println!("{} now unlocks with Windows Hello (`oneup vault unlock {}`).", e.name, e.name);
        }
        Some(false) => {
            if vaultpair::forget_hello(&e.dir)? {
                println!("{} no longer unlocks with Windows Hello.", e.name);
            } else {
                println!("Windows Hello wasn't set up for {}.", e.name);
            }
        }
    }
    Ok(())
}

pub async fn lock(main: &Path, name: &str, forget: bool) -> Result<()> {
    let e = find(main, name)?;
    crate::agent::stop(&e.dir).await?;
    if forget && vaultpair::forget_key(&e.dir)? {
        println!("Its remembered keys are gone; unlocking it needs the password again.");
    }
    Ok(())
}

pub async fn remove(main: &Path, name: &str, yes: bool) -> Result<()> {
    let e = find(main, name)?;
    if crate::agent::current(&e.dir).await.ok().flatten().is_some() {
        bail!("{} is unlocked; `oneup vault lock {}` first", e.name, e.name);
    }
    let (local, on) = vaultpair::describe(&e)?;
    if !yes {
        println!("This stops syncing the vault {} with {}.", e.remote, local.display());
        println!("The vault in OneDrive stays as it is.");
        if on {
            println!("Online-only files leave that folder (they're in the vault); files with their content here stay,");
            println!("decrypted, as ordinary files.");
        } else {
            println!("The decrypted files in that folder stay as ordinary files.");
        }
        println!("Run again with --yes to do it.");
        return Ok(());
    }
    vaultpair::remove(&e)?;
    println!("{} is no longer synced; {} stays as it is now.", e.name, local.display());
    Ok(())
}
