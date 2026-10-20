//! Instant search across the whole drive (`oneup find`, the settings
//! window's Search page, the search hotkey): names from the store's index
//! (`oneup_store::search`), so online-only files are found without
//! downloading anything and without asking OneDrive. Each result says where
//! its content is. Optionally, and only when asked, OneDrive's own search
//! looks inside files too; that sends the search text to Microsoft.
//!
//! Reads the database and file attributes (never a file's content, and
//! never in a way that downloads an online-only file).

use std::path::{Path, PathBuf};

use anyhow::{Context, Result, bail};
use oneup_store::{Kind, Store};
use serde_json::{Value, json};

pub use oneup_store::search::MIN_CHARS;

/// Results shown at most, unless asked for more.
pub const DEFAULT_LIMIT: usize = 50;

/// What every result list says, because the index can't do better.
pub const LIMIT_NOTE: &str = crate::t_key!("Items inside excluded folders aren't searched.");

/// Recall on data access (Cloud Files): an online-only file.
const RECALL_ON_DATA_ACCESS: u32 = 0x0040_0000;

/// Where a result's content is.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Placement {
    /// On this PC.
    Here,
    /// On this PC as an online-only file: opening it downloads it.
    OnlineOnly,
    /// In the synced folder, but not on this PC (yet).
    NotHereYet,
    /// Left out by the rules (for a folder: its contents aren't listed).
    Excluded,
    /// In OneDrive, outside the synced folder.
    NotSynced,
    /// A OneNote notebook or the Personal Vault: never synced (they work
    /// only in OneDrive, `planner.md`), so only OneDrive can open it.
    InOneDrive,
}

impl Placement {
    pub fn name(self) -> &'static str {
        match self {
            Placement::Here => "here",
            Placement::OnlineOnly => "online-only",
            Placement::NotHereYet => "not here yet",
            Placement::Excluded => "excluded",
            Placement::NotSynced => "not synced",
            Placement::InOneDrive => "in OneDrive",
        }
    }
}

/// One item found.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Item {
    /// Its OneDrive ID, to open it there ([`open_in_onedrive`]).
    pub id: String,
    /// Its path in OneDrive (`/Documents/Budget.xlsx`).
    pub path: String,
    pub name: String,
    pub folder: bool,
    /// Files only.
    pub size: Option<i64>,
    pub modified_ms: Option<i64>,
    pub placement: Placement,
    /// Its path inside the synced folder (`Documents/Budget.xlsx`), if it's
    /// in it: what "sync this first" names.
    pub rel: Option<String>,
    /// Where it is (or would be) on this PC, if it's in the synced folder.
    pub local: Option<PathBuf>,
    /// Found by OneDrive's own search (inside the file), not by name here.
    pub by_onedrive: bool,
}

/// What a search found.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Found {
    pub items: Vec<Item>,
    /// More match than were listed.
    pub more: bool,
    /// Too many names hold the text's letters apart for the search to read
    /// them all: some matches may be missing ([`oneup_store::Found::too_many`]).
    pub too_many: bool,
}

/// What a search says when it gave up ([`Found::too_many`]).
pub const TOO_MANY: &str = crate::t_key!("Too many matches. Type more.");

/// The synced folder pair and the rules, to tell each result's placement.
struct Pair {
    local: Option<PathBuf>,
    /// The OneDrive folder synced (`/` for the whole drive).
    remote: Option<String>,
    rules: Option<oneup_rules::RuleSet>,
}

impl Pair {
    fn load(dir: &Path, store: &Store) -> Pair {
        let local = store.read(|o| o.meta_get(crate::sync::KEY_LOCAL)).ok().flatten().map(PathBuf::from);
        let remote = store.read(|o| o.meta_get(crate::sync::KEY_REMOTE)).ok().flatten();
        // Rules that can't be read leave placement to the files themselves.
        let rules = crate::rulefiles::load(dir, store, &mut Default::default()).ok().map(|(r, _)| r);
        Pair { local, remote, rules }
    }

    /// `path`'s place inside the synced folder (`a/b`, `""` for the folder
    /// itself), if it's in it.
    fn rel(&self, path: &str) -> Option<String> {
        let remote = self.remote.as_deref()?;
        let rest = if remote.trim_end_matches('/').is_empty() {
            path
        } else {
            oneup_sync::strip_folded_prefix(path, remote).filter(|r| r.is_empty() || r.starts_with('/'))?
        };
        Some(rest.trim_matches('/').to_string())
    }

    fn item(&self, row: &oneup_store::RemoteRow, excluded: bool, path: String) -> Item {
        let folder = matches!(row.kind, Kind::Folder | Kind::Root);
        // Never synced, so nothing of it is (or will be) on this PC.
        let package = row.kind == Kind::Package;
        let rel = self.rel(&path).filter(|_| !package);
        let local = match (&self.local, &rel) {
            (Some(l), Some(r)) => Some(r.split('/').filter(|p| !p.is_empty()).fold(l.clone(), |p, x| p.join(x))),
            _ => None,
        };
        let ruled_out = |r: &str| self.rules.as_ref().is_some_and(|set| set.verdict(r, folder).is_excluded());
        let placement = match (&rel, &local) {
            _ if package => Placement::InOneDrive,
            _ if excluded => Placement::Excluded,
            (None, _) | (_, None) => Placement::NotSynced,
            (Some(r), _) if !r.is_empty() && ruled_out(r) => Placement::Excluded,
            // Attributes only: reading them never downloads anything.
            (_, Some(l)) => match std::fs::symlink_metadata(l) {
                Ok(m) if attributes(&m) & RECALL_ON_DATA_ACCESS != 0 => Placement::OnlineOnly,
                Ok(_) => Placement::Here,
                Err(_) => Placement::NotHereYet,
            },
        };
        Item {
            id: row.id.clone(),
            path,
            name: row.name.clone(),
            folder,
            size: (!folder).then_some(row.size),
            modified_ms: row.mtime_ms,
            placement,
            local: local.map(|p| PathBuf::from(shown(&p))),
            rel: rel.filter(|r| !r.is_empty()),
            by_onedrive: false,
        }
    }
}

impl Pair {
    /// A result of OneDrive's search that the mirror doesn't hold: placed
    /// by the path OneDrive gives (`parentReference.path`, percent-encoded,
    /// as `/drive/root:/a%20b`) and the rules, like any other; "not here
    /// yet" when OneDrive gives no path.
    fn unlisted(&self, it: &oneup_graph::model::DriveItem) -> Item {
        let name = it.name.clone().unwrap_or_default();
        let folder = it.folder.is_some();
        let kind = if it.package.is_some() {
            Kind::Package
        } else if folder {
            Kind::Folder
        } else {
            Kind::File
        };
        let row = oneup_store::RemoteRow {
            id: it.id.clone(),
            parent_id: None,
            name: name.clone(),
            kind,
            size: it.size.unwrap_or(0),
            etag: None,
            qxh: None,
            mtime_ms: None,
        };
        let parent = it.parent_reference.as_ref().and_then(|p| p.path.as_deref()).and_then(|p| p.split_once(':'));
        match parent {
            Some((_, parent)) => {
                self.item(&row, false, format!("{}/{name}", percent_decoded(parent).trim_end_matches('/')))
            }
            None => Item {
                id: it.id.clone(),
                path: name.clone(),
                name,
                folder,
                size: if folder { None } else { it.size },
                modified_ms: None,
                placement: if kind == Kind::Package { Placement::InOneDrive } else { Placement::NotHereYet },
                rel: None,
                local: None,
                by_onedrive: true,
            },
        }
    }
}

/// `a%20b` → `a b`: Graph percent-encodes the paths it gives.
fn percent_decoded(s: &str) -> String {
    let b = s.as_bytes();
    let mut out = Vec::with_capacity(b.len());
    let mut i = 0;
    while i < b.len() {
        let hex = (b[i] == b'%' && i + 2 < b.len() && b[i + 1..i + 3].iter().all(u8::is_ascii_hexdigit))
            .then(|| std::str::from_utf8(&b[i + 1..i + 3]).ok().and_then(|h| u8::from_str_radix(h, 16).ok()))
            .flatten();
        match hex {
            Some(v) => {
                out.push(v);
                i += 3;
            }
            None => {
                out.push(b[i]);
                i += 1;
            }
        }
    }
    String::from_utf8_lossy(&out).into_owned()
}

fn attributes(m: &std::fs::Metadata) -> u32 {
    std::os::windows::fs::MetadataExt::file_attributes(m)
}

/// `\\?\C:\x` → `C:\x`, as people know paths.
fn shown(p: &Path) -> String {
    let s = p.to_string_lossy();
    s.strip_prefix(r"\\?\").unwrap_or(&s).to_string()
}

/// Checks what was typed: trimmed, at least [`MIN_CHARS`] characters.
pub fn pattern(text: &str) -> Result<&str> {
    let t = text.trim();
    if t.chars().count() < MIN_CHARS {
        bail!("type at least {MIN_CHARS} characters");
    }
    Ok(t)
}

/// Opens the data folder's database and makes sure the index holds every
/// item: on the first search after an upgrade (unless the agent got there
/// first) that adds the items stored before, calling `progress` with the
/// count so far. Once built this reads one setting.
pub fn open(dir: &Path, mut progress: impl FnMut(u64)) -> Result<Store> {
    let mut store = crate::setup::open_store(dir, 4 * 1024)?;
    store
        .search_build(oneup_store::search::BUILD_STEP, |n| {
            progress(n);
            true
        })
        .context("building the search index")?;
    Ok(store)
}

/// The agent's part: on its first start after an upgrade, adds the items
/// stored before the index to it, in short steps, until done or
/// `keep_going` says stop (paused, or stopping); the next start, or the
/// first search, carries on from there. Noted in the activity log once
/// done. Nothing to do (one setting read) once built.
pub fn build_index(store: &mut Store, mut keep_going: impl FnMut() -> bool) -> Result<()> {
    if store.read(|o| o.search_state())? == oneup_store::IndexState::Ready {
        return Ok(());
    }
    let started = std::time::Instant::now();
    let n = store.search_build(oneup_store::search::BUILD_STEP, |_| keep_going())?;
    if store.read(|o| o.search_state())? != oneup_store::IndexState::Ready {
        return Ok(());
    }
    let detail = format!("search index built: {n} items in {} s", started.elapsed().as_secs());
    println!("oneupd: {detail}");
    let entry = oneup_store::ActivityEntry {
        seq: 0,
        at_ms: crate::sync::now_ms(),
        outcome: oneup_store::ActivityOutcome::Note,
        action: "search index".into(),
        path: String::new(),
        detail,
    };
    store.write(|o| o.activity_add(&[entry]))?;
    Ok(())
}

/// Items in OneDrive whose name contains `text` (any case), up to `limit`,
/// best matches first, each with its placement.
pub fn find(dir: &Path, store: &Store, text: &str, limit: usize) -> Result<Found> {
    let text = pattern(text)?;
    // Attributes as they are (AGENTS.md).
    oneup_cfapi::expose_placeholders()?;
    let pair = Pair::load(dir, store);
    let found = store.read(|o| o.search_names(text, limit))?;
    let mut items = Vec::with_capacity(found.hits.len());
    for h in found.hits {
        // An item whose parents aren't all stored yet has no path to show.
        if let Some(path) = store.read(|o| o.path_of(&h.row.id))? {
            items.push(pair.item(&h.row, h.excluded, path));
        }
    }
    Ok(Found { items, more: found.more, too_many: found.too_many })
}

/// OneDrive's own search, inside files too ([`oneup_graph::Graph::search`]):
/// sends `text` to Microsoft, so only on the user's say-so. Results the
/// mirror holds get their path and placement from it; the others (inside
/// an excluded folder, or newer than the last look at OneDrive) from the
/// path OneDrive gives ([`Pair::unlisted`]).
pub async fn find_in_onedrive(dir: &Path, graph: &oneup_graph::Graph, text: &str, limit: usize) -> Result<Found> {
    let text = pattern(text)?;
    let top = u32::try_from(limit.clamp(1, 200)).unwrap_or(50);
    let found = graph.search(text, top).await.context("asking OneDrive")?;
    // Attributes as they are (AGENTS.md).
    oneup_cfapi::expose_placeholders()?;
    let store = crate::setup::open_store(dir, 1024)?;
    let pair = Pair::load(dir, &store);
    let mut items = Vec::new();
    for it in &found {
        let row = store.read(|o| o.get_item(&it.id))?;
        let path = store.read(|o| o.path_of(&it.id))?;
        let mut item = match (row, path) {
            (Some(row), Some(path)) => {
                let excluded = store.read(|o| o.item_state(&it.id))? == oneup_store::ItemState::Excluded;
                pair.item(&row, excluded, path)
            }
            _ => pair.unlisted(it),
        };
        item.by_onedrive = true;
        items.push(item);
    }
    Ok(Found { more: found.len() >= top as usize, items, too_many: false })
}

/// The Graph client OneDrive's search and web pages go through for data
/// folder `dir`: an extra pair's is scoped to its OneDrive folder
/// (`crate::pairs::scoped`), in another person's drive for a shared one,
/// so a search stays inside the pair and its items are found there.
pub fn graph(dir: &Path, client_id: Option<&str>) -> Result<oneup_graph::Graph> {
    let http = crate::setup::http()?;
    let auth = crate::setup::auth(dir, client_id, &http)?;
    if !auth.is_signed_in()? {
        bail!("sign in first (`oneup login`)");
    }
    crate::pairs::scoped(dir, oneup_graph::Graph::new(http, auth, oneup_graph::Aimd::new(2, 4)))
}

/// Opens item `id` on OneDrive's website: for what has no copy on this
/// PC, like a OneNote notebook, or a result of OneDrive's own search the
/// mirror doesn't hold yet. Only reads the item's address.
pub async fn open_in_onedrive(graph: &oneup_graph::Graph, id: &str) -> Result<()> {
    let url = graph.web_url(id).await?.context("OneDrive has no web page for it (any more); search again")?;
    if !url.starts_with("https://") {
        bail!("unexpected address from OneDrive");
    }
    crate::item::launch(&url)
}

/// [`Found`] as the window and `oneup find --json` give it.
pub fn to_json(f: &Found) -> Value {
    let items: Vec<Value> = f
        .items
        .iter()
        .map(|i| {
            json!({
                "id": i.id,
                "path": i.path,
                "name": i.name,
                "folder": i.folder,
                "size": i.size,
                "modified_ms": i.modified_ms,
                "placement": i.placement.name(),
                "rel": i.rel,
                "local": i.local.as_ref().map(|p| p.to_string_lossy().into_owned()),
                "onedrive": i.by_onedrive,
            })
        })
        .collect();
    json!({
        "items": items,
        "more": f.more,
        "too_many": f.too_many.then_some(TOO_MANY),
        "note": LIMIT_NOTE,
    })
}

/// The key combination that opens search from anywhere (`[search] hotkey`):
/// modifiers and one key, as `Ctrl+Alt+F`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Hotkey {
    pub ctrl: bool,
    pub alt: bool,
    pub shift: bool,
    pub win: bool,
    /// The key's Windows virtual-key code.
    pub key: u32,
}

impl Hotkey {
    /// `Ctrl+Alt+F`, `Win+Shift+S`, `Ctrl+Alt+Space`, `Alt+F12`: at least
    /// Ctrl, Alt or Win (a key alone, or with Shift, would take typing
    /// away from every program), and a letter, digit, F1 to F24 or Space.
    pub fn parse(text: &str) -> Result<Hotkey> {
        let mut h = Hotkey { ctrl: false, alt: false, shift: false, win: false, key: 0 };
        for part in text.split('+').map(str::trim) {
            let p = part.to_ascii_lowercase();
            match p.as_str() {
                "ctrl" | "control" => h.ctrl = true,
                "alt" => h.alt = true,
                "shift" => h.shift = true,
                "win" | "windows" => h.win = true,
                _ if h.key != 0 => bail!("one key besides Ctrl, Alt, Shift and Win"),
                "space" => h.key = 0x20,
                _ if p.len() == 1 && p.as_bytes()[0].is_ascii_alphanumeric() => {
                    h.key = u32::from(p.as_bytes()[0].to_ascii_uppercase());
                }
                _ if p.starts_with('f') && p[1..].parse::<u32>().is_ok_and(|n| (1..=24).contains(&n)) => {
                    h.key = 0x6F + p[1..].parse::<u32>().unwrap_or(1);
                }
                _ => bail!("`{part}` isn't a key: use a letter, a digit, F1 to F24 or Space"),
            }
        }
        if h.key == 0 {
            bail!("add a key, as in Ctrl+Alt+F");
        }
        if !(h.ctrl || h.alt || h.win) {
            bail!("add Ctrl, Alt or Win, as in Ctrl+Alt+F");
        }
        Ok(h)
    }
}

impl Hotkey {
    /// Windows' `MOD_*` flags for it (`oneup_sys::tray::Tray::set_hotkey`).
    pub fn modifiers(&self) -> u32 {
        u32::from(self.alt) | u32::from(self.ctrl) << 1 | u32::from(self.shift) << 2 | u32::from(self.win) << 3
    }
}

impl std::fmt::Display for Hotkey {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        for (on, name) in [(self.ctrl, "Ctrl+"), (self.alt, "Alt+"), (self.shift, "Shift+"), (self.win, "Win+")] {
            if on {
                f.write_str(name)?;
            }
        }
        match self.key {
            0x20 => f.write_str("Space"),
            k @ 0x70..=0x87 => write!(f, "F{}", k - 0x6F),
            k => write!(f, "{}", char::from_u32(k).unwrap_or('?')),
        }
    }
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used)]
    use super::*;

    #[test]
    fn hotkeys_need_a_modifier_and_one_key() {
        let h = Hotkey::parse("ctrl + alt + f").unwrap();
        assert_eq!((h.ctrl, h.alt, h.shift, h.win, h.key), (true, true, false, false, u32::from(b'F')));
        assert_eq!(h.to_string(), "Ctrl+Alt+F");
        assert_eq!(h.modifiers(), 0b0011, "MOD_ALT | MOD_CONTROL");
        assert_eq!(Hotkey::parse("Win+Shift+Space").unwrap().to_string(), "Shift+Win+Space");
        assert_eq!(Hotkey::parse("Alt+F12").unwrap().key, 0x7B);
        assert_eq!(Hotkey::parse("Alt+F12").unwrap().to_string(), "Alt+F12");
        assert_eq!(Hotkey::parse("Ctrl+7").unwrap().key, u32::from(b'7'));
        for bad in ["F", "Shift+F", "Ctrl+Alt", "Ctrl+F+G", "Ctrl+F25", "Ctrl+Esc", ""] {
            assert!(Hotkey::parse(bad).is_err(), "{bad}");
        }
    }

    /// The agent builds the index for what an older store held, notes it
    /// in the activity log, and has nothing to do at later starts.
    #[test]
    fn the_agent_builds_the_index_once_and_says_so() {
        let dir = std::env::temp_dir().join(format!("oneup-search-build-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        let mut store = crate::setup::open_store(&dir, 1024).unwrap();
        store
            .write(|o| {
                for (id, parent, name, kind) in [
                    ("r", None, "", Kind::Root),
                    ("a", Some("r"), "alpha.txt", Kind::File),
                    ("b", Some("r"), "beta", Kind::Folder),
                ] {
                    let row = oneup_store::RemoteRow {
                        id: id.into(),
                        parent_id: parent.map(Into::into),
                        name: name.into(),
                        kind,
                        size: 1,
                        etag: Some("e".into()),
                        qxh: None,
                        mtime_ms: None,
                    };
                    o.upsert_item(&row, false)?;
                }
                // As a store from before the index is.
                o.search_reset()
            })
            .unwrap();
        assert_eq!(store.read(|o| o.search_state()).unwrap(), oneup_store::IndexState::Building);
        let notes = |s: &Store| {
            s.read(|o| o.activity_recent(None, 10)).unwrap().into_iter().filter(|e| e.action == "search index").count()
        };
        build_index(&mut store, || true).unwrap();
        assert_eq!(store.read(|o| o.search_state()).unwrap(), oneup_store::IndexState::Ready);
        assert_eq!(store.read(|o| o.search_names("alpha", 5)).unwrap().hits.len(), 1);
        assert_eq!(notes(&store), 1);
        build_index(&mut store, || true).unwrap();
        assert_eq!(notes(&store), 1, "nothing to do once built");
        drop(store);
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// An agent that's paused (or stopping) when it starts adds nothing to
    /// the index, not even one step.
    #[test]
    fn a_paused_agent_builds_nothing() {
        let dir = std::env::temp_dir().join(format!("oneup-search-paused-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        let mut store = crate::setup::open_store(&dir, 1024).unwrap();
        store
            .write(|o| {
                for (id, parent, name, kind) in [("r", None, "", Kind::Root), ("a", Some("r"), "alpha.txt", Kind::File)]
                {
                    let row = oneup_store::RemoteRow {
                        id: id.into(),
                        parent_id: parent.map(Into::into),
                        name: name.into(),
                        kind,
                        size: 1,
                        etag: Some("e".into()),
                        qxh: None,
                        mtime_ms: None,
                    };
                    o.upsert_item(&row, false)?;
                }
                o.search_reset()
            })
            .unwrap();
        let mut asked = 0;
        build_index(&mut store, || {
            asked += 1;
            false
        })
        .unwrap();
        assert_eq!(asked, 1, "asked before the first step");
        assert_eq!(store.read(|o| o.search_state()).unwrap(), oneup_store::IndexState::Building);
        assert!(store.read(|o| o.search_names("alpha", 5)).unwrap().hits.is_empty(), "nothing added");
        assert!(store.read(|o| o.activity_recent(None, 10)).unwrap().iter().all(|e| e.action != "search index"));
        drop(store);
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// OneDrive's search can return what the mirror doesn't hold yet: it's
    /// placed by the path OneDrive gives and the rules, not called excluded.
    #[test]
    fn onedrive_results_the_mirror_lacks_are_placed_by_their_path() {
        let dir = std::env::temp_dir().join(format!("oneup-search-unlisted-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(dir.join("Docs")).unwrap();
        std::fs::write(dir.join("Docs").join("here.txt"), b"x").unwrap();
        let pair = Pair {
            local: Some(dir.clone()),
            remote: Some("/Sync".into()),
            rules: Some(oneup_rules::RuleSet::new(&[], &["node_modules/".to_string()]).unwrap()),
        };
        let item = |v: Value| pair.unlisted(&serde_json::from_value(v).unwrap());
        let at = |path: &str, name: &str| {
            item(json!({ "id": "x", "name": name, "size": 3, "file": {}, "parentReference": { "path": path } }))
        };
        assert_eq!(percent_decoded("/My%20Docs/caf%C3%A9/100%25/50%"), "/My Docs/café/100%/50%");
        let spaced = at("/drive/root:/Sync/My%20Docs", "x.txt");
        assert_eq!((spaced.path.as_str(), spaced.rel.as_deref()), ("/Sync/My Docs/x.txt", Some("My Docs/x.txt")));
        let new = at("/drive/root:/Sync/Docs", "new upload.txt");
        assert_eq!((new.placement, new.rel.as_deref()), (Placement::NotHereYet, Some("Docs/new upload.txt")));
        assert_eq!(at("/drive/root:/Sync/Docs", "here.txt").placement, Placement::Here);
        assert_eq!(at("/drive/root:/Sync/app/node_modules/pkg", "a.js").placement, Placement::Excluded);
        assert_eq!(at("/drive/root:/Elsewhere", "b.txt").placement, Placement::NotSynced);
        let unknown = item(json!({ "id": "y", "name": "c.txt", "file": {} }));
        assert_eq!((unknown.placement, unknown.path.as_str()), (Placement::NotHereYet, "c.txt"));
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// A OneNote notebook (a package) is never synced: it says it's in
    /// OneDrive, with no place on this PC and nothing to sync first.
    #[test]
    fn notebooks_are_in_onedrive_only() {
        let dir = std::env::temp_dir().join(format!("oneup-search-package-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(dir.join("Notes")).unwrap();
        let pair = Pair { local: Some(dir.clone()), remote: Some("/".into()), rules: None };
        let row = oneup_store::RemoteRow {
            id: "nb".into(),
            parent_id: Some("r".into()),
            name: "Notes".into(),
            kind: Kind::Package,
            size: 5,
            etag: None,
            qxh: None,
            mtime_ms: None,
        };
        // Even with a folder of that name here (a stand-in): it isn't ours.
        let it = pair.item(&row, false, "/Notes".into());
        assert_eq!((it.placement, it.rel.as_deref(), it.local.as_ref()), (Placement::InOneDrive, None, None));
        assert_eq!(to_json(&Found { items: vec![it], ..Default::default() })["items"][0]["id"], "nb");
        // Found by OneDrive's search, not in the mirror.
        let v = json!({ "id": "nb2", "name": "Work", "package": {}, "parentReference": { "path": "/drive/root:" } });
        let it = pair.unlisted(&serde_json::from_value(v).unwrap());
        assert_eq!((it.placement, it.rel), (Placement::InOneDrive, None));
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn a_search_needs_three_characters() {
        assert_eq!(pattern("  tax ").unwrap(), "tax");
        assert!(pattern(" ab ").is_err());
    }

    /// Each result says where its content is: here, online-only, not here
    /// yet, excluded (by the rules, or an excluded folder), or outside the
    /// synced folder.
    #[test]
    fn results_say_where_each_item_is() {
        let dir = std::env::temp_dir().join(format!("oneup-search-placement-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        let local = dir.join("Synced");
        std::fs::create_dir_all(local.join("Docs")).unwrap();
        std::fs::write(local.join("Docs").join("tax 2024.pdf"), b"x").unwrap();
        let mut store = crate::setup::open_store(&dir, 1024).unwrap();
        let row = |id: &str, parent: Option<&str>, name: &str, kind| oneup_store::RemoteRow {
            id: id.into(),
            parent_id: parent.map(Into::into),
            name: name.into(),
            kind,
            size: 7,
            etag: Some("e".into()),
            qxh: None,
            mtime_ms: Some(1),
        };
        store
            .write(|o| {
                o.meta_set(crate::sync::KEY_LOCAL, &local.to_string_lossy())?;
                o.meta_set(crate::sync::KEY_REMOTE, "/Sync")?;
                o.upsert_item(&row("r", None, "", Kind::Root), false)?;
                o.upsert_item(&row("s", Some("r"), "Sync", Kind::Folder), false)?;
                o.upsert_item(&row("d", Some("s"), "Docs", Kind::Folder), false)?;
                o.upsert_item(&row("a", Some("d"), "tax 2024.pdf", Kind::File), false)?;
                o.upsert_item(&row("b", Some("d"), "tax 2023.pdf", Kind::File), false)?;
                o.upsert_item(&row("c", Some("s"), "tax.iso", Kind::File), false)?;
                o.upsert_item(&row("n", Some("s"), "node_modules", Kind::Folder), true)?;
                o.upsert_item(&row("o", Some("r"), "tax 2019.pdf", Kind::File), false)
            })
            .unwrap();
        crate::config::save_rules(&dir, &crate::config::Rules { exclude: vec!["*.iso".into()], ..Default::default() })
            .unwrap();
        let found = find(&dir, &store, "tax", 10).unwrap();
        let by_path = |p: &str| found.items.iter().find(|i| i.path == p).unwrap();
        assert_eq!(by_path("/Sync/Docs/tax 2024.pdf").placement, Placement::Here);
        assert_eq!(by_path("/Sync/Docs/tax 2024.pdf").rel.as_deref(), Some("Docs/tax 2024.pdf"));
        assert_eq!(by_path("/Sync/Docs/tax 2023.pdf").placement, Placement::NotHereYet);
        assert_eq!(by_path("/Sync/tax.iso").placement, Placement::Excluded);
        assert_eq!(by_path("/tax 2019.pdf").placement, Placement::NotSynced);
        assert_eq!(by_path("/tax 2019.pdf").rel, None);
        let x = find(&dir, &store, "node_mod", 10).unwrap();
        assert_eq!((x.items[0].placement, x.items[0].folder), (Placement::Excluded, true));
        let v = to_json(&found);
        assert_eq!(v["note"], LIMIT_NOTE);
        assert!(v["items"].as_array().unwrap().iter().any(|i| i["placement"] == "not synced"));
        drop(store);
        let _ = std::fs::remove_dir_all(&dir);
    }
}
