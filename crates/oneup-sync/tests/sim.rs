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
//!    plans must be identical (`oneup_store::synced`'s `Trees` impl);
//! 5. `data-safety.md` invariant 3: nothing is deleted in the cloud without
//!    evidence of its deletion here.
//!
//! Faults: OneDrive's listing lagging behind the cloud ([`Mirror`]), and the
//! change journal losing users' latest deletions ([`World::lose_journal`]).
//!
//! `SIM_SEEDS=N` runs more seeds (default 200; 50 for the fault runs, which
//! take longer per seed), `SIM_SEED_FROM=k` starts at seed `k` instead of 1
//! (to split a long run), `SIM_SEED=n` replays one, `SIM_DEBUG=1` prints each
//! round's operations, `SIM_STORE=0` skips check 4 (for long runs: it
//! multiplies the run time).

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

#[derive(Debug, Clone, PartialEq, Eq)]
struct RItem {
    parent: Option<String>,
    name: String,
    kind: NodeKind,
    content: Content,
    etag: u64,
}

/// OneDrive's listing as the planner reads it when it lags behind the cloud
/// (delta lag): what our own operations got back as they ran, overwritten
/// at the start of each round by a delivery of the cloud's state as it was
/// at the start of the round before. So a delivery can be older than what
/// an upload, a move or a deletion of ours just saw.
#[derive(Debug, Clone)]
struct Mirror {
    rows: BTreeMap<String, RItem>,
    /// Deletions delivered (delta `deleted` facets), and our own.
    gone: BTreeSet<String>,
    /// What the next delivery brings.
    next: (BTreeMap<String, RItem>, BTreeSet<String>),
}

#[derive(Debug, Clone)]
struct World {
    local: BTreeMap<u128, LItem>,
    /// Local IDs removed with evidence (USN delete records).
    local_gone: BTreeSet<u128>,
    /// Users' deletions since the observer last read the journal: what a
    /// journal loss can take ([`World::lose_journal`]).
    unread: Vec<u128>,
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

    /// `Some` while OneDrive's listing lags ([`World::lag`]).
    mirror: Option<Mirror>,

    bases: BTreeMap<Sid, Base>,
    next_sid: Sid,
    next_content: Content,
}

impl World {
    fn new() -> Self {
        let mut w = World {
            local: BTreeMap::new(),
            local_gone: BTreeSet::new(),
            unread: Vec::new(),
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
            mirror: None,
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

    /// Removes a Synced row with every row below it, as the store does
    /// (`Ops::synced_remove`): a record whose folder is gone can't stay.
    fn remove_bases(&mut self, sid: Sid) {
        let mut out = vec![sid];
        let mut i = 0;
        while i < out.len() {
            let p = out[i];
            out.extend(self.bases.values().filter(|b| b.parent == Some(p)).map(|b| b.sid));
            i += 1;
        }
        for s in out {
            self.bases.remove(&s);
        }
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
                // As the executor: only for a Synced row still there.
                let Some(b) = self.bases.get(sid) else { return false };
                assert!(
                    b.remote.as_ref() == Some(&remote.id) && b.local.is_some_and(|l| self.local_gone.contains(&l.0)),
                    "invariant 3: deleted in the cloud without evidence of its deletion here: {op:?} {b:?}"
                );
                if let Some(it) = self.remote.remove(&remote.id.0)
                    && it.kind == NodeKind::File
                {
                    self.remote_bin.push(it.content);
                }
                self.remote_gone.insert(remote.id.0.clone());
                self.remove_bases(*sid);
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
                self.remove_bases(*sid);
                true
            }
            // Never planned by the reconciler (`oneup cloud prune` only).
            Op::PruneRemote { .. } => panic!("the reconciler planned a prune"),
            Op::Dehydrate { .. } => panic!("the reconciler planned a dehydration"),
            Op::Hold { .. } => false,
        }
    }
}

// ---------------------------------------------------------------- journal loss

impl World {
    /// The change journal wraps before the observer reads users' latest
    /// deletions ([`World::unread`]), and the folder is rescanned. The
    /// agent's journal watcher kept some of their delete records
    /// (`SeenDeletes`): those are evidence after the rescan. The rest
    /// (the watcher was wrapped past too, or over its bound) are lost: their
    /// items read as Missing and are held. A rescan finds the folder as it
    /// is, which the model's local side already is.
    fn lose_journal(&mut self, rng: &mut Rng) {
        let watcher_lost_all = rng.chance(30);
        for id in std::mem::take(&mut self.unread) {
            if watcher_lost_all || rng.chance(50) {
                self.local_gone.remove(&id);
            }
        }
    }

    /// Synced items held because they're missing here without evidence
    /// while still in the cloud.
    fn held_missing(&self) -> Vec<&Base> {
        let missing = |l: &LocalId| !self.local.contains_key(&l.0) && !self.local_gone.contains(&l.0);
        let there = |r: &RemoteId| self.remote.contains_key(&r.0);
        self.bases
            .values()
            .filter(|b| b.local.as_ref().is_some_and(missing) && b.remote.as_ref().is_some_and(there))
            .collect()
    }
}

// ---------------------------------------------------------------- delta lag

impl World {
    /// From now on, OneDrive's listing lags behind the cloud ([`Mirror`]).
    fn lag(&mut self) {
        let now = (self.remote.clone(), self.remote_gone.clone());
        self.mirror = Some(Mirror { rows: now.0.clone(), gone: now.1.clone(), next: now });
    }

    /// The start of a round, while the listing lags: it delivers the cloud
    /// as it was at the last delivery, over whatever the mirror holds.
    /// Without `fresh`, nothing new arrives (the lag grows by a round).
    fn deliver(&mut self, fresh: bool) {
        let now = (self.remote.clone(), self.remote_gone.clone());
        let Some(m) = self.mirror.as_mut() else { return };
        if !fresh {
            return;
        }
        let (rows, gone) = std::mem::replace(&mut m.next, now);
        // Deletions first: a folder's deletion takes what's stored below it
        // (`delete_subtree`), and what had moved out comes back with the rows.
        for id in &gone {
            m.delete(id);
        }
        for (id, it) in rows {
            m.gone.remove(&id);
            m.rows.insert(id, it);
        }
    }

    /// The trees the planner reads: the cloud through the mirror while the
    /// listing lags.
    fn view(&self) -> World {
        let mut v = self.clone();
        if let Some(m) = v.mirror.take() {
            v.remote = m.rows;
            v.remote_gone = m.gone;
        }
        v
    }

    /// Whether the listing shows the cloud as it is (always, without lag).
    fn caught_up(&self) -> bool {
        self.mirror.as_ref().is_none_or(|m| m.rows == self.remote && m.gone == self.remote_gone)
    }

    /// Runs `op` ([`World::apply`]) of a round planned from `planned`;
    /// while the listing lags, the mirror stores what it changed in the
    /// cloud, as the executor records the items its requests return.
    fn run(&mut self, op: &Op, planned: &World) -> bool {
        if let Op::MoveLocal { sid, .. } = op {
            // As the executor asks (`Cloud::is_at`): only while OneDrive
            // still has the item where the listing had it, which this move
            // follows. The listing is the mirror, or what the round was
            // planned from.
            let rid = self.bases.get(sid).and_then(|b| b.remote.clone());
            let listing = self.mirror.as_ref().map_or(&planned.remote, |m| &m.rows);
            if let Some(rid) = rid
                && let Some(listed) = listing.get(&rid.0)
                && self.remote.get(&rid.0).is_none_or(|it| (&it.parent, &it.name) != (&listed.parent, &listed.name))
            {
                return false;
            }
        }
        if self.mirror.is_none() {
            return self.apply(op);
        }
        let before = self.remote.clone();
        let ran = self.apply(op);
        if let Some(m) = self.mirror.as_mut() {
            for (id, it) in &self.remote {
                if before.get(id) != Some(it) {
                    m.gone.remove(id);
                    m.rows.insert(id.clone(), it.clone());
                }
            }
            for id in before.keys().filter(|id| !self.remote.contains_key(*id)) {
                m.delete(id);
            }
        }
        ran
    }
}

impl Mirror {
    /// A deletion of `id`: it and everything stored below it are gone, as
    /// the store's `delete_subtree` records them.
    fn delete(&mut self, id: &str) {
        let mut out = vec![id.to_owned()];
        let mut i = 0;
        while i < out.len() {
            let p = out[i].clone();
            out.extend(self.rows.iter().filter(|(_, it)| it.parent.as_deref() == Some(&p)).map(|(c, _)| c.clone()));
            i += 1;
        }
        for c in out {
            self.rows.remove(&c);
            self.gone.insert(c);
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
                self.unread.push(f);
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
                        self.unread.push(id);
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
    let mut rng = rng;
    // A lagging listing sometimes brings nothing new for a while; settling
    // rounds always get the next delivery.
    let fresh = w.mirror.is_none() || rng.as_deref_mut().is_none_or(|r| r.chance(70));
    w.deliver(fresh);
    // The observer has read the journal.
    w.unread.clear();
    let snap = w.view();
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
    for op in &ops {
        if let Some(r) = rng.as_deref_mut()
            && r.chance(15)
        {
            w.user(r);
        }
        w.run(op, &snap);
    }
    assert_no_loop(w);
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
        // Behind the cloud, a quiet round proves nothing yet.
        if actions == 0 && w.caught_up() {
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
/// `SIM_SEEDS` of them (default `default`) from `SIM_SEED_FROM` (default 1).
fn seeds(default: u64) -> std::ops::RangeInclusive<u64> {
    let var = |name: &str| std::env::var(name).ok().and_then(|s| s.parse::<u64>().ok());
    if let Some(only) = var("SIM_SEED") {
        return only..=only;
    }
    let from = var("SIM_SEED_FROM").unwrap_or(1).max(1);
    let count = var("SIM_SEEDS").unwrap_or(default);
    from..=from.saturating_add(count).saturating_sub(1)
}

#[test]
fn random_histories_converge_without_losing_edits() {
    let debug = std::env::var("SIM_DEBUG").is_ok();
    for seed in seeds(200) {
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

/// The same random histories with OneDrive's listing lagging behind the
/// cloud ([`Mirror`]): each round's listing is the cloud as it was a round or
/// more ago, older than what our own uploads, moves and deletions just saw.
/// Plans made from it fail their preconditions in the cloud, or move and
/// rename things here that the next listing moves back; none loses an
/// edit, and once the listing catches up, both sides converge.
#[test]
fn a_lagging_listing_converges_without_losing_edits() {
    let debug = std::env::var("SIM_DEBUG").is_ok();
    for seed in seeds(50) {
        let mut rng = Rng(seed.wrapping_mul(0xA24B_AED4_963E_E407) | 1);
        let mut w = World::new();
        let result = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
            // Some history first, so the lag meets synced items.
            for _ in 0..3 {
                w.user(&mut rng);
            }
            settle(&mut w, debug);
            w.lag();
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
            eprintln!("lagging-listing simulation failed for seed {seed}; replay with SIM_SEED={seed} SIM_DEBUG=1");
            std::panic::resume_unwind(e);
        }
    }
}

/// The same random histories with the change journal lost now and then,
/// while idle or while a round runs ([`World::lose_journal`]): of users'
/// deletions not read yet, those the journal watcher kept are evidence and
/// the rest are held. Checked: nothing is deleted in the cloud without
/// evidence (`World::apply`) and every edit survives on some side; once the
/// user answers for what's missing (deleted here), both sides converge.
#[test]
fn journal_losses_never_invent_or_lose_a_deletion() {
    let debug = std::env::var("SIM_DEBUG").is_ok();
    for seed in seeds(50) {
        let mut rng = Rng(seed.wrapping_mul(0x8EBC_6AF0_9C88_C6E3) | 1);
        let mut w = World::new();
        let result = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
            for _ in 0..25 {
                for _ in 0..=rng.below(4) {
                    w.user(&mut rng);
                }
                if rng.chance(10) {
                    w.lose_journal(&mut rng);
                }
                round(&mut w, Some(&mut rng), debug);
                if rng.chance(20) {
                    w.lose_journal(&mut rng);
                }
            }
            let must = unsynced_edits(&w);
            let holds = settle(&mut w, debug);
            if debug {
                eprintln!("local: {:#?}\nremote: {:#?}\nbases: {:#?}", w.local, w.remote, w.bases);
            }
            // Never deleted in the cloud by sync: `World::apply` checks every
            // deletion there has evidence here. Held items hold up what
            // depends on them (an item moved into a held folder waits, and
            // so does an edit of it), but every edit is still somewhere.
            assert!(holds >= w.held_missing().len().min(1), "held items aren't reported");
            let (l, r) = (local_view(&w), remote_view(&w));
            let live: BTreeSet<Content> = l.values().chain(r.values()).map(|(_, c)| *c).collect();
            for c in &must {
                assert!(live.contains(c), "edit {c} was lost");
            }
            // The user answers for what's missing here: deleted
            // (`oneup unsynced resolve`, evidence naming the item). Then
            // nothing is held, both sides agree, and every edit is on both.
            let missing: Vec<u128> =
                w.bases.values().filter_map(|b| b.local).map(|l| l.0).filter(|l| !w.local.contains_key(l)).collect();
            w.local_gone.extend(missing);
            let holds = settle(&mut w, debug);
            assert_eq!(holds, 0, "held items once nothing is missing without evidence");
            assert_converged(&w, &must);
        }));
        if let Err(e) = result {
            eprintln!("journal-loss simulation failed for seed {seed}; replay with SIM_SEED={seed} SIM_DEBUG=1");
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
    for seed in seeds(200) {
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

/// Renamed here, renamed in OneDrive, renamed here again while the listing
/// still shows the item under its first name (delta lag): the round plans
/// to follow that name, which OneDrive no longer has, so the move here is
/// refused (as the executor refuses it, `Cloud::is_at`), and the second
/// rename is sent once the listing catches up.
#[test]
fn a_lagging_listing_never_undoes_a_rename_here() {
    let mut w = synced();
    w.lag();
    let id = lid(&w, "doc.txt");
    edit(w.local.get_mut(&id), |it| it.name = "b2".into());
    round(&mut w, None, false);
    assert_eq!(w.remote.get(&rid(&w, "b2")).map(|it| it.name.as_str()), Some("b2"));
    edit(w.local.get_mut(&id), |it| it.name = "b3".into());
    round(&mut w, None, false);
    assert_eq!(w.local[&id].name, "b3", "the listing's old name was followed");
    settle(&mut w, false);
    assert_converged(&w, &BTreeSet::new());
    assert_eq!(w.local[&id].name, "b3");
}

/// Every Synced row reaches the top of the Synced tree: a loop is never
/// planned again (full passes walk the tree from its root).
fn assert_no_loop(w: &World) {
    for b in w.bases.values() {
        let mut cur = Some(b.sid);
        for _ in 0..=w.bases.len() {
            cur = cur.and_then(|s| w.bases.get(&s)).and_then(|b| b.parent);
        }
        assert_eq!(cur, None, "{b:?} doesn't reach the top of the Synced tree");
    }
}

/// Synced `C/A` and `X`, with their local IDs `(c, a)`.
fn c_holding_a() -> (World, u128, u128) {
    let mut w = World::new();
    let c = w.l_create(LOCAL_ROOT, "C", NodeKind::Folder, 0);
    let a = w.l_create(c, "A", NodeKind::Folder, 0);
    w.l_create(LOCAL_ROOT, "X", NodeKind::Folder, 0);
    settle(&mut w, false);
    (w, c, a)
}

/// Moves cloud item `name` into cloud folder `into`, as a user on the web.
fn move_in_cloud(w: &mut World, name: &str, into: &str) {
    let (id, to) = (rid(w, name), rid(w, into));
    let e = w.etag();
    edit(w.remote.get_mut(&id), |it| (it.parent, it.etag) = (Some(to), e));
}

/// A round in which A's own operation is refused (its preconditions
/// failed, a name was in the way): what puts C inside A must wait for it,
/// or the Synced tree would have C below A while A is still below C. Then
/// the rounds settle without losing `must`.
fn refuse_a_once_then_settle(mut w: World, must: &BTreeSet<Content>) {
    let sid_a = w.bases.values().find(|b| b.name == "A").map(|b| b.sid);
    for op in plan_both(&w) {
        if op.subjects().0 != sid_a {
            w.apply(&op);
        }
    }
    assert_no_loop(&w);
    settle(&mut w, false);
    assert_converged(&w, must);
}

/// Here, A goes to the top and C into A; in the cloud, A goes into X. C's
/// move into A waits for A's (simulation seed 60953 of the journal-loss
/// run).
#[test]
fn a_folder_never_moves_below_itself_in_the_synced_tree() {
    let (mut w, c, a) = c_holding_a();
    edit(w.local.get_mut(&a), |it| it.parent = Some(LOCAL_ROOT));
    edit(w.local.get_mut(&c), |it| it.parent = Some(a));
    move_in_cloud(&mut w, "A", "X");
    refuse_a_once_then_settle(w, &BTreeSet::new());
}

/// Here, A goes to the top and C is deleted; in the cloud, A goes into X
/// and C into A. The cloud's move beats the deletion, so C is made here
/// again inside A (`local_gone`), once A's move is recorded. With
/// `new_file`, a new cloud file in C needs C here too (`resurrect_local`).
fn deleted_here_moved_there(new_file: bool) {
    let (mut w, _, a) = c_holding_a();
    edit(w.local.get_mut(&a), |it| it.parent = Some(LOCAL_ROOT));
    take_local(&mut w, "C", Gone::Deleted);
    move_in_cloud(&mut w, "A", "X");
    move_in_cloud(&mut w, "C", "A");
    let mut must = BTreeSet::new();
    if new_file {
        let c = w.content();
        let rc = rid(&w, "C");
        w.r_create(&rc, "new.txt", NodeKind::File, c);
        must.insert(c);
    }
    refuse_a_once_then_settle(w, &must);
}

#[test]
fn a_folder_made_here_again_never_lands_below_itself() {
    deleted_here_moved_there(false);
}

#[test]
fn a_folder_made_here_again_for_new_contents_never_lands_below_itself() {
    deleted_here_moved_there(true);
}

/// The mirror image: in the cloud, A goes into X and C is deleted; here,
/// A goes to the top and C into A. C is made in the cloud again inside A
/// (`remote_gone`), once A's move is recorded. With `new_file`, a new file
/// here in C needs C in the cloud too (`resurrect_remote`).
fn deleted_there_moved_here(new_file: bool) {
    let (mut w, c, a) = c_holding_a();
    move_in_cloud(&mut w, "A", "X");
    take_remote(&mut w, "C", Gone::Deleted);
    edit(w.local.get_mut(&a), |it| it.parent = Some(LOCAL_ROOT));
    edit(w.local.get_mut(&c), |it| it.parent = Some(a));
    let mut must = BTreeSet::new();
    if new_file {
        let n = w.content();
        w.l_create(c, "new.txt", NodeKind::File, n);
        must.insert(n);
    }
    refuse_a_once_then_settle(w, &must);
}

#[test]
fn a_folder_made_in_the_cloud_again_never_lands_below_itself() {
    deleted_there_moved_here(false);
}

#[test]
fn a_folder_made_in_the_cloud_again_for_new_contents_never_lands_below_itself() {
    deleted_there_moved_here(true);
}

/// A conflict copy detaches the Synced row from the local file it renames.
/// If the cloud's side then turns out unchanged (the conflict was planned
/// from a listing behind the cloud, simulation seed 6 of the delta-lag
/// run), the cloud's file keeps its name and comes back here, as after any
/// conflict; it isn't deleted as if the file had been deleted here.
#[test]
fn a_conflict_copy_never_deletes_the_cloud_original() {
    let mut w = synced();
    let (l, r) = (lid(&w, "doc.txt"), rid(&w, "doc.txt"));
    let original = w.remote[&r].content;
    let (c, m) = (w.content(), w.tick());
    edit(w.local.get_mut(&l), |it| (it.content, it.mtime, it.name) = (c, m, "doc (conflict 1).txt".into()));
    let sid = w.bases.values().find(|b| b.name == "doc.txt").map_or(0, |b| b.sid);
    edit(w.bases.get_mut(&sid), |b| b.local = None);
    let ops = plan_both(&w);
    assert!(!ops.iter().any(|o| matches!(o, Op::DeleteRemote { .. })), "{ops:?}");
    settle(&mut w, false);
    assert_converged(&w, &[c].into());
    assert_eq!(w.remote.get(&r).map(|it| (it.name.as_str(), it.content)), Some(("doc.txt", original)));
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
