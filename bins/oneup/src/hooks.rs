//! The agent's event hooks (`docs/architecture/hooks.md`): `config.toml`'s
//! `[[hook]]` sections, run by `oneup_hooks::Runner` on the events of the
//! agent's feed (`crate::events`).
//!
//! * **Where programs may be.** Every folder OneUpDrive syncs on this PC
//!   (this account's pairs and vaults, and every other data folder's) is
//!   found when the hooks load, again when `config.toml` changes and at
//!   most every [`PLACES_EVERY`] otherwise; a hook whose program (or a
//!   script its arguments name) is in one is refused, at load and again
//!   right before each start (`oneup_hooks::program`).
//! * **What it says.** Every run goes to [`LOG_FILE`] in the data folder
//!   (with the program's output), and to the agent's log at debug level.
//!   The activity log gets what needs attention: refusals, failed runs (at
//!   most one entry a minute per hook) and dropped runs.
//! * **Control.** `[hooks] paused` stops them all, `enabled` one; `oneup
//!   hooks`, the settings window and `oneup explain` show them.

use std::collections::HashMap;
use std::io::Write as _;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant, SystemTime};

use anyhow::Result;
use oneup_hooks::program::Places;
use oneup_hooks::{Contained, HookSpec, Note, Ran, Runner, Settings};
use oneup_store::{ActivityEntry, ActivityOutcome};

use crate::config;
use crate::events::Feed;

/// In the data folder: what each run did, with its output.
pub const LOG_FILE: &str = "hooks.log";
/// [`LOG_FILE`] is moved to `hooks.log.1` (replacing it) past this size.
const LOG_MAX: u64 = 1024 * 1024;
/// The synced folders are looked up again at most this often while
/// `config.toml` doesn't change.
pub const PLACES_EVERY: Duration = Duration::from_secs(10 * 60);
/// A hook's failures reach the activity log at most this often.
const FAILURE_EVERY: Duration = Duration::from_secs(60);

/// Every folder OneUpDrive syncs on this PC, with what syncs it: the pairs
/// and vaults of `dir`'s account, then every other data folder's.
pub fn places(dir: &Path) -> Places {
    let main = crate::vaultpair::main_dir(dir);
    let mut out: Vec<(PathBuf, String)> = crate::pairs::local_folders(&main, None).unwrap_or_default();
    for d in crate::datafolders::all() {
        let Ok(store) = crate::setup::open_store(&d, 1024) else { continue };
        let Ok(Some(local)) = store.read(|o| o.meta_get(crate::sync::KEY_LOCAL)) else { continue };
        let local = PathBuf::from(local);
        if !out
            .iter()
            .any(|(p, _)| oneup_local::coexist::is_within(p, &local) && oneup_local::coexist::is_within(&local, p))
        {
            out.push((local, "a folder another OneUpDrive account syncs".into()));
        }
    }
    Places::new(out)
}

/// What `config.toml` says, the last good version if it can't be read now
/// (`config::kept`); none if it never could.
fn load(dir: &Path) -> config::Hooks {
    match config::kept(dir, config::load_hooks(dir)) {
        Ok(h) => h,
        Err(e) => {
            println!("oneupd: hooks: {e:#}");
            config::Hooks::default()
        }
    }
}

fn stamp(dir: &Path) -> Option<SystemTime> {
    std::fs::metadata(config::path(dir)).and_then(|m| m.modified()).ok()
}

/// The hooks of a running agent.
pub struct AgentHooks {
    runner: Arc<Runner>,
    dir: PathBuf,
    root: PathBuf,
    stamp: Option<SystemTime>,
    places: Places,
    places_at: Instant,
}

impl AgentHooks {
    /// Starts the hooks of data folder `dir`, whose synced folder is
    /// `root`, on `feed`'s events.
    pub fn start(dir: &Path, root: &Path, feed: &Feed) -> Self {
        let notes = Notes { dir: dir.to_path_buf(), log: Mutex::new(()), failures: Mutex::default() };
        let places = places(dir);
        let h = load(dir);
        let settings = Settings { hooks: h.list, paused: h.paused, root: root.to_path_buf(), places: places.clone() };
        let runner = Arc::new(Runner::start(settings, Arc::new(Contained), Box::new(move |n| notes.note(n))));
        let offer = Arc::clone(&runner);
        feed.add_sink(Arc::new(move |e| offer.offer(e)));
        Self {
            runner,
            dir: dir.to_path_buf(),
            root: root.to_path_buf(),
            stamp: stamp(dir),
            places,
            places_at: Instant::now(),
        }
    }

    /// Reads the hooks again if `config.toml` changed (called at each of
    /// the agent's wake-ups: a file's time, nothing more, unless it did),
    /// and the synced folders if they're old.
    pub fn refresh(&mut self) {
        let now = stamp(&self.dir);
        let old_places = self.places_at.elapsed() >= PLACES_EVERY;
        if now == self.stamp && !old_places {
            return;
        }
        if now != self.stamp || old_places {
            self.places = places(&self.dir);
            self.places_at = Instant::now();
        }
        self.stamp = now;
        let h = load(&self.dir);
        self.runner.configure(Settings {
            hooks: h.list,
            paused: h.paused,
            root: self.root.clone(),
            places: self.places.clone(),
        });
    }
}

/// Where the runner's notes go (see the module docs).
struct Notes {
    dir: PathBuf,
    log: Mutex<()>,
    /// Per hook: when a failure last reached the activity log, and how
    /// many came since.
    failures: Mutex<HashMap<String, (Instant, u64)>>,
}

fn utc(at: SystemTime) -> String {
    let fmt = time::macros::format_description!("[year]-[month]-[day]T[hour]:[minute]:[second]Z");
    time::OffsetDateTime::from(at).format(fmt).unwrap_or_default()
}

/// How a run ended, in a few words.
pub fn outcome(r: &Ran) -> (bool, String) {
    match &r.result {
        Ok(f) if f.timed_out => (false, format!("ended after {} s (its timeout)", r.took.as_secs())),
        Ok(f) => match f.exit_code {
            Some(0) => (true, "exit 0".into()),
            Some(c) => (false, format!("exit {c}")),
            None => (false, "ended without an exit code".into()),
        },
        Err(e) => (false, e.clone()),
    }
}

impl Notes {
    fn note(&self, n: Note) {
        match n {
            Note::Ran(r) => {
                let (ok, how) = outcome(&r);
                // Every run is in hooks.log; the agent's log has them at
                // debug level only, so a burst doesn't flood it.
                tracing::debug!(hook = %r.hook, event = r.event.kind.name(), ok, ms = r.took.as_millis() as u64, "hook ran");
                tracing::debug!(hook = %r.hook, path = %r.event.path, "hook ran for");
                let output = r.result.as_ref().map(|f| (f.output.clone(), f.truncated)).unwrap_or_default();
                let merged =
                    if r.merged > 0 { format!(" ({} more events folded in)", r.merged) } else { String::new() };
                self.write(
                    &format!(
                        "{}  {}  {} {}  {how}  {} ms{merged}",
                        utc(SystemTime::now()),
                        r.hook,
                        r.event.kind.name(),
                        r.event.path,
                        r.took.as_millis()
                    ),
                    &output.0,
                    output.1,
                );
                if !ok {
                    self.failed(&r, &how);
                }
            }
            Note::Refused { hook, why } => {
                println!("oneupd: hook {hook} doesn't run: {why}");
                self.write(&format!("{}  {hook}  refused: {why}", utc(SystemTime::now())), &[], false);
                self.activity(ActivityOutcome::Failed, String::new(), format!("hook {hook} doesn't run: {why}"));
            }
            Note::Dropped { count } => {
                let detail = format!("{count} hook runs dropped: {} were waiting already", oneup_hooks::MAX_PENDING);
                println!("oneupd: {detail}");
                self.write(&format!("{}  {detail}", utc(SystemTime::now())), &[], false);
                self.activity(ActivityOutcome::Failed, String::new(), detail);
            }
        }
    }

    /// A failed run, in the activity log at most once a minute per hook.
    fn failed(&self, r: &Ran, how: &str) {
        let more = {
            let mut f = self.failures.lock().unwrap_or_else(std::sync::PoisonError::into_inner);
            let entry = f.entry(r.hook.clone()).or_insert((Instant::now() - FAILURE_EVERY * 2, 0));
            if entry.0.elapsed() < FAILURE_EVERY {
                entry.1 += 1;
                return;
            }
            let more = entry.1;
            *entry = (Instant::now(), 0);
            more
        };
        let also = if more > 0 { format!(" ({more} more failures before this)") } else { String::new() };
        self.activity(
            ActivityOutcome::Failed,
            r.event.path.clone(),
            format!("hook {} on {}: {how}{also}; see {LOG_FILE}", r.hook, r.event.kind.name()),
        );
    }

    fn activity(&self, outcome: ActivityOutcome, path: String, detail: String) {
        let e = ActivityEntry { seq: 0, at_ms: crate::control::now_ms(), outcome, action: "hook".into(), path, detail };
        let logged = crate::setup::open_store(&self.dir, 1024).and_then(|mut s| Ok(s.write(|o| o.activity_add(&[e]))?));
        if let Err(e) = logged {
            tracing::warn!(error = %format!("{e:#}"), "a hook's note didn't reach the activity log");
        }
    }

    /// Appends to [`LOG_FILE`] (OneUpDrive's own file), moving it aside
    /// first once it's large.
    fn write(&self, line: &str, output: &[u8], truncated: bool) {
        let _one = self.log.lock().unwrap_or_else(std::sync::PoisonError::into_inner);
        let file = self.dir.join(LOG_FILE);
        if std::fs::metadata(&file).is_ok_and(|m| m.len() > LOG_MAX) {
            let _ = std::fs::rename(&file, self.dir.join(format!("{LOG_FILE}.1")));
        }
        let mut text = format!("{line}\n");
        for l in String::from_utf8_lossy(output).lines() {
            text.push_str("    ");
            text.push_str(l);
            text.push('\n');
        }
        if truncated {
            text.push_str("    (more output not kept)\n");
        }
        let written = std::fs::OpenOptions::new()
            .create(true)
            .append(true)
            .open(&file)
            .and_then(|mut f| f.write_all(text.as_bytes()));
        if let Err(e) = written {
            tracing::warn!(error = %e, "can't write {LOG_FILE}");
        }
    }
}

/// A hook as `oneup hooks` and the settings window show it.
#[derive(Debug, Clone)]
pub struct Listed {
    pub spec: HookSpec,
    /// Why it doesn't run, if it can't.
    pub refused: Option<String>,
}

/// The hooks, and whether they're paused, each checked as the agent would.
pub fn list(dir: &Path) -> Result<(bool, Vec<Listed>)> {
    let h = config::load_hooks(dir)?;
    let places = places(dir);
    let listed = h
        .list
        .into_iter()
        .map(|spec| {
            let refused = oneup_hooks::check(&spec, &places).err();
            Listed { spec, refused }
        })
        .collect();
    Ok((h.paused, listed))
}

/// For `oneup explain`: the hooks the item at `rel` (relative to the synced
/// folder) starts, one line each.
pub fn explain(dir: &Path, rel: &str) -> Result<Vec<String>> {
    let (paused, listed) = list(dir)?;
    let path = format!("/{}", rel.trim_start_matches('/'));
    let mut out = Vec::new();
    for l in listed {
        let kinds: Vec<&str> = l.spec.on.iter().filter(|k| k.has_path()).map(|k| k.name()).collect();
        if kinds.is_empty() || rel.is_empty() {
            continue;
        }
        let hit = l.spec.paths.is_empty()
            || oneup_rules::RuleSet::new(&[] as &[String], &l.spec.paths).is_ok_and(|r| r.excludes(&path, false));
        if !hit {
            continue;
        }
        let state = match (&l.refused, l.spec.enabled, paused) {
            (Some(why), ..) => format!(" (refused: {why})"),
            (None, false, _) => " (off)".into(),
            (None, true, true) => " (hooks paused)".into(),
            (None, true, false) => String::new(),
        };
        out.push(format!("hook {} runs on {}{state}", l.spec.name, kinds.join(", ")));
    }
    Ok(out)
}

/// The hooks for the settings window.
pub fn json(dir: &Path) -> Result<serde_json::Value> {
    let (paused, listed) = list(dir)?;
    let hooks: Vec<serde_json::Value> = listed
        .iter()
        .map(|l| {
            serde_json::json!({
                "name": l.spec.name,
                "on": l.spec.on.iter().map(|k| k.name()).collect::<Vec<_>>(),
                "paths": l.spec.paths,
                "program": l.spec.program,
                "enabled": l.spec.enabled,
                "refused": l.refused,
            })
        })
        .collect();
    Ok(serde_json::json!({ "paused": paused, "hooks": hooks, "log": dir.join(LOG_FILE).to_string_lossy() }))
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used)]
    use super::*;
    use oneup_ipc::{Event, EventKind};

    fn ran(code: Option<i32>, timed_out: bool) -> Ran {
        Ran {
            hook: "h".into(),
            event: Event {
                seq: 1,
                at_ms: 0,
                kind: EventKind::Synced,
                path: "/a.txt".into(),
                action: "upload".into(),
                detail: String::new(),
            },
            merged: 0,
            took: Duration::from_secs(61),
            result: Ok(oneup_hooks::Finished { exit_code: code, timed_out, output: b"hi\n".to_vec(), truncated: true }),
        }
    }

    #[test]
    fn runs_are_written_to_the_hooks_log_and_failures_to_the_activity_log_once_a_minute() {
        let dir = std::env::temp_dir().join(format!("oneup-app-hooks-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        let notes = Notes { dir: dir.clone(), log: Mutex::new(()), failures: Mutex::default() };
        assert_eq!(outcome(&ran(Some(0), false)), (true, "exit 0".into()));
        assert_eq!(outcome(&ran(None, true)).1, "ended after 61 s (its timeout)");
        notes.note(Note::Ran(ran(Some(0), false)));
        notes.note(Note::Ran(ran(Some(2), false)));
        notes.note(Note::Ran(ran(Some(3), false)));
        let log = std::fs::read_to_string(dir.join(LOG_FILE)).unwrap();
        assert!(
            log.contains("h  synced /a.txt  exit 0") && log.contains("    hi\n    (more output not kept)"),
            "{log}"
        );
        let store = crate::setup::open_store(&dir, 1024).unwrap();
        let entries = store.read(|o| o.activity_recent(None, 10)).unwrap();
        assert_eq!(entries.len(), 1, "the second failure within a minute waits: {entries:?}");
        assert!(entries[0].detail.contains("hook h on synced: exit 2") && entries[0].path == "/a.txt");
        drop(store);
        let _ = std::fs::remove_dir_all(&dir);
    }
}
