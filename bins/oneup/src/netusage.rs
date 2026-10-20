//! Data used per network profile, by direction, and its monthly budget
//! (`docs/user-guide/slow-and-metered-networks.md` § Data budget).
//!
//! The agent counts what its Graph client sent and received
//! ([`Meter`]: file contents as they go, and every response) and adds it
//! to the profile in effect, kept in the store's `meta` table
//! (`net.usage.<profile>`), so a restart doesn't forget it. A new month
//! starts from zero. When a budget is reached the agent pauses on that
//! network and says so; the user can allow more for the rest of the month.

use oneup_store::Store;

/// What one "allow more" answer adds unless the user says otherwise.
pub const ALLOW_MB: u64 = 500;

const KEY: &str = "net.usage.";

/// One profile's use this month.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Usage {
    /// `YYYY-MM`, local time.
    pub month: String,
    pub up: u64,
    pub down: u64,
    /// Bytes the user allowed on top of the budget this month.
    pub extra: u64,
}

impl Usage {
    pub fn total(&self) -> u64 {
        self.up.saturating_add(self.down)
    }

    /// What's left of `budget` (bytes) and what was allowed on top;
    /// `None`: no budget.
    pub fn left(&self, budget: Option<u64>) -> Option<u64> {
        Some(budget?.saturating_add(self.extra).saturating_sub(self.total()))
    }

    pub fn reached(&self, budget: Option<u64>) -> bool {
        self.left(budget) == Some(0)
    }
}

/// The month of Unix time `ms`, local time (UTC if the offset can't be told).
pub fn month_of(ms: i64) -> String {
    let offset = time::UtcOffset::current_local_offset().unwrap_or(time::UtcOffset::UTC);
    let t = time::OffsetDateTime::from_unix_timestamp(ms / 1000).unwrap_or(time::OffsetDateTime::UNIX_EPOCH);
    let t = t.to_offset(offset);
    format!("{:04}-{:02}", t.year(), u8::from(t.month()))
}

/// Profile `profile`'s use in `month` (a new month starts from zero).
pub fn load(store: &Store, profile: &str, month: &str) -> oneup_store::Result<Usage> {
    let text = store.read(|o| o.meta_get(&format!("{KEY}{profile}")))?;
    let v: Option<serde_json::Value> = text.and_then(|t| serde_json::from_str(&t).ok());
    let fresh = Usage { month: month.to_string(), ..Usage::default() };
    Ok(match v {
        Some(v) if v["month"].as_str() == Some(month) => Usage {
            month: month.to_string(),
            up: v["up"].as_u64().unwrap_or(0),
            down: v["down"].as_u64().unwrap_or(0),
            extra: v["extra"].as_u64().unwrap_or(0),
        },
        _ => fresh,
    })
}

pub fn save(store: &mut Store, profile: &str, u: &Usage) -> oneup_store::Result<()> {
    let text = serde_json::json!({ "month": u.month, "up": u.up, "down": u.down, "extra": u.extra }).to_string();
    store.write(|o| o.meta_set(&format!("{KEY}{profile}"), &text))
}

/// Adds `up` and `down` bytes and `extra` allowed bytes to `profile`'s
/// use this month; returns it.
pub fn add(
    store: &mut Store,
    profile: &str,
    month: &str,
    up: u64,
    down: u64,
    extra: u64,
) -> oneup_store::Result<Usage> {
    let mut u = load(store, profile, month)?;
    if up == 0 && down == 0 && extra == 0 {
        return Ok(u);
    }
    u.up = u.up.saturating_add(up);
    u.down = u.down.saturating_add(down);
    u.extra = u.extra.saturating_add(extra);
    save(store, profile, &u)?;
    Ok(u)
}

/// Counts what went over the network since it was last asked, from
/// counters that only grow (the Graph client's).
#[derive(Debug, Clone, Copy, Default)]
pub struct Meter {
    up: u64,
    down: u64,
}

impl Meter {
    pub fn new(up: u64, down: u64) -> Self {
        Self { up, down }
    }

    /// The bytes sent and received since the last [`Meter::commit`],
    /// given the counters now.
    pub fn peek(&self, up: u64, down: u64) -> (u64, u64) {
        (up.saturating_sub(self.up), down.saturating_sub(self.down))
    }

    /// What [`Meter::peek`] said is accounted for.
    pub fn commit(&mut self, up: u64, down: u64) {
        self.up = self.up.max(up);
        self.down = self.down.max(down);
    }
}

/// Charges what went over the network to the profile in effect while it
/// went: what went before a change of network is the old profile's, and a
/// failed write charges it again the next time instead of losing it.
#[derive(Debug, Clone, Default)]
pub struct Account {
    meter: Meter,
    /// The profile in effect since the last charge (`crate::network::Key::id`).
    profile: Option<String>,
}

impl Account {
    pub fn new(up: u64, down: u64) -> Self {
        Self { meter: Meter::new(up, down), profile: None }
    }

    /// Adds what went since the last charge (the counters now: `up`,
    /// `down`) to the profile in effect then, through `write(profile, up,
    /// down)`; the meter moves on only once that succeeded.
    pub fn charge<E>(
        &mut self,
        up: u64,
        down: u64,
        write: impl FnOnce(&str, u64, u64) -> Result<(), E>,
    ) -> Result<(), E> {
        let (du, dd) = self.meter.peek(up, down);
        if let Some(p) = &self.profile
            && (du > 0 || dd > 0)
        {
            write(p, du, dd)?;
        }
        self.meter.commit(up, down);
        Ok(())
    }

    /// Charge to `profile` from now on (after charging what went before).
    pub fn switch(&mut self, profile: &str) {
        self.profile = Some(profile.to_string());
    }

    pub fn profile(&self) -> Option<&str> {
        self.profile.as_deref()
    }
}

/// The agent's record of data used: an [`Account`] written to its own
/// database connection, reading the transport's counters (`counters`:
/// bytes sent, bytes received so far). Charged whenever the agent asks,
/// and once more when dropped, so what went just before a stop, an error
/// or a restart is kept.
pub struct Ledger {
    db: Store,
    account: Account,
    counters: Box<dyn Fn() -> (u64, u64) + Send>,
}

impl Ledger {
    pub fn new(db: Store, counters: Box<dyn Fn() -> (u64, u64) + Send>) -> Self {
        let (up, down) = counters();
        Self { db, account: Account::new(up, down), counters }
    }

    /// Adds what went since the last charge to the profile in effect
    /// meanwhile.
    pub fn charge(&mut self) -> oneup_store::Result<()> {
        let (up, down) = (self.counters)();
        let month = month_of(crate::control::now_ms());
        let db = &mut self.db;
        self.account.charge(up, down, |p, u, d| add(db, p, &month, u, d, 0).map(drop))
    }

    /// Charge to `profile` from now on (charge first what went before).
    pub fn switch(&mut self, profile: &str) {
        self.account.switch(profile);
    }

    pub fn db(&mut self) -> &mut Store {
        &mut self.db
    }
}

impl Drop for Ledger {
    fn drop(&mut self) {
        if let Err(e) = self.charge() {
            tracing::warn!(error = %e, "the data used last couldn't be recorded");
        }
    }
}

/// The automatic pause when the budget is reached (how much shows with the
/// data used, `oneup status` and the window).
pub fn pause_reason(u: &Usage, budget: Option<u64>) -> Option<crate::autopause::Reason> {
    budget?;
    u.reached(budget)
        .then(|| crate::autopause::Reason::fixed(crate::t_key!("this month's data budget on this network is used up")))
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used)]
    use super::*;

    const MB: u64 = 1_000_000;

    #[test]
    fn usage_adds_up_by_direction_and_starts_again_each_month() {
        let mut s = Store::open_in_memory().unwrap();
        add(&mut s, "metered", "2026-10", 3 * MB, 5 * MB, 0).unwrap();
        let u = add(&mut s, "metered", "2026-10", MB, 0, 0).unwrap();
        assert_eq!((u.up, u.down, u.total()), (4 * MB, 5 * MB, 9 * MB));
        assert_eq!(load(&s, "default", "2026-10").unwrap().total(), 0, "kept per profile");
        assert_eq!(load(&s, "metered", "2026-11").unwrap(), Usage { month: "2026-11".into(), ..Usage::default() });
    }

    #[test]
    fn a_reached_budget_pauses_until_more_is_allowed() {
        let mut s = Store::open_in_memory().unwrap();
        let budget = Some(10 * MB);
        let u = add(&mut s, "metered", "2026-10", 4 * MB, 7 * MB, 0).unwrap();
        assert!(u.reached(budget) && u.left(budget) == Some(0));
        assert!(pause_reason(&u, budget).unwrap().english().contains("data budget"));
        assert_eq!(u.left(None), None, "no budget, never reached");
        let u = add(&mut s, "metered", "2026-10", 0, 0, ALLOW_MB * MB).unwrap();
        assert_eq!(u.left(budget), Some(ALLOW_MB * MB - MB));
        assert_eq!(pause_reason(&u, budget), None);
    }

    #[test]
    fn the_meter_counts_only_whats_new() {
        let mut m = Meter::new(100, 1000);
        assert_eq!(m.peek(150, 1000), (50, 0));
        m.commit(150, 1000);
        assert_eq!(m.peek(150, 1500), (0, 500));
        m.commit(150, 1500);
        assert_eq!(m.peek(10, 10), (0, 0), "counters never go back");
    }

    /// Traffic before a change of network is the old profile's; a write
    /// that fails is charged again next time, not lost.
    #[test]
    fn each_profile_is_charged_its_own_traffic_and_nothing_is_lost() {
        let mut s = Store::open_in_memory().unwrap();
        let month = "2026-10";
        let mut a = Account::new(0, 0);
        let write = |s: &mut Store, p: &str, up, down| add(s, p, month, up, down, 0).map(drop);
        a.charge(5, 5, |p, u, d| write(&mut s, p, u, d)).unwrap();
        a.switch("default");
        a.charge(100, 200, |p, u, d| write(&mut s, p, u, d)).unwrap();
        // Windows reports a metered network: what went until now is still
        // the default profile's.
        a.charge(150, 300, |p, u, d| write(&mut s, p, u, d)).unwrap();
        a.switch("metered");
        a.charge(160, 300, |p, u, d| write(&mut s, p, u, d)).unwrap();
        assert_eq!(load(&s, "default", month).unwrap().total(), 145 + 295);
        assert_eq!(load(&s, "metered", month).unwrap().up, 10);
        // The database refused: charged at the next try.
        assert!(a.charge(200, 300, |_, _, _| Err("busy")).is_err());
        a.charge(210, 300, |p, u, d| write(&mut s, p, u, d)).unwrap();
        assert_eq!(load(&s, "metered", month).unwrap().up, 60);
    }

    /// What went just before the agent stopped (a stop, an error that
    /// ends the attempt, a restart) is kept: the ledger charges once more
    /// as it goes.
    #[test]
    fn what_went_before_a_stop_is_kept() {
        use std::sync::Arc;
        use std::sync::atomic::{AtomicU64, Ordering::Relaxed};
        let path = std::env::temp_dir().join(format!("oneup-ledger-{}.db", std::process::id()));
        let _ = std::fs::remove_file(&path);
        let (sent, got) = (Arc::new(AtomicU64::new(10)), Arc::new(AtomicU64::new(10)));
        let (s, g) = (Arc::clone(&sent), Arc::clone(&got));
        let mut l =
            Ledger::new(Store::open(&path, 1024).unwrap(), Box::new(move || (s.load(Relaxed), g.load(Relaxed))));
        l.switch("metered");
        sent.fetch_add(3 * MB, Relaxed);
        got.fetch_add(4 * MB, Relaxed);
        drop(l);
        let month = month_of(crate::control::now_ms());
        let u = load(&Store::open(&path, 1024).unwrap(), "metered", &month).unwrap();
        assert_eq!((u.up, u.down), (3 * MB, 4 * MB));
        let _ = std::fs::remove_file(&path);
    }

    #[test]
    fn months_are_named_by_year_and_month() {
        let m = month_of(1_790_000_000_000);
        assert!(m.len() == 7 && m.as_bytes()[4] == b'-', "{m}");
    }
}
