//! **Parking**: hides from the reconciler the local items OneDrive would
//! refuse, and the files that are local by nature (Office lock files,
//! `desktop.ini`, `*.tmp`, the OneDrive app's marker file), so a sync never
//! tries to upload them.
//!
//! [`Parked`] wraps any [`Trees`] and filters those items out of children
//! lists and reads them as [`Seen::Missing`] by ID, as the `Trees` contract
//! requires. Missing never causes a deletion, so an item that *becomes*
//! unsyncable (renamed to `report.tmp`, grown past 250 GB) is held on both
//! sides, not deleted in the cloud.
//!
//! The local-by-nature names are filtered on the cloud side too: the
//! official client doesn't upload them, but if one is in OneDrive anyway,
//! downloading it would collide with the local file of the same name every
//! round. Names OneDrive refuses can't exist in the cloud, so only the local
//! side is checked for those. A cloud folder named like the engine's own
//! `.oneup` directly inside the synced OneDrive folder is parked too: the
//! local side of that name is the engine folder, which is never synced.
//!
//! **The user's rules** (`docs/architecture/rules.md`), when given with
//! [`Parked::with_rules`], hide what they exclude, judged by each item's
//! path relative to the synced folder in its own tree. A Synced record is
//! hidden too when its path is excluded and neither side's item is visible
//! elsewhere: then the reconciler plans nothing for it (no hold, no
//! deletion; both copies stay), and removing the rule resumes sync from
//! that record. A synced item moved *out* of an excluded folder keeps its
//! visible record (so it never looks new and gets a second one); its other
//! side, still excluded, reads as missing and the item is held.
//!
//! Pure: no I/O, so the reconciler stays deterministic. Problem items
//! (refused names, oversized files) are collected for reporting; the
//! local-by-nature ones are skipped silently, like the official client does.
//!
//! Not checked here: OneDrive's 400-character path limit, which needs the
//! whole path. An upload that exceeds it fails and is reported as a failed
//! operation instead.

use std::cell::{OnceCell, RefCell};
use std::collections::BTreeMap;

use oneup_rules::RuleSet;
use oneup_sync::{Base, LocalId, LocalNode, NodeKind, Op, ROOT, RemoteId, RemoteNode, Seen, Sid, Trees};

use crate::names;

/// Why a local item isn't synced, for items the user may want to fix.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Why {
    /// OneDrive refuses the name (see [`names::is_invalid`]).
    InvalidName,
    /// Larger than OneDrive's 250 GB limit.
    TooLarge,
}

/// A [`Trees`] view without parked items. Remembers the problem items it
/// filtered out ([`Parked::problems`]), keyed by ID so repeated reads of
/// the same item count once; at most the items of the folders a round
/// visits, so bounded by the round's work.
pub struct Parked<'a, T: ?Sized> {
    inner: &'a T,
    problems: RefCell<BTreeMap<LocalId, (String, Why)>>,
    /// The synced OneDrive folder's ID (the Synced root's cloud side), read
    /// once: a cloud folder named like the engine's own `.oneup` directly
    /// inside it would collide with the local engine folder, which the
    /// scanner excludes at the top of the sync folder.
    top: OnceCell<Option<RemoteId>>,
    /// The user's rules; `None` when there are none.
    rules: Option<&'a RuleSet>,
    /// Folders' paths and whether the rules exclude them, per tree, filled
    /// as the round visits them (bounded by the round's folders).
    local_dirs: RefCell<BTreeMap<LocalId, Dir>>,
    remote_dirs: RefCell<BTreeMap<RemoteId, Option<Dir>>>,
    base_dirs: RefCell<BTreeMap<Sid, Dir>>,
}

/// A folder as the rules see it.
#[derive(Debug, Clone, Default)]
struct Dir {
    /// Relative to the synced folder, `/`-separated; empty for the folder itself.
    path: String,
    excluded: bool,
}

impl<'a, T: Trees + ?Sized> Parked<'a, T> {
    pub fn new(inner: &'a T) -> Self {
        Self {
            inner,
            problems: RefCell::default(),
            top: OnceCell::new(),
            rules: None,
            local_dirs: RefCell::default(),
            remote_dirs: RefCell::default(),
            base_dirs: RefCell::default(),
        }
    }

    /// Also hides what the user's `rules` exclude (see the module docs).
    pub fn with_rules(inner: &'a T, rules: &'a RuleSet) -> Self {
        Self { rules: (!rules.is_empty()).then_some(rules), ..Self::new(inner) }
    }

    /// `name` inside `parent`, as the rules see it.
    fn child(rules: &RuleSet, parent: &Dir, name: &str, is_dir: bool) -> Dir {
        let path = if parent.path.is_empty() { name.to_string() } else { format!("{}/{name}", parent.path) };
        Dir { excluded: parent.excluded || rules.excludes_child(&parent.path, name, is_dir), path }
    }

    fn local_dir(&self, rules: &RuleSet, id: LocalId) -> Result<Dir, T::Error> {
        if let Some(d) = self.local_dirs.borrow().get(&id) {
            return Ok(d.clone());
        }
        let d = match self.inner.local(id)? {
            Seen::Present(LocalNode { parent: Some(p), name, .. }) => {
                let pd = self.local_dir(rules, p)?;
                Self::child(rules, &pd, &name, true)
            }
            // The synced folder itself (or, in an inconsistent tree, an
            // unknown parent: judged as the top).
            _ => Dir::default(),
        };
        self.local_dirs.borrow_mut().insert(id, d.clone());
        Ok(d)
    }

    fn local_excluded(&self, n: &LocalNode) -> Result<bool, T::Error> {
        let (Some(rules), Some(p)) = (self.rules, n.parent) else { return Ok(false) };
        let pd = self.local_dir(rules, p)?;
        Ok(pd.excluded || rules.excludes_child(&pd.path, &n.name, n.kind == NodeKind::Folder))
    }

    /// `None`: outside the synced OneDrive folder.
    fn remote_dir(&self, rules: &RuleSet, id: &RemoteId) -> Result<Option<Dir>, T::Error> {
        if let Some(d) = self.remote_dirs.borrow().get(id) {
            return Ok(d.clone());
        }
        let d = if self.top()? == Some(id) {
            Some(Dir::default())
        } else {
            match self.inner.remote(id)? {
                Seen::Present(RemoteNode { parent: Some(p), name, .. }) => {
                    self.remote_dir(rules, &p)?.map(|pd| Self::child(rules, &pd, &name, true))
                }
                _ => None,
            }
        };
        self.remote_dirs.borrow_mut().insert(id.clone(), d.clone());
        Ok(d)
    }

    fn remote_excluded(&self, n: &RemoteNode) -> Result<bool, T::Error> {
        let (Some(rules), Some(p)) = (self.rules, n.parent.as_ref()) else { return Ok(false) };
        Ok(match self.remote_dir(rules, p)? {
            Some(pd) => pd.excluded || rules.excludes_child(&pd.path, &n.name, n.kind == NodeKind::Folder),
            None => false,
        })
    }

    fn base_dir(&self, rules: &RuleSet, sid: Sid) -> Result<Dir, T::Error> {
        if let Some(d) = self.base_dirs.borrow().get(&sid) {
            return Ok(d.clone());
        }
        let d = match self.inner.base(sid)? {
            Some(Base { parent: Some(p), name, .. }) => {
                let pd = self.base_dir(rules, p)?;
                Self::child(rules, &pd, &name, true)
            }
            _ => Dir::default(),
        };
        self.base_dirs.borrow_mut().insert(sid, d.clone());
        Ok(d)
    }

    /// Whether the Synced record `b` is hidden (see the module docs).
    fn base_hidden(&self, b: &Base) -> Result<bool, T::Error> {
        let Some(rules) = self.rules else { return Ok(false) };
        if !self.base_path_excluded(rules, b)? {
            return Ok(false);
        }
        // Visible on a side (moved out of the excluded part): keep it.
        if let Some(l) = b.local
            && matches!(self.local(l)?, Seen::Present(_))
        {
            return Ok(false);
        }
        if let Some(r) = &b.remote
            && matches!(self.remote(r)?, Seen::Present(_))
        {
            return Ok(false);
        }
        Ok(true)
    }

    /// Whether `op` would change OneDrive for something these rules exclude
    /// (the item, its Synced record, or where it would go): what the `pull`
    /// setting never does (`docs/architecture/rules.md` § Cloud cleanup).
    /// Use a view built [`Parked::with_rules`] the full rules for this, and
    /// another without them for planning, so excluded items stay visible.
    pub fn sends_excluded(&self, op: &Op) -> Result<bool, T::Error> {
        let Some(rules) = self.rules else { return Ok(false) };
        let local = |id: LocalId| -> Result<bool, T::Error> {
            Ok(match self.inner.local(id)? {
                Seen::Present(n) => self.local_excluded(&n)?,
                _ => false,
            })
        };
        let remote = |id: &RemoteId| -> Result<bool, T::Error> {
            Ok(match self.inner.remote(id)? {
                Seen::Present(n) => self.remote_excluded(&n)?,
                _ => false,
            })
        };
        let base = |sid: Sid| -> Result<bool, T::Error> {
            Ok(match self.inner.base(sid)? {
                Some(b) => self.base_path_excluded(rules, &b)?,
                None => false,
            })
        };
        Ok(match op {
            Op::Upload { local: l, .. } | Op::MkdirRemote { local: l, .. } => local(l.id)?,
            Op::MoveRemote { sid, remote: r, to, name, .. } => {
                let folder = self.inner.base(*sid)?.is_some_and(|b| b.kind == NodeKind::Folder);
                let target = match self.remote_dir(rules, to)? {
                    Some(d) => d.excluded || rules.excludes_child(&d.path, name, folder),
                    None => false,
                };
                base(*sid)? || remote(&r.id)? || target
            }
            Op::DeleteRemote { sid, remote: r } => base(*sid)? || remote(&r.id)?,
            _ => false,
        })
    }

    /// Whether the rules exclude Synced record `b`'s own path.
    fn base_path_excluded(&self, rules: &RuleSet, b: &Base) -> Result<bool, T::Error> {
        let Some(p) = b.parent else { return Ok(false) };
        let pd = self.base_dir(rules, p)?;
        Ok(pd.excluded || rules.excludes_child(&pd.path, &b.name, b.kind == NodeKind::Folder))
    }

    fn visible_base(&self, b: Option<Base>) -> Result<Option<Base>, T::Error> {
        Ok(match b {
            Some(b) if self.base_hidden(&b)? => None,
            b => b,
        })
    }

    fn top(&self) -> Result<Option<&RemoteId>, T::Error> {
        if self.top.get().is_none() {
            let top = self.inner.base(ROOT)?.and_then(|b| b.remote);
            let _ = self.top.set(top);
        }
        Ok(self.top.get().and_then(Option::as_ref))
    }

    fn remote_parked(&self, n: &RemoteNode) -> Result<bool, T::Error> {
        if n.parent.is_none() {
            return Ok(false);
        }
        if is_local_by_nature(&n.name) {
            return Ok(true);
        }
        Ok(oneup_rules::is_engine_dir(&n.name) && n.parent.as_ref() == self.top()?)
    }

    /// The problem items seen so far: (ID, name, why).
    pub fn problems(&self) -> Vec<(LocalId, String, Why)> {
        self.problems.borrow().iter().map(|(id, (name, why))| (*id, name.clone(), *why)).collect()
    }

    /// Whether `n` is parked, noting it if it's a problem item.
    fn local_parked(&self, n: &LocalNode) -> bool {
        if n.parent.is_none() {
            return false; // the sync folder itself
        }
        let why = if names::is_invalid(&n.name) {
            Some(Why::InvalidName)
        } else if n.kind == NodeKind::File && n.size > names::MAX_FILE_BYTES {
            Some(Why::TooLarge)
        } else {
            None
        };
        if let Some(why) = why {
            self.problems.borrow_mut().insert(n.id, (n.name.clone(), why));
            return true;
        }
        is_local_by_nature(&n.name)
    }
}

/// Files the official client never syncs, on either side.
fn is_local_by_nature(name: &str) -> bool {
    names::not_synced(name).is_some() || names::is_onedrive_marker(name)
}

impl<T: Trees + ?Sized> Trees for Parked<'_, T> {
    type Error = T::Error;

    fn base(&self, sid: Sid) -> Result<Option<Base>, T::Error> {
        self.inner.base(sid)
    }

    fn base_children(&self, sid: Sid) -> Result<Vec<Base>, T::Error> {
        let mut out = Vec::new();
        for b in self.inner.base_children(sid)? {
            if !self.base_hidden(&b)? {
                out.push(b);
            }
        }
        Ok(out)
    }

    fn base_by_local(&self, id: LocalId) -> Result<Option<Base>, T::Error> {
        self.visible_base(self.inner.base_by_local(id)?)
    }

    fn base_by_remote(&self, id: &RemoteId) -> Result<Option<Base>, T::Error> {
        self.visible_base(self.inner.base_by_remote(id)?)
    }

    fn local(&self, id: LocalId) -> Result<Seen<LocalNode>, T::Error> {
        Ok(match self.inner.local(id)? {
            // Excluded first: an excluded item isn't a problem to report.
            Seen::Present(n) if self.local_excluded(&n)? || self.local_parked(&n) => Seen::Missing,
            s => s,
        })
    }

    fn remote(&self, id: &RemoteId) -> Result<Seen<RemoteNode>, T::Error> {
        Ok(match self.inner.remote(id)? {
            Seen::Present(n) if self.remote_excluded(&n)? || self.remote_parked(&n)? => Seen::Missing,
            s => s,
        })
    }

    fn local_children(&self, id: LocalId) -> Result<Vec<LocalNode>, T::Error> {
        if let Some(rules) = self.rules
            && self.local_dir(rules, id)?.excluded
        {
            return Ok(Vec::new());
        }
        let mut out = Vec::new();
        for n in self.inner.local_children(id)? {
            if !self.local_excluded(&n)? && !self.local_parked(&n) {
                out.push(n);
            }
        }
        Ok(out)
    }

    fn remote_children(&self, id: &RemoteId) -> Result<Vec<RemoteNode>, T::Error> {
        if let Some(rules) = self.rules
            && self.remote_dir(rules, id)?.is_some_and(|d| d.excluded)
        {
            return Ok(Vec::new());
        }
        let mut out = Vec::new();
        for n in self.inner.remote_children(id)? {
            if !self.remote_excluded(&n)? && !self.remote_parked(&n)? {
                out.push(n);
            }
        }
        Ok(out)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use oneup_sync::{Op, ROOT, all_folders, plan};

    /// A minimal in-memory world: one synced root, and children on each
    /// side; plus one Synced file (sid 2) whose local copy can be renamed.
    #[derive(Default, Clone)]
    struct World {
        local: Vec<LocalNode>,
        remote: Vec<RemoteNode>,
        base: Vec<Base>,
    }

    fn lroot() -> LocalNode {
        LocalNode {
            id: LocalId(1),
            parent: None,
            name: String::new(),
            kind: NodeKind::Folder,
            size: 0,
            mtime: 0,
            hash: None,
        }
    }
    fn rroot() -> RemoteNode {
        RemoteNode {
            id: RemoteId("r".into()),
            parent: None,
            name: String::new(),
            kind: NodeKind::Folder,
            size: 0,
            hash: None,
            etag: "e".into(),
        }
    }
    fn lfile(id: u128, name: &str, size: i64) -> LocalNode {
        LocalNode {
            id: LocalId(id),
            parent: Some(LocalId(1)),
            name: name.into(),
            kind: NodeKind::File,
            size,
            mtime: 1,
            hash: None,
        }
    }

    impl Trees for World {
        type Error = std::convert::Infallible;
        fn base(&self, sid: Sid) -> Result<Option<Base>, Self::Error> {
            Ok(self.base.iter().find(|b| b.sid == sid).cloned())
        }
        fn base_children(&self, sid: Sid) -> Result<Vec<Base>, Self::Error> {
            Ok(self.base.iter().filter(|b| b.parent == Some(sid)).cloned().collect())
        }
        fn base_by_local(&self, id: LocalId) -> Result<Option<Base>, Self::Error> {
            Ok(self.base.iter().find(|b| b.local == Some(id)).cloned())
        }
        fn base_by_remote(&self, id: &RemoteId) -> Result<Option<Base>, Self::Error> {
            Ok(self.base.iter().find(|b| b.remote.as_ref() == Some(id)).cloned())
        }
        fn local(&self, id: LocalId) -> Result<Seen<LocalNode>, Self::Error> {
            Ok(self.local.iter().find(|n| n.id == id).cloned().map_or(Seen::Missing, Seen::Present))
        }
        fn remote(&self, id: &RemoteId) -> Result<Seen<RemoteNode>, Self::Error> {
            Ok(self.remote.iter().find(|n| &n.id == id).cloned().map_or(Seen::Missing, Seen::Present))
        }
        fn local_children(&self, id: LocalId) -> Result<Vec<LocalNode>, Self::Error> {
            Ok(self.local.iter().filter(|n| n.parent == Some(id)).cloned().collect())
        }
        fn remote_children(&self, id: &RemoteId) -> Result<Vec<RemoteNode>, Self::Error> {
            Ok(self.remote.iter().filter(|n| n.parent.as_ref() == Some(id)).cloned().collect())
        }
    }

    fn world() -> World {
        let root = Base {
            sid: ROOT,
            parent: None,
            name: String::new(),
            kind: NodeKind::Folder,
            size: 0,
            hash: None,
            local: Some(LocalId(1)),
            local_mtime: 0,
            remote: Some(RemoteId("r".into())),
            remote_etag: "e".into(),
        };
        World { local: vec![lroot()], remote: vec![rroot()], base: vec![root] }
    }

    fn uploads(ops: &[Op]) -> Vec<String> {
        ops.iter()
            .filter_map(|op| match op {
                Op::Upload { name, .. } => Some(name.clone()),
                _ => None,
            })
            .collect()
    }

    #[test]
    fn refused_and_local_by_nature_items_are_not_uploaded() {
        let mut w = world();
        for (i, name) in
            ["ok.txt", "bad:name.txt", "trailing. ", "~$doc.docx", "x.tmp", "desktop.ini"].iter().enumerate()
        {
            w.local.push(lfile(10 + i as u128, name, 5));
        }
        w.local.push(lfile(20, "huge.bin", names::MAX_FILE_BYTES + 1));
        let p = Parked::new(&w);
        let ops = plan(&p, [ROOT]).unwrap();
        assert_eq!(uploads(&ops), ["ok.txt"]);
        let mut problems: Vec<_> = p.problems().into_iter().map(|(_, n, w)| (n, w)).collect();
        problems.sort_by(|a, b| a.0.cmp(&b.0));
        assert_eq!(
            problems,
            [
                ("bad:name.txt".to_string(), Why::InvalidName),
                ("huge.bin".to_string(), Why::TooLarge),
                ("trailing. ".to_string(), Why::InvalidName),
            ]
        );
    }

    #[test]
    fn local_by_nature_cloud_files_are_not_downloaded() {
        let mut w = world();
        w.remote.push(RemoteNode {
            id: RemoteId("d".into()),
            parent: Some(RemoteId("r".into())),
            name: "desktop.ini".into(),
            kind: NodeKind::File,
            size: 3,
            hash: None,
            etag: "e1".into(),
        });
        let ops = plan(&Parked::new(&w), [ROOT]).unwrap();
        assert!(ops.iter().all(|op| !op.is_action()), "{ops:?}");
    }

    #[test]
    fn a_cloud_engine_folder_directly_inside_the_synced_folder_is_not_downloaded() {
        let mut w = world();
        let folder = |id: &str, parent: &str, name: &str| RemoteNode {
            id: RemoteId(id.into()),
            parent: Some(RemoteId(parent.into())),
            name: name.into(),
            kind: NodeKind::Folder,
            size: 0,
            hash: None,
            etag: "e".into(),
        };
        w.remote.push(folder("top", "r", ".OneUp"));
        w.remote.push(folder("docs", "r", "docs"));
        let ops = plan(&Parked::new(&w), [ROOT]).unwrap();
        let made: Vec<&str> = ops
            .iter()
            .filter_map(|op| match op {
                Op::MkdirLocal { name, .. } => Some(name.as_str()),
                _ => None,
            })
            .collect();
        assert_eq!(made, ["docs"]);
    }

    fn rnode(id: &str, parent: &str, name: &str, kind: NodeKind) -> RemoteNode {
        RemoteNode {
            id: RemoteId(id.into()),
            parent: Some(RemoteId(parent.into())),
            name: name.into(),
            kind,
            size: if kind == NodeKind::File { 5 } else { 0 },
            hash: None,
            etag: "e1".into(),
        }
    }

    fn lnode(id: u128, parent: u128, name: &str, kind: NodeKind) -> LocalNode {
        let size = if kind == NodeKind::File { 5 } else { 0 };
        LocalNode { parent: Some(LocalId(parent)), kind, ..lfile(id, name, size) }
    }

    fn synced(sid: Sid, parent: Sid, name: &str, kind: NodeKind, local: u128, remote: &str) -> Base {
        Base {
            sid,
            parent: Some(parent),
            name: name.into(),
            kind,
            size: if kind == NodeKind::File { 5 } else { 0 },
            hash: None,
            local: Some(LocalId(local)),
            local_mtime: 1,
            remote: Some(RemoteId(remote.into())),
            remote_etag: "e1".into(),
        }
    }

    /// A synced folder `renders` (sid 2) holding a synced file `a.mp4`
    /// (sid 3), plus a synced `notes.txt` (sid 4).
    fn synced_world() -> World {
        let mut w = world();
        w.local.push(lnode(2, 1, "renders", NodeKind::Folder));
        w.local.push(lnode(3, 2, "a.mp4", NodeKind::File));
        w.local.push(lnode(4, 1, "notes.txt", NodeKind::File));
        w.remote.push(rnode("R2", "r", "renders", NodeKind::Folder));
        w.remote.push(rnode("R3", "R2", "a.mp4", NodeKind::File));
        w.remote.push(rnode("R4", "r", "notes.txt", NodeKind::File));
        w.base.push(synced(2, ROOT, "renders", NodeKind::Folder, 2, "R2"));
        w.base.push(synced(3, 2, "a.mp4", NodeKind::File, 3, "R3"));
        w.base.push(synced(4, ROOT, "notes.txt", NodeKind::File, 4, "R4"));
        w
    }

    #[test]
    fn excluded_new_items_are_neither_uploaded_nor_downloaded() {
        let mut w = world();
        w.local.push(lfile(10, "disk.iso", 5));
        w.local.push(lfile(11, "ok.txt", 5));
        w.remote.push(rnode("c1", "r", "renders", NodeKind::Folder));
        w.remote.push(rnode("c2", "c1", "a.mp4", NodeKind::File));
        let rules = RuleSet::new(&[] as &[&str], &["*.iso", "renders/"]).unwrap();
        let ops = plan(&Parked::with_rules(&w, &rules), [ROOT]).unwrap();
        assert_eq!(uploads(&ops), ["ok.txt"]);
        assert!(!ops.iter().any(|op| matches!(op, Op::MkdirLocal { .. } | Op::Download { .. })), "{ops:?}");
        assert!(!ops.iter().any(|op| matches!(op, Op::Hold { .. })), "{ops:?}");
    }

    #[test]
    fn excluding_synced_items_plans_nothing_for_them() {
        // Changes on both sides while excluded: a.mp4 gone locally (without
        // evidence), notes.txt edited in the cloud.
        let mut changed = synced_world();
        changed.local.retain(|n| n.id != LocalId(3));
        changed.remote.iter_mut().filter(|n| n.id.0 == "R4").for_each(|n| n.etag = "e2".into());
        let rules = RuleSet::new(&[] as &[&str], &["renders/", "notes.txt"]).unwrap();
        for w in [synced_world(), changed.clone()] {
            let p = Parked::with_rules(&w, &rules);
            let ops = plan(&p, all_folders(&p).unwrap()).unwrap();
            assert!(ops.iter().all(|op| !op.is_action() && !matches!(op, Op::Hold { .. })), "{ops:?}");
        }
        // Without the rules, the same changes are acted on (or held).
        let p = Parked::new(&changed);
        let ops = plan(&p, all_folders(&p).unwrap()).unwrap();
        assert!(ops.iter().any(|op| matches!(op, Op::Hold { .. })), "{ops:?}");
        assert!(ops.iter().any(|op| matches!(op, Op::Download { .. })), "{ops:?}");
    }

    #[test]
    fn a_synced_file_moved_out_of_an_excluded_folder_is_held_not_duplicated() {
        let mut w = synced_world();
        // a.mp4 moved locally from renders/ to the top; the cloud copy is
        // still in the excluded folder.
        w.local.iter_mut().find(|n| n.id == LocalId(3)).unwrap().parent = Some(LocalId(1));
        let rules = RuleSet::new(&[] as &[&str], &["renders/"]).unwrap();
        let p = Parked::with_rules(&w, &rules);
        let folders = all_folders(&p).unwrap();
        let ops = plan(&p, folders).unwrap();
        assert!(!uploads(&ops).contains(&"a.mp4".to_string()), "it has a Synced record: {ops:?}");
        assert!(!ops.iter().any(|op| matches!(op, Op::DeleteRemote { .. } | Op::RecycleLocal { .. })), "{ops:?}");
    }

    #[test]
    fn a_rule_file_can_re_include_and_no_rules_changes_nothing() {
        let mut w = world();
        w.local.push(lfile(10, "keep.iso", 5));
        w.local.push(lfile(11, "other.iso", 5));
        let mut rules = RuleSet::new(&[] as &[&str], &["*.iso"]).unwrap();
        rules.add_ignore_file("", "!keep.iso");
        let ops = plan(&Parked::with_rules(&w, &rules), [ROOT]).unwrap();
        assert_eq!(uploads(&ops), ["keep.iso"]);
        let none = RuleSet::default();
        let ops = plan(&Parked::with_rules(&w, &none), [ROOT]).unwrap();
        let mut up = uploads(&ops);
        up.sort();
        assert_eq!(up, ["keep.iso", "other.iso"]);
    }

    #[test]
    fn a_synced_file_renamed_to_a_parked_name_is_held_not_deleted() {
        let mut w = world();
        w.local.push(lfile(5, "report.tmp", 5));
        w.remote.push(RemoteNode {
            id: RemoteId("f".into()),
            parent: Some(RemoteId("r".into())),
            name: "report.docx".into(),
            kind: NodeKind::File,
            size: 5,
            hash: None,
            etag: "e1".into(),
        });
        w.base.push(Base {
            sid: 2,
            parent: Some(ROOT),
            name: "report.docx".into(),
            kind: NodeKind::File,
            size: 5,
            hash: None,
            local: Some(LocalId(5)),
            local_mtime: 1,
            remote: Some(RemoteId("f".into())),
            remote_etag: "e1".into(),
        });
        let ops = plan(&Parked::new(&w), [ROOT]).unwrap();
        assert!(ops.iter().all(|op| !matches!(op, Op::DeleteRemote { .. } | Op::RecycleLocal { .. })), "{ops:?}");
        assert!(ops.iter().any(|op| matches!(op, Op::Hold { .. })), "{ops:?}");
    }
}
