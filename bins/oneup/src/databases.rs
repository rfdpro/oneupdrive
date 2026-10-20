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
/// Databases held back listed in the status, at most; the rest are
/// counted (`Status::databases_more`).
pub const MAX_WAITS: usize = 100;

/// What a round's report says about databases, looked up by operation
/// once: a large round has many.
pub struct Found<'a> {
    held: std::collections::HashMap<&'a Op, &'a DbState>,
    recognised: std::collections::HashMap<&'a Op, Format>,
}

impl<'a> Found<'a> {
    pub fn of(r: &'a Report) -> Self {
        Self {
            held: r.databases.iter().map(|(op, d)| (op, d)).collect(),
            recognised: r.recognised.iter().map(|(op, f)| (op, *f)).collect(),
        }
    }
}

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
        Wait::Beside => "beside",
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

/// The notes for the databases a round held back, at most
/// [`MAX_WAITS`], and how many more there were. `paths`: each planned
/// operation's path (indexed once for the round).
pub fn held_notes(r: &Report, paths: &std::collections::HashMap<&Op, &str>) -> (Vec<DatabaseNote>, u64) {
    let notes: Vec<DatabaseNote> =
        r.databases.iter().filter_map(|(op, db)| note((*paths.get(op)?).to_string(), op, db)).take(MAX_WAITS).collect();
    let more = r.databases.len().saturating_sub(notes.len());
    (notes, u64::try_from(more).unwrap_or(u64::MAX))
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
        "beside" => "a database's working file, or a file going into such a name: they stay with the database \
                     and never sync"
            .to_string(),
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
pub fn skip_detail(found: &Found<'_>, op: &Op) -> Option<String> {
    let db = found.held.get(op)?;
    note(String::new(), op, db).map(|n| detail(&n))
}

/// Whether `op` sends or fetches a file that shouldn't sync while in use
/// (by its name, or by what the executor found), and which format.
pub fn live_format(found: &Found<'_>, op: &Op) -> Option<Format> {
    let (Op::Upload { name, .. } | Op::Download { name, .. }) = op else { return None };
    let held = found.held.get(op).map(|d| d.recognised.format);
    let sent = || found.recognised.get(op).copied();
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
        assert_eq!(live_format(&Found::of(&r), &op), None, "nothing in its name says so");
        r.recognised.push((op.clone(), Format::Outlook));
        assert_eq!(live_format(&Found::of(&r), &op), Some(Format::Outlook));
        r.recognised = vec![(op.clone(), Format::Sqlite)];
        assert_eq!(live_format(&Found::of(&r), &op), None, "a database, but one that may sync live");
    }

    /// A round that held back many databases lists a bounded number, and
    /// counts the rest.
    #[test]
    fn many_databases_held_back_are_listed_up_to_a_limit_and_counted() {
        let op = |i: u128| Op::Upload {
            local: oneup_sync::LocalPre { id: oneup_sync::LocalId(i), stamp: Some((10, 1)) },
            to: oneup_sync::RemoteId("root".into()),
            parent: 0,
            name: format!("db{i}.db"),
            replace: None,
            link: oneup_sync::Link::New,
        };
        let db = DbState {
            recognised: oneup_rules::database::Recognised {
                format: Format::Sqlite,
                by: oneup_rules::database::By::Header,
            },
            wait: Some(Wait::Wal),
            programs: Vec::new(),
        };
        let r = Report { databases: (0..250).map(|i| (op(i), db.clone())).collect(), ..Report::default() };
        let names: Vec<String> = (0..250).map(|i| format!("/db{i}.db")).collect();
        let paths = r.databases.iter().zip(&names).map(|((o, _), n)| (o, n.as_str())).collect();
        let (notes, more) = held_notes(&r, &paths);
        assert_eq!((notes.len(), more), (MAX_WAITS, 150));
        assert_eq!(notes[0].path, "/db0.db");
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
