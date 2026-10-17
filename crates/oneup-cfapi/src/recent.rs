//! A bounded, in-memory record of what happened to files lately, keyed by
//! file ID: what [`crate::closes`] and [`crate::ours`] keep. Making room
//! costs amortized O(1) per entry, not a pass over the whole map: the
//! close notifications that fill one arrive on Windows' callback thread,
//! under a lock every new file would otherwise hold for two passes over
//! thousands of entries.

use std::collections::{HashMap, VecDeque};
use std::time::{Duration, Instant};

/// At most `max` entries, each forgotten `ttl` after it was last touched;
/// when full, the least recently touched goes first.
pub(crate) struct Recent<V> {
    map: HashMap<u128, Entry<V>>,
    /// Keys by when they were touched, oldest first. Touching a key again
    /// leaves its earlier place behind: that place is stale (its `seq` is
    /// no longer the entry's) and is skipped, never taken for the entry.
    order: VecDeque<(u128, u64)>,
    next: u64,
    max: usize,
    ttl: Duration,
}

struct Entry<V> {
    /// Which place in `order` is this entry's.
    seq: u64,
    at: Instant,
    value: V,
}

impl<V: Default> Recent<V> {
    pub(crate) fn new(max: usize, ttl: Duration) -> Self {
        Self { map: HashMap::new(), order: VecDeque::new(), next: 0, max, ttl }
    }

    /// The value for `key`, created if it isn't kept, touched `now`. `now`
    /// must not be earlier than any earlier touch (callers pass
    /// `Instant::now()` under the lock that guards this).
    pub(crate) fn touch(&mut self, key: u128, now: Instant) -> &mut V {
        self.expire(now);
        if self.map.len() >= self.max && !self.map.contains_key(&key) {
            self.drop_oldest();
        }
        let seq = self.next;
        self.next += 1;
        self.order.push_back((key, seq));
        // Stale places pile up when the same keys are touched again and
        // again; clearing them when they outnumber the entries keeps
        // `order` within twice the limit, at O(1) per touch.
        if self.order.len() > 2 * self.max.max(1) {
            let map = &self.map;
            self.order.retain(|(k, s)| map.get(k).is_some_and(|e| e.seq == *s));
        }
        let e = self.map.entry(key).or_insert_with(|| Entry { seq, at: now, value: V::default() });
        e.seq = seq;
        e.at = now;
        &mut e.value
    }

    /// When `key` was last touched, and its value.
    pub(crate) fn get(&self, key: u128) -> Option<(Instant, &V)> {
        self.map.get(&key).map(|e| (e.at, &e.value))
    }

    #[cfg(test)]
    pub(crate) fn len(&self) -> usize {
        self.map.len()
    }

    /// Forgets the entries last touched `ttl` or longer before `now`: all at
    /// the front of `order`.
    fn expire(&mut self, now: Instant) {
        while let Some(&(key, seq)) = self.order.front() {
            match self.map.get(&key) {
                Some(e) if e.seq == seq => {
                    if now.duration_since(e.at) < self.ttl {
                        return;
                    }
                    self.map.remove(&key);
                }
                // Stale: the key was touched again later, or is gone.
                _ => {}
            }
            self.order.pop_front();
        }
    }

    /// Forgets the least recently touched entry.
    fn drop_oldest(&mut self) {
        while let Some((key, seq)) = self.order.pop_front() {
            if self.map.get(&key).is_some_and(|e| e.seq == seq) {
                self.map.remove(&key);
                return;
            }
        }
    }
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used)]
    use super::Recent;
    use std::time::{Duration, Instant};

    #[test]
    fn the_least_recently_touched_goes_first() {
        let t = Instant::now();
        let mut r = Recent::<u32>::new(3, Duration::from_secs(60));
        for k in 1..=3 {
            *r.touch(k, t) = k as u32;
        }
        *r.touch(4, t) = 4;
        assert!(r.get(1).is_none(), "the oldest went");
        assert_eq!([2, 3, 4].map(|k| r.get(k).map(|(_, v)| *v)), [Some(2), Some(3), Some(4)]);
        assert_eq!(r.len(), 3);
    }

    #[test]
    fn a_touched_entry_is_never_dropped_for_its_earlier_place() {
        let t = Instant::now();
        let s = Duration::from_secs(1);
        let mut r = Recent::<u32>::new(2, Duration::from_secs(60));
        *r.touch(1, t) = 10;
        *r.touch(2, t + s) = 20;
        // 1 is touched again: now the most recent, keeping its value.
        assert_eq!(*r.touch(1, t + 2 * s), 10);
        r.touch(3, t + 3 * s);
        assert!(r.get(2).is_none(), "2 was the least recently touched");
        assert_eq!(r.get(1), Some((t + 2 * s, &10)));
        r.touch(4, t + 4 * s);
        assert!(r.get(1).is_none() && r.get(3).is_some() && r.get(4).is_some());
    }

    #[test]
    fn expired_entries_go_and_touching_one_key_stays_bounded() {
        let t = Instant::now();
        let ttl = Duration::from_secs(60);
        let mut r = Recent::<()>::new(4, ttl);
        r.touch(1, t);
        r.touch(2, t + Duration::from_secs(30));
        r.touch(3, t + ttl);
        assert!(r.get(1).is_none(), "touched a minute ago");
        assert!(r.get(2).is_some());
        for i in 0..1000 {
            r.touch(7, t + ttl + Duration::from_millis(i));
        }
        assert!(r.order.len() <= 8, "{}", r.order.len());
        assert_eq!(r.len(), 3);
    }
}
