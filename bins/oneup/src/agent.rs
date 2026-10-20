//! `oneup status/pause/resume/approve/keep/discard/sync-now`: talking to the
//! running background agent (`oneupd`) through its control pipe.

use std::path::Path;

use anyhow::{Result, bail};
use oneup_app::control::now_ms;
use oneup_app::versions::size;
use oneup_ipc::{Activity, Answer, Call, ClientError, Direction, Status};

/// Makes one call to the agent of `data_dir`.
async fn call(data_dir: &Path, c: Call) -> Result<Status, ClientError> {
    let mut client = oneup_ipc::connect(data_dir).await?;
    client.call(&c).await
}

fn not_running(what: &str) -> anyhow::Error {
    anyhow::anyhow!("oneupd isn't running for this data folder, so {what}")
}

pub async fn pause(data_dir: &Path, direction: Direction, minutes: Option<u64>) -> Result<()> {
    match call(data_dir, Call::Pause { direction, minutes }).await {
        Ok(s) => {
            print_status(&s);
            Ok(())
        }
        Err(ClientError::NotRunning) => Err(not_running("nothing is syncing in the background to pause")),
        Err(e) => Err(e.into()),
    }
}

pub async fn resume(data_dir: &Path, direction: Direction) -> Result<()> {
    match call(data_dir, Call::Resume { direction }).await {
        Ok(s) => {
            print_status(&s);
            Ok(())
        }
        Err(ClientError::NotRunning) => Err(not_running("there's nothing to resume")),
        Err(e) => Err(e.into()),
    }
}

pub async fn answer(data_dir: &Path, code: u64, answer: Answer) -> Result<()> {
    let code = format!("{code:016x}");
    match call(data_dir, Call::Answer { code: code.clone(), answer }).await {
        Ok(_) => {
            println!(
                "{} oneupd goes ahead now; `oneup status` shows how it went.",
                match answer {
                    Answer::Approve => "Approved.",
                    Answer::Keep => "The files will be kept in the version store.",
                    Answer::Discard => "The files will be removed without a local copy.",
                    Answer::Prune => "The excluded items will be removed from OneDrive (into its recycle bin).",
                    Answer::KeepInCloud =>
                        "The excluded items stay in OneDrive and won't be offered for removal again.",
                }
            );
            Ok(())
        }
        Err(ClientError::NotRunning) => {
            let flag = match answer {
                Answer::Approve => "approve",
                Answer::Keep => "keep",
                Answer::Discard => "discard",
                Answer::Prune => return Err(not_running("use `oneup cloud prune --yes` instead")),
                Answer::KeepInCloud => return Err(not_running("use `oneup cloud prune --keep` instead")),
            };
            Err(not_running(&format!("give the code to `oneup sync --{flag} {code}` instead")))
        }
        Err(ClientError::Rpc(e)) => bail!("{e}"),
        Err(e) => Err(e.into()),
    }
}

pub async fn sync_now(data_dir: &Path, first: Option<String>) -> Result<()> {
    let c = match &first {
        Some(path) => Call::SyncFirst { path: path.clone() },
        None => Call::SyncNow,
    };
    match call(data_dir, c).await {
        Ok(s) => {
            if s.paused.local && s.paused.remote {
                println!("oneupd is paused; `oneup resume` first.");
            } else if let Some(p) = first {
                println!("oneupd is checking now, and transfers /{p} before anything else.");
            } else {
                println!("oneupd is checking both sides for changes now.");
            }
            Ok(())
        }
        Err(ClientError::NotRunning) => Err(not_running("run `oneup sync` instead")),
        Err(e) => Err(e.into()),
    }
}

/// Asks a running agent to look again now (after a settings change).
/// Whether one was running.
pub async fn nudge(data_dir: &Path) -> bool {
    call(data_dir, Call::SyncNow).await.is_ok()
}

/// Waits (bounded) until the agent answers or is gone.
async fn wait_for(data_dir: &Path, running: bool, limit: std::time::Duration) -> bool {
    let started = std::time::Instant::now();
    loop {
        let up = oneup_ipc::connect(data_dir).await.is_ok();
        if up == running {
            return true;
        }
        if started.elapsed() > limit {
            return false;
        }
        tokio::time::sleep(std::time::Duration::from_millis(200)).await;
    }
}

/// `oneup start`: the agent, hidden, without keeping this window busy.
#[cfg(windows)]
pub async fn start(data_dir: &Path) -> Result<()> {
    start_with(data_dir, None).await
}

/// [`start`]; for a vault pair, with its keys (else they must be remembered).
pub async fn start_with(data_dir: &Path, key: Option<&oneup_vault::key::Masterkey>) -> Result<()> {
    oneup_app::setup::require_test_account(data_dir)?;
    if let Ok(s) = call(data_dir, Call::Status).await {
        println!("oneupd is already running.");
        print_status(&s);
        oneup_app::launch::start_tray_if_wanted(data_dir);
        return Ok(());
    }
    let mut child = match key {
        Some(k) => oneup_app::launch::start_vault_agent(data_dir, k)?,
        None => oneup_app::launch::start_agent(data_dir)?,
    };
    // Up as soon as its control pipe answers; or it ended (say, not signed in).
    let started = std::time::Instant::now();
    loop {
        if let Ok(s) = call(data_dir, Call::Status).await {
            println!("oneupd started (output in {}).", data_dir.join(oneup_app::launch::LOG_FILE).display());
            print_status(&s);
            oneup_app::launch::start_tray_if_wanted(data_dir);
            return Ok(());
        }
        if let Some(code) = child.try_wait()? {
            let log = std::fs::read_to_string(data_dir.join(oneup_app::launch::LOG_FILE)).unwrap_or_default();
            bail!("oneupd ended at once ({code}):\n{}", log.trim());
        }
        if started.elapsed() > std::time::Duration::from_secs(30) {
            bail!("oneupd started but doesn't answer; see {}", data_dir.join(oneup_app::launch::LOG_FILE).display());
        }
        tokio::time::sleep(std::time::Duration::from_millis(200)).await;
    }
}

/// `oneup stop`: the agent stops after its current round.
pub async fn stop(data_dir: &Path) -> Result<()> {
    match call(data_dir, Call::Stop).await {
        Ok(_) => {}
        Err(ClientError::NotRunning) => {
            println!("oneupd isn't running for this data folder.");
            return Ok(());
        }
        Err(e) => return Err(e.into()),
    }
    println!("Stopping oneupd (after its current step) …");
    if wait_for(data_dir, false, std::time::Duration::from_secs(120)).await {
        println!("oneupd stopped.");
        Ok(())
    } else {
        bail!("oneupd is still busy; it stops once its current step is done (`oneup status` shows it)")
    }
}

/// `oneup autostart on|off`, and the state with neither.
#[cfg(windows)]
pub fn autostart(data_dir: &Path, on: Option<bool>) -> Result<()> {
    use oneup_app::launch::{RUN_KEY, autostart, set_autostart, sibling};
    match on {
        Some(true) => {
            oneup_app::setup::require_test_account(data_dir)?;
            let launcher = sibling("oneupd-start.exe")?;
            set_autostart(RUN_KEY, data_dir, Some(&launcher))?;
            println!("oneupd will start when you sign in to Windows (`oneup autostart off` to undo).");
        }
        Some(false) => {
            if set_autostart(RUN_KEY, data_dir, None)? {
                println!("oneupd won't start at sign-in any more.");
            } else {
                println!("oneupd wasn't set to start at sign-in.");
            }
        }
        None => match autostart(RUN_KEY, data_dir)? {
            Some(cmd) => println!("oneupd starts when you sign in to Windows:\n  {cmd}"),
            None => println!("oneupd doesn't start at sign-in (`oneup autostart on` to change that)."),
        },
    }
    Ok(())
}

/// The running agent's status; `None` if it isn't running.
pub async fn current(data_dir: &Path) -> Result<Option<Status>> {
    match call(data_dir, Call::Status).await {
        Ok(s) => Ok(Some(s)),
        Err(ClientError::NotRunning) => Ok(None),
        Err(e) => Err(e.into()),
    }
}

/// The agent's part of `oneup status`.
pub async fn status(data_dir: &Path) -> Result<()> {
    match call(data_dir, Call::Status).await {
        Ok(s) => print_status(&s),
        Err(ClientError::NotRunning) => match oneup_app::problem::load_stopped(data_dir) {
            Some(p) => {
                println!("Background agent (oneupd): stopped by a problem");
                print_problem(&p);
                println!();
            }
            None => println!("Background agent (oneupd): not running\n"),
        },
        Err(e) => println!("Background agent (oneupd): can't be reached ({e})\n"),
    }
    Ok(())
}

fn ago(ms: i64) -> String {
    let s = ((now_ms() - ms) / 1000).max(0);
    match s {
        s if s < 60 => format!("{s} s ago"),
        s if s < 3600 => format!("{} min ago", s / 60),
        s => format!("{} h {} min ago", s / 3600, s % 3600 / 60),
    }
}

fn remaining(until: i64) -> String {
    // Whole minutes, rounded up: "30 min" right after `--for 30m`.
    let m = ((until - now_ms()).max(0) + 59_999) / 60_000;
    if m < 60 { format!("{m} min") } else { format!("{} h {} min", m / 60, m % 60) }
}

fn print_status(s: &Status) {
    let activity = match s.activity {
        Activity::Starting => "starting (looking at every folder once)",
        Activity::Syncing => "syncing",
        Activity::Idle if s.overflow.as_ref().is_some_and(|o| o.waiting) => {
            "up to date, apart from files waiting for your decision (see below)"
        }
        Activity::Idle if s.problem.is_some() => "has a problem (see below)",
        Activity::Idle if s.paused.local || s.paused.remote => "running, with one direction paused (see below)",
        Activity::Idle if s.unsynced.is_some() => "up to date, apart from items that can't sync (see below)",
        Activity::Idle if s.retrying.is_some() => "up to date, apart from failed items (see below)",
        Activity::Idle => "up to date, waiting for changes",
        Activity::Paused => "paused",
        Activity::WaitingForYou => "waiting for you (see below); nothing changes until then",
        Activity::Retrying if s.problem.is_some() => "has a problem (see below)",
        Activity::Retrying => "will retry some items shortly",
        Activity::Stopping => "stopping after the current step",
    };
    println!("Background agent (oneupd {}): {activity}", s.version);
    println!("  folders           {}  <->  OneDrive {}", s.local, s.remote);
    if let Some(p) = &s.progress {
        println!("  progress          {}", oneup_app::tray::progress_line(p));
    }
    if s.paused.local || s.paused.remote {
        let what = match (s.paused.local, s.paused.remote) {
            (true, true) => "everything",
            (true, false) => "changes to this PC (OneDrive still gets yours)",
            _ => "changes to OneDrive (this PC still gets OneDrive's)",
        };
        let until = s.paused.until_ms.map(|u| format!(", for another {}", remaining(u))).unwrap_or_default();
        println!("  paused            {what}{until}  (`oneup resume`)");
    }
    if let Some(why) = &s.auto_pause {
        let why = oneup_app::autopause::said(why, s.auto_pause_hours.as_deref(), oneup_app::l10n::current());
        println!("  paused by itself  {why}; resumes when that ends (`oneup settings` to change this)");
    }
    if let Some(n) = &s.network
        && !n.profile.is_empty()
    {
        let name = if n.name.is_empty() { "this network" } else { n.name.as_str() };
        let metered = if n.metered { ", metered" } else { "" };
        let profile = match n.profile.as_str() {
            "own" => "its own settings",
            "metered" => "the metered profile",
            _ => "the default profile",
        };
        println!("  network           {name}{metered}: {} ({profile}; `oneup network`)", n.transfer);
    }
    if let Some(w) = s.network.as_ref().and_then(|n| n.waiting.as_ref()) {
        println!(
            "  waiting           {} files ({}): {}  (`oneup network waiting` lists them)",
            w.count,
            size(w.bytes),
            w.why
        );
    }
    let interval = s.network.as_ref().map_or(0, |n| n.interval_min);
    println!(
        "  OneDrive changes  {}",
        match (s.push, interval) {
            (true, _) => "notified as they happen",
            (false, 0) => "checked on a timer",
            (false, _) => "checked with each pass",
        }
    );
    if interval > 0 {
        let next = s.network.as_ref().and_then(|n| n.next_ms).map(|t| format!("; next in {}", remaining(t)));
        println!(
            "  sync interval     every {interval} min{}  (`oneup settings --sync-interval`)",
            next.unwrap_or_default()
        );
    }
    if let Some(c) = s.checked_ms {
        println!("  last check        {}", ago(c));
    }
    if let Some(l) = &s.last {
        println!(
            "  last changes      {}: {} done, {} skipped, {} failed, {} held",
            ago(l.at_ms),
            l.done,
            l.skipped,
            l.failed,
            l.held
        );
    }
    if let Some(u) = &s.unsynced {
        let more = if u.more { " or more" } else { "" };
        println!("  can't sync        {} items{more} until you change them  (`oneup unsynced` lists them)", u.count);
    }
    if let Some(r) = &s.retrying {
        println!(
            "  failed            {} items; the next is tried again in {}  (`oneup activity` says why)",
            r.count,
            remaining(r.next_ms)
        );
    }
    for d in &s.databases {
        let label = if d.why == "live" { "database" } else { "waiting" };
        println!("  {label:<17} {}: {}", d.path, oneup_app::databases::detail(d));
    }
    if s.databases_more > 0 {
        println!("  waiting           {} more databases (`oneup activity` lists them)", s.databases_more);
    }
    for st in &s.safety_stops {
        let side = if st.side == "local" { "on this PC" } else { "in OneDrive" };
        let earlier =
            if st.recent > 0 { format!(", on top of {} in the last 15 minutes", st.recent) } else { String::new() };
        println!(
            "  SAFETY STOP       {} items {side} would be deleted, overwritten or moved{earlier} ({} are synced).\n                    \
             If that's what you meant: `oneup approve {}`",
            st.items(),
            st.synced_items,
            st.code
        );
    }
    if let Some(o) = &s.overflow
        && o.waiting
    {
        println!(
            "  VERSION STORE     {} removed files ({}) don't fit (room for {}); they're untouched on this PC.\n                    \
             `oneup keep {}` keeps them anyway, `oneup discard {}` removes them without a local copy",
            o.count,
            size(o.bytes),
            size(o.room),
            o.code,
            o.code
        );
    }
    if let Some(o) = &s.prune_offer {
        // OneDrive reports a folder's size some time after a change: 0
        // then means "not known yet".
        let bytes = if o.bytes > 0 { format!(", {}", size(o.bytes)) } else { String::new() };
        println!(
            "  IN ONEDRIVE       {} items{bytes} excluded by your rules are still in OneDrive.\n                    \
             `oneup cloud prune` lists them; `oneup cloud prune --yes` removes them (the copies here stay)",
            o.items
        );
    }
    if let Some(p) = &s.problem {
        print_problem(p);
    }
    println!();
}

/// A problem, as the settings window tells it: what, what to do, when it's
/// tried again, and the technical text last.
pub fn print_problem(p: &oneup_ipc::Problem) {
    const PAD: &str = "                    ";
    println!("  PROBLEM           {}. {}", p.title, p.detail);
    println!("{PAD}{}", p.action);
    match p.retry_ms {
        Some(r) => println!("{PAD}Trying again in {} (`oneup sync-now` tries now).", remaining(r)),
        None => println!("{PAD}OneUpDrive stopped (`oneup start` starts it again)."),
    }
    println!("{PAD}Details: {}", p.technical);
}

/// `30m`, `2h`, `1d` or a number of minutes, as minutes.
pub fn parse_minutes(s: &str) -> Result<u64, String> {
    let s = s.trim().to_ascii_lowercase();
    let (num, unit) = match s.find(|c: char| !c.is_ascii_digit()) {
        Some(i) => (&s[..i], s[i..].trim()),
        None => (s.as_str(), "m"),
    };
    let n: u64 = num.parse().map_err(|_| "expected a duration like 30m, 2h or 1d".to_string())?;
    let per = match unit {
        "m" | "min" | "mins" | "minute" | "minutes" => 1,
        "h" | "hour" | "hours" => 60,
        "d" | "day" | "days" => 1440,
        _ => return Err("expected a duration like 30m, 2h or 1d".into()),
    };
    match n.checked_mul(per) {
        Some(0) => Err("the duration must be at least a minute".into()),
        Some(m) => Ok(m),
        None => Err("that's too long".into()),
    }
}

#[cfg(test)]
mod tests {
    use super::parse_minutes;

    #[test]
    fn durations_parse() {
        assert_eq!(parse_minutes("45"), Ok(45));
        assert_eq!(parse_minutes("30m"), Ok(30));
        assert_eq!(parse_minutes("2h"), Ok(120));
        assert_eq!(parse_minutes("1 day"), Ok(1440));
        assert!(parse_minutes("0h").is_err());
        assert!(parse_minutes("soon").is_err());
        assert!(parse_minutes("5y").is_err());
    }
}
