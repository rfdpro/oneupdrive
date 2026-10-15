//! Which programs change and open files in the synced folder (`[agent]
//! track_apps`, on by default; the settings window's **Apps** page, `oneup
//! apps`). Counts only, per hour and program, kept on this PC for 90 days.
//!
//! * **Opened**: an online-only file a program opened, which OneUpDrive
//!   downloaded for it. Exact: Windows names the program in the request
//!   (`oneup_exec::hydrate`).
//! * **Changed**: a file a program wrote to. The change journal doesn't say
//!   who, so the agent's journal watcher hands each written file to the
//!   [`Tracker`], which asks Windows which programs have it open
//!   (`oneup_sys::in_use`, Restart Manager) right away, while the writer
//!   usually still does, and which programs closed it lately
//!   (`oneup_cfapi::closes`, Windows' close notifications for placeholders)
//!   for a writer that was quicker. A change not identified at once waits
//!   [`SETTLE`] for a close. Best effort: a new file (not a placeholder yet)
//!   whose program closed it before the lookup counts as "not identified".
//!   At most [`LOOKUPS_PER_S`] Restart Manager lookups a second, each file
//!   at most once per [`QUIET`].

use std::collections::{HashMap, VecDeque};
use std::path::Path;
use std::sync::mpsc::{Receiver, RecvTimeoutError, SyncSender};
use std::time::{Duration, Instant};

use anyhow::Result;
use oneup_store::{AppFile, AppUse, UseKind};
use serde_json::{Value, json};

/// Restart Manager lookups per second, at most; past that, a change is
/// identified by its close only (a mass change is one program's anyway).
pub const LOOKUPS_PER_S: u32 = 20;
/// A file written again within this long isn't counted again.
pub const QUIET: Duration = Duration::from_secs(30);
/// How long a change nobody has open waits for Windows to say who closed
/// the file: a program usually closes it within a second of its last write.
pub const SETTLE: Duration = Duration::from_secs(3);
/// Closes this long before the tracker heard of a write still count: the
/// watcher hands writes on within a fraction of a second, and the writer
/// may have closed the file meanwhile.
const LOOKBACK: Duration = Duration::from_secs(2);
/// Changes waiting for a close, at most; more are counted at once.
const MAX_WAITING: usize = 4096;
/// How long counts are kept.
pub const KEEP_DAYS: i64 = 90;
/// Counts reach the store (and the window) this often, when there are any.
const FLUSH: Duration = Duration::from_secs(15);
const PRUNE: Duration = Duration::from_secs(10 * 60);

/// Programs that open files to read them on their own (search, antivirus,
/// previews, thumbnails): never taken for the writer when another program
/// had the file open too.
const READERS: &[&str] = &[
    "searchprotocolhost.exe",
    "searchindexer.exe",
    "searchfilterhost.exe",
    "msmpeng.exe",
    "mpdefendercoreservice.exe",
    "explorer.exe",
    "prevhost.exe",
    "dllhost.exe",
];

/// Hands written files (their IDs from the change journal) to the tracker
/// thread. Dropping it ends the thread, after a last write.
#[derive(Clone)]
pub struct Tracker {
    tx: SyncSender<u128>,
}

impl Tracker {
    /// Starts the tracker for the synced folder `root`. `None` if the
    /// volume or the database can't be opened (the agent runs on without).
    pub fn start(data_dir: &Path, root: &Path) -> Option<Tracker> {
        let (tx, rx) = std::sync::mpsc::sync_channel(4096);
        let (dir, root) = (data_dir.to_path_buf(), root.to_path_buf());
        std::thread::Builder::new().name("app-tracker".into()).spawn(move || run(&dir, &root, &rx)).ok()?;
        Some(Tracker { tx })
    }

    /// A file was written. Never blocks: when the tracker is behind, the
    /// change goes uncounted.
    pub fn saw(&self, file_id: u128) {
        let _ = self.tx.try_send(file_id);
    }
}

/// A change waiting for a close to say who made it.
struct Waiting {
    id: u128,
    /// Closes from then on count.
    since: Instant,
    due: Instant,
    /// What Restart Manager found: a usual reader, or nobody (`""`).
    guess: String,
    file: AppFile,
}

/// Changes identified (or given up on), until the next flush.
#[derive(Default)]
struct Counted {
    counts: HashMap<(i64, String), (i64, i64)>,
    /// Which files, for the window's per-program list (`app_file`).
    files: Vec<AppFile>,
}

impl Counted {
    fn add(&mut self, mut file: AppFile, app: String) {
        let e = self.counts.entry((file.at_ms / 3_600_000, app.clone())).or_default();
        e.0 += 1;
        e.1 += file.bytes;
        if !file.path.is_empty() {
            file.app = app;
            self.files.push(file);
        }
    }
}

fn run(dir: &Path, root: &Path, rx: &Receiver<u128>) {
    use std::os::windows::fs::MetadataExt as _;
    let Ok(volume) = oneup_ntfs::win::Volume::open_containing(root) else { return };
    let Ok(mut store) = crate::setup::open_store(dir, 1024) else { return };
    let root = comparable(root);
    let own = format!("{root}\\{}", oneup_rules::ENGINE_DIR.to_lowercase());
    let mut seen: HashMap<u128, Instant> = HashMap::new();
    let mut waiting: VecDeque<Waiting> = VecDeque::new();
    let mut done = Counted::default();
    let (mut second, mut lookups) = (Instant::now(), 0u32);
    let (mut flushed, mut pruned) = (Instant::now(), None::<Instant>);
    let mut on = crate::config::load_agent(dir).is_ok_and(|a| a.track_apps);
    loop {
        let now = Instant::now();
        let next_flush = FLUSH.saturating_sub(flushed.elapsed());
        let timeout = waiting.front().map_or(next_flush, |w| w.due.saturating_duration_since(now).min(next_flush));
        let got = rx.recv_timeout(timeout);
        // OneUpDrive's own writes (downloads, placeholders filled or freed)
        // aren't a program's.
        if let Ok(id) = got
            && on
            && !oneup_cfapi::ours::wrote(id)
        {
            let now = Instant::now();
            if seen.get(&id).is_none_or(|t| now.duration_since(*t) > QUIET) {
                seen.insert(id, now);
                if let Some(path) = volume.path_of(oneup_ntfs::FileId(id)) {
                    let p = comparable(&path);
                    // Attributes only (no data access, so nothing is
                    // downloaded): a file that went online-only since the
                    // journal named it is OneUpDrive's doing, and asking who
                    // has it open would download it.
                    let meta = std::fs::symlink_metadata(&path).ok();
                    let online_only = meta.as_ref().is_none_or(|m| oneup_ntfs::dirent::recalls(m.file_attributes()));
                    if p.starts_with(&format!("{root}\\")) && !p.starts_with(&own) && !online_only {
                        if now.duration_since(second) >= Duration::from_secs(1) {
                            (second, lookups) = (now, 0);
                        }
                        let open = if lookups < LOOKUPS_PER_S {
                            lookups += 1;
                            open_by(&path)
                        } else {
                            Vec::new()
                        };
                        let since = now.checked_sub(LOOKBACK).unwrap_or(now);
                        let file = AppFile {
                            at_ms: crate::sync::now_ms(),
                            app: String::new(),
                            kind: UseKind::Changed,
                            path: relative(&path, root.len()).unwrap_or_default(),
                            bytes: meta.map_or(0, |m| i64::try_from(m.len()).unwrap_or(0)),
                        };
                        match choose(&open, &closed_by(id, since)) {
                            Some(app) if !is_reader(&app) => done.add(file, app),
                            guess if waiting.len() < MAX_WAITING => waiting.push_back(Waiting {
                                id,
                                since,
                                due: now + SETTLE,
                                guess: guess.unwrap_or_default(),
                                file,
                            }),
                            guess => done.add(file, guess.unwrap_or_default()),
                        }
                    }
                }
            }
        }
        let closed = matches!(got, Err(RecvTimeoutError::Disconnected));
        // Changes whose wait is over (all of them at the end), oldest first.
        while waiting.front().is_some_and(|w| closed || w.due <= Instant::now()) {
            let Some(w) = waiting.pop_front() else { break };
            let app = choose(std::slice::from_ref(&w.guess), &closed_by(w.id, w.since)).unwrap_or_default();
            done.add(w.file, app);
        }
        if closed || flushed.elapsed() >= FLUSH {
            flushed = Instant::now();
            let uses: Vec<AppUse> = done
                .counts
                .drain()
                .map(|((hour, app), (files, bytes))| AppUse { hour, app, kind: UseKind::Changed, files, bytes })
                .collect();
            let named = std::mem::take(&mut done.files);
            let prune = pruned.is_none_or(|t| t.elapsed() >= PRUNE);
            if !uses.is_empty() || !named.is_empty() || prune {
                let h = hour_now();
                if let Err(e) = store.write(|o| {
                    o.app_use_add(&uses)?;
                    o.app_files_add(&named)?;
                    if prune {
                        o.app_use_prune(h - KEEP_DAYS * 24)?;
                        o.local_used_trim(1000)?;
                    }
                    Ok(())
                }) {
                    tracing::debug!(error = %e, "program counts weren't saved");
                }
                if prune {
                    pruned = Some(Instant::now());
                }
            }
            seen.retain(|_, t| t.elapsed() <= QUIET);
            on = crate::config::load_agent(dir).is_ok_and(|a| a.track_apps);
        }
        if closed {
            return;
        }
    }
}

/// The programs that have `path` open now, other than this one.
fn open_by(path: &Path) -> Vec<String> {
    let me = std::process::id();
    oneup_sys::in_use::users(path)
        .unwrap_or_default()
        .into_iter()
        .filter(|u| u.pid != me && !u.exe.is_empty())
        .map(|u| u.exe)
        .collect()
}

/// The programs that closed the file with ID `id` at `since` or later.
fn closed_by(id: u128, since: Instant) -> Vec<String> {
    oneup_cfapi::closes::since(id, since).into_iter().map(|c| c.exe).collect()
}

fn is_reader(exe: &str) -> bool {
    READERS.contains(&exe.to_lowercase().as_str())
}

/// The program that most likely wrote a file, from those that have it open
/// (`open`) and those that closed it since the write (`closed`): the first
/// that isn't a usual reader, a reader if that's all there is.
fn choose(open: &[String], closed: &[String]) -> Option<String> {
    let mut all = open.iter().chain(closed).filter(|e| !e.is_empty());
    let first = all.clone().next();
    all.find(|e| !is_reader(e)).or(first).cloned()
}

/// `path` relative to the synced folder (`/docs/a.txt`), whose
/// [`comparable`] form is `root_len` bytes long (the caller checked that
/// the path is inside it).
fn relative(path: &Path, root_len: usize) -> Option<String> {
    let s = path.to_string_lossy();
    let s = s.strip_prefix(r"\\?\").unwrap_or(&s);
    let rest = s.get(root_len..)?;
    Some(format!("/{}", rest.trim_start_matches('\\').replace('\\', "/")))
}

/// `\\?\C:\X\` → `c:\x`, for prefix checks.
fn comparable(p: &Path) -> String {
    let s = p.to_string_lossy();
    s.strip_prefix(r"\\?\").unwrap_or(&s).trim_end_matches('\\').to_lowercase()
}

fn hour_now() -> i64 {
    crate::sync::now_ms() / 3_600_000
}

/// A friendlier name for a program's executable.
pub fn display_name(exe: &str) -> String {
    let base = exe.strip_suffix(".exe").or_else(|| exe.strip_suffix(".EXE")).unwrap_or(exe);
    let known = match base.to_lowercase().as_str() {
        "" => "Not identified",
        "oneupd" => "OneUpDrive (changes from OneDrive)",
        "winword" => "Word",
        "excel" => "Excel",
        "powerpnt" => "PowerPoint",
        "outlook" | "olk" => "Outlook",
        "onenote" => "OneNote",
        "msaccess" => "Access",
        "explorer" => "File Explorer",
        "notepad" => "Notepad",
        "msedge" => "Microsoft Edge",
        "chrome" => "Google Chrome",
        "firefox" => "Firefox",
        "code" => "Visual Studio Code",
        "devenv" => "Visual Studio",
        "photos" | "microsoft.photos" => "Photos",
        "acrobat" | "acrord32" => "Adobe Acrobat",
        "photoshop" => "Photoshop",
        "lightroom" => "Lightroom",
        "cmd" => "Command Prompt",
        "powershell" | "pwsh" => "PowerShell",
        "robocopy" => "Robocopy",
        _ => return base.to_string(),
    };
    known.to_string()
}

/// The counts of the last `days` days, for the window and `oneup apps`:
/// per program (most active first) and per time bucket (hours for a day
/// or less, days otherwise) for the busiest programs.
pub fn summary(dir: &Path, days: u32) -> Result<Value> {
    let days = i64::from(days.clamp(1, KEEP_DAYS as u32));
    let store = crate::setup::open_store(dir, 1024)?;
    let now = hour_now();
    let since = now - days * 24 + 1;
    let rows = store.read(|o| o.app_use_since(since))?;
    let mut per: HashMap<String, (i64, i64, i64)> = HashMap::new();
    for r in &rows {
        let e = per.entry(r.app.clone()).or_default();
        match r.kind {
            UseKind::Changed => e.0 += r.files,
            UseKind::Opened => e.1 += r.files,
        }
        e.2 += r.bytes;
    }
    let mut apps: Vec<(String, (i64, i64, i64))> = per.into_iter().collect();
    apps.sort_by(|a, b| (b.1.0 + b.1.1).cmp(&(a.1.0 + a.1.1)).then(a.0.cmp(&b.0)));
    // Buckets: hours for one day, days for longer.
    let hourly = days <= 1;
    let buckets = if hourly { 24 } else { days };
    let bucket_of = |hour: i64| if hourly { hour - since } else { (hour - since) / 24 };
    let top: Vec<String> = apps.iter().take(5).map(|(a, _)| a.clone()).collect();
    let mut series: Vec<Vec<i64>> = vec![vec![0; buckets as usize]; top.len() + 1];
    for r in &rows {
        let b = bucket_of(r.hour);
        if !(0..buckets).contains(&b) {
            continue;
        }
        let i = top.iter().position(|a| *a == r.app).unwrap_or(top.len());
        series[i][b as usize] += r.files;
    }
    let mut names: Vec<String> = top.iter().map(|a| display_name(a)).collect();
    names.push("Others".into());
    Ok(json!({
        "days": days,
        "hourly": hourly,
        "start_ms": since * 3_600_000,
        "apps": apps.iter().map(|(a, (changed, opened, bytes))| json!({
            "exe": a,
            "name": display_name(a),
            "changed": changed,
            "opened": opened,
            "bytes": bytes,
        })).collect::<Vec<_>>(),
        "series": names.iter().zip(series).map(|(n, s)| json!({ "name": n, "counts": s })).collect::<Vec<_>>(),
    }))
}

/// The files programs used most recently, newest first: `exe`'s only if
/// given (`""` is "not identified"), at most `limit`.
pub fn recent_files(dir: &Path, exe: Option<&str>, limit: usize) -> Result<Value> {
    let store = crate::setup::open_store(dir, 1024)?;
    let files = store.read(|o| o.app_files(exe, limit))?;
    Ok(Value::Array(
        files
            .into_iter()
            .map(|f| {
                json!({
                    "at_ms": f.at_ms,
                    "exe": f.app,
                    "name": display_name(&f.app),
                    "kind": match f.kind { UseKind::Changed => "changed", UseKind::Opened => "opened" },
                    "path": f.path,
                    "bytes": f.bytes,
                })
            })
            .collect(),
    ))
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used)]
    use super::*;

    /// A changed file's path as the Apps page lists it: relative to the
    /// synced folder, `/`-separated, its own case kept.
    #[test]
    fn files_are_named_relative_to_the_synced_folder() {
        let root = comparable(Path::new(r"\\?\C:\Users\X\OneDrive"));
        assert_eq!(root, r"c:\users\x\onedrive");
        let rel = |p: &str| relative(Path::new(p), root.len());
        assert_eq!(rel(r"\\?\C:\Users\X\OneDrive\Docs\Plan B.docx").as_deref(), Some("/Docs/Plan B.docx"));
        assert_eq!(rel(r"C:\Users\X\OneDrive\a.txt").as_deref(), Some("/a.txt"));
    }

    /// The files each program used come back newest first, per program,
    /// with the friendly name.
    #[test]
    fn recent_files_are_listed_per_program() {
        let dir = std::env::temp_dir().join(format!("oneup-apps-files-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let mut store = crate::setup::open_store(&dir, 1024).unwrap();
        let f = |at, app: &str, path: &str| AppFile {
            at_ms: at,
            app: app.into(),
            kind: UseKind::Changed,
            path: path.into(),
            bytes: 3,
        };
        store
            .write(|o| {
                o.app_files_add(&[f(1, "WINWORD.EXE", "/a.docx"), f(2, "", "/b.txt"), f(3, "WINWORD.EXE", "/c.docx")])
            })
            .unwrap();
        drop(store);
        let word = recent_files(&dir, Some("WINWORD.EXE"), 10).unwrap();
        let paths: Vec<&str> = word.as_array().unwrap().iter().map(|f| f["path"].as_str().unwrap()).collect();
        assert_eq!(paths, ["/c.docx", "/a.docx"]);
        assert_eq!(word[0]["name"], "Word");
        assert_eq!(recent_files(&dir, None, 10).unwrap().as_array().unwrap().len(), 3);
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// A program that had the file open or closed it since the write is
    /// the writer; search, antivirus and Explorer only when nobody else is.
    #[test]
    fn the_writer_is_the_program_that_is_not_a_usual_reader() {
        let v = |x: &[&str]| x.iter().map(|s| (*s).to_string()).collect::<Vec<_>>();
        assert_eq!(choose(&v(&["SearchProtocolHost.exe", "WINWORD.EXE"]), &[]).as_deref(), Some("WINWORD.EXE"));
        assert_eq!(choose(&v(&["MsMpEng.exe"]), &v(&["cmd.exe"])).as_deref(), Some("cmd.exe"));
        assert_eq!(choose(&v(&[""]), &v(&["explorer.exe"])).as_deref(), Some("explorer.exe"));
        assert_eq!(choose(&v(&["explorer.exe"]), &v(&["SearchProtocolHost.exe"])).as_deref(), Some("explorer.exe"));
        assert_eq!(choose(&v(&[""]), &[]), None);
    }

    #[test]
    fn program_names_read_well() {
        assert_eq!(display_name("WINWORD.EXE"), "Word");
        assert_eq!(display_name("oneupd.exe"), "OneUpDrive (changes from OneDrive)");
        assert_eq!(display_name("MyTool.exe"), "MyTool");
        assert_eq!(display_name(""), "Not identified");
    }

    #[test]
    fn the_summary_groups_by_program_and_time() {
        let dir = std::env::temp_dir().join(format!("oneup-apps-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let mut store = crate::setup::open_store(&dir, 1024).unwrap();
        let h = hour_now();
        let u = |hour, app: &str, kind, files| AppUse { hour, app: app.into(), kind, files, bytes: 1 };
        store
            .write(|o| {
                o.app_use_add(&[
                    u(h, "WINWORD.EXE", UseKind::Changed, 3),
                    u(h - 2, "WINWORD.EXE", UseKind::Opened, 1),
                    u(h - 1, "code.exe", UseKind::Changed, 9),
                    u(h - 30, "old.exe", UseKind::Changed, 50),
                ])
            })
            .unwrap();
        drop(store);
        let s = summary(&dir, 1).unwrap();
        let apps = s["apps"].as_array().unwrap();
        assert_eq!(apps.len(), 2, "{s}");
        assert_eq!((apps[0]["name"].as_str(), apps[0]["changed"].as_i64()), (Some("Visual Studio Code"), Some(9)));
        assert_eq!((apps[1]["changed"].as_i64(), apps[1]["opened"].as_i64()), (Some(3), Some(1)));
        let word = &s["series"][1]["counts"];
        assert_eq!(word.as_array().unwrap().len(), 24);
        assert_eq!(word[23].as_i64(), Some(3));
        assert_eq!(summary(&dir, 7).unwrap()["apps"].as_array().unwrap().len(), 3);
        let _ = std::fs::remove_dir_all(&dir);
    }
}
