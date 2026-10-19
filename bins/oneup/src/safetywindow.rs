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

use std::collections::VecDeque;

use oneup_gate::Recent;
use oneup_store::Store;

/// How far back the safety stops look.
pub const WINDOW_MS: i64 = 15 * 60 * 1000;
/// Entries kept at most (one per sequence of rounds that changed anything).
pub const MAX_ENTRIES: usize = 64;
/// In `meta`: `at_ms,local,remote,suspicious` per entry, `;` between them.
pub const KEY: &str = "safety.window";

/// Mass-sensitive operations carried out recently, by when (Unix ms).
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct SafetyWindow {
    done: VecDeque<(i64, Recent)>,
}

impl SafetyWindow {
    /// The window as saved, at `now_ms`. Unreadable entries are skipped
    /// (it's our own record; at worst the window starts empty, as before it
    /// was kept). One stamped later than `now_ms` (the clock was set back)
    /// counts as just done, so it neither expires early nor lasts forever.
    pub fn load(store: &Store, now_ms: i64) -> oneup_store::Result<Self> {
        let saved = store.read(|o| o.meta_get(KEY))?.unwrap_or_default();
        let mut done: Vec<(i64, Recent)> = saved.split(';').filter_map(parse).collect();
        for (at, _) in &mut done {
            *at = (*at).min(now_ms);
        }
        done.sort_by_key(|(at, _)| *at);
        let mut w = Self { done: done.into() };
        w.trim(now_ms);
        Ok(w)
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

fn parse(entry: &str) -> Option<(i64, Recent)> {
    let mut f = entry.split(',').map(str::trim);
    let at = f.next()?.parse().ok()?;
    let mut n = || f.next()?.parse::<usize>().ok();
    let (local, remote, suspicious) = (n()?, n()?, n()?);
    Some((at, Recent { local, remote, suspicious }))
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used)]
    use super::*;

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
            let mut w = SafetyWindow::load(&store, t0).unwrap();
            assert_eq!(w.recent(t0), Recent::default());
            w.add(r(150, 0, 0), t0);
            w.add(r(0, 40, 12), t0 + MIN);
            w.save(&mut store).unwrap();
        }
        let store = Store::open(&db, 1024).unwrap();
        let mut w = SafetyWindow::load(&store, t0 + 5 * MIN).unwrap();
        assert_eq!(w.recent(t0 + 5 * MIN), r(150, 40, 12));
        // Still only for 15 minutes.
        let mut w2 = SafetyWindow::load(&store, t0 + 15 * MIN + 1).unwrap();
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
        let mut w = SafetyWindow::load(&store, 9 * 60 * MIN).unwrap();
        assert_eq!(w.recent(9 * 60 * MIN), r(100, 0, 0));
        // ...and is over 15 minutes later.
        assert_eq!(w.recent(9 * 60 * MIN + 16 * MIN), Recent::default());
        // Something unreadable is skipped, not taken for a count.
        store.write(|o| o.meta_set(KEY, "garbage;5,1,2,3;7,x,1,1")).unwrap();
        assert_eq!(SafetyWindow::load(&store, 6).unwrap().recent(6), r(1, 2, 3));
    }
}
