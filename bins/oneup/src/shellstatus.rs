//! What Explorer's sync-status flyout shows for a OneUpDrive folder (the
//! cloud button in Explorer's address bar on Windows 11): the agent writes
//! a few lines per sync root to `HKCU\Software\OneUpDrive\Status`, named by
//! the root's shell ID, whenever its status changes; the shell extension
//! (`crates/oneup-shellext`, in its surrogate process) reads them when
//! Explorer asks, and watches the key to tell Explorer when to ask again.
//! Plain `key=value` lines, so the extension needs no parser. No file
//! names, nothing secret: the state, a sentence, the storage used, and
//! where to point the buttons.

use std::path::Path;

use oneup_ipc::{Activity, Status};

/// Where the agents publish (a value per sync root).
pub const KEY: &str = r"Software\OneUpDrive\Status";

/// One sync root's flyout.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct ShellStatus {
    /// `insync`, `syncing`, `paused`, `attention` or `error`.
    pub state: &'static str,
    pub label: String,
    pub detail: String,
    /// The data folder (for the Settings button).
    pub dir: String,
    /// `main`, `vault` or `pair`.
    pub kind: &'static str,
    /// The synced folder on this PC.
    pub local: String,
    pub quota_used: Option<i64>,
    pub quota_total: Option<i64>,
}

impl ShellStatus {
    /// From the agent's status.
    pub fn from_status(s: &Status, dir: &Path, quota: Option<(i64, i64)>) -> Self {
        let kind = if crate::vaultpair::is_vault_dir(dir) {
            "vault"
        } else if crate::pairs::is_pair_dir(dir) {
            "pair"
        } else {
            "main"
        };
        let (state, label, detail) = if !s.safety_stops.is_empty() || s.overflow.as_ref().is_some_and(|o| o.waiting) {
            ("attention", "Needs your review", "Review it from the OneUpDrive icon's menu.".to_string())
        } else if let Some(p) = &s.problem {
            ("error", "Problem", format!("{} Select for details.", p.title))
        } else if let Some(why) = &s.auto_pause {
            let why = crate::autopause::said(why, s.auto_pause_hours.as_deref(), &crate::l10n::Locale::english());
            ("paused", "Paused", format!("Paused: {why}."))
        } else {
            match s.activity {
                Activity::Idle => ("insync", "Up to date", String::new()),
                Activity::Paused => ("paused", "Paused", String::new()),
                Activity::Retrying => ("syncing", "Retrying soon", String::new()),
                Activity::WaitingForYou => {
                    ("attention", "Needs your review", "Review it from the OneUpDrive icon's menu.".to_string())
                }
                Activity::Starting | Activity::Syncing | Activity::Stopping => ("syncing", "Syncing", String::new()),
            }
        };
        let shown = |p: &str| p.strip_prefix(r"\\?\").unwrap_or(p).to_string();
        ShellStatus {
            state,
            label: label.into(),
            detail,
            dir: shown(&dir.to_string_lossy()),
            kind,
            local: shown(&s.local),
            quota_used: quota.map(|q| q.0),
            quota_total: quota.map(|q| q.1),
        }
    }

    /// The registry text (`key=value` lines; line breaks in values become
    /// spaces).
    pub fn encode(&self) -> String {
        let one = |v: &str| v.replace(['\r', '\n'], " ");
        let mut out = format!(
            "state={}\nlabel={}\ndetail={}\ndir={}\nkind={}\nlocal={}\n",
            self.state,
            one(&self.label),
            one(&self.detail),
            one(&self.dir),
            self.kind,
            one(&self.local)
        );
        if let (Some(u), Some(t)) = (self.quota_used, self.quota_total) {
            out.push_str(&format!("quota_used={u}\nquota_total={t}\n"));
        }
        out
    }
}

/// The value's name: the sync root's shell ID (as registered).
pub fn value_name(token: &str) -> Option<String> {
    let sid = oneup_sys::pipe::current_user_sid().ok()?;
    Some(oneup_cfapi::shell::root_id(&sid, token))
}

/// Publishes `s` for the sync root of `token`.
pub fn publish(token: &str, s: &ShellStatus) {
    if let Some(name) = value_name(token)
        && let Err(e) = oneup_sys::user_registry::set(KEY, &name, &s.encode())
    {
        tracing::debug!(error = %e, "flyout status not published");
    }
}

/// Removes it (the agent stopped: Explorer then shows no status).
pub fn clear(token: &str) {
    if let Some(name) = value_name(token) {
        let _ = oneup_sys::user_registry::remove(KEY, &name);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn statuses_read_as_people_would_say_them() {
        let s = Status {
            version: "x".into(),
            rev: 0,
            local: r"\\?\C:\Sync".into(),
            remote: "/Sync".into(),
            activity: Activity::Idle,
            paused: oneup_ipc::PauseState::default(),
            push: true,
            checked_ms: None,
            last: None,
            safety_stops: Vec::new(),
            overflow: None,
            problem: None,
            auto_pause: None,
            auto_pause_hours: None,
            prune_offer: None,
            progress: None,
            unsynced: None,
            retrying: None,
            databases: Vec::new(),
            databases_more: 0,
            network: None,
        };
        let f = ShellStatus::from_status(&s, Path::new(r"C:\Data"), Some((5, 10)));
        assert_eq!((f.state, f.label.as_str(), f.kind, f.local.as_str()), ("insync", "Up to date", "main", r"C:\Sync"));
        assert!(f.encode().contains("quota_used=5\nquota_total=10\n"));
        let p = oneup_ipc::Problem {
            kind: "offline".into(),
            title: "Can't reach\nOneDrive".into(),
            detail: String::new(),
            action: String::new(),
            technical: "network error: timed out".into(),
            since_ms: 0,
            attempts: 1,
            retry_ms: None,
        };
        let busy = ShellStatus::from_status(&Status { problem: Some(p), ..s }, Path::new("D"), None);
        assert_eq!(busy.state, "error");
        let text = busy.encode();
        assert!(!text.contains("quota") && text.contains("Can't reach OneDrive"), "{text}");
        assert!(!text.contains("network error"), "never the technical text");
    }
}
