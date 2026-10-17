//! `oneup activity`: what sync did, newest first, from the data folder's
//! activity log (`oneup_store::activity`). It reads the database directly,
//! so it works whether or not `oneupd` is running.

use std::path::{Component, Path};

use anyhow::{Result, bail};
use oneup_store::{ActivityEntry, ActivityOutcome, Store};

/// `docs\a.txt` or `/docs/a.txt` → `/docs/a.txt`, as the log stores paths.
pub fn normalize(path: &str) -> String {
    let parts: Vec<&str> = path.split(['\\', '/']).filter(|p| !p.is_empty() && *p != ".").collect();
    format!("/{}", parts.join("/"))
}

/// A full path such as `C:\Sync\docs\a.txt` → `docs/a.txt` when it is in the
/// synced folder `root` (Windows paths compare without case). Other paths
/// are taken as relative already.
pub fn within(root: &Path, path: &str) -> Result<String> {
    if !Path::new(path).is_absolute() {
        return Ok(path.to_owned());
    }
    // The recorded folder is canonical (`\\?\C:\…`); typed paths usually aren't.
    let plain = |s: &str| oneup_local::coexist::plain(Path::new(s)).to_string_lossy().into_owned();
    let (root, p) = (plain(&root.to_string_lossy()), plain(path));
    let folded = |c: Component<'_>| oneup_sync::fold(&c.as_os_str().to_string_lossy());
    let mut parts = Path::new(&p).components().filter(|c| !matches!(c, Component::CurDir));
    for r in Path::new(&root).components().filter(|c| !matches!(c, Component::CurDir)) {
        match parts.next() {
            Some(c) if folded(c) == folded(r) => {}
            _ => bail!("{path} isn't in the synced folder {root}"),
        }
    }
    Ok(parts.map(|c| c.as_os_str().to_string_lossy().into_owned()).collect::<Vec<_>>().join("/"))
}

/// The PC's time zone offset (UTC if it can't be told).
pub fn local_offset() -> time::UtcOffset {
    time::UtcOffset::current_local_offset().unwrap_or(time::UtcOffset::UTC)
}

pub fn show(store: &Store, path: Option<&str>, limit: usize) -> Result<()> {
    let path = match path {
        Some(p) if Path::new(p).is_absolute() => Some(normalize(&within(&oneup_app::sync::pairing(store)?.local, p)?)),
        p => p.map(normalize),
    };
    // The synced folder itself: everything.
    let path = path.filter(|p| p != "/");
    let entries = store.read(|o| o.activity_recent(path.as_deref(), limit))?;
    if entries.is_empty() {
        match &path {
            Some(p) => println!("Nothing recorded for {p}."),
            None => println!("Nothing recorded yet."),
        }
        return Ok(());
    }
    let offset = local_offset();
    // Oldest first on screen, so the newest is next to the prompt.
    for e in entries.iter().rev() {
        println!("{}", line(e, offset));
    }
    if entries.len() == limit {
        println!("(the latest {limit}; --limit shows more)");
    }
    Ok(())
}

pub fn when(at_ms: i64, offset: time::UtcOffset) -> String {
    let fmt = time::macros::format_description!("[year]-[month]-[day] [hour]:[minute]:[second]");
    time::OffsetDateTime::from_unix_timestamp_nanos(i128::from(at_ms) * 1_000_000)
        .map(|t| t.to_offset(offset))
        .ok()
        .and_then(|t| t.format(fmt).ok())
        .unwrap_or_else(|| at_ms.to_string())
}

pub fn line(e: &ActivityEntry, offset: time::UtcOffset) -> String {
    let outcome = match e.outcome {
        ActivityOutcome::Done => "ok  ",
        ActivityOutcome::Skipped => "skip",
        ActivityOutcome::Failed => "FAIL",
        ActivityOutcome::Waiting => "wait",
        ActivityOutcome::Note => "note",
    };
    let detail = if e.detail.is_empty() { String::new() } else { format!("  ({})", e.detail) };
    format!("{}  {outcome}  {:<12}{}{detail}", when(e.at_ms, offset), e.action, e.path)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn paths_are_normalized_like_the_log() {
        assert_eq!(normalize(r"docs\a.txt"), "/docs/a.txt");
        assert_eq!(normalize("/docs//a.txt/"), "/docs/a.txt");
        assert_eq!(normalize(r".\x"), "/x");
    }

    #[cfg(windows)]
    #[test]
    fn full_paths_in_the_synced_folder_become_relative() {
        let root = Path::new(r"C:\Sync\Docs");
        assert_eq!(within(root, r"c:\sync\docs\a\b.txt").unwrap(), "a/b.txt");
        assert_eq!(within(root, r"C:\Sync\Docs").unwrap(), "");
        assert_eq!(within(root, r"a\b.txt").unwrap(), r"a\b.txt");
        assert!(within(root, r"C:\Sync\Docs2\a.txt").is_err());
        assert!(within(root, r"D:\Sync\Docs\a.txt").is_err());
        assert_eq!(within(Path::new(r"C:\ΟΔΟΣ"), r"C:\οδοσ\a.txt").unwrap(), "a.txt");
        assert!(within(Path::new("C:\\u{212A}"), r"C:\k\a.txt").is_err(), "the Kelvin sign isn't k");
        let canonical = Path::new(r"\\?\C:\Sync\Docs");
        assert_eq!(within(canonical, r"C:\Sync\Docs\a.txt").unwrap(), "a.txt");
        let unc = Path::new(r"\\?\UNC\server\share\Docs");
        assert_eq!(within(unc, r"\\server\share\docs\a.txt").unwrap(), "a.txt");
    }

    #[test]
    fn lines_show_time_outcome_action_path_and_detail() {
        let e = ActivityEntry {
            seq: 1,
            at_ms: 0,
            outcome: ActivityOutcome::Skipped,
            action: "download".into(),
            path: "/a.txt".into(),
            detail: "open in another program".into(),
        };
        assert_eq!(
            line(&e, time::UtcOffset::UTC),
            "1970-01-01 00:00:00  skip  download    /a.txt  (open in another program)"
        );
    }
}
