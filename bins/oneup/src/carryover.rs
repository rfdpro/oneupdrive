//! Moving over from the OneDrive app keeps the choices made there:
//!
//! * **Always keep on this device**: the items the app had pinned stay on
//!   this PC under OneUpDrive too.
//! * **Kinds of files left out** by the app's policy ("Exclude specific
//!   kinds of files from being uploaded", the documented machine policy
//!   `EnableODIgnoreListFromGPO`) become exclusion rules.
//! * **Folders not on this PC**: folders in OneDrive that the app's folder
//!   doesn't have (most likely unticked in its **Choose folders**) are
//!   paused: not brought here (not even online-only), nothing sent, and
//!   never offered for removal from OneDrive. Resuming one syncs it.
//!
//! [`capture`] reads them while the app still manages its folder (pins
//! are visible only then; an unlinking that stalled still counts, as the
//! folder is untouched until the app releases it), from this data folder's read-only scan of that
//! folder and its OneDrive listing, during `oneup migrate check`, and keeps
//! them in `carry-over.json`. [`apply`] adds the rules and pauses the folders when that folder is
//! first paired with the whole drive; the pins wait in the store until the
//! agent's placement pass finds each item as OneUpDrive's own
//! ([`pin_pending`]). Nothing is read from the app's own settings files
//! (undocumented); the choices are read from what's on disk and in the
//! listing.

use std::path::{Path, PathBuf};

use anyhow::{Context, Result};
use oneup_store::{Kind, LocalKind, Skip, Store};
use serde_json::{Value, json};

/// In the data folder, until it's applied.
pub const FILE: &str = "carry-over.json";
/// Pins waiting for their items (one relative path a line).
const KEY_PINS: &str = "carry.pins";
/// The app's policy list of file kinds it doesn't upload.
const POLICY: &str = r"SOFTWARE\Policies\Microsoft\OneDrive\EnableODIgnoreListFromGPO";

#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct CarryOver {
    /// The OneDrive app's folder it was read from.
    pub folder: PathBuf,
    /// Relative paths kept on this device (the tops of pinned subtrees).
    pub pinned: Vec<String>,
    /// Rules for kinds of files the app didn't upload (`*.pst`).
    pub file_kinds: Vec<String>,
    /// Relative paths of OneDrive folders the app's folder doesn't have.
    pub not_on_pc: Vec<String>,
    /// The account the app synced (its drive ID and e-mail address): the
    /// app's own settings forget it when it's unlinked, and the first
    /// pairing must still refuse another account.
    pub drive: Option<String>,
    pub email: Option<String>,
}

/// The file kinds the OneDrive app's policy leaves out, as rules.
pub fn policy_file_kinds() -> Vec<String> {
    let mut out: Vec<String> = Vec::new();
    for (_, v) in oneup_sys::user_registry::machine_values(POLICY).unwrap_or_default() {
        let v = v.trim().to_string();
        if crate::config::check_rule(&v).is_ok() && !out.contains(&v) {
            out.push(v);
        }
    }
    out
}

/// Reads the choices from this data folder's scan of the app's folder
/// `folder` and its OneDrive listing (of the whole drive).
pub fn capture(store: &Store, app: &crate::moveover::App) -> Result<CarryOver> {
    let folder = &app.folder;
    let root = store.read(|o| o.local_root())?.context("the OneDrive app's folder hasn't been scanned")?;
    let pinned = store.read(|o| {
        let mut v = Vec::new();
        for id in o.local_pinned_tops()? {
            if let Some(p) = o.local_path_of(id)? {
                v.push(crate::filetree::clean(&p));
            }
        }
        Ok(v)
    })?;
    // Folders in OneDrive that the app's folder doesn't have, below folders
    // it does have and lists (an online-only folder is there, unlisted).
    let mut not_on_pc = Vec::new();
    if let Some(remote) = store.read(|o| o.remote_root())? {
        let mut stack = vec![(root.root_file_id, remote.id, String::new())];
        while let Some((local, cloud, rel)) = stack.pop() {
            let here = store.read(|o| o.local_children(local))?;
            for (r, excluded) in store.read(|o| o.remote_children(&cloud))? {
                if r.kind != Kind::Folder || excluded {
                    continue;
                }
                let child = if rel.is_empty() { r.name.clone() } else { format!("{rel}/{}", r.name) };
                match here.iter().find(|l| oneup_sync::same_name(&l.name, &r.name)) {
                    None => not_on_pc.push(child),
                    Some(l) if l.kind == LocalKind::Folder && l.skip == Skip::None => {
                        stack.push((l.file_id, r.id, child));
                    }
                    Some(_) => {}
                }
            }
        }
    }
    not_on_pc.sort_by_key(|p| p.to_lowercase());
    Ok(CarryOver {
        folder: folder.clone(),
        pinned,
        file_kinds: policy_file_kinds(),
        not_on_pc,
        drive: app.drive.clone(),
        email: app.email.clone(),
    })
}

pub fn save(data_dir: &Path, c: &CarryOver) -> Result<()> {
    let v = json!({
        "folder": c.folder.to_string_lossy(),
        "pinned": c.pinned,
        "file_kinds": c.file_kinds,
        "not_on_pc": c.not_on_pc,
        "drive": c.drive,
        "email": c.email,
    });
    let tmp = data_dir.join(format!("{FILE}.tmp"));
    std::fs::write(&tmp, serde_json::to_vec_pretty(&v)?)?;
    std::fs::rename(&tmp, data_dir.join(FILE))?;
    Ok(())
}

pub fn load(data_dir: &Path) -> Result<Option<CarryOver>> {
    let text = match std::fs::read(data_dir.join(FILE)) {
        Ok(t) => t,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(None),
        Err(e) => return Err(e.into()),
    };
    let v: Value = serde_json::from_slice(&text).context("reading carry-over.json")?;
    let list = |k: &str| -> Vec<String> {
        v.get(k)
            .and_then(Value::as_array)
            .map(|a| a.iter().filter_map(|x| x.as_str().map(str::to_string)).collect())
            .unwrap_or_default()
    };
    Ok(Some(CarryOver {
        folder: PathBuf::from(v.get("folder").and_then(Value::as_str).unwrap_or_default()),
        pinned: list("pinned"),
        file_kinds: list("file_kinds"),
        not_on_pc: list("not_on_pc"),
        drive: v.get("drive").and_then(Value::as_str).map(str::to_string),
        email: v.get("email").and_then(Value::as_str).map(str::to_string),
    }))
}

/// When the folder being paired (`local`, with the drive's `remote`) is the
/// one the choices were read from: adds the rules and queues the pins. Says
/// what it did, if anything. Applying again changes nothing more (rules
/// aren't added twice, the pins are the same list), so the file is marked
/// applied only once the pairing is recorded ([`mark_applied`]): a pairing
/// that fails on the way doesn't use the choices up.
pub fn apply(data_dir: &Path, store: &mut Store, local: &Path, remote: &str) -> Result<Option<String>> {
    let Some(c) = load(data_dir)? else { return Ok(None) };
    if !crate::test_account::same_dir(&c.folder, local) {
        return Ok(None);
    }
    let whole_drive = remote.trim_matches('/').is_empty();
    let mut rules = crate::config::load_rules(data_dir, store)?;
    let mut added = 0;
    let mut add = |r: String| {
        if !rules.exclude.iter().any(|x| oneup_sync::same_name(x, &r)) {
            rules.exclude.push(r);
            added += 1;
        }
    };
    for k in &c.file_kinds {
        add(k.clone());
    }
    // Paths are from the drive's top: they fit only a whole-drive pair.
    let mut paused = 0;
    if whole_drive {
        for f in &c.not_on_pc {
            if !rules.paused.iter().any(|x| oneup_sync::same_name(x, f)) {
                rules.paused.push(f.clone());
                paused += 1;
            }
        }
    }
    crate::config::save_rules(data_dir, &rules)?;
    let pins = if whole_drive { c.pinned.clone() } else { Vec::new() };
    store.write(|o| o.meta_set(KEY_PINS, &pins.join("\n")))?;
    Ok(Some(format!(
        "Kept from the OneDrive app: {added} exclusion rule(s) for kinds of files it didn't upload, \
         {paused} paused folder(s) that weren't on this PC, and {} item(s) set to always keep on this device.",
        pins.len()
    )))
}

/// After the pairing that [`apply`] was for is recorded: the choices are
/// used, and not applied again.
pub fn mark_applied(data_dir: &Path) -> Result<()> {
    match std::fs::rename(data_dir.join(FILE), data_dir.join(format!("{FILE}.applied"))) {
        Err(e) if e.kind() != std::io::ErrorKind::NotFound => Err(e.into()),
        _ => Ok(()),
    }
}

/// Sets the pins still waiting on the items that are now OneUpDrive's own
/// placeholders under `root`; keeps the rest waiting. Returns how many were
/// set. Called by the agent's placement pass (Files On-Demand only).
pub fn pin_pending(store: &mut Store, root: &Path) -> Result<usize> {
    use oneup_cfapi::provider;
    let Some(text) = store.read(|o| o.meta_get(KEY_PINS))? else { return Ok(0) };
    if text.is_empty() {
        return Ok(0);
    }
    let mut left = Vec::new();
    let mut done = 0;
    for rel in text.lines().filter(|l| !l.is_empty()) {
        let full = rel.split('/').fold(root.to_path_buf(), |p, x| p.join(x));
        let set = provider::open(&full).ok().is_some_and(|f| {
            if full.is_dir() {
                provider::set_pin_tree(&f, provider::Pin::Pinned)
            } else {
                provider::set_pin(&f, provider::Pin::Pinned)
            }
            .is_ok()
        });
        if set {
            done += 1;
        } else if full.exists() {
            // Not one of ours yet: tried again next pass. (Gone: dropped.)
            left.push(rel.to_string());
        }
    }
    store.write(|o| o.meta_set(KEY_PINS, &left.join("\n")))?;
    Ok(done)
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used)]
    use super::*;

    #[test]
    fn choices_are_applied_once_to_the_same_folder() {
        let dir = std::env::temp_dir().join(format!("oneup-carry-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        let app = dir.join("OneDrive");
        let c = CarryOver {
            folder: app.clone(),
            pinned: vec!["Work".into(), "Docs/plan.docx".into()],
            file_kinds: vec!["*.pst".into()],
            not_on_pc: vec!["Old Photos".into()],
            drive: Some("abc123".into()),
            email: None,
        };
        save(&dir, &c).unwrap();
        assert_eq!(load(&dir).unwrap().unwrap(), c);
        let mut store = crate::setup::open_store(&dir, 1024).unwrap();
        // Another folder: nothing happens.
        assert!(apply(&dir, &mut store, &dir.join("Elsewhere"), "/").unwrap().is_none());
        let said = apply(&dir, &mut store, &app, "/").unwrap().unwrap();
        assert!(said.contains("1 exclusion rule(s)") && said.contains("1 paused folder(s)"), "{said}");
        let rules = crate::config::load_rules(&dir, &store).unwrap();
        assert_eq!(rules.exclude, ["*.pst"]);
        assert_eq!(rules.paused, ["Old Photos"], "not brought here, not even online-only");
        assert_eq!(store.read(|o| o.meta_get(KEY_PINS)).unwrap().as_deref(), Some("Work\nDocs/plan.docx"));
        // The pairing failed on the way: applied again, nothing added twice.
        let again = apply(&dir, &mut store, &app, "/").unwrap().unwrap();
        assert!(again.contains("0 exclusion rule(s)") && again.contains("0 paused"), "{again}");
        // The pairing is recorded: used, and not again.
        mark_applied(&dir).unwrap();
        assert!(load(&dir).unwrap().is_none());
        assert!(apply(&dir, &mut store, &app, "/").unwrap().is_none());
        // Pins whose items are gone are dropped.
        assert_eq!(pin_pending(&mut store, &dir.join("nowhere")).unwrap(), 0);
        assert_eq!(store.read(|o| o.meta_get(KEY_PINS)).unwrap().as_deref(), Some(""));
        drop(store);
        let _ = std::fs::remove_dir_all(&dir);
    }
}
