//! The agent's safety window: the mass-sensitive operations (and the files
//! the content guard judged) it carried out in the last 15 minutes, which
//! the safety stops count on top of each round (`oneup_gate::Recent`).
//!
//! Kept in the state store (`meta` [`KEY`]), so restarting the agent, or
//! the PC, doesn't reset what the stops count: a mass deletion trickling in
//! would otherwise get a fresh allowance after every restart. Bounded: what
//! is older than [`WINDOW_MS`] is dropped, and at most [`MAX_ENTRIES`] are
//! kept (the oldest merge into the next, which only keeps their counts a
//! little longer).
//!
//! Never lower than what ran: the agent records each round's count before
//! the round runs (`oneup_app::sync::Keep`), and a saved window it can't
//! read counts as full for [`WINDOW_MS`] ([`UNREADABLE`]), so every
//! deletion, overwrite or move waits for approval meanwhile.

use std::collections::VecDeque;

use oneup_gate::Recent;
use oneup_store::Store;

/// How far back the safety stops look.
pub const WINDOW_MS: i64 = 15 * 60 * 1000;
/// Entries kept at most (one per sequence of rounds that changed anything).
pub const MAX_ENTRIES: usize = 64;
/// In `meta`: `at_ms,local,remote,suspicious` per entry, `;` between them.
pub const KEY: &str = "safety.window";
/// What each count of a window that can't be read is taken to be: more
/// than any limit, so the stops trip on anything.
pub const UNREADABLE: usize = 1_000_000_000;

/// Mass-sensitive operations carried out recently, by when (Unix ms).
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct SafetyWindow {
    done: VecDeque<(i64, Recent)>,
}

impl SafetyWindow {
    /// The window as saved, at `now_ms`, and whether it could be read.
    /// None saved (never, or nothing in the last 15 minutes) is an empty
    /// window. One that can't be read in full counts as full from now on
    /// ([`UNREADABLE`]), never as less than it held. An entry stamped later
    /// than `now_ms` (the clock was set back) counts as just done, so it
    /// neither expires early nor lasts forever.
    pub fn load(store: &Store, now_ms: i64) -> oneup_store::Result<(Self, bool)> {
        let Some(saved) = store.read(|o| o.meta_get(KEY))? else { return Ok((Self::default(), true)) };
        let parsed: Option<Vec<(i64, Recent)>> =
            if saved.is_empty() { Some(Vec::new()) } else { saved.split(';').map(parse).collect() };
        let Some(mut done) = parsed else {
            let full = Recent { local: UNREADABLE, remote: UNREADABLE, suspicious: UNREADABLE };
            return Ok((Self { done: [(now_ms, full)].into() }, false));
        };
        for (at, _) in &mut done {
            *at = (*at).min(now_ms);
        }
        done.sort_by_key(|(at, _)| *at);
        let mut w = Self { done: done.into() };
        w.trim(now_ms);
        Ok((w, true))
    }

    /// Adds what a sequence of rounds carried out at `now_ms`.
    pub fn add(&mut self, r: Recent, now_ms: i64) {
        if r.local + r.remote + r.suspicious == 0 {
            return;
        }
        // Kept in time order, whatever the clock did meanwhile.
        let at = self.done.back().map_or(now_ms, |(last, _)| now_ms.max(*last));
        self.done.push_back((at, r));
        self.trim(now_ms);
    }

    /// What the window holds at `now_ms`.
    pub fn recent(&mut self, now_ms: i64) -> Recent {
        self.trim(now_ms);
        self.done.iter().fold(Recent::default(), |a, (_, r)| a.plus(*r))
    }

    /// Saves the window with `extra` added at `now_ms` (a sequence of
    /// rounds' counts so far, or what a round is about to carry out),
    /// leaving this copy as it is.
    pub fn save_with(&self, store: &mut Store, extra: Recent, now_ms: i64) -> oneup_store::Result<()> {
        let mut w = self.clone();
        w.add(extra, now_ms);
        w.save(store)
    }

    /// Saves the window to the store.
    pub fn save(&self, store: &mut Store) -> oneup_store::Result<()> {
        let text = self
            .done
            .iter()
            .map(|(at, r)| format!("{at},{},{},{}", r.local, r.remote, r.suspicious))
            .collect::<Vec<_>>()
            .join(";");
        store.write(|o| if text.is_empty() { o.meta_delete(KEY) } else { o.meta_set(KEY, &text) })
    }

    fn trim(&mut self, now_ms: i64) {
        while self.done.front().is_some_and(|(at, _)| now_ms.saturating_sub(*at) > WINDOW_MS) {
            self.done.pop_front();
        }
        while self.done.len() > MAX_ENTRIES {
            if let (Some((_, a)), Some((_, b))) = (self.done.pop_front(), self.done.front_mut()) {
                *b = a.plus(*b);
            }
        }
    }
}

/// One entry exactly as [`SafetyWindow::save`] writes it.
fn parse(entry: &str) -> Option<(i64, Recent)> {
    let f: Vec<&str> = entry.split(',').collect();
    let [at, local, remote, suspicious] = f.as_slice() else { return None };
    let n = |s: &str| s.parse::<usize>().ok();
    Some((at.parse().ok()?, Recent { local: n(local)?, remote: n(remote)?, suspicious: n(suspicious)? }))
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used)]
    use super::*;
    use oneup_sync::Op;

    const MIN: i64 = 60_000;

    fn r(local: usize, remote: usize, suspicious: usize) -> Recent {
        Recent { local, remote, suspicious }
    }

    /// What the stops count adds up across calls of the rounds.
    #[test]
    fn the_safety_window_adds_up_what_ran_and_forgets_after_15_minutes() {
        let mut w = SafetyWindow::default();
        w.add(r(190, 0, 0), 0);
        w.add(Recent::default(), MIN);
        w.add(r(5, 2, 1), 2 * MIN);
        assert_eq!(w.recent(3 * MIN), r(195, 2, 1));
        assert_eq!(w.done.len(), 2, "nothing done, nothing kept");
        assert_eq!(w.recent(16 * MIN), r(5, 2, 1), "the first is over 15 minutes old");
        assert_eq!(w.recent(18 * MIN), Recent::default());
    }

    /// A restart of the agent (or the PC) no longer forgets the window.
    #[test]
    fn the_window_survives_a_restart() {
        let dir = std::env::temp_dir().join(format!("oneup-safety-window-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        let db = dir.join("state.db");
        let t0 = 1_700_000_000_000;
        {
            let mut store = Store::open(&db, 1024).unwrap();
            let (mut w, readable) = SafetyWindow::load(&store, t0).unwrap();
            assert!(readable, "none saved yet is an empty window");
            assert_eq!(w.recent(t0), Recent::default());
            w.add(r(150, 0, 0), t0);
            w.add(r(0, 40, 12), t0 + MIN);
            w.save(&mut store).unwrap();
        }
        let store = Store::open(&db, 1024).unwrap();
        let mut w = SafetyWindow::load(&store, t0 + 5 * MIN).unwrap().0;
        assert_eq!(w.recent(t0 + 5 * MIN), r(150, 40, 12));
        // Still only for 15 minutes.
        let mut w2 = SafetyWindow::load(&store, t0 + 15 * MIN + 1).unwrap().0;
        assert_eq!(w2.recent(t0 + 15 * MIN + 1), r(0, 40, 12));
        drop(store);
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn the_window_is_bounded_and_keeps_its_counts() {
        let mut w = SafetyWindow::default();
        for i in 0..1000 {
            w.add(r(1, 0, 0), i);
        }
        assert_eq!(w.done.len(), MAX_ENTRIES);
        assert_eq!(w.recent(1000), r(1000, 0, 0), "merged, not dropped");
    }

    #[test]
    fn a_clock_set_back_neither_keeps_entries_forever_nor_drops_them_early() {
        let mut store = Store::open_in_memory().unwrap();
        let mut w = SafetyWindow::default();
        w.add(r(100, 0, 0), 10 * 60 * MIN);
        w.save(&mut store).unwrap();
        // The clock went back an hour: the entry counts as just done...
        let mut w = SafetyWindow::load(&store, 9 * 60 * MIN).unwrap().0;
        assert_eq!(w.recent(9 * 60 * MIN), r(100, 0, 0));
        // ...and is over 15 minutes later.
        assert_eq!(w.recent(9 * 60 * MIN + 16 * MIN), Recent::default());
    }

    /// A saved window that can't be read in full never counts as less than
    /// it held: it counts as full for 15 minutes, so the stops trip on any
    /// deletion meanwhile, and saved again it stays so after a restart.
    #[test]
    fn an_unreadable_window_counts_as_full() {
        let mut store = Store::open_in_memory().unwrap();
        let full = r(UNREADABLE, UNREADABLE, UNREADABLE);
        for bad in ["garbage;5,1,2,3", "5,1,2,3;7,x,1,1", "5,1,2", "5,1,2,3,4", "5,-1,2,3", " 5,1,2,3"] {
            store.write(|o| o.meta_set(KEY, bad)).unwrap();
            let (mut w, readable) = SafetyWindow::load(&store, 10).unwrap();
            assert!(!readable, "{bad}");
            assert_eq!(w.recent(10), full, "{bad}");
            let one = vec![Op::Forget(1)];
            let deletion = vec![Op::DeleteRemote {
                sid: 1,
                remote: oneup_sync::RemotePre { id: oneup_sync::RemoteId("r1".into()), etag: "e".into() },
            }];
            let cx = oneup_gate::Context { synced_items: 1_000_000, recent: w.recent(10), ..Default::default() };
            let p = oneup_gate::Policy::default();
            assert_eq!(oneup_gate::review_in(&p, &cx, deletion).breakers.len(), 1, "one deletion trips");
            assert!(oneup_gate::review_in(&p, &cx, one).breakers.is_empty(), "bookkeeping doesn't");
            w.save(&mut store).unwrap();
            let (mut again, readable) = SafetyWindow::load(&store, 10 + MIN).unwrap();
            assert!(readable);
            assert_eq!(again.recent(10 + MIN), full, "still full after a restart");
            assert_eq!(again.recent(10 + 16 * MIN), Recent::default(), "for 15 minutes");
        }
        // Saved by this version, read back the same.
        let mut w = SafetyWindow::default();
        w.add(r(3, 2, 1), 7);
        w.save(&mut store).unwrap();
        assert_eq!(SafetyWindow::load(&store, 8).unwrap(), (w, true));
    }

    /// The agent records a round's count before the round runs: a crash
    /// mid-round (190 deletions done, the process killed) restarts with
    /// them counted.
    #[test]
    fn a_rounds_count_saved_before_it_runs_survives_a_crash() {
        let mut store = Store::open_in_memory().unwrap();
        let mut w = SafetyWindow::default();
        w.add(r(10, 0, 0), 0);
        w.save(&mut store).unwrap();
        // Reserved before the round, which then never got to say how it went.
        w.save_with(&mut store, r(190, 0, 0), MIN).unwrap();
        let (mut after, _) = SafetyWindow::load(&store, 2 * MIN).unwrap();
        assert_eq!(after.recent(2 * MIN), r(200, 0, 0));
        assert_eq!(w.done.len(), 1, "the agent's copy is left as it was");
    }
}
