//! `oneup settings`: how the background agent uses this PC (`config.toml`
//! `[agent]`, `rules.md` § User controls): bandwidth caps, memory profile
//! and its numbers, hashing, priority.

use std::path::Path;

use anyhow::Result;
use oneup_app::config::{self, Agent};

/// The changes asked for on the command line.
#[derive(Debug, Default)]
pub struct Changes {
    pub upload_kib: Option<u64>,
    pub download_kib: Option<u64>,
    pub memory: Option<&'static str>,
    pub background: Option<bool>,
    pub pause_on_metered: Option<bool>,
    pub pause_on_battery_saver: Option<bool>,
    pub pause_on_full_screen: Option<bool>,
    pub pause_on_full_screen_video: Option<bool>,
    /// `HH:MM-HH:MM`, or `off`.
    pub quiet_hours: Option<String>,
    /// `HH:MM-HH:MM`, or `always`.
    pub cap_hours: Option<String>,
    pub tray_icon: Option<bool>,
    /// Files that keep changing (`oneup_app::busy`).
    pub hold_busy_files: Option<bool>,
    pub busy_changes: Option<u32>,
    pub busy_window_s: Option<u64>,
    pub busy_quiet_s: Option<u64>,
    pub busy_max_s: Option<u64>,
    pub compact_database: Option<bool>,
    /// `[dev] experimental` (`config::load_experimental`).
    pub experimental: Option<bool>,
    pub tuning: TuningArgs,
    /// `[sync]` (`oneup_app::pace`): minutes, 0 = off.
    pub sync_interval: Option<u32>,
    pub keep_push: Option<bool>,
}

/// A number instead of the memory profile's, or `profile` for the
/// profile's again.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Override(pub Option<u64>);

pub fn parse_override(s: &str) -> Result<Override, String> {
    match s.trim() {
        "profile" => Ok(Override(None)),
        n => n.parse().map(|n| Override(Some(n))).map_err(|_| "expected a whole number or `profile`".into()),
    }
}

/// `oneup settings`' numbers for memory and hashing (`config::Overrides`).
#[derive(Debug, Clone, Default, clap::Args)]
pub struct TuningArgs {
    /// Database cache in MiB instead of the memory profile's (1 to 1024), or `profile`
    #[arg(long, value_name = "MIB|profile", value_parser = parse_override)]
    pub database_cache_mib: Option<Override>,
    /// Operations at once (transfers and the rest) instead of the memory profile's (1 to 32), or `profile`
    #[arg(long, value_name = "N|profile", value_parser = parse_override)]
    pub operations_at_once: Option<Override>,
    /// Uploads over 4 MiB at once instead of the memory profile's (1 to 8), or `profile`
    #[arg(long, value_name = "N|profile", value_parser = parse_override)]
    pub large_uploads_at_once: Option<Override>,
    /// Largest upload fragment in MiB instead of the memory profile's (2 to 59), or `profile`
    #[arg(long, value_name = "MIB|profile", value_parser = parse_override)]
    pub upload_fragment_mib: Option<Override>,
    /// Files hashed at once, to compare them with OneDrive's (1 to 16; default 1)
    #[arg(long, value_name = "N")]
    pub hash_files_at_once: Option<u64>,
}

impl TuningArgs {
    /// The overrides given, by `config.toml` key.
    fn overrides(&self) -> impl Iterator<Item = (&'static str, Override)> {
        config::OVERRIDE_KEYS
            .into_iter()
            .zip([
                self.database_cache_mib,
                self.operations_at_once,
                self.large_uploads_at_once,
                self.upload_fragment_mib,
            ])
            .filter_map(|(k, v)| v.map(|v| (k, v)))
    }
}

pub async fn run(data_dir: &Path, c: &Changes) -> Result<()> {
    if let Some(on) = c.experimental
        && on != config::load_experimental(data_dir)?
    {
        config::save_experimental(data_dir, on)?;
        println!("Experimental options are {}.", if on { "shown" } else { "hidden" });
    }
    let mut interval = oneup_app::pace::load(data_dir)?;
    let interval_before = interval;
    if let Some(m) = c.sync_interval {
        interval.minutes = m;
    }
    if let Some(b) = c.keep_push {
        interval.keep_push = b;
    }
    if interval != interval_before {
        oneup_app::pace::save(data_dir, &interval)?;
        if crate::agent::nudge(data_dir).await {
            println!("The background agent uses the new sync interval from its next pass.");
        }
    }
    let mut a = config::load_agent(data_dir)?;
    let before = a.clone();
    if let Some(n) = c.upload_kib {
        a.upload_kib = n;
    }
    if let Some(n) = c.download_kib {
        a.download_kib = n;
    }
    if let Some(m) = c.memory {
        a.memory = m.to_string();
    }
    if let Some(b) = c.background {
        a.background = b;
    }
    if let Some(b) = c.pause_on_metered {
        a.pause_on_metered = b;
    }
    if let Some(b) = c.pause_on_battery_saver {
        a.pause_on_battery_saver = b;
    }
    if let Some(b) = c.pause_on_full_screen {
        a.pause_on_full_screen = b;
    }
    if let Some(b) = c.pause_on_full_screen_video {
        a.pause_on_full_screen_video = b;
    }
    if let Some(b) = c.tray_icon {
        a.tray_icon = b;
    }
    if let Some(b) = c.hold_busy_files {
        a.busy.on = b;
    }
    if let Some(n) = c.busy_changes {
        a.busy.changes = n;
    }
    let secs = std::time::Duration::from_secs;
    if let Some(n) = c.busy_window_s {
        a.busy.window = secs(n);
    }
    if let Some(n) = c.busy_quiet_s {
        a.busy.quiet = secs(n);
    }
    if let Some(n) = c.busy_max_s {
        a.busy.max = secs(n);
    }
    config::check_busy(&a.busy)?;
    if let Some(b) = c.compact_database {
        a.compact_database = b;
    }
    for (key, Override(n)) in c.tuning.overrides() {
        a.set_override(key, n)?;
    }
    if let Some(n) = c.tuning.hash_files_at_once {
        a.hash_files_at_once = config::HASH_FILES_AT_ONCE.check(n)?;
    }
    match c.cap_hours.as_deref() {
        Some("always" | "off" | "") => a.cap_hours = None,
        Some(q) => {
            oneup_app::autopause::parse_quiet_hours(q)?;
            a.cap_hours = Some(q.to_string());
        }
        None => {}
    }
    match c.quiet_hours.as_deref() {
        Some("off" | "") => a.quiet_hours = None,
        Some(q) => {
            oneup_app::autopause::parse_quiet_hours(q)?;
            a.quiet_hours = Some(q.to_string());
        }
        None => {}
    }
    if a != before {
        config::save_agent(data_dir, &a)?;
        println!("Saved in {}.", config::path(data_dir).display());
        let restart = a.memory != before.memory
            || a.overrides != before.overrides
            || a.hash_files_at_once != before.hash_files_at_once
            || a.background != before.background
            || a.compact_database != before.compact_database;
        if crate::agent::nudge(data_dir).await {
            println!("The background agent applies the caps and automatic pauses now.");
            if restart {
                println!(
                    "Memory, hashing, priority and compacting the database apply when it next starts \
                     (`oneup stop`, `oneup start`)."
                );
            }
        }
    }
    show(&a, &interval);
    Ok(())
}

fn show(a: &Agent, interval: &oneup_app::pace::Interval) {
    let cap = |n: u64| if n == 0 { "no cap".to_string() } else { format!("{n} KiB/s") };
    println!("Background agent settings (oneup settings --help to change them):");
    println!("  upload          {}", cap(a.upload_kib));
    println!("  download        {}", cap(a.download_kib));
    if a.upload_kib + a.download_kib > 0 {
        println!("  caps apply      {}", a.cap_hours.as_deref().map_or("always".to_string(), |h| format!("{h} only")));
    }
    println!("  memory profile  {}", a.memory);
    for t in a.tuned() {
        let set = if t.set.is_some() { format!("  (set; default {})", t.default) } else { String::new() };
        let indent = if t.knob.key == config::HASH_FILES_AT_ONCE.key { "  " } else { "    " };
        let label = t.knob.name();
        let width = 34 - indent.len();
        println!("{indent}{label:<width$}{}{set}", t.value);
    }
    println!(
        "  priority        {}",
        if a.background {
            "background: your programs come first (lower CPU, disk and memory priority; power-efficient)"
        } else {
            "normal"
        }
    );
    let yes = |b: bool| if b { "yes" } else { "no" };
    println!("  pause on a metered connection   {}", yes(a.pause_on_metered));
    println!("  pause with battery saver on     {}", yes(a.pause_on_battery_saver));
    println!("  pause during games, presentations  {}", yes(a.pause_on_full_screen));
    println!("  pause during full-screen video     {}", yes(a.pause_on_full_screen_video));
    println!("  quiet hours (no syncing)        {}", a.quiet_hours.as_deref().unwrap_or("none"));
    match interval.minutes {
        0 => println!("  sync interval                   off: within seconds of a change"),
        m => println!(
            "  sync interval                   every {m} min; OneDrive's changes {}",
            if interval.keep_push { "at once" } else { "with each pass" }
        ),
    }
    println!("  icon in the notification area   {}", yes(a.tray_icon));
    let b = &a.busy;
    if b.on {
        println!(
            "  files that keep changing        sent once left alone for {} s ({} changes within {} s make a file busy; \
             sent at least every {} s)",
            b.quiet.as_secs(),
            b.changes,
            b.window.as_secs(),
            b.max.as_secs()
        );
    } else {
        println!("  files that keep changing        sent after every change");
    }
    println!("  compact the database at start   {}", yes(a.compact_database));
}
