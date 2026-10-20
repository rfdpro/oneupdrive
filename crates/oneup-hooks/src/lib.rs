//! Event hooks (`docs/architecture/hooks.md`): programs the user names in
//! `config.toml`, started by the agent when sync events match (an event
//! kind and, for items, a path rule), with the event as data.
//!
//! * [`HookSpec`]: one hook as configured, and the checks its shape must
//!   pass ([`HookSpec::check`]).
//! * [`program`]: where its program is. One that lives in a synced folder
//!   (ours or another sync app's) is refused, at load and again right
//!   before each start: a change in OneDrive must never become code running
//!   on the PC.
//! * [`Runner`]: the queue. Coalesced per hook and path, rate-limited per
//!   hook, bounded, run on background threads in contained processes
//!   (`oneup_sys::contained`), so a storm of events can't grow memory
//!   without limit or slow sync down.
//!
//! What a hook gets: the event's kind, the item's path (full and relative),
//! what was done and a short detail, through `ONEUPHOOK_*` environment
//! variables and one JSON line on standard input ([`Fill::env`],
//! [`Fill::json_line`]); a plain `.exe` that isn't an interpreter may also
//! take them as template arguments ([`TEMPLATES`]). Never file contents,
//! tokens, approval codes or OneUpDrive's own environment variables.

use std::ffi::OsString;
use std::path::Path;
use std::time::Duration;

use oneup_ipc::{Event, EventKind};

pub mod program;
mod runner;

pub use runner::{
    Contained, Finished, Launch, LaunchRequest, MAX_PENDING, Note, PlacesSource, Ran, Runner, Settings, check,
};

/// At most this many hooks.
pub const MAX_HOOKS: usize = 32;
/// Defaults and limits for a hook's settings.
pub const DEFAULT_TIMEOUT: Duration = Duration::from_secs(60);
pub const MAX_TIMEOUT: Duration = Duration::from_secs(3600);
pub const DEFAULT_PER_MINUTE: u32 = 60;
pub const MAX_PER_MINUTE: u32 = 6000;
pub const DEFAULT_COALESCE: Duration = Duration::from_secs(2);
pub const MAX_COALESCE: Duration = Duration::from_secs(3600);

/// One hook, as `config.toml` describes it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct HookSpec {
    /// Unique among the hooks; how the user refers to it.
    pub name: String,
    /// The kinds of events that start it.
    pub on: Vec<EventKind>,
    /// Rules in the exclusion rules' syntax (`/Photos/`, `*.docx`): only
    /// items they match start it. Empty: every item, and the events that
    /// aren't about an item (pauses, problems, safety stops).
    pub paths: Vec<String>,
    /// A full path, or a bare program name looked up on `PATH`.
    pub program: String,
    /// Arguments, with templates ([`TEMPLATES`]).
    pub args: Vec<String>,
    /// Ended (with everything it started) after this long.
    pub timeout: Duration,
    /// Started at most this often (a burst of up to a tenth of a minute's
    /// worth goes at once).
    pub per_minute: u32,
    /// Waits this long after the last event for the same item, so a burst
    /// of changes to one file starts it once.
    pub coalesce: Duration,
    pub enabled: bool,
}

impl HookSpec {
    /// A hook with the default settings.
    pub fn new(name: &str, on: &[EventKind], program: &str, args: &[&str]) -> Self {
        Self {
            name: name.into(),
            on: on.to_vec(),
            paths: Vec::new(),
            program: program.into(),
            args: args.iter().map(|a| (*a).to_string()).collect(),
            timeout: DEFAULT_TIMEOUT,
            per_minute: DEFAULT_PER_MINUTE,
            coalesce: DEFAULT_COALESCE,
            enabled: true,
        }
    }

    /// Whether its shape is right (the program's place is checked by
    /// [`program::check`]).
    pub fn check(&self) -> Result<(), String> {
        if self.name.trim().is_empty() {
            return Err("a hook needs a name".into());
        }
        if self.on.is_empty() {
            return Err("`on` needs at least one event".into());
        }
        if self.program.trim().is_empty() {
            return Err("`program` is empty".into());
        }
        for p in &self.paths {
            if oneup_rules::Rule::parse(p).is_none() || p.contains(['\n', '\r']) {
                return Err(format!("`{p}` isn't a path rule"));
            }
        }
        if self.timeout.is_zero() || self.timeout > MAX_TIMEOUT {
            return Err(format!("timeout_s must be from 1 to {}", MAX_TIMEOUT.as_secs()));
        }
        if !(1..=MAX_PER_MINUTE).contains(&self.per_minute) {
            return Err(format!("per_minute must be from 1 to {MAX_PER_MINUTE}"));
        }
        if self.coalesce > MAX_COALESCE {
            return Err(format!("coalesce_s must be at most {}", MAX_COALESCE.as_secs()));
        }
        for a in &self.args {
            templates_in(a)?;
        }
        Ok(())
    }

    /// Whether `e` starts it.
    fn matches(&self, rules: Option<&oneup_rules::RuleSet>, e: &Event) -> bool {
        if !self.on.contains(&e.kind) {
            return false;
        }
        match rules {
            None => true,
            Some(r) => e.kind.has_path() && !e.path.is_empty() && r.excludes(&e.path, false),
        }
    }
}

/// The templates an argument may hold (`{{` is a plain `{`).
pub const TEMPLATES: [&str; 5] = ["{event}", "{path}", "{relpath}", "{action}", "{detail}"];

/// The templates in `arg`, or why it's wrong (an unknown or unclosed one).
fn templates_in(arg: &str) -> Result<Vec<&'static str>, String> {
    let mut found = Vec::new();
    let mut rest = arg;
    while let Some(i) = rest.find('{') {
        let tail = &rest[i..];
        if let Some(after) = tail.strip_prefix("{{") {
            rest = after;
            continue;
        }
        let Some(t) = TEMPLATES.iter().find(|t| tail.starts_with(**t)) else {
            return Err(format!("unknown template in `{arg}`; use {}", TEMPLATES.join(", ")));
        };
        found.push(*t);
        rest = &tail[t.len()..];
    }
    Ok(found)
}

/// A value made safe to quote in a batch file or a shell: no double quote
/// (it becomes `'`), so `"%ONEUPHOOK_PATH%"` can't be broken out of, and no
/// control character (it becomes a space). Windows and OneDrive names can't
/// hold either; details could, so every value is cleaned the same way.
fn env_safe(v: &str) -> String {
    v.chars()
        .map(|c| {
            if c == '"' {
                '\''
            } else if c.is_control() {
                ' '
            } else {
                c
            }
        })
        .collect()
}

/// What a hook's templates stand for, for one event.
pub struct Fill<'a> {
    pub event: &'a Event,
    /// The synced folder on this PC, for full paths.
    pub root: &'a Path,
}

impl Fill<'_> {
    /// The arguments for the program, templates replaced.
    pub fn args(&self, args: &[String]) -> Vec<OsString> {
        args.iter().map(|a| self.arg(a)).collect()
    }

    /// The item's full path on this PC (empty for an event without a path).
    pub fn full_path(&self) -> String {
        let rel = self.event.path.trim_start_matches('/');
        if rel.is_empty() {
            String::new()
        } else {
            let p = self.root.join(rel.replace('/', "\\"));
            oneup_local::coexist::plain(&p).to_string_lossy().into_owned()
        }
    }

    /// The event as the environment variables a hook reads (`ONEUPHOOK_*`),
    /// never carrying tokens or file contents.
    pub fn env(&self) -> Vec<(OsString, OsString)> {
        let e = self.event;
        [
            ("ONEUPHOOK_EVENT", e.kind.name().to_string()),
            ("ONEUPHOOK_PATH", self.full_path()),
            ("ONEUPHOOK_RELPATH", e.path.trim_start_matches('/').to_string()),
            ("ONEUPHOOK_ACTION", e.action.clone()),
            ("ONEUPHOOK_DETAIL", e.detail.clone()),
        ]
        .into_iter()
        .map(|(k, v)| (OsString::from(k), OsString::from(env_safe(&v))))
        .collect()
    }

    /// The event as one JSON line for standard input.
    pub fn json_line(&self) -> Vec<u8> {
        fn esc(s: &str) -> String {
            let mut o = String::with_capacity(s.len() + 2);
            for c in s.chars() {
                match c {
                    '"' => o.push_str("\\\""),
                    '\\' => o.push_str("\\\\"),
                    '\n' => o.push_str("\\n"),
                    '\r' => o.push_str("\\r"),
                    '\t' => o.push_str("\\t"),
                    c if (c as u32) < 0x20 => o.push_str(&format!("\\u{:04x}", c as u32)),
                    c => o.push(c),
                }
            }
            o
        }
        let e = self.event;
        format!(
            "{{\"event\":\"{}\",\"path\":\"{}\",\"relpath\":\"{}\",\"action\":\"{}\",\"detail\":\"{}\",\"seq\":{},\"at_ms\":{}}}\n",
            e.kind.name(),
            esc(&self.full_path()),
            esc(e.path.trim_start_matches('/')),
            esc(&e.action),
            esc(&e.detail),
            e.seq,
            e.at_ms,
        )
        .into_bytes()
    }

    fn arg(&self, a: &str) -> OsString {
        let rel = self.event.path.trim_start_matches('/');
        let full = || self.full_path();
        let mut out = String::new();
        let mut rest = a;
        while let Some(i) = rest.find('{') {
            out.push_str(&rest[..i]);
            let tail = &rest[i..];
            if let Some(after) = tail.strip_prefix("{{") {
                out.push('{');
                rest = after;
                continue;
            }
            match TEMPLATES.iter().find(|t| tail.starts_with(**t)) {
                Some(t) => {
                    match *t {
                        "{event}" => out.push_str(self.event.kind.name()),
                        "{path}" => out.push_str(&full()),
                        "{relpath}" => out.push_str(rel),
                        "{action}" => out.push_str(&self.event.action),
                        _ => out.push_str(&self.event.detail),
                    }
                    rest = &tail[t.len()..];
                }
                // Checked at load; kept as written otherwise.
                None => {
                    out.push('{');
                    rest = &tail[1..];
                }
            }
        }
        out.push_str(rest);
        OsString::from(out)
    }
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used)]
    use super::*;

    fn event(kind: EventKind, path: &str) -> Event {
        Event { seq: 1, at_ms: 0, kind, path: path.into(), action: "upload".into(), detail: "new version".into() }
    }

    /// No `ONEUPHOOK_*` value can hold a double quote or a control
    /// character, so a quoted `"%ONEUPHOOK_DETAIL%"` in a batch file stays
    /// one quoted string.
    #[test]
    fn environment_values_hold_no_quotes() {
        let e = Event {
            seq: 1,
            at_ms: 0,
            kind: EventKind::Failure,
            path: "/a&b.txt".into(),
            action: "upload".into(),
            detail: "said \"no\" & quit\r\nnext".into(),
        };
        let env = Fill { event: &e, root: Path::new(r"C:\Sync") }.env();
        for (k, v) in &env {
            let v = v.to_string_lossy();
            assert!(!v.contains('"') && !v.chars().any(char::is_control), "{k:?}={v}");
        }
        let detail = env.iter().find(|(k, _)| k == "ONEUPHOOK_DETAIL").unwrap().1.to_string_lossy().into_owned();
        assert_eq!(detail, "said 'no' & quit  next");
    }

    #[test]
    fn templates_become_the_events_text() {
        let e = event(EventKind::Synced, "/Photos/a b.jpg");
        let f = Fill { event: &e, root: Path::new(r"\\?\C:\Sync") };
        let out = f.args(&["{event}".into(), "{path}".into(), "--rel={relpath}".into(), "{action}: {detail}".into()]);
        assert_eq!(out, ["synced", r"C:\Sync\Photos\a b.jpg", "--rel=Photos/a b.jpg", "upload: new version"]);
        assert_eq!(f.args(&["{{{event}}".into()]), ["{synced}"]);
        assert!(templates_in("{{x}").is_ok());
        // Events about sync itself have no path.
        let e = event(EventKind::Paused, "");
        assert_eq!(Fill { event: &e, root: Path::new(r"C:\Sync") }.args(&["{path}".into()]), [""]);
    }

    #[test]
    fn a_hooks_shape_is_checked() {
        let ok = HookSpec::new("thumbs", &[EventKind::Synced], r"C:\Tools\thumb.exe", &["{path}"]);
        assert_eq!(ok.check(), Ok(()));
        let bad = |f: &dyn Fn(&mut HookSpec)| {
            let mut h = ok.clone();
            f(&mut h);
            h.check().unwrap_err()
        };
        assert!(bad(&|h| h.on.clear()).contains("on"));
        assert!(bad(&|h| h.args = vec!["{file}".into()]).contains("unknown template"));
        assert!(bad(&|h| h.args = vec!["{path".into()]).contains("unknown template"));
        assert!(bad(&|h| h.timeout = Duration::ZERO).contains("timeout_s"));
        assert!(bad(&|h| h.per_minute = 0).contains("per_minute"));
        assert!(bad(&|h| h.paths = vec!["  ".into()]).contains("path rule"));
        assert!(bad(&|h| h.name = " ".into()).contains("name"));
    }

    #[test]
    fn hooks_match_kinds_and_paths() {
        let mut h = HookSpec::new("h", &[EventKind::Synced, EventKind::Paused], "x.exe", &[]);
        assert!(h.matches(None, &event(EventKind::Synced, "/a.txt")));
        assert!(h.matches(None, &event(EventKind::Paused, "")));
        assert!(!h.matches(None, &event(EventKind::Failure, "/a.txt")));
        h.paths = vec!["/Photos/".into(), "*.docx".into()];
        let rules = oneup_rules::RuleSet::new(&[] as &[String], &h.paths).unwrap();
        assert!(h.matches(Some(&rules), &event(EventKind::Synced, "/photos/2024/a.jpg")));
        assert!(h.matches(Some(&rules), &event(EventKind::Synced, "/work/b.DOCX")));
        assert!(!h.matches(Some(&rules), &event(EventKind::Synced, "/work/b.txt")));
        assert!(!h.matches(Some(&rules), &event(EventKind::Paused, "")), "path rules: items only");
    }
}
