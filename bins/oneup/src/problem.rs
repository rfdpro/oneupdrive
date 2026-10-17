//! Problems, told for people. The agent's errors say where they happened
//! and why, for a developer ("mirror inconsistency: synced: 1 rows whose
//! parent is missing or not a folder"). What people need is what it means
//! for their files, whether OneUpDrive deals with it by itself, and what
//! they can do: an [`oneup_ipc::Problem`]. The technical text stays with it
//! for a report, never shown first.
//!
//! Each kind has a section in the user guide's troubleshooting page
//! (`docs/user-guide/troubleshooting.md`, anchored by the kind), with the
//! manual fixes for when retrying doesn't help.
//!
//! When an error stops the agent for good, the problem is saved to
//! [`STOPPED_FILE`], so the tray and the settings window can say why it
//! isn't running; the next start removes it.

use std::path::Path;

use oneup_ipc::Problem;

/// In the data folder: why the agent stopped (JSON), until it starts again.
pub const STOPPED_FILE: &str = "agent-stopped.json";

/// What kind of problem an error is.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Kind {
    /// Nothing is set up to sync yet.
    Setup,
    /// No connection to OneDrive.
    Offline,
    /// OneDrive no longer accepts the sign-in.
    SignIn,
    /// The OneDrive account has no room left.
    CloudFull,
    /// The disk on this PC has no room left.
    DiskFull,
    /// The synced folder moved, went, or another sync app took it.
    Folder,
    /// OneUpDrive's own records contradict themselves.
    Records,
    /// OneDrive keeps answering with errors.
    OneDrive,
    Other,
}

impl Kind {
    pub fn code(self) -> &'static str {
        match self {
            Kind::Setup => "setup",
            Kind::Offline => "offline",
            Kind::SignIn => "signin",
            Kind::CloudFull => "cloud-full",
            Kind::DiskFull => "disk-full",
            Kind::Folder => "folder",
            Kind::Records => "records",
            Kind::OneDrive => "onedrive",
            Kind::Other => "other",
        }
    }

    /// Title, what happened, and what to do.
    fn words(self) -> (&'static str, &'static str, &'static str) {
        match self {
            Kind::Setup => (
                "Nothing to sync yet",
                "No folder is set up to sync with OneDrive.",
                "Open Settings and set up a folder.",
            ),
            Kind::Offline => (
                "Can't reach OneDrive",
                "Changes wait on this PC and in OneDrive until the connection is back.",
                "Check your internet connection. Syncing resumes by itself.",
            ),
            Kind::SignIn => (
                "Sign-in needed",
                "OneDrive no longer accepts OneUpDrive's sign-in. Nothing syncs until you sign in again.",
                "Open Settings > Accounts and select Sign in.",
            ),
            Kind::CloudFull => (
                "OneDrive storage is full",
                "Uploads can't go through until there's room in OneDrive.",
                "Free up space in OneDrive (its recycle bin counts too), or get more storage.",
            ),
            Kind::DiskFull => (
                "This PC's disk is full",
                "Downloads can't finish until there's room on the disk.",
                "Free up space on the disk, or turn on Files On-Demand (Settings > Files) so files download only when opened.",
            ),
            Kind::Folder => (
                "Synced folder unavailable",
                "The synced folder was moved, renamed or removed, or another sync app took it over. \
                 OneUpDrive stopped so nothing changes by mistake.",
                "Put the folder back where it was, then start OneUpDrive from its icon. To sync another folder, \
                 stop syncing this one in Settings > Files and set up again.",
            ),
            Kind::Records => (
                "Sync records need repair",
                "OneUpDrive found a mistake in its own records of what's synced. It changes nothing until \
                 that's fixed, so your files are safe.",
                "Install the latest update: repairs come with updates. If this lasts, copy the details and report it.",
            ),
            Kind::OneDrive => (
                "OneDrive isn't responding",
                "OneDrive answered with errors. This is usually temporary.",
                "Nothing to do. If it lasts for hours, check OneDrive's service status.",
            ),
            Kind::Other => (
                "Sync problem",
                "Something went wrong while syncing.",
                "If it keeps happening, copy the details and report it.",
            ),
        }
    }
}

/// Which kind `e` is, from the errors in its chain.
pub fn classify(e: &anyhow::Error) -> Kind {
    let mut kind = Kind::Other;
    for c in e.chain() {
        let found = if c.is::<crate::sync::NotPaired>() {
            Some(Kind::Setup)
        } else if c.is::<crate::sync::Halted>() {
            Some(Kind::Folder)
        } else if let Some(g) = c.downcast_ref::<oneup_graph::Error>() {
            graph_kind(g)
        } else if let Some(r) = c.downcast_ref::<reqwest::Error>() {
            (r.is_connect() || r.is_timeout() || r.is_request()).then_some(Kind::Offline)
        } else if let Some(r) = c.downcast_ref::<oneup_remote::Error>() {
            matches!(r, oneup_remote::Error::Inconsistent(_)).then_some(Kind::Records)
        } else if let Some(s) = c.downcast_ref::<oneup_store::Error>() {
            store_kind(s)
        } else if let Some(x) = c.downcast_ref::<oneup_effects::Error>() {
            match x {
                oneup_effects::Error::RootChanged(_) | oneup_effects::Error::OtherEngine(_) => Some(Kind::Folder),
                oneup_effects::Error::Io(io) => io_kind(io),
                _ => None,
            }
        } else {
            c.downcast_ref::<std::io::Error>().and_then(io_kind)
        };
        // The most specific cause wins: the chain goes from the outermost
        // context to the root cause, so a later find replaces an earlier one.
        if let Some(k) = found {
            kind = k;
        }
    }
    // Some layers keep an OS error only as text.
    if kind == Kind::Other {
        let text = format!("{e:#}");
        if text.contains("(os error 112)") || text.contains("(os error 39)") {
            kind = Kind::DiskFull;
        }
    }
    kind
}

fn graph_kind(g: &oneup_graph::Error) -> Option<Kind> {
    use oneup_graph::Error as G;
    match g {
        G::Http(r) => {
            Some(if r.is_connect() || r.is_timeout() || r.is_request() { Kind::Offline } else { Kind::OneDrive })
        }
        G::Stalled(..) => Some(Kind::Offline),
        G::Auth(_) | G::NotSignedIn => Some(Kind::SignIn),
        G::Status { status: 401, .. } => Some(Kind::SignIn),
        G::Status { status: 507, .. } => Some(Kind::CloudFull),
        G::Status { body, .. } if body.contains("quotaLimitReached") || body.contains("insufficientStorage") => {
            Some(Kind::CloudFull)
        }
        G::Status { status, .. } if *status == 429 || *status >= 500 => Some(Kind::OneDrive),
        G::Exhausted(_) => Some(Kind::OneDrive),
        G::Local(io) => io_kind(io),
        _ => None,
    }
}

fn store_kind(s: &oneup_store::Error) -> Option<Kind> {
    if s.is_damaged() {
        Some(Kind::Records)
    } else if s.is_disk_full() {
        Some(Kind::DiskFull)
    } else {
        None
    }
}

fn io_kind(io: &std::io::Error) -> Option<Kind> {
    // ERROR_HANDLE_DISK_FULL (39), ERROR_DISK_FULL (112).
    (io.kind() == std::io::ErrorKind::StorageFull || matches!(io.raw_os_error(), Some(39 | 112)))
        .then_some(Kind::DiskFull)
}

/// The problem `e` is, at `now_ms`. Continues `prev` (when it began, how
/// many tries met it) if that was the same kind. `retry_ms`: when the agent
/// tries again (`None`: it stopped).
pub fn describe(e: &anyhow::Error, prev: Option<&Problem>, now_ms: i64, retry_ms: Option<i64>) -> Problem {
    let kind = classify(e);
    let (title, detail, action) = kind.words();
    let same = prev.filter(|p| p.kind == kind.code());
    Problem {
        kind: kind.code().into(),
        title: title.into(),
        detail: detail.into(),
        action: action.into(),
        technical: format!("{e:#}"),
        since_ms: same.map_or(now_ms, |p| p.since_ms),
        attempts: same.map_or(1, |p| p.attempts.saturating_add(1)),
        retry_ms,
    }
}

/// Notes in the activity log that `p` began (`resolved` false) or cleared,
/// so what happened, and when, can be seen later.
pub fn log(store: &mut oneup_store::Store, p: &Problem, resolved: bool) -> oneup_store::Result<()> {
    use oneup_store::{ActivityEntry, ActivityOutcome};
    let (outcome, detail) = if resolved {
        (ActivityOutcome::Note, format!("resolved: {}", p.title))
    } else {
        (ActivityOutcome::Failed, p.title.clone())
    };
    let at_ms = crate::control::now_ms();
    let entry = ActivityEntry { seq: 0, at_ms, outcome, action: "problem".into(), path: String::new(), detail };
    store.write(|o| o.activity_add(&[entry]))
}

/// Saves why the agent stopped (our own file: written whole, then renamed).
pub fn save_stopped(data_dir: &Path, p: &Problem) -> std::io::Result<()> {
    let file = data_dir.join(STOPPED_FILE);
    let tmp = file.with_extension("json.tmp");
    std::fs::write(&tmp, serde_json::to_vec(p).map_err(std::io::Error::other)?)?;
    std::fs::rename(&tmp, file)
}

/// Why the agent last stopped, if an error stopped it (and it hasn't
/// started since).
pub fn load_stopped(data_dir: &Path) -> Option<Problem> {
    let text = std::fs::read_to_string(data_dir.join(STOPPED_FILE)).ok()?;
    serde_json::from_str(&text).ok()
}

/// Forgets why the agent last stopped (it's starting again).
pub fn clear_stopped(data_dir: &Path) {
    let _ = std::fs::remove_file(data_dir.join(STOPPED_FILE));
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used)]
    use super::*;

    fn err<E: std::error::Error + Send + Sync + 'static>(e: E) -> anyhow::Error {
        anyhow::Error::new(e).context("syncing a round")
    }

    #[test]
    fn errors_become_kinds_people_can_act_on() {
        // A case seen in use.
        let records = err(oneup_remote::Error::Inconsistent("synced: 1 rows whose parent is missing".into()));
        assert_eq!(classify(&records), Kind::Records);
        assert_eq!(classify(&err(oneup_graph::Error::NotSignedIn)), Kind::SignIn);
        assert_eq!(classify(&err(crate::sync::NotPaired)), Kind::Setup);
        let status = |status, body: &str| oneup_graph::Error::Status { status, what: "x".into(), body: body.into() };
        assert_eq!(classify(&err(status(401, ""))), Kind::SignIn);
        assert_eq!(classify(&err(status(507, ""))), Kind::CloudFull);
        assert_eq!(classify(&err(status(403, r#"{"error":{"code":"quotaLimitReached"}}"#))), Kind::CloudFull);
        assert_eq!(classify(&err(status(503, ""))), Kind::OneDrive);
        assert_eq!(classify(&err(oneup_graph::Error::Exhausted(5))), Kind::OneDrive);
        assert_eq!(classify(&err(oneup_graph::Error::Stalled("download", 60))), Kind::Offline);
        // The sign-in service busy is not "sign in again".
        assert_eq!(
            classify(&err(oneup_graph::Error::refused(503, "the sign-in service", String::new()))),
            Kind::OneDrive
        );
        assert_eq!(classify(&err(std::io::Error::from_raw_os_error(112))), Kind::DiskFull);
        assert_eq!(classify(&err(oneup_effects::Error::RootChanged("moved"))), Kind::Folder);
        assert_eq!(
            classify(&err(crate::sync::Halted("the sync folder failed its identity check".into()))),
            Kind::Folder
        );
        assert_eq!(classify(&anyhow::anyhow!("something else")), Kind::Other);
        // Only as text, through a layer that kept the message.
        assert_eq!(
            classify(&anyhow::anyhow!("writing: There is not enough space on the disk. (os error 112)")),
            Kind::DiskFull
        );
    }

    #[test]
    fn a_problem_keeps_its_start_while_it_lasts() {
        let e = err(oneup_graph::Error::NotSignedIn);
        let first = describe(&e, None, 1_000, Some(61_000));
        assert_eq!((first.kind.as_str(), first.since_ms, first.attempts), ("signin", 1_000, 1));
        assert!(first.technical.contains("not signed in") && first.title == "Sign-in needed");
        let again = describe(&e, Some(&first), 70_000, Some(190_000));
        assert_eq!((again.since_ms, again.attempts, again.retry_ms), (1_000, 2, Some(190_000)));
        // Another kind starts over.
        let other = describe(&anyhow::anyhow!("x"), Some(&again), 200_000, None);
        assert_eq!((other.kind.as_str(), other.since_ms, other.attempts), ("other", 200_000, 1));
    }

    #[test]
    fn why_it_stopped_is_kept_until_the_next_start() {
        let dir = std::env::temp_dir().join(format!("oneup-problem-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        assert!(load_stopped(&dir).is_none());
        let p = describe(&err(oneup_effects::Error::RootChanged("moved")), None, 5, None);
        save_stopped(&dir, &p).unwrap();
        assert_eq!(load_stopped(&dir), Some(p));
        clear_stopped(&dir);
        assert!(load_stopped(&dir).is_none());
        std::fs::remove_dir_all(&dir).unwrap();
    }
}
