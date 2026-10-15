//! Choosing upload-session fragment sizes from what the connection shows
//! (`docs/architecture/sync-engine.md` § Cloud over Graph).
//!
//! Every request to OneDrive costs a roughly fixed overhead (measured on
//! the test account: about 0.7–1.2 s), so small fragments waste much of the
//! line: a 120 MiB file took 16 s in 10 MiB fragments and 7.6 s in the
//! largest allowed. Large fragments, on the other hand, cost more to resend
//! when one fails on a poor connection.
//!
//! [`Sizer`] models each fragment's time as `overhead + bytes / speed`,
//! fitted by least squares over the most recent fragments, and picks the
//! size that keeps the overhead to about a tenth of the time, but never one
//! the line needs more than [`MAX_SECS`] to send. It probes upwards (×2)
//! until it has three samples of clearly different sizes to fit, then moves
//! at most ×2 or ÷2 per fragment (one noisy fragment can't swing it far),
//! halves after a failed fragment, and stays within the memory profile's
//! limit. Pure, so it is unit-tested without a network.

use std::collections::VecDeque;

/// Fragments must be multiples of 320 KiB.
pub const UNIT: usize = 320 * 1024;
/// The largest fragment Graph accepts is under 60 MiB: 191 units (59.69 MiB).
pub const MAX_UNITS: usize = 191;
/// Where a new sizer starts: 10 MiB.
pub const START_UNITS: usize = 32;
/// The smallest fragment it goes down to: 1.25 MiB.
pub const MIN_UNITS: usize = 4;
/// Longest a fragment should take to send, so a failed one is cheap to resend.
pub const MAX_SECS: f64 = 30.0;
/// The fitted overhead should be at most `1 / (1 + EFFICIENCY)` of a
/// fragment's time (9: about 10%).
const EFFICIENCY: f64 = 9.0;
/// Fragments remembered for the fit.
const SAMPLES: usize = 12;

#[derive(Debug, Clone)]
pub struct Sizer {
    max_units: usize,
    /// The size the next fragment uses, in units.
    current: usize,
    /// `(bytes, seconds)` of recent fragments that went through first time.
    samples: VecDeque<(f64, f64)>,
}

/// The fitted line: `seconds = overhead + bytes * per_byte`.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct Fit {
    pub overhead: f64,
    pub per_byte: f64,
}

impl Sizer {
    /// `max_bytes`: the memory profile's largest fragment (rounded down to
    /// whole units, at most [`MAX_UNITS`]).
    pub fn new(max_bytes: usize) -> Self {
        let max_units = (max_bytes / UNIT).clamp(MIN_UNITS, MAX_UNITS);
        Self { max_units, current: START_UNITS.min(max_units), samples: VecDeque::new() }
    }

    /// The largest fragment this sizer may choose, in bytes.
    pub fn max_bytes(&self) -> usize {
        self.max_units * UNIT
    }

    /// The next fragment's size in bytes, for an upload with `remaining`
    /// bytes left: a whole number of units, or everything left when that
    /// is less.
    pub fn next(&self, remaining: u64) -> usize {
        let want = self.current * UNIT;
        usize::try_from(remaining).map_or(want, |r| r.min(want))
    }

    /// A fragment of `bytes` went through in `secs` (excluding waits the
    /// service asked for). `retried`: it needed more than one attempt.
    pub fn observe(&mut self, bytes: usize, secs: f64, retried: bool) {
        if retried {
            // A poor line: resending is costly, so smaller fragments, and
            // forget the samples of better times.
            self.current = (self.current / 2).max(MIN_UNITS);
            self.samples.clear();
            return;
        }
        // The last fragment of a file is usually a remainder; it still fits
        // the line as well as any other.
        if bytes == 0 || !secs.is_finite() || secs <= 0.0 {
            return;
        }
        if self.samples.len() == SAMPLES {
            self.samples.pop_front();
        }
        self.samples.push_back((bytes as f64, secs));
        self.current = self.choose();
    }

    /// A fragment failed outright (the upload is abandoned).
    pub fn failed(&mut self) {
        self.observe(0, 0.0, true);
    }

    /// The least-squares line through the samples; `None` with fewer than
    /// three, or without clearly different sizes.
    pub fn fit(&self) -> Option<Fit> {
        let n = self.samples.len() as f64;
        if self.samples.len() < 3 {
            return None;
        }
        let mx = self.samples.iter().map(|s| s.0).sum::<f64>() / n;
        let my = self.samples.iter().map(|s| s.1).sum::<f64>() / n;
        let sxx: f64 = self.samples.iter().map(|s| (s.0 - mx).powi(2)).sum();
        let sxy: f64 = self.samples.iter().map(|s| (s.0 - mx) * (s.1 - my)).sum();
        // Sizes within ~50% of each other can't separate overhead from speed.
        let spread = self.samples.iter().map(|s| s.0).fold(f64::MIN, f64::max)
            / self.samples.iter().map(|s| s.0).fold(f64::MAX, f64::min);
        if sxx <= 0.0 || spread < 1.5 {
            return None;
        }
        let per_byte = sxy / sxx;
        Some(Fit { overhead: my - per_byte * mx, per_byte })
    }

    fn choose(&self) -> usize {
        let Some(fit) = self.fit() else {
            // Not enough to go on: probe upwards from the largest size seen.
            let largest = self.samples.iter().map(|s| s.0).fold(0.0, f64::max) as usize / UNIT;
            return (largest.max(self.current) * 2).clamp(MIN_UNITS, self.max_units);
        };
        let bytes = if fit.per_byte <= 0.0 {
            // Bigger fragments weren't slower at all (noise, or a very fast
            // line): the largest allowed.
            f64::MAX
        } else {
            // Overhead at most ~10% of the time, and at most MAX_SECS per fragment.
            let overhead = fit.overhead.clamp(0.05, 5.0);
            (EFFICIENCY * overhead / fit.per_byte).min(MAX_SECS / fit.per_byte)
        };
        let units = (bytes / UNIT as f64).floor();
        let units = if units.is_finite() && units < self.max_units as f64 { units as usize } else { self.max_units };
        units.clamp(self.current / 2, self.current * 2).clamp(MIN_UNITS, self.max_units)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const MIB: f64 = 1024.0 * 1024.0;

    /// A line with `overhead` seconds per request and `speed` MiB/s.
    fn send(s: &mut Sizer, overhead: f64, speed: f64, remaining: &mut u64) -> usize {
        let n = s.next(*remaining);
        s.observe(n, overhead + n as f64 / MIB / speed, false);
        *remaining -= n as u64;
        n
    }

    #[test]
    fn grows_to_the_largest_on_a_fast_line_with_costly_requests() {
        // As measured on the test account: ~0.75 s per request, ~35 MiB/s.
        let mut s = Sizer::new(usize::MAX);
        let mut left = 10 << 30;
        let sizes: Vec<usize> = (0..6).map(|_| send(&mut s, 0.75, 35.0, &mut left) / UNIT).collect();
        assert_eq!(sizes[0], START_UNITS);
        assert_eq!(*sizes.last().unwrap(), MAX_UNITS, "{sizes:?}");
    }

    #[test]
    fn stays_moderate_on_a_slow_line() {
        // 1 MiB/s: 30 s at most per fragment, and 10% overhead needs ~6.75 MiB.
        let mut s = Sizer::new(usize::MAX);
        let mut left = 10 << 30;
        for _ in 0..12 {
            send(&mut s, 0.75, 1.0, &mut left);
        }
        let mib = s.next(u64::MAX) as f64 / MIB;
        assert!((5.0..=8.0).contains(&mib), "{mib} MiB");
    }

    #[test]
    fn a_very_slow_line_is_capped_by_time() {
        // 0.1 MiB/s with almost no overhead: 30 s is ~3 MiB.
        let mut s = Sizer::new(usize::MAX);
        let mut left = 10 << 30;
        for _ in 0..12 {
            send(&mut s, 0.2, 0.1, &mut left);
        }
        let mib = s.next(u64::MAX) as f64 / MIB;
        assert!((1.25..=3.2).contains(&mib), "{mib} MiB");
    }

    #[test]
    fn one_slow_fragment_while_probing_does_not_undo_the_growth() {
        // As seen live: 10 MiB in 1.13 s, then 20 MiB in a slow 2.15 s.
        let mut s = Sizer::new(usize::MAX);
        s.observe(10 * 1024 * 1024, 1.13, false);
        s.observe(20 * 1024 * 1024, 2.15, false);
        assert!(s.next(u64::MAX) >= 20 * 1024 * 1024, "still probing upwards");
        let mut left = 10 << 30;
        for _ in 0..6 {
            send(&mut s, 0.75, 35.0, &mut left);
        }
        assert_eq!(s.next(u64::MAX), MAX_UNITS * UNIT);
    }

    #[test]
    fn failures_halve_and_respect_the_floor() {
        let mut s = Sizer::new(usize::MAX);
        s.observe(START_UNITS * UNIT, 1.0, true);
        assert_eq!(s.next(u64::MAX), START_UNITS / 2 * UNIT);
        for _ in 0..10 {
            s.failed();
        }
        assert_eq!(s.next(u64::MAX), MIN_UNITS * UNIT);
    }

    #[test]
    fn the_memory_limit_and_the_remainder_bound_the_size() {
        let mut s = Sizer::new(12 * 1024 * 1024);
        assert_eq!(s.max_bytes(), 38 * UNIT);
        let mut left = 10 << 30;
        for _ in 0..8 {
            send(&mut s, 0.75, 35.0, &mut left);
        }
        assert_eq!(s.next(u64::MAX), 38 * UNIT);
        assert_eq!(s.next(1000), 1000, "the last fragment is what's left");
        assert_eq!(Sizer::new(0).max_bytes(), MIN_UNITS * UNIT);
    }

    #[test]
    fn every_size_is_a_whole_number_of_units_or_the_remainder() {
        let mut s = Sizer::new(usize::MAX);
        let mut left: u64 = 777 * 1024 * 1024 + 12345;
        let mut speed = 3.0;
        while left > 0 {
            let n = s.next(left);
            assert!(n.is_multiple_of(UNIT) || n as u64 == left);
            assert!(n <= MAX_UNITS * UNIT);
            // A wandering line, including noise that makes bigger look faster.
            speed = if speed > 40.0 { 2.0 } else { speed * 1.7 };
            s.observe(n, 0.5 + n as f64 / MIB / speed, false);
            left -= n as u64;
        }
    }
}
