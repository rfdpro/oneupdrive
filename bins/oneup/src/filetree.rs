//! The settings window's file tree (its **Files** page): what's in a synced
//! folder, on this PC and in OneDrive side by side, and for each item
//! whether it syncs and why, and where its content is. The page's actions
//! change the same settings the command line and `.oneupignore` files do
//! ([`set_sync`], [`set_paused`], [`set_order`] edit `config.toml`); this is
//! another way to look at them, not another system.
//!
//! Reads the data folder's database (both trees and the Synced links) and,
//! for items on this PC, their file attributes. Never a file's content. A
//! folder that's left out is stored as one row (its contents aren't
//! tracked), so its contents aren't listed.

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};

use anyhow::{Context, Result, bail};
use oneup_rules::{ENGINE_DIR, Match, RuleSet, Source, Verdict};
use oneup_store::{Kind, LocalKind, LocalRow, RemoteRow, Skip, Store};
use serde_json::{Value, json};

use crate::config;

/// At most this many items of one folder are listed.
const MAX_LISTED: usize = 2000;

/// File attributes (Cloud Files).
const PINNED: u32 = 0x0008_0000;
const UNPINNED: u32 = 0x0010_0000;
const RECALL_ON_DATA_ACCESS: u32 = 0x0040_0000;

/// `rel` cleaned: `/`-separated, no leading or trailing `/`.
pub fn clean(rel: &str) -> String {
    rel.replace('\\', "/").split('/').filter(|p| !p.is_empty() && *p != ".").collect::<Vec<_>>().join("/")
}

/// The rule matching exactly the item at `rel` (a folder if `folder`):
/// anchored at the synced folder, `[` taken literally.
pub fn rule_for(rel: &str, folder: bool) -> String {
    let body: String = clean(rel).chars().map(|c| if c == '[' { "[[]".to_string() } else { c.to_string() }).collect();
    format!("/{body}{}", if folder { "/" } else { "" })
}

/// The rules that apply inside the folder at `parts` (config, folders
/// synced on their own, folders left to the OneDrive app, and every
/// `.oneupignore` on the way down), and that folder's Local row if it's
/// on this PC and listed.
fn rules_at(data_dir: &Path, store: &Store, parts: &[&str]) -> Result<(RuleSet, Option<LocalRow>)> {
    let root = store.read(|o| o.local_root())?.context("this folder hasn't been looked at yet")?;
    let mut set = config::load_rules(data_dir, store)?.rule_set()?;
    crate::rulefiles::add_separate(data_dir, store, &mut set)?;
    crate::rulefiles::add_onedrive_app(data_dir, store, &mut set)?;
    let mut verifier = oneup_local::verify::Verifier::open(&root, 64 * 1024)?;
    let mut row = store.read(|o| o.local_get(root.root_file_id))?;
    for i in 0..=parts.len() {
        let Some(dir) = row.clone() else { break };
        if !matches!(dir.kind, LocalKind::Folder | LocalKind::Root) || dir.skip != Skip::None {
            row = None;
            break;
        }
        if let Some(f) = store.read(|o| crate::rulefiles::find(o, dir.file_id))? {
            // A rule file that fails its checks is ignored, as by sync.
            let _ = crate::rulefiles::add(&mut set, &mut verifier, &parts[..i].join("/"), &f);
        }
        let Some(part) = parts.get(i) else { break };
        let part = oneup_sync::fold(part);
        row = store.read(|o| o.local_children(dir.file_id))?.into_iter().find(|c| oneup_sync::fold(&c.name) == part);
    }
    Ok((set, row))
}

/// The cloud folder at `parts` below the pair's OneDrive folder.
fn remote_at(store: &Store, parts: &[&str]) -> Result<Option<String>> {
    let root = store.read(|o| o.local_root())?.context("this folder hasn't been looked at yet")?;
    let Some(mut cur) = store.read(|o| o.synced_by_local(root.root_file_id))?.and_then(|b| b.remote) else {
        return Ok(None);
    };
    for part in parts {
        let part = oneup_sync::fold(part);
        let next = store.read(|o| o.remote_children(&cur.0))?.into_iter().find_map(|(r, excluded)| {
            (r.kind == Kind::Folder && !excluded && oneup_sync::fold(&r.name) == part).then_some(r.id)
        });
        match next {
            Some(id) => cur = oneup_sync::RemoteId(id),
            None => return Ok(None),
        }
    }
    Ok(Some(cur.0))
}

/// Where a rule came from, for people.
fn source_json(m: &Match) -> Value {
    match &m.source {
        Source::Preset(n) => json!({ "kind": "preset", "name": n, "rule": m.rule }),
        Source::Config => json!({ "kind": "own", "rule": m.rule }),
        Source::IgnoreFile(d) => json!({ "kind": "file", "folder": format!("/{d}"), "rule": m.rule }),
        Source::Pause => json!({ "kind": "pause", "rule": m.rule }),
        Source::Vault => json!({ "kind": "separate", "rule": m.rule }),
        Source::OneDriveApp => json!({ "kind": "onedrive_app", "rule": m.rule }),
    }
}

/// An item's state from its rule verdict and where it is.
fn state_of(verdict: &Verdict, synced: bool, here: bool, cloud: bool) -> (&'static str, Option<Value>) {
    match verdict {
        Verdict::Excluded(m) | Verdict::InsideExcluded { by: m, .. } => {
            let state = match m.source {
                Source::Pause => "paused",
                Source::Vault => "separate",
                Source::OneDriveApp => "onedrive_app",
                _ => "left_out",
            };
            let mut why = source_json(m);
            if let Verdict::InsideExcluded { folder, .. } = verdict {
                why["inside"] = Value::from(format!("/{folder}"));
            }
            (state, Some(why))
        }
        Verdict::Included(_) if synced => ("synced", None),
        Verdict::Included(_) if here && !cloud => ("to_upload", None),
        Verdict::Included(_) if cloud && !here => ("to_download", None),
        Verdict::Included(_) => ("pending", None),
    }
}

/// Where a file's content is (Files On-Demand): `online`, `always` or `here`.
fn placement(path: &Path) -> Option<&'static str> {
    let m = std::fs::symlink_metadata(path).ok()?;
    let a = std::os::windows::fs::MetadataExt::file_attributes(&m);
    Some(if a & PINNED != 0 {
        "always"
    } else if a & RECALL_ON_DATA_ACCESS != 0 || (m.is_dir() && a & UNPINNED != 0) {
        "online"
    } else {
        "here"
    })
}

/// An item on this PC, in OneDrive (with its excluded flag), or both.
type Both = (Option<LocalRow>, Option<(RemoteRow, bool)>);

/// The first row for folded name `key` whose side isn't `taken` yet.
fn free_slot<V>(rows: &BTreeMap<(String, usize), V>, key: String, taken: impl Fn(&V) -> bool) -> (String, usize) {
    let mut n = 0;
    while rows.get(&(key.clone(), n)).is_some_and(&taken) {
        n += 1;
    }
    (key, n)
}

/// The pair's folders and modes.
pub fn pair_info(store: &Store) -> Result<(PathBuf, String, bool)> {
    let pair = crate::sync::pairing(store)?;
    let token = store.read(|o| o.meta_get(crate::sync::KEY_TOKEN))?.unwrap_or_default();
    let on_demand = crate::ondemand::is_on(&pair.local, &token);
    Ok((pair.local, pair.remote, on_demand))
}

/// The folder at `rel` in data folder `data_dir`'s synced folder: its
/// items (folders first, then by name) with their state.
pub fn list(data_dir: &Path, rel: &str) -> Result<Value> {
    oneup_cfapi::expose_placeholders()?;
    let rel = clean(rel);
    let parts: Vec<&str> = rel.split('/').filter(|p| !p.is_empty()).collect();
    let store = crate::setup::open_store(data_dir, 4 * 1024)?;
    let (local_root, _, on_demand) = pair_info(&store)?;
    let (set, row) = rules_at(data_dir, &store, &parts)?;
    // The folder itself: left out (or inside a left-out one) means its
    // contents aren't tracked.
    if !parts.is_empty() {
        let v = set.verdict(&rel, true);
        if v.is_excluded() {
            let (state, why) = state_of(&v, false, row.is_some(), false);
            return Ok(json!({ "rel": rel, "state": state, "why": why, "items": [], "untracked": true }));
        }
    }
    let locals: Vec<LocalRow> = match &row {
        Some(r) => store.read(|o| o.local_children(r.file_id))?,
        None => Vec::new(),
    };
    let remotes: Vec<(RemoteRow, bool)> = match remote_at(&store, &parts)? {
        Some(id) => store.read(|o| o.remote_children(&id))?,
        None => Vec::new(),
    };
    // Merged by name, as Windows and OneDrive compare names. Two names on
    // one side that fold alike (OneDrive may keep both) each get a row.
    let mut by_name: BTreeMap<(String, usize), Both> = BTreeMap::new();
    for l in locals {
        if parts.is_empty() && l.name.eq_ignore_ascii_case(ENGINE_DIR) {
            continue;
        }
        let key = free_slot(&by_name, oneup_sync::fold(&l.name), |b| b.0.is_some());
        by_name.entry(key).or_default().0 = Some(l);
    }
    for r in remotes {
        let key = free_slot(&by_name, oneup_sync::fold(&r.0.name), |b| b.1.is_some());
        by_name.entry(key).or_default().1 = Some(r);
    }
    let total = by_name.len();
    let mut items = Vec::new();
    for (_, (l, r)) in by_name {
        let name =
            l.as_ref().map(|l| l.name.clone()).or_else(|| r.as_ref().map(|r| r.0.name.clone())).unwrap_or_default();
        let folder = l.as_ref().map_or_else(
            || r.as_ref().is_some_and(|r| r.0.kind == Kind::Folder),
            |l| matches!(l.kind, LocalKind::Folder | LocalKind::Root),
        );
        let link = l.as_ref().is_some_and(|l| l.kind == LocalKind::Link);
        let child = if rel.is_empty() { name.clone() } else { format!("{rel}/{name}") };
        let synced = match (&l, &r) {
            (Some(l), _) if store.read(|o| o.synced_by_local(l.file_id))?.is_some() => true,
            (_, Some(r)) => store.read(|o| o.synced_by_remote(&r.0.id))?.is_some(),
            _ => false,
        };
        let verdict = set.verdict(&child, folder);
        let (mut state, why) = state_of(&verdict, synced, l.is_some(), r.is_some());
        if !folder && oneup_plan::names::not_synced(&name).is_some() {
            state = "never";
        } else if link {
            state = "link";
        }
        let path = local_root.join(child.replace('/', "\\"));
        let place = if l.is_some() && on_demand && state == "synced" { placement(&path) } else { None };
        let size = if folder { None } else { l.as_ref().map(|l| l.size).or_else(|| r.as_ref().map(|r| r.0.size)) };
        let modified_ms = r.as_ref().and_then(|r| r.0.mtime_ms);
        items.push(json!({
            "name": name,
            "folder": folder,
            "size": size,
            "modified_ms": modified_ms,
            "here": l.is_some(),
            "cloud": r.is_some(),
            "state": state,
            "why": why,
            "placement": place,
            "path": l.is_some().then(|| shown(&path)),
        }));
    }
    items.sort_by(|a, b| {
        let key =
            |v: &Value| (!v["folder"].as_bool().unwrap_or(false), v["name"].as_str().unwrap_or("").to_lowercase());
        key(a).cmp(&key(b))
    });
    items.truncate(MAX_LISTED);
    Ok(json!({ "rel": rel, "items": items, "more": total.saturating_sub(MAX_LISTED), "untracked": false }))
}

/// The total size of everything inside each folder of the folder at `rel`,
/// by name: this PC's copy where it has one (online-only files count at
/// their full size), else OneDrive's. A left-out folder isn't tracked
/// inside, so it has no total. Asked for after [`list`], as it reads the
/// whole subtree.
pub fn sizes(data_dir: &Path, rel: &str) -> Result<Value> {
    let rel = clean(rel);
    let parts: Vec<&str> = rel.split('/').filter(|p| !p.is_empty()).collect();
    let store = crate::setup::open_store(data_dir, 4 * 1024)?;
    let (_, row) = rules_at(data_dir, &store, &parts)?;
    let mut out = serde_json::Map::new();
    if let Some(remote) = remote_at(&store, &parts)? {
        let totals = store.read(|o| o.remote_folder_totals(&remote))?;
        let names: std::collections::HashMap<String, String> = store
            .read(|o| o.remote_children(&remote))?
            .into_iter()
            .map(|(r, _)| (r.id, r.name.to_lowercase()))
            .collect();
        for (id, bytes) in totals {
            if let Some(n) = names.get(&id) {
                out.insert(n.clone(), Value::from(bytes));
            }
        }
    }
    if let Some(r) = row {
        let names: std::collections::HashMap<u128, String> = store
            .read(|o| o.local_children(r.file_id))?
            .into_iter()
            .map(|c| (c.file_id, c.name.to_lowercase()))
            .collect();
        for (id, bytes) in store.read(|o| o.local_folder_totals(r.file_id))? {
            if let Some(n) = names.get(&id) {
                out.insert(n.clone(), Value::from(bytes));
            }
        }
    }
    Ok(Value::Object(out))
}

/// Items whose name contains `needle`, on this PC or in OneDrive: up to
/// `limit` of each, as paths relative to the synced folder.
pub fn find(data_dir: &Path, needle: &str, limit: usize) -> Result<Value> {
    let needle = needle.trim();
    if needle.chars().count() < 2 {
        return Ok(Value::from(Vec::<Value>::new()));
    }
    let store = crate::setup::open_store(data_dir, 4 * 1024)?;
    let root = store.read(|o| o.local_root())?.context("this folder hasn't been looked at yet")?;
    let base = store.read(|o| o.synced_by_local(root.root_file_id))?.and_then(|b| b.remote);
    let base_path = match &base {
        Some(id) => store.read(|o| o.path_of(&id.0))?.unwrap_or_default(),
        None => String::new(),
    };
    let mut seen = std::collections::BTreeSet::new();
    let mut out = Vec::new();
    for l in store.read(|o| o.local_find(needle, limit))? {
        if let Some(p) = store.read(|o| o.local_path_of(l.file_id))? {
            let rel = clean(&p);
            if rel.split('/').next().is_some_and(|f| f.eq_ignore_ascii_case(ENGINE_DIR)) {
                continue;
            }
            if seen.insert(rel.to_lowercase()) {
                out.push(json!({ "rel": rel, "folder": l.kind != LocalKind::File }));
            }
        }
    }
    for r in store.read(|o| o.remote_find(needle, limit))? {
        let Some(p) = store.read(|o| o.path_of(&r.id))? else { continue };
        // Only what's inside this pair's OneDrive folder.
        let Some(rest) = p.get(base_path.len()..).filter(|_| {
            base.is_some() && p.to_lowercase().starts_with(&base_path.to_lowercase()) && p.len() > base_path.len()
        }) else {
            continue;
        };
        let rel = clean(rest);
        if seen.insert(rel.to_lowercase()) {
            out.push(json!({ "rel": rel, "folder": r.kind == Kind::Folder }));
        }
    }
    out.sort_by_key(|v| v["rel"].as_str().map(str::to_lowercase));
    Ok(Value::from(out))
}

/// The folders holding a `.oneupignore` file (relative paths, `""` for the
/// synced folder itself), for the Left out view.
pub fn rule_files(data_dir: &Path) -> Result<Vec<String>> {
    let store = crate::setup::open_store(data_dir, 1024)?;
    let mut out: Vec<String> = store.read(|o| {
        let mut v = Vec::new();
        for f in o.local_rule_files()? {
            if let Some(parent) = f.parent_id
                && let Some(p) = o.local_path_of(parent)?
            {
                v.push(clean(&p));
            }
        }
        Ok(v)
    })?;
    out.sort_by_key(|p| p.to_lowercase());
    Ok(out)
}

/// Whether the item at `rel` syncs, as the rules say.
fn verdict(data_dir: &Path, store: &Store, rel: &str, folder: bool) -> Result<Verdict> {
    let parts: Vec<&str> = rel.split('/').filter(|p| !p.is_empty()).collect();
    let parent = &parts[..parts.len().saturating_sub(1)];
    let (set, _) = rules_at(data_dir, store, parent)?;
    Ok(set.verdict(rel, folder))
}

/// Makes the item at `rel` sync (`on`) or be left out, by changing your
/// own rules: leaving out adds a rule for exactly it; syncing again
/// removes such a rule, or adds an exception (`!/path`) when a preset or a
/// broader rule of yours leaves it out. Says what it did.
pub fn set_sync(data_dir: &Path, rel: &str, folder: bool, on: bool) -> Result<String> {
    let rel = clean(rel);
    if rel.is_empty() {
        bail!("that's the whole synced folder");
    }
    let store = crate::setup::open_store(data_dir, 1024)?;
    let mut rules = config::load_rules(data_dir, &store)?;
    let exact = rule_for(&rel, folder);
    let exception = format!("!{exact}");
    let same = |a: &str, b: &str| a.trim().eq_ignore_ascii_case(b);
    let before = rules.clone();
    if on {
        rules.exclude.retain(|r| !same(r, &exact));
    } else {
        rules.exclude.retain(|r| !same(r, &exception));
    }
    config::save_rules(data_dir, &rules)?;
    let now = verdict(data_dir, &store, &rel, folder)?;
    let done = |rules: &config::Rules| -> Result<()> { config::save_rules(data_dir, rules) };
    match (on, &now) {
        (true, v) if !v.is_excluded() => Ok(format!("/{rel} syncs again")),
        (true, Verdict::InsideExcluded { folder: f, .. }) => {
            done(&before)?;
            bail!("/{rel} is inside excluded /{f}: sync that folder first")
        }
        (true, Verdict::Excluded(m)) => match m.source {
            Source::Preset(_) | Source::Config => {
                rules.exclude.push(exception.clone());
                config::check_rule(&exception)?;
                done(&rules)?;
                if verdict(data_dir, &store, &rel, folder)?.is_excluded() {
                    done(&before)?;
                    bail!("/{rel} stays excluded by a later rule; edit Rules as text");
                }
                Ok(format!("/{rel} syncs again (exception to `{}`)", m.rule))
            }
            Source::IgnoreFile(ref d) => {
                done(&before)?;
                bail!("/{rel} is excluded by /{d}/.oneupignore; edit that file to sync it")
            }
            Source::Pause => {
                done(&before)?;
                bail!("/{rel} is paused: resume it instead")
            }
            Source::Vault => {
                done(&before)?;
                bail!("/{rel} is synced separately")
            }
            Source::OneDriveApp => {
                done(&before)?;
                bail!("/{rel} is synced by the OneDrive app (Office page)")
            }
        },
        (true, _) => Ok(format!("/{rel} syncs again")),
        (false, v) if v.is_excluded() => Ok(format!("/{rel} is excluded")),
        (false, _) => {
            config::check_rule(&exact)?;
            rules.exclude.push(exact);
            done(&rules)?;
            if !verdict(data_dir, &store, &rel, folder)?.is_excluded() {
                done(&before)?;
                bail!("an exception in a .oneupignore file keeps /{rel} syncing; edit that file to exclude it");
            }
            Ok(format!("/{rel} is excluded. Nothing is deleted."))
        }
    }
}

/// Pauses (`on`) or resumes the folder at `rel`.
pub fn set_paused(data_dir: &Path, rel: &str, on: bool) -> Result<String> {
    let rel = clean(rel);
    if rel.is_empty() {
        bail!("that's the whole folder: pause from the status menu");
    }
    let store = crate::setup::open_store(data_dir, 1024)?;
    let mut rules = config::load_rules(data_dir, &store)?;
    let same = |p: &String| clean(p).eq_ignore_ascii_case(&rel);
    if on && !rules.paused.iter().any(same) {
        rules.paused.push(rel.clone());
    } else if !on {
        rules.paused.retain(|p| !same(p));
    }
    config::save_rules(data_dir, &rules)?;
    Ok(if on { format!("/{rel} is paused") } else { format!("/{rel} syncs again") })
}

/// Which order the item at `rel` syncs in: `first`, `last` or `normal`.
pub fn order(data_dir: &Path, rel: &str, folder: bool) -> Result<&'static str> {
    let (first, last) = config::load_priority(data_dir)?;
    let r = rule_for(rel, folder);
    Ok(if first.iter().any(|x| x.eq_ignore_ascii_case(&r)) {
        "first"
    } else if last.iter().any(|x| x.eq_ignore_ascii_case(&r)) {
        "last"
    } else {
        "normal"
    })
}

/// Sets [`order`] for the item at `rel` (its own entry in `[priority]`).
pub fn set_order(data_dir: &Path, rel: &str, folder: bool, which: &str) -> Result<()> {
    let (mut first, mut last) = config::load_priority(data_dir)?;
    let r = rule_for(rel, folder);
    first.retain(|x| !x.eq_ignore_ascii_case(&r));
    last.retain(|x| !x.eq_ignore_ascii_case(&r));
    match which {
        "first" => first.push(r),
        "last" => last.push(r),
        "normal" => {}
        _ => bail!("first, last or normal"),
    }
    config::save_priority(data_dir, &first, &last)
}

fn shown(p: &Path) -> String {
    let s = p.to_string_lossy();
    s.strip_prefix(r"\\?\").unwrap_or(&s).to_string()
}

#[cfg(test)]
mod tests {
    #[test]
    fn rules_name_exactly_one_item() {
        assert_eq!(super::rule_for("a/b", true), "/a/b/");
        assert_eq!(super::rule_for("\\a\\c.txt", false), "/a/c.txt");
        assert_eq!(super::rule_for("x/[draft].docx", false), "/x/[[]draft].docx");
        let r = oneup_rules::RuleSet::new(&[], &[super::rule_for("x/[draft].docx", false)]).unwrap();
        assert!(r.verdict("x/[draft].docx", false).is_excluded());
        assert!(!r.verdict("x/d.docx", false).is_excluded());
        assert_eq!(super::clean("/a//b/./"), "a/b");
    }

    #[test]
    fn names_that_fold_alike_on_one_side_each_get_a_row() {
        use std::collections::BTreeMap;
        type Pair<'a> = (Option<&'a str>, Option<&'a str>);
        let mut rows: BTreeMap<(String, usize), Pair> = BTreeMap::new();
        for name in ["λογος", "ΛΟΓΟΣ"] {
            let key = super::free_slot(&rows, oneup_sync::fold(name), |b| b.0.is_some());
            rows.entry(key).or_default().0 = Some(name);
        }
        for name in ["λογοσ", "λογος"] {
            let key = super::free_slot(&rows, oneup_sync::fold(name), |b| b.1.is_some());
            rows.entry(key).or_default().1 = Some(name);
        }
        let got: Vec<_> = rows.into_values().collect();
        assert_eq!(got, [(Some("λογος"), Some("λογοσ")), (Some("ΛΟΓΟΣ"), Some("λογος"))]);
    }
}
