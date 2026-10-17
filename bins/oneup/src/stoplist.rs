//! What a safety stop holds back, one line per item, for the user to look
//! through before answering: `safety-stops\<code>.txt` in the data folder.
//! The tray's review shows it in the settings window ([`read`]); `oneup
//! approve` names it. Written when the stop trips, removed once it no
//! longer stands.

use std::path::{Path, PathBuf};

/// In the data folder.
pub const DIR: &str = "safety-stops";

/// Lines written at most: a stop over millions of items still makes a file
/// a text editor opens. The header says how many there are in all.
pub const MAX_LINES: usize = 100_000;

/// The list for the stop with `code` (16 hex digits).
pub fn path(data_dir: &Path, code: &str) -> PathBuf {
    data_dir.join(DIR).join(format!("{code}.txt"))
}

/// Writes the list for `code`: `header`, then `lines`, the first of `total`
/// (atomically: a reader never sees half of it).
pub fn write(data_dir: &Path, code: &str, header: &str, lines: &[String], total: usize) -> std::io::Result<()> {
    let file = path(data_dir, code);
    std::fs::create_dir_all(data_dir.join(DIR))?;
    let mut text = format!("{header}\r\n\r\n");
    for l in lines.iter().take(MAX_LINES) {
        text.push_str(l);
        text.push_str("\r\n");
    }
    let shown = lines.len().min(MAX_LINES);
    if total > shown {
        text.push_str(&format!("… and {} more (`oneup sync --dry-run` lists them all)\r\n", total - shown));
    }
    crate::ownfile::write(&file, text.as_bytes())
}

/// One held-back change, as a line of the list says it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Row {
    /// `delete`, `upload`, `move`…
    pub action: String,
    /// Relative to the synced folders; a folder's ends in `/`.
    pub path: String,
    /// Where a move or rename goes.
    pub to: Option<String>,
    pub note: String,
}

/// A list read back: its header line, and the lines matching a filter.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct List {
    pub header: String,
    /// At most the `limit` asked for.
    pub rows: Vec<Row>,
    /// Lines matching the filter, `rows` or more.
    pub matching: usize,
    /// Items held back but not written (over [`MAX_LINES`]).
    pub unwritten: usize,
}

/// A line as `sync`'s `Line` writes it: `action      path[ -> to][  (note)]`.
fn parse(line: &str) -> Row {
    let (action, rest) = line.split_once(char::is_whitespace).unwrap_or((line, ""));
    let rest = rest.trim_start();
    let (rest, note) = match rest.rfind("  (") {
        Some(i) if rest.ends_with(')') => (&rest[..i], &rest[i + 3..rest.len() - 1]),
        _ => (rest, ""),
    };
    let (path, to) = match rest.split_once(" -> ") {
        Some((p, t)) => (p, Some(t.to_string())),
        None => (rest, None),
    };
    Row { action: action.to_string(), path: path.to_string(), to, note: note.to_string() }
}

/// The list for `code`, with up to `limit` of the lines whose text contains
/// `filter` (any case; empty: all); `None` if there's none (yet).
pub fn read(data_dir: &Path, code: &str, filter: &str, limit: usize) -> Option<List> {
    if code.is_empty() || !code.chars().all(|c| c.is_ascii_hexdigit()) {
        return None;
    }
    let text = std::fs::read_to_string(path(data_dir, code)).ok()?;
    let mut lines = text.lines();
    let header = lines.next().unwrap_or_default().to_string();
    let filter = filter.to_lowercase();
    let (mut rows, mut matching, mut unwritten) = (Vec::new(), 0, 0);
    for l in lines.filter(|l| !l.is_empty()) {
        if let Some(n) = l.strip_prefix("… and ").and_then(|r| r.split(' ').next()?.parse().ok()) {
            unwritten = n;
            continue;
        }
        if filter.is_empty() || l.to_lowercase().contains(&filter) {
            matching += 1;
            if rows.len() < limit {
                rows.push(parse(l));
            }
        }
    }
    Some(List { header, rows, matching, unwritten })
}

/// Removes the lists of stops other than `codes` (those standing now).
pub fn keep_only(data_dir: &Path, codes: &[String]) {
    let Ok(entries) = std::fs::read_dir(data_dir.join(DIR)) else { return };
    for e in entries.flatten() {
        let name = e.file_name().to_string_lossy().into_owned();
        let code = name.split('.').next().unwrap_or_default();
        if !codes.iter().any(|c| c == code) {
            let _ = std::fs::remove_file(e.path());
        }
    }
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used)]
    use super::*;

    #[test]
    fn lines_are_read_back_into_their_parts() {
        let r = parse("move        /a/b.txt -> /c/b.txt  (in OneDrive)");
        assert_eq!(
            (r.action.as_str(), r.path.as_str(), r.to.as_deref(), r.note.as_str()),
            ("move", "/a/b.txt", Some("/c/b.txt"), "in OneDrive")
        );
        let r = parse("delete      /old (2)/");
        assert_eq!((r.path.as_str(), r.to, r.note.as_str()), ("/old (2)/", None, ""));
        let r = parse(r"download    /x.docx  (old local version kept in .oneup\versions)");
        assert_eq!((r.path.as_str(), r.note.as_str()), ("/x.docx", r"old local version kept in .oneup\versions"));
    }

    #[test]
    fn lists_are_written_capped_and_removed_when_the_stop_is_gone() {
        let dir = std::env::temp_dir().join(format!("oneup-stoplist-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        let lines: Vec<String> = (0..MAX_LINES + 2).map(|i| format!("delete      /f{i}")).collect();
        write(&dir, "00000000000000ab", "2 items", &lines[..2], 2).unwrap();
        write(&dir, "00000000000000cd", "many", &lines, lines.len()).unwrap();
        let small = std::fs::read_to_string(path(&dir, "00000000000000ab")).unwrap();
        assert_eq!(small, "2 items\r\n\r\ndelete      /f0\r\ndelete      /f1\r\n");
        let big = std::fs::read_to_string(path(&dir, "00000000000000cd")).unwrap();
        assert!(big.ends_with("… and 2 more (`oneup sync --dry-run` lists them all)\r\n"));
        assert!(!big.contains(&format!("/f{MAX_LINES}\r\n")));

        let l = read(&dir, "00000000000000cd", "", 3).unwrap();
        assert_eq!((l.header.as_str(), l.rows.len(), l.matching, l.unwritten), ("many", 3, MAX_LINES, 2));
        let l = read(&dir, "00000000000000ab", "F1", 10).unwrap();
        assert_eq!((l.matching, l.rows[0].path.as_str(), l.rows[0].action.as_str()), (1, "/f1", "delete"));
        assert!(read(&dir, r"..\x", "", 1).is_none() && read(&dir, "00000000000000ef", "", 1).is_none());

        keep_only(&dir, &["00000000000000cd".into()]);
        assert!(!path(&dir, "00000000000000ab").exists() && path(&dir, "00000000000000cd").exists());
        keep_only(&dir, &[]);
        assert!(!path(&dir, "00000000000000cd").exists());
        std::fs::remove_dir_all(&dir).unwrap();
    }
}
