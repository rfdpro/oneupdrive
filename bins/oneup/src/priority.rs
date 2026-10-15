//! Transfer priority (`rules.md` § User controls): what the user wants synced first, or
//! last, within a round. `config.toml` `[priority] first` / `last` hold
//! rules in the same gitignore syntax as exclusions.
//!
//! Only transfers (uploads and downloads) change places, and only among
//! themselves: every other operation keeps its slot in the planner's order.
//! A transfer that needed something earlier in the round would only fail
//! its precondition and be retried next round; nothing is done differently.

use oneup_rules::RuleSet;
use oneup_sync::Op;

/// The priority rules.
#[derive(Debug, Default)]
pub struct Priority {
    first: Option<RuleSet>,
    last: Option<RuleSet>,
}

impl Priority {
    pub fn new(first: &[String], last: &[String]) -> Self {
        let set =
            |lines: &[String]| (!lines.is_empty()).then(|| RuleSet::new(&[] as &[String], lines).unwrap_or_default());
        Self { first: set(first), last: set(last) }
    }

    pub fn is_empty(&self) -> bool {
        self.first.is_none() && self.last.is_none()
    }

    /// 0 first, 1 as planned, 2 last. `path` relative to the synced folder.
    fn rank(&self, path: &str) -> u8 {
        let hit = |s: &Option<RuleSet>| s.as_ref().is_some_and(|s| s.excludes(path, false));
        if hit(&self.first) {
            0
        } else if hit(&self.last) {
            2
        } else {
            1
        }
    }

    /// The new order of `ops` (with their paths) as indices into them.
    pub fn order(&self, ops: &[(&Op, &str)]) -> Vec<usize> {
        let mut order: Vec<usize> = (0..ops.len()).collect();
        if self.is_empty() {
            return order;
        }
        let slots: Vec<usize> =
            (0..ops.len()).filter(|&i| matches!(ops[i].0, Op::Upload { .. } | Op::Download { .. })).collect();
        let mut moved = slots.clone();
        moved.sort_by_key(|&i| (self.rank(ops[i].1), i));
        for (slot, i) in slots.into_iter().zip(moved) {
            order[slot] = i;
        }
        order
    }
}

/// Reorders `items` (each with its operation and path) by `p`.
pub fn apply<T>(p: &Priority, items: Vec<T>, key: impl Fn(&T) -> (&Op, &str)) -> Vec<T> {
    if p.is_empty() {
        return items;
    }
    let order = p.order(&items.iter().map(&key).collect::<Vec<_>>());
    let mut slots: Vec<Option<T>> = items.into_iter().map(Some).collect();
    order.into_iter().filter_map(|i| slots[i].take()).collect()
}

#[cfg(test)]
mod tests {
    use super::*;
    use oneup_sync::{Link, LocalId, LocalPre, RemoteId, Sid};

    fn up(name: &str) -> Op {
        Op::Upload {
            local: LocalPre { id: LocalId(1), stamp: None },
            to: RemoteId("r".into()),
            parent: 0 as Sid,
            name: name.into(),
            replace: None,
            link: Link::New,
        }
    }

    #[test]
    fn only_transfers_change_places() {
        let ops = [up("big.iso"), Op::Forget(9), up("notes.txt"), up("report.docx"), Op::Forget(8)];
        let paths = ["big.iso", "-", "notes.txt", "work/report.docx", "-"];
        let items: Vec<(&Op, &str)> = ops.iter().zip(paths).collect();
        let p = Priority::new(&["/work/".into()], &["*.iso".into()]);
        let names: Vec<&str> = apply(&p, items, |x| *x).into_iter().map(|(_, path)| path).collect();
        assert_eq!(names, ["work/report.docx", "-", "notes.txt", "big.iso", "-"]);
        let none = Priority::default();
        let same: Vec<&str> =
            apply(&none, ops.iter().zip(paths).collect(), |x: &(&Op, &str)| *x).into_iter().map(|(_, p)| p).collect();
        assert_eq!(same, paths);
    }
}

#[cfg(test)]
mod sync_now {
    use super::*;

    #[test]
    fn sync_now_paths_rank_first() {
        let lines = oneup_rules::literal_path_rules("z-first.txt");
        let p = Priority::new(&lines, &[]);
        assert_eq!(p.rank("/z-first.txt"), 0, "{lines:?}");
    }
}
