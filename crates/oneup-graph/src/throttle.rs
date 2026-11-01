//! Bandwidth caps (`rules.md` § User controls): transfers wait so that, over time, no
//! more than the set number of bytes per second goes each way. The cap can
//! change at any moment (the agent re-reads its settings every round).
//! Waiting is an async sleep, so a capped transfer never holds a thread.

use std::sync::Mutex;
use std::sync::atomic::{AtomicU64, Ordering::Relaxed};
use std::time::{Duration, Instant};

/// One direction's cap.
#[derive(Debug)]
pub struct Throttle {
    /// Bytes per second; 0 = no cap.
    rate: AtomicU64,
    /// When the bytes already allowed will have gone out at the cap.
    next: Mutex<Option<Instant>>,
    /// Every byte that went this way, capped or not (for progress).
    moved: AtomicU64,
    /// Pinged when the cap changes: a transfer waiting its slot books it
    /// again at the new cap (a switch to a capped network applies to it).
    changed: tokio::sync::Notify,
}

impl Default for Throttle {
    fn default() -> Self {
        Self {
            rate: AtomicU64::new(0),
            next: Mutex::new(None),
            moved: AtomicU64::new(0),
            changed: tokio::sync::Notify::new(),
        }
    }
}

impl Throttle {
    /// Sets the cap in bytes per second (0 = none).
    pub fn set(&self, bytes_per_s: u64) {
        if self.rate.swap(bytes_per_s, Relaxed) != bytes_per_s {
            *self.next.lock().unwrap_or_else(|p| p.into_inner()) = None;
            self.changed.notify_waiters();
        }
    }

    pub fn get(&self) -> u64 {
        self.rate.load(Relaxed)
    }

    /// Bytes that went this way so far (each chunk counts as it's sent or
    /// received, so a large file moves this while it's on its way).
    pub fn moved(&self) -> u64 {
        self.moved.load(Relaxed)
    }

    /// How long to wait before `n` more bytes may go, booking them: until
    /// the end of their time slot at the cap, so even a single request (a
    /// small file goes in one) keeps to it.
    pub fn book(&self, n: u64, now: Instant) -> Duration {
        self.moved.fetch_add(n, Relaxed);
        self.slot(n, now)
    }

    fn slot(&self, n: u64, now: Instant) -> Duration {
        let rate = self.rate.load(Relaxed);
        if rate == 0 || n == 0 {
            return Duration::ZERO;
        }
        let mut next = self.next.lock().unwrap_or_else(|p| p.into_inner());
        let end = next.map_or(now, |t| t.max(now)) + Duration::from_secs_f64(n as f64 / rate as f64);
        *next = Some(end);
        end.saturating_duration_since(now)
    }

    /// Waits until `n` more bytes may go; returns how long it waited. A
    /// change of cap meanwhile books them again at the new one.
    pub async fn take(&self, n: u64) -> Duration {
        self.moved.fetch_add(n, Relaxed);
        let started = tokio::time::Instant::now();
        loop {
            let changed = self.changed.notified();
            tokio::pin!(changed);
            changed.as_mut().enable();
            let wait = self.slot(n, Instant::now());
            if wait.is_zero() {
                return started.elapsed();
            }
            tokio::select! {
                () = tokio::time::sleep(wait) => return started.elapsed(),
                () = &mut changed => {}
            }
        }
    }
}

/// How long any transfer was under way: the union of their times, so
/// transfers side by side count once, and the time spent planning or
/// waiting between them not at all (for judging how fast the link moves
/// contents).
#[derive(Debug, Default)]
pub struct Active {
    state: Mutex<(usize, Option<Instant>, Duration)>,
}

/// Held while a transfer is under way ([`Active::begin`]).
pub struct Transferring<'a>(&'a Active);

impl Drop for Transferring<'_> {
    fn drop(&mut self) {
        let mut s = self.0.state.lock().unwrap_or_else(|p| p.into_inner());
        s.0 -= 1;
        if s.0 == 0
            && let Some(t) = s.1.take()
        {
            s.2 += t.elapsed();
        }
    }
}

impl Active {
    pub fn begin(&self) -> Transferring<'_> {
        let mut s = self.state.lock().unwrap_or_else(|p| p.into_inner());
        if s.0 == 0 {
            s.1 = Some(Instant::now());
        }
        s.0 += 1;
        Transferring(self)
    }

    /// The time some transfer was under way so far.
    pub fn total(&self) -> Duration {
        let s = self.state.lock().unwrap_or_else(|p| p.into_inner());
        s.2 + s.1.map_or(Duration::ZERO, |t| t.elapsed())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A transfer waiting its slot when the cap changes waits by the new
    /// cap: lowered, longer; lifted, it goes at once.
    #[tokio::test(start_paused = true)]
    async fn a_waiting_transfer_follows_a_change_of_cap() {
        let t = std::sync::Arc::new(Throttle::default());
        t.set(1000);
        let t2 = std::sync::Arc::clone(&t);
        let waiting = tokio::spawn(async move { t2.take(1000).await });
        tokio::time::sleep(Duration::from_millis(100)).await;
        t.set(100);
        let waited = waiting.await.unwrap();
        assert!(waited >= Duration::from_secs(10), "lowered: {waited:?}");
        let t2 = std::sync::Arc::clone(&t);
        let waiting = tokio::spawn(async move { t2.take(1000).await });
        tokio::time::sleep(Duration::from_millis(100)).await;
        t.set(0);
        assert!(waiting.await.unwrap() < Duration::from_secs(1), "lifted");
    }

    #[test]
    fn transfers_side_by_side_count_once_and_idle_time_not_at_all() {
        let a = Active::default();
        let one = a.begin();
        std::thread::sleep(Duration::from_millis(30));
        let two = a.begin();
        std::thread::sleep(Duration::from_millis(30));
        drop(one);
        drop(two);
        let busy = a.total();
        std::thread::sleep(Duration::from_millis(50));
        assert_eq!(a.total(), busy, "idle time isn't counted");
        assert!(busy >= Duration::from_millis(60) && busy < Duration::from_millis(110), "{busy:?}");
    }

    #[test]
    fn bytes_are_spread_at_the_cap() {
        let t = Throttle::default();
        let now = Instant::now();
        assert_eq!(t.book(1_000_000, now), Duration::ZERO, "no cap");
        t.set(1000);
        assert_eq!(t.book(500, now), Duration::from_millis(500), "even the first waits its slot");
        assert_eq!(t.book(500, now), Duration::from_secs(1));
        assert_eq!(t.book(1000, now), Duration::from_secs(2));
        // Idle time isn't saved up for a burst.
        let later = now + Duration::from_secs(10);
        assert_eq!(t.book(1000, later), Duration::from_secs(1));
        assert_eq!(t.book(1000, later), Duration::from_secs(2));
        t.set(0);
        assert_eq!(t.book(1_000_000, later), Duration::ZERO, "cap removed");
        assert_eq!(t.moved(), 1_000_000 + 500 + 500 + 1000 + 1000 + 1000 + 1_000_000);
    }
}
