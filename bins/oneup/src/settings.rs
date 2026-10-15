//! `oneup settings`: how the background agent uses this PC (`config.toml`
//! `[agent]`, `rules.md` § User controls): bandwidth caps, memory profile, priority.

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
    /// `[dev] experimental` (`config::load_experimental`).
    pub experimental: Option<bool>,
}

pub async fn run(data_dir: &Path, c: &Changes) -> Result<()> {
    if let Some(on) = c.experimental
        && on != config::load_experimental(data_dir)?
    {
        config::save_experimental(data_dir, on)?;
        println!("Experimental options are {}.", if on { "shown" } else { "hidden" });
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
        let restart = a.memory != before.memory || a.background != before.background;
        if crate::agent::nudge(data_dir).await {
            println!("The background agent applies the caps and automatic pauses now.");
            if restart {
                println!("The memory profile and priority apply when it next starts (`oneup stop`, `oneup start`).");
            }
        }
    }
    show(&a);
    Ok(())
}

fn show(a: &Agent) {
    let cap = |n: u64| if n == 0 { "no cap".to_string() } else { format!("{n} KiB/s") };
    println!("Background agent settings (oneup settings --help to change them):");
    println!("  upload          {}", cap(a.upload_kib));
    println!("  download        {}", cap(a.download_kib));
    if a.upload_kib + a.download_kib > 0 {
        println!("  caps apply      {}", a.cap_hours.as_deref().map_or("always".to_string(), |h| format!("{h} only")));
    }
    println!("  memory profile  {} ({} MiB database cache)", a.memory, a.cache_kib() / 1024);
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
    println!("  icon in the notification area   {}", yes(a.tray_icon));
}
