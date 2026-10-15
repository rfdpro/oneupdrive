//! Finishing a stalled unlink: releasing the sync root another program (the
//! OneDrive app) left registered on a folder after removing its account
//! (`data-safety.md` invariant 16).
//!
//! Unregistering a sync root is what unlinking does, and Windows then
//! removes every online-only placeholder in it (their content is only in
//! the cloud) and turns everything with content here into an ordinary file
//! (measured: `oneup-cfapi` test
//! `a_stalled_unlink_can_be_finished_by_another_program`). So it's a change
//! to user content, and only a [`Release`] token from [`review_release`]
//! lets the effects layer do it. The caller gathers the [`Evidence`] just
//! before asking; this stays pure.
//!
//! Windows skips any folder another program has open at that moment (its
//! current folder, as Explorer or a terminal has Documents): its subtree is
//! left as it was, orphaned placeholders with no root, though unregistering
//! reports success (`a_folder_held_open_is_left_orphaned_until_released_again`).
//! The same token then covers releasing the folder again, through a short
//! registration of OneUpDrive's own that is unregistered at once
//! ([`Evidence::held`] is `None`, [`Evidence::left_behind`] counts them).

use std::path::PathBuf;

/// A scan or listing older than this isn't evidence of what's there now.
pub const FRESH_MS: i64 = 10 * 60 * 1000;

/// Who has the folder registered as a sync root, as Windows reports it now.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Holder {
    pub provider: String,
    pub identity: Vec<u8>,
    /// That provider is OneUpDrive itself (never released this way).
    pub ours: bool,
}

/// What the caller found, just now.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Evidence {
    /// The folder.
    pub root: PathBuf,
    /// Its sync root's provider; `None` when no one has it registered any
    /// more (an earlier release left placeholders behind).
    pub held: Option<Holder>,
    /// Cloud placeholders the scan found in it (with no root: left behind).
    pub left_behind: u64,
    /// The app removed its account for this folder: the user started
    /// unlinking, and the app stalled before releasing the folder.
    pub account_removed: bool,
    /// Where the app's executables are, and how many of its processes run.
    pub app_dir: PathBuf,
    pub app_running: usize,
    /// Whether this data folder is signed in with the account the app
    /// synced (`None`: can't tell).
    pub same_account: Option<bool>,
    /// When this data folder's scan of the folder finished, and when its
    /// OneDrive listing was last brought up to date (ms since 1970).
    pub scanned_at_ms: Option<i64>,
    pub listed_at_ms: Option<i64>,
    pub now_ms: i64,
    /// From that scan and listing: online-only files and never-listed
    /// folders that are in OneDrive at the same path, and those that aren't.
    pub in_cloud: (u64, u64),
    pub missing: (u64, u64),
    /// The app's choices (always keep on this device…) are recorded.
    pub choices_recorded: bool,
    /// The user said yes to this, having been told what it does.
    pub confirmed: bool,
}

/// Why the gate won't release the folder.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum Refusal {
    #[error("the folder is OneUpDrive's own")]
    Ours,
    #[error("no one holds the folder and nothing was left behind in it")]
    NothingToRelease,
    #[error("the OneDrive app still has its account for this folder: unlink it in the app first")]
    NotUnlinking,
    #[error("{0} of the OneDrive app's processes are still running")]
    AppRunning(usize),
    #[error("this data folder is signed in with another account than the OneDrive app's")]
    OtherAccount,
    #[error("the folder's scan or OneDrive listing isn't recent; check again")]
    NotFresh,
    #[error("{0} online-only files and {1} folders aren't in OneDrive at the same path")]
    NotInCloud(u64, u64),
    #[error("nothing online-only was checked against OneDrive")]
    NothingChecked,
    #[error("the OneDrive app's choices for this folder aren't recorded yet")]
    ChoicesNotRecorded,
    #[error("not confirmed")]
    NotConfirmed,
}

/// The gate's approval to release one folder. Only this crate can make
/// one; the effects layer re-checks, just before acting, that the folder is
/// still registered to the same provider and identity (or still to no one),
/// and that none of the app's processes runs.
#[derive(Debug, PartialEq, Eq)]
pub struct Release {
    root: PathBuf,
    held: Option<(String, Vec<u8>)>,
    app_dir: PathBuf,
    _seal: Seal,
}

#[derive(Debug, PartialEq, Eq)]
struct Seal;

impl Release {
    pub fn root(&self) -> &std::path::Path {
        &self.root
    }
    /// The provider and root identity the gate saw; `None`: placeholders
    /// left behind, with no root.
    pub fn held(&self) -> Option<(&str, &[u8])> {
        self.held.as_ref().map(|(p, i)| (p.as_str(), i.as_slice()))
    }
    pub fn app_dir(&self) -> &std::path::Path {
        &self.app_dir
    }
}

fn fresh(at: Option<i64>, now: i64) -> bool {
    at.is_some_and(|t| t <= now && now - t <= FRESH_MS)
}

/// Approves releasing the folder only when nothing of the user's can be
/// lost: everything online-only is in OneDrive (checked from a fresh scan
/// and listing, for the same account), the app's choices are recorded, the
/// app itself started unlinking and isn't running, and the user agreed.
pub fn review_release(e: &Evidence) -> Result<Release, Refusal> {
    match &e.held {
        Some(h) if h.ours => return Err(Refusal::Ours),
        None if e.left_behind == 0 => return Err(Refusal::NothingToRelease),
        _ => {}
    }
    if !e.account_removed {
        return Err(Refusal::NotUnlinking);
    }
    if e.app_running > 0 {
        return Err(Refusal::AppRunning(e.app_running));
    }
    if e.same_account == Some(false) {
        return Err(Refusal::OtherAccount);
    }
    if !fresh(e.scanned_at_ms, e.now_ms) || !fresh(e.listed_at_ms, e.now_ms) {
        return Err(Refusal::NotFresh);
    }
    if e.missing != (0, 0) {
        return Err(Refusal::NotInCloud(e.missing.0, e.missing.1));
    }
    // Without the account known, the check itself is the evidence that the
    // listing is this folder's OneDrive: it must have found something.
    if e.same_account.is_none() && e.in_cloud == (0, 0) {
        return Err(Refusal::NothingChecked);
    }
    if !e.choices_recorded {
        return Err(Refusal::ChoicesNotRecorded);
    }
    if !e.confirmed {
        return Err(Refusal::NotConfirmed);
    }
    Ok(Release {
        root: e.root.clone(),
        held: e.held.as_ref().map(|h| (h.provider.clone(), h.identity.clone())),
        app_dir: e.app_dir.clone(),
        _seal: Seal,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn ok() -> Evidence {
        Evidence {
            root: PathBuf::from(r"C:\Users\u\OneDrive"),
            held: Some(Holder { provider: "OneDrive - Personal".into(), identity: b"theirs".to_vec(), ours: false }),
            left_behind: 0,
            account_removed: true,
            app_dir: PathBuf::from(r"C:\Users\u\AppData\Local\Microsoft\OneDrive"),
            app_running: 0,
            same_account: Some(true),
            scanned_at_ms: Some(1_000_000),
            listed_at_ms: Some(1_000_000),
            now_ms: 1_000_000 + FRESH_MS,
            in_cloud: (10, 2),
            missing: (0, 0),
            choices_recorded: true,
            confirmed: true,
        }
    }

    #[test]
    fn releases_only_with_every_piece_of_evidence() {
        let r = review_release(&ok()).unwrap();
        assert_eq!(r.held(), Some(("OneDrive - Personal", &b"theirs"[..])));
        let refused = |f: fn(&mut Evidence)| {
            let mut e = ok();
            f(&mut e);
            review_release(&e).unwrap_err()
        };
        assert_eq!(refused(|e| e.held.as_mut().unwrap().ours = true), Refusal::Ours);
        assert_eq!(refused(|e| e.account_removed = false), Refusal::NotUnlinking);
        assert_eq!(refused(|e| e.app_running = 2), Refusal::AppRunning(2));
        assert_eq!(refused(|e| e.same_account = Some(false)), Refusal::OtherAccount);
        assert_eq!(refused(|e| e.scanned_at_ms = None), Refusal::NotFresh);
        assert_eq!(refused(|e| e.listed_at_ms = Some(e.now_ms - FRESH_MS - 1)), Refusal::NotFresh);
        assert_eq!(refused(|e| e.scanned_at_ms = Some(e.now_ms + 1)), Refusal::NotFresh, "not from the future");
        assert_eq!(refused(|e| e.missing = (1, 0)), Refusal::NotInCloud(1, 0));
        assert_eq!(refused(|e| e.missing = (0, 3)), Refusal::NotInCloud(0, 3));
        assert_eq!(refused(|e| e.choices_recorded = false), Refusal::ChoicesNotRecorded);
        assert_eq!(refused(|e| e.confirmed = false), Refusal::NotConfirmed);
    }

    #[test]
    fn an_unknown_account_needs_the_check_to_have_found_something() {
        let mut e = ok();
        e.same_account = None;
        assert!(review_release(&e).is_ok());
        e.in_cloud = (0, 0);
        assert_eq!(review_release(&e).unwrap_err(), Refusal::NothingChecked);
    }

    #[test]
    fn placeholders_left_behind_are_released_again_on_the_same_evidence() {
        let mut e = ok();
        e.held = None;
        assert_eq!(review_release(&e).unwrap_err(), Refusal::NothingToRelease);
        e.left_behind = 5;
        assert_eq!(review_release(&e).unwrap().held(), None);
        e.missing = (1, 0);
        assert_eq!(review_release(&e).unwrap_err(), Refusal::NotInCloud(1, 0));
    }
}
