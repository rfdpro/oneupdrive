//! Deterministic simulation of the reconciler against an in-memory disk and
//! an in-memory OneDrive (`docs/architecture/sync-engine.md` § Simulation).
//!
//! The world models what matters for safety: local file IDs that change on
//! atomic saves, cloud eTags and version history, recycle bins on both
//! sides, and deletion *evidence*. The executor here enforces every
//! operation's preconditions exactly as the real one must, so a plan made
//! from a stale view fails instead of doing damage. Random user edits are
//! injected between sync rounds *and between operations within a round*.
//!
//! Checked:
//! 1. convergence once users stop (both sides identical, Synced tree
//!    consistent, nothing held);
//! 2. no lost edits: content that was new or changed when users stopped is
//!    on both sides at the end;
//! 3. `data-safety.md` invariant 1: local content is only replaced or recycled
//!    when the cloud holds it (current version, history or recycle bin).
//!
//! 4. the SQLite store plans exactly like the in-memory world: every round
//!    is also planned from a store holding a copy of the world, and the two
//!    plans must be identical (`oneup_store::synced`'s `Trees` impl).
//!
//! `SIM_SEEDS=N` runs more seeds (default 200), `SIM_SEED_FROM=k` starts at
//! seed `k` instead of 1 (to split a long run), `SIM_SEED=n` replays one,
//! `SIM_DEBUG=1` prints each round's operations, `SIM_STORE=0` skips check 4
//! (for long runs: it multiplies the run time).

use std::collections::{BTreeMap, BTreeSet};
use std::convert::Infallible;

use oneup_store::{Kind, LocalKind, LocalRow, RemoteRow, Skip, Store};
use oneup_sync::{
    Base, Flow, Hash, HoldReason, Link, LocalId, LocalNode, LocalPre, NodeKind, Op, ROOT, RemoteId, RemoteNode,
    RemotePre, Seen, Sid, Trees, all_folders, fold, plan, plan_flow,
};

/// File content is an opaque token; every user write makes a new one.
type Content = u64;

fn hash_of(c: Content) -> Hash {
    let mut h = [0u8; 20];
    h[..8].copy_from_slice(&c.to_le_bytes());
    h
}

/// Tiny sizes on purpose: same size with different content must be common,
/// so decisions that need a hash are exercised.
fn size_of(c: Content) -> i64 {
    (c % 3) as i64
}

const LOCAL_ROOT: u128 = 1;
const REMOTE_ROOT: &str = "r0";

#[derive(Debug, Clone)]
struct LItem {
    parent: Option<u128>,
    name: String,
    kind: NodeKind,
    content: Content,
    mtime: i64,
}

#[derive(Debug, Clone)]
struct RItem {
    parent: Option<String>,
    name: String,
    kind: NodeKind,
    content: Content,
    etag: u64,
}

#[derive(Debug, Clone)]
struct World {
    local: BTreeMap<u128, LItem>,
    /// Local IDs removed with evidence (USN delete records).
    local_gone: BTreeSet<u128>,
    next_local: u128,
    clock: i64,
    /// (file ID, mtime) pairs whose hash has been computed.
    hashed: BTreeSet<(u128, i64)>,
    local_bin: Vec<Content>,
    local_versions: Vec<Content>,

    remote: BTreeMap<String, RItem>,
    /// Remote IDs removed with evidence (delta `deleted` facets).
    remote_gone: BTreeSet<String>,
    next_remote: u64,
    next_etag: u64,
    remote_bin: Vec<Content>,
    remote_versions: Vec<Content>,

    bases: BTreeMap<Sid, Base>,
    next_sid: Sid,
    next_content: Content,
}

impl World {
    fn new() -> Self {
        let mut w = World {
            local: BTreeMap::new(),
            local_gone: BTreeSet::new(),
            next_local: LOCAL_ROOT + 1,
            clock: 1,
            hashed: BTreeSet::new(),
            local_bin: Vec::new(),
            local_versions: Vec::new(),
            remote: BTreeMap::new(),
            remote_gone: BTreeSet::new(),
            next_remote: 1,
            next_etag: 1,
            remote_bin: Vec::new(),
            remote_versions: Vec::new(),
            bases: BTreeMap::new(),
            next_sid: ROOT + 1,
            next_content: 1,
        };
        w.local.insert(
            LOCAL_ROOT,
            LItem { parent: None, name: String::new(), kind: NodeKind::Folder, content: 0, mtime: 0 },
        );
        w.remote.insert(
            REMOTE_ROOT.into(),
            RItem { parent: None, name: "root".into(), kind: NodeKind::Folder, content: 0, etag: 0 },
        );
        w.bases.insert(
            ROOT,
            Base {
                sid: ROOT,
                parent: None,
                name: String::new(),
                kind: NodeKind::Folder,
                size: 0,
                hash: None,
                local: Some(LocalId(LOCAL_ROOT)),
                local_mtime: 0,
                remote: Some(RemoteId(REMOTE_ROOT.into())),
                remote_etag: "0".into(),
            },
        );
        w
    }

    fn content(&mut self) -> Content {
        self.next_content += 1;
        self.next_content
    }

    fn tick(&mut self) -> i64 {
        self.clock += 1;
        self.clock
    }

    fn etag(&mut self) -> u64 {
        self.next_etag += 1;
        self.next_etag
    }

    // ------------------------------------------------------------ local fs

    fn lnode(&self, id: u128, it: &LItem) -> LocalNode {
        let file = it.kind == NodeKind::File;
        LocalNode {
            id: LocalId(id),
            parent: it.parent.map(LocalId),
            name: it.name.clone(),
            kind: it.kind,
            size: if file { size_of(it.content) } else { 0 },
            mtime: if file { it.mtime } else { 0 },
            hash: (file && self.hashed.contains(&(id, it.mtime))).then(|| hash_of(it.content)),
        }
    }

    fn l_child(&self, parent: u128, name: &str) -> Option<u128> {
        let key = fold(name);
        self.local.iter().find(|(_, it)| it.parent == Some(parent) && fold(&it.name) == key).map(|(id, _)| *id)
    }

    fn l_is_folder(&self, id: u128) -> bool {
        self.local.get(&id).is_some_and(|it| it.kind == NodeKind::Folder)
    }

    fn l_within(&self, dir: u128, item: u128) -> bool {
        let mut cur = Some(dir);
        while let Some(id) = cur {
            if id == item {
                return true;
            }
            cur = self.local.get(&id).and_then(|it| it.parent);
        }
        false
    }

    fn l_subtree(&self, id: u128) -> Vec<u128> {
        let mut out = vec![id];
        let mut i = 0;
        while i < out.len() {
            let p = out[i];
            out.extend(self.local.iter().filter(|(_, it)| it.parent == Some(p)).map(|(c, _)| *c));
            i += 1;
        }
        out
    }

    fn l_create(&mut self, parent: u128, name: &str, kind: NodeKind, content: Content) -> u128 {
        let id = self.next_local;
        self.next_local += 1;
        let mtime = if kind == NodeKind::File { self.tick() } else { 0 };
        self.local.insert(id, LItem { parent: Some(parent), name: name.into(), kind, content, mtime });
        id
    }

    fn pre_ok(&self, p: &LocalPre) -> bool {
        let Some(it) = self.local.get(&p.id.0) else { return false };
        match p.stamp {
            Some((size, mtime)) => it.kind == NodeKind::File && size_of(it.content) == size && it.mtime == mtime,
            None => it.kind == NodeKind::Folder,
        }
    }

    // ------------------------------------------------------------ cloud

    fn rnode(&self, id: &str, it: &RItem) -> RemoteNode {
        let file = it.kind == NodeKind::File;
        RemoteNode {
            id: RemoteId(id.into()),
            parent: it.parent.clone().map(RemoteId),
            name: it.name.clone(),
            kind: it.kind,
            size: if file { size_of(it.content) } else { 0 },
            hash: file.then(|| hash_of(it.content)),
            etag: it.etag.to_string(),
        }
    }

    fn r_child(&self, parent: &str, name: &str) -> Option<String> {
        let key = fold(name);
        self.remote
            .iter()
            .find(|(_, it)| it.parent.as_deref() == Some(parent) && fold(&it.name) == key)
            .map(|(id, _)| id.clone())
    }

    fn r_is_folder(&self, id: &str) -> bool {
        self.remote.get(id).is_some_and(|it| it.kind == NodeKind::Folder)
    }

    fn r_within(&self, dir: &str, item: &str) -> bool {
        let mut cur = Some(dir.to_owned());
        while let Some(id) = cur {
            if id == item {
                return true;
            }
            cur = self.remote.get(&id).and_then(|it| it.parent.clone());
        }
        false
    }

    fn r_subtree(&self, id: &str) -> Vec<String> {
        let mut out = vec![id.to_owned()];
        let mut i = 0;
        while i < out.len() {
            let p = out[i].clone();
            out.extend(self.remote.iter().filter(|(_, it)| it.parent.as_deref() == Some(&p)).map(|(c, _)| c.clone()));
            i += 1;
        }
        out
    }

    fn r_create(&mut self, parent: &str, name: &str, kind: NodeKind, content: Content) -> String {
        let id = format!("r{}", self.next_remote);
        self.next_remote += 1;
        let etag = self.etag();
        self.remote.insert(id.clone(), RItem { parent: Some(parent.into()), name: name.into(), kind, content, etag });
        id
    }

    fn rpre_ok(&self, p: &RemotePre) -> bool {
        self.remote.get(&p.id.0).is_some_and(|it| it.etag.to_string() == p.etag)
    }

    /// Everything the cloud can still give back: live files, version
    /// history, recycle bin.
    fn cloud_has(&self, c: Content) -> bool {
        self.remote.values().any(|it| it.kind == NodeKind::File && it.content == c)
            || self.remote_versions.contains(&c)
            || self.remote_bin.contains(&c)
    }

    fn base_mut(&mut self, sid: Sid) -> Option<&mut Base> {
        self.bases.get_mut(&sid)
    }

    fn link(&mut self, link: Link, b: Base) -> bool {
        match link {
            Link::New => {
                let sid = self.next_sid;
                self.next_sid += 1;
                self.bases.insert(sid, Base { sid, ..b });
                true
            }
            Link::Existing(sid) => match self.base_mut(sid) {
                Some(slot) => {
                    *slot = Base { sid, ..b };
                    true
                }
                None => false,
            },
        }
    }
}

impl Trees for World {
    type Error = Infallible;

    fn base(&self, sid: Sid) -> Result<Option<Base>, Infallible> {
        Ok(self.bases.get(&sid).cloned())
    }
    fn base_children(&self, sid: Sid) -> Result<Vec<Base>, Infallible> {
        Ok(self.bases.values().filter(|b| b.parent == Some(sid)).cloned().collect())
    }
    fn base_by_local(&self, id: LocalId) -> Result<Option<Base>, Infallible> {
        Ok(self.bases.values().find(|b| b.local == Some(id)).cloned())
    }
    fn base_by_remote(&self, id: &RemoteId) -> Result<Option<Base>, Infallible> {
        Ok(self.bases.values().find(|b| b.remote.as_ref() == Some(id)).cloned())
    }
    fn local(&self, id: LocalId) -> Result<Seen<LocalNode>, Infallible> {
        Ok(match self.local.get(&id.0) {
            Some(it) => Seen::Present(self.lnode(id.0, it)),
            None if self.local_gone.contains(&id.0) => Seen::Deleted,
            None => Seen::Missing,
        })
    }
    fn remote(&self, id: &RemoteId) -> Result<Seen<RemoteNode>, Infallible> {
        Ok(match self.remote.get(&id.0) {
            Some(it) => Seen::Present(self.rnode(&id.0, it)),
            None if self.remote_gone.contains(&id.0) => Seen::Deleted,
            None => Seen::Missing,
        })
    }
    fn local_children(&self, id: LocalId) -> Result<Vec<LocalNode>, Infallible> {
        Ok(self.local.iter().filter(|(_, it)| it.parent == Some(id.0)).map(|(c, it)| self.lnode(*c, it)).collect())
    }
    fn remote_children(&self, id: &RemoteId) -> Result<Vec<RemoteNode>, Infallible> {
        Ok(self
            .remote
            .iter()
            .filter(|(_, it)| it.parent.as_deref() == Some(id.0.as_str()))
            .map(|(c, it)| self.rnode(c, it))
            .collect())
    }
}

// ---------------------------------------------------------------- executor

impl World {
    /// Carries out one operation if its preconditions still hold, as the
    /// real executor must. Returns whether it ran.
    fn apply(&mut self, op: &Op) -> bool {
        match op {
            Op::Pair { local, remote, parent, name, kind, size, hash } => {
                if !self.pre_ok(local) || !self.rpre_ok(remote) {
                    return false;
                }
                let b = Base {
                    sid: 0,
                    parent: Some(*parent),
                    name: name.clone(),
                    kind: *kind,
                    size: *size,
                    hash: *hash,
                    local: Some(local.id),
                    local_mtime: local.stamp.map_or(0, |s| s.1),
                    remote: Some(remote.id.clone()),
                    remote_etag: remote.etag.clone(),
                };
                self.link(Link::New, b)
            }
            Op::Record(b) => match self.base_mut(b.sid) {
                Some(slot) => {
                    *slot = b.clone();
                    true
                }
                None => false,
            },
            Op::Forget(sid) => self.bases.remove(sid).is_some(),
            Op::Hash(p) => {
                if !self.pre_ok(p) {
                    return false;
                }
                let mtime = self.local[&p.id.0].mtime;
                self.hashed.insert((p.id.0, mtime))
            }
            Op::Upload { local, to, parent, name, replace, link } => {
                if !self.pre_ok(local) || local.stamp.is_none() || !self.r_is_folder(&to.0) {
                    return false;
                }
                let it = self.local[&local.id.0].clone();
                let rid = match replace {
                    Some(rp) => {
                        if !self.rpre_ok(rp) {
                            return false;
                        }
                        let etag = self.etag();
                        let Some(r) = self.remote.get_mut(&rp.id.0) else { return false };
                        self.remote_versions.push(r.content);
                        r.content = it.content;
                        r.etag = etag;
                        rp.id.0.clone()
                    }
                    None => {
                        if self.r_child(&to.0, name).is_some() {
                            return false;
                        }
                        self.r_create(&to.0, name, NodeKind::File, it.content)
                    }
                };
                let etag = self.remote[&rid].etag.to_string();
                self.link(*link, file_base(*parent, name, it.content, local.id, it.mtime, rid, etag))
            }
            Op::Download { remote, to, parent, name, replace, link } => {
                if !self.rpre_ok(remote) || !self.l_is_folder(to.0) {
                    return false;
                }
                let r = self.remote[&remote.id.0].clone();
                if r.kind != NodeKind::File {
                    return false;
                }
                let mtime = self.tick();
                let lid = match replace {
                    Some(lp) => {
                        if !self.pre_ok(lp) {
                            return false;
                        }
                        let old = self.local[&lp.id.0].content;
                        assert!(
                            self.cloud_has(old),
                            "invariant 1: replaced local content {old} the cloud doesn't hold"
                        );
                        self.local_versions.push(old);
                        let Some(it) = self.local.get_mut(&lp.id.0) else { return false };
                        it.content = r.content;
                        it.mtime = mtime;
                        lp.id.0
                    }
                    None => {
                        if self.l_child(to.0, name).is_some() {
                            return false;
                        }
                        let id = self.l_create(to.0, name, NodeKind::File, r.content);
                        if let Some(it) = self.local.get_mut(&id) {
                            it.mtime = mtime;
                        }
                        id
                    }
                };
                let b =
                    file_base(*parent, name, r.content, LocalId(lid), mtime, remote.id.0.clone(), r.etag.to_string());
                self.link(*link, b)
            }
            Op::MkdirRemote { local, to, parent, name, link } => {
                if !self.pre_ok(local) || !self.r_is_folder(&to.0) || self.r_child(&to.0, name).is_some() {
                    return false;
                }
                let rid = self.r_create(&to.0, name, NodeKind::Folder, 0);
                let etag = self.remote[&rid].etag.to_string();
                self.link(*link, folder_base(*parent, name, local.id, rid, etag))
            }
            Op::MkdirLocal { remote, to, parent, name, link } => {
                if !self.rpre_ok(remote) || !self.l_is_folder(to.0) || self.l_child(to.0, name).is_some() {
                    return false;
                }
                let lid = self.l_create(to.0, name, NodeKind::Folder, 0);
                let b = folder_base(*parent, name, LocalId(lid), remote.id.0.clone(), remote.etag.clone());
                self.link(*link, b)
            }
            Op::MoveRemote { sid, remote, to, parent, name } => {
                if !self.rpre_ok(remote)
                    || !self.r_is_folder(&to.0)
                    || self.r_within(&to.0, &remote.id.0)
                    || self.r_child(&to.0, name).is_some_and(|c| c != remote.id.0)
                    || !self.bases.contains_key(sid)
                {
                    return false;
                }
                let etag = self.etag();
                if let Some(it) = self.remote.get_mut(&remote.id.0) {
                    it.parent = Some(to.0.clone());
                    it.name.clone_from(name);
                    it.etag = etag;
                }
                if let Some(b) = self.base_mut(*sid) {
                    b.parent = Some(*parent);
                    b.name.clone_from(name);
                    b.remote_etag = etag.to_string();
                }
                true
            }
            Op::RenameRemote { remote, dir, name } => {
                if !self.rpre_ok(remote)
                    || self.remote[&remote.id.0].parent.as_deref() != Some(dir.0.as_str())
                    || self.r_child(&dir.0, name).is_some_and(|c| c != remote.id.0)
                {
                    return false;
                }
                let etag = self.etag();
                if let Some(it) = self.remote.get_mut(&remote.id.0) {
                    it.name.clone_from(name);
                    it.etag = etag;
                }
                true
            }
            Op::MoveLocal { sid, local, to, parent, name } => {
                if !self.pre_ok(local)
                    || !self.l_is_folder(to.0)
                    || self.l_within(to.0, local.id.0)
                    || self.l_child(to.0, name).is_some_and(|c| c != local.id.0)
                    || !self.bases.contains_key(sid)
                {
                    return false;
                }
                if let Some(it) = self.local.get_mut(&local.id.0) {
                    it.parent = Some(to.0);
                    it.name.clone_from(name);
                }
                if let Some(b) = self.base_mut(*sid) {
                    b.parent = Some(*parent);
                    b.name.clone_from(name);
                }
                true
            }
            Op::RenameLocal { local, dir, name } | Op::ConflictCopy { local, dir, name, .. } => {
                if !self.pre_ok(local)
                    || self.local[&local.id.0].parent != Some(dir.0)
                    || self.l_child(dir.0, name).is_some_and(|c| c != local.id.0)
                {
                    return false;
                }
                if let Op::ConflictCopy { sid, .. } = op {
                    match self.base_mut(*sid) {
                        Some(b) => b.local = None,
                        None => return false,
                    }
                }
                if let Some(it) = self.local.get_mut(&local.id.0) {
                    it.name.clone_from(name);
                }
                true
            }
            Op::DeleteRemote { sid, remote } => {
                if !self.rpre_ok(remote) || self.remote.values().any(|it| it.parent.as_deref() == Some(&remote.id.0)) {
                    return false;
                }
                if let Some(it) = self.remote.remove(&remote.id.0)
                    && it.kind == NodeKind::File
                {
                    self.remote_bin.push(it.content);
                }
                self.remote_gone.insert(remote.id.0.clone());
                self.bases.remove(sid);
                true
            }
            Op::RecycleLocal { sid, local } => {
                if !self.pre_ok(local) || self.local.values().any(|it| it.parent == Some(local.id.0)) {
                    return false;
                }
                if let Some(it) = self.local.remove(&local.id.0)
                    && it.kind == NodeKind::File
                {
                    assert!(self.cloud_has(it.content), "invariant 1: recycled local content the cloud doesn't hold");
                    self.local_bin.push(it.content);
                }
                self.local_gone.insert(local.id.0);
                self.bases.remove(sid);
                true
            }
            // Never planned by the reconciler (`oneup cloud prune` only).
            Op::PruneRemote { .. } => panic!("the reconciler planned a prune"),
            Op::Dehydrate { .. } => panic!("the reconciler planned a dehydration"),
            Op::Hold { .. } => false,
        }
    }
}

fn file_base(parent: Sid, name: &str, c: Content, local: LocalId, mtime: i64, remote: String, etag: String) -> Base {
    Base {
        sid: 0,
        parent: Some(parent),
        name: name.into(),
        kind: NodeKind::File,
        size: size_of(c),
        hash: Some(hash_of(c)),
        local: Some(local),
        local_mtime: mtime,
        remote: Some(RemoteId(remote)),
        remote_etag: etag,
    }
}

fn folder_base(parent: Sid, name: &str, local: LocalId, remote: String, etag: String) -> Base {
    Base {
        sid: 0,
        parent: Some(parent),
        name: name.into(),
        kind: NodeKind::Folder,
        size: 0,
        hash: None,
        local: Some(local),
        local_mtime: 0,
        remote: Some(RemoteId(remote)),
        remote_etag: etag,
    }
}

// ---------------------------------------------------------------- users

/// Small deterministic PRNG (xorshift) so failures are reproducible by seed.
struct Rng(u64);

impl Rng {
    fn next(&mut self) -> u64 {
        let mut x = self.0;
        x ^= x << 13;
        x ^= x >> 7;
        x ^= x << 17;
        self.0 = x;
        x
    }
    fn below(&mut self, n: usize) -> usize {
        (self.next() % n.max(1) as u64) as usize
    }
    fn chance(&mut self, percent: u64) -> bool {
        self.next() % 100 < percent
    }
    fn pick<T: Clone>(&mut self, v: &[T]) -> Option<T> {
        if v.is_empty() { None } else { Some(v[self.below(v.len())].clone()) }
    }
}

/// Few names, with case variants, so collisions are frequent.
const NAMES: &[&str] = &["a", "A", "b", "c.txt", "C.TXT", "d", "e.md"];

impl World {
    fn user_local(&mut self, rng: &mut Rng) {
        let folders: Vec<u128> =
            self.local.iter().filter(|(_, it)| it.kind == NodeKind::Folder).map(|(i, _)| *i).collect();
        let files: Vec<u128> = self.local.iter().filter(|(_, it)| it.kind == NodeKind::File).map(|(i, _)| *i).collect();
        let items: Vec<u128> = self.local.keys().copied().filter(|i| *i != LOCAL_ROOT).collect();
        let name = rng.pick(NAMES).unwrap_or("a");
        match rng.below(7) {
            0 | 1 => {
                let Some(p) = rng.pick(&folders) else { return };
                if self.l_child(p, name).is_none() {
                    let kind = if rng.chance(70) { NodeKind::File } else { NodeKind::Folder };
                    let c = if kind == NodeKind::File { self.content() } else { 0 };
                    self.l_create(p, name, kind, c);
                }
            }
            2 => {
                let Some(f) = rng.pick(&files) else { return };
                let (c, m) = (self.content(), self.tick());
                if let Some(it) = self.local.get_mut(&f) {
                    it.content = c;
                    it.mtime = m;
                }
            }
            3 => {
                // Atomic save: new file ID under the same name, old ID deleted.
                let Some(f) = rng.pick(&files) else { return };
                let Some(old) = self.local.remove(&f) else { return };
                self.local_gone.insert(f);
                let c = self.content();
                self.l_create(old.parent.unwrap_or(LOCAL_ROOT), &old.name, NodeKind::File, c);
            }
            4 => {
                let Some(i) = rng.pick(&items) else { return };
                let p = self.local[&i].parent.unwrap_or(LOCAL_ROOT);
                if self.l_child(p, name).is_none_or(|c| c == i)
                    && let Some(it) = self.local.get_mut(&i)
                {
                    it.name = name.into();
                }
            }
            5 => {
                let (Some(i), Some(to)) = (rng.pick(&items), rng.pick(&folders)) else { return };
                let n = self.local[&i].name.clone();
                if !self.l_within(to, i)
                    && self.l_child(to, &n).is_none()
                    && let Some(it) = self.local.get_mut(&i)
                {
                    it.parent = Some(to);
                }
            }
            _ => {
                let Some(i) = rng.pick(&items) else { return };
                for id in self.l_subtree(i) {
                    if let Some(it) = self.local.remove(&id) {
                        if it.kind == NodeKind::File {
                            self.local_bin.push(it.content);
                        }
                        self.local_gone.insert(id);
                    }
                }
            }
        }
    }

    fn user_remote(&mut self, rng: &mut Rng) {
        let folders: Vec<String> =
            self.remote.iter().filter(|(_, it)| it.kind == NodeKind::Folder).map(|(i, _)| i.clone()).collect();
        let files: Vec<String> =
            self.remote.iter().filter(|(_, it)| it.kind == NodeKind::File).map(|(i, _)| i.clone()).collect();
        let items: Vec<String> = self.remote.keys().filter(|i| *i != REMOTE_ROOT).cloned().collect();
        let name = rng.pick(NAMES).unwrap_or("a");
        match rng.below(6) {
            0 | 1 => {
                let Some(p) = rng.pick(&folders) else { return };
                if self.r_child(&p, name).is_none() {
                    let kind = if rng.chance(70) { NodeKind::File } else { NodeKind::Folder };
                    let c = if kind == NodeKind::File { self.content() } else { 0 };
                    self.r_create(&p, name, kind, c);
                }
            }
            2 => {
                let Some(f) = rng.pick(&files) else { return };
                let (c, e) = (self.content(), self.etag());
                if let Some(it) = self.remote.get_mut(&f) {
                    self.remote_versions.push(it.content);
                    it.content = c;
                    it.etag = e;
                }
            }
            3 => {
                let Some(i) = rng.pick(&items) else { return };
                let p = self.remote[&i].parent.clone().unwrap_or_default();
                let e = self.etag();
                if self.r_child(&p, name).is_none_or(|c| c == i)
                    && let Some(it) = self.remote.get_mut(&i)
                {
                    it.name = name.into();
                    it.etag = e;
                }
            }
            4 => {
                let (Some(i), Some(to)) = (rng.pick(&items), rng.pick(&folders)) else { return };
                let n = self.remote[&i].name.clone();
                let e = self.etag();
                if !self.r_within(&to, &i)
                    && self.r_child(&to, &n).is_none()
                    && let Some(it) = self.remote.get_mut(&i)
                {
                    it.parent = Some(to);
                    it.etag = e;
                }
            }
            _ => {
                // Deleting a folder deletes its subtree; delta reports the
                // folder, and the mirror records the whole subtree as gone.
                let Some(i) = rng.pick(&items) else { return };
                for id in self.r_subtree(&i) {
                    if let Some(it) = self.remote.remove(&id) {
                        if it.kind == NodeKind::File {
                            self.remote_bin.push(it.content);
                        }
                        self.remote_gone.insert(id);
                    }
                }
            }
        }
    }

    fn user(&mut self, rng: &mut Rng) {
        if rng.chance(50) { self.user_local(rng) } else { self.user_remote(rng) }
    }
}

// ---------------------------------------------------------------- store

/// An in-memory SQLite store holding the same three trees as `w`, the way
/// production fills it: the scan and the mirror write rows, the executor
/// writes Synced rows and hashes, the observers write deletion evidence.
fn store_of(w: &World) -> Store {
    let mut s = Store::open_in_memory().expect("in-memory store");
    s.write(|o| {
        for (id, it) in &w.local {
            let file = it.kind == NodeKind::File;
            let kind = match it.kind {
                _ if *id == LOCAL_ROOT => LocalKind::Root,
                NodeKind::File => LocalKind::File,
                NodeKind::Folder => LocalKind::Folder,
            };
            let size = if file { size_of(it.content) } else { 0 };
            o.local_put(&LocalRow {
                file_id: *id,
                parent_id: it.parent,
                name: it.name.clone(),
                name_lossy: false,
                kind,
                size,
                mtime: it.mtime,
                attributes: 0,
                reparse_tag: 0,
                skip: Skip::None,
            })?;
            if file && w.hashed.contains(&(*id, it.mtime)) {
                o.local_hash_put(*id, size, it.mtime, &hash_of(it.content))?;
            }
        }
        for (id, it) in &w.remote {
            let file = it.kind == NodeKind::File;
            let kind = match it.kind {
                _ if id == REMOTE_ROOT => Kind::Root,
                NodeKind::File => Kind::File,
                NodeKind::Folder => Kind::Folder,
            };
            let row = RemoteRow {
                id: id.clone(),
                parent_id: it.parent.clone(),
                name: it.name.clone(),
                kind,
                size: if file { size_of(it.content) } else { 0 },
                etag: Some(it.etag.to_string()),
                qxh: file.then(|| hash_of(it.content)),
                mtime_ms: None,
            };
            o.upsert_item(&row, false)?;
        }
        for b in w.bases.values() {
            o.synced_put(b)?;
        }
        for id in &w.local_gone {
            o.note_local_deleted(*id)?;
        }
        for id in &w.remote_gone {
            o.note_remote_deleted(id)?;
        }
        let problems = o.check_invariants()?;
        assert!(problems.is_empty(), "store invariants: {problems:?}");
        Ok(())
    })
    .expect("copying the world into the store");
    s
}

fn plan_from_store(w: &World, flow: Flow) -> Vec<Op> {
    let s = store_of(w);
    s.read(|o| {
        let folders = all_folders(o)?;
        plan_flow(o, folders, flow)
    })
    .expect("planning from the store")
}

fn check_store() -> bool {
    std::env::var("SIM_STORE").map_or(true, |v| v != "0")
}

// ---------------------------------------------------------------- rounds

/// Plans from a snapshot, then executes against the live world, optionally
/// letting users act between operations. Returns (actions planned, holds).
fn round(w: &mut World, rng: Option<&mut Rng>, debug: bool) -> (usize, usize) {
    round_flow(w, rng, debug, Flow::TwoWay)
}

fn round_flow(w: &mut World, rng: Option<&mut Rng>, debug: bool, flow: Flow) -> (usize, usize) {
    let snap = w.clone();
    let ops = plan_flow(&snap, all_folders(&snap).unwrap_or_default(), flow).unwrap_or_default();
    if check_store() {
        assert_eq!(plan_from_store(&snap, flow), ops, "the SQLite store planned differently from the in-memory world");
    }
    if flow == Flow::Backup {
        for op in &ops {
            assert!(!op.changes_local(), "a backup changed this PC: {op:?}");
            assert!(
                !matches!(op, Op::DeleteRemote { .. } | Op::PruneRemote { .. }),
                "a backup deleted from the cloud: {op:?}"
            );
        }
    }
    // Folders a safety stop lists to delete later are deleted with evidence
    // on that side, and are folders (`pending_deletes`).
    for op in oneup_sync::pending_deletes(&snap, &snap, &ops, flow).unwrap_or_default() {
        let (sid, gone) = match &op {
            Op::DeleteRemote { sid, .. } => {
                (*sid, snap.bases[sid].local.is_none_or(|l| snap.local_gone.contains(&l.0)))
            }
            Op::RecycleLocal { sid, .. } => {
                (*sid, snap.bases[sid].remote.as_ref().is_none_or(|r| snap.remote_gone.contains(&r.0)))
            }
            _ => panic!("not a deletion: {op:?}"),
        };
        assert!(gone && snap.bases[&sid].kind == NodeKind::Folder, "listed without evidence: {op:?}");
    }
    if debug {
        eprintln!("--- round: {} ops", ops.len());
        for op in &ops {
            eprintln!("  {op:?}");
        }
    }
    let mut rng = rng;
    for op in &ops {
        if let Some(r) = rng.as_deref_mut()
            && r.chance(15)
        {
            w.user(r);
        }
        w.apply(op);
    }
    let actions = ops.iter().filter(|o| o.is_action()).count();
    (actions, ops.len() - actions)
}

/// Rounds until nothing is left to do. Panics if that doesn't happen.
fn settle(w: &mut World, debug: bool) -> usize {
    settle_flow(w, debug, Flow::TwoWay)
}

fn settle_flow(w: &mut World, debug: bool, flow: Flow) -> usize {
    for i in 0..60 {
        let (actions, holds) = round_flow(w, None, debug, flow);
        if actions == 0 {
            return holds;
        }
        assert!(i < 59, "no convergence after 60 rounds");
    }
    unreachable!()
}

/// Path → (kind, content) on one side, with exact names.
fn local_view(w: &World) -> BTreeMap<String, (NodeKind, Content)> {
    let mut out = BTreeMap::new();
    for (id, it) in &w.local {
        if *id == LOCAL_ROOT {
            continue;
        }
        let mut parts = vec![it.name.clone()];
        let mut cur = it.parent;
        while let Some(p) = cur.filter(|p| *p != LOCAL_ROOT) {
            parts.push(w.local[&p].name.clone());
            cur = w.local[&p].parent;
        }
        parts.reverse();
        out.insert(parts.join("/"), (it.kind, it.content));
    }
    out
}

fn remote_view(w: &World) -> BTreeMap<String, (NodeKind, Content)> {
    let mut out = BTreeMap::new();
    for (id, it) in &w.remote {
        if id == REMOTE_ROOT {
            continue;
        }
        let mut parts = vec![it.name.clone()];
        let mut cur = it.parent.clone();
        while let Some(p) = cur.filter(|p| p != REMOTE_ROOT) {
            parts.push(w.remote[&p].name.clone());
            cur = w.remote[&p].parent.clone();
        }
        parts.reverse();
        out.insert(parts.join("/"), (it.kind, it.content));
    }
    out
}

/// Content that's new or changed relative to its Synced row: users' latest
/// edits, which must survive on both sides.
fn unsynced_edits(w: &World) -> BTreeSet<Content> {
    let mut out = BTreeSet::new();
    for (id, it) in &w.local {
        if it.kind == NodeKind::File {
            let b = w.bases.values().find(|b| b.local == Some(LocalId(*id)));
            if b.is_none_or(|b| b.hash != Some(hash_of(it.content))) {
                out.insert(it.content);
            }
        }
    }
    for (id, it) in &w.remote {
        if it.kind == NodeKind::File {
            let b = w.bases.values().find(|b| b.remote.as_ref().is_some_and(|r| r.0 == *id));
            if b.is_none_or(|b| b.hash != Some(hash_of(it.content))) {
                out.insert(it.content);
            }
        }
    }
    out
}

fn assert_converged(w: &World, must_survive: &BTreeSet<Content>) {
    let (l, r) = (local_view(w), remote_view(w));
    assert_eq!(l, r, "sides differ after settling");
    for path in l.keys() {
        assert!(!path.contains(".oneup-staging-"), "staging name left behind: {path}");
    }
    let live: BTreeSet<Content> = l.values().filter(|(k, _)| *k == NodeKind::File).map(|(_, c)| *c).collect();
    for c in must_survive {
        assert!(live.contains(c), "edit {c} was lost");
    }
    // Synced tree: exactly one row per item, linking the two sides of it.
    for b in w.bases.values().filter(|b| b.sid != ROOT) {
        let (Some(lid), Some(rid)) = (&b.local, &b.remote) else { panic!("unlinked row {b:?}") };
        let (Some(li), Some(ri)) = (w.local.get(&lid.0), w.remote.get(&rid.0)) else {
            panic!("stale Synced row {b:?}")
        };
        assert_eq!((li.name.as_str(), ri.name.as_str()), (b.name.as_str(), b.name.as_str()), "{b:?}");
        if b.kind == NodeKind::File {
            assert_eq!((b.hash, li.mtime), (Some(hash_of(ri.content)), b.local_mtime), "{b:?}");
        }
    }
    assert_eq!(w.bases.len(), w.local.len(), "Synced rows vs local items");
}

/// The seeds to run: `SIM_SEED` alone if set (whatever the range), else
/// `SIM_SEEDS` of them (default 200) from `SIM_SEED_FROM` (default 1).
fn seeds() -> std::ops::RangeInclusive<u64> {
    let var = |name: &str| std::env::var(name).ok().and_then(|s| s.parse::<u64>().ok());
    if let Some(only) = var("SIM_SEED") {
        return only..=only;
    }
    let from = var("SIM_SEED_FROM").unwrap_or(1).max(1);
    let count = var("SIM_SEEDS").unwrap_or(200);
    from..=from.saturating_add(count).saturating_sub(1)
}

#[test]
fn random_histories_converge_without_losing_edits() {
    let debug = std::env::var("SIM_DEBUG").is_ok();
    for seed in seeds() {
        let mut rng = Rng(seed.wrapping_mul(0x9E37_79B9_7F4A_7C15) | 1);
        let mut w = World::new();
        let result = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
            for _ in 0..25 {
                for _ in 0..=rng.below(4) {
                    w.user(&mut rng);
                }
                round(&mut w, Some(&mut rng), debug);
            }
            let must = unsynced_edits(&w);
            let holds = settle(&mut w, debug);
            if debug {
                eprintln!("local: {:#?}\nremote: {:#?}\nbases: {:#?}", w.local, w.remote, w.bases);
            }
            assert_eq!(holds, 0, "held items without any missing evidence");
            assert_converged(&w, &must);
        }));
        if let Err(e) = result {
            eprintln!("simulation failed for seed {seed}; replay with SIM_SEED={seed} SIM_DEBUG=1");
            std::panic::resume_unwind(e);
        }
    }
}

/// Backup pairs (`Flow::Backup`): random histories on both sides, then
/// settling. Checked: no plan ever changes this PC or deletes from the cloud
/// (`round_flow`); settling leaves this PC exactly as it was; and every local
/// file is then in the cloud at the same path with the same content, unless
/// it's held as one-way (a folder of the same name in the cloud).
#[test]
fn backups_send_this_pc_and_never_change_it() {
    let debug = std::env::var("SIM_DEBUG").is_ok();
    for seed in seeds() {
        let mut rng = Rng(seed.wrapping_mul(0xD1B5_4A32_D192_ED03) | 1);
        let mut w = World::new();
        let result = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
            for _ in 0..25 {
                for _ in 0..=rng.below(4) {
                    w.user(&mut rng);
                }
                round_flow(&mut w, Some(&mut rng), debug, Flow::Backup);
            }
            let before = local_view(&w);
            settle_flow(&mut w, debug, Flow::Backup);
            assert_eq!(local_view(&w), before, "settling a backup changed this PC");
            let held: BTreeSet<u128> = plan_flow(&w, all_folders(&w).unwrap_or_default(), Flow::Backup)
                .unwrap_or_default()
                .iter()
                .filter_map(|op| match op {
                    Op::Hold { local: Some(l), reason: HoldReason::OneWay, .. } => Some(l.0),
                    _ => None,
                })
                .collect();
            let remote = remote_view(&w);
            if debug {
                eprintln!(
                    "held: {held:?}
local: {:#?}
remote: {:#?}
bases: {:#?}",
                    w.local, w.remote, w.bases
                );
            }
            for (id, it) in &w.local {
                if it.kind != NodeKind::File || held.iter().any(|h| *h == *id || w.l_within(*id, *h)) {
                    continue;
                }
                let path = local_path(&w, *id);
                if held_above(&w, *id, &held) {
                    continue;
                }
                assert_eq!(remote.get(&path), Some(&(NodeKind::File, it.content)), "{path} isn't backed up");
            }
        }));
        if let Err(e) = result {
            eprintln!("backup simulation failed for seed {seed}; replay with SIM_SEED={seed} SIM_DEBUG=1");
            std::panic::resume_unwind(e);
        }
    }
}

/// `id`'s path relative to the local root.
fn local_path(w: &World, id: u128) -> String {
    let mut parts = vec![w.local[&id].name.clone()];
    let mut cur = w.local[&id].parent;
    while let Some(p) = cur.filter(|p| *p != LOCAL_ROOT) {
        parts.push(w.local[&p].name.clone());
        cur = w.local[&p].parent;
    }
    parts.reverse();
    parts.join("/")
}

/// Whether one of `id`'s folders is held (so `id` itself can't be placed).
fn held_above(w: &World, id: u128, held: &BTreeSet<u128>) -> bool {
    let mut cur = w.local[&id].parent;
    while let Some(p) = cur {
        if held.contains(&p) {
            return true;
        }
        cur = w.local.get(&p).and_then(|i| i.parent);
    }
    false
}

// ---------------------------------------------------------------- scenarios

fn synced() -> World {
    let mut w = World::new();
    let c = w.content();
    w.l_create(LOCAL_ROOT, "doc.txt", NodeKind::File, c);
    let docs = w.l_create(LOCAL_ROOT, "Docs", NodeKind::Folder, 0);
    let c = w.content();
    w.l_create(docs, "inner.txt", NodeKind::File, c);
    settle(&mut w, false);
    assert_converged(&w, &BTreeSet::new());
    w
}

/// Edits an item in place (scenario set-up); the item must exist.
fn edit<T>(item: Option<&mut T>, f: impl FnOnce(&mut T)) {
    match item {
        Some(it) => f(it),
        None => panic!("scenario edits a missing item"),
    }
}

fn rid(w: &World, name: &str) -> String {
    w.remote.iter().find(|(_, it)| it.name == name).map(|(i, _)| i.clone()).unwrap_or_default()
}

fn lid(w: &World, name: &str) -> u128 {
    w.local.iter().find(|(_, it)| it.name == name).map(|(i, _)| *i).unwrap_or_default()
}

#[test]
fn first_sync_adopts_identical_files_and_keeps_both_on_difference() {
    let mut w = World::new();
    let same = w.content();
    w.l_create(LOCAL_ROOT, "same.txt", NodeKind::File, same);
    w.r_create(REMOTE_ROOT, "same.txt", NodeKind::File, same);
    let (mine, theirs) = (w.content(), w.content());
    w.l_create(LOCAL_ROOT, "diff.txt", NodeKind::File, mine);
    w.r_create(REMOTE_ROOT, "diff.txt", NodeKind::File, theirs);
    let before = rid(&w, "same.txt");
    settle(&mut w, false);
    assert_converged(&w, &[mine, theirs].into());
    assert_eq!(rid(&w, "same.txt"), before, "identical file adopted, not re-uploaded");
    assert!(w.remote_versions.is_empty() && w.local_versions.is_empty(), "nothing overwritten");
    assert_eq!(w.remote.values().find(|it| it.name == "diff.txt").map(|it| it.content), Some(theirs));
    assert_eq!(w.remote.values().find(|it| it.name == "diff (conflict 1).txt").map(|it| it.content), Some(mine));
}

#[test]
fn atomic_save_updates_the_same_cloud_item() {
    let mut w = synced();
    let (old_lid, old_rid) = (lid(&w, "doc.txt"), rid(&w, "doc.txt"));
    let old = w.local[&old_lid].clone();
    w.local.remove(&old_lid);
    w.local_gone.insert(old_lid);
    let c = w.content();
    w.l_create(LOCAL_ROOT, "doc.txt", NodeKind::File, c);
    settle(&mut w, false);
    assert_converged(&w, &[c].into());
    assert_eq!(rid(&w, "doc.txt"), old_rid, "same cloud item, so its version history continues");
    assert_eq!(w.remote_versions, vec![old.content]);
}

#[test]
fn swapped_names_are_renamed_not_reuploaded() {
    let mut w = synced();
    let (a, b) = (lid(&w, "doc.txt"), lid(&w, "Docs"));
    let (ra, rb) = (rid(&w, "doc.txt"), rid(&w, "Docs"));
    // Swap via a temporary name, as a user would.
    edit(w.local.get_mut(&a), |it| it.name = "tmp".into());
    edit(w.local.get_mut(&b), |it| it.name = "doc.txt".into());
    edit(w.local.get_mut(&a), |it| it.name = "Docs".into());
    settle(&mut w, false);
    assert_converged(&w, &BTreeSet::new());
    assert_eq!((rid(&w, "Docs"), rid(&w, "doc.txt")), (ra, rb), "same cloud items, swapped names");
}

#[test]
fn an_edit_beats_a_deletion() {
    let mut w = synced();
    let (l, r) = (lid(&w, "doc.txt"), rid(&w, "doc.txt"));
    w.local.remove(&l);
    w.local_gone.insert(l);
    let c = w.content();
    edit(w.remote.get_mut(&r), |it| it.content = c);
    let e = w.etag();
    edit(w.remote.get_mut(&r), |it| it.etag = e);
    settle(&mut w, false);
    assert_converged(&w, &[c].into());
    assert_eq!(rid(&w, "doc.txt"), r);
}

#[test]
fn missing_without_evidence_is_held_not_deleted() {
    let mut w = synced();
    let l = lid(&w, "doc.txt");
    w.local.remove(&l); // gone, but no USN record says so
    let r = rid(&w, "doc.txt");
    let ops = plan(&w, all_folders(&w).unwrap_or_default()).unwrap_or_default();
    assert!(ops.iter().all(|o| !matches!(o, Op::DeleteRemote { .. })), "{ops:?}");
    assert!(ops.iter().any(|o| matches!(o, Op::Hold { reason: HoldReason::MissingWithoutEvidence, .. })));
    settle(&mut w, false);
    assert!(w.remote.contains_key(&r), "cloud copy untouched");
}

#[test]
fn a_missing_sync_folder_halts_everything() {
    let mut w = synced();
    w.local.retain(|id, _| *id != LOCAL_ROOT);
    let ops = plan(&w, all_folders(&w).unwrap_or_default()).unwrap_or_default();
    assert!(matches!(ops.as_slice(), [Op::Hold { sid: Some(ROOT), .. }]), "{ops:?}");
}

#[test]
fn a_folder_deleted_in_the_cloud_survives_for_a_new_local_file() {
    let mut w = synced();
    let docs = lid(&w, "Docs");
    let c = w.content();
    w.l_create(docs, "new.txt", NodeKind::File, c);
    let r = rid(&w, "Docs");
    for id in w.r_subtree(&r) {
        let it = w.remote.remove(&id);
        if let Some(it) = it.filter(|it| it.kind == NodeKind::File) {
            w.remote_bin.push(it.content);
        }
        w.remote_gone.insert(id);
    }
    settle(&mut w, false);
    assert_converged(&w, &[c].into());
    let view = remote_view(&w);
    assert!(view.contains_key("Docs/new.txt"), "{view:?}");
    assert!(!view.contains_key("Docs/inner.txt"), "unchanged children follow the deletion: {view:?}");
}

#[test]
fn folders_moved_into_each_other_dont_loop() {
    let mut w = World::new();
    let a = w.l_create(LOCAL_ROOT, "A", NodeKind::Folder, 0);
    let b = w.l_create(LOCAL_ROOT, "B", NodeKind::Folder, 0);
    settle(&mut w, false);
    let (ra, rb) = (rid(&w, "A"), rid(&w, "B"));
    edit(w.local.get_mut(&a), |it| it.parent = Some(b)); // locally: B/A
    edit(w.remote.get_mut(&rb), |it| it.parent = Some(ra.clone())); // in the cloud: A/B
    settle(&mut w, false);
    assert_converged(&w, &BTreeSet::new());
}

/// How a scenario takes an item away from one side.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Gone {
    /// With evidence (a USN delete record, a delta `deleted` facet).
    Deleted,
    /// Without (hidden, or not found by a rescan or a mirror rebuild).
    Missing,
}

fn take_local(w: &mut World, name: &str, how: Gone) {
    let id = lid(w, name);
    w.local.remove(&id);
    if how == Gone::Deleted {
        w.local_gone.insert(id);
    }
}

fn take_remote(w: &mut World, name: &str, how: Gone) {
    let id = rid(w, name);
    w.remote.remove(&id);
    if how == Gone::Deleted {
        w.remote_gone.insert(id);
    }
}

/// The plan for every folder, checked to be the same from the store.
fn plan_both(w: &World) -> Vec<Op> {
    let ops = plan(w, all_folders(w).unwrap_or_default()).unwrap_or_default();
    assert_eq!(plan_from_store(w, Flow::TwoWay), ops, "the SQLite store planned differently");
    ops
}

#[test]
fn a_record_is_forgotten_only_when_both_sides_are_deleted() {
    use Gone::{Deleted, Missing};
    for (local, remote) in [(Deleted, Deleted), (Deleted, Missing), (Missing, Deleted), (Missing, Missing)] {
        let mut w = synced();
        let sid = w.bases.values().find(|b| b.name == "doc.txt").map(|b| b.sid);
        take_local(&mut w, "doc.txt", local);
        take_remote(&mut w, "doc.txt", remote);
        let ops = plan_both(&w);
        if (local, remote) == (Deleted, Deleted) {
            assert!(sid.is_some_and(|s| ops == [Op::Forget(s)]), "{ops:?}");
        } else {
            assert!(ops.is_empty(), "{local:?}/{remote:?}: the record stays and nothing is held: {ops:?}");
        }
    }
}

#[test]
fn a_folder_keeps_its_record_while_a_child_keeps_its_own() {
    let mut w = synced();
    take_local(&mut w, "inner.txt", Gone::Deleted);
    take_remote(&mut w, "inner.txt", Gone::Missing);
    take_local(&mut w, "Docs", Gone::Deleted);
    take_remote(&mut w, "Docs", Gone::Deleted);
    let ops = plan_both(&w);
    assert!(ops.is_empty(), "{ops:?}");
}

/// Both sides hidden at once (moved into excluded folders, say), then back:
/// the kept record still links them, so nothing is hashed, paired or copied.
#[test]
fn items_missing_on_both_sides_come_back_with_their_history() {
    let before = synced();
    let mut w = before.clone();
    take_local(&mut w, "doc.txt", Gone::Missing);
    take_remote(&mut w, "doc.txt", Gone::Missing);
    settle(&mut w, false);
    w.local.clone_from(&before.local);
    w.remote.clone_from(&before.remote);
    let ops = plan_both(&w);
    assert!(ops.is_empty(), "{ops:?}");
    assert_eq!(w.bases, before.bases);
}
