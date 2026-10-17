//! Which of a round's operations may run side by side.
//!
//! Each operation (or `$batch` group) claims what it touches: the items it
//! changes, the folders it puts things into or takes them out of, and the
//! names it takes or frees. An operation starts once no earlier operation
//! that hasn't finished claims anything it conflicts with, so whatever
//! depends on an earlier operation still runs after it, in the planner's
//! order, and re-checks its preconditions then. Everything else runs
//! concurrently, up to [`Parallel`]'s limits.
//!
//! Claims only decide *when* an operation runs. What it may do is still
//! guarded where it acts: every effect re-checks its own preconditions (a
//! file opened by ID with its size and time, an eTag, a name that must be
//! free), so a claim computed from state that changed meanwhile can turn
//! an operation into a skip, never into a wrong change.

use std::collections::HashMap;
use std::hash::{DefaultHasher, Hash, Hasher};

use oneup_sync::{LocalId, RemoteId, Sid};

/// An item of one of the three trees.
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub(crate) enum Node {
    Synced(Sid),
    Local(LocalId),
    Remote(RemoteId),
}

/// A claimed item, folder or name, as a hash: two that collide only make
/// operations wait for each other, and nothing is cloned to compare them.
type Key = u64;

fn key(k: impl Hash) -> Key {
    let mut h = DefaultHasher::new();
    k.hash(&mut h);
    h.finish()
}

/// What one operation, or one batch, touches.
#[derive(Debug, Default)]
pub(crate) struct Claims {
    /// Items it creates, changes, moves or removes: nothing else may touch
    /// them meanwhile.
    items: Vec<Key>,
    /// Folders it puts something into or takes something out of. Any number
    /// of operations may work in one folder; none may change the folder
    /// itself meanwhile.
    within: Vec<Key>,
    /// Names it takes or frees, as (folder, folded name).
    names: Vec<Key>,
    /// It reaches further than its claims say (a folder's whole subtree,
    /// say): it runs with nothing else.
    pub alone: bool,
    /// The store doesn't have an item it moves or removes, so where that
    /// item is, and the name it frees, aren't known (it runs alone too).
    pub unknown: bool,
    /// An upload large enough to hold a fragment in memory.
    pub large: bool,
}

impl Claims {
    /// Claims for an operation whose items can't be read: it runs with
    /// nothing else.
    pub fn unknown() -> Self {
        Self { alone: true, unknown: true, ..Self::default() }
    }

    pub fn item(&mut self, n: &Node) {
        self.items.push(key(n));
    }

    pub fn within(&mut self, n: &Node) {
        self.within.push(key(n));
    }

    /// Names compare as OneDrive compares them ([`oneup_sync::fold`]).
    pub fn name(&mut self, dir: &Node, name: &str) {
        self.names.push(key((dir, oneup_sync::fold(name))));
    }

    /// Adds `other`'s claims (a batch claims what its members do).
    pub fn merge(&mut self, other: Claims) {
        self.items.extend(other.items);
        self.within.extend(other.within);
        self.names.extend(other.names);
        self.alone |= other.alone;
        self.unknown |= other.unknown;
        self.large |= other.large;
    }
}

/// The claims of several operations together, counted so they can be
/// released one operation at a time.
#[derive(Debug, Default)]
pub(crate) struct Held {
    items: HashMap<Key, usize>,
    within: HashMap<Key, usize>,
    names: HashMap<Key, usize>,
    /// Operations (or batches) holding claims.
    pub units: usize,
    pub alone: usize,
    pub large: usize,
}

fn add(map: &mut HashMap<Key, usize>, keys: &[Key]) {
    for k in keys {
        *map.entry(*k).or_default() += 1;
    }
}

fn remove(map: &mut HashMap<Key, usize>, keys: &[Key]) {
    for k in keys {
        if let Some(n) = map.get_mut(k) {
            *n -= 1;
            if *n == 0 {
                map.remove(k);
            }
        }
    }
}

impl Held {
    /// Whether `c` may not run alongside what is held: they touch the same
    /// item, one changes a folder the other works in, or they take or free
    /// the same name.
    pub fn conflicts(&self, c: &Claims) -> bool {
        c.items.iter().any(|k| self.items.contains_key(k) || self.within.contains_key(k))
            || c.within.iter().any(|k| self.items.contains_key(k))
            || c.names.iter().any(|k| self.names.contains_key(k))
    }

    pub fn add(&mut self, c: &Claims) {
        add(&mut self.items, &c.items);
        add(&mut self.within, &c.within);
        add(&mut self.names, &c.names);
        self.units += 1;
        self.alone += usize::from(c.alone);
        self.large += usize::from(c.large);
    }

    pub fn remove(&mut self, c: &Claims) {
        remove(&mut self.items, &c.items);
        remove(&mut self.within, &c.within);
        remove(&mut self.names, &c.names);
        self.units -= 1;
        self.alone -= usize::from(c.alone);
        self.large -= usize::from(c.large);
    }
}

/// How many operations run at once.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Parallel {
    /// Operations (a `$batch` counts as one) under way at once.
    pub ops: usize,
    /// Of those, uploads larger than one request (above 4 MiB), each of
    /// which holds a fragment in memory (`UploadMemory`).
    pub large_uploads: usize,
}

impl Parallel {
    /// One operation at a time.
    pub const ONE: Self = Self { ops: 1, large_uploads: 1 };

    /// The memory profile's limits. Small transfers wait mostly on the
    /// network, so several at once is where the time goes; large uploads
    /// already keep the connection busy and hold a fragment each (up to
    /// 10, 40 or 60 MiB), so fewer of them run together.
    pub fn for_profile(name: &str) -> Self {
        match name {
            "minimal" => Self { ops: 4, large_uploads: 1 },
            "performance" => Self { ops: 16, large_uploads: 4 },
            _ => Self { ops: 8, large_uploads: 2 },
        }
    }

    /// The Graph client's limits on requests in flight, `(to start with,
    /// at most)`: enough for every operation under way to have its
    /// request out, with room for the round's listing and checks. The
    /// client starts there and grows towards the most while OneDrive
    /// answers, and halves when it throttles (`oneup_graph::limiter`).
    pub fn requests(self) -> (usize, usize) {
        let ops = self.ops.max(1);
        (ops, (ops * 2).min(32))
    }

    /// At least one of each, whatever was set.
    pub(crate) fn at_least_one(self) -> Self {
        Self { ops: self.ops.max(1), large_uploads: self.large_uploads.max(1) }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn l(n: u128) -> Node {
        Node::Local(LocalId(n))
    }

    /// Claims on local items `items`, in folders `within`, of names `names`
    /// in folder 10.
    fn claims(items: &[u128], within: &[u128], names: &[&str]) -> Claims {
        let mut c = Claims::default();
        items.iter().for_each(|&n| c.item(&l(n)));
        within.iter().for_each(|&n| c.within(&l(n)));
        names.iter().for_each(|n| c.name(&l(10), n));
        c
    }

    fn held(cs: &[&Claims]) -> Held {
        let mut h = Held::default();
        cs.iter().for_each(|c| h.add(c));
        h
    }

    #[test]
    fn work_in_one_folder_runs_together_but_not_with_a_change_to_it() {
        let upload_a = claims(&[1], &[10], &[]);
        let upload_b = claims(&[2], &[10], &[]);
        let move_folder = claims(&[10], &[20], &[]);
        assert!(!held(&[&upload_a]).conflicts(&upload_b));
        assert!(held(&[&upload_a]).conflicts(&move_folder));
        assert!(held(&[&move_folder]).conflicts(&upload_b));
    }

    #[test]
    fn the_same_item_or_name_waits() {
        assert!(held(&[&claims(&[1], &[], &[])]).conflicts(&claims(&[1, 2], &[], &[])));
        let frees = claims(&[3], &[], &["x"]);
        assert!(held(&[&frees]).conflicts(&claims(&[4], &[], &["x"])));
        assert!(!held(&[&frees]).conflicts(&claims(&[5], &[], &["y"])));
        // The same name in another folder is another name.
        let mut elsewhere = Claims::default();
        elsewhere.name(&l(11), "x");
        assert!(!held(&[&frees]).conflicts(&elsewhere));
    }

    #[test]
    fn every_operation_under_way_can_have_a_request_out() {
        for profile in ["minimal", "balanced", "performance"] {
            let p = Parallel::for_profile(profile);
            let (start, most) = p.requests();
            assert_eq!(start, p.ops, "{profile}");
            assert!(most > start && most <= 32, "{profile}");
        }
        assert_eq!(Parallel { ops: 0, large_uploads: 0 }.requests(), (1, 2));
    }

    #[test]
    fn released_claims_free_what_they_held() {
        let a = claims(&[1], &[10], &[]);
        let mut b = claims(&[2], &[10], &[]);
        b.large = true;
        let mut h = held(&[&a, &b]);
        let folder = claims(&[10], &[], &[]);
        h.remove(&a);
        assert!(h.conflicts(&folder), "b still works in the folder");
        assert_eq!((h.units, h.large), (1, 1));
        h.remove(&b);
        assert!(!h.conflicts(&folder));
        assert_eq!((h.units, h.large, h.alone), (0, 0, 0));
    }
}
