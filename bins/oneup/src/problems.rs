//! Everything that needs the user's attention, in one list: `oneup
//! problems` and the Activity page of the settings window.
//!
//! Nothing here is worked out anew. A running agent's status already says
//! what waits for the user (safety stops, a full version store, a problem
//! with its kind, pauses, items that can't sync), as the tray shows it; a
//! stopped one left why in [`crate::problem::STOPPED_FILE`]; the data
//! folder holds the rest (the sign-in, paused folders, the record of what
//! can't sync, the conflict copies in the activity log). [`gather`] reads
//! those for the main folder and each extra pair and vault; [`collect`]
//! turns them into the list, and is pure so it can be tested.

use std::path::{Path, PathBuf};

use oneup_ipc::{Call, ClientError, Status, Unsynced};
use serde_json::{Value, json};

use crate::tray::View;

/// How far back conflict copies are listed.
const CONFLICT_DAYS: i64 = 7;
/// At most this many conflict copies are listed.
const CONFLICTS_MAX: usize = 100;
/// The log's conflict entries are read this many at a time, at most
/// [`CONFLICT_PAGES`] times, so copies deleted since don't hide older ones
/// still there, and a log full of them still costs a bounded read.
const CONFLICT_PAGE: usize = 100;
const CONFLICT_PAGES: usize = 10;

/// What one data folder shows.
#[derive(Debug, Clone)]
pub struct Facts {
    /// An extra pair's or vault's name; `None` for the main folder.
    pub pair: Option<String>,
    pub dir: PathBuf,
    pub view: View,
    /// A folder pair is set up (`settingsapi::needs_setup`).
    pub set_up: bool,
    /// A sign-in is stored (the main folder's; pairs use it too).
    pub signed_in: bool,
    /// `[pause] folders`.
    pub paused_folders: Vec<String>,
    /// The record of what can't sync, read from the store: what the
    /// agent's status carries when it runs.
    pub unsynced: Option<Unsynced>,
    /// Conflict copies made lately and still there.
    pub conflicts: Vec<Conflict>,
    /// Whether there are conflict copies besides [`Facts::conflicts`].
    pub conflicts_more: More,
    /// An extra pair or vault that should be running: the main agent runs
    /// (it starts them), it wasn't asked to stop, and for a vault its keys
    /// are remembered (`launch::start_others`).
    pub expected: bool,
    /// The note that its agent was stopped on purpose is left over from
    /// before its last start (`problem::stale_stop_note`).
    pub stale_note: bool,
    /// Reading which extra pairs and vaults the main folder has failed
    /// (the main folder only), so they aren't checked.
    pub children_unreadable: Option<String>,
}

/// Whether conflict copies were left out of the list.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub enum More {
    /// The log was read to its end: none.
    #[default]
    No,
    /// More copies that are still there than the list holds.
    Yes,
    /// The search stopped before the end of the log: older entries
    /// weren't looked at.
    NotChecked,
}

/// A conflict copy: OneDrive's version kept the name, this PC's got `copy`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Conflict {
    /// Relative to the synced folder (`/docs/a.txt`).
    pub path: String,
    /// The copy's name, in the same folder.
    pub copy: String,
    pub at_ms: i64,
}

/// One thing that needs the user.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Item {
    /// An extra pair's or vault's name; `None` for the main folder.
    pub pair: Option<String>,
    /// What kind: `setup`, `signed_out`, `stopped`, `not_running`,
    /// `unreachable`, `safety_stop`, `version_store`, a problem's kind
    /// (`offline`, `signin`, `cloud-full`, `disk-full`, `folder`, `records`,
    /// `onedrive`, `other`), `paused`, `auto_paused`, `unsynced`,
    /// `retrying`, `excluded_in_onedrive`, `paused_folder`, `conflict`.
    pub kind: String,
    pub title: String,
    pub detail: String,
    /// What to do.
    pub action: String,
    /// The code an answer takes (`oneup approve`, `keep`, `discard`).
    pub code: Option<String>,
    /// The data folder it's about (commands for a pair take it as
    /// `--data-dir`).
    pub data_dir: PathBuf,
}

impl Item {
    fn new(kind: &str, title: impl Into<String>, detail: impl Into<String>, action: impl Into<String>) -> Self {
        Self {
            pair: None,
            kind: kind.into(),
            title: title.into(),
            detail: detail.into(),
            action: action.into(),
            code: None,
            data_dir: PathBuf::new(),
        }
    }

    pub fn to_json(&self) -> Value {
        json!({
            "pair": self.pair,
            "kind": self.kind,
            "title": self.title,
            "detail": self.detail,
            "action": self.action,
            "code": self.code,
            "data_dir": self.data_dir,
        })
    }
}

/// The list for one data folder, most pressing first: what stops all
/// syncing, then what waits for an answer, then the rest.
pub fn collect(f: &Facts, now_ms: i64) -> Vec<Item> {
    let mut out = Vec::new();
    // Nothing else applies before the main folder syncs anything. An extra
    // pair or vault without its records still has an agent that should run,
    // or that stopped saying why.
    if !f.set_up && f.pair.is_none() {
        let (title, detail, action) = crate::problem::Kind::Setup.words();
        out.push(Item::new("setup", title, detail, action));
        return tagged(out, f);
    }
    if let Some(e) = &f.children_unreadable {
        out.push(Item::new(
            "records",
            "Other folders not checked",
            "OneUpDrive couldn't read which other folder pairs and vaults it has, so they aren't listed here.",
            format!("Check that the pairs and vaults folders in {} can be opened. Details: {e}", f.dir.display()),
        ));
    }
    if f.pair.is_none() && !f.signed_in {
        out.push(Item::new(
            "signed_out",
            "Not signed in",
            "Nothing syncs until you sign in.",
            "Open Settings > Accounts and select Sign in, or run `oneup login`.",
        ));
    }
    let running = match &f.view {
        View::Running(s) => Some(s.as_ref()),
        View::Stopped(p) => {
            out.push(Item::new(
                "stopped",
                format!("Stopped: {}", p.title),
                p.detail.clone(),
                format!("{} `oneup start` starts it again.", p.action),
            ));
            None
        }
        View::NotRunning if f.pair.is_none() => {
            out.push(Item::new(
                "not_running",
                "Not running",
                "Nothing syncs in the background.",
                "Start OneUpDrive from its icon, or run `oneup start`.",
            ));
            None
        }
        // An extra pair or a vault starts with the main agent: one that
        // isn't running then failed to start or ended. One stopped on
        // purpose, or a locked vault, isn't a problem.
        View::NotRunning if f.expected => {
            out.push(Item::new(
                "not_running",
                "Not running",
                "It starts with the main folder, but isn't running: this folder doesn't sync.",
                format!(
                    "Start it on the Files page, or run `oneup --data-dir \"{}\" start`. If it stops again, \
                     `oneupd.log` in that folder says why.",
                    f.dir.display()
                ),
            ));
            None
        }
        View::Unreachable(e) => {
            out.push(Item::new(
                "unreachable",
                "Can't connect to OneUpDrive",
                format!("It runs, but doesn't answer: {e}."),
                "Restart it: `oneup stop`, then `oneup start`.",
            ));
            None
        }
        View::NotRunning | View::Connecting => None,
    };
    if let Some(s) = running {
        from_status(s, now_ms, &mut out);
    } else if let Some(u) = &f.unsynced {
        out.push(unsynced(u));
    }
    for folder in &f.paused_folders {
        let shown = folder.trim_matches('/');
        out.push(Item::new(
            "paused_folder",
            "Paused folder",
            format!("/{shown}: changes there wait."),
            format!("Resume it on the Files page, or run `oneup resume --folder \"{shown}\"`."),
        ));
    }
    for c in &f.conflicts {
        out.push(Item::new(
            "conflict",
            "Changed on both sides",
            format!("{}: both versions kept. This PC's is now {}.", c.path, c.copy),
            "Compare the two and delete the one you don't need.",
        ));
    }
    if f.stale_note {
        out.push(Item::new(
            "stop_note",
            "Old stop note not deleted",
            "OneUpDrive couldn't delete its note that this agent was stopped on purpose. It's ignored.",
            format!("Delete {} when you can.", f.dir.join(crate::problem::ASKED_FILE).display()),
        ));
    }
    match f.conflicts_more {
        More::Yes => out.push(Item::new(
            "more_conflicts",
            "More files changed on both sides",
            format!("Only the newest {CONFLICTS_MAX} are listed."),
            "`oneup activity` lists them all (as \"keep both\").",
        )),
        More::NotChecked => out.push(Item::new(
            "conflicts_not_checked",
            "Older conflicts not checked",
            format!("Only the newest {} conflict entries of the log were checked.", CONFLICT_PAGE * CONFLICT_PAGES),
            "`oneup activity` lists them all (as \"keep both\").",
        )),
        More::No => {}
    }
    tagged(out, f)
}

fn tagged(mut out: Vec<Item>, f: &Facts) -> Vec<Item> {
    for i in &mut out {
        i.pair.clone_from(&f.pair);
        i.data_dir.clone_from(&f.dir);
    }
    out
}

/// What a running agent's status says needs the user (what the tray shows).
fn from_status(s: &Status, now_ms: i64, out: &mut Vec<Item>) {
    if let Some(p) = &s.problem {
        out.push(Item::new(&p.kind, p.title.clone(), p.detail.clone(), p.action.clone()));
    }
    for st in &s.safety_stops {
        let notice = crate::tray::stop_notice(st);
        out.push(Item {
            code: Some(st.code.clone()),
            ..Item::new(
                "safety_stop",
                notice.title,
                notice.text,
                format!(
                    "See the items on the Activity page (Safety stops). If that's what you meant, run `oneup approve {}`.",
                    st.code
                ),
            )
        });
    }
    if let Some(o) = &s.overflow
        && o.waiting
    {
        let size = crate::versions::size;
        out.push(Item {
            code: Some(o.code.clone()),
            ..Item::new(
                "version_store",
                "Version store full",
                format!(
                    "{} removed files ({}) don't fit (room for {}). They're untouched on this PC.",
                    o.count,
                    size(o.bytes),
                    size(o.room)
                ),
                format!(
                    "`oneup keep {0}` keeps them anyway; `oneup discard {0}` removes them without a local copy.",
                    o.code
                ),
            )
        });
    }
    if s.paused.local || s.paused.remote {
        let what = match (s.paused.local, s.paused.remote) {
            (true, false) => "Changes to this PC wait; OneDrive still gets yours.",
            (false, true) => "Changes to OneDrive wait; this PC still gets OneDrive's.",
            _ => "Nothing syncs.",
        };
        let until = s
            .paused
            .until_ms
            .map(|u| {
                format!(" Resumes by itself in {}.", crate::tray::time_left(u64::try_from(u - now_ms).unwrap_or(0)))
            })
            .unwrap_or_default();
        out.push(Item::new(
            "paused",
            "Paused",
            format!("{what}{until}"),
            "Resume from the icon, or run `oneup resume`.",
        ));
    }
    if let Some(why) = &s.auto_pause {
        out.push(Item::new(
            "auto_paused",
            "Paused by itself",
            format!("{why}."),
            "Resumes when that ends. `oneup settings` changes when it pauses.",
        ));
    }
    if let Some(u) = &s.unsynced {
        out.push(unsynced(u));
    }
    if let Some(r) = &s.retrying {
        out.push(Item::new(
            "retrying",
            format!("Failed: {} items", r.count),
            format!("Tried again in {}.", crate::tray::time_left(u64::try_from(r.next_ms - now_ms).unwrap_or(0))),
            "`oneup activity` says why.",
        ));
    }
    if let Some(o) = &s.prune_offer {
        out.push(Item {
            code: Some(o.code.clone()),
            ..Item::new(
                "excluded_in_onedrive",
                "Excluded items in OneDrive",
                format!("{} items excluded by your rules are still in OneDrive.", o.items),
                "`oneup cloud prune` lists them; `--yes` removes them from OneDrive, `--keep` keeps them there.",
            )
        });
    }
}

fn unsynced(u: &Unsynced) -> Item {
    Item::new(
        "unsynced",
        format!("Sync failure: {}", crate::tray::unsynced_count(u)),
        "These can't sync until you change them.",
        "See them on the Activity page (Sync failures), or run `oneup unsynced`.",
    )
}

/// The facts of the main data folder `main` and of each extra pair and
/// vault. Asks each running agent for its status (a short call each).
pub async fn gather(main: &Path) -> Vec<Facts> {
    // Given a pair's or vault's own data folder: that one alone, judged
    // against its main folder's agent all the same.
    if crate::vaultpair::is_child_dir(main) {
        let top = crate::vaultpair::main_dir(main);
        let main_runs = status(&top).await.is_ok();
        let same = |d: &Path| crate::test_account::same_dir(d, main);
        let found = children(&top).unwrap_or_default().into_iter().find(|(_, d, _)| same(d));
        let name = main.file_name().map(|n| n.to_string_lossy().into_owned()).unwrap_or_default();
        let mut f = facts(main, Some(name)).await;
        f.expected = found.is_some_and(|(_, d, starts)| should_run(main_runs, starts, &d));
        return vec![f];
    }
    let mut all = vec![facts(main, None).await];
    let main_runs = matches!(all[0].view, View::Running(_));
    let found = children(main).unwrap_or_else(|e| {
        all[0].children_unreadable = Some(e.to_string());
        Vec::new()
    });
    for (name, dir, starts) in found {
        let mut f = facts(&dir, Some(name)).await;
        f.expected = should_run(main_runs, starts, &dir);
        all.push(f);
    }
    all
}

/// The extra pairs and vaults of `main`: name, data folder, and whether the
/// main agent starts it (`launch::start_others`: every pair, and the vaults
/// whose keys are remembered; a locked vault waits for its password).
/// Strict: a folder that can't be read is an error, not "none", so the list
/// can say they weren't checked rather than leave them out silently.
fn children(main: &Path) -> std::io::Result<Vec<(String, PathBuf, bool)>> {
    let pairs = crate::pairs::list_strict(main)?.into_iter().map(|p| (p.name, p.dir, true));
    let vaults = crate::vaultpair::list_strict(main)?.into_iter().map(|v| {
        let remembered = v.dir.join(crate::vaultpair::KEY_FILE).exists();
        (v.name, v.dir, remembered)
    });
    Ok(pairs.chain(vaults).collect())
}

/// Whether the agent of the pair or vault in `dir` should be running: the
/// main agent runs, starts it, and it wasn't asked to stop since.
fn should_run(main_runs: bool, starts_with_main: bool, dir: &Path) -> bool {
    main_runs && starts_with_main && !crate::problem::asked_to_stop(dir)
}

/// Every item of every data folder of `main`.
pub async fn list(main: &Path) -> Vec<Item> {
    let now = crate::control::now_ms();
    gather(main).await.iter().flat_map(|f| collect(f, now)).collect()
}

async fn facts(dir: &Path, pair: Option<String>) -> Facts {
    let view = match status(dir).await {
        Ok(s) => View::Running(Box::new(s)),
        Err(ClientError::NotRunning) => match crate::problem::load_stopped(dir) {
            Some(p) => View::Stopped(Box::new(p)),
            None => View::NotRunning,
        },
        Err(e) => View::Unreachable(e.to_string()),
    };
    let set_up = !crate::settingsapi::needs_setup(dir);
    let (unsynced, (conflicts, conflicts_more)) = if set_up { from_store(dir) } else { (None, (Vec::new(), More::No)) };
    Facts {
        signed_in: dir.join("token.dpapi").exists(),
        paused_folders: crate::config::paused_folders(dir),
        pair,
        dir: dir.to_path_buf(),
        view,
        set_up,
        unsynced,
        conflicts,
        conflicts_more,
        expected: false,
        stale_note: crate::problem::stale_stop_note(dir),
        children_unreadable: None,
    }
}

async fn status(dir: &Path) -> Result<Status, ClientError> {
    oneup_ipc::connect(dir).await?.call(&Call::Status).await
}

/// What the store records: what can't sync, and the conflict copies of the
/// last [`CONFLICT_DAYS`] still in the synced folder. Unreadable: nothing
/// (the rest of the list still helps).
fn from_store(dir: &Path) -> (Option<Unsynced>, (Vec<Conflict>, More)) {
    let Ok(store) = crate::setup::open_store(dir, 1024) else { return (None, (Vec::new(), More::No)) };
    let unsynced = crate::unsynced::load(&store).ok().and_then(|r| crate::unsynced::count(&r));
    let since = crate::control::now_ms() - CONFLICT_DAYS * 24 * 3600 * 1000;
    let root = crate::sync::pairing(&store).ok().map(|p| p.local);
    let conflicts = conflicts(&store, since, CONFLICTS_MAX, |c| root.as_deref().is_none_or(|r| copy_exists(r, c)));
    (unsynced, conflicts)
}

/// The newest `max` conflict copies logged since `since_ms` that are
/// `still_there`, and whether there are more: the log is read a page at a
/// time until one more than `max` is found or it ends, at most
/// [`CONFLICT_PAGES`] pages (stopping there: [`More::NotChecked`]). A log
/// that can't be read gives none.
fn conflicts(
    store: &oneup_store::Store,
    since_ms: i64,
    max: usize,
    still_there: impl Fn(&Conflict) -> bool,
) -> (Vec<Conflict>, More) {
    let mut found = Vec::new();
    let mut before = None;
    for _ in 0..CONFLICT_PAGES {
        let read = store.read(|o| o.activity_done(crate::sync::CONFLICT_ACTION, since_ms, before, CONFLICT_PAGE));
        let Ok(page) = read else { return (found, More::No) };
        let last = page.len() < CONFLICT_PAGE;
        for e in page {
            before = Some(e.seq);
            let Some(copy) = crate::sync::conflict_copy_name(&e.detail).map(str::to_string) else { continue };
            let c = Conflict { path: e.path, copy, at_ms: e.at_ms };
            if still_there(&c) {
                if found.len() == max {
                    return (found, More::Yes);
                }
                found.push(c);
            }
        }
        if last {
            return (found, More::No);
        }
    }
    (found, More::NotChecked)
}

/// Whether the conflict copy is still in the synced folder `root`. Looks
/// at the name only (no data access, so a placeholder isn't downloaded).
fn copy_exists(root: &Path, c: &Conflict) -> bool {
    let mut p = root.to_path_buf();
    let parts: Vec<&str> = c.path.split('/').filter(|s| !s.is_empty()).collect();
    let Some((_, folders)) = parts.split_last() else { return false };
    folders.iter().for_each(|f| p.push(f));
    p.push(&c.copy);
    std::fs::symlink_metadata(&p).is_ok()
}

/// `oneup problems`: the list, as text or JSON.
pub async fn show(main: &Path, as_json: bool) -> anyhow::Result<()> {
    let items = list(main).await;
    if as_json {
        let all: Vec<Value> = items.iter().map(Item::to_json).collect();
        // Also items of their own; here for scripts that only look for these.
        let kind = |k: &str| items.iter().any(|i| i.kind == k);
        let out = json!({
            "problems": all,
            "more_conflicts": kind("more_conflicts"),
            "conflicts_not_checked": kind("conflicts_not_checked"),
        });
        println!("{}", serde_json::to_string_pretty(&out)?);
        return Ok(());
    }
    if items.is_empty() {
        println!("Nothing needs your attention.");
        return Ok(());
    }
    // Headings only when extra pairs or vaults have something too.
    let headed = items.iter().any(|i| i.pair.is_some());
    let mut pair: Option<&Option<String>> = None;
    for i in &items {
        if headed && pair != Some(&i.pair) {
            pair = Some(&i.pair);
            match &i.pair {
                Some(name) => println!("{name} (commands take --data-dir \"{}\"):", i.data_dir.display()),
                None => println!("Main folder:"),
            }
        }
        println!("  {}", i.title);
        println!("    {}", i.detail);
        println!("    {}", i.action);
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use oneup_ipc::{Activity, Overflow, PauseState, Retrying, SafetyStop};

    fn status() -> Status {
        Status {
            version: "0".into(),
            rev: 1,
            local: String::new(),
            remote: String::new(),
            activity: Activity::Idle,
            paused: PauseState::default(),
            push: true,
            checked_ms: None,
            last: None,
            safety_stops: Vec::new(),
            overflow: None,
            problem: None,
            auto_pause: None,
            prune_offer: None,
            progress: None,
            unsynced: None,
            retrying: None,
        }
    }

    fn facts(view: View) -> Facts {
        Facts {
            pair: None,
            dir: PathBuf::new(),
            view,
            set_up: true,
            signed_in: true,
            paused_folders: Vec::new(),
            unsynced: None,
            conflicts: Vec::new(),
            conflicts_more: More::No,
            expected: false,
            stale_note: false,
            children_unreadable: None,
        }
    }

    fn kinds(items: &[Item]) -> Vec<&str> {
        items.iter().map(|i| i.kind.as_str()).collect()
    }

    #[test]
    fn an_agent_up_to_date_with_nothing_waiting_lists_nothing() {
        assert!(collect(&facts(View::Running(Box::new(status()))), 0).is_empty());
    }

    /// Everything a running agent's status says needs the user, most
    /// pressing first, with the codes answers take.
    #[test]
    fn what_the_status_says_needs_the_user_is_listed_with_its_codes() {
        let mut s = status();
        s.problem = Some(crate::problem::cloud_full(0, None, 60_000));
        s.safety_stops.push(SafetyStop {
            kind: "mass".into(),
            side: "local".into(),
            count: 12,
            at_least: false,
            recent: 0,
            synced_items: 100,
            code: "00000000000000ab".into(),
        });
        s.overflow =
            Some(Overflow { count: 3, bytes: 5 << 20, room: 1 << 20, code: "00000000000000cd".into(), waiting: true });
        s.paused = PauseState { local: true, remote: false, until_ms: Some(30 * 60_000) };
        s.auto_pause = Some("battery saver is on".into());
        s.unsynced = Some(Unsynced { count: 4, more: false });
        s.retrying = Some(Retrying { count: 2, next_ms: 60_000 });
        let mut f = facts(View::Running(Box::new(s)));
        f.paused_folders = vec!["Videos".into()];
        f.conflicts = vec![Conflict { path: "/docs/a.txt".into(), copy: "a (1).txt".into(), at_ms: 0 }];
        // The store's record isn't used while the agent runs: its status
        // has the same, fresher.
        f.unsynced = Some(Unsynced { count: 99, more: true });
        let items = collect(&f, 0);
        assert_eq!(
            kinds(&items),
            [
                "cloud-full",
                "safety_stop",
                "version_store",
                "paused",
                "auto_paused",
                "unsynced",
                "retrying",
                "paused_folder",
                "conflict"
            ]
        );
        assert_eq!(items[1].code.as_deref(), Some("00000000000000ab"));
        assert!(items[1].action.contains("oneup approve 00000000000000ab"), "{}", items[1].action);
        assert!(items[2].action.contains("oneup keep 00000000000000cd"));
        assert!(items[3].detail.contains("this PC") && items[3].detail.contains("30 min"), "{}", items[3].detail);
        assert_eq!(items[5].title, "Sync failure: 4 items");
        assert!(items[7].action.contains("oneup resume --folder \"Videos\""));
        assert!(items[8].detail.contains("/docs/a.txt") && items[8].detail.contains("a (1).txt"));
        assert!(items.iter().all(|i| i.pair.is_none()));
    }

    /// Without a running agent: why it stopped, or that it isn't running,
    /// and the store's record of what can't sync.
    #[test]
    fn without_an_agent_its_state_and_the_stores_record_are_listed() {
        let mut f = facts(View::NotRunning);
        f.unsynced = Some(Unsynced { count: 1, more: false });
        assert_eq!(kinds(&collect(&f, 0)), ["not_running", "unsynced"]);
        let stopped = crate::problem::describe(&anyhow::anyhow!(crate::sync::Halted("moved".into())), None, 0, None);
        f.view = View::Stopped(Box::new(stopped));
        let items = collect(&f, 0);
        assert_eq!(kinds(&items), ["stopped", "unsynced"]);
        assert!(items[0].title.starts_with("Stopped: ") && items[0].action.contains("oneup start"));
        f.view = View::Unreachable("pipe busy".into());
        assert_eq!(kinds(&collect(&f, 0)), ["unreachable", "unsynced"]);
    }

    /// An extra pair or vault that isn't running: a problem when it should
    /// be (the main agent runs and it wasn't asked to stop, or for a vault,
    /// its keys are remembered), not when it was stopped on purpose or is
    /// locked. Its items carry its name.
    #[test]
    fn a_pair_not_running_is_a_problem_only_when_it_should_be() {
        let mut f = facts(View::NotRunning);
        f.pair = Some("Photos".into());
        f.signed_in = false;
        assert!(collect(&f, 0).is_empty(), "stopped on purpose; a pair uses the main folder's sign-in");
        f.paused_folders = vec!["/Raw/".into()];
        let items = collect(&f, 0);
        assert_eq!(kinds(&items), ["paused_folder"]);
        assert_eq!(items[0].pair.as_deref(), Some("Photos"));
        assert_eq!(items[0].detail, "/Raw: changes there wait.");
        f.expected = true;
        f.dir = PathBuf::from("pairs").join("Photos");
        let items = collect(&f, 0);
        assert_eq!(kinds(&items), ["not_running", "paused_folder"]);
        assert!(items[0].action.contains("--data-dir") && items[0].action.contains("start"), "{}", items[0].action);
    }

    /// An extra pair or vault without its records (its database gone, or
    /// never paired) that should run is still listed: not running, or why
    /// it stopped. Only the main folder stops at "nothing to sync yet".
    #[test]
    fn a_pair_without_its_records_that_should_run_is_still_listed() {
        let mut f = facts(View::NotRunning);
        f.pair = Some("Photos".into());
        f.set_up = false;
        assert!(collect(&f, 0).is_empty(), "stopped on purpose");
        f.expected = true;
        assert_eq!(kinds(&collect(&f, 0)), ["not_running"]);
        let stopped = crate::problem::describe(&anyhow::anyhow!(crate::sync::NotPaired), None, 0, None);
        f.view = View::Stopped(Box::new(stopped));
        let items = collect(&f, 0);
        assert_eq!(kinds(&items), ["stopped"]);
        assert_eq!(items[0].title, "Stopped: Nothing to sync yet");
    }

    /// Stopped, then a start asked for that fails (here, nothing runs after
    /// it): the stop no longer counts, so the pair is listed. A note that
    /// can't be removed fails the start instead of hiding the pair.
    #[tokio::test]
    async fn a_pair_whose_start_failed_after_a_stop_is_listed() {
        use std::os::windows::fs::OpenOptionsExt as _;
        let main = std::env::temp_dir().join(format!("oneup-problems-restart-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&main);
        let dir = main.join(crate::pairs::PAIRS).join("Photos");
        std::fs::create_dir_all(&dir).unwrap_or_default();
        std::fs::write(dir.join(crate::pairs::MARKER), "/Photos").unwrap_or_default();
        crate::problem::note_asked_to_stop(&dir).unwrap_or_default();
        assert!(!should_run(true, true, &dir), "stopped on purpose");
        // The launcher's first step; whatever happens next, it counts.
        crate::problem::start_requested(&dir).unwrap_or_default();
        assert!(should_run(true, true, &dir));
        let mut all = gather(&main).await;
        all[1].expected = should_run(true, true, &dir);
        assert_eq!(kinds(&collect(&all[1], 0)), ["not_running"]);
        // Asked about the pair's own data folder: judged by its main
        // folder's agent, which doesn't run here.
        let alone = gather(&dir).await;
        assert_eq!((alone.len(), alone[0].pair.as_deref(), alone[0].expected), (1, Some("Photos"), false));
        // Removing the note fails (held open, not shared for deleting):
        // the start fails with it, rather than leave a stale stop.
        crate::problem::note_asked_to_stop(&dir).unwrap_or_default();
        let held = std::fs::OpenOptions::new()
            .read(true)
            .share_mode(0)
            .open(dir.join(crate::problem::ASKED_FILE))
            .unwrap_or_else(|e| panic!("{e}"));
        assert!(crate::problem::start_requested(&dir).is_err());
        assert!(crate::launch::start_agent(&dir).is_err());
        drop(held);
        let _ = std::fs::remove_dir_all(&main);
    }

    /// A stop note the agent's last start couldn't delete is older than
    /// that start: it no longer counts, and the list says it's there.
    #[tokio::test]
    async fn a_stop_note_older_than_the_last_start_doesnt_count() {
        use std::time::{Duration, SystemTime};
        let dir = std::env::temp_dir().join(format!("oneup-problems-stale-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap_or_default();
        let at = |f: &str, t: SystemTime| {
            let file = std::fs::OpenOptions::new().write(true).open(dir.join(f)).unwrap_or_else(|e| panic!("{e}"));
            file.set_modified(t).unwrap_or_else(|e| panic!("{e}"));
        };
        let t0 = SystemTime::now() - Duration::from_secs(60);
        // The name a stopped agent left behind (it reads as not running).
        let gone = format!(r"\\.\pipe\oneupdrive-{}", "0".repeat(32));
        std::fs::write(dir.join(oneup_ipc::PIPE_FILE), gone).unwrap_or_default();
        at(oneup_ipc::PIPE_FILE, t0);
        crate::problem::note_asked_to_stop(&dir).unwrap_or_default();
        // Started, then asked to stop: it counts.
        assert!(crate::problem::asked_to_stop(&dir) && !crate::problem::stale_stop_note(&dir));
        // Started again since, without deleting it: left over.
        at(crate::problem::ASKED_FILE, t0 - Duration::from_secs(30));
        assert!(!crate::problem::asked_to_stop(&dir) && crate::problem::stale_stop_note(&dir));
        assert!(should_run(true, true, &dir), "a pair it would have hidden is listed again");
        let f = super::facts(&dir, Some("Photos".into())).await;
        let items = collect(&Facts { expected: true, ..f }, 0);
        assert!(kinds(&items).contains(&"stop_note") && kinds(&items).contains(&"not_running"), "{items:?}");
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// A pairs folder that can't be read hides no pair silently: the main
    /// folder says the others weren't checked.
    #[tokio::test]
    async fn an_unreadable_pairs_folder_is_told() {
        let main = std::env::temp_dir().join(format!("oneup-problems-unreadable-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&main);
        std::fs::create_dir_all(&main).unwrap_or_default();
        // A file where the folder should be: listing it fails.
        std::fs::write(main.join(crate::pairs::PAIRS), b"not a folder").unwrap_or_default();
        assert!(children(&main).is_err());
        let all = gather(&main).await;
        assert_eq!(all.len(), 1);
        assert!(all[0].children_unreadable.is_some());
        let mut f = all[0].clone();
        f.set_up = true;
        let items = collect(&f, 0);
        assert_eq!(items[0].kind, "records");
        assert_eq!(items[0].title, "Other folders not checked");
        let _ = std::fs::remove_dir_all(&main);
    }

    /// Which extra pairs and vaults should be running, from the data
    /// folders: a pair or a remembered vault whose agent wasn't asked to
    /// stop; not a locked vault, nor one stopped on purpose; none while the
    /// main agent isn't running (starting it starts them).
    #[tokio::test]
    async fn which_pairs_and_vaults_should_be_running() {
        let main = std::env::temp_dir().join(format!("oneup-problems-pairs-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&main);
        let child = |kind: &str, name: &str, marker: &str| {
            let d = main.join(kind).join(name);
            std::fs::create_dir_all(&d).unwrap_or_default();
            std::fs::write(d.join(marker), "/x").unwrap_or_default();
            d
        };
        let pair = child(crate::pairs::PAIRS, "Photos", crate::pairs::MARKER);
        let stopped = child(crate::pairs::PAIRS, "Stopped", crate::pairs::MARKER);
        crate::problem::note_asked_to_stop(&stopped).unwrap_or_default();
        let remembered = child(crate::vaultpair::VAULTS, "Kept", crate::vaultpair::MARKER);
        std::fs::write(remembered.join(crate::vaultpair::KEY_FILE), b"k").unwrap_or_default();
        child(crate::vaultpair::VAULTS, "Locked", crate::vaultpair::MARKER);
        let should = |main_runs: bool| -> Vec<String> {
            children(&main)
                .unwrap_or_default()
                .into_iter()
                .filter(|(_, d, starts)| should_run(main_runs, *starts, d))
                .map(|c| c.0)
                .collect()
        };
        assert_eq!(should(true), ["Photos", "Kept"], "not the stopped pair, nor the locked vault");
        assert!(should(false).is_empty(), "none while the main agent isn't running");
        let all = gather(&main).await;
        assert_eq!(all.len(), 5, "the main folder, two pairs and two vaults");
        assert!(all.iter().all(|f| !f.expected), "no agent runs here");
        // Starting again clears the note.
        crate::problem::clear_stopped(&stopped);
        assert_eq!(should(true), ["Photos", "Stopped", "Kept"]);
        assert_eq!(pair.file_name(), Some(std::ffi::OsStr::new("Photos")));
        assert!(remembered.join(crate::vaultpair::KEY_FILE).exists());
        let _ = std::fs::remove_dir_all(&main);
    }

    /// Conflict copies deleted since don't hide older ones still there:
    /// the log is read page after page, up to the limit, and what's left
    /// over is told.
    #[test]
    fn deleted_conflict_copies_dont_hide_older_ones_and_the_rest_is_told() {
        let mut store = oneup_store::Store::open_in_memory().expect("store");
        let entry = |n: usize| oneup_store::ActivityEntry {
            seq: 0,
            at_ms: n as i64,
            outcome: oneup_store::ActivityOutcome::Done,
            action: crate::sync::CONFLICT_ACTION.into(),
            path: format!("/f{n:04}.txt"),
            detail: format!("-> f{n:04} (1).txt; changed on both sides"),
        };
        // Oldest first: 0..250. The newest 150 (100..250) were deleted.
        let all: Vec<_> = (0..250).map(entry).collect();
        store.write(|o| o.activity_add(&all)).expect("log");
        let there = |c: &Conflict| c.path.as_str() < "/f0100.txt";
        let (found, more) = conflicts(&store, 0, 10, there);
        assert_eq!(found.len(), 10);
        assert_eq!(found[0].path, "/f0099.txt", "the newest still there, past a page of deleted ones");
        assert_eq!(more, More::Yes, "more still there");
        let (found, more) = conflicts(&store, 0, 100, there);
        assert_eq!((found.len(), more), (100, More::No), "exactly all of them");
        // A log of more than the pages read, every copy in them deleted:
        // not "more", but "not checked".
        let many: Vec<_> = (250..250 + CONFLICT_PAGE * CONFLICT_PAGES).map(entry).collect();
        store.write(|o| o.activity_add(&many)).expect("log");
        let (found, more) = conflicts(&store, 0, 100, there);
        assert_eq!((found.len(), more), (0, More::NotChecked));
        let mut f = facts(View::Running(Box::new(status())));
        f.conflicts_more = More::Yes;
        assert_eq!(kinds(&collect(&f, 0)), ["more_conflicts"]);
        f.conflicts_more = More::NotChecked;
        let items = collect(&f, 0);
        assert_eq!(kinds(&items), ["conflicts_not_checked"]);
        assert_eq!(items[0].title, "Older conflicts not checked");
    }

    #[test]
    fn nothing_set_up_or_not_signed_in_comes_first() {
        let mut f = facts(View::NotRunning);
        f.set_up = false;
        f.paused_folders = vec!["x".into()];
        assert_eq!(kinds(&collect(&f, 0)), ["setup"]);
        f.set_up = true;
        f.signed_in = false;
        assert_eq!(kinds(&collect(&f, 0)), ["signed_out", "not_running", "paused_folder"]);
    }

    #[test]
    fn items_become_json_with_their_codes() {
        let i = Item { code: Some("ab".into()), ..Item::new("safety_stop", "t", "d", "a") };
        assert_eq!(
            i.to_json(),
            json!({
                "pair": null,
                "kind": "safety_stop",
                "title": "t",
                "detail": "d",
                "action": "a",
                "code": "ab",
                "data_dir": "",
            })
        );
    }

    /// The data folder's own records, read without an agent: what can't
    /// sync, paused folders, and conflict copies still there.
    #[tokio::test]
    async fn a_data_folders_records_are_read_without_an_agent() {
        let dir = std::env::temp_dir().join(format!("oneup-problems-data-{}", std::process::id()));
        let synced = dir.join("synced");
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(synced.join("docs")).unwrap_or_default();
        std::fs::write(dir.join("token.dpapi"), b"x").unwrap_or_default();
        std::fs::write(
            dir.join("config.toml"),
            "[pause]
folders = [\"Videos\"]
",
        )
        .unwrap_or_default();
        std::fs::write(synced.join("docs").join("a (1).txt"), b"x").unwrap_or_default();
        {
            let mut store = crate::setup::open_store(&dir, 1024).expect("store");
            let entry = |path: &str, copy: &str| oneup_store::ActivityEntry {
                seq: 0,
                at_ms: crate::control::now_ms(),
                outcome: oneup_store::ActivityOutcome::Done,
                action: crate::sync::CONFLICT_ACTION.into(),
                path: path.into(),
                detail: format!("-> {copy}; changed on both sides"),
            };
            store
                .write(|o| {
                    o.meta_set(crate::sync::KEY_LOCAL, &synced.to_string_lossy())?;
                    o.meta_set(crate::sync::KEY_REMOTE, "/")?;
                    // One copy still there, one deleted since.
                    o.activity_add(&[entry("/docs/a.txt", "a (1).txt"), entry("/b.txt", "b (1).txt")])
                })
                .expect("write");
            crate::unsynced::update(
                &mut store,
                &synced,
                &oneup_rules::RuleSet::new(&[] as &[&str], &[] as &[&str]).expect("rules"),
                [("/gone.txt".to_string(), crate::unsynced::Why::GoneHere)],
                [],
                false,
            )
            .expect("unsynced");
        }
        let all = gather(&dir).await;
        assert_eq!(all.len(), 1);
        let items = collect(&all[0], crate::control::now_ms());
        assert_eq!(kinds(&items), ["not_running", "unsynced", "paused_folder", "conflict"]);
        assert!(items[3].detail.starts_with("/docs/a.txt:"), "{}", items[3].detail);
        assert!(items.iter().all(|i| i.data_dir == dir));
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// A conflict copy counts only while it's still there, found by name
    /// in the original's folder.
    #[test]
    fn a_conflict_copy_counts_while_it_is_there() {
        let root = std::env::temp_dir().join(format!("oneup-problems-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&root);
        std::fs::create_dir_all(root.join("docs")).unwrap_or_default();
        let c = Conflict { path: "/docs/a.txt".into(), copy: "a (1).txt".into(), at_ms: 0 };
        assert!(!copy_exists(&root, &c));
        std::fs::write(root.join("docs").join("a (1).txt"), b"x").unwrap_or_default();
        assert!(copy_exists(&root, &c));
        let _ = std::fs::remove_dir_all(&root);
    }
}
