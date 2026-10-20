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

use std::collections::{HashMap, HashSet};
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

/// One folder on the way down to the one listed, as an earlier listing
/// gave it: the IDs of its rows on this PC and in OneDrive (`None`: not on
/// that side). Folders whose names fold alike are told apart by these;
/// names alone can't.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Step {
    pub local: Option<u128>,
    pub remote: Option<String>,
}

/// [`Step`]s as the window sends them: `[{ "l": "<id>" | null, "r":
/// "<id>" | null }, …]`, one per folder from the top.
pub fn steps_from(v: &Value) -> Result<Vec<Step>> {
    let Some(list) = v.as_array() else { return Ok(Vec::new()) };
    list.iter()
        .map(|s| {
            let local = match s["l"].as_str() {
                Some(id) => Some(id.parse().context("a folder's ID on this PC isn't a number")?),
                None => None,
            };
            Ok(Step { local, remote: s["r"].as_str().map(str::to_string) })
        })
        .collect()
}

/// Checks that `steps` (if any) name each folder of `parts`.
fn check_steps(parts: &[&str], steps: &[Step]) -> Result<()> {
    if !steps.is_empty() && steps.len() != parts.len() {
        bail!("the window's folder IDs don't match its path; list the folder again");
    }
    Ok(())
}

/// The rules that apply inside the folder at `parts` (config, folders
/// synced on their own, folders left to the OneDrive app, and every
/// `.oneupignore` on the way down), and that folder's Local row if it's
/// on this PC and listed. With `steps`, each folder on the way is the one
/// with that ID in its parent (none if it isn't there any more); without,
/// the one with that name.
fn rules_at(data_dir: &Path, store: &Store, parts: &[&str], steps: &[Step]) -> Result<(RuleSet, Option<LocalRow>)> {
    check_steps(parts, steps)?;
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
        let want = steps.get(i).map_or(Want::Name(part), |s| Want::Id(s.local.as_ref()));
        row = step(store.read(|o| o.local_children(dir.file_id))?, |c| &c.name, |c| &c.file_id, want);
    }
    Ok((set, row))
}

/// The cloud folder at `parts` below the pair's OneDrive folder (found as
/// in [`rules_at`]).
fn remote_at(store: &Store, parts: &[&str], steps: &[Step]) -> Result<Option<String>> {
    check_steps(parts, steps)?;
    let root = store.read(|o| o.local_root())?.context("this folder hasn't been looked at yet")?;
    let Some(mut cur) = store.read(|o| o.synced_by_local(root.root_file_id))?.and_then(|b| b.remote) else {
        return Ok(None);
    };
    for (i, part) in parts.iter().enumerate() {
        let folders: Vec<RemoteRow> = store
            .read(|o| o.remote_children(&cur.0))?
            .into_iter()
            .filter_map(|(r, excluded)| (r.kind == Kind::Folder && !excluded).then_some(r))
            .collect();
        let want = steps.get(i).map_or(Want::Name(part), |s| Want::Id(s.remote.as_deref()));
        match step(folders, |r| &r.name, |r| r.id.as_str(), want) {
            Some(r) => cur = oneup_sync::RemoteId(r.id),
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

/// The item named `part` among `items`: the one spelled exactly so, else
/// the only one whose name folds alike (as Windows compares names). Two
/// that fold alike and neither spelled so: none, rather than a guess.
fn pick<T>(items: Vec<T>, name: impl Fn(&T) -> &str, part: &str) -> Option<T> {
    let folded = oneup_sync::fold(part);
    let mut alike = Vec::new();
    for item in items {
        if name(&item) == part {
            return Some(item);
        }
        if oneup_sync::fold(name(&item)) == folded {
            alike.push(item);
        }
    }
    if alike.len() == 1 { alike.pop() } else { None }
}

/// How a path step is found.
#[derive(Debug, Clone, Copy)]
enum Want<'a, I: ?Sized> {
    /// By name ([`pick`]).
    Name(&'a str),
    /// By the ID an earlier listing gave (`None`: it had none on this side).
    Id(Option<&'a I>),
}

/// The item among `items` (one folder's) that `want` names. By ID, only
/// that one: one that's gone isn't replaced by another with a like name.
fn step<T, I: PartialEq + ?Sized>(
    items: Vec<T>,
    name: impl Fn(&T) -> &str,
    id: impl Fn(&T) -> &I,
    want: Want<'_, I>,
) -> Option<T> {
    match want {
        Want::Name(part) => pick(items, name, part),
        Want::Id(Some(want)) => items.into_iter().find(|t| id(t) == want),
        Want::Id(None) => None,
    }
}

/// A folder's items on this PC and in OneDrive side by side, one row per
/// item. An item with a Synced row (`link`: `Some`, with its partner's ID
/// if it has one; `rlinked` for an item in OneDrive) goes only with its
/// partner, and stands alone if the partner isn't in this folder (moved
/// elsewhere, or gone): a name says nothing about it. The others go with
/// one spelled the same, else with one whose name folds alike (as Windows
/// and OneDrive compare names), in turn. Names on one side that fold alike
/// (OneDrive may keep both) each get a row.
fn merge<'k, L, R>(
    locals: Vec<L>,
    remotes: Vec<R>,
    lname: impl Fn(&L) -> &str,
    rname: impl Fn(&R) -> &str,
    link: impl Fn(&L) -> Option<Option<&'k str>>,
    rid: impl Fn(&R) -> &str,
    rlinked: impl Fn(&R) -> bool,
) -> Vec<(Option<L>, Option<R>)> {
    use std::collections::{HashMap, VecDeque};
    let mut remotes: Vec<Option<R>> = remotes.into_iter().map(Some).collect();
    let by_id: HashMap<String, usize> =
        remotes.iter().enumerate().filter_map(|(i, r)| r.as_ref().map(|r| (rid(r).to_string(), i))).collect();
    let mut rows = Vec::new();
    let mut left = Vec::new();
    for l in locals {
        match link(&l) {
            Some(partner) => {
                let r = partner.and_then(|id| by_id.get(id)).and_then(|&i| remotes[i].take());
                rows.push((Some(l), r));
            }
            None => left.push(l),
        }
    }
    let mut exact: HashMap<String, usize> = HashMap::new();
    let mut alike: HashMap<String, VecDeque<usize>> = HashMap::new();
    for (i, r) in remotes.iter().enumerate() {
        // Linked with an item that isn't here: never matched by name.
        if let Some(r) = r.as_ref().filter(|r| !rlinked(r)) {
            exact.insert(rname(r).to_string(), i);
            alike.entry(oneup_sync::fold(rname(r))).or_default().push_back(i);
        }
    }
    let mut still = Vec::new();
    for l in left {
        match exact.get(lname(&l)).and_then(|&i| remotes[i].take()) {
            Some(r) => rows.push((Some(l), Some(r))),
            None => still.push(l),
        }
    }
    for l in still {
        let queue = alike.get_mut(&oneup_sync::fold(lname(&l)));
        let r = queue.and_then(|q| std::iter::from_fn(|| q.pop_front()).find_map(|i| remotes[i].take()));
        rows.push((Some(l), r));
    }
    rows.extend(remotes.into_iter().flatten().map(|r| (None, Some(r))));
    rows
}

/// The pair's folders and modes.
pub fn pair_info(store: &Store) -> Result<(PathBuf, String, bool)> {
    let pair = crate::sync::pairing(store)?;
    let token = store.read(|o| o.meta_get(crate::sync::KEY_TOKEN))?.unwrap_or_default();
    let on_demand = crate::ondemand::is_on(&pair.local, &token);
    Ok((pair.local, pair.remote, on_demand))
}

/// The folder at `rel` in data folder `data_dir`'s synced folder: its
/// items (folders first, then by name) with their state, and the IDs of
/// each item's rows (`local_id`, `remote_id`), which the window sends back
/// as `steps` (see [`rules_at`]; empty: by name).
pub fn list(data_dir: &Path, rel: &str, steps: &[Step]) -> Result<Value> {
    oneup_cfapi::expose_placeholders()?;
    let rel = clean(rel);
    let parts: Vec<&str> = rel.split('/').filter(|p| !p.is_empty()).collect();
    let store = crate::setup::open_store(data_dir, 4 * 1024)?;
    let (local_root, _, on_demand) = pair_info(&store)?;
    let (set, row) = rules_at(data_dir, &store, &parts, steps)?;
    let pins = config::load_rules(data_dir, &store)?.pin_set();
    // The folder itself: left out (or inside a left-out one) means its
    // contents aren't tracked.
    if !parts.is_empty() {
        let v = set.verdict(&rel, true);
        if v.is_excluded() {
            let (state, why) = state_of(&v, false, row.is_some(), false);
            return Ok(json!({ "rel": rel, "state": state, "why": why, "items": [], "untracked": true }));
        }
    }
    // The child folders holding a `.oneupignore` themselves or below (the
    // rule set above has only those on the way down): partly ticked. Up
    // from the rule files, as far as this folder, by the primary key.
    let ruled: HashSet<u128> = match &row {
        Some(r) => store.read(|o| o.local_children_with_rule_files(r.file_id))?.into_iter().collect(),
        None => HashSet::new(),
    };
    let (locals, links): (Vec<LocalRow>, HashMap<u128, Option<String>>) = match &row {
        Some(r) => (
            store.read(|o| o.local_children(r.file_id))?,
            store.read(|o| o.local_children_links(r.file_id))?.into_iter().collect(),
        ),
        None => (Vec::new(), HashMap::new()),
    };
    let (remotes, rlinks): (Vec<(RemoteRow, bool)>, HashSet<String>) = match remote_at(&store, &parts, steps)? {
        Some(id) => (
            store.read(|o| o.remote_children(&id))?,
            store.read(|o| o.remote_children_linked(&id))?.into_iter().collect(),
        ),
        None => (Vec::new(), HashSet::new()),
    };
    let here: Vec<LocalRow> =
        locals.into_iter().filter(|l| !(parts.is_empty() && oneup_sync::same_name(&l.name, ENGINE_DIR))).collect();
    let rows = merge(
        here,
        remotes,
        |l| &l.name,
        |r| &r.0.name,
        |l| links.get(&l.file_id).map(|r| r.as_deref()),
        |r| &r.0.id,
        |r| rlinks.contains(&r.0.id),
    );
    let total = rows.len();
    let mut items = Vec::new();
    for row in first_rows(rows, MAX_LISTED) {
        let (name, folder) = (row_name(&row).to_string(), row_is_folder(&row));
        let (l, r) = row;
        let link = l.as_ref().is_some_and(|l| l.kind == LocalKind::Link);
        let child = if rel.is_empty() { name.clone() } else { format!("{rel}/{name}") };
        let synced = match (&l, &r) {
            (Some(l), _) if links.contains_key(&l.file_id) => true,
            (_, Some(r)) => rlinks.contains(&r.0.id),
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
        // A ticked folder some rule inside leaves partly out (the tree's
        // partly ticked box). A `.oneupignore` below isn't read here (it may
        // exclude nothing: empty, `!` rules only, unreadable), so it's its
        // own mark, not a partly ticked box.
        let partial = folder && !verdict.is_excluded() && set.has_rules_inside(&child);
        let rule_file_below = folder && l.as_ref().is_some_and(|l| ruled.contains(&l.file_id));
        // The pin rule keeping it on this PC.
        let pinned_by = if on_demand { pins.pinned_by(&child, folder) } else { None };
        items.push(json!({
            "partial": partial,
            "rule_file_below": rule_file_below,
            "pinned_by": pinned_by,
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
            "local_id": l.as_ref().map(|l| l.file_id.to_string()),
            "remote_id": r.as_ref().map(|r| r.0.id.clone()),
        }));
    }
    Ok(json!({ "rel": rel, "items": items, "more": total.saturating_sub(MAX_LISTED), "untracked": false }))
}

type Row = (Option<LocalRow>, Option<(RemoteRow, bool)>);

/// The first `max` rows as listed, folders first, then by name. Cut before
/// any work per row (a rule verdict, a file's attributes), so a huge folder
/// costs no more than that.
fn first_rows(mut rows: Vec<Row>, max: usize) -> Vec<Row> {
    rows.sort_by_cached_key(|row| (!row_is_folder(row), row_name(row).to_lowercase()));
    rows.truncate(max);
    rows
}

/// A row's name: this PC's, else OneDrive's.
fn row_name(row: &Row) -> &str {
    match row {
        (Some(l), _) => &l.name,
        (None, Some(r)) => &r.0.name,
        (None, None) => "",
    }
}

/// Whether a row is a folder: as on this PC, else as in OneDrive.
fn row_is_folder(row: &Row) -> bool {
    match row {
        (Some(l), _) => matches!(l.kind, LocalKind::Folder | LocalKind::Root),
        (None, Some(r)) => r.0.kind == Kind::Folder,
        (None, None) => false,
    }
}

/// The total size of everything inside each folder of the folder at `rel`
/// (found as in [`list`]), by the IDs [`list`] gives: `{"local": {id:
/// bytes}, "remote": {id: bytes}}`. The window shows this PC's copy where
/// it has one (online-only files count at their full size), else
/// OneDrive's. A left-out folder isn't tracked inside, so it has no total.
/// Asked for after [`list`], as it reads the whole subtree.
pub fn sizes(data_dir: &Path, rel: &str, steps: &[Step]) -> Result<Value> {
    let rel = clean(rel);
    let parts: Vec<&str> = rel.split('/').filter(|p| !p.is_empty()).collect();
    let store = crate::setup::open_store(data_dir, 4 * 1024)?;
    let (_, row) = rules_at(data_dir, &store, &parts, steps)?;
    let mut remote = serde_json::Map::new();
    if let Some(id) = remote_at(&store, &parts, steps)? {
        for (id, bytes) in store.read(|o| o.remote_folder_totals(&id))? {
            remote.insert(id, Value::from(bytes));
        }
    }
    let mut local = serde_json::Map::new();
    if let Some(r) = row {
        for (id, bytes) in store.read(|o| o.local_folder_totals(r.file_id))? {
            local.insert(id.to_string(), Value::from(bytes));
        }
    }
    Ok(json!({ "local": local, "remote": remote }))
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
            if rel.split('/').next().is_some_and(|f| oneup_sync::same_name(f, ENGINE_DIR)) {
                continue;
            }
            if seen.insert(oneup_sync::fold(&rel)) {
                out.push(json!({ "rel": rel, "folder": l.kind != LocalKind::File }));
            }
        }
    }
    for r in store.read(|o| o.remote_find(needle, limit))? {
        let Some(p) = store.read(|o| o.path_of(&r.id))? else { continue };
        // Only what's inside this pair's OneDrive folder, cut by
        // characters as names fold (a cut in bytes could split one).
        let Some(rest) = base
            .as_ref()
            .and_then(|_| oneup_sync::strip_folded_prefix(&p, &base_path))
            .filter(|rest| !rest.is_empty() && (base_path.ends_with('/') || rest.starts_with('/')))
        else {
            continue;
        };
        let rel = clean(rest);
        if seen.insert(oneup_sync::fold(&rel)) {
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

/// What excluding the item at `rel` would take out of sync, before it's
/// done (the window's preview, `oneup rules --exclude-path`): what's inside
/// it on this PC (and how much of that is online-only) and in OneDrive,
/// whether it was synced, and what the setting for OneDrive's copies of
/// excluded items (`[rules] cloud_copies`) then does with them. `local` and
/// `remote` are the item's row IDs as a listing gave them (`None`: found
/// by name). Reads the database only; one pass over the item's subtree.
pub fn preview(data_dir: &Path, rel: &str, folder: bool, local: Option<u128>, remote: Option<String>) -> Result<Value> {
    let rel = clean(rel);
    if rel.is_empty() {
        bail!("that's the whole synced folder");
    }
    let store = crate::setup::open_store(data_dir, 4 * 1024)?;
    let (_, _, on_demand) = pair_info(&store)?;
    let rules = config::load_rules(data_dir, &store)?;
    let (local_row, remote_row) = if local.is_some() || remote.is_some() {
        let l = match local {
            Some(id) => store.read(|o| o.local_get(id))?,
            None => None,
        };
        let r = match remote {
            Some(id) => store.read(|o| o.get_item(&id))?,
            None => None,
        };
        (l, r)
    } else {
        by_name(&store, &rel)?
    };
    let here = match &local_row {
        Some(l) if folder => store.read(|o| o.local_subtree_totals(l.file_id))?,
        Some(l) => {
            let online = l.attributes & RECALL_ON_DATA_ACCESS != 0;
            let size = u64::try_from(l.size).unwrap_or(0);
            oneup_store::Totals {
                files: 1,
                bytes: size,
                online_files: u64::from(online),
                online_bytes: if online { size } else { 0 },
                ..Default::default()
            }
        }
        None => oneup_store::Totals::default(),
    };
    let cloud = match &remote_row {
        Some(r) if folder => {
            // OneDrive's own total for the folder covers what isn't listed.
            let mut t = store.read(|o| o.remote_subtree_totals(&r.id))?;
            t.bytes = t.bytes.max(u64::try_from(r.size).unwrap_or(0));
            t
        }
        Some(r) => oneup_store::Totals { files: 1, bytes: u64::try_from(r.size).unwrap_or(0), ..Default::default() },
        None => oneup_store::Totals::default(),
    };
    let synced = match (&local_row, &remote_row) {
        (Some(l), _) => store.read(|o| o.synced_by_local(l.file_id))?.is_some(),
        (None, Some(r)) => store.read(|o| o.synced_by_remote(&r.id))?.is_some(),
        _ => false,
    };
    let totals = |t: &oneup_store::Totals| {
        json!({ "files": t.files, "folders": t.folders, "bytes": t.bytes,
                "online_files": t.online_files, "online_bytes": t.online_bytes, "skipped": t.skipped })
    };
    Ok(json!({
        "rel": rel,
        "folder": folder,
        "rule": rule_for(&rel, folder),
        // What the preview was of: excluding with it refuses if the path
        // now names something else (`exclude_checked`).
        "token": token(&rel, folder, local_row.as_ref().map(|l| l.file_id), remote_row.as_ref().map(|r| r.id.as_str())),
        "here": local_row.as_ref().map(|_| totals(&here)),
        "cloud": remote_row.as_ref().map(|_| totals(&cloud)),
        "synced": synced,
        "on_demand": on_demand,
        "mode": rules.cloud.name(),
    }))
}

/// The items at `rel` (relative to the synced folder) on this PC and in
/// OneDrive, by name as Windows compares names.
fn by_name(store: &Store, rel: &str) -> Result<(Option<LocalRow>, Option<RemoteRow>)> {
    let root = store.read(|o| o.local_root())?.context("this folder hasn't been looked at yet")?;
    let l = store.read(|o| row_at(o, root.root_file_id, rel))?;
    let pair = crate::sync::pairing(store)?;
    let full = format!("{}/{rel}", pair.remote.trim_end_matches('/'));
    let (parent, name) = full.rsplit_once('/').unwrap_or(("", &full));
    let r = match crate::sync::find_remote(store, parent)? {
        Some(p) => store.read(|o| o.remote_named(&p, name))?.into_iter().next(),
        None => None,
    };
    Ok((l, r))
}

/// What a preview was of: the path, its kind, and the items' IDs on each
/// side (`null`: none there).
fn token(rel: &str, folder: bool, local: Option<u128>, remote: Option<&str>) -> Value {
    json!({ "rel": rel, "folder": folder, "local": local.map(|l| l.to_string()), "remote": remote })
}

/// Excludes the item a preview showed (`expect`, its `token`): only if the
/// path still names the same items on both sides, so a folder moved away
/// and another put in its place since isn't excluded (or, with `prune`,
/// removed from OneDrive) in its stead. Then as [`set_sync`].
pub fn exclude_checked(data_dir: &Path, expect: &Value) -> Result<String> {
    let rel = clean(expect["rel"].as_str().context("the preview names no item")?);
    let folder = expect["folder"].as_bool().context("the preview names no kind")?;
    let store = crate::setup::open_store(data_dir, 1024)?;
    let (l, r) = by_name(&store, &rel)?;
    let now = token(&rel, folder, l.as_ref().map(|l| l.file_id), r.as_ref().map(|r| r.id.as_str()));
    let kind_same = l.as_ref().is_none_or(|l| (l.kind != LocalKind::File) == folder)
        && r.as_ref().is_none_or(|r| (r.kind == Kind::Folder) == folder);
    if now != *expect || !kind_same {
        bail!("/{rel} isn't what was shown any more (moved or replaced); look again before excluding it");
    }
    drop(store);
    set_sync(data_dir, &rel, folder, false)
}

/// The Local row at `rel` (relative to the synced folder), by name as
/// Windows compares names: the exact spelling, else the only one alike.
pub fn row_at(o: &oneup_store::Ops<'_>, root: u128, rel: &str) -> oneup_store::Result<Option<LocalRow>> {
    let mut cur = root;
    let mut row = None;
    for part in rel.split('/').filter(|p| !p.is_empty()) {
        let mut found = o.local_named(cur, part)?;
        let pick = match found.iter().position(|r| r.name == part) {
            Some(i) => Some(found.swap_remove(i)),
            None if found.len() == 1 => found.pop(),
            None => None,
        };
        let Some(r) = pick else { return Ok(None) };
        cur = r.file_id;
        row = Some(r);
    }
    Ok(row)
}

/// Whether the item at `rel` syncs, as the rules say.
fn verdict(data_dir: &Path, store: &Store, rel: &str, folder: bool) -> Result<Verdict> {
    let parts: Vec<&str> = rel.split('/').filter(|p| !p.is_empty()).collect();
    let parent = &parts[..parts.len().saturating_sub(1)];
    let (set, _) = rules_at(data_dir, store, parent, &[])?;
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
    let same = |a: &str, b: &str| oneup_sync::same_name(a.trim(), b);
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
    let same = |p: &String| oneup_sync::same_name(&clean(p), &rel);
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
    Ok(if first.iter().any(|x| oneup_sync::same_name(x, &r)) {
        "first"
    } else if last.iter().any(|x| oneup_sync::same_name(x, &r)) {
        "last"
    } else {
        "normal"
    })
}

/// Sets [`order`] for the item at `rel` (its own entry in `[priority]`).
pub fn set_order(data_dir: &Path, rel: &str, folder: bool, which: &str) -> Result<()> {
    let (mut first, mut last) = config::load_priority(data_dir)?;
    let r = rule_for(rel, folder);
    first.retain(|x| !oneup_sync::same_name(x, &r));
    last.retain(|x| !oneup_sync::same_name(x, &r));
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

    /// `merge` of plain names (remote IDs are their names here). `links`:
    /// items here with the one in OneDrive they're synced with; `elsewhere`:
    /// items in OneDrive synced with an item here that isn't in this folder.
    fn merged(
        locals: &[&'static str],
        remotes: &[&'static str],
        links: &[(&str, &'static str)],
        elsewhere: &[&str],
    ) -> Vec<(Option<&'static str>, Option<&'static str>)> {
        let link = |l: &&str| links.iter().find(|(a, _)| a == l).map(|(_, r)| Some(*r));
        let rlinked = |r: &&str| links.iter().any(|(_, x)| x == r) || elsewhere.contains(r);
        super::merge(locals.to_vec(), remotes.to_vec(), |l| l, |r| r, link, |r| r, rlinked)
    }

    #[test]
    fn names_that_fold_alike_on_one_side_each_get_a_row() {
        let got = merged(&["λογος", "ΛΟΓΟΣ"], &["λογοσ", "λογος"], &[], &[]);
        // The same spelling first, then one that folds alike.
        assert_eq!(got, [(Some("λογος"), Some("λογος")), (Some("ΛΟΓΟΣ"), Some("λογοσ"))]);
        // One here, two in OneDrive that fold alike: the other gets its own
        // row, under its own name.
        let got = merged(&["λογος"], &["λογοσ", "λογος"], &[], &[]);
        assert_eq!(got, [(Some("λογος"), Some("λογος")), (None, Some("λογοσ"))]);
        // Only alike: paired in order, the rest alone.
        let got = merged(&["Kit"], &["KIT", "\u{212A}it"], &[], &[]);
        assert_eq!(got, [(Some("Kit"), Some("KIT")), (None, Some("\u{212A}it"))]);
    }

    #[test]
    fn an_item_goes_with_the_one_it_is_synced_with_before_any_name() {
        // Synced with the other spelling (a rename not sent yet): the link
        // wins over the same name.
        let got = merged(&["λογος"], &["λογος", "λογοσ"], &[("λογος", "λογοσ")], &[]);
        assert_eq!(got, [(Some("λογος"), Some("λογοσ")), (None, Some("λογος"))]);
    }

    #[test]
    fn an_item_moved_in_from_another_folder_never_pairs_by_name() {
        // Moved here on this PC (its OneDrive partner is still in the old
        // folder): it stands alone, beside the unrelated `a` in OneDrive.
        let got = merged(&["a"], &["A"], &[("a", "elsewhere")], &[]);
        assert_eq!(got, [(Some("a"), None), (None, Some("A"))]);
        // Synced, its partner gone: alone too.
        let link = |_: &&str| Some(None);
        let got = super::merge(vec!["a"], vec!["a"], |l| l, |r| r, link, |r| r, |_| false);
        assert_eq!(got, [(Some("a"), None), (None, Some("a"))]);
        // Moved here in OneDrive (its partner on this PC is still in the
        // old folder): not paired with the new `a` here.
        let got = merged(&["a"], &["a"], &[], &["a"]);
        assert_eq!(got, [(Some("a"), None), (None, Some("a"))]);
        // Both ways at once: each alone, never merged with the other's tree.
        let got = merged(&["a", "b"], &["a", "b"], &[("a", "x")], &["b"]);
        assert_eq!(got, [(Some("a"), None), (Some("b"), None), (None, Some("a")), (None, Some("b"))]);
        // Unlinked items beside them still pair by name.
        let got = merged(&["a", "c"], &["a", "C"], &[], &["a"]);
        assert_eq!(got, [(Some("a"), None), (Some("c"), Some("C")), (None, Some("a"))]);
    }

    #[test]
    fn a_path_step_by_id_takes_only_that_item() {
        use super::{Want, step};
        let items = vec![("λογος", 1u128), ("λογοσ", 2)];
        let by = |want| step(items.clone(), |t| t.0, |t| &t.1, want);
        assert_eq!(by(Want::Id(Some(&2))), Some(("λογοσ", 2)));
        assert_eq!(by(Want::Id(Some(&1))), Some(("λογος", 1)));
        // Gone since it was listed: not another with a like name.
        assert_eq!(by(Want::Id(Some(&3))), None);
        // Not on this side when listed: none, though a name folds alike.
        assert_eq!(by(Want::Id(None)), None);
        // By name: as before.
        assert_eq!(by(Want::Name("λογοσ")), Some(("λογοσ", 2)));
        assert_eq!(by(Want::Name("ΛΟΓΟΣ")), None);
    }

    #[test]
    fn steps_come_from_the_window_as_text_ids() {
        let v = serde_json::json!([{ "l": "340282366920938463463374607431768211455", "r": null }, { "l": null, "r": "A!1" }]);
        let got = super::steps_from(&v).unwrap();
        assert_eq!(
            got,
            [
                super::Step { local: Some(u128::MAX), remote: None },
                super::Step { local: None, remote: Some("A!1".into()) }
            ]
        );
        assert!(super::steps_from(&serde_json::json!([{ "l": "x" }])).is_err());
        assert!(super::steps_from(&serde_json::Value::Null).unwrap().is_empty());
    }

    #[test]
    fn rows_are_ordered_and_cut_before_anything_else() {
        use oneup_store::{Kind, LocalKind, LocalRow, RemoteRow, Skip};
        let local = |name: &str, kind| LocalRow {
            file_id: 1,
            parent_id: None,
            name: name.into(),
            name_lossy: false,
            kind,
            size: 0,
            mtime: 0,
            attributes: 0,
            reparse_tag: 0,
            skip: Skip::None,
        };
        let remote = |name: &str, kind| RemoteRow {
            id: name.into(),
            parent_id: None,
            name: name.into(),
            kind,
            size: 0,
            etag: None,
            qxh: None,
            mtime_ms: None,
        };
        let rows: Vec<super::Row> = vec![
            (Some(local("b.txt", LocalKind::File)), None),
            (None, Some((remote("Zeta", Kind::Folder), false))),
            (Some(local("a.txt", LocalKind::File)), Some((remote("a.txt", Kind::File), false))),
            (Some(local("alpha", LocalKind::Folder)), None),
        ];
        let names = |rows: Vec<super::Row>| rows.iter().map(|r| super::row_name(r).to_string()).collect::<Vec<_>>();
        assert_eq!(names(super::first_rows(rows.clone(), 10)), ["alpha", "Zeta", "a.txt", "b.txt"]);
        assert_eq!(names(super::first_rows(rows, 3)), ["alpha", "Zeta", "a.txt"]);
    }

    /// Two OneDrive folders whose names fold alike, both listed and opened
    /// by their IDs.
    #[test]
    fn cloud_folders_whose_names_fold_alike_each_open() {
        use oneup_store::{Kind, LocalKind, LocalRoot, LocalRow, RemoteRow, Skip, Store};
        use oneup_sync::{LocalId, NodeKind, ROOT, RemoteId};
        let mut s = Store::open_in_memory().unwrap();
        let row = |id: &str, parent: Option<&str>, name: &str, kind| RemoteRow {
            id: id.into(),
            parent_id: parent.map(Into::into),
            name: name.into(),
            kind,
            size: 0,
            etag: Some("e".into()),
            qxh: None,
            mtime_ms: None,
        };
        s.write(|o| {
            let root = LocalRow {
                file_id: 1,
                parent_id: None,
                name: String::new(),
                name_lossy: false,
                kind: LocalKind::Root,
                size: 0,
                mtime: 0,
                attributes: 0x10,
                reparse_tag: 0,
                skip: Skip::None,
            };
            let info = LocalRoot {
                path: r"C:\Sync".into(),
                volume_serial: 1,
                root_file_id: 1,
                rules: String::new(),
                journal: None,
                started_at_ms: 0,
                finished_at_ms: Some(0),
                sync_root_provider: None,
            };
            o.local_begin(&info, &root)?;
            o.upsert_item(&row("root", None, "", Kind::Root), false)?;
            o.upsert_item(&row("r1", Some("root"), "λογος", Kind::Folder), false)?;
            o.upsert_item(&row("r2", Some("root"), "λογοσ", Kind::Folder), false)?;
            o.upsert_item(&row("x", Some("root"), "left out", Kind::Folder), true)?;
            o.synced_put(&oneup_sync::Base {
                sid: ROOT,
                parent: None,
                name: String::new(),
                kind: NodeKind::Folder,
                size: 0,
                hash: None,
                local: Some(LocalId(1)),
                local_mtime: 0,
                remote: Some(RemoteId("root".into())),
                remote_etag: "e".into(),
            })
        })
        .unwrap();
        let at = |part: &str, remote: Option<&str>| {
            let steps = [super::Step { local: None, remote: remote.map(Into::into) }];
            super::remote_at(&s, &[part], &steps).unwrap()
        };
        assert_eq!(at("λογος", Some("r1")).as_deref(), Some("r1"));
        assert_eq!(at("λογος", Some("r2")).as_deref(), Some("r2"), "by ID, whatever the name");
        assert_eq!(at("λογοσ", Some("gone")), None);
        assert_eq!(at("left out", Some("x")), None, "an excluded folder isn't listed inside");
        // Without IDs, by name: the exact spelling, never a guess.
        assert_eq!(super::remote_at(&s, &["λογοσ"], &[]).unwrap().as_deref(), Some("r2"));
        assert_eq!(super::remote_at(&s, &["ΛΟΓΟΣ"], &[]).unwrap(), None);
        // IDs that don't match the path are refused.
        assert!(super::remote_at(&s, &["a", "b"], &[super::Step::default()]).is_err());
    }

    /// A data folder whose database has a scan of an empty synced folder,
    /// for the rule changes the tree's checkboxes make.
    fn data_dir(tag: &str) -> std::path::PathBuf {
        let d = std::env::temp_dir().join(format!("oneup-filetree-{tag}-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&d);
        let sync = d.join("sync");
        std::fs::create_dir_all(&sync).unwrap();
        let store = crate::setup::open_store(&d, 1024).unwrap();
        let cfg = oneup_local::Config::new(oneup_rules::ExcludeSet::none());
        let mut scan = oneup_local::Scan::open(store, &sync, cfg).unwrap();
        assert!(scan.run().unwrap().finished);
        drop(scan.into_store());
        d
    }

    /// [`data_dir`] with these files (`/`-separated, with their text) in
    /// the synced folder before its scan, paired with the whole drive.
    fn paired_dir(tag: &str, files: &[(&str, &str)]) -> std::path::PathBuf {
        let d = std::env::temp_dir().join(format!("oneup-filetree-{tag}-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&d);
        let sync = d.join("sync");
        for (rel, text) in files {
            let p = rel.split('/').fold(sync.clone(), |p, x| p.join(x));
            std::fs::create_dir_all(p.parent().unwrap()).unwrap();
            if !text.is_empty() || rel.ends_with(".oneupignore") {
                std::fs::write(&p, text).unwrap();
            } else {
                std::fs::create_dir_all(&p).unwrap();
            }
        }
        std::fs::create_dir_all(&sync).unwrap();
        let mut store = crate::setup::open_store(&d, 1024).unwrap();
        store
            .write(|o| {
                o.meta_set(crate::sync::KEY_LOCAL, &sync.to_string_lossy())?;
                o.meta_set(crate::sync::KEY_REMOTE, "/")
            })
            .unwrap();
        let cfg = oneup_local::Config::new(oneup_rules::ExcludeSet::none());
        let mut scan = oneup_local::Scan::open(store, &sync, cfg).unwrap();
        assert!(scan.run().unwrap().finished);
        drop(scan.into_store());
        d
    }

    /// The preview names what it showed; excluding with it (the window's
    /// `tree_sync`, the CLI's `--exclude-path --yes`) refuses once the path
    /// names another item: a folder moved away and another put in its place.
    #[test]
    fn excluding_after_a_preview_refuses_a_replaced_item() {
        let d = paired_dir("replaced", &[("Docs", "")]);
        let pv = super::preview(&d, "Docs", true, None, None).unwrap();
        let token = pv["token"].clone();
        assert!(token["local"].is_string(), "{pv}");
        // Docs is moved away and another folder takes its name.
        let mut store = crate::setup::open_store(&d, 1024).unwrap();
        let old: u128 = token["local"].as_str().unwrap().parse().unwrap();
        store
            .write(|o| {
                let mut row = o.local_get(old)?.unwrap();
                o.local_remove_subtree(old)?;
                row.file_id = old + 1;
                o.local_put(&row).map(|_| ())
            })
            .unwrap();
        drop(store);
        let args = serde_json::json!({ "rel": "Docs", "folder": true, "on": false, "expect": token });
        let err = crate::settingsapi::tree_sync(&d, &args).unwrap_err();
        assert!(err.to_string().contains("isn't what was shown"), "{err}");
        let store = crate::setup::open_store(&d, 1024).unwrap();
        assert!(crate::config::load_rules(&d, &store).unwrap().exclude.is_empty(), "nothing written");
        drop(store);
        // Previewed again: the new folder, and excluding it works.
        let pv = super::preview(&d, "Docs", true, None, None).unwrap();
        let args = serde_json::json!({ "rel": "Docs", "folder": true, "on": false, "expect": pv["token"] });
        crate::settingsapi::tree_sync(&d, &args).unwrap();
        let store = crate::setup::open_store(&d, 1024).unwrap();
        assert_eq!(crate::config::load_rules(&d, &store).unwrap().exclude, ["/Docs/"]);
        drop(store);
        let _ = std::fs::remove_dir_all(&d);
    }

    /// A `.oneupignore` below a folder that isn't read for the listing (it
    /// may exclude nothing) marks it as having one, not as partly ticked; a
    /// rule of yours inside it does make it partly ticked.
    #[test]
    fn a_rule_file_below_is_its_own_mark_not_a_partly_ticked_box() {
        let d =
            paired_dir("rulefile", &[("a/sub/.oneupignore", "!keep.txt\n"), ("b/x.txt", "x"), ("c/.oneupignore", "")]);
        std::fs::write(crate::config::path(&d), "[rules]\nexclude = [\"/b/x.txt\"]\n").unwrap();
        let v = super::list(&d, "", &[]).unwrap();
        let item = |name: &str| v["items"].as_array().unwrap().iter().find(|i| i["name"] == name).unwrap().clone();
        assert_eq!((item("a")["partial"].as_bool(), item("a")["rule_file_below"].as_bool()), (Some(false), Some(true)));
        assert_eq!((item("c")["partial"].as_bool(), item("c")["rule_file_below"].as_bool()), (Some(false), Some(true)));
        assert_eq!((item("b")["partial"].as_bool(), item("b")["rule_file_below"].as_bool()), (Some(true), Some(false)));
        let _ = std::fs::remove_dir_all(&d);
    }

    /// Unticking writes the rule naming exactly that folder, ticking takes
    /// it out again, and a tick against a preset writes an exception: all
    /// in `config.toml`, with the user's comments and other settings kept.
    #[test]
    fn ticks_write_rules_and_keep_the_users_comments() {
        let d = data_dir("ticks");
        let cfg = crate::config::path(&d);
        std::fs::write(
            &cfg,
            "# my settings\n[rules]\n# kept by hand\npresets = [\"dev\"]\nexclude = [\"*.iso\"]\n\n[agent]\nupload_kib_per_s = 7 # cap\n",
        )
        .unwrap();
        let store = crate::setup::open_store(&d, 1024).unwrap();
        let rules = || crate::config::load_rules(&d, &store).unwrap();
        let said = super::set_sync(&d, "Docs/Drafts", true, false).unwrap();
        assert!(said.contains("excluded") && said.contains("Nothing is deleted"), "{said}");
        assert_eq!(rules().exclude, ["*.iso", "/Docs/Drafts/"]);
        let text = std::fs::read_to_string(&cfg).unwrap();
        for kept in ["# my settings", "# kept by hand", "upload_kib_per_s = 7 # cap"] {
            assert!(text.contains(kept), "{kept}: {text}");
        }
        // Unticking it again changes nothing.
        super::set_sync(&d, "docs/drafts", true, false).unwrap();
        assert_eq!(rules().exclude, ["*.iso", "/Docs/Drafts/"]);

        super::set_sync(&d, "Docs/Drafts", true, true).unwrap();
        assert_eq!(rules().exclude, ["*.iso"]);
        // Left out by a preset: ticking adds an exception, unticking takes it out.
        let said = super::set_sync(&d, "app/node_modules", true, true).unwrap();
        assert!(said.contains("exception"), "{said}");
        assert_eq!(rules().exclude, ["*.iso", "!/app/node_modules/"]);
        assert!(!rules().rule_set().unwrap().excludes("app/node_modules/x.js", false));
        super::set_sync(&d, "app/node_modules", true, false).unwrap();
        assert_eq!(rules().exclude, ["*.iso"]);
        // Inside an excluded folder: refused, nothing written.
        super::set_sync(&d, "Docs", true, false).unwrap();
        assert!(super::set_sync(&d, "Docs/Keep", true, true).is_err());
        assert_eq!(rules().exclude, ["*.iso", "/Docs/"]);
        let text = std::fs::read_to_string(&cfg).unwrap();
        assert!(text.contains("# kept by hand") && text.contains("# cap"), "{text}");
        drop(store);
        let _ = std::fs::remove_dir_all(&d);
    }

    /// Pin rules round-trip through `config.toml` like the others, and a
    /// file without any keeps no `pin` key.
    #[test]
    fn pin_rules_round_trip_with_comments() {
        let d = data_dir("pins");
        let cfg = crate::config::path(&d);
        std::fs::write(&cfg, "[rules]\n# mine\nexclude = []\n").unwrap();
        let store = crate::setup::open_store(&d, 1024).unwrap();
        let r = crate::config::load_rules(&d, &store).unwrap();
        crate::config::save_rules(&d, &r).unwrap();
        assert!(!std::fs::read_to_string(&cfg).unwrap().contains("pin"), "no pin key until there are pin rules");
        let v = serde_json::json!({ "pin": ["*.psd", " /Work/ ", ""] });
        let r = crate::settingsapi::rules_with(r, &v).unwrap();
        crate::config::save_rules(&d, &r).unwrap();
        let back = crate::config::load_rules(&d, &store).unwrap();
        assert_eq!(back.pin, ["*.psd", "/Work/"]);
        assert!(back.pin_set().pins("Work/a.txt", false) && back.pin_set().pins("x/y.PSD", false));
        assert!(std::fs::read_to_string(&cfg).unwrap().contains("# mine"));
        assert!(crate::settingsapi::rules_with(back, &serde_json::json!({ "pin": ["# not a rule"] })).is_err());
        drop(store);
        let _ = std::fs::remove_dir_all(&d);
    }

    #[test]
    fn a_path_step_prefers_the_exact_name_and_refuses_a_guess() {
        let pick = |names: &[&'static str], part| super::pick(names.to_vec(), |n| n, part);
        // Exact spelling first, among names that fold alike.
        assert_eq!(pick(&["λογος", "λογοσ"], "λογοσ"), Some("λογοσ"));
        assert_eq!(pick(&["λογοσ", "λογος"], "λογος"), Some("λογος"));
        // Otherwise the one that folds alike, as Windows compares names.
        assert_eq!(pick(&["ΟΔΟΣ", "other"], "οδος"), Some("ΟΔΟΣ"));
        assert_eq!(pick(&["Kit"], "KIT"), Some("Kit"));
        // Not one that only lower-cases alike: the Kelvin sign isn't k.
        assert_eq!(pick(&["\u{212A}it"], "kit"), None);
        // Two alike and neither spelled so: no guess.
        assert_eq!(pick(&["λογος", "λογοσ"], "ΛΟΓΟΣ"), None);
    }
}
