//! The observe-mode driver: walks the Local and Remote trees together, folder
//! by folder, and fills a [`Report`]. It reads the state database and,
//! optionally, local file contents (through a [`ContentCheck`] the caller
//! supplies). It never writes anything, locally or in OneDrive.
//!
//! Cost: one pass over both trees, one indexed child query per folder and
//! side. Memory: the children of one folder at a time, plus a depth-first
//! stack of pending folders (bounded by depth × fan-out, not by tree size).

use std::sync::atomic::{AtomicBool, AtomicU64, Ordering::Relaxed};

use oneup_store::{CursorState, LocalRow, Ops, Skip};

use crate::folder::{Next, Verified, after_verify, plan_folder};
use crate::report::{Category, Entry, Report, Unchecked};

#[derive(Debug, thiserror::Error)]
pub enum Error {
    #[error(transparent)]
    Store(#[from] oneup_store::Error),
    /// The inputs aren't there or don't fit together; the message says what
    /// to run first.
    #[error("{0}")]
    NotReady(String),
    #[error("cancelled")]
    Cancelled,
}

/// Compares a local file's content with the cloud copy's hash. Implemented by
/// the CLI on top of `oneup-local`'s verifier; tests use fakes.
pub trait ContentCheck {
    /// Must never read a cloud placeholder (return
    /// `Verified::Skipped(Unchecked::Placeholder)` instead).
    fn check(&mut self, local: &LocalRow, expected: &[u8; 20]) -> Verified;
}

/// What the report is based on.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Basis {
    /// The scanned local folder.
    pub local_path: String,
    pub scanned_at_ms: i64,
    /// When the OneDrive mirror's change cursor was taken.
    pub mirror_at_ms: i64,
    /// Exclusion rules fingerprint both trees were built with.
    pub rules: String,
    /// Set if the local folder belongs to another sync engine.
    pub other_engine: Option<String>,
}

/// Live counters, readable from another thread for progress output.
#[derive(Debug, Default)]
pub struct Progress {
    pub folders: AtomicU64,
    pub hashed_files: AtomicU64,
    pub hashed_bytes: AtomicU64,
}

/// Checks that a finished scan and a complete mirror exist and were built
/// with the same exclusion rules, and describes them.
pub fn basis(o: &Ops<'_>) -> Result<Basis, Error> {
    let not_ready = |m: &str| Err(Error::NotReady(m.into()));
    let Some(local) = o.local_root()? else {
        return not_ready("no folder has been scanned yet: run `oneup scan <folder>` first");
    };
    let Some(scanned_at_ms) = local.finished_at_ms else {
        return not_ready("the folder scan isn't finished: run `oneup scan` again to resume it");
    };
    if !o.local_scan_is_current()? {
        return not_ready(
            "the folder scan was made by an older version of OneUpDrive that couldn't recognise all cloud files: \
             run `oneup scan` again",
        );
    }
    let cursor = match o.cursor()? {
        Some(c) if c.state == CursorState::Current => c,
        Some(_) => return not_ready("the OneDrive mirror isn't finished: run `oneup mirror` to resume it"),
        None => return not_ready("there's no OneDrive mirror yet: run `oneup mirror` first"),
    };
    // Different rules would make each side skip different folders, and every
    // difference would show up as a bogus upload or download.
    if o.meta_get("rules")?.as_deref() != Some(local.rules.as_str()) {
        return not_ready(
            "the folder scan and the OneDrive mirror used different exclusion rules: \
             run `oneup scan` and `oneup mirror --rebuild` with the same --exclude choice",
        );
    }
    Ok(Basis {
        local_path: local.path,
        scanned_at_ms,
        mirror_at_ms: cursor.taken_at_ms,
        rules: local.rules,
        other_engine: local.sync_root_provider,
    })
}

/// Builds the report. `check` enables content comparison of same-size files.
pub fn observe(
    o: &Ops<'_>,
    max_examples: usize,
    mut check: Option<&mut dyn ContentCheck>,
    progress: &Progress,
    cancel: &AtomicBool,
) -> Result<(Basis, Report), Error> {
    let basis = basis(o)?;
    let Some(local_root) = o.local_root()? else {
        return Err(Error::NotReady("the folder scan disappeared".into()));
    };
    let Some(remote_root) = o.remote_root()? else {
        return Err(Error::NotReady("the OneDrive mirror has no root folder: run `oneup mirror --rebuild`".into()));
    };
    let root_row = o.local_get(local_root.root_file_id)?;
    let first = match root_row.map(|r| r.skip) {
        // The scanned folder itself is online-only (an unpopulated placeholder
        // of another provider): everything in OneDrive is online-only here.
        Some(Skip::Unpopulated) => {
            Next::Remote { id: remote_root.id, path: String::new(), inherit: Some(Category::OnlineOnly) }
        }
        Some(Skip::None | Skip::Excluded) => {
            Next::Both { local: local_root.root_file_id, remote: remote_root.id, path: String::new() }
        }
        Some(Skip::Unreadable) | None => {
            return Err(Error::NotReady("the scanned folder couldn't be read: run `oneup scan` again".into()));
        }
    };

    let mut report = Report::new(max_examples);
    let mut stack = vec![first];
    while let Some(next) = stack.pop() {
        if cancel.load(Relaxed) {
            return Err(Error::Cancelled);
        }
        let (path, inherit, local, remote) = match next {
            Next::Both { local, remote, path } => (path, None, o.local_children(local)?, o.remote_children(&remote)?),
            Next::Local { id, path, inherit } => (path, inherit, o.local_children(id)?, Vec::new()),
            Next::Remote { id, path, inherit } => (path, inherit, Vec::new(), o.remote_children(&id)?),
        };
        let plan = plan_folder(&path, inherit, local, remote);
        for e in plan.entries {
            report.add(e);
        }
        for req in plan.verify {
            let outcome = match check.as_deref_mut() {
                Some(c) => c.check(&req.local, &req.expected),
                None => Verified::Skipped(Unchecked::NotRequested),
            };
            let size = u64::try_from(req.local.size).unwrap_or(0);
            if matches!(outcome, Verified::Match | Verified::Mismatch) {
                report.hashed_files += 1;
                report.hashed_bytes += size;
                progress.hashed_files.fetch_add(1, Relaxed);
                progress.hashed_bytes.fetch_add(size, Relaxed);
            }
            report.add(Entry { path: req.path, category: after_verify(outcome), is_folder: false, bytes: size });
        }
        // Reversed so the stack pops subfolders in name order (examples then
        // read like a sorted listing).
        stack.extend(plan.next.into_iter().rev());
        progress.folders.fetch_add(1, Relaxed);
    }
    Ok((basis, report))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::report::{Conflict, Park};
    use oneup_store::{Cursor, Kind, LocalKind, LocalRoot, RemoteRow, Store};

    fn lrow(id: u128, parent: Option<u128>, name: &str, kind: LocalKind, size: i64) -> LocalRow {
        LocalRow {
            file_id: id,
            parent_id: parent,
            name: name.into(),
            name_lossy: false,
            kind,
            size,
            mtime: 0,
            attributes: 0,
            reparse_tag: 0,
            skip: Skip::None,
        }
    }

    fn rrow(id: &str, parent: Option<&str>, name: &str, kind: Kind, size: i64, h: u8) -> RemoteRow {
        RemoteRow {
            id: id.into(),
            parent_id: parent.map(Into::into),
            name: name.into(),
            kind,
            size,
            etag: None,
            qxh: (kind == Kind::File).then_some([h; 20]),
            mtime_ms: None,
        }
    }

    const RULES: &str = "v1:folders=node_modules";

    /// Local:  /Docs/{a.txt(5), b.txt(5), mine.txt(1)}, /node_modules (excluded), /aux/{x.txt}
    /// Remote: /docs/{a.txt(5, hash 1), b.txt(5, hash 2), cloud.txt(3)}, /node_modules (excluded),
    ///         /Photos/{p.jpg(7)}
    fn store(finished: bool, cursor: CursorState, rules: &str) -> Store {
        let mut s = Store::open_in_memory().unwrap();
        s.write(|o| {
            let root = LocalRoot {
                path: r"C:\t".into(),
                volume_serial: 1,
                root_file_id: 1,
                rules: RULES.into(),
                journal: None,
                started_at_ms: 1,
                finished_at_ms: None,
                sync_root_provider: None,
            };
            o.local_begin(&root, &lrow(1, None, "", LocalKind::Root, 0))?;
            o.local_dequeue(1)?;
            o.local_put(&lrow(2, Some(1), "Docs", LocalKind::Folder, 0))?;
            o.local_put(&lrow(3, Some(2), "a.txt", LocalKind::File, 5))?;
            o.local_put(&lrow(4, Some(2), "b.txt", LocalKind::File, 5))?;
            o.local_put(&lrow(5, Some(2), "mine.txt", LocalKind::File, 1))?;
            o.local_put(&LocalRow { skip: Skip::Excluded, ..lrow(6, Some(1), "node_modules", LocalKind::Folder, 0) })?;
            o.local_put(&lrow(7, Some(1), "aux", LocalKind::Folder, 0))?;
            o.local_put(&lrow(8, Some(7), "x.txt", LocalKind::File, 2))?;
            if finished {
                o.local_finish(2)?;
            }
            o.upsert_item(&rrow("r", None, "root", Kind::Root, 0, 0), false)?;
            o.upsert_item(&rrow("d", Some("r"), "docs", Kind::Folder, 0, 0), false)?;
            o.upsert_item(&rrow("a", Some("d"), "a.txt", Kind::File, 5, 1), false)?;
            o.upsert_item(&rrow("b", Some("d"), "b.txt", Kind::File, 5, 2), false)?;
            o.upsert_item(&rrow("c", Some("d"), "cloud.txt", Kind::File, 3, 3), false)?;
            o.upsert_item(&rrow("n", Some("r"), "node_modules", Kind::Folder, 0, 0), true)?;
            o.upsert_item(&rrow("p", Some("r"), "Photos", Kind::Folder, 0, 0), false)?;
            o.upsert_item(&rrow("pj", Some("p"), "p.jpg", Kind::File, 7, 4), false)?;
            o.set_cursor(&Cursor { delta_link: "x".into(), taken_at_ms: 3, state: cursor })?;
            o.meta_set("rules", rules)
        })
        .unwrap();
        s
    }

    /// Pretends local file 3 hashes to [1; 20] (matches) and 4 to [9; 20].
    struct Fake(Vec<u128>);
    impl ContentCheck for Fake {
        fn check(&mut self, local: &LocalRow, expected: &[u8; 20]) -> Verified {
            self.0.push(local.file_id);
            let actual = if local.file_id == 3 { [1; 20] } else { [9; 20] };
            if &actual == expected { Verified::Match } else { Verified::Mismatch }
        }
    }

    fn run(s: &Store, check: Option<&mut dyn ContentCheck>) -> Result<(Basis, Report), Error> {
        s.read(|o| Ok(observe(o, 10, check, &Progress::default(), &AtomicBool::new(false)))).unwrap()
    }

    #[test]
    fn report_without_verify() {
        let s = store(true, CursorState::Current, RULES);
        let (basis, rep) = run(&s, None).unwrap();
        assert_eq!((basis.scanned_at_ms, basis.mirror_at_ms), (2, 3));
        let t = |c| rep.tally(c);
        assert_eq!(t(Category::SameSize(Unchecked::NotRequested)).files, 2);
        assert_eq!(t(Category::FolderInBoth).folders, 1);
        assert_eq!(t(Category::Upload).files, 1);
        assert_eq!(
            (t(Category::Download).files, t(Category::Download).folders, t(Category::Download).bytes),
            (2, 1, 10)
        );
        assert_eq!(t(Category::Excluded).folders, 1, "excluded on both sides counts once");
        assert_eq!(
            (t(Category::Parked(Park::InvalidName)).folders, t(Category::Parked(Park::InvalidName)).files),
            (1, 1)
        );
        // A folder's own entries come before its subfolders' (and paired
        // folders take the local spelling, "Docs").
        assert_eq!(rep.examples[&Category::Download], ["/Photos", "/Docs/cloud.txt", "/Photos/p.jpg"]);
        assert_eq!(rep.hashed_files, 0);
    }

    #[test]
    fn verify_upgrades_same_size_pairs() {
        let s = store(true, CursorState::Current, RULES);
        let mut fake = Fake(Vec::new());
        let (_, rep) = run(&s, Some(&mut fake)).unwrap();
        fake.0.sort();
        assert_eq!(fake.0, [3, 4], "only same-size pairs are hashed");
        assert_eq!(rep.examples[&Category::Identical], ["/Docs/a.txt"]);
        assert_eq!(rep.examples[&Category::Conflict(Conflict::ContentDiffers)], ["/Docs/b.txt"]);
        assert_eq!((rep.hashed_files, rep.hashed_bytes), (2, 10));
        assert_eq!(rep.tally(Category::SameSize(Unchecked::NotRequested)).files, 0);
    }

    #[test]
    fn refuses_mismatched_or_unfinished_inputs() {
        let not_ready = |s: Store| matches!(run(&s, None), Err(Error::NotReady(_)));
        assert!(not_ready(store(false, CursorState::Current, RULES)), "unfinished scan");
        assert!(not_ready(store(true, CursorState::Pending, RULES)), "unfinished mirror");
        assert!(not_ready(store(true, CursorState::Current, "v1:folders=")), "different rules");
        assert!(not_ready(Store::open_in_memory().unwrap()), "nothing at all");
        let mut old = store(true, CursorState::Current, RULES);
        old.write(|o| o.meta_set("local_scan_format", "1")).unwrap();
        assert!(not_ready(old), "scan made before placeholders were exposed");
    }

    #[test]
    fn cancel_stops_the_walk() {
        let s = store(true, CursorState::Current, RULES);
        let r = s.read(|o| Ok(observe(o, 10, None, &Progress::default(), &AtomicBool::new(true)))).unwrap();
        assert!(matches!(r, Err(Error::Cancelled)));
    }
}
