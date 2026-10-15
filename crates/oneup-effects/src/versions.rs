//! The local version store's contents and its **automatic trimming rule**
//! Pure, so the rule is tested on
//! its own; [`crate::LocalFs::trim_versions`] applies it.
//!
//! Trimming is the one place OneUpDrive permanently deletes user content,
//! so the rule is deliberately lopsided:
//! * nothing kept less than [`Trim::min_age_days`] ago is ever removed,
//!   however large the store;
//! * older items are removed oldest first, and only while the store is
//!   over its cap: the **smaller** of [`Trim::max_gib`] and
//!   [`Trim::max_percent`] of the disk. Under the cap, old items stay.
//!
//! A limit of 0 means "no limit of this kind"; with both size limits at 0
//! there is no cap, so nothing is ever trimmed automatically.
//!
//! **Rotation** ([`Trim::rotate`], off by default, the user's choice): when
//! new items wouldn't fit under the cap, the oldest go to make room for
//! them, whatever their age ([`Trim::make_room`]). It only ever makes room
//! *before* items are added, and only if that makes them fit, so it never
//! removes what was just kept (e.g. a batch the user chose to keep although
//! it is larger than the cap).
//!
//! [`Trim::room`] says how much a round may add without the store ending up
//! over its cap; the safety gate holds a batch that needs more (the
//! overflow decision, `oneup_gate`).

/// One item in the version store.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Slot {
    /// The slot's folder name (`<unix ms>-<n>`); what `restore` takes.
    pub id: String,
    /// When it was kept (Unix ms).
    pub at_ms: i64,
    pub name: String,
    /// Where it was, relative to the sync folder (if recorded).
    pub origin: Option<String>,
    pub is_dir: bool,
    pub bytes: u64,
}

/// The automatic trimming settings.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct Trim {
    pub min_age_days: u32,
    pub max_gib: f64,
    pub max_percent: f64,
    /// Make room for new items by removing the oldest, whatever their age.
    pub rotate: bool,
}

impl Default for Trim {
    fn default() -> Self {
        Self { min_age_days: 30, max_gib: 5.0, max_percent: 2.0, rotate: false }
    }
}

impl Trim {
    /// The store's size cap for a disk of `disk_bytes` (`None`: no cap).
    pub fn cap(&self, disk_bytes: u64) -> Option<u64> {
        let gib = (self.max_gib > 0.0).then_some((self.max_gib * (1u64 << 30) as f64) as u64);
        let pct = (self.max_percent > 0.0).then_some((disk_bytes as f64 * self.max_percent / 100.0) as u64);
        match (gib, pct) {
            (Some(a), Some(b)) => Some(a.min(b)),
            (a, b) => a.or(b),
        }
    }

    /// Slots kept at or after this time are protected from trimming.
    fn protect_from(&self, now_ms: i64) -> i64 {
        now_ms.saturating_sub(i64::from(self.min_age_days) * 86_400_000)
    }

    /// How many bytes can be added to the store at time `now_ms` without it
    /// staying over its cap after trimming (`None`: no cap). Trimming can't
    /// remove protected items, so they count against the cap; with rotation
    /// everything can make way.
    pub fn room(&self, slots: &[Slot], now_ms: i64, disk_bytes: u64) -> Option<u64> {
        let cap = self.cap(disk_bytes)?;
        if self.rotate {
            return Some(cap);
        }
        let protect_from = self.protect_from(now_ms);
        let protected: u64 = slots.iter().filter(|s| s.at_ms >= protect_from).map(|s| s.bytes).sum();
        Some(cap.saturating_sub(protected))
    }

    /// With rotation on: how many of `slots` (sorted oldest first) to remove,
    /// from the front and whatever their age, so that `need` more bytes fit
    /// under the cap. 0 without rotation or a cap, and 0 if `need` alone is
    /// over the cap (removing old items couldn't make it fit).
    pub fn make_room(&self, slots: &[Slot], need: u64, disk_bytes: u64) -> usize {
        let Some(cap) = self.cap(disk_bytes).filter(|&c| self.rotate && need > 0 && need <= c) else {
            return 0;
        };
        let mut total: u64 = slots.iter().map(|s| s.bytes).sum();
        let mut n = 0;
        for s in slots {
            if total + need <= cap {
                break;
            }
            total -= s.bytes;
            n += 1;
        }
        n
    }

    /// How many of `slots` (sorted oldest first) to remove, from the front,
    /// at time `now_ms` on a disk of `disk_bytes`. Never protected items,
    /// with or without rotation.
    pub fn select(&self, slots: &[Slot], now_ms: i64, disk_bytes: u64) -> usize {
        let Some(cap) = self.cap(disk_bytes) else { return 0 };
        let protect_from = self.protect_from(now_ms);
        let mut total: u64 = slots.iter().map(|s| s.bytes).sum();
        let mut n = 0;
        for s in slots {
            if total <= cap || s.at_ms >= protect_from {
                break;
            }
            total -= s.bytes;
            n += 1;
        }
        n
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const DAY: i64 = 86_400_000;
    const GIB: u64 = 1 << 30;

    fn slot(age_days: i64, bytes: u64, now: i64) -> Slot {
        Slot {
            id: format!("{}-0", now - age_days * DAY),
            at_ms: now - age_days * DAY,
            name: "x".into(),
            origin: None,
            is_dir: false,
            bytes,
        }
    }

    #[test]
    fn cap_is_the_smaller_limit_and_zero_means_none() {
        let t = Trim::default();
        assert_eq!(t.cap(1000 * GIB), Some(5 * GIB)); // 2% = 20 GiB
        assert_eq!(t.cap(100 * GIB), Some(2 * GIB)); // 2% = 2 GiB
        assert_eq!(Trim { max_gib: 0.0, ..t }.cap(100 * GIB), Some(2 * GIB));
        assert_eq!(Trim { max_percent: 0.0, ..t }.cap(100 * GIB), Some(5 * GIB));
        assert_eq!(Trim { max_gib: 0.0, max_percent: 0.0, ..t }.cap(100 * GIB), None);
    }

    #[test]
    fn only_old_items_go_oldest_first_and_only_while_over_the_cap() {
        let now = 1000 * DAY;
        let t = Trim::default();
        let disk = 1000 * GIB; // cap 5 GiB
        // 8 GiB in total: the two oldest (3 GiB) bring it to the cap.
        let slots = [slot(90, 2 * GIB, now), slot(60, GIB, now), slot(40, GIB, now), slot(1, 4 * GIB, now)];
        assert_eq!(t.select(&slots, now, disk), 2);
        // Under the cap: nothing, however old.
        assert_eq!(t.select(&slots[..3], now, disk), 0);
        // Over the cap, but the excess is all recent: protected.
        let recent = [slot(10, 3 * GIB, now), slot(5, 3 * GIB, now)];
        assert_eq!(t.select(&recent, now, disk), 0);
        // Old items go even if the recent ones alone keep it over the cap.
        let mixed = [slot(45, GIB, now), slot(2, 9 * GIB, now)];
        assert_eq!(t.select(&mixed, now, disk), 1);
        // No cap: never.
        assert_eq!(Trim { max_gib: 0.0, max_percent: 0.0, ..t }.select(&slots, now, disk), 0);
    }

    #[test]
    fn room_is_the_cap_less_what_trimming_cant_remove() {
        let now = 1000 * DAY;
        let disk = 1000 * GIB; // cap 5 GiB
        let t = Trim::default();
        let slots = [slot(90, 2 * GIB, now), slot(10, 3 * GIB, now)];
        // The old 2 GiB can be trimmed; the recent 3 GiB can't.
        assert_eq!(t.room(&slots, now, disk), Some(2 * GIB));
        assert_eq!(t.room(&[slot(1, 6 * GIB, now)], now, disk), Some(0));
        assert_eq!(Trim { rotate: true, ..t }.room(&slots, now, disk), Some(5 * GIB));
        assert_eq!(Trim { max_gib: 0.0, max_percent: 0.0, ..t }.room(&slots, now, disk), None);
    }

    #[test]
    fn rotation_makes_room_oldest_first_only_when_that_fits() {
        let now = 1000 * DAY;
        let disk = 1000 * GIB; // cap 5 GiB
        let slots = [slot(20, GIB, now), slot(10, 2 * GIB, now), slot(1, GIB, now)];
        let t = Trim { rotate: true, ..Trim::default() };
        // 4 GiB kept + 2 GiB new: the oldest (1 GiB, though recent) goes.
        assert_eq!(t.make_room(&slots, 2 * GIB, disk), 1);
        assert_eq!(t.make_room(&slots, GIB, disk), 0);
        assert_eq!(t.make_room(&slots, 5 * GIB, disk), 3);
        // More than the cap: removing everything wouldn't help, so nothing.
        assert_eq!(t.make_room(&slots, 6 * GIB, disk), 0);
        // Rotation off: never.
        assert_eq!(Trim::default().make_room(&slots, 2 * GIB, disk), 0);
        // The after-sync rule still protects recent items with rotation on.
        let over = [slot(10, 4 * GIB, now), slot(1, 4 * GIB, now)];
        assert_eq!(t.select(&over, now, disk), 0);
    }
}
