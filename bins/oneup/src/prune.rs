//! Removing excluded items from OneDrive (`oneup cloud prune`, or the
//! `prune` setting; `docs/architecture/rules.md` § Cloud cleanup). Excluding
//! never deletes anything by itself; this is the explicit way to make the
//! cloud match the rules.
//!
//! What qualifies: an item in the synced OneDrive folder that the rules
//! exclude *now*, outermost only (a folder, not each file in it). A folder
//! the mirror skipped by name but that the rules no longer exclude (it's
//! about to be listed again) doesn't; nor does the engine's `.oneup` folder.
//! Each becomes an [`Op::PruneRemote`] for the safety gate and the executor,
//! with the Synced rows below it to forget first.
//!
//! **Only once it's here.** A cloud copy is removed only when this PC has
//! all of it: every item synced, files with their content here (not
//! online-only) and unchanged in OneDrive since. A candidate that isn't
//! [`Candidate::ready`] waits: online-only files are set to always keep on
//! this device, so the placement pass downloads them, and items not here
//! yet arrive by syncing (exclusions only stop uploads, unless the setting
//! is `keep`, which then offers only what's here). A folder listing skips
//! by name is never looked inside, on either side: it counts as here when
//! this PC has a folder of that name in the same place. Paused folders and
//! the OneDrive app's are never candidates: they aren't excluded junk.

use anyhow::{Context, Result};
use oneup_rules::{RuleSet, Source, Verdict};
use oneup_store::{Kind, LocalKind, Ops, Store};
use oneup_sync::{LocalId, LocalPre, Op, ROOT, RemoteId, RemotePre, Sid};

/// `FILE_ATTRIBUTE_RECALL_ON_DATA_ACCESS`: an online-only file.
const ONLINE_ONLY: u32 = 0x0040_0000;
/// `FILE_ATTRIBUTE_PINNED`: "Always keep on this device".
const PINNED: u32 = 0x0008_0000;

/// An item to prune.
#[derive(Debug, Clone)]
pub struct Candidate {
    pub id: String,
    pub etag: String,
    /// Relative to the synced folder, `/`-separated, with a leading `/`.
    pub path: String,
    pub folder: bool,
    /// Bytes (a folder: everything in it, as OneDrive reports).
    pub size: i64,
    /// A folder the mirror skipped by name: its contents were never listed.
    pub unlisted: bool,
    /// Items inside, as far as known (an unlisted folder: see [`counted`]).
    pub items: u64,
    /// `items` is all of them. Not for a folder never listed in full (one
    /// skipped by name, or holding one): its count is a lower bound, and
    /// removing it removes everything in it, so the safety gate always asks
    /// first (`oneup_gate::unknown_size`).
    pub exact: bool,
    /// Synced rows at and below it, deepest first.
    pub forget: Vec<Sid>,
    /// The copy here, deepest first, as far as it's here.
    pub keep: Vec<LocalPre>,
    /// Online-only files here to download first (relative paths, `/`-
    /// separated, with a leading `/`), not yet set to always keep.
    pub fetch: Vec<String>,
    /// Items not here yet (on their way, or being downloaded).
    pub waiting: u64,
}

impl Candidate {
    /// Whether all of it is here, so its cloud copy can go.
    pub fn ready(&self) -> bool {
        self.waiting == 0 && self.fetch.is_empty()
    }

    pub fn op(&self) -> Op {
        Op::PruneRemote {
            remote: RemotePre { id: RemoteId(self.id.clone()), etag: self.etag.clone() },
            forget: self.forget.clone(),
            items: self.items.max(1),
            exact: self.exact,
            keep: self.keep.clone(),
        }
    }
}

/// How much of a cloud subtree is here.
#[derive(Default)]
struct Here {
    keep: Vec<LocalPre>,
    fetch: Vec<String>,
    /// Online-only files set to always keep: being downloaded.
    downloading: u64,
    /// Not here at all: only syncing can bring them.
    arriving: u64,
    /// A folder skipped by name that isn't here: nothing ever brings it.
    never: bool,
}

impl Here {
    /// Looks at the synced cloud item `id` (a row of `kind` with cloud
    /// content `qxh`) and, for a listed folder, everything in it.
    fn walk(&mut self, o: &Ops<'_>, id: &str, kind: Kind, qxh: Option<oneup_sync::Hash>) -> oneup_store::Result<()> {
        let local = o.synced_by_remote(id)?.and_then(|b| {
            // A file changed in OneDrive since: the change isn't here yet.
            let current = kind != Kind::File || (b.hash.is_some() && b.hash == qxh);
            b.local.filter(|_| current)
        });
        let row = match local {
            Some(l) => o.local_get(l.0)?,
            None => None,
        };
        match row {
            None => self.arriving += 1,
            Some(r) if r.kind == LocalKind::Folder => {
                for (c, unlisted) in o.remote_children(id)? {
                    if unlisted {
                        self.unlisted(o, r.file_id, &c.name);
                    } else {
                        self.walk(o, &c.id, c.kind, c.qxh)?;
                    }
                }
                self.keep.push(LocalPre { id: LocalId(r.file_id), stamp: None });
            }
            Some(r) if r.attributes & ONLINE_ONLY != 0 => match o.local_path_of(r.file_id)? {
                Some(p) if r.attributes & PINNED == 0 => self.fetch.push(p),
                _ => self.downloading += 1,
            },
            Some(r) => self.keep.push(LocalPre { id: LocalId(r.file_id), stamp: Some((r.size, r.mtime)) }),
        }
        Ok(())
    }

    /// A folder listing skips by name, in the local folder `parent`: here
    /// if this PC has a folder of that name there.
    fn unlisted(&mut self, o: &Ops<'_>, parent: u128, name: &str) {
        let found = o.local_children(parent).ok().and_then(|kids| {
            kids.into_iter().find(|l| l.kind == LocalKind::Folder && oneup_sync::same_name(&l.name, name))
        });
        match found {
            Some(l) => self.keep.push(LocalPre { id: LocalId(l.file_id), stamp: None }),
            None => self.never = true,
        }
    }
}

/// Where to look: everything, or only the children of these Synced folders
/// (the ones a round found changed).
pub enum Scope<'a> {
    All,
    Children(&'a [Sid]),
}

/// The cloud items the rules exclude now, that are or will be here.
/// `brings`: excluded items not here yet are brought here by syncing
/// (every setting but `keep`); without it, only what's here is offered.
pub fn candidates(store: &Store, rules: &RuleSet, scope: Scope<'_>, brings: bool) -> Result<Vec<Candidate>> {
    if rules.is_empty() {
        return Ok(Vec::new());
    }
    store
        .read(|o| {
            let Some(top) = o.synced_get(ROOT)?.and_then(|b| b.remote) else { return Ok(Vec::new()) };
            let mut out = Vec::new();
            // (cloud folder id, its path relative to the synced folder, walk below)
            let mut todo: Vec<(String, String, bool)> = match scope {
                Scope::All => vec![(top.0.clone(), String::new(), true)],
                Scope::Children(sids) => {
                    let mut v = Vec::new();
                    for sid in sids {
                        if let Some(r) = o.synced_get(*sid)?.and_then(|b| b.remote)
                            && let Some(path) = relative_path(o, &top.0, &r.0)?
                        {
                            v.push((r.0, path, false));
                        }
                    }
                    v
                }
            };
            while let Some((id, path, deep)) = todo.pop() {
                for (row, unlisted) in o.remote_children(&id)? {
                    let folder = matches!(row.kind, Kind::Folder);
                    let child = if path.is_empty() { row.name.clone() } else { format!("{path}/{}", row.name) };
                    if path.is_empty() && oneup_rules::is_engine_dir(&row.name) {
                        continue;
                    }
                    // A vault synced as its own pair: never a candidate,
                    // and never looked into (its encrypted files are the
                    // vault, not excluded junk).
                    if rules.is_vault(&child)
                        || rules.is_onedrive_app(&child, folder)
                        || is_paused(rules, &child, folder)
                    {
                        continue;
                    }
                    if rules.excludes_child(&path, &row.name, folder) {
                        let mut here = Here::default();
                        if unlisted {
                            let parent = o.synced_by_remote(&id)?.and_then(|b| b.local);
                            match parent {
                                Some(p) => here.unlisted(o, p.0, &row.name),
                                None => here.never = true,
                            }
                        } else {
                            here.walk(o, &row.id, row.kind, row.qxh)?;
                        }
                        if here.never || (!brings && here.arriving > 0) {
                            continue;
                        }
                        let (items, exact) = match (folder, unlisted) {
                            (false, _) => (1, true),
                            (true, true) => (0, false),
                            (true, false) => o.remote_subtree_known(&row.id)?,
                        };
                        out.push(Candidate {
                            forget: forget_below(o, &row.id)?,
                            id: row.id,
                            etag: row.etag.unwrap_or_default(),
                            path: format!("/{child}"),
                            folder,
                            size: row.size,
                            unlisted,
                            items,
                            exact,
                            keep: here.keep,
                            fetch: here.fetch,
                            waiting: here.downloading + here.arriving,
                        });
                    } else if folder && !unlisted && deep {
                        todo.push((row.id, child, true));
                    }
                }
            }
            out.sort_by(|a, b| a.path.cmp(&b.path));
            Ok(out)
        })
        .context("finding excluded items in OneDrive")
}

/// Whether the item at `path` is excluded by being paused (it's waiting,
/// not junk).
fn is_paused(rules: &RuleSet, path: &str, folder: bool) -> bool {
    matches!(rules.verdict(path, folder), Verdict::Excluded(m) if m.source == Source::Pause)
}

/// `id`'s path relative to the synced folder `top` (`None` if outside it).
fn relative_path(o: &Ops<'_>, top: &str, id: &str) -> oneup_store::Result<Option<String>> {
    let (Some(t), Some(p)) = (o.path_of(top)?, o.path_of(id)?) else { return Ok(None) };
    Ok(p.strip_prefix(&t).map(|rest| rest.trim_start_matches('/').to_string()))
}

/// The Synced rows at and below cloud item `id`, deepest first.
fn forget_below(o: &Ops<'_>, id: &str) -> oneup_store::Result<Vec<Sid>> {
    let Some(b) = o.synced_by_remote(id)? else { return Ok(Vec::new()) };
    let mut all = vec![b.sid];
    let mut i = 0;
    while i < all.len() {
        let kids = o.synced_children(all[i])?;
        all.extend(kids.into_iter().map(|k| k.sid));
        i += 1;
    }
    all.reverse();
    Ok(all)
}

/// In the state database: the OneDrive IDs of excluded items the user chose
/// to keep in OneDrive (JSON list). They're no longer offered for removal,
/// whatever changes in them; items excluded later are asked about anew.
pub const KEY_KEPT: &str = "prune.kept_in_cloud";

/// The items the user chose to keep in OneDrive ([`KEY_KEPT`]).
pub fn kept(store: &Store) -> Result<Vec<String>> {
    let v = store.read(|o| o.meta_get(KEY_KEPT))?;
    // Our own record: unreadable counts as nothing kept (the offer returns).
    Ok(v.and_then(|s| serde_json::from_str(&s).ok()).unwrap_or_default())
}

/// Adds `ids` to the items kept in OneDrive ([`KEY_KEPT`]).
pub fn keep(store: &mut Store, ids: impl IntoIterator<Item = String>) -> Result<()> {
    let mut all = kept(store)?;
    for id in ids {
        if !all.contains(&id) {
            all.push(id);
        }
    }
    let json = serde_json::to_string(&all)?;
    store.write(|o| o.meta_set(KEY_KEPT, &json))?;
    Ok(())
}

/// `found` without the items the user chose to keep in OneDrive.
pub fn without_kept(store: &Store, mut found: Vec<Candidate>) -> Result<Vec<Candidate>> {
    let kept = kept(store)?;
    if !kept.is_empty() {
        found.retain(|c| !kept.contains(&c.id));
    }
    Ok(found)
}

/// The code of a list of candidates: it changes when any item does
/// (OneDrive gives a folder a new eTag when anything in it changes).
pub fn fingerprint(found: &[Candidate]) -> u64 {
    let mut h: u64 = 0xcbf2_9ce4_8422_2325;
    for c in found {
        for b in c.id.bytes().chain([0]).chain(c.etag.bytes()).chain([0]) {
            h = (h ^ u64::from(b)).wrapping_mul(0x0100_0000_01b3);
        }
    }
    h
}

/// What OneDrive says about a folder: how many items it holds directly
/// (not in its subfolders), and its size.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct FolderFacts {
    pub children: Option<u64>,
    pub size: Option<i64>,
}

/// Where [`counted`] asks: OneDrive (one read-only request per folder).
pub trait FolderInfo {
    fn folder_facts(&self, id: &str) -> impl std::future::Future<Output = Result<Option<FolderFacts>>> + Send;
}

impl FolderInfo for oneup_graph::Graph {
    async fn folder_facts(&self, id: &str) -> Result<Option<FolderFacts>> {
        Ok(self.item(id).await?.map(|item| FolderFacts {
            children: item.folder.and_then(|f| f.child_count).map(|n| n.max(0) as u64),
            size: item.size,
        }))
    }
}

/// Fills in what's known of unlisted folders from OneDrive: their size, and
/// as their item count its direct children. That's only a lower bound (a
/// folder of 10 subfolders reports 10, however much they hold), so they stay
/// of unknown size ([`Candidate::exact`] false): shown as "at least", and
/// always asked about before removal.
pub async fn counted<G: FolderInfo>(graph: &G, mut c: Vec<Candidate>) -> Result<Vec<Candidate>> {
    for x in c.iter_mut().filter(|x| x.unlisted) {
        if let Some(f) = graph.folder_facts(&x.id).await? {
            x.items = f.children.map_or(1, |n| n.max(1));
            x.size = f.size.unwrap_or(x.size);
        }
        x.exact = false;
    }
    Ok(c)
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used)]
    use oneup_store::RemoteRow;

    use super::*;

    fn row(id: &str, parent: &str, name: &str, kind: Kind) -> RemoteRow {
        RemoteRow {
            id: id.into(),
            parent_id: Some(parent.into()),
            name: name.into(),
            kind,
            size: 1,
            etag: Some("e".into()),
            qxh: None,
            mtime_ms: None,
        }
    }

    /// A vault synced as its own pair inside the synced OneDrive folder is
    /// excluded from this pair, but its encrypted files are the vault: they
    /// must never be offered for removal (found live).
    #[test]
    fn a_vault_synced_on_its_own_is_never_a_candidate() {
        let mut s = Store::open_in_memory().unwrap();
        s.write(|o| {
            o.upsert_item(&RemoteRow { parent_id: None, kind: Kind::Root, ..row("root", "", "", Kind::Root) }, false)?;
            o.upsert_item(&row("top", "root", "Sync", Kind::Folder), false)?;
            o.upsert_item(&row("v", "top", "Vault", Kind::Folder), false)?;
            o.upsert_item(&row("vd", "v", "d", Kind::Folder), false)?;
            o.upsert_item(&row("vn", "vd", "node_modules", Kind::Folder), false)?;
            o.upsert_item(&row("nm", "top", "node_modules", Kind::Folder), false)?;
            oneup_exec::link_roots(o, 1, "top").map(|_| ())
        })
        .unwrap();
        let mut rules = RuleSet::new(&["dev"], &[] as &[&str]).unwrap();
        rules.set_vaults(&["Vault"]);
        let found: Vec<String> =
            candidates(&s, &rules, Scope::All, true).unwrap().into_iter().map(|c| c.path).collect();
        assert_eq!(found, ["/node_modules"], "only the junk; nothing of (or in) the vault");
    }

    /// A cloud copy is removed only once it's here: an excluded file not
    /// here yet waits (and isn't even offered when nothing brings it here);
    /// paused folders and folders skipped by name with nothing here aren't
    /// candidates at all.
    #[test]
    fn only_what_is_or_will_be_here_is_offered() {
        let mut s = Store::open_in_memory().unwrap();
        s.write(|o| {
            o.upsert_item(&RemoteRow { parent_id: None, kind: Kind::Root, ..row("root", "", "", Kind::Root) }, false)?;
            o.upsert_item(&row("top", "root", "Sync", Kind::Folder), false)?;
            o.upsert_item(&row("env", "top", "a.env", Kind::File), false)?;
            o.upsert_item(&row("old", "top", "Old", Kind::Folder), false)?;
            o.upsert_item(&row("tg", "top", "target", Kind::Folder), true)?;
            oneup_exec::link_roots(o, 1, "top").map(|_| ())
        })
        .unwrap();
        let mut rules = RuleSet::new(&["dev"], &["*.env"]).unwrap();
        rules.set_paused(&["Old"]);
        let found = candidates(&s, &rules, Scope::All, true).unwrap();
        let paths: Vec<&str> = found.iter().map(|c| c.path.as_str()).collect();
        assert_eq!(paths, ["/a.env"], "not the paused folder, nor a skipped folder with nothing here");
        assert!(!found[0].ready() && found[0].waiting == 1 && found[0].keep.is_empty());
        assert!(candidates(&s, &rules, Scope::All, false).unwrap().is_empty(), "`keep`: nothing brings it here");
    }

    /// OneDrive as the prune flow reads it, reporting a folder's direct
    /// children only (as Graph's `childCount` does).
    struct DirectChildren(u64);

    impl FolderInfo for DirectChildren {
        async fn folder_facts(&self, _: &str) -> Result<Option<FolderFacts>> {
            Ok(Some(FolderFacts { children: Some(self.0), size: Some(5_000_000) }))
        }
    }

    /// A folder whose contents were never listed (skipped by name) is
    /// counted from OneDrive's direct children: 10 subfolders, however much
    /// they hold. That's a lower bound, so it's removed only after the user
    /// approves it, whatever the limits, and once removed the safety window
    /// counts it as over the limit. A listed folder holding one is the same.
    #[tokio::test]
    async fn a_folder_never_listed_in_full_is_always_asked_about() {
        let mut s = Store::open_in_memory().unwrap();
        s.write(|o| {
            o.upsert_item(&RemoteRow { parent_id: None, kind: Kind::Root, ..row("root", "", "", Kind::Root) }, false)?;
            o.upsert_item(&row("top", "root", "Sync", Kind::Folder), false)?;
            // Skipped by name when listing: nothing inside was ever seen.
            o.upsert_item(&row("nm", "top", "node_modules", Kind::Folder), true)?;
            // Listed, but holding a folder that was skipped.
            o.upsert_item(&row("rd", "top", "renders", Kind::Folder), false)?;
            o.upsert_item(&row("x", "rd", "x.png", Kind::File), false)?;
            o.upsert_item(&row("deep", "rd", "node_modules", Kind::Folder), true)?;
            oneup_exec::link_roots(o, 1, "top")?;
            let here = oneup_store::LocalRow {
                file_id: 2,
                parent_id: Some(1),
                name: "node_modules".into(),
                name_lossy: false,
                kind: LocalKind::Folder,
                size: 0,
                mtime: 1,
                attributes: 0x10,
                reparse_tag: 0,
                skip: oneup_store::Skip::Excluded,
            };
            o.local_put(&here).map(|_| ())
        })
        .unwrap();
        let rules = RuleSet::new(&["dev"], &["renders/"]).unwrap();
        let found = candidates(&s, &rules, Scope::All, true).unwrap();
        let by = |p: &str| found.iter().find(|c| c.path == p).unwrap().clone();
        let renders = by("/renders");
        assert_eq!((renders.items, renders.exact), (2, false), "holds a folder never listed");
        assert!(!by("/node_modules").exact);

        let ready: Vec<Candidate> = found.into_iter().filter(Candidate::ready).collect();
        let counted = counted(&DirectChildren(10), ready).await.unwrap();
        assert_eq!(counted.len(), 1);
        let c = &counted[0];
        assert_eq!((c.path.as_str(), c.items, c.exact, c.size), ("/node_modules", 10, false, 5_000_000));

        let p = oneup_gate::Policy::default();
        let r = oneup_gate::review(&p, 10_000_000, &[], vec![c.op()]);
        assert_eq!(r.breakers.len(), 1, "10 known items are far under the limits, but it may hold any number");
        let b = &r.breakers[0];
        assert_eq!((b.count, b.at_least), (10, true));
        assert!(r.approved.is_empty());
        let cx = oneup_gate::Context { synced_items: 10_000_000, approved: &b.items, ..Default::default() };
        let r = oneup_gate::review_in(&p, &cx, vec![c.op()]);
        assert_eq!(r.approved.len(), 1, "approved, it goes");
        let mut window = oneup_gate::Recent::default();
        window.add_op(r.approved[0].op(), &[]);
        assert_eq!(window.remote, oneup_gate::FULL, "counted as full, not as 10");
    }

    /// Items the user chose to keep in OneDrive aren't offered again, also
    /// from a store opened anew (the record is in the store, not in memory);
    /// items excluded later still are.
    #[test]
    fn kept_items_are_not_offered_again() {
        let mut s = Store::open_in_memory().unwrap();
        s.write(|o| {
            o.upsert_item(&RemoteRow { parent_id: None, kind: Kind::Root, ..row("root", "", "", Kind::Root) }, false)?;
            o.upsert_item(&row("top", "root", "Sync", Kind::Folder), false)?;
            o.upsert_item(&row("a", "top", "a.env", Kind::File), false)?;
            o.upsert_item(&row("b", "top", "b.env", Kind::File), false)?;
            oneup_exec::link_roots(o, 1, "top").map(|_| ())
        })
        .unwrap();
        let rules = RuleSet::new(&[] as &[&str], &["*.env"]).unwrap();
        let found = |s: &Store| without_kept(s, candidates(s, &rules, Scope::All, true).unwrap()).unwrap();
        assert_eq!(found(&s).len(), 2);
        keep(&mut s, ["a".to_string()]).unwrap();
        keep(&mut s, ["a".to_string()]).unwrap();
        assert_eq!(kept(&s).unwrap(), ["a"], "recorded once");
        let left: Vec<String> = found(&s).into_iter().map(|c| c.id).collect();
        assert_eq!(left, ["b"]);
    }
}
