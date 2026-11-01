//! Data used per network profile, by direction, and its monthly budget
//! (`docs/user-guide/slow-and-metered-networks.md` § Data budget).
//!
//! The agent counts what its Graph client sent and received
//! ([`Meter`]: every request body and every answer, as they go) and adds
//! it to the profile in effect, kept in the `meta` table of the account's
//! **main** data folder (`net.usage.<profile>`), so every agent of the
//! account (its extra pairs and vaults too) shares one count and one
//! budget, and a restart doesn't forget it. Agents take the budget in
//! reservations ([`reserve`]), each in one transaction, so two agents
//! can't both spend what's left. A new month
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

/// A profile's record this month: what was used, and what each agent
/// (by process) has reserved and not used yet.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
struct Record {
    usage: Usage,
    reserved: std::collections::BTreeMap<String, u64>,
}

fn read_record(o: &oneup_store::Ops<'_>, profile: &str, month: &str) -> oneup_store::Result<Record> {
    let text = o.meta_get(&format!("{KEY}{profile}"))?;
    let v: Option<serde_json::Value> = text.and_then(|t| serde_json::from_str(&t).ok());
    let fresh = Record { usage: Usage { month: month.to_string(), ..Usage::default() }, ..Record::default() };
    Ok(match v {
        Some(v) if v["month"].as_str() == Some(month) => Record {
            usage: Usage {
                month: month.to_string(),
                up: v["up"].as_u64().unwrap_or(0),
                down: v["down"].as_u64().unwrap_or(0),
                extra: v["extra"].as_u64().unwrap_or(0),
            },
            reserved: v["reserved"]
                .as_object()
                .map(|m| m.iter().filter_map(|(k, n)| Some((k.clone(), n.as_u64()?))).collect())
                .unwrap_or_default(),
        },
        _ => fresh,
    })
}

fn write_record(o: &oneup_store::Ops<'_>, profile: &str, r: &Record) -> oneup_store::Result<()> {
    let u = &r.usage;
    let text = serde_json::json!({
        "month": u.month, "up": u.up, "down": u.down, "extra": u.extra, "reserved": r.reserved,
    })
    .to_string();
    o.meta_set(&format!("{KEY}{profile}"), &text)
}

/// Profile `profile`'s use in `month` (a new month starts from zero).
pub fn load(store: &Store, profile: &str, month: &str) -> oneup_store::Result<Usage> {
    store.read(|o| Ok(read_record(o, profile, month)?.usage))
}

/// Adds `up` and `down` bytes and `extra` allowed bytes to `profile`'s
/// use this month, in one transaction; returns it.
pub fn add(
    store: &mut Store,
    profile: &str,
    month: &str,
    up: u64,
    down: u64,
    extra: u64,
) -> oneup_store::Result<Usage> {
    charge_owned(store, profile, month, None, up, down, extra)
}

/// [`add`], the bytes taken from what `owner` had reserved.
fn charge_owned(
    store: &mut Store,
    profile: &str,
    month: &str,
    owner: Option<&str>,
    up: u64,
    down: u64,
    extra: u64,
) -> oneup_store::Result<Usage> {
    store.write(|o| {
        let mut r = read_record(o, profile, month)?;
        if up == 0 && down == 0 && extra == 0 {
            return Ok(r.usage);
        }
        r.usage.up = r.usage.up.saturating_add(up);
        r.usage.down = r.usage.down.saturating_add(down);
        r.usage.extra = r.usage.extra.saturating_add(extra);
        if let Some(own) = owner.and_then(|w| r.reserved.get_mut(w)) {
            *own = own.saturating_sub(up.saturating_add(down));
        }
        write_record(o, profile, &r)?;
        Ok(r.usage)
    })
}

/// The reservations of agents no longer running (`alive`; never
/// `owner`'s), counted as used and dropped: such an agent may have used
/// all of it since it last charged (a hard kill loses what it counted
/// meanwhile), so it's never given out again. Counted as received.
/// Whether any was.
fn reap(r: &mut Record, owner: &str, alive: &dyn Fn(&str) -> bool) -> bool {
    let stopped: Vec<String> = r.reserved.keys().filter(|k| k.as_str() != owner && !alive(k)).cloned().collect();
    for k in &stopped {
        let n = r.reserved.remove(k).unwrap_or(0);
        r.usage.down = r.usage.down.saturating_add(n);
    }
    !stopped.is_empty()
}

/// [`reap`] at a look at the budget, before what's left is worked out:
/// read first, and written only if an agent stopped without giving its
/// reservation back.
pub fn reap_stopped(
    store: &mut Store,
    profile: &str,
    month: &str,
    owner: &str,
    alive: &dyn Fn(&str) -> bool,
) -> oneup_store::Result<()> {
    let any = store.read(|o| {
        let r = read_record(o, profile, month)?;
        Ok(r.reserved.keys().any(|k| k.as_str() != owner && !alive(k)))
    })?;
    if any {
        store.write(|o| {
            let mut r = read_record(o, profile, month)?;
            if reap(&mut r, owner, alive) {
                write_record(o, profile, &r)?;
            }
            Ok(())
        })?;
    }
    Ok(())
}

/// Reserves up to `want` bytes of `budget` (and what was allowed on top)
/// for `owner` (an agent's process), in one transaction with every other
/// agent's reservations: what's granted is what nobody else has used or
/// reserved. Reservations of agents no longer running (`alive`) count as
/// used ([`reap`]). Returns what was granted.
pub fn reserve(
    store: &mut Store,
    profile: &str,
    month: &str,
    owner: &str,
    want: u64,
    budget: u64,
    alive: &dyn Fn(&str) -> bool,
) -> oneup_store::Result<u64> {
    store.write(|o| {
        let mut r = read_record(o, profile, month)?;
        reap(&mut r, owner, alive);
        r.reserved.retain(|_, n| *n > 0);
        let committed = r.usage.total().saturating_add(r.reserved.values().sum());
        let grant = want.min(budget.saturating_add(r.usage.extra).saturating_sub(committed));
        if grant > 0 {
            *r.reserved.entry(owner.to_string()).or_default() += grant;
        }
        write_record(o, profile, &r)?;
        Ok(grant)
    })
}

/// What's left of `budget` for `owner`: not used, nor reserved by another
/// agent.
pub fn left_for(store: &Store, profile: &str, month: &str, owner: &str, budget: u64) -> oneup_store::Result<u64> {
    store.read(|o| {
        let r = read_record(o, profile, month)?;
        let others: u64 = r.reserved.iter().filter(|(k, _)| k.as_str() != owner).map(|(_, n)| *n).sum();
        Ok(budget.saturating_add(r.usage.extra).saturating_sub(r.usage.total().saturating_add(others)))
    })
}

/// Gives back what `owner` reserved and didn't use.
pub fn release(store: &mut Store, profile: &str, month: &str, owner: &str) -> oneup_store::Result<()> {
    store.write(|o| {
        let mut r = read_record(o, profile, month)?;
        if r.reserved.remove(owner).is_some() {
            write_record(o, profile, &r)?;
        }
        Ok(())
    })
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
    /// The account's main data folder's store, shared by its agents.
    db: Store,
    account: Account,
    counters: Box<dyn Fn() -> (u64, u64) + Send>,
    /// Who reserves (this agent's process ID).
    owner: String,
}

impl Ledger {
    pub fn new(db: Store, counters: Box<dyn Fn() -> (u64, u64) + Send>, owner: String) -> Self {
        let (up, down) = counters();
        Self { db, account: Account::new(up, down), counters, owner }
    }

    /// Adds what went since the last charge to the profile in effect
    /// meanwhile, taken from what this agent reserved.
    pub fn charge(&mut self) -> oneup_store::Result<()> {
        let (up, down) = (self.counters)();
        let month = month_of(crate::control::now_ms());
        let (db, owner) = (&mut self.db, self.owner.as_str());
        self.account.charge(up, down, |p, u, d| charge_owned(db, p, &month, Some(owner), u, d, 0).map(drop))
    }

    /// Charge to `profile` from now on (charge first what went before);
    /// what was reserved on the profile before goes back.
    pub fn switch(&mut self, profile: &str) {
        if let Some(old) = self.account.profile().filter(|p| *p != profile).map(str::to_string) {
            let month = month_of(crate::control::now_ms());
            if let Err(e) = release(&mut self.db, &old, &month, &self.owner) {
                tracing::debug!(error = %e, "a reservation of the data budget wasn't given back");
            }
        }
        self.account.switch(profile);
    }

    pub fn owner(&self) -> &str {
        &self.owner
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
        if let Some(p) = self.account.profile().map(str::to_string) {
            let _ = release(&mut self.db, &p, &month_of(crate::control::now_ms()), &self.owner);
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

/// Whether the agent that took a reservation (`owner`, its process ID)
/// still runs: a process with that ID runs `oneupd.exe`. Windows reuses
/// process IDs, so the ID alone could keep a dead agent's share forever.
pub fn agent_alive(owner: &str) -> bool {
    owner.parse::<u32>().ok().and_then(oneup_sys::processes::image_of).is_some_and(|p| is_agent(&p))
}

fn is_agent(image: &std::path::Path) -> bool {
    image.file_stem().is_some_and(|s| s.eq_ignore_ascii_case("oneupd"))
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used)]
    use super::*;

    /// A process that took over a stopped agent's ID isn't the agent.
    #[test]
    fn only_a_running_oneupd_holds_a_reservation() {
        assert!(!agent_alive(&std::process::id().to_string()), "this test runs, but isn't oneupd");
        assert!(!agent_alive("not a number"));
        assert!(is_agent(std::path::Path::new(r"C:\Program Files\OneUpDrive\OneUpD.exe")));
        assert!(!is_agent(std::path::Path::new(r"C:\Windows\System32\cmd.exe")));
    }

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
        let mut l = Ledger::new(
            Store::open(&path, 1024).unwrap(),
            Box::new(move || (s.load(Relaxed), g.load(Relaxed))),
            "1".into(),
        );
        l.switch("metered");
        sent.fetch_add(3 * MB, Relaxed);
        got.fetch_add(4 * MB, Relaxed);
        drop(l);
        let month = month_of(crate::control::now_ms());
        let u = load(&Store::open(&path, 1024).unwrap(), "metered", &month).unwrap();
        assert_eq!((u.up, u.down), (3 * MB, 4 * MB));
        let _ = std::fs::remove_file(&path);
    }

    /// Two agents (an account's main folder and an extra pair) share one
    /// budget: each takes it in reservations from the main data folder's
    /// store, so together they never get more than it holds; what one
    /// uses counts for both; a stopped agent's reservation goes back.
    #[test]
    fn agents_of_one_account_share_one_budget() {
        let path = std::env::temp_dir().join(format!("oneup-shared-budget-{}.db", std::process::id()));
        let _ = std::fs::remove_file(&path);
        let (mut a, mut b) = (Store::open(&path, 1024).unwrap(), Store::open(&path, 1024).unwrap());
        let m = "2026-10";
        let all = |_: &str| true;
        let budget = 100 * MB;
        assert_eq!(reserve(&mut a, "metered", m, "A", 40 * MB, budget, &all).unwrap(), 40 * MB);
        assert_eq!(reserve(&mut b, "metered", m, "B", 40 * MB, budget, &all).unwrap(), 40 * MB);
        assert_eq!(reserve(&mut a, "metered", m, "A", 40 * MB, budget, &all).unwrap(), 20 * MB, "only what's left");
        assert_eq!(reserve(&mut b, "metered", m, "B", 1, budget, &all).unwrap(), 0);
        // B used 30 of its 40: it counts for both, and B's reservation shrinks.
        charge_owned(&mut b, "metered", m, Some("B"), 10 * MB, 20 * MB, 0).unwrap();
        assert_eq!(load(&a, "metered", m).unwrap().total(), 30 * MB);
        assert_eq!(left_for(&a, "metered", m, "A", budget).unwrap(), 100 * MB - 30 * MB - 10 * MB);
        // B stops: what it didn't use goes back to A.
        release(&mut b, "metered", m, "B").unwrap();
        assert_eq!(reserve(&mut a, "metered", m, "A", 40 * MB, budget, &all).unwrap(), 10 * MB);
        // An agent that stopped without giving its reservation back (killed)
        // may have used all of it: it counts as used, at the next look,
        // and is never given out again.
        let none = |w: &str| w == "A";
        release(&mut a, "metered", m, "A").unwrap();
        assert_eq!(reserve(&mut b, "metered", m, "B", 5 * MB, budget, &all).unwrap(), 5 * MB);
        reap_stopped(&mut a, "metered", m, "A", &none).unwrap();
        assert_eq!(load(&a, "metered", m).unwrap().total(), 35 * MB, "counted as used at the next look");
        assert_eq!(left_for(&a, "metered", m, "A", budget).unwrap(), 65 * MB);
        assert_eq!(
            reserve(&mut a, "metered", m, "A", 100 * MB, budget, &none).unwrap(),
            65 * MB,
            "not given out again"
        );
        // Killed, and found by a transfer's reservation: the same.
        release(&mut a, "metered", m, "A").unwrap();
        assert_eq!(reserve(&mut b, "metered", m, "B", MB, budget, &all).unwrap(), MB);
        reserve(&mut a, "metered", m, "A", 1, budget, &none).unwrap();
        assert_eq!(load(&a, "metered", m).unwrap().total(), 36 * MB);
        drop((a, b));
        let _ = std::fs::remove_file(&path);
    }

    #[test]
    fn months_are_named_by_year_and_month() {
        let m = month_of(1_790_000_000_000);
        assert!(m.len() == 7 && m.as_bytes()[4] == b'-', "{m}");
    }
}
