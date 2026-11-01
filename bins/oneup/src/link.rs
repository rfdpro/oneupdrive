//! How good the link to OneDrive is, judged after each pass of rounds from
//! what its requests saw: how many had to be retried, how long answers
//! took to begin, and how fast contents moved. On a poor link the agent
//! sends fewer requests at once, smaller upload fragments (a drop then
//! costs less to send again) and waits longer before retrying
//! (`docs/architecture/network.md` § Poor links). Pure.

/// What one pass's requests saw (differences of the Graph client's
/// counters over the pass).
#[derive(Debug, Clone, Copy, Default, PartialEq)]
pub struct Sample {
    pub requests: u64,
    /// Sent again after a network or server error.
    pub retries: u64,
    /// Requests without a body, and the time until their answer began.
    pub timed: u64,
    pub timed_ms: u64,
    /// File contents moved, and over how long.
    pub bytes: u64,
    pub secs: f64,
    /// A bandwidth cap applied: slowness says nothing about the link.
    pub capped: bool,
}

/// Retried requests above this share mark a poor link.
const RETRY_SHARE: f64 = 0.05;
/// Answers beginning this late, on average, mark a poor link.
const SLOW_MS: u64 = 1500;
/// Contents moving slower than this (bytes a second, over at least
/// [`SPEED_BYTES`]), uncapped, mark a poor link: 32 KB/s, about a
/// 256 kbit/s line.
const SLOW_SPEED: f64 = 32_000.0;
const SPEED_BYTES: u64 = 1 << 20;
/// Good passes in a row before a poor link counts as good again.
const RECOVER: u32 = 2;

impl Sample {
    /// Whether this pass alone looks poor; `None`: too little to tell.
    pub fn poor(&self) -> Option<bool> {
        let mut told = false;
        let mut poor = false;
        if self.requests >= 10 {
            told = true;
            poor |= self.retries as f64 / self.requests as f64 > RETRY_SHARE;
        }
        if self.timed >= 5 {
            told = true;
            poor |= self.timed_ms / self.timed >= SLOW_MS;
        }
        if !self.capped && self.bytes >= SPEED_BYTES && self.secs > 0.0 {
            told = true;
            poor |= (self.bytes as f64 / self.secs) < SLOW_SPEED;
        }
        told.then_some(poor)
    }
}

/// The link's state across passes.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct Link {
    poor: bool,
    good_in_a_row: u32,
}

impl Link {
    /// Takes in a pass; returns whether the link counts as poor now. One
    /// poor pass is enough; it takes [`RECOVER`] good ones to undo, so it
    /// doesn't flip at every pass.
    pub fn observe(&mut self, s: &Sample) -> bool {
        match s.poor() {
            Some(true) => {
                self.poor = true;
                self.good_in_a_row = 0;
            }
            Some(false) if self.poor => {
                self.good_in_a_row += 1;
                if self.good_in_a_row >= RECOVER {
                    self.poor = false;
                    self.good_in_a_row = 0;
                }
            }
            _ => {}
        }
        self.poor
    }

    pub fn is_poor(&self) -> bool {
        self.poor
    }
}

/// On a poor link: requests at once.
pub const POOR_REQUESTS: usize = 2;
/// On a poor link: the largest upload fragment, 1.25 MiB (four of Graph's
/// 320 KiB units: about 40 s at 256 kbit/s).
pub const POOR_FRAGMENT: usize = 4 * 320 * 1024;
/// On a poor link: how much longer the agent waits before trying again.
pub const POOR_BACKOFF: u32 = 2;

#[cfg(test)]
mod tests {
    use super::*;

    fn pass(requests: u64, retries: u64, ms: u64, bytes: u64, secs: f64) -> Sample {
        Sample { requests, retries, timed: requests, timed_ms: ms * requests, bytes, secs, capped: false }
    }

    #[test]
    fn a_pass_is_poor_by_retries_slow_answers_or_slow_contents() {
        assert_eq!(pass(100, 1, 200, 50 << 20, 10.0).poor(), Some(false));
        assert_eq!(pass(100, 10, 200, 0, 0.0).poor(), Some(true), "10% retried");
        assert_eq!(pass(20, 0, 1800, 0, 0.0).poor(), Some(true), "slow answers");
        assert_eq!(pass(20, 0, 300, 4 << 20, 200.0).poor(), Some(true), "20 KB/s");
        let capped = Sample { capped: true, ..pass(20, 0, 300, 4 << 20, 200.0) };
        assert_eq!(capped.poor(), Some(false), "a cap isn't the link");
        assert_eq!(pass(2, 2, 5000, 0, 0.0).poor(), None, "too little to tell");
    }

    #[test]
    fn one_poor_pass_marks_the_link_and_two_good_ones_clear_it() {
        let mut l = Link::default();
        assert!(!l.observe(&pass(100, 0, 100, 0, 0.0)));
        assert!(l.observe(&pass(100, 20, 100, 0, 0.0)));
        assert!(l.observe(&pass(1, 0, 0, 0, 0.0)), "too little to tell changes nothing");
        assert!(l.observe(&pass(100, 0, 100, 0, 0.0)), "one good pass isn't enough");
        assert!(!l.observe(&pass(100, 0, 100, 0, 0.0)));
    }
}
