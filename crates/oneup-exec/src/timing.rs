//! Where a round's time goes: how long each kind of operation, and each
//! kind of request behind it, took. The agent logs it after each round
//! that did something (debug level), which tells a slow sync waiting on
//! the network from one waiting on this PC.

use std::collections::BTreeMap;
use std::sync::Mutex;
use std::time::{Duration, Instant};

/// One stage's times since the last [`Timings::take`].
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct Stage {
    pub count: u64,
    pub total: Duration,
    pub max: Duration,
}

/// Times by stage, shared by the executor and its cloud. A few dozen
/// stages at most, each a fixed name.
#[derive(Debug, Default)]
pub struct Timings {
    stages: Mutex<BTreeMap<&'static str, Stage>>,
}

impl Timings {
    /// Adds the time from `since` to now to `stage`.
    pub fn record(&self, stage: &'static str, since: Instant) {
        let took = since.elapsed();
        let mut stages = self.stages.lock().unwrap_or_else(|p| p.into_inner());
        let s = stages.entry(stage).or_default();
        s.count += 1;
        s.total += took;
        s.max = s.max.max(took);
    }

    /// The times so far, which start again from nothing.
    pub fn take(&self) -> BTreeMap<&'static str, Stage> {
        std::mem::take(&mut *self.stages.lock().unwrap_or_else(|p| p.into_inner()))
    }

    /// Logs [`Timings::take`]'s stages, one line each (debug level).
    pub fn log(&self) {
        for (stage, s) in self.take() {
            let avg_ms = s.total.as_millis() / u128::from(s.count.max(1));
            tracing::debug!(
                stage,
                count = s.count,
                total_ms = s.total.as_millis(),
                avg_ms,
                max_ms = s.max.as_millis(),
                "time per stage"
            );
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn stages_add_up_and_start_again_once_taken() {
        let t = Timings::default();
        let long_ago = Instant::now().checked_sub(Duration::from_millis(30)).unwrap_or_else(Instant::now);
        t.record("upload", long_ago);
        t.record("upload", Instant::now());
        t.record("download", Instant::now());
        let got = t.take();
        assert_eq!(got["upload"].count, 2);
        assert!(got["upload"].max >= Duration::from_millis(30));
        assert!(got["upload"].total >= got["upload"].max);
        assert_eq!(got["download"].count, 1);
        assert!(t.take().is_empty());
    }
}
