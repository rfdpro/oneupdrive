//! What background transfers may still move this month on the network's
//! data budget, enforced where every byte passes (`docs/architecture/network.md`
//! § Data budget). Counted as the bytes actually go: every body sent (each
//! attempt of it), every chunk received, before anything else is done with
//! it. A download that keeps dropping near its end still uses up the
//! budget, and stops once it has. The agent sets what's left from its own
//! count at each look; between looks the transport takes from it.
//!
//! **Overshoot.** A body is sent only if it fits. A chunk is counted once
//! it arrived, so a download can go past the budget by what one read
//! brings. Downloads side by side would each do so; near the limit
//! ([`NEAR`]) budgeted downloads therefore read one at a time
//! ([`Budget::near`]), and a read is only started while something is
//! left. With at most [`READS`] downloads reading at once of at most
//! [`READ_MAX`] each before that, the budget is passed by one read at most.

use std::sync::Mutex;

/// Takes more of the budget from where it's shared (the account's other
/// agents use it too): asked for `want` bytes, returns what it got.
pub type Refill = Box<dyn Fn(u64) -> u64 + Send + Sync>;

/// How much is taken at a time from a shared budget: more than [`NEAR`],
/// so downloads read one at a time only when the shared budget itself is
/// nearly used up.
pub const RESERVE: u64 = 2 * NEAR;

/// The most one read of a download brings (hyper's largest read buffer is
/// a little under 400 KiB).
pub const READ_MAX: u64 = 512 * 1024;
/// The most downloads reading at once (the request limiter's ceiling).
pub const READS: u64 = 32;
/// Below this much left, budgeted downloads read one at a time.
pub const NEAR: u64 = READS * READ_MAX;

#[derive(Default)]
pub struct Budget {
    /// Bytes left; `None`: no budget.
    left: Mutex<Option<u64>>,
    /// Held while a budgeted download reads, near the limit.
    near: tokio::sync::Mutex<()>,
    /// Where more comes from when what's held runs low (`None`: nowhere,
    /// what's set is all).
    refill: Mutex<Option<Refill>>,
}

impl std::fmt::Debug for Budget {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Budget").field("left", &self.left()).finish_non_exhaustive()
    }
}

impl Budget {
    /// What's left (`None`: no budget, everything goes).
    pub fn set(&self, left: Option<u64>) {
        *self.lock() = left;
    }

    pub fn left(&self) -> Option<u64> {
        *self.lock()
    }

    /// Where more of the budget comes from (`None`: nowhere).
    pub fn set_refill(&self, f: Option<Refill>) {
        *self.refill.lock().unwrap_or_else(|p| p.into_inner()) = f;
    }

    /// Takes at least `need` more from where the budget is shared, if
    /// anywhere ([`RESERVE`] at a time).
    fn top_up(&self, need: u64) {
        let more = {
            let r = self.refill.lock().unwrap_or_else(|p| p.into_inner());
            match r.as_ref() {
                Some(f) => f(need.max(RESERVE)),
                None => return,
            }
        };
        if more > 0
            && let Some(left) = &mut *self.lock()
        {
            *left = left.saturating_add(more);
        }
    }

    /// Below `n` (and a budget is set): take more first.
    fn ensure(&self, n: u64) {
        let short = matches!(self.left(), Some(left) if left < n);
        if short {
            let left = self.left().unwrap_or(0);
            self.top_up(n - left);
        }
    }

    fn lock(&self) -> std::sync::MutexGuard<'_, Option<u64>> {
        self.left.lock().unwrap_or_else(|p| p.into_inner())
    }

    /// Before sending `n` bytes: whether they fit, taking them if so.
    pub fn send(&self, n: u64) -> bool {
        self.ensure(n);
        let mut l = self.lock();
        match &mut *l {
            Some(left) if *left < n => false,
            Some(left) => {
                *left -= n;
                true
            }
            None => true,
        }
    }

    /// `n` bytes went out or came in that are never held (metadata: a
    /// request's JSON, a listing's answer, the push channel): they use the
    /// budget all the same.
    pub fn spent(&self, n: u64) {
        if let Some(left) = &mut *self.lock() {
            *left = left.saturating_sub(n);
        }
    }

    /// `n` bytes arrived (they're used, whatever is left): whether they
    /// fit in what was left before them. A chunk that uses the budget up
    /// exactly still fits; only one past it doesn't.
    pub fn received(&self, n: u64) -> bool {
        self.ensure(n);
        let mut l = self.lock();
        match &mut *l {
            Some(left) => {
                let fit = n <= *left;
                *left = left.saturating_sub(n);
                fit
            }
            None => true,
        }
    }

    /// Before a budgeted download reads: [`crate::Error::OverBudget`] if nothing is left; near
    /// the limit, a guard the read holds, so reads go one at a time.
    pub async fn before_read(&self) -> crate::Result<Option<tokio::sync::MutexGuard<'_, ()>>> {
        self.ensure(NEAR);
        match self.left() {
            None => Ok(None),
            Some(0) => Err(crate::Error::OverBudget),
            Some(n) if n >= NEAR => Ok(None),
            Some(_) => {
                let g = self.near.lock().await;
                // What the read before used may have left nothing.
                if self.left() == Some(0) { Err(crate::Error::OverBudget) } else { Ok(Some(g)) }
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn sends_fit_or_wait_and_receipts_always_count() {
        let b = Budget::default();
        assert!(b.send(u64::MAX) && b.received(u64::MAX), "no budget");
        b.set(Some(100));
        assert!(b.send(60) && !b.send(41) && b.send(40));
        assert_eq!(b.left(), Some(0));
        b.set(Some(10));
        assert!(b.received(5) && !b.received(50));
        assert_eq!(b.left(), Some(0));
    }

    /// A chunk that uses up exactly what's left fits: the download it
    /// ends is complete, not refused.
    #[test]
    fn an_exact_fit_is_a_fit() {
        let b = Budget::default();
        b.set(Some(100));
        assert!(b.received(100));
        assert_eq!(b.left(), Some(0));
        assert!(!b.received(1), "past it doesn't");
    }

    /// A budget shared with other agents is taken a reservation at a
    /// time: what runs out is taken again from where it's shared, and
    /// once that has nothing more, sends wait.
    #[test]
    fn a_shared_budget_is_taken_a_reservation_at_a_time() {
        use std::sync::Arc;
        use std::sync::atomic::{AtomicU64, Ordering::Relaxed};
        let shared = Arc::new(AtomicU64::new(RESERVE + 10));
        let b = Budget::default();
        let s = Arc::clone(&shared);
        b.set(Some(0));
        b.set_refill(Some(Box::new(move |want| {
            let got = want.min(s.load(Relaxed));
            s.fetch_sub(got, Relaxed);
            got
        })));
        assert!(b.send(100), "taken from where it's shared");
        assert_eq!(shared.load(Relaxed), 10);
        assert_eq!(b.left(), Some(RESERVE - 100));
        assert!(b.send(RESERVE - 100) && b.send(10), "the last of it");
        assert!(!b.send(1), "nothing more anywhere");
    }

    #[tokio::test]
    async fn near_the_limit_reads_go_one_at_a_time_and_none_when_used_up() {
        let b = Budget::default();
        assert!(b.before_read().await.unwrap().is_none(), "no budget");
        b.set(Some(NEAR));
        assert!(b.before_read().await.unwrap().is_none(), "far from it");
        b.set(Some(NEAR - 1));
        let g = b.before_read().await.unwrap();
        assert!(g.is_some());
        assert!(tokio::time::timeout(std::time::Duration::from_millis(50), b.before_read()).await.is_err());
        drop(g);
        b.set(Some(0));
        assert!(b.before_read().await.is_err());
    }
}
