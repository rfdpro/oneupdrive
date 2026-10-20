//! The sync interval: "sync every N minutes" (`config.toml` `[sync]`, or the
//! network's profile, `crate::network`). Off, the agent syncs within seconds
//! of a change, as it always has.
//!
//! On, local changes still collect from the change journal as they happen
//! (reading it costs nothing on the network), but a pass of rounds runs at
//! most once per interval: a busy afternoon of edits becomes a few passes
//! instead of hundreds. OneDrive is checked with each pass, not on a timer
//! of its own, and its push channel is closed unless the user keeps it
//! (`keep_push`): then a change in OneDrive still starts a pass at once.
//! What the user asks for (sync now, an answer, resume) never waits.
//!
//! Pure: the agent tells the [`Pacer`] what woke it and when a pass began.

use std::path::Path;
use std::time::{Duration, Instant};

use anyhow::{Context, Result, bail};
use toml_edit::value;

use crate::config;

/// The longest interval: a day.
pub const MAX_MINUTES: u32 = 24 * 60;

/// `[sync]`: the default interval (a network profile may set its own).
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct Interval {
    /// 0: off (near-instant).
    pub minutes: u32,
    /// With an interval, a change in OneDrive still starts a pass at once.
    pub keep_push: bool,
}

impl Interval {
    pub fn every(&self) -> Option<Duration> {
        (self.minutes > 0).then(|| Duration::from_secs(u64::from(self.minutes) * 60))
    }
}

/// Checks an interval the user typed (minutes, 0 = off).
pub fn check_minutes(n: u64) -> Result<u32> {
    match u32::try_from(n) {
        Ok(m) if m <= MAX_MINUTES => Ok(m),
        _ => bail!("the sync interval is from 0 (off) to {MAX_MINUTES} minutes"),
    }
}

pub fn load(data_dir: &Path) -> Result<Interval> {
    let mut i = Interval::default();
    let Some(doc) = config::read_doc(data_dir)? else { return Ok(i) };
    let Some(t) = config::section(&doc, "sync")? else { return Ok(i) };
    if let Some(n) = config::number(t, "interval_minutes")? {
        i.minutes = check_minutes(n).with_context(|| format!("{}: [sync] interval_minutes", config::FILE))?;
    }
    if let Some(b) = t.get("keep_push") {
        i.keep_push =
            b.as_bool().with_context(|| format!("{}: [sync] keep_push must be true or false", config::FILE))?;
    }
    Ok(i)
}

pub fn save(data_dir: &Path, i: &Interval) -> Result<()> {
    config::save(
        data_dir,
        "sync",
        "# Sync interval in minutes (0: within seconds of a change). keep_push: with an interval,\n\
         # a change in OneDrive still syncs at once.\n",
        vec![("interval_minutes", value(i64::from(i.minutes))), ("keep_push", value(i.keep_push))],
    )
}

/// What ended the agent's wait.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Woke {
    /// The agent started, or restarted after an error.
    Start,
    /// The wait's time ran out: a pass is due, a retry, a timed check.
    Timer,
    /// The last pass didn't finish its work (too many rounds): it goes on.
    Continue,
    /// A change in the synced folder.
    Local,
    /// OneDrive's push channel reported a change (or dropped).
    Cloud,
    /// The user: sync now, an answer, resume, changed settings.
    User,
    /// Windows reported a change of network.
    Network,
    Stop,
}

/// When passes may run.
#[derive(Debug, Clone, Default)]
pub struct Pacer {
    interval: Interval,
    /// When the last pass began.
    last: Option<Instant>,
}

impl Pacer {
    pub fn set(&mut self, interval: Interval) {
        self.interval = interval;
    }

    pub fn interval(&self) -> Interval {
        self.interval
    }

    /// Whether a change of kind `w` ends the agent's wait. With an
    /// interval, a local change is only collected; a cloud change too,
    /// unless the push channel is kept.
    pub fn wakes_on(&self, w: Woke) -> bool {
        match w {
            _ if self.interval.every().is_none() => true,
            Woke::Local => false,
            Woke::Cloud => self.interval.keep_push,
            _ => true,
        }
    }

    /// Whether OneDrive's push channel should be open.
    pub fn wants_push(&self) -> bool {
        self.interval.every().is_none() || self.interval.keep_push
    }

    /// When the next pass is due; `None`: any time (no interval, or no
    /// pass yet).
    pub fn due(&self) -> Option<Instant> {
        Some(self.last? + self.interval.every()?)
    }

    /// Woken by `w` at `now`: `None` if a pass runs now, else how long
    /// until the next may.
    pub fn decide(&self, w: Woke, now: Instant) -> Option<Duration> {
        let due = self.due()?;
        if now >= due || matches!(w, Woke::Start | Woke::User | Woke::Continue) {
            return None;
        }
        if w == Woke::Cloud && self.interval.keep_push {
            return None;
        }
        Some(due - now)
    }

    /// A pass begins at `now`.
    pub fn started(&mut self, now: Instant) {
        self.last = Some(now);
    }
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used)]
    use super::*;

    const MIN: Duration = Duration::from_secs(60);

    fn every(minutes: u32, keep_push: bool) -> Pacer {
        let mut p = Pacer::default();
        p.set(Interval { minutes, keep_push });
        p
    }

    #[test]
    fn without_an_interval_everything_wakes_and_runs_at_once() {
        let mut p = every(0, false);
        let t = Instant::now();
        p.started(t);
        for w in [Woke::Local, Woke::Cloud, Woke::Timer, Woke::Network] {
            assert!(p.wakes_on(w));
            assert_eq!(p.decide(w, t + Duration::from_secs(1)), None);
        }
        assert!(p.wants_push());
        assert_eq!(p.due(), None);
    }

    #[test]
    fn with_an_interval_changes_wait_for_it_and_the_user_never_does() {
        let mut p = every(15, false);
        let t = Instant::now();
        assert_eq!(p.decide(Woke::Start, t), None, "the first pass runs at once");
        p.started(t);
        assert!(!p.wakes_on(Woke::Local) && !p.wakes_on(Woke::Cloud) && !p.wants_push());
        assert!(p.wakes_on(Woke::User) && p.wakes_on(Woke::Network));
        let at = t + 5 * MIN;
        assert_eq!(p.decide(Woke::Timer, at), Some(10 * MIN));
        assert_eq!(p.decide(Woke::Network, at), Some(10 * MIN), "a network change doesn't bring it forward");
        assert_eq!(p.decide(Woke::User, at), None);
        assert_eq!(p.decide(Woke::Continue, at), None, "a pass that didn't finish goes on");
        assert_eq!(p.decide(Woke::Timer, t + 15 * MIN), None);
        // Kept, the push channel still brings OneDrive's changes at once.
        let mut p = every(15, true);
        p.started(t);
        assert!(p.wants_push() && p.wakes_on(Woke::Cloud) && !p.wakes_on(Woke::Local));
        assert_eq!(p.decide(Woke::Cloud, at), None);
    }

    /// The exit test of the roadmap: steady edits for an hour, a change in
    /// OneDrive every few minutes, retries and checks on their own timers,
    /// with a 15-minute interval: at most 4 passes. Simulated time, driven
    /// the way the agent drives its pacer: a wake-up the pacer doesn't
    /// wake on is only collected; any other asks [`Pacer::decide`].
    #[test]
    fn an_interval_of_15_minutes_runs_at_most_4_passes_an_hour_under_steady_edits() {
        let mut p = every(15, false);
        let t0 = Instant::now();
        // (seconds from the start, what happened)
        let mut events: Vec<(u64, Woke)> = Vec::new();
        events.extend((0..3600).step_by(7).map(|s| (s, Woke::Local)));
        events.extend((30..3600).step_by(180).map(|s| (s, Woke::Cloud)));
        events.extend((45..3600).step_by(60).map(|s| (s, Woke::Timer)));
        events.extend((100..3600).step_by(600).map(|s| (s, Woke::Network)));
        events.sort_by_key(|e| e.0);
        let mut passes = Vec::new();
        for (s, w) in std::iter::once((0, Woke::Start)).chain(events) {
            let now = t0 + Duration::from_secs(s);
            // With an interval, the agent's wait ends when the next pass is
            // due, if nothing that may start one came first.
            while let Some(d) = p.due()
                && d <= now
            {
                p.started(d);
                passes.push(d);
            }
            if !p.wakes_on(w) {
                continue;
            }
            if p.decide(w, now).is_none() {
                p.started(now);
                passes.push(now);
            }
        }
        let in_hour = passes.iter().filter(|t| t.duration_since(t0) < 60 * MIN).count();
        assert!(in_hour <= 4, "{in_hour} passes");
        assert!(in_hour >= 4, "one per interval: {in_hour}");
    }

    #[test]
    fn the_interval_is_read_written_and_checked() {
        let d = std::env::temp_dir().join(format!("oneup-pace-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&d);
        std::fs::create_dir_all(&d).unwrap();
        assert_eq!(load(&d).unwrap(), Interval::default());
        let i = Interval { minutes: 15, keep_push: true };
        save(&d, &i).unwrap();
        assert_eq!(load(&d).unwrap(), i);
        assert_eq!(i.every(), Some(15 * MIN));
        std::fs::write(config::path(&d), "[sync]\ninterval_minutes = 99999\n").unwrap();
        assert!(load(&d).is_err());
        assert!(check_minutes(1440).is_ok() && check_minutes(1441).is_err());
        let _ = std::fs::remove_dir_all(&d);
    }
}
