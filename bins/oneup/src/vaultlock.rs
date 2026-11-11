//! Locking a vault pair clears its decrypted copies from this PC
//! (`docs/architecture/vaults.md` § Locking).
//!
//! When its agent stops (Lock, `oneup vault lock`, signing out, an update),
//! every decrypted file that is the synced version is overwritten with
//! zeros and freed ([`crate::sync::Engine::clear_vault`]); the placeholders
//! stay, so the names stay visible here, as online-only files that only an
//! unlocked vault can open. A copy with an edit not sent yet, or open in a
//! program, is kept and named in the activity log and in [`REPORT`].
//!
//! Overwriting is recorded first ([`journal`]): a crash in between leaves a
//! record that the next start finishes ([`finish_wipes`]), with or without
//! the vault's keys. Leftover temporary copies (hydration staging, download
//! staging) are overwritten and removed too ([`clear_staging`]).
//!
//! A vault left unlocked by an unclean shutdown (a crash, power lost, the
//! agent ended) still has [`UNLOCKED`] in its data folder. At the next
//! start ([`clean_after_unclean_shutdown`], from `launch::start_others`, so
//! at sign-in) it's cleared without its keys: copies Windows still has in
//! sync go, and changes not sent yet are kept decrypted until the next
//! unlock sends them, or with `[vault] keep_unsent = false`, overwritten
//! and taken back to their synced version (`oneup_sync::Op::Discard`).

use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};

use anyhow::{Context, Result};
use oneup_effects::{LocalFs, Wipe, WipeEnd, WipeJournal, WipeThen};
use oneup_store::Store;

use crate::sync::Cleared;

/// In a vault pair's data folder from the moment its agent runs unlocked
/// until a lock has cleared its decrypted copies: there after a start means
/// the last session ended uncleanly.
pub const UNLOCKED: &str = "vault-unlocked";

/// Notes that the vault runs unlocked ([`UNLOCKED`]).
pub fn mark_unlocked(dir: &Path) -> Result<()> {
    Ok(crate::ownfile::write(&dir.join(UNLOCKED), b"")?)
}

/// Notes that a lock cleared the vault ([`UNLOCKED`] goes).
pub fn mark_locked(dir: &Path) -> Result<()> {
    match std::fs::remove_file(dir.join(UNLOCKED)) {
        Err(e) if e.kind() != std::io::ErrorKind::NotFound => Err(e.into()),
        _ => Ok(()),
    }
}

/// Clears the vaults of `main` an unclean shutdown left unlocked (see the
/// module docs), each while holding its data folder, so its agent can't
/// start meanwhile; one that's running is unlocked and left alone. Returns
/// those that couldn't be cleared, with why.
pub async fn clean_after_unclean_shutdown(main: &Path) -> Vec<(String, String)> {
    let mut failed = Vec::new();
    // Placeholders are told apart from ordinary files below: without that,
    // nothing is cleared.
    if let Err(e) = oneup_cfapi::expose_placeholders() {
        return vec![("vaults".into(), format!("can't tell placeholders apart: {e}"))];
    }
    for v in crate::vaultpair::list(main) {
        if !v.dir.join(UNLOCKED).exists() {
            continue;
        }
        let Ok(_held) = crate::setup::DirLock::acquire(&v.dir) else { continue };
        match clean_one(&v.dir).await {
            Ok(true) => {
                if let Err(e) = mark_locked(&v.dir) {
                    failed.push((format!("vault {}", v.name), format!("{e:#}")));
                }
            }
            // Something stayed: tried again at the next start.
            Ok(false) => {}
            Err(e) => failed.push((format!("vault {}", v.name), format!("{e:#}"))),
        }
    }
    failed
}

/// One vault of [`clean_after_unclean_shutdown`]: whether it's done.
async fn clean_one(dir: &Path) -> Result<bool> {
    let settings = crate::config::load_vault_lock(dir)?;
    let mut store = crate::setup::open_store(dir, 1024)?;
    // Never used to reach OneDrive: nothing here sends or downloads.
    let http = crate::setup::http()?;
    let auth = crate::setup::auth(dir, None, &http).context("the vault's sign-in")?;
    let graph = Arc::new(oneup_graph::Graph::new(http, auth, oneup_graph::Aimd::new(1, 1)));
    let rules = crate::config::load_rules(dir, &store)?.rule_set()?.pruning();
    let engine = crate::sync::resume_locked_vault(&store, graph, rules, dir)?;
    finish_wipes(&mut store, engine.local())?;
    let c = engine.clean_locked(&mut store, !settings.keep_unsent).await?;
    let staged = root_of(&store)?.map_or(0, |r| clear_staging(dir, &r));
    let mut detail =
        format!("after an unclean shutdown: {} decrypted file(s) cleared, {staged} temporary copies removed", c.freed);
    if c.discarded > 0 {
        detail += &format!("; {} unsent change(s) discarded", c.discarded);
    }
    if c.unsent > 0 {
        detail += &format!("; {} unsent change(s) kept until the next unlock", c.unsent);
    }
    if c.kept > 0 {
        detail += &format!("; {} file(s) couldn't be cleared now (open in a program), tried again later", c.kept);
    }
    crate::sync::note(&mut store, "lock", detail)?;
    Ok(c.kept == 0 && (settings.keep_unsent || c.unsent == 0))
}

/// Meta keys of overwrites under way: `vault.wipe.<file id>` = `size mtime`,
/// then `free`, `revert <size> <mtime>` or `delete` ([`WipeThen`]). The
/// executor sends nothing that has one (`Ops::wipe_pending`).
const WIPE: &str = oneup_store::WIPE_PREFIX;

fn encode(w: &Wipe) -> String {
    let (size, mtime) = w.stamp;
    match w.then {
        WipeThen::Free => format!("{size} {mtime} free"),
        WipeThen::Revert { size: s, mtime: m } => format!("{size} {mtime} revert {s} {m}"),
        WipeThen::Delete => format!("{size} {mtime} delete"),
    }
}

fn decode(v: &str) -> Option<Wipe> {
    let parts: Vec<&str> = v.split(' ').collect();
    let n = |i: usize| parts.get(i).and_then(|p| p.parse::<i64>().ok());
    let stamp = (n(0)?, n(1)?);
    let then = match parts.get(2).copied() {
        None | Some("free") => WipeThen::Free,
        Some("revert") => WipeThen::Revert { size: n(3)?, mtime: n(4)? },
        Some("delete") => WipeThen::Delete,
        Some(_) => return None,
    };
    Some(Wipe { stamp, then })
}
/// In a vault pair's data folder: what the last lock cleared and kept.
pub const REPORT: &str = "vault-lock.json";
/// In a vault pair's data folder: where opened files are decrypted first
/// (`oneup_exec::hydrate`).
pub const HYDRATION: &str = "hydration";

/// The effects layer's record of overwrites (`LocalFs::set_wipe_journal`):
/// written durably before the first zero, removed once the file is freed,
/// through a connection of its own.
pub fn journal(dir: &Path) -> Result<WipeJournal> {
    let store = Mutex::new(crate::setup::open_store(dir, 256)?);
    Ok(Box::new(move |id, wipe| {
        let mut s = store.lock().unwrap_or_else(std::sync::PoisonError::into_inner);
        let key = format!("{WIPE}.{id}");
        match wipe {
            Some(w) => s.write_durable(|o| o.meta_set(&key, &encode(&w))),
            None => s.write(|o| o.meta_delete(&key)),
        }
        .map_err(std::io::Error::other)
    }))
}

/// Finishes the overwrites a crash cut short (`LocalFs::finish_wipe`):
/// returns how many were freed and how many kept because they changed
/// since. A record that can't be finished now stays for the next start.
pub fn finish_wipes(store: &mut Store, fs: &LocalFs) -> Result<(usize, usize)> {
    let pending = store.read(|o| o.meta_under(WIPE))?;
    let (mut freed, mut kept) = (0, 0);
    for (key, value) in pending {
        let id = key.strip_prefix(WIPE).and_then(|k| k.strip_prefix('.')).and_then(|k| k.parse::<u128>().ok());
        let (Some(id), Some(wipe)) = (id, decode(&value)) else {
            store.write(|o| o.meta_delete(&key))?;
            continue;
        };
        match fs.finish_wipe(id, wipe) {
            Ok(WipeEnd::Freed) => freed += 1,
            Ok(WipeEnd::Kept) => kept += 1,
            Ok(WipeEnd::AlreadyFree) => {}
            Err(e) => {
                tracing::warn!(error = %e, "an overwrite cut short couldn't be finished; tried at the next start")
            }
        }
    }
    if freed + kept > 0 {
        let detail = format!(
            "finished clearing after an interruption: {freed} decrypted file(s) cleared, {kept} kept (changed since)"
        );
        crate::sync::note(store, "lock", detail)?;
    }
    Ok((freed, kept))
}

/// Overwrites and removes leftover temporary decrypted copies: the
/// hydration staging in the data folder `dir` and the download staging in
/// the synced folder `root` (both OneUpDrive's own folders). Best effort;
/// returns how many went.
pub fn clear_staging(dir: &Path, root: &Path) -> usize {
    let staging = root.join(oneup_rules::ENGINE_DIR).join("staging");
    let mut n = 0;
    for (folder, ext) in [(dir.join(HYDRATION), "hydrating"), (staging, "oneup-part")] {
        let Ok(entries) = std::fs::read_dir(&folder) else { continue };
        for e in entries.flatten() {
            let p = e.path();
            if e.file_type().is_ok_and(|t| t.is_file()) && p.extension().is_some_and(|x| x == ext) {
                match wipe_own_file(&p) {
                    Ok(()) => n += 1,
                    Err(e) => tracing::debug!(error = %e, "a temporary copy wasn't cleared"),
                }
            }
        }
    }
    n
}

/// One of OneUpDrive's own temporary files: overwritten with zeros, then
/// removed.
pub fn wipe_own_file(p: &Path) -> std::io::Result<()> {
    use std::io::Write;
    {
        let mut f = std::fs::OpenOptions::new().write(true).open(p)?;
        let mut left = f.metadata()?.len();
        let zeros = vec![0u8; 1 << 20];
        while left > 0 {
            let n = usize::try_from(left.min(zeros.len() as u64)).unwrap_or(zeros.len());
            f.write_all(&zeros[..n])?;
            left -= n as u64;
        }
        f.sync_data()?;
    }
    std::fs::remove_file(p)
}

/// Records what a lock cleared ([`REPORT`]) and logs it.
pub fn record(dir: &Path, store: &mut Store, cleared: &Cleared, staged: usize) -> Result<()> {
    let json = serde_json::json!({
        "at_ms": crate::sync::now_ms(),
        "freed": cleared.freed,
        "kept": cleared.kept,
        "kept_count": cleared.kept_count,
    });
    crate::ownfile::write(&dir.join(REPORT), json.to_string().as_bytes())?;
    let mut detail = format!("locked: {} decrypted file(s) cleared", cleared.freed);
    if staged > 0 {
        detail += &format!(", {staged} temporary copies removed");
    }
    if cleared.kept_count > 0 {
        detail += &format!("; {} kept (not synced yet, or open in a program)", cleared.kept_count);
    }
    crate::sync::note(store, "lock", detail)
}

/// The last lock's record ([`REPORT`]): what was cleared and kept.
pub fn last(dir: &Path) -> Option<(Cleared, i64)> {
    let v: serde_json::Value = serde_json::from_slice(&std::fs::read(dir.join(REPORT)).ok()?).ok()?;
    let n = |k: &str| v.get(k).and_then(serde_json::Value::as_u64).and_then(|x| usize::try_from(x).ok());
    let kept = v.get("kept")?.as_array()?.iter().filter_map(|p| p.as_str().map(str::to_string)).collect();
    Some((Cleared { freed: n("freed")?, kept, kept_count: n("kept_count")? }, v.get("at_ms")?.as_i64()?))
}

/// The synced folder of the vault pair whose store is `store`.
pub fn root_of(store: &Store) -> Result<Option<PathBuf>> {
    Ok(store.read(|o| o.local_root())?.map(|r| PathBuf::from(r.path)))
}
