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
}

impl Default for Throttle {
    fn default() -> Self {
        Self { rate: AtomicU64::new(0), next: Mutex::new(None), moved: AtomicU64::new(0) }
    }
}

impl Throttle {
    /// Sets the cap in bytes per second (0 = none).
    pub fn set(&self, bytes_per_s: u64) {
        if self.rate.swap(bytes_per_s, Relaxed) != bytes_per_s {
            *self.next.lock().unwrap_or_else(|p| p.into_inner()) = None;
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
        let rate = self.rate.load(Relaxed);
        if rate == 0 || n == 0 {
            return Duration::ZERO;
        }
        let mut next = self.next.lock().unwrap_or_else(|p| p.into_inner());
        let end = next.map_or(now, |t| t.max(now)) + Duration::from_secs_f64(n as f64 / rate as f64);
        *next = Some(end);
        end.saturating_duration_since(now)
    }

    /// Waits until `n` more bytes may go.
    pub async fn take(&self, n: u64) {
        let wait = self.book(n, Instant::now());
        if !wait.is_zero() {
            tokio::time::sleep(wait).await;
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

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
