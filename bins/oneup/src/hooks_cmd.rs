//! `oneup watch` (the agent's events as they happen) and `oneup hooks`
//! (the programs it starts on them): `docs/user-guide/hooks.md`.

use std::io::Write as _;
use std::path::Path;

use anyhow::{Result, bail};
use oneup_ipc::{ClientError, Event};

/// One event, as `oneup watch` shows it.
fn line(e: &Event, offset: time::UtcOffset) -> String {
    let what = match (e.action.is_empty(), e.detail.is_empty()) {
        (true, true) => String::new(),
        (false, true) => format!("  ({})", e.action),
        (true, false) => format!("  ({})", e.detail),
        (false, false) => format!("  ({}; {})", e.action, e.detail),
    };
    let path = &e.path;
    let what = if path.is_empty() { what.trim_start() } else { &what };
    format!("{}  {:<12}{path}{what}", crate::activity::when(e.at_ms, offset), e.kind.name())
}

/// Prints the agent's events as they happen, until it stops (or Ctrl+C):
/// one line each, or one JSON object each with `json`.
pub async fn watch(data_dir: &Path, json: bool) -> Result<()> {
    let mut c = match oneup_ipc::connect(data_dir).await {
        Ok(c) => c,
        Err(ClientError::NotRunning) => bail!("oneupd isn't running for this data folder; `oneup start` starts it"),
        Err(e) => return Err(e.into()),
    };
    let mut since = c.events(None).await?.next;
    if !json {
        eprintln!("Watching. Ctrl+C to stop.");
    }
    let offset = crate::activity::local_offset();
    loop {
        let page = match c.events(Some(since)).await {
            Ok(p) => p,
            Err(ClientError::Rpc(e)) => bail!("oneupd refused: {e}"),
            Err(_) => bail!("oneupd stopped"),
        };
        if page.missed {
            eprintln!("(events missed: more came at once than oneupd keeps; `oneup activity` has them)");
        }
        let mut out = std::io::stdout().lock();
        for e in &page.items {
            let text = if json { serde_json::to_string(e)? } else { line(e, offset) };
            writeln!(out, "{text}")?;
        }
        out.flush()?;
        since = page.next;
    }
}

/// Lets a running agent read `config.toml` again now; nothing if it isn't
/// running (it reads it when it starts).
async fn nudge(data_dir: &Path) {
    if let Ok(mut c) = oneup_ipc::connect(data_dir).await {
        let _ = c.call(&oneup_ipc::Call::SyncNow).await;
    }
}

pub fn list(data_dir: &Path) -> Result<()> {
    let (paused, listed) = oneup_app::hooks::list(data_dir)?;
    if listed.is_empty() {
        println!("No hooks. Add a [[hook]] section to {}.", oneup_app::config::path(data_dir).display());
        return Ok(());
    }
    if paused {
        println!("Hooks paused (`oneup hooks resume`).");
    }
    for l in &listed {
        let state = match (&l.refused, l.spec.enabled) {
            (Some(_), _) => "refused",
            (None, true) => "on",
            (None, false) => "off",
        };
        println!("{} ({state})", l.spec.name);
        let on: Vec<&str> = l.spec.on.iter().map(|k| k.name()).collect();
        let paths = if l.spec.paths.is_empty() { "everything".to_string() } else { l.spec.paths.join(", ") };
        println!("  on: {}; paths: {paths}", on.join(", "));
        let args: Vec<String> =
            l.spec.args.iter().map(|a| if a.contains(' ') { format!("\"{a}\"") } else { a.clone() }).collect();
        println!("  runs: {} {}", l.spec.program, args.join(" "));
        if let Some(why) = &l.refused {
            println!("  refused: {why}");
        }
    }
    println!("Runs and their output: {}", data_dir.join(oneup_app::hooks::LOG_FILE).display());
    Ok(())
}

pub async fn pause(data_dir: &Path, paused: bool) -> Result<()> {
    oneup_app::config::set_hooks_paused(data_dir, paused)?;
    nudge(data_dir).await;
    println!("{}", if paused { "Hooks paused." } else { "Hooks resumed." });
    Ok(())
}

pub async fn enable(data_dir: &Path, name: &str, on: bool) -> Result<()> {
    oneup_app::config::set_hook_enabled(data_dir, name, on)?;
    nudge(data_dir).await;
    println!("{} {}.", name, if on { "on" } else { "off" });
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use oneup_ipc::EventKind;

    #[test]
    fn events_show_as_one_line_each() {
        let e = Event {
            seq: 1,
            at_ms: 0,
            kind: EventKind::Synced,
            path: "/a.txt".into(),
            action: "upload".into(),
            detail: "new version".into(),
        };
        assert_eq!(line(&e, time::UtcOffset::UTC), "1970-01-01 00:00:00  synced      /a.txt  (upload; new version)");
        let e = Event {
            kind: EventKind::Paused,
            path: String::new(),
            action: String::new(),
            detail: "both directions".into(),
            ..e
        };
        assert_eq!(line(&e, time::UtcOffset::UTC), "1970-01-01 00:00:00  paused      (both directions)");
    }
}
