//! Files that keep changing (`[agent] hold_busy_files`, on by default):
//! their uploads wait until they've been left alone for a while.
//!
//! A file written again and again (a database, an app's settings saved
//! every few seconds) would otherwise be sent after every write. An upload
//! started while the program is still writing is skipped when the file
//! changes under it, and each skip pushes the next try further out. Once a
//! file has changed [`Settings::changes`] times within
//! [`Settings::window`], its uploads (and the hashing before them) wait
//! until it has gone [`Settings::quiet`] without a change. A file that never
//! stops is still sent every [`Settings::max`], so it can't go unsynced.
//!
//! The agent's change-journal watcher feeds it every file written on the
//! volume ([`Busy::changed`]); rounds ask it which uploads to hold
//! ([`Busy::held_until`]). In memory only, at most [`MAX_FILES`] files.

use std::collections::HashMap;
use std::sync::Mutex;
use std::time::{Duration, Instant, SystemTime};

/// Files remembered at most. The watcher sees writes anywhere on the
/// volume, most of them outside the synced folder; past this, the files
/// written longest ago are forgotten first.
pub const MAX_FILES: usize = 4096;

/// Journal records closer together than this are one change: a single save
/// leaves several (data written, size changed, file closed).
const ONE_SAVE: Duration = Duration::from_secs(1);

/// The most changes a setting may ask for (each file keeps that many times).
pub const MAX_CHANGES: u32 = 100;

/// The longest any of the durations may be.
pub const MAX_DURATION: Duration = Duration::from_secs(24 * 3600);

/// When a journal record's FILETIME `filetime` happened, as an `Instant`
/// (given `now` both ways): a backlog read at once keeps its records'
/// spacing. A time in the future (a clock change) counts as now.
pub fn instant_of(filetime: i64, now: Instant, now_wall: SystemTime) -> Instant {
    const UNIX_AT: i64 = 116_444_736_000_000_000;
    let unix_100ns = filetime.saturating_sub(UNIX_AT).max(0);
    let at = SystemTime::UNIX_EPOCH + Duration::from_nanos(u64::try_from(unix_100ns).unwrap_or(0).saturating_mul(100));
    now_wall.duration_since(at).ok().and_then(|ago| now.checked_sub(ago)).unwrap_or(now)
}

/// When a file counts as busy, and for how long its uploads wait.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Settings {
    pub on: bool,
    /// Changes within `window` that make a file busy (at least 2).
    pub changes: u32,
    pub window: Duration,
    /// How long a busy file must go unchanged before it's sent.
    pub quiet: Duration,
    /// The longest an upload waits, however often the file changes.
    pub max: Duration,
}

impl Default for Settings {
    fn default() -> Self {
        Self {
            on: true,
            changes: 2,
            window: Duration::from_secs(5),
            quiet: Duration::from_secs(15),
            max: Duration::from_secs(600),
        }
    }
}

#[derive(Debug, Clone)]
struct File {
    /// Recent changes, oldest first, at most `changes` of them.
    recent: Vec<Instant>,
    last: Instant,
    /// When it became busy, while it is.
    since: Option<Instant>,
}

/// Which files are busy. Shared between the journal watcher's thread and
/// the rounds.
#[derive(Debug, Default)]
pub struct Busy {
    inner: Mutex<Inner>,
}

#[derive(Debug, Default)]
struct Inner {
    settings: Settings,
    files: HashMap<u128, File>,
}

impl Busy {
    pub fn new(settings: Settings) -> Self {
        Self { inner: Mutex::new(Inner { settings, files: HashMap::new() }) }
    }

    /// The settings changed (`config.toml` is read again at every wake-up):
    /// every file is judged again by the new ones, from the changes it had.
    pub fn set_settings(&self, s: Settings) {
        let mut g = self.lock();
        if g.settings == s {
            return;
        }
        g.settings = s;
        let keep = s.changes.clamp(2, MAX_CHANGES) as usize;
        for f in g.files.values_mut() {
            let last = f.last;
            f.recent.retain(|t| last.saturating_duration_since(*t) <= s.window);
            let drop = f.recent.len().saturating_sub(keep);
            f.recent.drain(..drop);
            if f.recent.len() < keep {
                f.since = None;
            } else if f.since.is_none() {
                f.since = Some(last);
            }
        }
    }

    /// The file with ID `id` was written at `now`.
    pub fn changed(&self, id: u128, now: Instant) {
        let mut g = self.lock();
        let s = g.settings;
        if !s.on {
            return;
        }
        if g.files.len() >= MAX_FILES && !g.files.contains_key(&id) {
            g.forget_idle(now);
            // The one written longest ago, a busy one only if all are.
            if g.files.len() >= MAX_FILES
                && let Some(oldest) = g.files.iter().min_by_key(|(_, f)| (f.since.is_some(), f.last)).map(|(k, _)| *k)
            {
                g.files.remove(&oldest);
            }
        }
        let f = g.files.entry(id).or_insert(File { recent: Vec::new(), last: now, since: None });
        if f.recent.last().is_some_and(|t| now.saturating_duration_since(*t) < ONE_SAVE) {
            f.last = now;
            return;
        }
        f.last = now;
        f.recent.retain(|t| now.saturating_duration_since(*t) <= s.window);
        f.recent.push(now);
        let keep = s.changes.clamp(2, MAX_CHANGES) as usize;
        if f.recent.len() > keep {
            f.recent.remove(0);
        }
        if f.since.is_none() && f.recent.len() >= keep {
            f.since = Some(now);
        }
    }

    /// Until when an upload of `id` waits, if it's busy at `now`: when it
    /// will have been quiet for long enough, or the longest wait is over,
    /// whichever comes first. `None`: send it now.
    pub fn held_until(&self, id: u128, now: Instant) -> Option<Instant> {
        let mut g = self.lock();
        let s = g.settings;
        if !s.on {
            return None;
        }
        let f = g.files.get_mut(&id)?;
        let since = f.since?;
        let until = f.last.checked_add(s.quiet)?.min(since.checked_add(s.max)?);
        if until <= now {
            // Sent now. Its next change starts a new wait (its recent
            // changes still count), so a file that never stops is sent
            // about every `max`.
            f.since = None;
            return None;
        }
        Some(until)
    }

    fn lock(&self) -> std::sync::MutexGuard<'_, Inner> {
        self.inner.lock().unwrap_or_else(|p| p.into_inner())
    }
}

impl Inner {
    /// Forgets files that aren't busy and haven't changed within the
    /// window: nothing about them affects an upload any more.
    fn forget_idle(&mut self, now: Instant) {
        let window = self.settings.window;
        self.files.retain(|_, f| f.since.is_some() || now.saturating_duration_since(f.last) <= window);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn at(base: Instant, ms: u64) -> Instant {
        base + Duration::from_millis(ms)
    }

    #[test]
    fn two_changes_in_the_window_hold_the_upload_until_it_is_quiet() {
        let b = Busy::new(Settings::default());
        let t = Instant::now();
        b.changed(1, t);
        assert_eq!(b.held_until(1, at(t, 100)), None, "one change: sent as usual");
        b.changed(1, at(t, 3_000));
        assert_eq!(b.held_until(1, at(t, 3_100)), Some(at(t, 18_000)), "quiet 15 s after the last change");
        b.changed(1, at(t, 10_000));
        assert_eq!(b.held_until(1, at(t, 10_100)), Some(at(t, 25_000)), "each change restarts the wait");
        assert_eq!(b.held_until(1, at(t, 25_000)), None, "quiet long enough");
    }

    #[test]
    fn records_of_one_save_count_once() {
        let b = Busy::new(Settings::default());
        let t = Instant::now();
        for ms in [0, 5, 40, 300] {
            b.changed(1, at(t, ms));
        }
        assert_eq!(b.held_until(1, at(t, 400)), None);
    }

    #[test]
    fn changes_further_apart_than_the_window_never_hold() {
        let b = Busy::new(Settings::default());
        let t = Instant::now();
        for s in [0, 6, 12, 18] {
            b.changed(1, at(t, s * 1_000));
            assert_eq!(b.held_until(1, at(t, s * 1_000 + 100)), None);
        }
    }

    #[test]
    fn a_file_that_never_stops_is_still_sent_every_max() {
        let s = Settings { max: Duration::from_secs(60), ..Settings::default() };
        let b = Busy::new(s);
        let t = Instant::now();
        let mut sent = Vec::new();
        for sec in 0..150u64 {
            let now = at(t, sec * 1_000);
            b.changed(1, now);
            if b.held_until(1, now).is_none() {
                sent.push(sec);
            }
        }
        // Sent at the first change, then every 60 s.
        assert_eq!(sent, vec![0, 61, 122], "{sent:?}");
    }

    #[test]
    fn more_changes_needed_when_set() {
        let b = Busy::new(Settings { changes: 3, ..Settings::default() });
        let t = Instant::now();
        b.changed(1, t);
        b.changed(1, at(t, 2_000));
        assert_eq!(b.held_until(1, at(t, 2_100)), None);
        b.changed(1, at(t, 4_000));
        assert!(b.held_until(1, at(t, 4_100)).is_some());
    }

    #[test]
    fn off_holds_nothing_and_other_files_are_unaffected() {
        let b = Busy::new(Settings::default());
        let t = Instant::now();
        b.changed(1, t);
        b.changed(1, at(t, 2_000));
        assert!(b.held_until(1, at(t, 2_100)).is_some());
        assert_eq!(b.held_until(2, at(t, 2_100)), None);
        b.set_settings(Settings { on: false, ..Settings::default() });
        assert_eq!(b.held_until(1, at(t, 2_200)), None);
    }

    #[test]
    fn new_settings_judge_every_file_again() {
        let b = Busy::new(Settings::default());
        let t = Instant::now();
        b.changed(1, t);
        b.changed(1, at(t, 2_000));
        assert!(b.held_until(1, at(t, 2_100)).is_some());
        b.set_settings(Settings { changes: 3, ..Settings::default() });
        assert_eq!(b.held_until(1, at(t, 2_200)), None, "two changes no longer make it busy");
        b.set_settings(Settings::default());
        assert!(b.held_until(1, at(t, 2_300)).is_some(), "and do again");
        b.set_settings(Settings { window: Duration::from_secs(1), ..Settings::default() });
        assert_eq!(b.held_until(1, at(t, 2_400)), None, "2 s apart is outside a 1 s window");
    }

    #[test]
    fn records_read_late_keep_their_own_times() {
        let (now, wall) = (Instant::now(), SystemTime::now());
        let filetime = |ago_s: u64| {
            let unix = wall.duration_since(SystemTime::UNIX_EPOCH).unwrap() - Duration::from_secs(ago_s);
            116_444_736_000_000_000 + i64::try_from(unix.as_nanos() / 100).unwrap()
        };
        let t = instant_of(filetime(30), now, wall);
        assert!(now.duration_since(t).as_secs() == 30 || now.duration_since(t).as_secs() == 29);
        assert_eq!(instant_of(filetime(0) + 10_000_000_000, now, wall), now, "the future is now");
        // Two saves 3 s apart, read in one batch 20 s later: still busy.
        let b = Busy::new(Settings::default());
        b.changed(1, instant_of(filetime(23), now, wall));
        b.changed(1, instant_of(filetime(20), now, wall));
        assert!(b.held_until(1, instant_of(filetime(19), now, wall)).is_some());
    }

    #[test]
    fn memory_stays_bounded_and_busy_files_are_kept() {
        let b = Busy::new(Settings::default());
        let t = Instant::now();
        b.changed(0, t);
        b.changed(0, at(t, 2_000));
        for id in 1..(MAX_FILES as u128 * 3) {
            b.changed(id, at(t, 2_500));
        }
        assert!(b.lock().files.len() <= MAX_FILES);
        assert!(b.held_until(0, at(t, 3_000)).is_some(), "the busy file wasn't forgotten");
    }
}
