//! Database files (`oneup_rules::database`, `docs/architecture/databases.md`):
//! what's checked before one is sent or replaced, and before one of SQLite's
//! working files, or a database beside them, changes here.
//!
//! * **Sent only in a consistent state**, whatever `[databases] wait`
//!   says. SQLite: every change is in the file itself (its `-wal` absent or
//!   empty, its `-journal` not in a transaction); the program may keep it
//!   open. The `-wal` and `-journal` are watched while it's read, and every
//!   read checks again that nothing waits outside it, so a change to any
//!   part of the group cancels the upload before it commits. A working file
//!   is sent only when it holds nothing the database needs. Every other
//!   format: no other program has it open (unless `wait` is off).
//! * **Replaced, moved or recycled only away from nothing it needs.** A
//!   newer version from OneDrive waits while a program has the database
//!   open, and no database is replaced, moved, renamed or recycled while
//!   changes wait beside it (`-wal`, `-journal`), nor moved beside such
//!   changes: SQLite would apply them to the wrong file. Checked last under
//!   the exclusive handle the change goes through, with the configured
//!   recognition (content, patterns, name; an encrypted database has no
//!   readable signature).
//! * **Working files stay with their database.** Before a SQLite working
//!   file changes here (a download into its name, a move or rename from or
//!   into such a name, a conflict copy or recycling), its database is
//!   classified from its attributes, compared with what was expected, and
//!   held with writers kept out ([`LocalFs::hold_for_companion`]) for
//!   exactly that step.
//!
//! Every state that can't be told is "wait" or "try again", never "go
//! ahead": a file's attributes that can't be read (only "not found" means
//! absent), a database whose content isn't all here, a link or another
//! provider's placeholder where a database should be, a Restart Manager
//! that can't answer. Content is read only from ordinary files and
//! OneUpDrive's own placeholders whose content is all here, never from
//! another provider's placeholder. Which programs have a file open is
//! asked of Windows only for a file whose data is all here (asking about
//! an online-only file downloads it).

use std::io::Read as _;
use std::os::windows::fs::{MetadataExt as _, OpenOptionsExt as _};
use std::path::{Path, PathBuf};

use oneup_gate::Approved;
use oneup_ntfs::FileId;
use oneup_ntfs::dirent::{FILE_ATTRIBUTE_DIRECTORY, FILE_ATTRIBUTE_REPARSE_POINT, NOT_PLAIN};
use oneup_ntfs::win;
use oneup_rules::database::{By, Companion, Consistent, Databases, Format, HEAD, Recognised, access_lock_name};
use oneup_sync::{LocalId, Op};

use crate::local::{ChangedMeanwhile, Error, LocalFs, Result, Staged, UploadSource, is_cloud_tag, long};

/// `CreateFile` flag: open a reparse point itself, never what it leads to.
const FILE_FLAG_OPEN_REPARSE_POINT: u32 = 0x0020_0000;

/// Programs that open files only to read them for a moment (search,
/// antivirus): never a reason to wait.
const READERS: &[&str] = &[
    "searchprotocolhost.exe",
    "searchindexer.exe",
    "searchfilterhost.exe",
    "msmpeng.exe",
    "mpdefendercoreservice.exe",
];

/// Why a database waits.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Wait {
    /// SQLite: changes in its `-wal` file aren't in the database yet.
    Wal,
    /// SQLite: a transaction is under way (or was cut short), in its
    /// `-journal` file.
    Journal,
    /// Open in another program ([`DbState::programs`] says which, if
    /// Windows would; an Access lock file beside it says it's open).
    Open,
    /// A SQLite working file whose database here isn't the one it belongs
    /// to (not the version OneDrive has, not one OneUpDrive knows, or a
    /// file put in its place): placed or moved now, SQLite would apply it
    /// to the wrong database.
    Behind,
}

/// A database found by a check, and why it waits, if it does.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DbState {
    pub recognised: Recognised,
    pub wait: Option<Wait>,
    /// When it waits: the programs that have it open (their executables,
    /// `KeePass.exe`), as far as Windows says.
    pub programs: Vec<String>,
}

/// What a check before changing a possible SQLite working file found
/// ([`LocalFs::check_companion_change`]).
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum CompanionCheck {
    /// Not a database's working file: an ordinary file.
    Ordinary,
    /// One, and nothing here holds it back. `main`: its database's name.
    Go { main: String },
    /// One, and it waits.
    Waits(DbState),
}

/// What an upload's check found ([`LocalFs::check_database_upload`]).
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum UploadCheck {
    /// Not a database: send it.
    Send,
    /// A database in a state to send: send it, read with its companions
    /// watched.
    Consistent(Recognised),
    /// It waits.
    Waits(DbState),
    /// A database that may only be copied while closed: ask which
    /// programs have it open ([`LocalFs::database_open_by`]) with the
    /// file closed, then open it again. Asking Windows (Restart Manager)
    /// breaks the file's watch, which would fail every read after it.
    IfClosed { recognised: Recognised, path: PathBuf },
}

/// The database a SQLite working file from OneDrive may be placed beside
/// ([`LocalFs::hold_for_companion`]): the one the state database has
/// there, at the stamp it was synced at. `None` where it has none.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct MainExpect {
    pub id: LocalId,
    pub size: i64,
    pub mtime: i64,
}

/// A SQLite database held with writers kept out while one of its working
/// files changes ([`LocalFs::hold_for_companion`]). Dropping it lets them
/// in again.
#[derive(Debug)]
pub struct CompanionHold(Option<win::Handle>);

impl CompanionHold {
    /// Whether a database is held (a working file beside a SQLite one).
    pub fn holds(&self) -> bool {
        self.0.is_some()
    }
}

/// What's at a path, from its attributes alone (nothing is opened for data).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Seen {
    Absent,
    Folder,
    /// A file: its size, and whether it's an ordinary one.
    File {
        size: u64,
        plain: bool,
    },
}

/// Reads what's beside a database by its attributes, the one place that
/// does: only "not found" means absent; any other failure is
/// [`Error::Busy`] (try again), never "absent". Tests can make it fail for
/// names with a given ending (`LocalFs::fail_metadata`).
#[derive(Debug, Clone, Default)]
pub(crate) struct Look {
    fail: Option<String>,
}

impl Look {
    #[cfg(feature = "crash-test")]
    pub(crate) fn failing(suffix: Option<String>) -> Self {
        Self { fail: suffix }
    }

    fn side(&self, p: &Path) -> Result<Seen> {
        if let Some(sfx) = &self.fail
            && p.to_string_lossy().ends_with(sfx.as_str())
        {
            return Err(Error::Busy);
        }
        match std::fs::symlink_metadata(p) {
            Ok(m) if m.is_dir() => Ok(Seen::Folder),
            Ok(m) => Ok(Seen::File { size: m.len(), plain: m.file_attributes() & NOT_PLAIN == 0 }),
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(Seen::Absent),
            Err(_) => Err(Error::Busy),
        }
    }

    fn exists(&self, p: &Path) -> Result<bool> {
        Ok(self.side(p)? != Seen::Absent)
    }

    /// Whether the rollback journal at `p` is in a transaction: present, not
    /// empty, and its header not zeroed. One that can't be read as an
    /// ordinary file counts as in one.
    fn journal_active(&self, p: &Path) -> Result<bool> {
        match self.side(p)? {
            Seen::Absent | Seen::File { size: 0, .. } => return Ok(false),
            Seen::Folder | Seen::File { plain: false, .. } => return Ok(true),
            Seen::File { .. } => {}
        }
        let Ok(mut f) = std::fs::OpenOptions::new().read(true).custom_flags(FILE_FLAG_OPEN_REPARSE_POINT).open(p)
        else {
            return Ok(true);
        };
        // Checked again on the handle: it may have become something else.
        if !f.metadata().is_ok_and(|m| m.file_attributes() & NOT_PLAIN == 0) {
            return Ok(true);
        }
        let mut head = [0u8; 8];
        let mut n = 0;
        while n < head.len() {
            match f.read(&mut head[n..]) {
                Ok(0) => break,
                Ok(k) => n += k,
                Err(_) => return Ok(true),
            }
        }
        Ok(oneup_rules::database::journal_active(&head[..n]))
    }

    /// Whether the working file `c` at `path` holds changes its database
    /// needs: a `-wal` with data (or that isn't an ordinary file), a
    /// `-journal` in a transaction.
    fn holds_changes(&self, c: Companion, path: &Path) -> Result<Option<Wait>> {
        Ok(match c {
            Companion::Wal => match self.side(path)? {
                Seen::Absent | Seen::File { size: 0, plain: true } => None,
                _ => Some(Wait::Wal),
            },
            Companion::Journal => self.journal_active(path)?.then_some(Wait::Journal),
            Companion::Shm => None,
        })
    }

    /// Why a SQLite database at `path` isn't consistent on its own, if it
    /// isn't: changes in its `-wal`, or a transaction in its `-journal`.
    pub(crate) fn sqlite_wait(&self, path: &Path) -> Result<Option<Wait>> {
        if let Some(w) = self.holds_changes(Companion::Wal, &beside(path, Companion::Wal.suffix()))? {
            return Ok(Some(w));
        }
        self.holds_changes(Companion::Journal, &beside(path, Companion::Journal.suffix()))
    }

    /// Whether a SQLite `-wal` or `-journal` is beside the file at `path`.
    fn has_companions(&self, path: &Path) -> Result<bool> {
        Ok(self.exists(&beside(path, Companion::Wal.suffix()))?
            || self.exists(&beside(path, Companion::Journal.suffix()))?)
    }

    /// Whether Access's lock file is beside the database at `path`.
    fn access_locked(&self, path: &Path, format: Format) -> Result<bool> {
        if format != Format::Access {
            return Ok(false);
        }
        match path.file_name().and_then(|n| access_lock_name(&n.to_string_lossy())) {
            Some(lock) => self.exists(&path.with_file_name(lock)),
            None => Ok(false),
        }
    }
}

/// `path` with `suffix` added to its name (`app.db` → `app.db-wal`).
fn beside(path: &Path, suffix: &str) -> PathBuf {
    let mut s = path.as_os_str().to_owned();
    s.push(suffix);
    PathBuf::from(s)
}

fn file_name(path: &Path) -> String {
    path.file_name().map(|n| n.to_string_lossy().into_owned()).unwrap_or_default()
}

/// Whether a database recognised as `r` may have SQLite working files: a
/// SQLite one, or one the user's patterns name (it may be an encrypted
/// SQLite database, with no signature to read).
fn may_be_sqlite(r: &Recognised) -> bool {
    matches!(r.format, Format::Sqlite | Format::Other)
}

/// Up to 16 first bytes of the file open as `h`, read from its start; it's
/// left at its start.
fn head_of(h: &win::Handle) -> std::io::Result<Vec<u8>> {
    use std::io::{Seek as _, SeekFrom};
    let f = h.as_file();
    let mut f: &std::fs::File = &f;
    f.seek(SeekFrom::Start(0))?;
    let mut head = vec![0u8; 16];
    let mut n = 0;
    while n < head.len() {
        match f.read(&mut head[n..])? {
            0 => break,
            k => n += k,
        }
    }
    f.seek(SeekFrom::Start(0))?;
    head.truncate(n);
    Ok(head)
}

/// The format a downloaded file's first bytes say it is, read from its
/// staged copy (OneUpDrive's own file, verified): so an Outlook data file
/// or a virtual disk under an ordinary name is known as one too.
pub fn staged_format(staged: &mut Staged) -> Option<Format> {
    use std::io::{Seek as _, SeekFrom};
    let f = staged.file()?;
    f.seek(SeekFrom::Start(0)).ok()?;
    let mut head = Vec::with_capacity(HEAD);
    f.by_ref().take(HEAD as u64).read_to_end(&mut head).ok()?;
    f.seek(SeekFrom::Start(0)).ok()?;
    Format::of_header(&head)
}

/// The other programs that have the file at `path` open, if its data is
/// all here (asking about an online-only file would download it; none can
/// be changing its data meanwhile). `None`: Windows couldn't say.
fn programs_using(path: &Path) -> Option<Vec<String>> {
    let here = std::fs::symlink_metadata(path).ok()?;
    if oneup_ntfs::dirent::recalls(here.file_attributes()) {
        return Some(Vec::new());
    }
    let me = std::process::id();
    let mut out: Vec<String> = oneup_sys::in_use::users(path)
        .ok()?
        .into_iter()
        .filter(|u| u.pid != me && !READERS.contains(&u.exe.to_lowercase().as_str()))
        .map(|u| u.exe)
        .collect();
    out.sort_unstable();
    out.dedup();
    Some(out)
}

/// What a database's place beside a working file holds
/// ([`LocalFs::classify_main`]).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Main {
    Absent,
    /// A file that isn't a database (or a folder, or opted out).
    Ordinary,
    /// A database, or a file that may be one (its content can't be read:
    /// not all here, empty).
    Database,
}

/// A file's identity and stamp, from its attributes.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct Ident {
    id: FileId,
    size: i64,
    mtime: i64,
}

impl Ident {
    fn is(&self, e: &MainExpect) -> bool {
        self.id.0 == e.id.0 && self.size == e.size && self.mtime == e.mtime
    }
}

impl LocalFs {
    pub(crate) fn look(&self) -> Look {
        #[cfg(feature = "crash-test")]
        return Look::failing(self.metadata_fails());
        #[cfg(not(feature = "crash-test"))]
        Look::default()
    }

    /// `path` relative to the sync folder, `/`-separated, without a
    /// leading `/` (as rules are matched).
    fn relative(&self, path: &Path) -> String {
        let rest = path.strip_prefix(self.root_path()).unwrap_or(path);
        rest.components().map(|c| c.as_os_str().to_string_lossy()).collect::<Vec<_>>().join("/")
    }

    /// What the configured recognition says the file at `path` is, from
    /// `head` (its first bytes, if read) and its name; with nothing in it
    /// to go by (empty, or not read), its working files beside it say
    /// SQLite.
    fn recognise(&self, dbs: &Databases, path: &Path, head: Option<&[u8]>) -> Result<Option<Recognised>> {
        let look = self.look();
        let rel = self.relative(path);
        let summary = look.exists(&beside(path, ".msf"))?;
        if let Some(r) = dbs.recognise(&rel, head, summary) {
            return Ok(Some(r));
        }
        let empty = head.is_none_or(<[u8]>::is_empty);
        Ok((empty && !dbs.opted_out(&rel) && look.has_companions(path)?)
            .then_some(Recognised { format: Format::Sqlite, by: By::Companions }))
    }

    /// [`programs_using`]; Windows not saying is [`Error::Busy`]: tried
    /// again later, never taken for "closed".
    fn programs(&self, path: &Path) -> Result<Vec<String>> {
        #[cfg(feature = "crash-test")]
        if self.restart_manager_fails() {
            return Err(Error::Busy);
        }
        programs_using(path).ok_or(Error::Busy)
    }

    /// Whether a database waits because another program has it open (for
    /// Access, also its lock file beside it), and which programs do.
    fn open_wait(&self, path: &Path, format: Format) -> Result<(Option<Wait>, Vec<String>)> {
        let programs = self.programs(path)?;
        let open = !programs.is_empty() || self.look().access_locked(path, format)?;
        Ok((open.then_some(Wait::Open), programs))
    }

    /// A state that waits, with the programs that have `path` open named
    /// as far as Windows says (only to tell the user: it waits anyway).
    fn waiting(&self, recognised: Recognised, wait: Wait, path: &Path) -> DbState {
        let programs = self.programs(path).unwrap_or_default();
        DbState { recognised, wait: Some(wait), programs }
    }

    /// Whether the file open (exclusively) as `h` at `path`, `size` bytes,
    /// is to be treated as a SQLite database with working files: the
    /// configured recognition (no `!` pattern; its content, if all of it is
    /// here; the patterns; its name, or `also_named`'s, where it goes) says
    /// SQLite or another database the user named, or its content can't
    /// say (not all here, empty).
    fn sqlite_like_on(&self, h: &win::Handle, path: &Path, size: u64, also_named: Option<&Path>) -> Result<bool> {
        let dbs = self.databases();
        if dbs.opted_out(&self.relative(path)) {
            return Ok(false);
        }
        let size = i64::try_from(size).unwrap_or(i64::MAX);
        let head = if size > 0 && self.fully_on_disk(h, size) { Some(head_of(h)?) } else { None };
        let Some(head) = head.filter(|h| !h.is_empty()) else { return Ok(true) };
        let by = |p: &Path| dbs.recognise(&self.relative(p), Some(&head), false).is_some_and(|r| may_be_sqlite(&r));
        Ok(by(path) || also_named.is_some_and(by))
    }

    /// For a replacement, move or recycling of the file open (exclusively)
    /// as `h` at `path`: whether it's a SQLite database (as
    /// [`LocalFs::sqlite_like_on`] judges) with changes beside it that
    /// aren't in it.
    pub(crate) fn pending_beside(&self, h: &win::Handle, path: &Path, size: u64) -> Result<bool> {
        if self.look().sqlite_wait(path)?.is_none() {
            return Ok(false);
        }
        self.sqlite_like_on(h, path, size, None)
    }

    /// What's in a database's place at `main`, beside a working file, from
    /// its attributes first: absent; a folder, a file a `!` pattern opts
    /// out, or one whose content says it isn't a database (ordinary); or a
    /// database (by the configured recognition, or because its content
    /// can't be told). A link or another provider's placeholder there is
    /// [`Error::DatabaseWaits`]: nothing is read from it. Content is read
    /// only through a handle on the same file (checked by ID), from an
    /// ordinary file or OneUpDrive's own placeholder all here.
    fn classify_main(&self, main: &Path, dbs: &Databases) -> Result<(Main, Option<Ident>)> {
        if dbs.opted_out(&self.relative(main)) {
            return Ok((Main::Ordinary, None));
        }
        if self.look().side(main)? == Seen::Absent {
            return Ok((Main::Absent, None));
        }
        let attrs = match win::open_path_for_attributes(main) {
            Ok(h) => h,
            Err(e) if e.is_not_found() => return Ok((Main::Absent, None)),
            Err(_) => return Err(Error::Busy),
        };
        let (attributes, tag) = win::attribute_tag(&attrs).map_err(|_| Error::Busy)?;
        if attributes & FILE_ATTRIBUTE_DIRECTORY != 0 {
            return Ok((Main::Ordinary, None));
        }
        if attributes & FILE_ATTRIBUTE_REPARSE_POINT != 0 && !(self.on_demand() && is_cloud_tag(tag)) {
            return Err(Error::DatabaseWaits);
        }
        let (_, id) = win::file_id(&attrs)?;
        let entry = win::stat_handle(&attrs, id)?.ok_or(Error::Changed)?;
        let size = i64::try_from(entry.size).unwrap_or(i64::MAX);
        let ident = Ident { id, size, mtime: entry.modified };
        if dbs.recognise(&self.relative(main), None, false).is_some_and(|r| may_be_sqlite(&r)) {
            return Ok((Main::Database, Some(ident)));
        }
        if size == 0 || !self.fully_on_disk(&attrs, size) {
            return Ok((Main::Database, Some(ident)));
        }
        drop(attrs);
        let mut w = self.volume().open_watched_path(main).map_err(|_| Error::Busy)?;
        if w.with_handle(|h| win::file_id(h).map(|(_, i)| i)).and_then(Result::ok) != Some(id) {
            return Err(Error::Changed);
        }
        let mut head = [0u8; 16];
        let mut n = 0;
        while n < head.len() {
            match w.read(&mut head[n..]) {
                Ok(0) => break,
                Ok(k) => n += k,
                Err(_) => return Err(Error::Changed),
            }
        }
        if w.broken() {
            return Err(Error::Changed);
        }
        let sqlite = Format::of_header(&head[..n]) == Some(Format::Sqlite);
        Ok((if sqlite { Main::Database } else { Main::Ordinary }, Some(ident)))
    }

    /// Holds the database at `main` with writers kept out, and checks it's
    /// still the file `ident` names. A program that has it open to write
    /// refuses the hold: [`Error::DatabaseWaits`].
    fn hold_main(&self, main: &Path, ident: Option<Ident>) -> Result<CompanionHold> {
        let h = match win::open_path_keeping_writers_out(main) {
            Ok(h) => h,
            Err(e) if e.is_busy() => return Err(Error::DatabaseWaits),
            Err(e) if e.is_not_found() => return Err(Error::Changed),
            Err(_) => return Err(Error::Busy),
        };
        let (_, id) = win::file_id(&h)?;
        let entry = win::stat_handle(&h, id)?.ok_or(Error::Changed)?;
        let now = Ident { id, size: i64::try_from(entry.size).unwrap_or(i64::MAX), mtime: entry.modified };
        if ident.is_some_and(|i| i != now) {
            return Err(Error::Changed);
        }
        Ok(CompanionHold(Some(h)))
    }

    /// Before an upload: whether the file `src` is reading is a database,
    /// or a database's working file, and if so whether it waits. SQLite's
    /// consistency is checked whatever `dbs.wait` says; that setting only
    /// decides whether other databases wait for their programs to close
    /// them. If a SQLite one doesn't wait, the rest of its group (the
    /// database, its `-wal` and `-journal`) is watched with it from now on,
    /// so a write to any of them fails the upload's reads like a write to
    /// the file itself, and each read checks again that nothing waits
    /// outside the database (a `-wal` or `-journal` created meanwhile).
    /// Reads the file's first bytes through `src`, which starts from the
    /// beginning again after.
    pub fn check_database_upload(&self, src: &mut UploadSource, dbs: &Databases) -> Result<UploadCheck> {
        let path = src.path().to_path_buf();
        let mut head = vec![0u8; HEAD];
        let mut n = 0;
        while n < head.len() {
            match src.read(&mut head[n..]) {
                Ok(0) => break,
                Ok(k) => n += k,
                Err(e) if ChangedMeanwhile::is(&e) => return Err(Error::Changed),
                Err(e) => return Err(e.into()),
            }
        }
        head.truncate(n);
        src.rewind()?;
        let look = self.look();
        if let Some((c, main)) = Companion::of_name(&file_name(&path)) {
            let main = path.with_file_name(main);
            if self.classify_main(&main, dbs)?.0 == Main::Database {
                // Sent only when it carries nothing the database needs.
                let wait = match c {
                    Companion::Wal if src.size > 0 => Some(Wait::Wal),
                    Companion::Journal if oneup_rules::database::journal_active(&head) => Some(Wait::Journal),
                    _ => look.sqlite_wait(&main)?,
                };
                let recognised = Recognised { format: Format::Sqlite, by: By::Companions };
                if let Some(w) = wait {
                    return Ok(UploadCheck::Waits(self.waiting(recognised, w, &main)));
                }
                self.watch_group(src, &main, Some(c))?;
                return Ok(UploadCheck::Consistent(recognised));
            }
        }
        let Some(recognised) = self.recognise(dbs, &path, Some(&head))? else {
            return Ok(UploadCheck::Send);
        };
        let sqlite = recognised.format.consistent() == Consistent::Sqlite || may_be_sqlite(&recognised);
        if sqlite {
            if let Some(w) = look.sqlite_wait(&path)? {
                return Ok(UploadCheck::Waits(self.waiting(recognised, w, &path)));
            }
            if recognised.format.consistent() == Consistent::Sqlite {
                self.watch_group(src, &path, None)?;
                return Ok(UploadCheck::Consistent(recognised));
            }
        }
        if !dbs.wait {
            return Ok(UploadCheck::Consistent(recognised));
        }
        Ok(UploadCheck::IfClosed { recognised, path })
    }

    /// For [`UploadCheck::IfClosed`]: why the database at `path` waits, if
    /// another program has it open. Run with the file closed. Windows not
    /// saying is [`Error::Busy`].
    pub fn database_open_by(&self, recognised: Recognised, path: &Path) -> Result<Option<DbState>> {
        let (wait, programs) = self.open_wait(path, recognised.format)?;
        Ok(wait.map(|w| DbState { recognised, wait: Some(w), programs }))
    }

    /// Watches the rest of the SQLite database `main`'s group with `src`:
    /// the database itself unless `src` is it (`of` is `None`), and its
    /// `-wal` and `-journal` other than `src`. Ordinary files only, and only
    /// those there now; one created later is caught by the state check on
    /// every read (`UploadSource::watch_sqlite`). Not the `-shm`: SQLite
    /// keeps it mapped for writing while the database is open, and it only
    /// indexes the `-wal`.
    fn watch_group(&self, src: &mut UploadSource, main: &Path, of: Option<Companion>) -> Result<()> {
        let look = self.look();
        let mut paths = Vec::new();
        if of.is_some() {
            paths.push(main.to_path_buf());
        }
        for c in [Companion::Wal, Companion::Journal] {
            if of != Some(c) {
                paths.push(beside(main, c.suffix()));
            }
        }
        for p in paths {
            match look.side(&p)? {
                Seen::Absent => continue,
                Seen::File { plain: true, .. } => {}
                // Not an ordinary file: nothing vouches for it.
                _ => return Err(Error::Changed),
            }
            match self.volume().open_watched_path(&p) {
                Ok(w) => src.watch_also(w),
                Err(e) if e.is_not_found() => {}
                // Being deleted (SQLite removes the journal at the end of
                // each transaction: access denied meanwhile), or mapped
                // for writing: changing under the read.
                Err(_) => return Err(Error::Changed),
            }
        }
        src.watch_sqlite(main.to_path_buf(), look);
        Ok(())
    }

    /// Before a download replaces a file here (`a`): whether that file is a
    /// database that must stay as it is for now ([`DbState::wait`]): open
    /// in another program, or with changes beside it that aren't in it.
    /// Read-only. With `dbs.wait` off, only the second (a database with
    /// changes beside it is never replaced; the replacement's own check
    /// refuses it too), and this says why. `None`: nothing waits. Windows
    /// not saying which programs have it open is [`Error::Busy`].
    pub fn check_database_replace(&self, a: &Approved, dbs: &Databases) -> Result<Option<DbState>> {
        let Op::Download { replace: Some(pre), .. } = a.op() else { return Ok(None) };
        let id = FileId(pre.id.0);
        let h = self.volume().open_attributes(id, self.hint(id).as_deref())?;
        let path = long(&win::final_path(&h)?);
        drop(h);
        if !self.is_inside_pub(&path) {
            return Err(Error::Outside);
        }
        let head = self.sample(pre, HEAD);
        let look = self.look();
        let pending = look.sqlite_wait(&path)?;
        let recognised = self.recognise(dbs, &path, head.as_deref())?;
        // With its content unknown, or recognised as a database that may
        // be SQLite, changes beside it hold it back.
        let sqlite_like = match (&recognised, head.as_deref()) {
            (Some(r), _) => may_be_sqlite(r),
            (None, Some(h)) => h.is_empty() && !dbs.opted_out(&self.relative(&path)),
            (None, None) => !dbs.opted_out(&self.relative(&path)),
        };
        let r = recognised.clone().unwrap_or(Recognised { format: Format::Sqlite, by: By::Companions });
        if !dbs.wait {
            return Ok(pending.filter(|_| sqlite_like).map(|w| self.waiting(r, w, &path)));
        }
        let Some(recognised) = recognised else {
            return Ok(pending.filter(|_| sqlite_like).map(|w| self.waiting(r, w, &path)));
        };
        // Open in a program first: that's what the user can act on.
        let (open, programs) = self.open_wait(&path, recognised.format)?;
        let wait = if sqlite_like { open.or(pending) } else { open };
        Ok(wait.map(|w| DbState { recognised, wait: Some(w), programs }))
    }

    /// Where a download into a SQLite working file's name goes: `None`
    /// for anything else.
    fn download_target(&self, a: &Approved) -> Result<Option<PathBuf>> {
        let Op::Download { to, name, .. } = a.op() else { return Ok(None) };
        if Companion::of_name(name).is_none() {
            return Ok(None);
        }
        let (_pin, dir) = self.pin_folder_pub(*to)?;
        Ok(Some(dir.join(name)))
    }

    /// Before a download into a SQLite working file's name (`a`): whether
    /// it waits while another program has the database beside it open. The
    /// database here must also be the one OneDrive's version is: the caller
    /// checks that from the state database ([`CompanionCheck::Go`]), and
    /// [`LocalFs::hold_for_companion`] makes the final check, under a hold.
    pub fn check_companion_change(&self, a: &Approved, dbs: &Databases) -> Result<CompanionCheck> {
        let Some(path) = self.download_target(a)? else { return Ok(CompanionCheck::Ordinary) };
        let name = file_name(&path);
        let Some((_, main_name)) = Companion::of_name(&name) else { return Ok(CompanionCheck::Ordinary) };
        let main = path.with_file_name(main_name);
        if self.classify_main(&main, dbs)?.0 != Main::Database {
            return Ok(CompanionCheck::Ordinary);
        }
        let recognised = Recognised { format: Format::Sqlite, by: By::Companions };
        let programs = self.programs(&main)?;
        if !programs.is_empty() {
            return Ok(CompanionCheck::Waits(DbState { recognised, wait: Some(Wait::Open), programs }));
        }
        Ok(CompanionCheck::Go { main: main_name.to_string() })
    }

    /// The final check before a download into a SQLite working file's name
    /// (`a`), made under a hold the caller keeps for exactly the placement,
    /// never during a transfer. The database's place is looked at from its
    /// attributes first: with `expect` (the database the state database
    /// knows there), it must be that file at that stamp, whatever it holds;
    /// without, there must be no database there (an ordinary file is fine).
    /// Then the database is held, writers kept out (a program that has it
    /// open to write refuses the hold), and checked to be the same file.
    /// Refused: [`Error::DatabaseWaits`].
    pub fn hold_for_companion(&self, a: &Approved, expect: Option<&MainExpect>) -> Result<CompanionHold> {
        let none = CompanionHold(None);
        let Some(path) = self.download_target(a)? else { return Ok(none) };
        let name = file_name(&path);
        let Some((_, main_name)) = Companion::of_name(&name) else { return Ok(none) };
        let main = path.with_file_name(main_name);
        let (kind, ident) = self.classify_main(&main, &self.databases())?;
        match (expect, kind, ident) {
            (Some(e), Main::Database | Main::Ordinary, Some(i)) if i.is(e) => {}
            (Some(_), _, _) => return Err(Error::DatabaseWaits),
            (None, Main::Absent | Main::Ordinary, _) => return Ok(none),
            (None, Main::Database, _) => return Err(Error::DatabaseWaits),
        }
        self.hold_main(&main, ident)
    }

    /// The final checks before the file open as `h` (exclusively) at
    /// `from`, `size` bytes, is moved to `to` (`to` is `None` for
    /// recycling), made by the move itself, right before it, on the actual
    /// names. Refused ([`Error::DatabaseWaits`]):
    /// * a database ([`LocalFs::sqlite_like_on`]) with changes beside it
    ///   (they'd stay behind), or moved beside changes at `to` (they'd be
    ///   applied to it);
    /// * a SQLite working file whose database is open, or that holds
    ///   changes the database needs;
    /// * anything moved into a database's working-file name.
    ///
    /// The database beside a working file moved away is returned held,
    /// writers kept out: the caller keeps it until the move is done.
    pub(crate) fn check_move(
        &self,
        h: &win::Handle,
        from: &Path,
        size: u64,
        to: Option<&Path>,
    ) -> Result<CompanionHold> {
        let look = self.look();
        let dbs = self.databases();
        let same =
            to.is_some_and(|t| oneup_sync::fold(&t.to_string_lossy()) == oneup_sync::fold(&from.to_string_lossy()));
        let pending_here = look.sqlite_wait(from)?.is_some();
        let pending_there = match to {
            Some(t) if !same => look.sqlite_wait(t)?.is_some(),
            _ => false,
        };
        if (pending_here || pending_there) && self.sqlite_like_on(h, from, size, to)? {
            return Err(Error::DatabaseWaits);
        }
        if let Some(t) = to.filter(|_| !same)
            && let Some((_, main)) = Companion::of_name(&file_name(t))
            && self.classify_main(&t.with_file_name(main), &dbs)?.0 == Main::Database
        {
            return Err(Error::DatabaseWaits);
        }
        let from_name = file_name(from);
        let Some((c, main)) = Companion::of_name(&from_name) else { return Ok(CompanionHold(None)) };
        let main = from.with_file_name(main);
        let (kind, ident) = self.classify_main(&main, &dbs)?;
        if kind != Main::Database {
            return Ok(CompanionHold(None));
        }
        let hold = self.hold_main(&main, ident)?;
        if look.holds_changes(c, from)?.is_some() {
            return Err(Error::DatabaseWaits);
        }
        Ok(hold)
    }

    /// Why an operation on a file here was refused for a database
    /// (`Error::DatabaseWaits`), for the report, as far as can be told
    /// now. `None`: nothing is now.
    pub fn database_pending(&self, a: &Approved) -> Option<DbState> {
        let look = self.look();
        let recognised = Recognised { format: Format::Sqlite, by: By::Companions };
        let (from, to) = match a.op() {
            Op::Download { to, name, replace, .. } => {
                let (_pin, dir) = self.pin_folder_pub(*to).ok()?;
                let target = dir.join(name);
                if replace.is_none() || Companion::of_name(name).is_some() {
                    if let Some((_, main)) = Companion::of_name(name) {
                        let main = target.with_file_name(main);
                        let programs = self.programs(&main).unwrap_or_default();
                        let wait = if programs.is_empty() { Wait::Behind } else { Wait::Open };
                        return Some(DbState { recognised, wait: Some(wait), programs });
                    }
                    return look.sqlite_wait(&target).ok().flatten().map(|w| self.waiting(recognised, w, &target));
                }
                (target, None)
            }
            Op::MoveLocal { local, to, name, .. } => {
                let (_pin, dir) = self.pin_folder_pub(*to).ok()?;
                (self.path_of(local.id)?, Some(dir.join(name)))
            }
            Op::RenameLocal { local, dir, name } | Op::ConflictCopy { local, dir, name, .. } => {
                let (_pin, d) = self.pin_folder_pub(*dir).ok()?;
                (self.path_of(local.id)?, Some(d.join(name)))
            }
            Op::RecycleLocal { local, .. } => (self.path_of(local.id)?, None),
            _ => return None,
        };
        if let Ok(Some(w)) = look.sqlite_wait(&from) {
            return Some(self.waiting(recognised, w, &from));
        }
        if let Some((c, main)) = Companion::of_name(&file_name(&from)) {
            let main = from.with_file_name(main);
            let programs = self.programs(&main).unwrap_or_default();
            if !programs.is_empty() {
                return Some(DbState { recognised, wait: Some(Wait::Open), programs });
            }
            if let Ok(Some(w)) = look.holds_changes(c, &from) {
                return Some(DbState { recognised, wait: Some(w), programs });
            }
        }
        let to = to?;
        if let Ok(Some(w)) = look.sqlite_wait(&to) {
            return Some(self.waiting(recognised, w, &to));
        }
        Some(DbState { recognised, wait: Some(Wait::Behind), programs: Vec::new() })
    }

    /// Where item `id` is now, if it can be found (read-only).
    fn path_of(&self, id: LocalId) -> Option<PathBuf> {
        let id = FileId(id.0);
        let h = self.volume().open_attributes(id, self.hint(id).as_deref()).ok()?;
        Some(long(&win::final_path(&h).ok()?))
    }

    /// Right before a new file from OneDrive is placed (`a`, a download
    /// into a free name, its content in `staged`; `None` for an online-only
    /// placeholder, whose content isn't here to say what it is): if it may
    /// be a SQLite database (its content or name says so, or can't tell)
    /// and a `-wal` or `-journal` with changes is already beside its name,
    /// SQLite would apply them to it. That's allowed only for working files
    /// that came from OneDrive with it: `known` (synced, at OneDrive's
    /// version, as the state database has them), checked by file ID and
    /// stamp. Otherwise [`Error::DatabaseWaits`].
    pub fn check_new_beside(&self, a: &Approved, staged: Option<&mut Staged>, known: &[MainExpect]) -> Result<()> {
        use std::io::{Seek as _, SeekFrom};
        let Op::Download { to, name, replace: None, .. } = a.op() else { return Ok(()) };
        let (_pin, dir) = self.pin_folder_pub(*to)?;
        let target = dir.join(name);
        let look = self.look();
        if look.sqlite_wait(&target)?.is_none() {
            return Ok(());
        }
        let head = match staged.and_then(Staged::file) {
            Some(f) => {
                f.seek(SeekFrom::Start(0))?;
                let mut head = Vec::with_capacity(HEAD);
                f.by_ref().take(HEAD as u64).read_to_end(&mut head)?;
                f.seek(SeekFrom::Start(0))?;
                Some(head)
            }
            None => None,
        };
        let dbs = self.databases();
        let rel = self.relative(&target);
        if dbs.opted_out(&rel) {
            return Ok(());
        }
        if let Some(h) = head.as_deref().filter(|h| !h.is_empty())
            && !dbs.recognise(&rel, Some(h), false).is_some_and(|r| may_be_sqlite(&r))
        {
            return Ok(());
        }
        for c in [Companion::Wal, Companion::Journal] {
            let p = beside(&target, c.suffix());
            if look.holds_changes(c, &p)?.is_none() {
                continue;
            }
            let h = win::open_path_for_attributes(&p).map_err(|_| Error::DatabaseWaits)?;
            let (_, id) = win::file_id(&h)?;
            let e = win::stat_handle(&h, id)?.ok_or(Error::Changed)?;
            let size = i64::try_from(e.size).unwrap_or(i64::MAX);
            if !known.iter().any(|k| k.id.0 == id.0 && k.size == size && k.mtime == e.modified) {
                return Err(Error::DatabaseWaits);
            }
        }
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used)]
    use super::*;

    fn dir(tag: &str) -> PathBuf {
        let d = std::env::temp_dir().join(format!("oneup-effects-db-{tag}-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&d);
        std::fs::create_dir_all(&d).unwrap();
        d
    }

    #[test]
    fn a_sqlite_database_waits_for_its_wal_and_an_active_journal_only() {
        let d = dir("pending");
        let db = d.join("app.db");
        let look = Look::default();
        std::fs::write(&db, b"SQLite format 3\0").unwrap();
        assert_eq!(look.sqlite_wait(&db).unwrap(), None, "nothing beside it");
        std::fs::write(d.join("app.db-wal"), b"").unwrap();
        std::fs::write(d.join("app.db-journal"), [0u8; 512]).unwrap();
        assert_eq!(look.sqlite_wait(&db).unwrap(), None, "an empty -wal and a zeroed -journal hold nothing");
        std::fs::write(d.join("app.db-wal"), b"\x37\x7f\x06\x82").unwrap();
        assert_eq!(look.sqlite_wait(&db).unwrap(), Some(Wait::Wal));
        std::fs::write(d.join("app.db-wal"), b"").unwrap();
        std::fs::write(d.join("app.db-journal"), [0xD9, 0xD5, 0x05, 0xF9, 0x20, 0xA1, 0x63, 0xD7]).unwrap();
        assert_eq!(look.sqlite_wait(&db).unwrap(), Some(Wait::Journal));
        let _ = std::fs::remove_dir_all(&d);
    }

    /// Only "not found" means absent: attributes that can't be read are
    /// "try again", never "nothing there".
    #[test]
    fn attributes_that_cant_be_read_are_never_absent() {
        let d = dir("unreadable");
        let db = d.join("app.db");
        std::fs::write(&db, b"SQLite format 3\0").unwrap();
        std::fs::write(d.join("app.db-wal"), b"changes").unwrap();
        let failing = Look { fail: Some("-wal".into()) };
        assert!(matches!(failing.sqlite_wait(&db), Err(Error::Busy)));
        assert!(matches!(failing.has_companions(&db), Err(Error::Busy)));
        assert_eq!(Look::default().side(&d.join("nothing")).unwrap(), Seen::Absent);
        let _ = std::fs::remove_dir_all(&d);
    }

    #[test]
    fn an_access_lock_file_says_the_database_is_open() {
        let d = dir("access");
        let db = d.join("Orders.accdb");
        let look = Look::default();
        std::fs::write(&db, b"\0\x01\0\0Standard ACE DB\0").unwrap();
        assert!(!look.access_locked(&db, Format::Access).unwrap());
        std::fs::write(d.join("Orders.laccdb"), b"PC USER").unwrap();
        assert!(look.access_locked(&db, Format::Access).unwrap());
        assert!(!look.access_locked(&db, Format::KeePass).unwrap());
        let _ = std::fs::remove_dir_all(&d);
    }

    #[test]
    fn a_file_held_open_by_another_program_is_named() {
        let d = dir("open");
        let f = d.join("vault.kdbx");
        std::fs::write(&f, b"x").unwrap();
        assert_eq!(programs_using(&f), Some(Vec::new()), "our own process doesn't count");
        // Another program keeps it open: PowerShell holding a FileStream.
        let script = format!(
            "$f=[IO.File]::Open('{}','Open','Read','ReadWrite'); [Console]::Out.WriteLine('open'); [Console]::In.ReadLine() | Out-Null; $f.Close()",
            f.display()
        );
        let mut child = std::process::Command::new("powershell.exe")
            .args(["-NoProfile", "-NonInteractive", "-Command", &script])
            .stdin(std::process::Stdio::piped())
            .stdout(std::process::Stdio::piped())
            .spawn()
            .unwrap();
        let mut line = String::new();
        std::io::BufRead::read_line(&mut std::io::BufReader::new(child.stdout.take().unwrap()), &mut line).unwrap();
        let open = programs_using(&f).unwrap();
        drop(child.stdin.take());
        child.wait().unwrap();
        assert_eq!(open.iter().map(|e| e.to_lowercase()).collect::<Vec<_>>(), ["powershell.exe"]);
        assert_eq!(programs_using(&f), Some(Vec::new()), "closed again");
        let _ = std::fs::remove_dir_all(&d);
    }
}
