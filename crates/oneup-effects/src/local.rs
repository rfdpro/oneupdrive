//! Local effects on a Windows sync folder (see the crate docs for the rules).

use std::ffi::OsString;
use std::fs::{File, OpenOptions};
use std::io::Write as _;
use std::path::{Component, Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering::Relaxed};
use std::time::{SystemTime, UNIX_EPOCH};

use oneup_gate::{Approved, OldCopy};
use oneup_ntfs::FileId;
use oneup_ntfs::dirent::{DirEntry, FILE_ATTRIBUTE_REPARSE_POINT};
use oneup_ntfs::win::{self, Handle, Volume, WatchedFile};
use oneup_rules::ENGINE_DIR;
use oneup_sync::rewind::Step;
use oneup_sync::{Hash, LocalId, LocalPre, Op};

use crate::name::is_valid_name;
use crate::versions::{Slot, Trim};

/// Inside [`ENGINE_DIR`]: replaced and recycled items, one folder each.
const VERSIONS: &str = "versions";
/// Inside [`ENGINE_DIR`]: downloads in progress. Same volume as their
/// destination, so placing one is a rename.
const STAGING: &str = "staging";
/// Inside [`ENGINE_DIR`]: the sync folder's identity token (`data-safety.md`
/// invariant 4). A folder at the right path with the right file ID but
/// without our token (restored from a backup, re-created by a tool that
/// preserves IDs) is not trusted.
const MARKER: &str = "root.marker";
/// Inside [`ENGINE_DIR`]: present while the sync folder is OneUpDrive's
/// Files On-Demand sync root. If the registration disappears without
/// OneUpDrive removing it (another program unregistered the folder, as the
/// OneDrive app's unlinking would), Windows deletes every online-only
/// placeholder, and those deletions must never reach OneDrive: the folder
/// then fails its identity check instead (`data-safety.md` invariant 15).
const ON_DEMAND: &str = "on-demand";
const LOST_ON_DEMAND: &str = "Windows no longer has it as OneUpDrive's Files On-Demand folder, so it may have removed the online-only files there. Nothing syncs until Files On-Demand is turned on again (Settings, Files, the folder's details)";
/// Extension of staged downloads. Only files with it are ever cleaned up
/// from the staging folder.
const PART_EXT: &str = "oneup-part";
/// Inside a version slot: the item's former path relative to the sync
/// folder, for `oneup restore`.
const ORIGIN: &str = "origin.txt";
/// Inside [`ENGINE_DIR`]: what [`LocalFs::versions`] measured of each slot,
/// with the slot folder's creation time. Only a cache: a slot is filled
/// once (its item moved or copied in, then its [`ORIGIN`] written) and
/// never changed after, so what was measured stays true while a slot of
/// that creation time is there. The listing of `versions` decides which
/// slots exist; a slot missing here, or of another creation time, is
/// measured again.
const VERSIONS_CACHE: &str = "versions.cache";

/// Attributes that make an item something other than an ordinary file or
/// folder whose data is on disk. Such items are never changed.
const NOT_ORDINARY: u32 = oneup_ntfs::dirent::NOT_PLAIN;

#[derive(Debug, thiserror::Error)]
pub enum Error {
    /// The item (or the destination folder) is no longer what was planned:
    /// changed, moved, deleted, or of another kind. Nothing was done; plan
    /// again from fresh state.
    #[error("changed since it was planned")]
    Changed,
    /// Another program has the item open. Nothing was done; try later.
    #[error("in use by another program")]
    Busy,
    /// The destination name is taken. Nothing was done.
    #[error("the name is already taken")]
    NameTaken,
    /// A placeholder, link or other special item: never changed.
    #[error("not an ordinary file or folder")]
    NotOrdinary,
    /// Recycling a folder that has contents (the planner recycles folders
    /// only once empty; something was added since).
    #[error("the folder is not empty")]
    NotEmpty,
    #[error("the name can't be used on Windows")]
    BadName,
    /// A file to replace has changes beside it that aren't in it yet (a
    /// SQLite database's `-wal` or `-journal`): replacing it would let
    /// SQLite apply them to the new file. Nothing was done; try later.
    #[error("a database with changes beside it that aren't in it yet")]
    DatabaseWaits,
    #[error("outside the sync folder")]
    Outside,
    /// The token is for an operation this layer doesn't run (a remote or
    /// bookkeeping one), or lacks a precondition it needs.
    #[error("not a local operation")]
    WrongOp,
    /// The sync folder failed its identity check: **halt** everything, never
    /// conclude that its contents were deleted.
    #[error("the sync folder failed its identity check: {0}")]
    RootChanged(&'static str),
    #[error("the sync folder is managed by {0}")]
    OtherEngine(String),
    /// A download's old content was moved into the version store, placing
    /// the new content failed, and the old content couldn't be moved back
    /// (the name was taken meanwhile). Nothing is lost: it is at `kept`.
    #[error("the previous version could not be put back; it is kept in the version store")]
    KeptAside { kept: PathBuf },
    #[error(transparent)]
    Win(win::Error),
    #[error(transparent)]
    Io(#[from] std::io::Error),
}

impl From<win::Error> for Error {
    /// The OS errors that mean "the world changed" become the matching
    /// variants, so callers can decide skip / retry / report.
    fn from(e: win::Error) -> Self {
        if e.is_not_found() {
            Error::Changed
        } else if e.is_busy() {
            Error::Busy
        } else if e.is_exists() {
            Error::NameTaken
        } else {
            Error::Win(e)
        }
    }
}

pub type Result<T, E = Error> = std::result::Result<T, E>;

/// What an effect did, for the executor to record.
#[derive(Debug)]
pub enum Done {
    /// The item is now in `parent` as described by `entry` (created, moved,
    /// renamed or downloaded). `kept`: where a replaced file's old content
    /// went (`None` if it was not kept, see [`OldCopy::DropIfSynced`]).
    Placed { parent: LocalId, entry: DirEntry, kept: Option<PathBuf> },
    /// Moved into the version store (`kept`), or removed without a local
    /// copy (`None`, see [`OldCopy::DropIfSynced`]).
    Recycled { kept: Option<PathBuf> },
    /// The file's QuickXorHash, for exactly this size and last-write time.
    Hashed { hash: Hash, size: i64, mtime: i64 },
    /// A placeholder's content was freed; `entry` is how it is now.
    Freed { entry: DirEntry },
}

/// A download being written into the staging folder. Dropped without being
/// placed, its partial file is removed (it is ours, never user content).
#[derive(Debug)]
pub struct Staged {
    path: PathBuf,
    file: Option<File>,
}

impl Staged {
    /// Where the content goes. Write all of it, verify it, then hand this to
    /// [`LocalFs::place_download`].
    pub fn file(&mut self) -> Option<&mut File> {
        self.file.as_mut()
    }
}

impl Drop for Staged {
    fn drop(&mut self) {
        self.file = None;
        if !self.path.as_os_str().is_empty() {
            // Our own partial download (PART_EXT, in our staging folder).
            let _ = std::fs::remove_file(&self.path);
        }
    }
}

/// A local file opened for an upload, at the checked stamp. Other programs
/// may keep using it, writing, replacing and deleting it included: a save
/// must never fail because the file is being uploaded. Instead, every read
/// through this fails with [`ChangedMeanwhile`] once anyone has written
/// the file since it was opened, or needed it closed (`win::WatchedFile`),
/// so a copy mixing two versions never completes an upload.
#[derive(Debug)]
pub struct UploadSource {
    file: WatchedFile,
    pub size: i64,
    pub mtime: i64,
    /// Where it was opened (`\\?\` form).
    path: PathBuf,
    /// Other files watched with it: the rest of a SQLite database's group
    /// ([`LocalFs::check_database_upload`]). A change to any of them counts
    /// as a change to this one.
    also: Vec<WatchedFile>,
    /// The SQLite database this file belongs to (or is): every read checks
    /// that nothing waits outside it (`-wal`, `-journal`), so one created
    /// during the read counts as a change too.
    sqlite: Option<(PathBuf, crate::database::Look)>,
}

impl UploadSource {
    /// Whether nobody has written the file (or needed it closed) since it
    /// was opened, nor any file watched with it: what was read so far is
    /// the checked version.
    pub fn unchanged(&self) -> bool {
        !self.file.broken()
            && self.also.iter().all(|w| !w.broken())
            && self.sqlite.as_ref().is_none_or(|(main, look)| matches!(look.sqlite_wait(main), Ok(None)))
    }

    pub(crate) fn path(&self) -> &Path {
        &self.path
    }

    pub(crate) fn watch_also(&mut self, w: WatchedFile) {
        self.also.push(w);
    }

    pub(crate) fn watch_sqlite(&mut self, main: PathBuf, look: crate::database::Look) {
        self.sqlite = Some((main, look));
    }

    pub fn rewind(&mut self) -> std::io::Result<()> {
        self.file.rewind();
        Ok(())
    }
}

impl std::io::Read for UploadSource {
    fn read(&mut self, buf: &mut [u8]) -> std::io::Result<usize> {
        let read = self.file.read(buf);
        if !self.unchanged() {
            return Err(std::io::Error::other(ChangedMeanwhile));
        }
        read
    }
}

/// A file read and hashed while its program could go on saving it
/// ([`LocalFs::witness`]): it stands for that content until written, or
/// needed closed by another program (its watched handle then closes).
pub struct Witnessed {
    /// `None`: a dehydrated placeholder whose identity vouched (nothing
    /// here to write).
    file: Option<WatchedFile>,
}

impl Witnessed {
    /// No write since it was read.
    pub fn unchanged(&self) -> bool {
        self.file.as_ref().is_none_or(|f| !f.broken())
    }
}

/// A planned file opened for reading at its checked stamp, watched from
/// the open on.
struct OpenedToRead {
    file: WatchedFile,
    size: i64,
    mtime: i64,
    path: PathBuf,
}

impl OpenedToRead {
    fn into_source(self) -> UploadSource {
        UploadSource {
            file: self.file,
            size: self.size,
            mtime: self.mtime,
            path: self.path,
            also: Vec::new(),
            sqlite: None,
        }
    }
}

/// An [`UploadSource`]'s file was written while it was being read.
#[derive(Debug, thiserror::Error)]
#[error("the file changed while it was read")]
pub struct ChangedMeanwhile;

impl ChangedMeanwhile {
    pub fn is(e: &std::io::Error) -> bool {
        e.get_ref().is_some_and(|inner| inner.is::<Self>())
    }
}

/// Test-only (feature `crash-test`): what an effect stopped by an injected
/// crash returns ([`LocalFs::crash_after`]). It travels as an I/O error, so
/// `Error` is the same with the feature on or off.
#[cfg(feature = "crash-test")]
#[derive(Debug, thiserror::Error)]
#[error("injected crash")]
pub struct Crashed;

#[cfg(feature = "crash-test")]
impl Crashed {
    pub fn is(e: &Error) -> bool {
        matches!(e, Error::Io(io) if io.get_ref().is_some_and(|inner| inner.is::<Self>()))
    }
}

/// One sync folder, opened for changes after its identity check.
#[derive(Debug)]
pub struct LocalFs {
    volume: Volume,
    /// The folder's normalised path in `\\?\` form (long paths work).
    root: PathBuf,
    root_id: FileId,
    serial: u64,
    token: String,
    engine: PathBuf,
    seq: AtomicU64,
    /// The folder is OneUpDrive's own Cloud Files sync root for this pair
    /// (Files On-Demand): our placeholders are items like any other, and
    /// cloud content arrives as placeholders (`ondemand.rs`).
    on_demand: bool,
    locator: Locate,
    /// What counts as a database, for the checks made under a hold
    /// ([`LocalFs::set_databases`]).
    databases: std::sync::RwLock<oneup_rules::database::Databases>,
    /// Crash points left to pass before the injected crash; negative: off.
    #[cfg(feature = "crash-test")]
    crash: std::sync::atomic::AtomicI64,
    /// Test-only: asking Windows which programs have a file open fails.
    #[cfg(feature = "crash-test")]
    restart_manager_fails: std::sync::atomic::AtomicBool,
    /// Test-only: reading the attributes of files with this name ending
    /// fails.
    #[cfg(feature = "crash-test")]
    metadata_fails: std::sync::RwLock<Option<String>>,
    /// See [`LocalFs::on_removing`].
    #[cfg(feature = "crash-test")]
    removing: Removing,
}

/// Test-only: called with a duplicate's path while [`LocalFs::remove_copy`]
/// holds it open exclusively, its place checked ([`LocalFs::on_removing`]).
#[cfg(feature = "crash-test")]
pub type RemovingHook = std::sync::Arc<dyn Fn(&Path) + Send + Sync>;

#[cfg(feature = "crash-test")]
#[derive(Default)]
struct Removing(std::sync::Mutex<Option<RemovingHook>>);

#[cfg(feature = "crash-test")]
impl std::fmt::Debug for Removing {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("Removing")
    }
}

/// Where the state database last saw a local item: its path relative to
/// the sync folder, `/`-separated (`/a/b.txt`; `/` for the folder itself).
/// The effects find items there by path, and by ID only if it no longer
/// leads to them: while a file is open through a handle opened by ID, a
/// program saving it by renaming another file over it fails
/// (`win::Volume`'s docs). Only a hint: what is found there is used only if
/// its file ID is the item's.
pub type Locator = Box<dyn Fn(LocalId) -> Option<String> + Send + Sync>;

#[derive(Default)]
struct Locate(Option<Locator>);

impl std::fmt::Debug for Locate {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(if self.0.is_some() { "Locate(Some)" } else { "Locate(None)" })
    }
}

impl LocalFs {
    /// Opens the sync folder at `root`, which must still be the folder with
    /// this volume serial and file ID, and must carry the marker `token`.
    /// `first_use`: the folder is being set up, so a missing marker is
    /// created; otherwise a missing marker fails the identity check.
    ///
    /// Creates `.oneup\versions` and `.oneup\staging` if needed, and clears
    /// partial downloads left by an interrupted run.
    pub fn open(root: &Path, volume_serial: u64, root_id: u128, token: &str, first_use: bool) -> Result<Self> {
        if let Some(info) = oneup_cfapi::other_sync_root(root) {
            return Err(Error::OtherEngine(info.provider_name));
        }
        let h = win::open_dir(root).map_err(|_| Error::RootChanged("the sync folder can't be opened"))?;
        let (serial, id) = win::file_id(&h)?;
        if serial != volume_serial || id != FileId(root_id) {
            return Err(Error::RootChanged("a different folder is at the sync folder's path"));
        }
        let volume = Volume::open_containing(root)?;
        let root = long(&win::final_path(&h)?);
        let engine = root.join(ENGINE_DIR);
        ensure_dir(&engine)?;
        ensure_dir(&engine.join(VERSIONS))?;
        ensure_dir(&engine.join(STAGING))?;
        // Our folder by its Cloud Files registration, or by the shell's
        // record for this pair when that registration lost its name and
        // identity (Windows has been seen doing that):
        // taken for an ordinary folder, its online-only files would have
        // looked like files to download. `oneup refresh-explorer-entry`
        // restores the identity at the agent's start.
        let on_demand = oneup_cfapi::provider::registration(&root)
            .is_some_and(|(name, identity)| name == oneup_cfapi::PROVIDER_NAME && identity == token.as_bytes())
            || (oneup_cfapi::provider::registration(&root).is_some()
                && oneup_cfapi::shell::is_pairs_root(&root, token));
        let fs = Self {
            volume,
            root,
            root_id: id,
            serial,
            token: token.to_string(),
            engine,
            seq: AtomicU64::new(0),
            on_demand,
            locator: Locate::default(),
            databases: std::sync::RwLock::default(),
            #[cfg(feature = "crash-test")]
            crash: std::sync::atomic::AtomicI64::new(-1),
            #[cfg(feature = "crash-test")]
            restart_manager_fails: std::sync::atomic::AtomicBool::new(false),
            #[cfg(feature = "crash-test")]
            metadata_fails: std::sync::RwLock::default(),
            #[cfg(feature = "crash-test")]
            removing: Removing::default(),
        };
        match std::fs::read_to_string(fs.engine.join(MARKER)) {
            Ok(_) => {}
            Err(e) if e.kind() == std::io::ErrorKind::NotFound && first_use => {
                let mut f = OpenOptions::new().write(true).create_new(true).open(fs.engine.join(MARKER))?;
                f.write_all(token.as_bytes())?;
                f.sync_all()?;
            }
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => {
                return Err(Error::RootChanged("the sync folder's marker is missing"));
            }
            Err(e) => return Err(e.into()),
        }
        let expected = fs.engine.join(ON_DEMAND);
        if fs.on_demand {
            if !expected.exists() {
                std::fs::write(&expected, token.as_bytes())?;
            }
        } else if expected.exists() {
            return Err(Error::RootChanged(LOST_ON_DEMAND));
        }
        fs.check_root()?;
        fs.clear_staging();
        Ok(fs)
    }

    /// Whether `root` was OneUpDrive's Files On-Demand folder when last
    /// opened (its own record says so), whatever Windows says now.
    pub fn was_on_demand(root: &Path) -> bool {
        root.join(ENGINE_DIR).join(ON_DEMAND).exists()
    }

    /// Before OneUpDrive itself unregisters its sync root `root` (turning
    /// Files On-Demand off, releasing the folder): it isn't expected to be
    /// one any more. Only OneUpDrive's own file goes.
    pub fn forget_on_demand(root: &Path) -> std::io::Result<()> {
        match std::fs::remove_file(root.join(ENGINE_DIR).join(ON_DEMAND)) {
            Err(e) if e.kind() != std::io::ErrorKind::NotFound => Err(e),
            _ => Ok(()),
        }
    }

    /// Re-checks the sync folder's identity: same path, same volume and
    /// file ID, same marker. The executor calls this before every round.
    pub fn check_root(&self) -> Result<()> {
        let h = win::open_dir(&self.root).map_err(|_| Error::RootChanged("the sync folder can't be opened"))?;
        if win::file_id(&h)? != (self.serial, self.root_id) {
            return Err(Error::RootChanged("a different folder is at the sync folder's path"));
        }
        match std::fs::read_to_string(self.engine.join(MARKER)) {
            Ok(s) if s.trim() == self.token => {}
            Ok(_) => return Err(Error::RootChanged("the sync folder's marker belongs to another setup")),
            Err(_) => return Err(Error::RootChanged("the sync folder's marker is missing")),
        }
        if self.on_demand
            && !oneup_cfapi::provider::registration(&self.root)
                .is_some_and(|(name, identity)| name == oneup_cfapi::PROVIDER_NAME && identity == self.token.as_bytes())
        {
            return Err(Error::RootChanged(LOST_ON_DEMAND));
        }
        Ok(())
    }

    pub fn root_id(&self) -> LocalId {
        LocalId(self.root_id.0)
    }

    /// Lets the effects find items where the state database last saw them
    /// ([`Locator`]). Without one, every item is found by ID.
    pub fn set_locator(&mut self, locator: Locator) {
        self.locator = Locate(Some(locator));
    }

    /// How many items had to be opened by ID to be found: for tests and
    /// diagnostics.
    pub fn opened_by_id(&self) -> u64 {
        self.volume.opened_by_id()
    }

    /// Where item `id` is expected ([`Locator`]), as a full path, if a
    /// locator knows it.
    pub(crate) fn hint(&self, id: FileId) -> Option<PathBuf> {
        let rel = (self.locator.0.as_ref()?)(LocalId(id.0))?;
        let mut path = self.root.clone();
        for part in rel.split('/').filter(|p| !p.is_empty()) {
            // Names from the store are single components; anything else
            // isn't a usable hint (`\\?\` paths aren't normalised).
            if part == "." || part == ".." || part.contains('\\') {
                return None;
            }
            path.push(part);
        }
        Some(path)
    }

    /// What counts as a database from now on (`[databases]`), for the
    /// checks the effects make themselves.
    pub fn set_databases(&self, d: oneup_rules::database::Databases) {
        *self.databases.write().unwrap_or_else(std::sync::PoisonError::into_inner) = d;
    }

    /// What counts as a database now ([`LocalFs::set_databases`]).
    pub fn databases(&self) -> oneup_rules::database::Databases {
        self.databases.read().unwrap_or_else(std::sync::PoisonError::into_inner).clone()
    }

    /// Test-only: asking Windows which programs have a file open fails
    /// (`true`) or works again.
    #[cfg(feature = "crash-test")]
    pub fn fail_restart_manager(&self, on: bool) {
        self.restart_manager_fails.store(on, Relaxed);
    }

    #[cfg(feature = "crash-test")]
    pub(crate) fn restart_manager_fails(&self) -> bool {
        self.restart_manager_fails.load(Relaxed)
    }

    /// Test-only: reading the attributes of a file whose name ends with
    /// `suffix` (what's beside a database) fails, as when access is denied;
    /// `None` makes it work again.
    #[cfg(feature = "crash-test")]
    pub fn fail_metadata(&self, suffix: Option<&str>) {
        *self.metadata_fails.write().unwrap_or_else(std::sync::PoisonError::into_inner) = suffix.map(str::to_string);
    }

    #[cfg(feature = "crash-test")]
    pub(crate) fn metadata_fails(&self) -> Option<String> {
        self.metadata_fails.read().unwrap_or_else(std::sync::PoisonError::into_inner).clone()
    }

    /// Whether the sync folder is in Files On-Demand mode (see `ondemand.rs`).
    pub fn on_demand(&self) -> bool {
        self.on_demand
    }

    /// Test-only: the crash point after `n` others have passed (here and
    /// wherever the caller counts through [`LocalFs::crash_now`]) is where
    /// the process dies. One inside an effect stops it there and returns
    /// [`Crashed`]: nothing after it runs, no rollback included. `None`
    /// turns it off.
    #[cfg(feature = "crash-test")]
    pub fn crash_after(&self, n: Option<u32>) {
        self.crash.store(n.map_or(-1, i64::from), Relaxed);
    }

    /// Test-only: `f` runs inside [`LocalFs::remove_copy`] once the copy is
    /// open exclusively and its place checked, just before it leaves it, so
    /// a test can try what another program might do in that moment. `None`
    /// turns it off.
    #[cfg(feature = "crash-test")]
    pub fn on_removing(&self, f: Option<RemovingHook>) {
        *self.removing.0.lock().unwrap_or_else(std::sync::PoisonError::into_inner) = f;
    }

    /// Test-only: whether a crash is still to come ([`LocalFs::crash_after`]).
    #[cfg(feature = "crash-test")]
    pub fn crash_armed(&self) -> bool {
        self.crash.load(Relaxed) >= 0
    }

    /// Test-only: counts a crash point; `true` if the process dies here.
    #[cfg(feature = "crash-test")]
    pub fn crash_now(&self) -> bool {
        self.crash.fetch_sub(1, Relaxed) == 0
    }

    /// A place inside a multi-step effect where a killed process would
    /// leave it half done. Without the `crash-test` feature, nothing.
    fn crash_point(&self) -> Result<()> {
        #[cfg(feature = "crash-test")]
        if self.crash_now() {
            return Err(Error::Io(std::io::Error::other(Crashed)));
        }
        Ok(())
    }

    pub(crate) fn volume(&self) -> &Volume {
        &self.volume
    }

    pub(crate) fn root_path(&self) -> &Path {
        &self.root
    }

    pub(crate) fn is_inside_pub(&self, path: &Path) -> bool {
        self.is_inside(path)
    }

    pub(crate) fn pin_folder_pub(&self, id: LocalId) -> Result<(Handle, PathBuf)> {
        self.pin_folder(id)
    }

    pub(crate) fn checked_name_pub<'n>(&self, dir: LocalId, name: &'n str) -> Result<&'n str> {
        self.checked_name(dir, name)
    }

    pub(crate) fn is_synced_pub(&self, h: &Handle, pre: &LocalPre, synced: Option<&Hash>) -> Result<bool> {
        self.is_synced(h, pre, synced)
    }

    /// Copies the open file's content into a new version-store slot as an
    /// ordinary file (on-demand mode keeps a replaced version this way: the
    /// placeholder itself stays where it is).
    pub(crate) fn copy_to_store(&self, h: &Handle, from: &Path) -> Result<PathBuf> {
        use std::io::{Seek, SeekFrom};
        let slot = self.new_slot()?;
        let name = from.file_name().map_or_else(|| "item".into(), |n| n.to_string_lossy().into_owned());
        let kept = slot.join(&name);
        let src = h.as_file();
        let mut src: &File = &src;
        src.seek(SeekFrom::Start(0))?;
        let mut dst = OpenOptions::new().write(true).create_new(true).open(&kept)?;
        std::io::copy(&mut src, &mut dst)?;
        dst.sync_all()?;
        if let Ok(rel) = from.strip_prefix(&self.root) {
            let _ = std::fs::write(slot.join(ORIGIN), rel.to_string_lossy().as_bytes());
        }
        Ok(kept)
    }

    /// Whether the item with this ID still exists inside the sync folder
    /// (outside OneUpDrive's own folder). Read-only.
    pub fn is_present(&self, id: LocalId) -> bool {
        let Ok(h) = self.volume.open_attributes(FileId(id.0), self.hint(FileId(id.0)).as_deref()) else { return false };
        win::final_path(&h).is_ok_and(|p| self.is_inside(&long(&p)))
    }

    /// [`LocalFs::is_present`] for a decision that needs certainty either
    /// way: `Some(false)` only when the ID no longer exists or the item is
    /// outside the sync folder (or in OneUpDrive's own), `None` when it
    /// can't be told (access denied, a path that can't be read). Read-only.
    pub fn presence(&self, id: LocalId) -> Option<bool> {
        match self.volume.open_attributes(FileId(id.0), self.hint(FileId(id.0)).as_deref()) {
            Err(e) if e.is_not_found() => Some(false),
            Err(_) => None,
            Ok(h) => win::final_path(&h).ok().map(|p| self.is_inside(&long(&p))),
        }
    }

    /// The current name of the item with this ID, if it exists inside the
    /// sync folder (outside OneUpDrive's own folder). Read-only.
    pub fn name_of(&self, id: LocalId) -> Option<String> {
        let h = self.volume.open_attributes(FileId(id.0), self.hint(FileId(id.0)).as_deref()).ok()?;
        let p = long(&win::final_path(&h).ok()?);
        if !self.is_inside(&p) {
            return None;
        }
        p.file_name().map(|n| n.to_string_lossy().into_owned())
    }

    /// Runs an approved local operation other than a download (see
    /// [`LocalFs::stage`] / [`LocalFs::place_download`] for those). A
    /// recycled file is always kept here; see [`LocalFs::recycle`].
    pub fn apply(&self, a: &Approved) -> Result<Done> {
        match a.op() {
            Op::Hash(pre) => self.hash(pre),
            // On-demand: a folder placeholder instead (`place_placeholder`).
            Op::MkdirLocal { .. } if self.on_demand => Err(Error::WrongOp),
            Op::MkdirLocal { to, name, .. } => self.mkdir(*to, name),
            Op::MoveLocal { local, to, name, .. } => self.move_item(local, *to, name, false),
            Op::RenameLocal { local, dir, name } | Op::ConflictCopy { local, dir, name, .. } => {
                self.move_item(local, *dir, name, true)
            }
            Op::RecycleLocal { local, .. } => self.recycle_item(local, OldCopy::Keep, None),
            _ => Err(Error::WrongOp),
        }
    }

    /// Reads file `pre` and hashes it without stopping its program from
    /// saving it (watched, `win::WatchedFile`, never an exclusive handle): it's
    /// witnessed if it's still the planned stamp, at path `at` in the synced
    /// folder ("/A/B/name", by its handle's final path, as Windows compares
    /// names), and
    /// hashes to `hash` with no write during the read. The witness then
    /// stands until the file is written ([`Witnessed::unchanged`]). A
    /// placeholder is never read: with `vouched`, one whose identity vouches
    /// for `hash` is witnessed by that (its content is OneDrive's). `None`:
    /// not witnessed.
    pub fn witness(&self, pre: &LocalPre, at: &str, hash: &Hash, vouched: bool) -> Result<Option<Witnessed>> {
        let Some((size, _)) = pre.stamp else { return Err(Error::WrongOp) };
        if let Some((h, _, path)) = self.open_if_dehydrated(pre)? {
            let ok = vouched && self.identity_hash(&h, size) == Some(*hash) && self.is_at(&path, at);
            return Ok(ok.then_some(Witnessed { file: None }));
        }
        let mut opened = match self.open_for_read(pre) {
            Ok(o) => o,
            Err(Error::Changed | Error::NotOrdinary) => return Ok(None),
            Err(e) => return Err(e),
        };
        let Some(path) = opened.file.with_handle(win::final_path) else { return Ok(None) };
        if !self.is_at(&long(&path?), at) {
            return Ok(None);
        }
        let ordinary = opened.file.with_handle(|h| self.is_ordinary(h)).unwrap_or(false);
        if !ordinary {
            let vouches = opened.file.with_handle(|h| self.identity_hash(h, size)).flatten() == Some(*hash);
            let w = Witnessed { file: Some(opened.file) };
            return Ok((vouched && vouches && w.unchanged()).then_some(w));
        }
        let mut buf = vec![0u8; 1 << 20];
        let got = match oneup_hash::hash_reader(&mut opened.file, &mut buf) {
            Ok(got) => got,
            Err(_) if opened.file.broken() => return Ok(None),
            Err(e) => return Err(e.into()),
        };
        let same = i64::try_from(got.len()).ok() == Some(size) && got.finalize() == *hash;
        let w = Witnessed { file: Some(opened.file) };
        Ok((same && w.unchanged()).then_some(w))
    }

    /// Whether `path` (an item's final path) is `at` in the synced folder
    /// ("/A/B/name"), every name as Windows compares names: a folder above
    /// it moved or renamed makes it another path.
    fn is_at(&self, path: &Path, at: &str) -> bool {
        let mut want = self.root.clone();
        want.extend(at.split('/').filter(|p| !p.is_empty()));
        let fold = |p: &Path| oneup_sync::fold(&p.to_string_lossy());
        fold(path) == fold(&want)
    }

    /// Runs the part on this PC of an approved [`Op::RemoveCopy`], once its
    /// OneDrive copy (if it has one) is gone: the file goes into the version
    /// store, if it's still the planned stamp, at its planned path (its
    /// folders' names and its own, by the exclusive handle's final path) and, read
    /// through that handle, still the chosen content (`hash`): an edit that
    /// kept the size and restored the time, another file that took the ID,
    /// or the same file moved elsewhere, is left alone. A placeholder isn't
    /// read (a dehydrated one would be downloaded): only for a synced copy,
    /// whose OneDrive copy this same operation removed, and only if its
    /// identity vouches for the chosen content; a dehydrated one then has
    /// nothing to keep (OneDrive's recycle bin has its content) and is
    /// deleted once it's out of its place, and a partly downloaded one is
    /// left alone. `kept`: the watch on the copy that stays here, if it's
    /// here; checked once the copy has left its place ([`Self::unless_saved`]):
    /// saved since it was witnessed, the copy goes back and nothing is
    /// removed.
    pub fn remove_copy(&self, a: &Approved, kept: Option<&Witnessed>) -> Result<Done> {
        let Op::RemoveCopy { local: Some(pre), local_dir: Some(_), local_up, remote, name, hash, .. } = a.op() else {
            return Err(Error::WrongOp);
        };
        let Some((size, _)) = pre.stamp else { return Err(Error::WrongOp) };
        let (h, entry, path) = self.open_item(pre)?;
        if entry.is_dir() {
            return Err(Error::WrongOp);
        }
        if !self.is_at(&path, &local_up.path(name)) {
            return Err(Error::Changed);
        }
        // From here on the path checked is the copy's until it leaves it:
        // while it's open, no folder above it can be moved or renamed
        // (`a_folder_above_a_copy_being_removed_cant_move`).
        #[cfg(feature = "crash-test")]
        if let Some(f) = self.removing.0.lock().unwrap_or_else(std::sync::PoisonError::into_inner).clone() {
            f(&path);
        }
        if !self.is_ordinary(&h) {
            if remote.is_none() || !self.on_demand || self.identity_hash(&h, size) != Some(*hash) {
                return Err(Error::NotOrdinary);
            }
            if self.is_dehydrated(&h) {
                // Under the hold no fetch can bring it content. It leaves its
                // place the same way, so the survivor's check comes after,
                // and is deleted from the version store only then.
                let _hold = self.hold_dehydrated(&h).ok_or(Error::Changed)?;
                let slot = self.keep(&h, &entry, &path)?;
                self.unless_saved(kept, &h, &slot, &path)?;
                win::delete_handle(&h)?;
                drop(h);
                self.drop_slot(&slot);
                return Ok(Done::Recycled { kept: None });
            }
            if self.is_partial(&h, size) {
                return Err(Error::NotOrdinary);
            }
            let slot = self.keep(&h, &entry, &path)?;
            self.unless_saved(kept, &h, &slot, &path)?;
            drop(h);
            self.revert_kept(&slot, size);
            return Ok(Done::Recycled { kept: Some(slot) });
        }
        let mut buf = vec![0u8; 1 << 20];
        let got = oneup_hash::hash_reader(&*h.as_file(), &mut buf)?;
        if i64::try_from(got.len()).ok() != Some(size) || got.finalize() != *hash {
            tracing::info!("a duplicate chosen for removal has other content now, so it was left alone");
            return Err(Error::Changed);
        }
        let slot = self.keep(&h, &entry, &path)?;
        self.unless_saved(kept, &h, &slot, &path)?;
        Ok(Done::Recycled { kept: Some(slot) })
    }

    /// A removal's last check, once the copy (open as `h`) has left its
    /// place `from` for the version store (`slot`): if the copy that stays
    /// here (`kept`) was saved since it was witnessed, the copy goes back
    /// where it was, through the same handle and never over anything, and
    /// nothing is removed ([`Error::Changed`]). So no save of the survivor
    /// goes unseen while the copy is still in its place: one after this
    /// check comes after the removal. Couldn't it go back (another file took
    /// its name in that moment), it stays in the version store.
    fn unless_saved(&self, kept: Option<&Witnessed>, h: &Handle, slot: &Path, from: &Path) -> Result<()> {
        if kept.is_none_or(Witnessed::unchanged) {
            return Ok(());
        }
        match win::rename_handle(h, from) {
            Ok(()) => {
                self.drop_slot(slot);
                tracing::info!(
                    "the copy of a duplicate that stays was saved meanwhile, so the chosen copy was left alone"
                );
                Err(Error::Changed)
            }
            Err(e) => {
                tracing::warn!(error = %e, "the copy of a duplicate that stays was saved meanwhile; the chosen copy stays in the version store");
                Ok(())
            }
        }
    }

    /// Runs an approved [`Op::RecycleLocal`]. `synced`: the hash of the last
    /// synced version of the file. If the token says
    /// [`OldCopy::DropIfSynced`] and the file still hashes to it, the file is
    /// deleted without a local copy (OneDrive's recycle bin has it);
    /// otherwise it goes into the version store.
    pub fn recycle(&self, a: &Approved, synced: Option<&Hash>) -> Result<Done> {
        match a.op() {
            Op::RecycleLocal { local, .. } => self.recycle_item(local, a.old_copy(), synced),
            // A rewind sets a copy changed here aside: it's never the synced
            // version, so it's always kept.
            Op::Rewind { step, .. } => match &**step {
                Step::SetAside { local } | Step::Discard { local } => self.recycle_item(local, OldCopy::Keep, None),
                _ => Err(Error::WrongOp),
            },
            _ => Err(Error::WrongOp),
        }
    }

    /// A new, empty staging file for a download.
    pub fn stage(&self) -> Result<Staged> {
        let name = format!("{}-{}.{PART_EXT}", now_ms(), self.seq.fetch_add(1, Relaxed));
        let path = self.engine.join(STAGING).join(name);
        let file = OpenOptions::new().read(true).write(true).create_new(true).open(&path)?;
        Ok(Staged { path, file: Some(file) })
    }

    /// Puts a fully written and verified download in place for an approved
    /// [`Op::Download`]. A new name must be free. A replaced file must still
    /// be exactly the planned stamp, in the planned folder under the planned
    /// name; its content is moved into the version store first, and moved
    /// back if placing the new file fails. Once the new file is in place,
    /// the old copy is deleted if the token says [`OldCopy::DropIfSynced`]
    /// and it hashed to `synced` (the last synced version's hash, which
    /// OneDrive keeps in its version history); otherwise it stays in the
    /// store.
    pub fn place_download(&self, a: &Approved, mut staged: Staged, synced: Option<&Hash>) -> Result<Done> {
        let Op::Download { to, name, replace, .. } = a.op() else {
            return Err(Error::WrongOp);
        };
        // On-demand: content never arrives by file writes (`data-safety.md`
        // invariant 12); placeholders do (`place_placeholder`).
        if self.on_demand {
            return Err(Error::WrongOp);
        }
        if let Some(f) = staged.file.take() {
            f.sync_all()?;
        }
        let new = win::open_path_for_change(&staged.path, false)?;
        // Our write, not a program's (`oneup_cfapi::ours`).
        oneup_cfapi::ours::note(win::file_id(&new)?.1.0);
        let (_pin, dir) = self.pin_folder(*to)?;
        let kept = match replace {
            None => {
                let target = dir.join(self.checked_name(*to, name)?);
                win::rename_handle(&new, &target)?;
                None
            }
            Some(pre) => {
                if pre.stamp.is_none() {
                    return Err(Error::WrongOp);
                }
                let (old, old_entry, old_path) = self.open_item(pre)?;
                // It must be where the plan expected it (names compare
                // case-insensitively, like Windows).
                if old_path.parent() != Some(dir.as_path())
                    || oneup_sync::fold(&old_entry.name) != oneup_sync::fold(name)
                {
                    return Err(Error::Changed);
                }
                // Checked under the exclusive handle, which keeps SQLite
                // from opening the database (and so from writing beside
                // it) until the replacement is done.
                if self.pending_beside(&old, &old_path, old_entry.size)? {
                    return Err(Error::DatabaseWaits);
                }
                // Decided before anything moves, under the exclusive handle,
                // so the content checked is the content dropped.
                let drop_old = a.old_copy() == OldCopy::DropIfSynced && self.is_synced(&old, pre, synced)?;
                let kept = self.keep(&old, &old_entry, &old_path)?;
                // Killed here, the old version is in the version store and
                // the new one in staging, which the next start clears: the
                // name is free until the next round downloads it again.
                self.crash_point()?;
                if let Err(e) = win::rename_handle(&new, &old_path) {
                    return match win::rename_handle(&old, &old_path) {
                        Ok(()) => {
                            self.drop_slot(&kept);
                            Err(e.into())
                        }
                        Err(_) => {
                            tracing::warn!(
                                "a download could not replace a file, and the old version stayed in the version store"
                            );
                            Err(Error::KeptAside { kept })
                        }
                    };
                }
                // Killed here, both versions are kept: the new one in place.
                self.crash_point()?;
                if drop_old { self.discard_kept(old, &kept)? } else { Some(kept) }
            }
        };
        // Placed: the staging path no longer holds anything of ours.
        staged.path = PathBuf::new();
        let id = win::file_id(&new)?.1;
        let entry = win::stat_handle(&new, id)?.ok_or(Error::Changed)?;
        Ok(Done::Placed { parent: *to, entry, kept })
    }

    /// Opens the local file of an approved [`Op::Upload`] for reading, after
    /// checking it is still the planned stamp.
    pub fn open_upload(&self, a: &Approved) -> Result<UploadSource> {
        let Op::Upload { local, name, .. } = a.op() else {
            return Err(Error::WrongOp);
        };
        let opened = match self.open_for_read(local) {
            // SQLite deletes a rollback journal at the end of every
            // transaction, and a file being deleted refuses to be opened
            // (access denied): it changed.
            Err(Error::Win(_)) if oneup_rules::database::Companion::of_name(name).is_some() => {
                return Err(Error::Changed);
            }
            r => r?,
        };
        // Reading a placeholder whose content isn't all here would ask the
        // cloud (ourselves) for it.
        if self.on_demand && !self.on_disk(&opened) {
            return Err(Error::NotOrdinary);
        }
        Ok(opened.into_source())
    }

    /// The first `max` bytes of a file planned for upload, for the content
    /// guard (`oneup_gate::content`). Read-only, with the same checks as
    /// [`LocalFs::open_upload`]; `None` if it can't be read now (changed,
    /// open elsewhere, or its content isn't all here).
    pub fn sample(&self, pre: &LocalPre, max: usize) -> Option<Vec<u8>> {
        use std::io::Read;
        let opened = self.open_for_read(pre).ok()?;
        if self.on_demand && !self.on_disk(&opened) {
            return None;
        }
        let mut head = Vec::with_capacity(max);
        opened.into_source().take(max as u64).read_to_end(&mut head).ok()?;
        Some(head)
    }

    // ------------------------------------------------------------ effects

    fn hash(&self, pre: &LocalPre) -> Result<Done> {
        if let Some((h, _, _)) = self.open_if_dehydrated(pre)?
            && let Some((size, mtime)) = pre.stamp
        {
            // Its identity is its content's hash; never opened for data.
            let hash = self.identity_hash(&h, size).ok_or(Error::NotOrdinary)?;
            return Ok(Done::Hashed { hash, size, mtime });
        }
        let opened = self.open_for_read(pre)?;
        let (size, mtime) = (opened.size, opened.mtime);
        if self.on_demand {
            // A placeholder whose identity vouches for its content is never
            // read (a dehydrated one would be downloaded from ourselves).
            if let Some(hash) = opened.file.with_handle(|h| self.identity_hash(h, size)).flatten() {
                return Ok(Done::Hashed { hash, size, mtime });
            }
            if !self.on_disk(&opened) {
                return Err(Error::NotOrdinary);
            }
        }
        let mut buf = vec![0u8; 1 << 20];
        let h = oneup_hash::hash_reader(opened.into_source(), &mut buf)
            .map_err(|e| if ChangedMeanwhile::is(&e) { Error::Changed } else { e.into() })?;
        if i64::try_from(h.len()).ok() != Some(size) {
            return Err(Error::Changed);
        }
        Ok(Done::Hashed { hash: h.finalize(), size, mtime })
    }

    fn mkdir(&self, to: LocalId, name: &str) -> Result<Done> {
        let (_pin, dir) = self.pin_folder(to)?;
        let target = dir.join(self.checked_name(to, name)?);
        match std::fs::create_dir(&target) {
            Ok(()) => {}
            Err(e) if e.kind() == std::io::ErrorKind::AlreadyExists => return Err(Error::NameTaken),
            Err(e) => return Err(e.into()),
        }
        let h = win::open_dir(&target)?;
        let id = win::file_id(&h)?.1;
        let entry = win::stat_handle(&h, id)?.ok_or(Error::Changed)?;
        Ok(Done::Placed { parent: to, entry, kept: None })
    }

    /// Moves/renames `pre` to `to`\`name`. `in_place`: it must currently be
    /// in `to` (a rename within its folder).
    fn move_item(&self, pre: &LocalPre, to: LocalId, name: &str, in_place: bool) -> Result<Done> {
        let (_pin, dir) = self.pin_folder(to)?;
        let target = dir.join(self.checked_name(to, name)?);
        let (h, entry, path) = self.open_item(pre)?;
        if in_place && path.parent() != Some(dir.as_path()) {
            return Err(Error::Changed);
        }
        // Databases and their working files, judged on the actual names
        // right before the move (`database::check_move`).
        if !entry.is_dir() {
            self.check_move(&h, &path, entry.size, Some(&target))?;
        }
        let was_in_sync = self.on_demand && self.is_in_sync(&h);
        win::rename_handle(&h, &target)?;
        if was_in_sync {
            self.resync_moved(&h, &target);
        }
        let entry = win::stat_handle(&h, FileId(pre.id.0))?.ok_or(Error::Changed)?;
        Ok(Done::Placed { parent: to, entry, kept: None })
    }

    fn recycle_item(&self, pre: &LocalPre, old: OldCopy, synced: Option<&Hash>) -> Result<Done> {
        let (h, entry, path) = self.open_item(pre)?;
        // As for a move (`database::check_move`).
        if !entry.is_dir() {
            self.check_move(&h, &path, entry.size, None)?;
        }
        if entry.is_dir() {
            // Children can still be added after this check (the folder is
            // shared for writing); they would then move along into the
            // version store, which loses nothing.
            let mut buf = vec![0u8; 4096];
            let mut kids = Vec::new();
            win::list_dir(&h, &mut buf, &mut kids)?;
            if !kids.is_empty() {
                return Err(Error::NotEmpty);
            }
        }
        if self.on_demand && !entry.is_dir() && self.is_dehydrated(&h) {
            // No content here to keep: it's the cloud version, now in
            // OneDrive's recycle bin. Held, so its content can't arrive
            // (and be edited) between the check and the removal.
            let _hold = self.hold_dehydrated(&h).ok_or(Error::Changed)?;
            win::delete_handle(&h)?;
            tracing::debug!("removed a dehydrated placeholder (no local content)");
            return Ok(Done::Recycled { kept: None });
        }
        if self.on_demand && !entry.is_dir() && pre.stamp.is_some_and(|(size, _)| self.is_partial(&h, size)) {
            // Partly here (a download cut short): only bytes of the cloud
            // version, now in OneDrive's recycle bin, and reading the rest
            // would ask for that removed version. Proved and removed under
            // a share-none handle, never read.
            let (x, hold) = self.open_partial(h, pre, &path)?;
            win::delete_handle(&x)?;
            drop(x);
            drop(hold);
            tracing::debug!("removed a partly downloaded placeholder (no local content)");
            return Ok(Done::Recycled { kept: None });
        }
        if old == OldCopy::DropIfSynced && !entry.is_dir() && self.is_synced(&h, pre, synced)? {
            win::delete_handle(&h)?;
            tracing::debug!("removed without a local copy (OneDrive has this version)");
            return Ok(Done::Recycled { kept: None });
        }
        let kept = self.keep(&h, &entry, &path)?;
        if self.on_demand && !entry.is_dir() {
            drop(h);
            self.revert_kept(&kept, i64::try_from(entry.size).unwrap_or(i64::MAX));
        }
        Ok(Done::Recycled { kept: Some(kept) })
    }

    /// Whether the open file is provably the last synced version: its
    /// content, read through the caller's exclusive handle (so nobody can
    /// change it meanwhile), has the planned size and hashes to `synced`.
    /// Without a recorded hash, it can't be proved.
    fn is_synced(&self, h: &Handle, pre: &LocalPre, synced: Option<&Hash>) -> Result<bool> {
        let (Some(want), Some((size, _))) = (synced, pre.stamp) else { return Ok(false) };
        let mut buf = vec![0u8; 1 << 20];
        let got = oneup_hash::hash_reader(&*h.as_file(), &mut buf)?;
        let same = i64::try_from(got.len()).ok() == Some(size) && got.finalize() == *want;
        if !same {
            tracing::warn!("a file differs from its last synced version, so its old copy is kept");
        }
        Ok(same)
    }

    /// Deletes an old copy just moved into the version store, through the
    /// handle it was checked on, and removes its slot. If that fails, it
    /// simply stays in the store (`Some`). Errs only for an injected crash.
    fn discard_kept(&self, h: Handle, kept: &Path) -> Result<Option<PathBuf>> {
        if let Err(e) = win::delete_handle(&h) {
            tracing::warn!(error = %e, "an old copy couldn't be removed; it stays in the version store");
            return Ok(Some(kept.to_path_buf()));
        }
        // The deletion completes when the handle closes.
        drop(h);
        // Killed here, the slot holds only its origin, which the store's
        // listing skips.
        self.crash_point()?;
        self.drop_slot(kept);
        Ok(None)
    }

    // ------------------------------------------------------- version store

    /// Everything in the version store, oldest first. Changes only our own
    /// [`VERSIONS_CACHE`]: each round that keeps or trims anything lists the
    /// store, and measuring every kept folder again each time would cost
    /// what the store holds, not what changed.
    pub fn versions(&self) -> Result<Vec<Slot>> {
        use std::os::windows::fs::MetadataExt as _;
        let cache = self.engine.join(VERSIONS_CACHE);
        let cached = read_versions_cache(&cache);
        let (mut out, mut keep, mut measured) = (Vec::new(), Vec::new(), false);
        for e in std::fs::read_dir(self.engine.join(VERSIONS))?.flatten() {
            let Some(id) = e.file_name().to_str().map(str::to_string) else { continue };
            let Some(at_ms) = id.split_once('-').and_then(|(ms, _)| ms.parse::<i64>().ok()) else { continue };
            if !e.file_type().is_ok_and(|t| t.is_dir()) {
                continue;
            }
            // From the listing itself: no open per slot.
            let Ok(created) = e.metadata().map(|m| m.creation_time()) else { continue };
            if let Some((c, s)) = cached.get(&id)
                && *c == created
            {
                keep.push((created, s.clone()));
                out.push(s.clone());
                continue;
            }
            let Some(slot) = measure_slot(&e.path(), id, at_ms)? else { continue };
            // A slot gets its origin last; one without it may still be
            // filling, so it's measured again next time.
            if slot.origin.is_some() {
                keep.push((created, slot.clone()));
                measured = true;
            }
            out.push(slot);
        }
        if measured || keep.len() != cached.len() {
            self.write_versions_cache(&cache, &keep);
        }
        out.sort_by(|a, b| (a.at_ms, &a.id).cmp(&(b.at_ms, &b.id)));
        Ok(out)
    }

    /// Replaces [`VERSIONS_CACHE`] atomically (a unique temp file, flushed,
    /// renamed over it). Best effort: without it, the store is measured.
    fn write_versions_cache(&self, path: &Path, slots: &[(u64, Slot)]) {
        let mut text = String::new();
        for (created, s) in slots {
            let origin = s.origin.as_deref().map(|o| format!("\t{o}")).unwrap_or_default();
            let line = format!("{}\t{created}\t{}\t{}\t{}{origin}\n", s.id, s.bytes, u8::from(s.is_dir), s.name);
            text.push_str(&line);
        }
        let tmp = path.with_extension(format!("{}-{}.tmp", std::process::id(), self.seq.fetch_add(1, Relaxed)));
        let written = (|| {
            let mut f = OpenOptions::new().write(true).create_new(true).open(&tmp)?;
            f.write_all(text.as_bytes())?;
            f.sync_all()?;
            drop(f);
            std::fs::rename(&tmp, path)
        })();
        if let Err(e) = written {
            let _ = std::fs::remove_file(&tmp);
            tracing::debug!(error = %e, "the version store's size cache wasn't written");
        }
    }

    /// What putting kept item `slot_id` back would move, and where: to `to`
    /// (a path relative to the sync folder, `\\` or `/` separated) or else
    /// where it came from. The kept item by its file ID and stamp (size and
    /// last-write time; none for a folder) and the destination folder by its
    /// file ID, for a [`Step::RestoreKept`] that acts on exactly these. The
    /// folder must exist.
    pub fn kept_target(&self, slot_id: &str, to: Option<&str>) -> Result<crate::KeptTarget> {
        if !is_valid_name(slot_id, false) {
            return Err(Error::BadName);
        }
        let slot = self.engine.join(VERSIONS).join(slot_id);
        let item = std::fs::read_dir(&slot)
            .map_err(|_| Error::Changed)?
            .flatten()
            .find(|i| i.file_name() != ORIGIN)
            .ok_or(Error::Changed)?;
        let rel = match to {
            Some(t) => t.to_string(),
            None => std::fs::read_to_string(slot.join(ORIGIN)).map_err(|_| Error::Outside)?.trim().to_string(),
        };
        let parts: Vec<&str> = rel.split(['\\', '/']).filter(|p| !p.is_empty()).collect();
        let Some((name, folders)) = parts.split_last() else { return Err(Error::BadName) };
        for (i, p) in parts.iter().enumerate() {
            if !is_valid_name(p, i == 0) {
                return Err(Error::BadName);
            }
        }
        let mut dir = self.root.clone();
        for f in folders {
            dir.push(f);
        }
        let probe = win::open_dir(&dir).map_err(|_| Error::Changed)?;
        let (_, dir_id) = win::file_id(&probe)?;
        drop(probe);
        // An ordinary folder in the sync folder.
        drop(self.pin_folder(LocalId(dir_id.0))?);
        let is_dir = item.file_type().is_ok_and(|t| t.is_dir());
        let h = win::open_path_for_change(&item.path(), is_dir)?;
        let (_, id) = win::file_id(&h)?;
        let entry = win::stat_handle(&h, id)?.ok_or(Error::Changed)?;
        let stamp = if is_dir { None } else { Some((i64::try_from(entry.size).unwrap_or(i64::MAX), entry.modified)) };
        check_placeholder(&entry, stamp, false)?;
        Ok(crate::KeptTarget {
            item: LocalPre { id: LocalId(id.0), stamp },
            dir: LocalId(dir_id.0),
            name: (*name).to_string(),
            rel: parts.join("\\"),
        })
    }

    /// Runs an approved [`Step::RestoreKept`]: the kept item, opened by its
    /// file ID and checked on that handle (in its slot, its size and
    /// last-write time as chosen), moves into the destination folder, pinned
    /// by its file ID. Never replaces anything: if the name is taken, it's
    /// restored as `name (restored N).ext` beside it. Returns the path it
    /// was restored to, relative to the sync folder. The next sync uploads
    /// it like any new file.
    pub fn restore_approved(&self, a: &Approved) -> Result<String> {
        let Op::Rewind { step, .. } = a.op() else { return Err(Error::WrongOp) };
        let Step::RestoreKept { slot, item, dir, name } = &**step else { return Err(Error::WrongOp) };
        if !is_valid_name(slot, false) {
            return Err(Error::BadName);
        }
        let is_dir = item.stamp.is_none();
        let slot_dir = self.engine.join(VERSIONS).join(slot);
        let id = FileId(item.id.0);
        let h = self.volume.open_for_change(id, is_dir, None).map_err(|_| Error::Changed)?;
        let entry = win::stat_handle(&h, id)?.ok_or(Error::Changed)?;
        check_placeholder(&entry, item.stamp, false)?;
        let at = long(&win::final_path(&h)?);
        if at.parent() != Some(slot_dir.as_path()) {
            return Err(Error::Changed);
        }
        let name = self.checked_name(*dir, name)?;
        // The destination pinned so it can't be swapped while we move.
        let (_pin, dir_path) = self.pin_folder(*dir)?;
        let rel_dir = dir_path.strip_prefix(&self.root).map(Path::to_path_buf).unwrap_or_default();
        for n in 0..100 {
            let candidate = if n == 0 { name.to_string() } else { restored_name(name, n, is_dir) };
            match win::rename_handle(&h, &dir_path.join(&candidate)) {
                Ok(()) => {
                    drop(h);
                    self.drop_slot(&at);
                    return Ok(rel_dir.join(&candidate).to_string_lossy().into_owned());
                }
                Err(e) if e.is_exists() => continue,
                Err(e) => return Err(e.into()),
            }
        }
        Err(Error::NameTaken)
    }

    /// Permanently removes version-store slots: those kept before
    /// `older_than_ms` (Unix ms), then, oldest first, as many as needed to
    /// bring the store under `max_bytes`. With `dry_run`, only reports.
    /// Returns what was (or would be) removed.
    ///
    /// Only whole slots inside OneUpDrive's own `.oneup\versions` folder are
    /// touched. `remove_dir_all` doesn't follow links, so nothing outside the
    /// store can be reached through one.
    pub fn prune_versions(
        &self,
        older_than_ms: Option<i64>,
        max_bytes: Option<u64>,
        dry_run: bool,
    ) -> Result<Vec<Slot>> {
        let slots = self.versions()?;
        let mut total: u64 = slots.iter().map(|s| s.bytes).sum();
        let mut out = Vec::new();
        for s in slots {
            let too_old = older_than_ms.is_some_and(|t| s.at_ms < t);
            let too_big = max_bytes.is_some_and(|m| total > m);
            if !(too_old || too_big) {
                continue;
            }
            total = total.saturating_sub(s.bytes);
            if !dry_run && !remove_slot(&self.engine.join(VERSIONS).join(&s.id))? {
                continue;
            }
            out.push(s);
        }
        Ok(out)
    }

    /// How many bytes the version store can take now (see [`Trim::room`]).
    /// Lists the store, so callers ask only when a round would add to it.
    pub fn room(&self, rule: &Trim) -> Result<Option<u64>> {
        let slots = self.versions()?;
        Ok(rule.room(&slots, now_ms() as i64, self.disk_bytes()?))
    }

    /// With rotation on, removes the oldest items, whatever their age, so
    /// that `need` more bytes fit (see [`Trim::make_room`]). Returns what was
    /// removed.
    pub fn make_room(&self, rule: &Trim, need: u64) -> Result<Vec<Slot>> {
        if !rule.rotate || need == 0 {
            return Ok(Vec::new());
        }
        let slots = self.versions()?;
        let n = rule.make_room(&slots, need, self.disk_bytes()?);
        self.remove_slots(slots.into_iter().take(n))
    }

    fn disk_bytes(&self) -> Result<u64> {
        oneup_sys::disk::total_bytes(&self.root).map_err(|e| Error::Io(std::io::Error::other(e)))
    }

    /// Removes whole slots of our own `.oneup\versions` folder
    /// (`remove_dir_all` doesn't follow links).
    fn remove_slots(&self, slots: impl Iterator<Item = Slot>) -> Result<Vec<Slot>> {
        let mut removed = Vec::new();
        for s in slots {
            if remove_slot(&self.engine.join(VERSIONS).join(&s.id))? {
                removed.push(s);
            }
        }
        Ok(removed)
    }

    /// Applies the automatic trimming rule ([`Trim`]) and returns what was
    /// removed. Run after a sync, never during one.
    pub fn trim_versions(&self, rule: &Trim) -> Result<Vec<Slot>> {
        let slots = self.versions()?;
        let n = rule.select(&slots, now_ms() as i64, self.disk_bytes()?);
        self.remove_slots(slots.into_iter().take(n))
    }

    // ------------------------------------------------------------ helpers

    /// Opens a planned item for change and checks it against the plan.
    fn open_item(&self, pre: &LocalPre) -> Result<(Handle, DirEntry, PathBuf)> {
        let id = FileId(pre.id.0);
        if id == self.root_id {
            return Err(Error::Outside);
        }
        if let Some((h, entry, path)) = self.open_if_dehydrated(pre)? {
            return Ok((h, entry, path));
        }
        let h = self.volume.open_for_change(id, pre.stamp.is_none(), self.hint(id).as_deref())?;
        let entry = win::stat_handle(&h, id)?.ok_or(Error::Changed)?;
        check_placeholder(&entry, pre.stamp, self.on_demand)?;
        let path = long(&win::final_path(&h)?);
        if !self.is_inside(&path) {
            return Err(Error::Outside);
        }
        Ok((h, entry, path))
    }

    /// Whether an opened file's content may be read: all of it on disk, and
    /// an ordinary file or one of OneUpDrive's own placeholders (its
    /// identity decodes as ours, `LocalFs::readable_here`), asked through
    /// its own handle. Every read of user content in on-demand mode (an
    /// upload, a hash, a sample) goes through this.
    fn on_disk(&self, opened: &OpenedToRead) -> bool {
        opened.file.with_handle(|h| self.readable_here(h, opened.size)).unwrap_or(false)
    }

    fn open_for_read(&self, pre: &LocalPre) -> Result<OpenedToRead> {
        let Some(stamp) = pre.stamp else {
            return Err(Error::WrongOp);
        };
        let id = FileId(pre.id.0);
        if self.dehydrated_path(id)?.is_some() {
            // Opening it with data access would download it from ourselves.
            return Err(Error::NotOrdinary);
        }
        let (file, path) = self.volume.open_watched(id, self.hint(id).as_deref())?;
        let entry = file.with_handle(|h| win::stat_handle(h, id)).ok_or(Error::Changed)??.ok_or(Error::Changed)?;
        check_placeholder(&entry, Some(stamp), self.on_demand)?;
        if !self.is_inside(&path) {
            return Err(Error::Outside);
        }
        Ok(OpenedToRead { file, size: stamp.0, mtime: stamp.1, path })
    }

    /// Opens destination folder `id`, pinned in place for as long as the
    /// returned handle lives, and returns its path. It must be an ordinary
    /// folder in the sync folder (or the sync folder itself).
    fn pin_folder(&self, id: LocalId) -> Result<(Handle, PathBuf)> {
        let id = FileId(id.0);
        let h = self.volume.open_dir_pinned(id)?;
        let (attributes, tag) = win::attribute_tag(&h)?;
        if attributes & oneup_ntfs::dirent::FILE_ATTRIBUTE_DIRECTORY == 0 {
            return Err(Error::Changed);
        }
        if attributes & NOT_ORDINARY != 0 && !(self.on_demand && is_cloud_tag(tag)) {
            return Err(Error::NotOrdinary);
        }
        let path = long(&win::final_path(&h)?);
        if id != self.root_id && !self.is_inside(&path) {
            return Err(Error::Outside);
        }
        if id == self.root_id && path != self.root {
            return Err(Error::RootChanged("the sync folder moved"));
        }
        Ok((h, path))
    }

    fn checked_name<'n>(&self, dir: LocalId, name: &'n str) -> Result<&'n str> {
        if is_valid_name(name, FileId(dir.0) == self.root_id) { Ok(name) } else { Err(Error::BadName) }
    }

    /// Strictly below the sync folder, and not in OneUpDrive's own folder.
    fn is_inside(&self, path: &Path) -> bool {
        match path.strip_prefix(&self.root) {
            Ok(rest) => match rest.components().next() {
                Some(Component::Normal(first)) => !first.to_str().is_some_and(oneup_rules::is_engine_dir),
                _ => false,
            },
            Err(_) => false,
        }
    }

    /// Moves the open item into a new version-store slot, keeping its name,
    /// and returns its new path.
    fn keep(&self, h: &Handle, entry: &DirEntry, from: &Path) -> Result<PathBuf> {
        let slot = self.new_slot()?;
        // Killed here, an empty slot is left, which the store's listing
        // skips; the item is where it was.
        self.crash_point()?;
        let name = if entry.name_lossy || entry.name.is_empty() { "item" } else { entry.name.as_str() };
        let kept = slot.join(name);
        if let Err(e) = win::rename_handle(h, &kept) {
            let _ = std::fs::remove_dir(&slot);
            return Err(e.into());
        }
        // Killed here, the item is kept without its origin.
        self.crash_point()?;
        // Best effort: where it came from, for `oneup restore`. The item is
        // safe either way.
        if let Ok(rel) = from.strip_prefix(&self.root) {
            let _ = std::fs::write(slot.join(ORIGIN), rel.to_string_lossy().as_bytes());
        }
        tracing::debug!(kept = %kept.display(), "kept in the version store");
        Ok(kept)
    }

    /// A fresh, empty folder under `versions`, named so that slots sort by
    /// time.
    fn new_slot(&self) -> Result<PathBuf> {
        let versions = self.engine.join(VERSIONS);
        loop {
            let slot = versions.join(format!("{}-{}", now_ms(), self.seq.fetch_add(1, Relaxed)));
            match std::fs::create_dir(&slot) {
                Ok(()) => return Ok(slot),
                Err(e) if e.kind() == std::io::ErrorKind::AlreadyExists => continue,
                Err(e) => return Err(e.into()),
            }
        }
    }

    /// Removes a version slot whose item was moved back out. Only our own
    /// bookkeeping file can be in it; `remove_dir` refuses non-empty folders.
    fn drop_slot(&self, kept: &Path) {
        if let Some(slot) = kept.parent() {
            let _ = std::fs::remove_file(slot.join(ORIGIN));
            let _ = std::fs::remove_dir(slot);
        }
    }

    /// Removes partial downloads of an interrupted run: only files with our
    /// own extension, only in our staging folder.
    fn clear_staging(&self) {
        let Ok(entries) = std::fs::read_dir(self.engine.join(STAGING)) else {
            return;
        };
        for e in entries.flatten() {
            let p = e.path();
            if p.extension().is_some_and(|x| x == PART_EXT) && e.file_type().is_ok_and(|t| t.is_file()) {
                let _ = std::fs::remove_file(&p);
            }
        }
    }
}

/// Checks an open item against its planned stamp: `None` for a folder,
/// `(size, last-write time)` for a file. `ours`: in on-demand mode, our own
/// cloud placeholders count as ordinary items.
pub(crate) fn check_placeholder(entry: &DirEntry, stamp: Option<(i64, i64)>, ours: bool) -> Result<()> {
    if entry.attributes & NOT_ORDINARY != 0 && !(ours && is_cloud_tag(entry.reparse_tag)) {
        return Err(Error::NotOrdinary);
    }
    let same = match stamp {
        None => entry.is_dir(),
        Some((size, mtime)) => {
            !entry.is_dir() && i64::try_from(entry.size).ok() == Some(size) && entry.modified == mtime
        }
    };
    if same { Ok(()) } else { Err(Error::Changed) }
}

/// A Cloud Files reparse tag (`IO_REPARSE_TAG_CLOUD` and its variants).
pub(crate) fn is_cloud_tag(tag: u32) -> bool {
    tag & 0xFFFF_0FFF == 0x9000_001A
}

/// Creates our own folder if missing. An existing one must be a real
/// folder: a link could send the version store (and so user content)
/// somewhere else.
fn ensure_dir(p: &Path) -> Result<()> {
    match std::fs::symlink_metadata(p) {
        Ok(m) => {
            use std::os::windows::fs::MetadataExt as _;
            if m.is_dir() && m.file_attributes() & FILE_ATTRIBUTE_REPARSE_POINT == 0 {
                Ok(())
            } else {
                Err(Error::RootChanged("OneUpDrive's folder in the sync folder is not an ordinary folder"))
            }
        }
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(std::fs::create_dir(p)?),
        Err(e) => Err(e.into()),
    }
}

/// `final_path` strips the `\\?\` prefix; put it back so paths longer than
/// 260 characters keep working.
pub(crate) fn long(p: &Path) -> PathBuf {
    let mut s = OsString::from(r"\\?\");
    s.push(p.as_os_str());
    PathBuf::from(s)
}

/// `report (restored 2).docx`; folders get no extension split.
fn restored_name(name: &str, n: u32, is_dir: bool) -> String {
    match name.rfind('.').filter(|&i| i > 0 && !is_dir) {
        Some(i) => format!("{} (restored {n}){}", &name[..i], &name[i..]),
        None => format!("{name} (restored {n})"),
    }
}

/// Removes a whole version slot (`remove_dir_all` doesn't follow links).
/// `false`: it was gone already (another `oneup` removed or restored it).
fn remove_slot(path: &Path) -> Result<bool> {
    match std::fs::remove_dir_all(path) {
        Ok(()) => Ok(true),
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(false),
        Err(e) => Err(e.into()),
    }
}

/// The version slot at `path` (`None`: empty, or gone meanwhile, as when
/// `oneup restore` or a trim from the command line removed it).
fn measure_slot(path: &Path, id: String, at_ms: i64) -> Result<Option<Slot>> {
    let items = match std::fs::read_dir(path) {
        Ok(items) => items,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(None),
        Err(e) => return Err(e.into()),
    };
    let origin = std::fs::read_to_string(path.join(ORIGIN)).ok().map(|s| s.trim().to_string());
    let Some(item) = items.flatten().find(|i| i.file_name() != ORIGIN) else { return Ok(None) };
    Ok(Some(Slot {
        id,
        at_ms,
        name: item.file_name().to_string_lossy().into_owned(),
        origin,
        is_dir: item.file_type().is_ok_and(|t| t.is_dir()),
        bytes: size_of(&item.path()),
    }))
}

/// What [`VERSIONS_CACHE`] says, by slot ID: the slot folder's creation
/// time and what was measured. Missing or unreadable lines are left out
/// (measured again).
fn read_versions_cache(path: &Path) -> std::collections::HashMap<String, (u64, Slot)> {
    let Ok(text) = std::fs::read_to_string(path) else { return Default::default() };
    text.lines()
        .filter_map(|line| {
            let mut f = line.split('\t');
            let id = f.next()?.to_string();
            let created = f.next()?.parse().ok()?;
            let bytes = f.next()?.parse().ok()?;
            let is_dir = match f.next()? {
                "0" => false,
                "1" => true,
                _ => return None,
            };
            let name = f.next()?.to_string();
            let origin = f.next().map(str::to_string);
            let at_ms = id.split_once('-').and_then(|(ms, _)| ms.parse().ok())?;
            Some((id.clone(), (created, Slot { id, at_ms, name, origin, is_dir, bytes })))
        })
        .collect()
}

/// Bytes in a file or folder tree, without following links.
fn size_of(p: &Path) -> u64 {
    let Ok(m) = std::fs::symlink_metadata(p) else { return 0 };
    if !m.is_dir() {
        return m.len();
    }
    let mut total = 0;
    let mut stack = vec![p.to_path_buf()];
    while let Some(dir) = stack.pop() {
        for e in std::fs::read_dir(&dir).into_iter().flatten().flatten() {
            match e.file_type() {
                Ok(t) if t.is_dir() => stack.push(e.path()),
                Ok(t) if t.is_file() => total += e.metadata().map(|m| m.len()).unwrap_or(0),
                _ => {}
            }
        }
    }
    total
}

fn now_ms() -> u128 {
    SystemTime::now().duration_since(UNIX_EPOCH).map(|d| d.as_millis()).unwrap_or(0)
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used)]
    use super::*;

    /// `oneup restore` or a trim from the command line can remove a slot
    /// while the agent lists or trims the store: that slot is skipped, and
    /// the round goes on.
    #[test]
    fn a_slot_gone_meanwhile_is_skipped() {
        let gone = std::env::temp_dir().join(format!("oneup-effects-gone-slot-{}", std::process::id()));
        assert!(measure_slot(&gone, "1-0".into(), 1).unwrap().is_none());
        assert!(!remove_slot(&gone).unwrap());
    }
}
