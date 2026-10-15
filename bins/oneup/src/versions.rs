//! `oneup versions` and `oneup restore`: the local version store
//! (`<sync folder>\.oneup\versions`), where sync keeps local files it
//! removed (and, if the user asks, files it replaced), plus the store's
//! settings: automatic trimming, rotation, and what happens when a batch
//! doesn't fit (see `oneup_gate`'s overflow decision).

use std::time::{Duration, SystemTime, UNIX_EPOCH};

use anyhow::{Result, bail};
use oneup_effects::{Slot, Trim};
use oneup_gate::{OverflowPolicy, Policy};
use oneup_store::Store;

const KEY_MIN_DAYS: &str = "versions.min_age_days";
const KEY_MAX_GIB: &str = "versions.max_gib";
const KEY_MAX_PERCENT: &str = "versions.max_percent";
const KEY_ROTATE: &str = "versions.rotate";
const KEY_OVERFLOW: &str = "versions.overflow";
const KEY_KEEP_REPLACED: &str = "versions.keep_replaced";
const KEY_MAX_CHANGES: &str = "safety.max_changes";
const KEY_MAX_SHARE: &str = "safety.max_percent";
const KEY_CONTENT_GUARD: &str = "safety.content_guard";

/// Changes to the safety stops (`oneup safety`; `None`: leave as is).
#[derive(Debug, Default, Clone, Copy)]
pub struct Safety {
    /// A round stops when it would delete, replace or move more than this
    /// many items on one side...
    pub max_changes: Option<usize>,
    /// ...or more than this share of all synced items (and over 20).
    pub max_percent: Option<u32>,
    /// It also stops when more than this many files would be sent with
    /// content that doesn't look like their type; 0 turns that off.
    pub content_guard: Option<usize>,
}

/// Saves changes to the safety stops.
pub fn set_safety(store: &mut Store, s: &Safety) -> Result<()> {
    if s.max_changes == Some(0) || s.max_percent.is_some_and(|p| p == 0 || p > 100) {
        bail!("the most changes must be 1 or more, and the share between 1 and 100 percent");
    }
    store.write(|o| {
        if let Some(v) = s.max_changes {
            o.meta_set(KEY_MAX_CHANGES, &v.to_string())?;
        }
        if let Some(v) = s.max_percent {
            o.meta_set(KEY_MAX_SHARE, &v.to_string())?;
        }
        if let Some(v) = s.content_guard {
            o.meta_set(KEY_CONTENT_GUARD, &v.to_string())?;
        }
        Ok(())
    })?;
    Ok(())
}

/// Changes to the version-store settings (`None`: leave as is).
#[derive(Debug, Default)]
pub struct Changes {
    pub min_days: Option<u32>,
    pub max_gib: Option<f64>,
    pub max_percent: Option<f64>,
    pub rotate: Option<bool>,
    pub overflow: Option<OverflowPolicy>,
    pub keep_replaced: Option<bool>,
}

impl Changes {
    pub fn is_empty(&self) -> bool {
        self.min_days.is_none()
            && self.max_gib.is_none()
            && self.max_percent.is_none()
            && self.rotate.is_none()
            && self.overflow.is_none()
            && self.keep_replaced.is_none()
    }
}

/// The safety gate's saved settings (the version store's, and the safety
/// stops' thresholds); the rest of the policy is the defaults.
pub fn policy(store: &Store) -> Result<Policy> {
    let d = Policy::default();
    let overflow = match store.read(|o| o.meta_get(KEY_OVERFLOW))?.as_deref() {
        Some("keep") => OverflowPolicy::Keep,
        Some("hold") => OverflowPolicy::Hold,
        _ => d.overflow,
    };
    let keep_replaced = flag(store, KEY_KEEP_REPLACED)?.unwrap_or(d.keep_replaced);
    let num = |k: &str| -> Result<Option<usize>> { Ok(store.read(|o| o.meta_get(k))?.and_then(|v| v.parse().ok())) };
    Ok(Policy {
        overflow,
        keep_replaced,
        max_count: num(KEY_MAX_CHANGES)?.filter(|&n| n > 0).unwrap_or(d.max_count),
        max_percent: num(KEY_MAX_SHARE)?
            .and_then(|n| u32::try_from(n).ok())
            .filter(|&n| n > 0)
            .unwrap_or(d.max_percent),
        max_suspicious: num(KEY_CONTENT_GUARD)?.unwrap_or(d.max_suspicious),
        ..d
    })
}

fn flag(store: &Store, key: &str) -> Result<Option<bool>> {
    Ok(store.read(|o| o.meta_get(key))?.map(|v| v == "on"))
}

fn on_off(b: bool) -> &'static str {
    if b { "on" } else { "off" }
}

/// The automatic trimming settings: saved ones, else the defaults.
pub fn trim_settings(store: &Store) -> Result<Trim> {
    let d = Trim::default();
    let get = |k: &str| store.read(|o| o.meta_get(k));
    Ok(Trim {
        min_age_days: get(KEY_MIN_DAYS)?.and_then(|v| v.parse().ok()).unwrap_or(d.min_age_days),
        max_gib: get(KEY_MAX_GIB)?.and_then(|v| v.parse().ok()).unwrap_or(d.max_gib),
        max_percent: get(KEY_MAX_PERCENT)?.and_then(|v| v.parse().ok()).unwrap_or(d.max_percent),
        rotate: flag(store, KEY_ROTATE)?.unwrap_or(d.rotate),
    })
}

/// Changes (and saves) the version-store settings.
pub fn set(store: &mut Store, c: &Changes) -> Result<()> {
    if c.max_gib.is_some_and(|g| !(g >= 0.0 && g.is_finite()))
        || c.max_percent.is_some_and(|p| !(0.0..=100.0).contains(&p))
    {
        bail!("sizes must be 0 or more, and the percentage at most 100");
    }
    store.write(|o| {
        if let Some(v) = c.min_days {
            o.meta_set(KEY_MIN_DAYS, &v.to_string())?;
        }
        if let Some(v) = c.max_gib {
            o.meta_set(KEY_MAX_GIB, &v.to_string())?;
        }
        if let Some(v) = c.max_percent {
            o.meta_set(KEY_MAX_PERCENT, &v.to_string())?;
        }
        if let Some(v) = c.rotate {
            o.meta_set(KEY_ROTATE, on_off(v))?;
        }
        if let Some(v) = c.overflow {
            o.meta_set(KEY_OVERFLOW, if v == OverflowPolicy::Keep { "keep" } else { "hold" })?;
        }
        if let Some(v) = c.keep_replaced {
            o.meta_set(KEY_KEEP_REPLACED, on_off(v))?;
        }
        Ok(())
    })?;
    Ok(())
}

/// The version-store settings, one line each.
pub fn describe(store: &Store) -> Result<String> {
    let t = trim_settings(store)?;
    let p = policy(store)?;
    let rotate = if t.rotate {
        "Rotation is on: when new items don't fit, the oldest kept items are removed to make room, whatever their age."
    } else {
        "Rotation is off (turn it on with --set-rotate on to make room by removing the oldest kept items)."
    };
    let overflow = match p.overflow {
        OverflowPolicy::Hold => {
            "If removed files don't fit in the store, sync leaves them on this PC and asks you once for all of them."
        }
        OverflowPolicy::Keep => {
            "If removed files don't fit in the store, sync keeps them anyway (the store goes over its limit) and tells you."
        }
    };
    let replaced = if p.keep_replaced {
        "Files replaced by a newer version from OneDrive are kept here too."
    } else {
        "Files replaced by a newer version from OneDrive aren't kept here: OneDrive's version history has them."
    };
    Ok(format!("{}\n{rotate}\n{overflow}\n{replaced}", describe_trim(&t)))
}

/// One line describing the automatic trimming settings.
pub fn describe_trim(t: &Trim) -> String {
    if t.max_gib <= 0.0 && t.max_percent <= 0.0 {
        return "Automatic trimming is off (both size limits are 0).".into();
    }
    let limit = match (t.max_gib > 0.0, t.max_percent > 0.0) {
        (true, true) => format!("{} GiB or {}% of the disk, whichever is smaller", t.max_gib, t.max_percent),
        (true, false) => format!("{} GiB", t.max_gib),
        _ => format!("{}% of the disk", t.max_percent),
    };
    format!(
        "Automatic trimming: items kept more than {} days ago are removed, oldest first, \
         only while the store is over {limit}.",
        t.min_age_days
    )
}

/// Lists the store, or with `older_than_days` / `max_gib` removes old items
/// (a preview unless `yes`).
pub fn versions(store: &Store, older_than_days: Option<u64>, max_gib: Option<f64>, yes: bool) -> Result<()> {
    let fs = crate::sync::open_paired(store)?;
    if older_than_days.is_none() && max_gib.is_none() {
        let slots = fs.versions()?;
        if slots.is_empty() {
            println!("The version store is empty.");
        }
        for s in &slots {
            print_slot(s);
        }
        let total: u64 = slots.iter().map(|s| s.bytes).sum();
        if !slots.is_empty() {
            println!("{} items, {}. Restore one with `oneup restore <id>`.", slots.len(), size(total));
        }
        println!("{}", describe(store)?);
        return Ok(());
    }
    let cutoff = older_than_days.map(|d| {
        let now = SystemTime::now().duration_since(UNIX_EPOCH).unwrap_or(Duration::ZERO);
        now.saturating_sub(Duration::from_secs(d * 86_400)).as_millis() as i64
    });
    let max_bytes = max_gib.map(|g| (g.max(0.0) * (1u64 << 30) as f64) as u64);
    let removed = fs.prune_versions(cutoff, max_bytes, !yes)?;
    let total: u64 = removed.iter().map(|s| s.bytes).sum();
    for s in &removed {
        print_slot(s);
    }
    if removed.is_empty() {
        println!("Nothing to remove.");
    } else if yes {
        println!("Removed {} items ({}) permanently.", removed.len(), size(total));
    } else {
        println!(
            "These {} items ({}) would be removed permanently. Add --yes to remove them.",
            removed.len(),
            size(total)
        );
    }
    Ok(())
}

pub fn restore(store: &Store, id: &str, to: Option<&str>) -> Result<()> {
    let fs = crate::sync::open_paired(store)?;
    let at = fs.restore(id, to)?;
    println!("Restored to {at}. The next `oneup sync` sends it to OneDrive too.");
    Ok(())
}

/// After a sync: applies the automatic rule and says what it removed.
pub fn auto_trim(fs: &oneup_effects::LocalFs, store: &Store) -> Result<Vec<Slot>> {
    let removed = fs.trim_versions(&trim_settings(store)?)?;
    if !removed.is_empty() {
        let total: u64 = removed.iter().map(|s| s.bytes).sum();
        println!(
            "Version store trimmed: {} old items ({}) removed for good (see `oneup versions`).",
            removed.len(),
            size(total)
        );
    }
    Ok(removed)
}

/// For the activity log: what was removed for good (`None`: nothing).
pub fn removed_detail(removed: &[Slot]) -> Option<String> {
    if removed.is_empty() {
        return None;
    }
    let total: u64 = removed.iter().map(|s| s.bytes).sum();
    let names: Vec<&str> = removed.iter().take(5).map(|s| s.origin.as_deref().unwrap_or(s.name.as_str())).collect();
    let more = if removed.len() > 5 { format!(" and {} more", removed.len() - 5) } else { String::new() };
    Some(format!("{} kept items ({}) removed for good: {}{more}", removed.len(), size(total), names.join(", ")))
}

fn print_slot(s: &Slot) {
    let age = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|n| (n.as_millis() as i64 - s.at_ms).max(0) / 1000)
        .unwrap_or(0);
    let age = match age {
        a if a < 3600 => format!("{} min ago", a / 60),
        a if a < 86_400 => format!("{} h ago", a / 3600),
        a => format!("{} days ago", a / 86_400),
    };
    let what = s.origin.clone().unwrap_or_else(|| s.name.clone());
    println!("  {}  {:>12}  {:>9}  {}{}", s.id, age, size(s.bytes), what, if s.is_dir { "\\" } else { "" });
}

/// After a round's review, with rotation on: removes the oldest kept items
/// so that what the round will keep fits, and says so.
pub fn make_room(fs: &oneup_effects::LocalFs, trim: &Trim, need: u64) -> Result<Vec<Slot>> {
    let removed = fs.make_room(trim, need)?;
    if !removed.is_empty() {
        let total: u64 = removed.iter().map(|s| s.bytes).sum();
        println!(
            "Version store full: the {} oldest items ({}) were removed for good to make room (rotation is on).",
            removed.len(),
            size(total)
        );
    }
    Ok(removed)
}

pub fn size(b: u64) -> String {
    match b {
        b if b < 1024 => format!("{b} B"),
        b if b < 1 << 20 => format!("{:.1} KiB", b as f64 / 1024.0),
        b if b < 1 << 30 => format!("{:.1} MiB", b as f64 / (1u64 << 20) as f64),
        b => format!("{:.1} GiB", b as f64 / (1u64 << 30) as f64),
    }
}
