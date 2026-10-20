//! Concurrency limiter shaped by OneDrive's throttling behaviour.
//!
//! Every Graph request holds a [`Permit`]. Three mechanisms keep us under
//! the service's limits instead of bouncing off them:
//!
//! * **AIMD**: the limit grows by about one per "round" of successful
//!   requests and halves on throttling (at most once per cool-down window,
//!   so one burst of 429s counts as one signal).
//! * **Global pause**: a 429/503 with `Retry-After` pauses *every* request
//!   until it expires. Throttled requests still count against the quota, so
//!   requests sent during a throttle only prolong it. (Early real-drive
//!   runs spent most of their time stalled because only the
//!   throttled request waited.)
//! * **Ceiling memory**: after throttling at limit L, growth is capped below L
//!   and the cap rises by one per minute without throttling. This stops the
//!   limiter from repeatedly climbing back into the same wall.
//!
//! It also reacts to the (best-effort) `RateLimit-*` headers OneDrive sends
//! when usage passes ~80% of a window, backing off *before* being throttled.

use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use tokio::sync::Notify;

#[derive(Debug)]
struct State {
    limit: f64,
    in_flight: usize,
    /// No request may start before this instant.
    paused_until: Option<Instant>,
    /// Growth cap learned from the last throttle, and when it was set.
    ceiling: Option<(f64, Instant)>,
    last_decrease: Option<Instant>,
    throttles: u64,
    paused_total: Duration,
    /// A cap the caller set (a poor link), below `max`.
    cap: Option<f64>,
}

#[derive(Debug)]
pub struct Aimd {
    state: Mutex<State>,
    notify: Notify,
    min: f64,
    max: f64,
    cooldown: Duration,
    /// How fast the learned ceiling relaxes.
    ceiling_relax: Duration,
}

/// Held while a request is in flight; releases its slot on drop.
#[derive(Debug)]
pub struct Permit {
    owner: Arc<Aimd>,
}

impl Drop for Permit {
    fn drop(&mut self) {
        self.owner.lock().in_flight -= 1;
        self.owner.notify.notify_one();
    }
}

/// Snapshot for progress reporting.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct LimiterStats {
    pub limit: usize,
    pub in_flight: usize,
    pub throttles: u64,
    /// Total time all requests were globally paused by `Retry-After`.
    pub paused_total: Duration,
    pub paused_now: bool,
}

impl Aimd {
    pub fn new(initial: usize, max: usize) -> Arc<Self> {
        Self::with_timing(initial, max, Duration::from_secs(2), Duration::from_secs(60))
    }

    fn with_timing(initial: usize, max: usize, cooldown: Duration, ceiling_relax: Duration) -> Arc<Self> {
        let max = max.max(1) as f64;
        Arc::new(Self {
            state: Mutex::new(State {
                limit: (initial.max(1) as f64).min(max),
                in_flight: 0,
                paused_until: None,
                ceiling: None,
                last_decrease: None,
                throttles: 0,
                paused_total: Duration::ZERO,
                cap: None,
            }),
            notify: Notify::new(),
            min: 1.0,
            max,
            cooldown,
            ceiling_relax,
        })
    }

    fn lock(&self) -> std::sync::MutexGuard<'_, State> {
        // A poisoned lock only means another thread panicked mid-update of
        // plain counters; the values are still usable.
        self.state.lock().unwrap_or_else(|e| e.into_inner())
    }

    pub async fn acquire(self: &Arc<Self>) -> Permit {
        loop {
            let notified = self.notify.notified();
            tokio::pin!(notified);
            notified.as_mut().enable();
            let wait = {
                let mut s = self.lock();
                match s.paused_until {
                    Some(t) if t > Instant::now() => Some(t),
                    _ => {
                        s.paused_until = None;
                        if (s.in_flight as f64) < s.limit.floor() {
                            s.in_flight += 1;
                            return Permit { owner: Arc::clone(self) };
                        }
                        None
                    }
                }
            };
            match wait {
                // Paused: sleep until the pause ends (or the state changes).
                Some(t) => {
                    tokio::select! {
                        _ = tokio::time::sleep_until(t.into()) => {}
                        _ = &mut notified => {}
                    }
                }
                None => notified.await,
            }
        }
    }

    fn effective_ceiling(&self, s: &State, now: Instant) -> f64 {
        let most = s.cap.unwrap_or(self.max).min(self.max);
        match s.ceiling {
            None => most,
            Some((c, since)) => {
                let relaxed = now.saturating_duration_since(since).as_secs_f64() / self.ceiling_relax.as_secs_f64();
                (c + relaxed.floor()).min(most)
            }
        }
    }

    /// The cap the caller set ([`Aimd::set_cap`]), if any.
    pub fn cap(&self) -> Option<usize> {
        self.lock().cap.map(|c| c as usize)
    }

    /// At most `n` requests at once from now on (a poor link: fewer
    /// requests competing for it, each more likely to get through);
    /// `None`: the limiter's own maximum again. Requests already under way
    /// finish.
    pub fn set_cap(&self, n: Option<usize>) {
        let mut s = self.lock();
        s.cap = n.map(|n| n.max(1) as f64);
        if let Some(c) = s.cap {
            s.limit = s.limit.min(c).max(self.min);
        }
    }

    /// A request succeeded: additive increase (≈ +1 per `limit` successes),
    /// up to the learned ceiling.
    pub fn on_success(&self) {
        let grew = {
            let mut s = self.lock();
            let before = s.limit.floor();
            let cap = self.effective_ceiling(&s, Instant::now());
            if s.limit < cap {
                s.limit = (s.limit + 1.0 / s.limit).min(cap);
            }
            s.limit.floor() > before
        };
        if grew {
            self.notify.notify_one();
        }
    }

    fn decrease(&self, s: &mut State, now: Instant) {
        if s.last_decrease.is_none_or(|t| now.duration_since(t) >= self.cooldown) {
            let at = s.limit.floor();
            s.ceiling = Some(((at - 1.0).max(self.min), now));
            s.limit = (s.limit / 2.0).max(self.min);
            s.last_decrease = Some(now);
        }
    }

    /// The service throttled us. Every request pauses for `retry_after`.
    pub fn on_throttle(&self, retry_after: Duration) {
        let now = Instant::now();
        let mut s = self.lock();
        s.throttles += 1;
        self.decrease(&mut s, now);
        let until = now + retry_after;
        match s.paused_until {
            Some(t) if t >= until => {}
            Some(t) if t > now => {
                s.paused_total += until - t;
                s.paused_until = Some(until);
            }
            _ => {
                s.paused_total += retry_after;
                s.paused_until = Some(until);
            }
        }
    }

    /// Early warning from `RateLimit-*` headers: `remaining` of `limit` units
    /// are left in a window that resets in `reset`.
    pub fn on_pressure(&self, remaining: u64, limit: u64, reset: Duration) {
        let now = Instant::now();
        let mut s = self.lock();
        if remaining == 0 {
            let until = now + reset;
            if s.paused_until.is_none_or(|t| t < until) {
                s.paused_total += reset;
                s.paused_until = Some(until);
            }
            self.decrease(&mut s, now);
        } else if remaining.saturating_mul(10) < limit {
            // Under 10% left: back off before the service has to throttle us.
            self.decrease(&mut s, now);
        }
    }

    pub fn limit(&self) -> usize {
        self.lock().limit.floor() as usize
    }

    pub fn in_flight(&self) -> usize {
        self.lock().in_flight
    }

    pub fn throttles(&self) -> u64 {
        self.lock().throttles
    }

    pub fn stats(&self) -> LimiterStats {
        let s = self.lock();
        LimiterStats {
            limit: s.limit.floor() as usize,
            in_flight: s.in_flight,
            throttles: s.throttles,
            paused_total: s.paused_total,
            paused_now: s.paused_until.is_some_and(|t| t > Instant::now()),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn respects_limit_and_releases() {
        let l = Aimd::new(2, 8);
        let a = l.acquire().await;
        let _b = l.acquire().await;
        assert_eq!(l.in_flight(), 2);
        let third = tokio::time::timeout(Duration::from_millis(50), l.acquire()).await;
        assert!(third.is_err(), "third permit must wait");
        drop(a);
        let _c = tokio::time::timeout(Duration::from_millis(500), l.acquire()).await.expect("released slot is reused");
        assert_eq!(l.in_flight(), 2);
    }

    #[test]
    fn a_cap_bounds_the_limit_until_lifted() {
        let l = Aimd::new(6, 16);
        l.set_cap(Some(2));
        assert_eq!(l.limit(), 2);
        for _ in 0..100 {
            l.on_success();
        }
        assert_eq!(l.limit(), 2, "it doesn't grow past the cap");
        l.set_cap(None);
        for _ in 0..100 {
            l.on_success();
        }
        assert!(l.limit() > 2);
    }

    #[test]
    fn additive_increase_multiplicative_decrease() {
        let l = Aimd::new(4, 16);
        for _ in 0..40 {
            l.on_success();
        }
        assert!(l.limit() > 4 && l.limit() <= 16, "grew to {}", l.limit());
        let before = l.limit();
        l.on_throttle(Duration::ZERO);
        l.on_throttle(Duration::ZERO); // same burst: only one decrease
        assert_eq!(l.limit(), (before / 2).max(1));
        assert_eq!(l.throttles(), 2);
    }

    #[test]
    fn never_exceeds_bounds() {
        let l = Aimd::new(100, 3);
        assert_eq!(l.limit(), 3);
        for _ in 0..1000 {
            l.on_success();
        }
        assert_eq!(l.limit(), 3);
        let l = Aimd::new(1, 3);
        l.on_throttle(Duration::ZERO);
        assert_eq!(l.limit(), 1);
    }

    #[tokio::test]
    async fn throttle_pauses_everyone_until_retry_after() {
        let l = Aimd::new(4, 8);
        l.on_throttle(Duration::from_millis(300));
        let t = Instant::now();
        let _p = l.acquire().await;
        assert!(t.elapsed() >= Duration::from_millis(250), "acquire returned during the pause");
        let st = l.stats();
        assert!(st.paused_total >= Duration::from_millis(300) && !st.paused_now);
    }

    #[test]
    fn learned_ceiling_caps_regrowth_then_relaxes() {
        let l = Aimd::with_timing(8, 16, Duration::ZERO, Duration::from_millis(100));
        l.on_throttle(Duration::ZERO); // throttled at 8: limit 4, ceiling 7
        for _ in 0..1000 {
            l.on_success();
        }
        assert_eq!(l.limit(), 7, "regrowth stops below the throttle point");
        std::thread::sleep(Duration::from_millis(350));
        for _ in 0..1000 {
            l.on_success();
        }
        assert!(l.limit() >= 9, "ceiling relaxes over time, got {}", l.limit());
    }

    #[test]
    fn rate_limit_headers_back_off_early() {
        let l = Aimd::new(8, 16);
        l.on_pressure(50, 100, Duration::from_secs(30));
        assert_eq!(l.limit(), 8, "plenty left: no change");
        l.on_pressure(5, 100, Duration::from_secs(30));
        assert_eq!(l.limit(), 4);
        let l = Aimd::new(8, 16);
        l.on_pressure(0, 100, Duration::from_millis(200));
        assert!(l.stats().paused_now);
    }
}
