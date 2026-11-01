//! Reading `.oneupignore` files from the synced folder into a
//! [`RuleSet`] (`docs/architecture/rules.md` § Where rules come from).
//! Only ordinary files are read (never a placeholder), at most
//! [`MAX_BYTES`], through `oneup_local::verify` and its guards.

use oneup_local::verify::{Checked, Small, Verifier};
use oneup_rules::{IGNORE_FILE, RuleSet};
use oneup_store::{LocalKind, LocalRow, Ops};

/// Larger rule files are ignored and reported.
pub const MAX_BYTES: u64 = 64 * 1024;

/// Why a rule file was ignored.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Problem {
    TooLarge,
    /// Not UTF-8 text.
    NotText,
    /// Not read: a placeholder or link, unreadable, or changed while read.
    NotRead(Checked),
}

impl std::fmt::Display for Problem {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Problem::TooLarge => write!(f, "larger than {} KiB", MAX_BYTES / 1024),
            Problem::NotText => f.write_str("not UTF-8 text"),
            Problem::NotRead(Checked::NotOrdinary) => f.write_str("not an ordinary file"),
            Problem::NotRead(Checked::Changed) => f.write_str("changed while being read; read again next time"),
            Problem::NotRead(_) => f.write_str("couldn't be read"),
        }
    }
}

/// The rule file directly in folder `dir`, if there is one.
pub fn find(o: &Ops<'_>, dir: u128) -> oneup_store::Result<Option<LocalRow>> {
    Ok(o.local_children(dir)?
        .into_iter()
        .find(|r| r.kind == LocalKind::File && oneup_sync::same_name(&r.name, IGNORE_FILE)))
}

/// Rule files read before, by file ID with the size and last-write time
/// they had, so an unchanged file isn't read again every round. Holds only
/// the current rule files.
#[derive(Debug, Default)]
pub struct Cache {
    texts: std::collections::HashMap<u128, (Stamp, Result<String, Problem>)>,
}

/// A file's size and last-write time.
type Stamp = (i64, i64);

/// The whole rule set for a round: `config.toml` plus every `.oneupignore`
/// in the synced folder, shallowest first. A rule file inside a folder the
/// rules exclude isn't read (as in git). Returns the rule files that were
/// ignored, with why, by path.
pub fn load(
    data_dir: &std::path::Path,
    store: &oneup_store::Store,
    cache: &mut Cache,
) -> anyhow::Result<(RuleSet, Vec<(String, Problem)>)> {
    let mut set = crate::config::load_rules(data_dir, store)?.rule_set()?;
    add_separate(data_dir, store, &mut set)?;
    add_onedrive_app(data_dir, store, &mut set)?;
    let mut files: Vec<(String, LocalRow)> = store.read(|o| {
        o.local_rule_files()?
            .into_iter()
            .filter_map(|r| o.local_path_of(r.file_id).transpose().map(|p| p.map(|p| (p, r))))
            .collect::<oneup_store::Result<_>>()
    })?;
    files.sort_by_key(|(p, _)| (p.matches('/').count(), p.to_lowercase()));
    cache.texts.retain(|id, _| files.iter().any(|(_, r)| r.file_id == *id));
    let mut verifier = None;
    let mut problems = Vec::new();
    for (path, row) in files {
        let dir = path.trim_start_matches('/').rsplit_once('/').map_or("", |(d, _)| d).to_string();
        if !dir.is_empty() && set.excludes(&dir, true) {
            continue;
        }
        let stamp = (row.size, row.mtime);
        let cached = cache.texts.get(&row.file_id).filter(|(s, _)| *s == stamp).map(|(_, t)| t.clone());
        let text = match cached {
            Some(t) => t,
            None => {
                if verifier.is_none() {
                    let root = store.read(|o| o.local_root())?.ok_or_else(|| anyhow::anyhow!("no local scan"))?;
                    verifier = Some(Verifier::open(&root, 64 * 1024)?);
                }
                let Some(v) = verifier.as_mut() else { continue };
                let t = read(v, &row);
                // A file changing while read is read again next round.
                if t != Err(Problem::NotRead(Checked::Changed)) {
                    cache.texts.insert(row.file_id, (stamp, t.clone()));
                }
                t
            }
        };
        match text {
            Ok(t) => set.add_ignore_file(&dir, &t),
            Err(p) => problems.push((path, p)),
        }
    }
    Ok((set, problems))
}

/// A vault or another folder pair synced on its own (`crate::vaultpair`,
/// `crate::pairs`) inside this pair's OneDrive folder: its files are left
/// to it, on both sides.
pub fn add_separate(data_dir: &std::path::Path, store: &oneup_store::Store, set: &mut RuleSet) -> anyhow::Result<()> {
    if crate::vaultpair::is_child_dir(data_dir) {
        return Ok(());
    }
    if let Some(remote) = store.read(|o| o.meta_get(crate::sync::KEY_REMOTE))? {
        let top = format!("{}/", remote.trim_end_matches('/'));
        let separate = crate::vaultpair::list(data_dir)
            .into_iter()
            .map(|v| v.remote)
            .chain(crate::pairs::own_drive_folders(data_dir));
        let inside: Vec<String> =
            separate.filter_map(|r| Some(oneup_sync::strip_folded_prefix(&r, &top)?.to_string())).collect();
        set.set_vaults(&inside);
    }
    Ok(())
}

/// Folders left to the OneDrive app (`config::OneDriveApp`), as paths in
/// this pair's OneDrive folder.
pub fn add_onedrive_app(
    data_dir: &std::path::Path,
    store: &oneup_store::Store,
    set: &mut RuleSet,
) -> anyhow::Result<()> {
    let app = crate::config::load_onedrive_app(data_dir)?;
    if app.folders.is_empty() && !app.top_files {
        return Ok(());
    }
    let remote = store.read(|o| o.meta_get(crate::sync::KEY_REMOTE))?.unwrap_or_default();
    let top = remote.trim_end_matches('/');
    let inside: Vec<String> = app
        .folders
        .iter()
        .filter_map(|f| {
            let f = format!("/{}", f.trim_matches(['/', '\\']).replace('\\', "/"));
            if top.is_empty() {
                return Some(f);
            }
            Some(oneup_sync::strip_folded_prefix(&f, &format!("{top}/"))?.to_string())
        })
        .collect();
    // The drive's top files are only this pair's when it syncs the whole drive.
    set.set_onedrive_app(&inside, app.top_files && top.is_empty());
    Ok(())
}

fn read(v: &mut Verifier, row: &LocalRow) -> Result<String, Problem> {
    let bytes = match v.read_small(row, MAX_BYTES) {
        Small::Read(b) => b,
        Small::TooLarge => return Err(Problem::TooLarge),
        Small::Checked(c) => return Err(Problem::NotRead(c)),
    };
    let text = String::from_utf8(bytes).map_err(|_| Problem::NotText)?;
    Ok(text.strip_prefix('\u{feff}').map(str::to_owned).unwrap_or(text))
}

/// Reads `row` (a rule file of folder `dir`, relative to the synced
/// folder) into `set`.
pub fn add(set: &mut RuleSet, v: &mut Verifier, dir: &str, row: &LocalRow) -> Result<(), Problem> {
    let text = read(v, row)?;
    set.add_ignore_file(dir, &text);
    Ok(())
}

#[cfg(all(test, windows))]
mod tests {
    #![allow(clippy::unwrap_used)]
    use super::*;
    use oneup_store::Store;

    #[test]
    fn rule_files_load_shallowest_first_and_not_inside_excluded_folders() {
        let dir = std::env::temp_dir().join(format!("oneup-rulefiles-{}", std::process::id()));
        let data = std::env::temp_dir().join(format!("oneup-rulefiles-data-{}", std::process::id()));
        for d in [&dir, &data] {
            let _ = std::fs::remove_dir_all(d);
            std::fs::create_dir_all(d).unwrap();
        }
        let w = |rel: &str, text: &[u8]| {
            let p = dir.join(rel);
            std::fs::create_dir_all(p.parent().unwrap()).unwrap();
            std::fs::write(p, text).unwrap();
        };
        w(".oneupignore", b"\xef\xbb\xbf*.iso\nbuild/\n");
        w("app/.oneupignore", b"!keep.iso\n");
        w("build/.oneupignore", b"!*\n"); // inside an excluded folder: never read
        w("bad/.oneupignore", b"\xff\xfe not utf-8");
        let cfg = oneup_local::Config::new(oneup_rules::ExcludeSet::none());
        let mut scan = oneup_local::Scan::open(Store::open_in_memory().unwrap(), &dir, cfg).unwrap();
        assert!(scan.run().unwrap().finished);
        let store = scan.into_store();

        let mut cache = Cache::default();
        let (set, problems) = load(&data, &store, &mut cache).unwrap();
        assert!(set.excludes("x.iso", false) && !set.excludes("app/keep.iso", false));
        assert!(set.excludes("build/out.bin", false), "build's own rule file wasn't read");
        assert_eq!(problems, vec![("/bad/.oneupignore".to_string(), Problem::NotText)]);
        assert_eq!(cache.texts.len(), 3, "the three read files (the bad one too)");
        let again = load(&data, &store, &mut cache).unwrap().0;
        assert_eq!(again.fingerprint(), set.fingerprint());
        for d in [&dir, &data] {
            let _ = std::fs::remove_dir_all(d);
        }
    }
}
