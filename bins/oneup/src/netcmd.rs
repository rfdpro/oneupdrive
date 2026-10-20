//! `oneup network`: the network this PC is on, the profile in effect, and
//! changing the profiles (`oneup_app::network`).

use std::path::Path;

use anyhow::{Result, bail};
use oneup_app::config;
use oneup_app::network::{self, Changes, Key, Layer, Networks, Target, Transfer};

pub async fn set(dir: &Path, target: &Target, c: &Changes) -> Result<()> {
    network::apply(dir, target, c)?;
    println!("Saved in {}.", config::path(dir).display());
    if crate::agent::nudge(dir).await {
        println!("The background agent uses it from now on.");
    }
    show(dir)
}

pub async fn forget(dir: &Path, target: &Target) -> Result<()> {
    let id = match target {
        Target::Id(id) => id.clone(),
        Target::This => network::this_network()?.0,
        _ => bail!("only a network's own settings can be forgotten (--this or --id)"),
    };
    let mut nets = network::load(dir)?;
    if !nets.forget(&id) {
        bail!("no settings of its own for network {id}");
    }
    network::save(dir, &nets)?;
    println!("Forgot the settings of network {id}.");
    crate::agent::nudge(dir).await;
    show(dir)
}

fn layer_line(l: &Layer) -> String {
    let mut parts = Vec::new();
    if let Some(t) = l.transfer {
        parts.push(t.label(l.small_mb.unwrap_or(network::SMALL_MB)));
    }
    if let Some(n) = l.upload_kib {
        parts.push(format!("upload {}", cap(n)));
    }
    if let Some(n) = l.download_kib {
        parts.push(format!("download {}", cap(n)));
    }
    if let Some(m) = l.interval_minutes {
        parts.push(interval(m));
    }
    if let Some(b) = l.budget_mb.filter(|b| *b > 0) {
        parts.push(format!("budget {b} MB a month"));
    }
    if parts.is_empty() { "as the default".into() } else { parts.join(", ") }
}

fn cap(kib: u64) -> String {
    if kib == 0 { "no cap".into() } else { format!("{kib} KiB/s") }
}

fn interval(m: u32) -> String {
    if m == 0 { "syncs within seconds".into() } else { format!("syncs every {m} min") }
}

/// Shows the network, the profile in effect and the profiles set up.
pub fn show(dir: &Path) -> Result<()> {
    let agent = config::load_agent(dir)?;
    let (n, p) = network::situation(dir, &agent);
    let nets: Networks = network::load(dir)?;
    let name = if n.name.is_empty() { "a network without a name".to_string() } else { n.name.clone() };
    let reach = match n.connectivity {
        oneup_sys::network::Connectivity::Internet | oneup_sys::network::Connectivity::Unknown => "",
        oneup_sys::network::Connectivity::Constrained => "; sign in to it first (no internet yet)",
        oneup_sys::network::Connectivity::LocalOnly => "; no internet",
        oneup_sys::network::Connectivity::None => "; not connected",
    };
    match &n.id {
        Some(id) => {
            println!("This network   {name} {id}, {}{reach}", if n.metered { "metered" } else { "not metered" })
        }
        None => println!("This network   none{reach}"),
    }
    let which = match &p.key {
        Key::Default => "the default profile",
        Key::Metered => "the metered profile",
        Key::Network(_) => "its own settings",
    };
    println!(
        "In use         {which}: {}; upload {}, download {}; {}",
        p.transfer.label(p.small_mb),
        cap(p.upload_kib),
        cap(p.download_kib),
        interval(p.interval.minutes)
    );
    let metered = if agent.pause_on_metered {
        "paused".to_string()
    } else {
        let mut l = nets.metered.clone();
        l.transfer = Some(l.transfer.filter(|t| *t != Transfer::Paused).unwrap_or(Transfer::Everything));
        layer_line(&l)
    };
    println!("Metered        {metered}");
    for (id, l) in &nets.own {
        println!("Network        {} {id}: {}", l.name.as_deref().unwrap_or("(unnamed)"), layer_line(l));
    }
    println!("\n`oneup network set --help` changes these.");
    Ok(())
}
