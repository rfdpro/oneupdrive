//! Database files (`[databases]` in `config.toml`, `oneup databases`, the
//! settings window's **Files → Databases**; `docs/architecture/databases.md`).
//!
//! The executor holds a database back until it's in a state to sync
//! (`oneup_effects::DbState`); this turns what it reported into what the
//! user sees: the activity log, `oneup status`, `oneup explain` and the
//! problems list. Outlook data files and virtual disks are also *warned*
//! about, with an offer to exclude them (`oneup databases --exclude`):
//! never excluded without the user's say.

use oneup_effects::{DbState, Wait};
use oneup_exec::Report;
use oneup_ipc::DatabaseNote;
use oneup_rules::database::Format;
use oneup_sync::Op;

/// Files warned about, at most (each costs a line in the status).
pub const MAX_LIVE: usize = 100;

/// The formats by their settings name.
const FORMATS: [Format; 7] = [
    Format::Sqlite,
    Format::KeePass,
    Format::Access,
    Format::Thunderbird,
    Format::Outlook,
    Format::VirtualDisk,
    Format::Other,
];

pub fn format_named(name: &str) -> Option<Format> {
    FORMATS.into_iter().find(|f| f.name() == name)
}

fn why_name(w: &Wait) -> &'static str {
    match w {
        Wait::Wal => "wal",
        Wait::Journal => "journal",
        Wait::Open => "open",
        Wait::Behind => "behind",
    }
}

/// What a skipped operation's database note says, for the round's report.
pub fn note(path: String, op: &Op, db: &DbState) -> Option<DatabaseNote> {
    let direction = match op {
        Op::Upload { .. } => "up",
        // OneDrive's change, waiting to be made here.
        Op::Download { .. }
        | Op::MoveLocal { .. }
        | Op::RenameLocal { .. }
        | Op::ConflictCopy { .. }
        | Op::RecycleLocal { .. } => "down",
        _ => return None,
    };
    let mut n = DatabaseNote {
        path,
        format: db.recognised.format.name().into(),
        why: why_name(db.wait.as_ref()?).into(),
        programs: db.programs.clone(),
        direction: Some(direction.into()),
        detail: String::new(),
    };
    n.detail = detail(&n);
    Some(n)
}

/// The programs, by their friendlier names: "KeePass, Word".
pub fn programs(p: &[String]) -> String {
    let mut names: Vec<String> = p.iter().map(|e| crate::apps::display_name(e)).collect();
    names.dedup();
    names.join(", ")
}

/// What the user reads about `n`: the activity log's detail, `oneup
/// status`, the window.
pub fn detail(n: &DatabaseNote) -> String {
    let label = format_named(&n.format).map_or("database", Format::label);
    let who = if n.programs.is_empty() { None } else { Some(programs(&n.programs)) };
    let open_in = who.as_ref().map(|w| format!(", open in {w}")).unwrap_or_default();
    let down = n.direction.as_deref() == Some("down");
    match n.why.as_str() {
        "open" => {
            let who = who.unwrap_or_else(|| "another program".into());
            if down {
                format!("{label} open in {who}; OneDrive's newer version waits until it's closed")
            } else {
                format!("{label} open in {who}; sent once it's closed")
            }
        }
        "behind" => {
            format!(
                "{label}'s working file from OneDrive; placed once the database here is synced at OneDrive's version"
            )
        }
        "wal" | "journal" => {
            let what = if n.why == "wal" {
                "changes not yet written into it (its -wal file)"
            } else {
                "a transaction under way (its -journal file)"
            };
            if down {
                format!("{label} with {what}{open_in}; OneDrive's newer version waits until they're in it")
            } else {
                format!("{label} with {what}{open_in}; sent once they're in it")
            }
        }
        _ => format!(
            "{label}: doesn't sync safely while in use, so it syncs only while closed; \
             `oneup databases --exclude` excludes it"
        ),
    }
}

/// The text of a skip the executor reported for `op` with a database
/// note, if it did (the activity log and the round's output).
pub fn skip_detail(r: &Report, op: &Op) -> Option<String> {
    let (_, db) = r.databases.iter().find(|(o, _)| o == op)?;
    note(String::new(), op, db).map(|n| detail(&n))
}

/// Whether `op` sends or fetches a file that shouldn't sync while in use
/// (by its name, or by what the executor found), and which format.
pub fn live_format(r: &Report, op: &Op) -> Option<Format> {
    let (Op::Upload { name, .. } | Op::Download { name, .. }) = op else { return None };
    let held = r.databases.iter().find(|(o, _)| o == op).map(|(_, d)| d.recognised.format);
    let sent = || r.recognised.iter().find(|(o, _)| o == op).map(|(_, f)| *f);
    held.or_else(sent).or_else(|| Format::of_name(name)).filter(|f| f.never_live())
}

/// A warning about a file that shouldn't sync while in use.
pub fn live_note(path: String, format: Format) -> DatabaseNote {
    let mut n = DatabaseNote {
        path,
        format: format.name().into(),
        why: "live".into(),
        programs: Vec::new(),
        direction: None,
        detail: String::new(),
    };
    n.detail = detail(&n);
    n
}

/// A path relative to the synced folder as the status writes it:
/// `Mail\x.pst` → `/Mail/x.pst`.
pub fn rel_path(p: &str) -> String {
    let parts: Vec<&str> = p.split(['\\', '/']).filter(|p| !p.is_empty() && *p != ".").collect();
    format!("/{}", parts.join("/"))
}

/// Whether `path` is one the user chose to keep syncing (`keep_syncing`).
pub fn kept_syncing(keep: &[String], path: &str) -> bool {
    keep.iter().any(|k| oneup_sync::same_name(k.trim_end_matches('/'), path))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn n(why: &str, dir: Option<&str>, programs: &[&str]) -> DatabaseNote {
        DatabaseNote {
            path: "/a.db".into(),
            format: "sqlite".into(),
            why: why.into(),
            programs: programs.iter().map(|p| p.to_string()).collect(),
            direction: dir.map(str::to_string),
            detail: String::new(),
        }
    }

    #[test]
    fn notes_say_what_waits_and_for_which_program() {
        assert_eq!(
            detail(&n("wal", Some("up"), &["Lightroom.exe"])),
            "SQLite database with changes not yet written into it (its -wal file), open in Lightroom; sent once they're in it"
        );
        assert_eq!(
            detail(&DatabaseNote { format: "keepass".into(), ..n("open", Some("down"), &["KeePass.exe"]) }),
            "KeePass database open in KeePass; OneDrive's newer version waits until it's closed"
        );
        assert_eq!(
            detail(&DatabaseNote { format: "access".into(), ..n("open", Some("up"), &[]) }),
            "Access database open in another program; sent once it's closed"
        );
        assert!(detail(&live_note("/x.pst".into(), Format::Outlook)).starts_with("Outlook data file: doesn't sync"));
        assert_eq!(programs(&["WINWORD.EXE".into(), "excel.exe".into()]), "Word, Excel");
    }

    /// An Outlook data file or virtual disk renamed to an ordinary name is
    /// still warned about, by what the executor found in it.
    #[test]
    fn a_renamed_outlook_file_sent_is_still_warned_about() {
        let op = Op::Upload {
            local: oneup_sync::LocalPre { id: oneup_sync::LocalId(7), stamp: Some((10, 1)) },
            to: oneup_sync::RemoteId("root".into()),
            parent: 0,
            name: "mail backup.dat".into(),
            replace: None,
            link: oneup_sync::Link::New,
        };
        let mut r = Report { done: vec![op.clone()], ..Report::default() };
        assert_eq!(live_format(&r, &op), None, "nothing in its name says so");
        r.recognised.push((op.clone(), Format::Outlook));
        assert_eq!(live_format(&r, &op), Some(Format::Outlook));
        r.recognised = vec![(op.clone(), Format::Sqlite)];
        assert_eq!(live_format(&r, &op), None, "a database, but one that may sync live");
    }

    #[test]
    fn kept_files_are_matched_as_windows_compares_names() {
        let keep = vec!["/Mail/Archive.PST".to_string()];
        assert!(kept_syncing(&keep, "/mail/archive.pst"));
        assert!(!kept_syncing(&keep, "/mail/other.pst"));
    }

    #[test]
    fn every_format_has_a_settings_name() {
        for f in FORMATS {
            assert_eq!(format_named(f.name()), Some(f));
        }
    }
}
