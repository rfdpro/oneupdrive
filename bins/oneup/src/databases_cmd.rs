//! `oneup databases`: which files count as databases, whether they wait
//! for a consistent state, and what waits now (`oneup_app::databases`).

use std::path::Path;

use anyhow::{Result, bail};
use oneup_app::config::{self, DatabaseSettings};
use oneup_rules::database::{DATABASE_PRESETS, database_preset};
use oneup_store::Store;
use serde_json::json;

/// What the command was asked to change.
#[derive(Debug, Default)]
pub struct Changes {
    pub wait: Option<bool>,
    pub on: Vec<String>,
    pub off: Vec<String>,
    pub add: Vec<String>,
    pub remove: Vec<String>,
    pub exclude: Vec<String>,
    pub keep_syncing: Vec<String>,
}

impl Changes {
    fn any(&self) -> bool {
        self.wait.is_some()
            || !(self.on.is_empty()
                && self.off.is_empty()
                && self.add.is_empty()
                && self.remove.is_empty()
                && self.exclude.is_empty()
                && self.keep_syncing.is_empty())
    }
}

/// Applies `c` to `d` (the `[databases]` section) and `rules` (`[rules]
/// exclude`, for `--exclude`). `paths` turns a typed path into one
/// relative to the synced folder (`/Mail/archive.pst`).
fn apply(
    d: &mut DatabaseSettings,
    rules: &mut config::Rules,
    c: &Changes,
    paths: &dyn Fn(&str) -> Result<String>,
) -> Result<()> {
    if let Some(w) = c.wait {
        d.wait = w;
    }
    for p in &c.on {
        let Some(preset) = database_preset(p) else {
            let known: Vec<&str> = DATABASE_PRESETS.iter().map(|p| p.name).collect();
            bail!("no database preset called `{p}` (there are: {})", known.join(", "));
        };
        if !d.presets.iter().any(|x| x == preset.name) {
            d.presets.push(preset.name.into());
        }
    }
    for p in &c.off {
        d.presets.retain(|x| !x.eq_ignore_ascii_case(p));
    }
    for r in &c.add {
        config::check_rule(r)?;
        if !d.patterns.contains(r) {
            d.patterns.push(r.clone());
        }
    }
    for r in &c.remove {
        if !d.patterns.contains(r) {
            bail!("`{r}` isn't one of your database patterns (`oneup databases` lists them)");
        }
        d.patterns.retain(|x| x != r);
    }
    for p in &c.exclude {
        let rel = paths(p)?;
        // The file itself, its name taken literally.
        let Some(rule) = oneup_rules::literal_path_rules(&rel).pop() else { bail!("{p} is the synced folder itself") };
        if !rules.exclude.contains(&rule) {
            rules.exclude.push(rule);
        }
    }
    for p in &c.keep_syncing {
        let rel = paths(p)?;
        if !oneup_app::databases::kept_syncing(&d.keep_syncing, &rel) {
            d.keep_syncing.push(rel);
        }
    }
    Ok(())
}

pub async fn run(data_dir: &Path, store: &Store, c: &Changes, as_json: bool) -> Result<()> {
    let mut d = config::load_databases(data_dir)?;
    if c.any() {
        let mut rules = config::load_rules(data_dir, store)?;
        let before = (d.clone(), rules.exclude.clone());
        let pair = oneup_app::sync::pairing(store)?;
        let paths =
            |p: &str| -> Result<String> { Ok(crate::activity::normalize(&crate::activity::within(&pair.local, p)?)) };
        apply(&mut d, &mut rules, c, &paths)?;
        if (d.clone(), rules.exclude.clone()) != before {
            config::save_databases(data_dir, &d)?;
            if rules.exclude != before.1 {
                config::save_rules(data_dir, &rules)?;
            }
            if !as_json {
                println!("Saved in {}.", config::path(data_dir).display());
            }
            if crate::agent::nudge(data_dir).await && !as_json {
                println!("The background agent uses it from its next round.");
            }
        }
    }
    let notes = match oneup_ipc::connect(data_dir).await {
        Ok(mut client) => client.call(&oneup_ipc::Call::Status).await.map(|s| s.databases).ok(),
        Err(_) => None,
    };
    if as_json {
        let v = json!({
            "wait": d.wait,
            "presets": d.presets,
            "patterns": d.patterns,
            "keep_syncing": d.keep_syncing,
            "agent_running": notes.is_some(),
            "notes": notes.unwrap_or_default(),
        });
        println!("{}", serde_json::to_string_pretty(&v)?);
        return Ok(());
    }
    let wait = if d.wait {
        "on: sent only while closed (SQLite: once consistent), replaced only while closed"
    } else {
        "off: KeePass, Access and the rest sync while open; SQLite databases still wait until consistent"
    };
    println!("Databases          {wait}  (--wait on|off)");
    println!("Recognised         SQLite, KeePass, Access, Thunderbird mail, Outlook data files, virtual disks");
    println!("Presets            (--on/--off PRESET)");
    for p in DATABASE_PRESETS {
        let on = if d.presets.iter().any(|x| x == p.name) { "on " } else { "off" };
        println!("  {on} {:<12} {} ({})", p.name, p.summary, p.lines.join(", "));
    }
    println!("Your patterns      (--add/--remove PATTERN)");
    for r in &d.patterns {
        println!("  {r}");
    }
    if d.patterns.is_empty() {
        println!("  none");
    }
    for k in &d.keep_syncing {
        println!("Kept syncing       {k}");
    }
    match notes {
        None => println!("Waiting            unknown: the background agent isn't running"),
        Some(n) if n.is_empty() => println!("Waiting            nothing"),
        Some(n) => {
            for note in n {
                println!("  {}: {}", note.path, oneup_app::databases::detail(&note));
            }
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used)]
    use super::*;

    #[test]
    fn changes_turn_presets_and_patterns_on_and_off_and_exclude_one_file() {
        let (mut d, mut r) = (DatabaseSettings::default(), config::Rules::default());
        let paths = |p: &str| Ok(crate::activity::normalize(p));
        let c = Changes {
            wait: Some(false),
            on: vec!["FINANCE".into()],
            add: vec!["*.ledger".into()],
            exclude: vec!["Mail\\archive[1].pst".into()],
            keep_syncing: vec!["/vm/disk.vhdx".into()],
            ..Changes::default()
        };
        apply(&mut d, &mut r, &c, &paths).unwrap();
        assert!(!d.wait);
        assert_eq!(d.presets, ["finance"]);
        assert_eq!(d.patterns, ["*.ledger"]);
        assert_eq!(r.exclude, ["/Mail/archive[[]1].pst"]);
        assert_eq!(d.keep_syncing, ["/vm/disk.vhdx"]);
        let c = Changes { off: vec!["finance".into()], remove: vec!["*.ledger".into()], ..Changes::default() };
        apply(&mut d, &mut r, &c, &paths).unwrap();
        assert!(d.presets.is_empty() && d.patterns.is_empty());
        assert!(apply(&mut d, &mut r, &Changes { on: vec!["nope".into()], ..Changes::default() }, &paths).is_err());
        assert!(apply(&mut d, &mut r, &Changes { remove: vec!["x".into()], ..Changes::default() }, &paths).is_err());
    }
}
