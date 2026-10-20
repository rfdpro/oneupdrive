//! The agent's safety window: the mass-sensitive operations (and the files
//! the content guard judged) it carried out in the last 15 minutes, which
//! the safety stops count on top of each round (`oneup_gate::Recent`).
//!
//! Kept in the state store (`meta` [`KEY`]), so restarting the agent, or
//! the PC, doesn't reset what the stops count: a mass deletion trickling in
//! would otherwise get a fresh allowance after every restart. Counts are
//! kept per minute ([`BUCKET_MS`]): a minute's counts go once the last
//! moment of that minute is [`WINDOW_MS`] old, so nothing is forgotten
//! early, nothing lasts more than a minute longer, and at most 16 minutes
//! are kept however busy the agent is. A minute later than now (the clock
//! was set back) is counted as now's, on every look.
//!
//! Never lower than what ran: the agent records each round's count before
//! the round runs (`oneup_app::sync::Keep`), and a saved window it can't
//! read counts as full for [`WINDOW_MS`] ([`UNREADABLE`]), so every
//! deletion, overwrite or move waits for approval meanwhile.

use std::collections::BTreeMap;

use oneup_gate::Recent;
use oneup_store::Store;

/// How far back the safety stops look.
pub const WINDOW_MS: i64 = 15 * 60 * 1000;
/// The window's counts are kept per this long.
pub const BUCKET_MS: i64 = 60 * 1000;
/// In `meta`: `minute_ms,local,remote,suspicious` per minute, `;` between.
pub const KEY: &str = "safety.window";
/// What each count of a window that can't be read is taken to be: more
/// than any limit ([`oneup_gate::FULL`], as for a removal of unknown size),
/// so the stops trip on anything. Also the most a count is kept as; a saved
/// count above it can't be ours, so it's unreadable.
pub const UNREADABLE: usize = oneup_gate::FULL;

/// Mass-sensitive operations carried out recently, by the minute they were
/// carried out in (its start, Unix ms).
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct SafetyWindow {
    done: BTreeMap<i64, Recent>,
}

/// The start of the minute `ms` is in.
fn bucket(ms: i64) -> i64 {
    ms.div_euclid(BUCKET_MS) * BUCKET_MS
}

/// Counts as kept: never above [`UNREADABLE`].
fn capped(r: Recent) -> Recent {
    Recent {
        local: r.local.min(UNREADABLE),
        remote: r.remote.min(UNREADABLE),
        suspicious: r.suspicious.min(UNREADABLE),
    }
}

impl SafetyWindow {
    /// The window as saved, at `now_ms`, and whether it could be read.
    /// None saved (never, or nothing in the last 15 minutes) is an empty
    /// window. One that can't be read in full counts as full from now on
    /// ([`UNREADABLE`]), never as less than it held: anything but minutes
    /// exactly as [`save`](Self::save) writes them, or a count above
    /// [`UNREADABLE`].
    pub fn load(store: &Store, now_ms: i64) -> oneup_store::Result<(Self, bool)> {
        let Some(saved) = store.read(|o| o.meta_get(KEY))? else { return Ok((Self::default(), true)) };
        let parsed: Option<Vec<(i64, Recent)>> =
            if saved.is_empty() { Some(Vec::new()) } else { saved.split(';').map(parse).collect() };
        let mut w = Self::default();
        let Some(done) = parsed else {
            let full = Recent { local: UNREADABLE, remote: UNREADABLE, suspicious: UNREADABLE };
            w.add(full, now_ms);
            return Ok((w, false));
        };
        for (at, r) in done {
            let e = w.done.entry(bucket(at)).or_default();
            *e = capped(e.plus(r));
        }
        w.trim(now_ms);
        Ok((w, true))
    }

    /// Adds what was carried out at `now_ms`.
    pub fn add(&mut self, r: Recent, now_ms: i64) {
        self.trim(now_ms);
        if r == Recent::default() {
            return;
        }
        let e = self.done.entry(bucket(now_ms)).or_default();
        *e = capped(e.plus(r));
    }

    /// What the window holds at `now_ms`.
    pub fn recent(&mut self, now_ms: i64) -> Recent {
        self.trim(now_ms);
        self.done.values().fold(Recent::default(), |a, r| a.plus(*r))
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

    /// Minutes later than now's (the clock was set back) count as now's;
    /// a minute goes once its last moment is [`WINDOW_MS`] old.
    fn trim(&mut self, now_ms: i64) {
        let now = bucket(now_ms);
        let future: Vec<i64> = self.done.range(now.saturating_add(1)..).map(|(at, _)| *at).collect();
        for at in future {
            if let Some(r) = self.done.remove(&at) {
                let e = self.done.entry(now).or_default();
                *e = capped(e.plus(r));
            }
        }
        self.done.retain(|at, _| now_ms < at.saturating_add(BUCKET_MS).saturating_add(WINDOW_MS));
    }
}

/// One minute exactly as [`SafetyWindow::save`] writes it.
fn parse(entry: &str) -> Option<(i64, Recent)> {
    let f: Vec<&str> = entry.split(',').collect();
    let [at, local, remote, suspicious] = f.as_slice() else { return None };
    let n = |s: &str| s.parse::<usize>().ok().filter(|n| *n <= UNREADABLE);
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
        assert_eq!(w.recent(15 * MIN), r(195, 2, 1), "never early");
        assert_eq!(w.recent(16 * MIN), r(5, 2, 1), "the first minute is over 15 minutes old");
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
        // Still only for 15 minutes (counted by the minute: up to 16).
        let mut w2 = SafetyWindow::load(&store, t0 + 16 * MIN + 1).unwrap().0;
        assert_eq!(w2.recent(t0 + 16 * MIN + 1), r(0, 40, 12));
        drop(store);
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn the_window_is_bounded_and_keeps_its_counts() {
        let mut w = SafetyWindow::default();
        for i in 0..1000 {
            w.add(r(1, 0, 0), i);
        }
        assert_eq!(w.done.len(), 1, "one minute");
        assert_eq!(w.recent(1000), r(1000, 0, 0));
    }

    /// Steady activity for longer than the window: what it counts is the
    /// last 15 to 16 minutes, never more, however long it goes on.
    #[test]
    fn steady_activity_is_counted_for_15_minutes_only() {
        let mut w = SafetyWindow::default();
        let end = 30 * MIN;
        for t in (0..end).step_by(10_000) {
            w.add(r(1, 0, 0), t);
        }
        let now = end - 10_000;
        let n = w.recent(now).local;
        assert!((90..=96).contains(&n), "{n}");
        assert!(w.done.len() <= 17);
    }

    /// The clock set back while the agent runs: what was stamped later is
    /// counted as now's, and goes 15 minutes from now, not from then.
    #[test]
    fn a_clock_set_back_while_running_doesnt_stretch_the_window() {
        let mut w = SafetyWindow::default();
        let (ten, nine) = (10 * 60 * MIN, 9 * 60 * MIN);
        w.add(r(100, 0, 0), ten);
        w.add(r(1, 0, 0), nine);
        assert_eq!(w.recent(nine), r(101, 0, 0));
        assert_eq!(w.recent(nine + 16 * MIN), Recent::default());
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
        for bad in [
            "garbage;5,1,2,3",
            "5,1,2,3;7,x,1,1",
            "5,1,2",
            "5,1,2,3,4",
            "5,-1,2,3",
            " 5,1,2,3",
            "5,18446744073709551615,0,0",
            "5,1000000001,0,0",
        ] {
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
            assert_eq!(again.recent(16 * MIN), Recent::default(), "for 15 minutes");
            // More on top of a full window stays readable.
            again.add(full, 10 + 2 * MIN);
            again.save(&mut store).unwrap();
            assert!(SafetyWindow::load(&store, 10 + 2 * MIN).unwrap().1);
        }
        // Saved by this version, read back the same.
        let mut w = SafetyWindow::default();
        w.add(r(3, 2, 1), 7);
        w.save(&mut store).unwrap();
        assert_eq!(SafetyWindow::load(&store, 8).unwrap(), (w, true));
    }

    /// A removal of unknown size counts as full for the window: raising the
    /// limits meanwhile doesn't let the next deletion through until the
    /// window has let it go.
    #[test]
    fn a_removal_of_unknown_size_fills_the_window_whatever_the_limits() {
        let prune = Op::PruneRemote {
            remote: oneup_sync::RemotePre { id: oneup_sync::RemoteId("nm".into()), etag: "e".into() },
            forget: Vec::new(),
            items: 10,
            exact: false,
            keep: Vec::new(),
        };
        let mut done = Recent::default();
        done.add_op(&prune, &[]);
        let mut store = Store::open_in_memory().unwrap();
        let mut w = SafetyWindow::default();
        w.add(done, 0);
        w.save(&mut store).unwrap();
        let (mut w, readable) = SafetyWindow::load(&store, 5 * MIN).unwrap();
        assert!(readable);
        // The user raises the limits right after.
        let raised = oneup_gate::Policy { max_count: 1_000_000, max_percent: 100, ..oneup_gate::Policy::default() };
        let deletion = || {
            vec![Op::DeleteRemote {
                sid: 1,
                remote: oneup_sync::RemotePre { id: oneup_sync::RemoteId("r1".into()), etag: "e".into() },
            }]
        };
        let stops = |w: &mut SafetyWindow, now| {
            let cx = oneup_gate::Context { synced_items: 1_000, recent: w.recent(now), ..Default::default() };
            oneup_gate::review_in(&raised, &cx, deletion()).breakers.len()
        };
        assert_eq!(stops(&mut w, 5 * MIN), 1, "one deletion within the window still stops");
        assert_eq!(stops(&mut w, 16 * MIN), 0, "once the window let it go, it doesn't");
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
