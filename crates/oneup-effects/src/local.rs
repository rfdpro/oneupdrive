//! Local effects on a Windows sync folder (see the crate docs for the rules).

use std::ffi::OsString;
use std::fs::{File, OpenOptions};
use std::io::Write as _;
use std::path::{Component, Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering::Relaxed};
use std::time::{SystemTime, UNIX_EPOCH};

use oneup_gate::{Approved, OldCopy};
use oneup_ntfs::FileId;
use oneup_ntfs::dirent::{
    DirEntry, FILE_ATTRIBUTE_OFFLINE, FILE_ATTRIBUTE_RECALL_ON_DATA_ACCESS, FILE_ATTRIBUTE_RECALL_ON_OPEN,
    FILE_ATTRIBUTE_REPARSE_POINT,
};
use oneup_ntfs::win::{self, Handle, Volume};
use oneup_rules::ENGINE_DIR;
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

/// Attributes that make an item something other than an ordinary file or
/// folder whose data is on disk. Such items are never changed.
const NOT_ORDINARY: u32 = FILE_ATTRIBUTE_REPARSE_POINT
    | FILE_ATTRIBUTE_OFFLINE
    | FILE_ATTRIBUTE_RECALL_ON_OPEN
    | FILE_ATTRIBUTE_RECALL_ON_DATA_ACCESS;

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

/// A local file opened for an upload. While this lives, nobody can write,
/// rename or delete the file, so what is sent is exactly the checked stamp.
#[derive(Debug)]
pub struct UploadSource {
    pub file: File,
    pub size: i64,
    pub mtime: i64,
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

    /// Whether the sync folder is in Files On-Demand mode (see `ondemand.rs`).
    pub fn on_demand(&self) -> bool {
        self.on_demand
    }

    pub(crate) fn volume(&self) -> &Volume {
        &self.volume
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
        let Ok(h) = self.volume.open_attributes_by_id(FileId(id.0)) else { return false };
        win::final_path(&h).is_ok_and(|p| self.is_inside(&long(&p)))
    }

    /// The current name of the item with this ID, if it exists inside the
    /// sync folder (outside OneUpDrive's own folder). Read-only.
    pub fn name_of(&self, id: LocalId) -> Option<String> {
        let h = self.volume.open_attributes_by_id(FileId(id.0)).ok()?;
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

    /// Runs an approved [`Op::RecycleLocal`]. `synced`: the hash of the last
    /// synced version of the file. If the token says
    /// [`OldCopy::DropIfSynced`] and the file still hashes to it, the file is
    /// deleted without a local copy (OneDrive's recycle bin has it);
    /// otherwise it goes into the version store.
    pub fn recycle(&self, a: &Approved, synced: Option<&Hash>) -> Result<Done> {
        let Op::RecycleLocal { local, .. } = a.op() else {
            return Err(Error::WrongOp);
        };
        self.recycle_item(local, a.old_copy(), synced)
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
                // Decided before anything moves, under the exclusive handle,
                // so the content checked is the content dropped.
                let drop_old = a.old_copy() == OldCopy::DropIfSynced && self.is_synced(&old, pre, synced)?;
                let kept = self.keep(&old, &old_entry, &old_path)?;
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
                if drop_old { self.discard_kept(old, &kept) } else { Some(kept) }
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
        let Op::Upload { local, .. } = a.op() else {
            return Err(Error::WrongOp);
        };
        let (h, size, mtime) = self.open_for_read(local)?;
        // Reading a placeholder whose content isn't all here would ask the
        // cloud (ourselves) for it.
        if self.on_demand && !self.fully_on_disk(&h, size) {
            return Err(Error::NotOrdinary);
        }
        Ok(UploadSource { file: h.into_file(), size, mtime })
    }

    /// The first `max` bytes of a file planned for upload, for the content
    /// guard (`oneup_gate::content`). Read-only, with the same checks as
    /// [`LocalFs::open_upload`]; `None` if it can't be read now (changed,
    /// open elsewhere, or its content isn't all here).
    pub fn sample(&self, pre: &LocalPre, max: usize) -> Option<Vec<u8>> {
        use std::io::Read;
        let (h, size, _) = self.open_for_read(pre).ok()?;
        if self.on_demand && !self.fully_on_disk(&h, size) {
            return None;
        }
        let mut head = Vec::with_capacity(max);
        h.into_file().take(max as u64).read_to_end(&mut head).ok()?;
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
        let (handle, size, mtime) = self.open_for_read(pre)?;
        if self.on_demand {
            // A placeholder whose identity vouches for its content is never
            // read (a dehydrated one would be downloaded from ourselves).
            if let Some(hash) = self.identity_hash(&handle, size) {
                return Ok(Done::Hashed { hash, size, mtime });
            }
            if !self.fully_on_disk(&handle, size) {
                return Err(Error::NotOrdinary);
            }
        }
        let file = handle.into_file();
        let mut buf = vec![0u8; 1 << 20];
        let h = oneup_hash::hash_reader(&file, &mut buf)?;
        // Writers are excluded while we hold the file, so this only fails
        // if the file system lied about the size.
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
        let (h, _, path) = self.open_item(pre)?;
        if in_place && path.parent() != Some(dir.as_path()) {
            return Err(Error::Changed);
        }
        win::rename_handle(&h, &target)?;
        if self.on_demand {
            self.resync_moved(&h, &target);
        }
        let entry = win::stat_handle(&h, FileId(pre.id.0))?.ok_or(Error::Changed)?;
        Ok(Done::Placed { parent: to, entry, kept: None })
    }

    fn recycle_item(&self, pre: &LocalPre, old: OldCopy, synced: Option<&Hash>) -> Result<Done> {
        let (h, entry, path) = self.open_item(pre)?;
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
    /// simply stays in the store (`Some`).
    fn discard_kept(&self, h: Handle, kept: &Path) -> Option<PathBuf> {
        if let Err(e) = win::delete_handle(&h) {
            tracing::warn!(error = %e, "an old copy couldn't be removed; it stays in the version store");
            return Some(kept.to_path_buf());
        }
        // The deletion completes when the handle closes.
        drop(h);
        self.drop_slot(kept);
        None
    }

    // ------------------------------------------------------- version store

    /// Everything in the version store, oldest first. Read-only.
    pub fn versions(&self) -> Result<Vec<Slot>> {
        let mut out = Vec::new();
        for e in std::fs::read_dir(self.engine.join(VERSIONS))?.flatten() {
            let Some(id) = e.file_name().to_str().map(str::to_string) else { continue };
            let Some(at_ms) = id.split_once('-').and_then(|(ms, _)| ms.parse::<i64>().ok()) else { continue };
            if !e.file_type().is_ok_and(|t| t.is_dir()) {
                continue;
            }
            let slot = e.path();
            let origin = std::fs::read_to_string(slot.join(ORIGIN)).ok().map(|s| s.trim().to_string());
            let mut items = std::fs::read_dir(&slot)?.flatten().filter(|i| i.file_name() != ORIGIN);
            let Some(item) = items.next() else { continue };
            let is_dir = item.file_type().is_ok_and(|t| t.is_dir());
            out.push(Slot {
                id,
                at_ms,
                name: item.file_name().to_string_lossy().into_owned(),
                origin,
                is_dir,
                bytes: size_of(&item.path()),
            });
        }
        out.sort_by(|a, b| (a.at_ms, &a.id).cmp(&(b.at_ms, &b.id)));
        Ok(out)
    }

    /// Moves a kept item back into the sync folder: to `to` (a path relative
    /// to the sync folder, `\` or `/` separated) or else where it came from.
    /// Never replaces anything: if the name is taken, the item is restored
    /// as `name (restored N).ext` beside it. The folder it goes into must
    /// exist. Returns the path it was restored to, relative to the sync
    /// folder. The next sync uploads it like any new file.
    pub fn restore(&self, slot_id: &str, to: Option<&str>) -> Result<String> {
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
        // Pin the destination folder so it can't be swapped while we move.
        let mut dir = self.root.clone();
        for f in folders {
            dir.push(f);
        }
        let probe = win::open_dir(&dir).map_err(|_| Error::Changed)?;
        let (_, dir_id) = win::file_id(&probe)?;
        drop(probe);
        let (_pin, dir_path) = self.pin_folder(LocalId(dir_id.0))?;

        let is_dir = item.file_type().is_ok_and(|t| t.is_dir());
        let h = win::open_path_for_change(&item.path(), is_dir)?;
        for n in 0..100 {
            let candidate = if n == 0 { (*name).to_string() } else { restored_name(name, n, is_dir) };
            match win::rename_handle(&h, &dir_path.join(&candidate)) {
                Ok(()) => {
                    drop(h);
                    self.drop_slot(&slot.join(item.file_name()));
                    let mut out: Vec<&str> = folders.to_vec();
                    out.push(&candidate);
                    return Ok(out.join("\\"));
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
            if !dry_run {
                std::fs::remove_dir_all(self.engine.join(VERSIONS).join(&s.id))?;
            }
            total = total.saturating_sub(s.bytes);
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
            std::fs::remove_dir_all(self.engine.join(VERSIONS).join(&s.id))?;
            removed.push(s);
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
        let h = self.volume.open_for_change(id, pre.stamp.is_none())?;
        let entry = win::stat_handle(&h, id)?.ok_or(Error::Changed)?;
        check_placeholder(&entry, pre.stamp, self.on_demand)?;
        let path = long(&win::final_path(&h)?);
        if !self.is_inside(&path) {
            return Err(Error::Outside);
        }
        Ok((h, entry, path))
    }

    /// Opens a planned file for reading with writers excluded.
    fn open_for_read(&self, pre: &LocalPre) -> Result<(Handle, i64, i64)> {
        let Some(stamp) = pre.stamp else {
            return Err(Error::WrongOp);
        };
        let id = FileId(pre.id.0);
        if self.dehydrated_path(id)?.is_some() {
            // Opening it with data access would download it from ourselves.
            return Err(Error::NotOrdinary);
        }
        let h = self.volume.open_for_upload(id)?;
        let entry = win::stat_handle(&h, id)?.ok_or(Error::Changed)?;
        check_placeholder(&entry, Some(stamp), self.on_demand)?;
        if !self.is_inside(&long(&win::final_path(&h)?)) {
            return Err(Error::Outside);
        }
        Ok((h, stamp.0, stamp.1))
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
        let name = if entry.name_lossy || entry.name.is_empty() { "item" } else { entry.name.as_str() };
        let kept = slot.join(name);
        if let Err(e) = win::rename_handle(h, &kept) {
            let _ = std::fs::remove_dir(&slot);
            return Err(e.into());
        }
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
fn is_cloud_tag(tag: u32) -> bool {
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
