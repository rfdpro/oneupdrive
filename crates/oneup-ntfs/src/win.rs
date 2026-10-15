//! Thin Windows FFI layer. Each function obtains a buffer from the OS and
//! hands it to the pure parsers in [`crate::record`] / [`crate::dirent`].
//!
//! Only [`rename_handle`] and [`delete_handle`] modify the file system, and
//! only through a handle the caller opened and checked; the effects layer
//! is their one user for user content.

use std::ffi::c_void;
use std::os::windows::ffi::OsStrExt;
use std::path::{Path, PathBuf};

use windows::Win32::Foundation::{
    CloseHandle, ERROR_ACCESS_DENIED, ERROR_ALREADY_EXISTS, ERROR_DELETE_PENDING, ERROR_FILE_EXISTS,
    ERROR_FILE_NOT_FOUND, ERROR_HANDLE_EOF, ERROR_INVALID_PARAMETER, ERROR_LOCK_VIOLATION, ERROR_NO_MORE_FILES,
    ERROR_PATH_NOT_FOUND, ERROR_SHARING_VIOLATION, HANDLE, WIN32_ERROR,
};
use windows::Win32::Storage::FileSystem::{
    CreateFileW, DELETE, ExtendedFileIdType, FILE_ATTRIBUTE_TAG_INFO, FILE_BASIC_INFO, FILE_DISPOSITION_FLAG_DELETE,
    FILE_DISPOSITION_FLAG_IGNORE_READONLY_ATTRIBUTE, FILE_DISPOSITION_INFO, FILE_DISPOSITION_INFO_EX,
    FILE_DISPOSITION_INFO_EX_FLAGS, FILE_FLAG_BACKUP_SEMANTICS, FILE_FLAG_OPEN_REPARSE_POINT,
    FILE_FLAGS_AND_ATTRIBUTES, FILE_ID_128, FILE_ID_DESCRIPTOR, FILE_ID_DESCRIPTOR_0, FILE_ID_INFO,
    FILE_INFO_BY_HANDLE_CLASS, FILE_LIST_DIRECTORY, FILE_NAME_NORMALIZED, FILE_READ_ATTRIBUTES, FILE_READ_DATA,
    FILE_RENAME_INFO, FILE_SHARE_DELETE, FILE_SHARE_MODE, FILE_SHARE_READ, FILE_SHARE_WRITE, FILE_STANDARD_INFO,
    FILE_TRAVERSE, FILE_WRITE_ATTRIBUTES, FileAttributeTagInfo, FileBasicInfo, FileDispositionInfo,
    FileDispositionInfoEx, FileIdExtdDirectoryInfo, FileIdExtdDirectoryRestartInfo, FileIdInfo, FileNameInfo,
    FileRenameInfo, FileStandardInfo, GetFileInformationByHandleEx, GetFinalPathNameByHandleW, GetVolumePathNameW,
    OPEN_EXISTING, OpenFileById, SetFileInformationByHandle,
};
use windows::Win32::System::IO::DeviceIoControl;
use windows::Win32::System::Ioctl::{FSCTL_QUERY_USN_JOURNAL, READ_USN_JOURNAL_DATA_V1, USN_JOURNAL_DATA_V2};
use windows::core::PCWSTR;

use crate::FileId;
use crate::dirent::{DirEntry, parse_dir_buffer};
use crate::record::{Records, parse_read_buffer};

/// `CTL_CODE(FILE_DEVICE_FILE_SYSTEM, 234, METHOD_NEITHER, FILE_ANY_ACCESS)`.
/// Defined here because not every SDK metadata version exports it.
pub const FSCTL_READ_UNPRIVILEGED_USN_JOURNAL: u32 = 0x0009_03AB;

#[derive(Debug, thiserror::Error)]
pub enum Error {
    #[error("{op} failed: {source}")]
    Win32 { op: &'static str, source: windows::core::Error },
    #[error(transparent)]
    Parse(#[from] crate::record::ParseError),
    #[error(transparent)]
    DirParse(#[from] crate::dirent::DirParseError),
}

impl Error {
    /// Whether the OS refused access (ACLs), as opposed to the item being gone
    /// or the call failing for another reason. Scanners count these and move on.
    pub fn is_access_denied(&self) -> bool {
        self.is(&[ERROR_ACCESS_DENIED])
    }

    /// The item doesn't exist (any more). `OpenFileById` reports an unknown
    /// ID as "invalid parameter".
    pub fn is_not_found(&self) -> bool {
        self.is(&[ERROR_FILE_NOT_FOUND, ERROR_PATH_NOT_FOUND, ERROR_INVALID_PARAMETER, ERROR_DELETE_PENDING])
    }

    /// Another program has the item open in a way that excludes us.
    pub fn is_busy(&self) -> bool {
        self.is(&[ERROR_SHARING_VIOLATION, ERROR_LOCK_VIOLATION])
    }

    /// The target name is taken.
    pub fn is_exists(&self) -> bool {
        self.is(&[ERROR_ALREADY_EXISTS, ERROR_FILE_EXISTS])
    }

    fn is(&self, codes: &[WIN32_ERROR]) -> bool {
        matches!(self, Error::Win32 { source, .. } if codes.iter().any(|c| source.code() == c.to_hresult()))
    }
}

pub type Result<T> = std::result::Result<T, Error>;

fn win<T>(op: &'static str, r: windows::core::Result<T>) -> Result<T> {
    r.map_err(|source| Error::Win32 { op, source })
}

fn wide(p: &Path) -> Vec<u16> {
    p.as_os_str().encode_wide().chain(Some(0)).collect()
}

/// An owned Win32 handle, closed on drop.
#[derive(Debug)]
pub struct Handle(HANDLE);

impl Handle {
    pub fn raw(&self) -> HANDLE {
        self.0
    }

    /// Lends the handle as a standard file (to read through it) while the
    /// handle keeps ownership. Reads move the handle's file position.
    pub fn as_file(&self) -> BorrowedFile<'_> {
        use std::os::windows::io::{FromRawHandle, RawHandle};
        // SAFETY: the handle stays open for the borrow's lifetime, and
        // `ManuallyDrop` stops the `File` from closing it.
        let file = unsafe { std::fs::File::from_raw_handle(self.0.0 as RawHandle) };
        BorrowedFile { file: std::mem::ManuallyDrop::new(file), _handle: std::marker::PhantomData }
    }

    /// Converts into a standard file, which then owns (and closes) the handle.
    pub fn into_file(self) -> std::fs::File {
        use std::os::windows::io::{FromRawHandle, RawHandle};
        let me = std::mem::ManuallyDrop::new(self);
        // SAFETY: we own the handle; `ManuallyDrop` stops `Handle::drop` from
        // closing it, so ownership passes to the `File` exactly once.
        unsafe { std::fs::File::from_raw_handle(me.0.0 as RawHandle) }
    }
}

impl Drop for Handle {
    fn drop(&mut self) {
        // SAFETY: we own this handle and close it exactly once.
        unsafe {
            let _ = CloseHandle(self.0);
        }
    }
}

/// A [`Handle`] lent as a [`std::fs::File`] (see [`Handle::as_file`]).
#[derive(Debug)]
pub struct BorrowedFile<'h> {
    file: std::mem::ManuallyDrop<std::fs::File>,
    _handle: std::marker::PhantomData<&'h Handle>,
}

impl std::ops::Deref for BorrowedFile<'_> {
    type Target = std::fs::File;
    fn deref(&self) -> &std::fs::File {
        &self.file
    }
}

// SAFETY: kernel handles may be used from any thread.
unsafe impl Send for Handle {}
// SAFETY: the operations we perform on shared handles are thread-safe in the kernel.
unsafe impl Sync for Handle {}

fn open(path: &Path, access: u32, flags: FILE_FLAGS_AND_ATTRIBUTES) -> Result<Handle> {
    open_shared(path, access, FILE_SHARE_READ | FILE_SHARE_WRITE | FILE_SHARE_DELETE, flags)
}

fn open_shared(path: &Path, access: u32, share: FILE_SHARE_MODE, flags: FILE_FLAGS_AND_ATTRIBUTES) -> Result<Handle> {
    let w = wide(path);
    // SAFETY: `w` is NUL-terminated and outlives the call.
    let h = unsafe { CreateFileW(PCWSTR(w.as_ptr()), access, share, None, OPEN_EXISTING, flags, None) };
    win("CreateFileW", h).map(Handle)
}

/// Opens the item at `path` for changing it, with the same access and
/// sharing as [`Volume::open_for_change`] (which callers should prefer for
/// user content: it addresses the item by ID). Reparse points are opened as
/// themselves, never followed.
pub fn open_path_for_change(path: &Path, folder: bool) -> Result<Handle> {
    // Data access (read data / list) is what makes the share mode bite:
    // Windows does not enforce sharing against a handle that only holds
    // attribute and delete access (tested).
    let access = DELETE.0 | FILE_READ_ATTRIBUTES.0 | if folder { FILE_LIST_DIRECTORY.0 } else { FILE_READ_DATA.0 };
    let share = if folder { FILE_SHARE_READ | FILE_SHARE_WRITE } else { FILE_SHARE_MODE(0) };
    open_shared(path, access, share, FILE_FLAG_BACKUP_SEMANTICS | FILE_FLAG_OPEN_REPARSE_POINT)
}

/// Like [`open_path_for_change`], for one of OneUpDrive's own cloud
/// placeholders: also with attribute-write access (what the Cloud Files
/// calls need), and without `FILE_FLAG_OPEN_REPARSE_POINT` (the placeholder
/// is opened through the Cloud Files filter, as Windows expects). Files are
/// opened share-none, so no other program can open one while it's held.
pub fn open_path_for_placeholder(path: &Path, folder: bool) -> Result<Handle> {
    let access = DELETE.0
        | FILE_READ_ATTRIBUTES.0
        | FILE_WRITE_ATTRIBUTES.0
        | if folder { FILE_LIST_DIRECTORY.0 } else { FILE_READ_DATA.0 };
    let share = if folder { FILE_SHARE_READ | FILE_SHARE_WRITE } else { FILE_SHARE_MODE(0) };
    open_shared(path, access, share, FILE_FLAG_BACKUP_SEMANTICS)
}

/// Opens an item with attribute access only, sharing everything: a folder
/// another program has as its current folder (whose handle doesn't share
/// delete access) still opens. What reverting another sync app's folder
/// placeholder needs, and no more.
pub fn open_path_for_attributes(path: &Path) -> Result<Handle> {
    open_shared(
        path,
        FILE_READ_ATTRIBUTES.0 | FILE_WRITE_ATTRIBUTES.0,
        FILE_SHARE_READ | FILE_SHARE_WRITE | FILE_SHARE_DELETE,
        FILE_FLAG_BACKUP_SEMANTICS,
    )
}

/// Opens one of OneUpDrive's own **dehydrated** placeholders with attribute
/// and delete access only. Data access would make Windows download it
/// (measured: even with `FILE_FLAG_OPEN_NO_RECALL`). Without data access,
/// Windows doesn't enforce share modes, so other programs aren't locked
/// out; callers that need that use the provider's hold instead.
pub fn open_path_for_dehydrated(path: &Path) -> Result<Handle> {
    open_shared(
        path,
        DELETE.0 | FILE_READ_ATTRIBUTES.0 | FILE_WRITE_ATTRIBUTES.0,
        FILE_SHARE_READ | FILE_SHARE_WRITE | FILE_SHARE_DELETE,
        FILE_FLAG_BACKUP_SEMANTICS,
    )
}

/// Opens a directory for listing. Reparse points are opened as themselves, never followed.
pub fn open_dir(path: &Path) -> Result<Handle> {
    open(path, FILE_LIST_DIRECTORY.0, FILE_FLAG_BACKUP_SEMANTICS | FILE_FLAG_OPEN_REPARSE_POINT)
}

/// Returns the volume mount point containing `path`, e.g. `C:\`.
pub fn volume_root(path: &Path) -> Result<PathBuf> {
    let w = wide(path);
    let mut buf = [0u16; 1024];
    // SAFETY: `w` is NUL-terminated; `buf` is a valid output buffer.
    win("GetVolumePathNameW", unsafe { GetVolumePathNameW(PCWSTR(w.as_ptr()), &mut buf) })?;
    let len = buf.iter().position(|&c| c == 0).unwrap_or(buf.len());
    Ok(PathBuf::from(String::from_utf16_lossy(&buf[..len])))
}

/// Journal identity and cursor bounds.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct JournalInfo {
    pub journal_id: u64,
    pub first_usn: i64,
    pub next_usn: i64,
    pub lowest_valid_usn: i64,
    pub max_usn: i64,
    pub max_size: u64,
    pub max_major_version: u16,
}

/// A volume opened with the minimum access needed for the unprivileged
/// change journal (`FILE_TRAVERSE`). Works without administrator rights.
#[derive(Debug)]
pub struct Volume {
    handle: Handle,
    /// A directory handle on the same volume, used as the `OpenFileById` hint.
    hint: Handle,
}

impl Volume {
    /// Opens the volume containing `path` (e.g. `\\.\C:` for `C:\Users\...`).
    pub fn open_containing(path: &Path) -> Result<Self> {
        let root = volume_root(path)?;
        let device = format!(r"\\.\{}", root.to_string_lossy().trim_end_matches('\\'));
        let handle = open(Path::new(&device), FILE_TRAVERSE.0, FILE_FLAGS_AND_ATTRIBUTES(0))?;
        let hint = open(&root, FILE_READ_ATTRIBUTES.0, FILE_FLAG_BACKUP_SEMANTICS)?;
        Ok(Self { handle, hint })
    }

    pub fn query_journal(&self) -> Result<JournalInfo> {
        let mut d = USN_JOURNAL_DATA_V2::default();
        let mut returned = 0u32;
        // SAFETY: output buffer is a properly sized, writable struct.
        win("FSCTL_QUERY_USN_JOURNAL", unsafe {
            DeviceIoControl(
                self.handle.raw(),
                FSCTL_QUERY_USN_JOURNAL,
                None,
                0,
                Some(std::ptr::from_mut(&mut d).cast::<c_void>()),
                size_of::<USN_JOURNAL_DATA_V2>() as u32,
                Some(&mut returned),
                None,
            )
        })?;
        Ok(JournalInfo {
            journal_id: d.UsnJournalID,
            first_usn: d.FirstUsn,
            next_usn: d.NextUsn,
            lowest_valid_usn: d.LowestValidUsn,
            max_usn: d.MaxUsn,
            max_size: d.MaximumSize,
            max_major_version: d.MaxSupportedMajorVersion,
        })
    }

    /// Reads journal records from `start` into `buf` (unprivileged FSCTL).
    ///
    /// If `wait` is true the call blocks in the kernel until at least one new
    /// record exists. That is how the agent idles at zero CPU.
    pub fn read_journal<'b>(
        &self,
        journal_id: u64,
        start: i64,
        wait: bool,
        buf: &'b mut [u8],
    ) -> Result<(i64, Records<'b>)> {
        let input = READ_USN_JOURNAL_DATA_V1 {
            StartUsn: start,
            ReasonMask: u32::MAX,
            ReturnOnlyOnClose: 0,
            Timeout: 0,
            BytesToWaitFor: u64::from(wait),
            UsnJournalID: journal_id,
            MinMajorVersion: 2,
            MaxMajorVersion: 3,
        };
        let mut returned = 0u32;
        // SAFETY: input/output buffers are valid for the stated sizes for the
        // whole (synchronous) call.
        let r = unsafe {
            DeviceIoControl(
                self.handle.raw(),
                FSCTL_READ_UNPRIVILEGED_USN_JOURNAL,
                Some(std::ptr::from_ref(&input).cast::<c_void>()),
                size_of::<READ_USN_JOURNAL_DATA_V1>() as u32,
                Some(buf.as_mut_ptr().cast::<c_void>()),
                buf.len() as u32,
                Some(&mut returned),
                None,
            )
        };
        win("FSCTL_READ_UNPRIVILEGED_USN_JOURNAL", r)?;
        let filled = &buf[..returned as usize];
        Ok(parse_read_buffer(filled)?)
    }

    /// Opens a file or directory by ID without following reparse points and
    /// returns its current normalized path. `None` if it no longer exists or
    /// isn't accessible.
    pub fn path_of(&self, id: FileId) -> Option<PathBuf> {
        let h = self.open_by_id(id, FILE_READ_ATTRIBUTES.0).ok()?;
        final_path(&h).ok()
    }

    /// The current metadata of the file or folder with this ID, in the same
    /// shape a directory listing returns, so the scanner's rules apply
    /// unchanged. `Ok(None)` if no such item exists any more, or it is being
    /// deleted (a delete-pending file still opens but is about to vanish).
    ///
    /// Used by the change-journal observer because unprivileged journal
    /// records carry no names (measured). One open plus four small queries,
    /// with `FILE_READ_ATTRIBUTES` only: never reads data, so a cloud
    /// placeholder is not downloaded.
    ///
    /// The name is the last component of the path Windows used to open the
    /// item. For a file with several hard links that is one of its names.
    pub fn stat_by_id(&self, id: FileId) -> Result<Option<DirEntry>> {
        let h = match self.open_by_id(id, FILE_READ_ATTRIBUTES.0) {
            Ok(h) => h,
            Err(e) if e.is_not_found() => return Ok(None),
            Err(e) => return Err(e),
        };
        stat_handle(&h, id)
    }

    /// Opens the item with this ID for changing it (rename, move, recycle),
    /// and for reading its metadata through the same handle, so what is
    /// checked is exactly what is then changed.
    ///
    /// Files are opened **exclusively** (no sharing at all): while we hold
    /// the handle nobody can write, rename or delete the file, and if
    /// anyone has it open already the open fails ([`Error::is_busy`]),
    /// which means "in use, try again later". Folders are shared for
    /// reading and writing (their children stay usable) but not for
    /// deletion, so nobody can move or delete the folder meanwhile.
    pub fn open_for_change(&self, id: FileId, folder: bool) -> Result<Handle> {
        // NTFS refuses to rename through a handle opened by ID ("invalid
        // parameter", tested), so: find the item's current path by ID, open
        // that path, and prove the handle is still the same item. If it
        // was moved or replaced in between, the IDs differ and we report
        // "not found", which callers treat as "changed, re-plan".
        let path = {
            let probe = self.open_by_id(id, FILE_READ_ATTRIBUTES.0)?;
            let mut long = std::ffi::OsString::from(r"\\?\");
            long.push(final_path(&probe)?.as_os_str());
            PathBuf::from(long)
        };
        let h = open_path_for_change(&path, folder)?;
        if file_id(&h)?.1 != id {
            return Err(Error::Win32 { op: "open_for_change", source: ERROR_FILE_NOT_FOUND.into() });
        }
        Ok(h)
    }

    /// Opens any item by ID for reading its attributes and path only. Fully
    /// shared, so it disturbs nobody.
    pub fn open_attributes_by_id(&self, id: FileId) -> Result<Handle> {
        self.open_by_id(id, FILE_READ_ATTRIBUTES.0)
    }

    /// Opens a folder by ID so that, while the handle is held, nobody can
    /// rename, move or delete it (no delete sharing); its contents stay
    /// fully usable. Used to keep a destination folder where it is while
    /// something is placed into it. No delete access is requested, so
    /// programs that merely have the folder open (Explorer, a console's
    /// current directory) don't block this.
    pub fn open_dir_pinned(&self, id: FileId) -> Result<Handle> {
        self.open_by_id_shared(id, FILE_LIST_DIRECTORY.0 | FILE_READ_ATTRIBUTES.0, FILE_SHARE_READ | FILE_SHARE_WRITE)
    }

    /// Opens a file by ID to read its content for an upload: others may
    /// keep reading it, but nobody can write, rename or delete it until the
    /// handle is closed, so the bytes sent are the bytes that were checked.
    pub fn open_for_upload(&self, id: FileId) -> Result<Handle> {
        self.open_by_id_shared(id, FILE_READ_DATA.0 | FILE_READ_ATTRIBUTES.0, FILE_SHARE_READ)
    }

    /// Opens a directory for listing by its file ID. Reparse points are opened
    /// as themselves, never followed.
    ///
    /// Opening by ID rather than by path means a scan is unaffected by paths
    /// longer than `MAX_PATH` and by folders renamed while it runs.
    pub fn open_dir_by_id(&self, id: FileId) -> Result<Handle> {
        self.open_by_id(id, FILE_LIST_DIRECTORY.0)
    }

    /// Opens a file by its ID for reading its contents (and attributes).
    /// Read-only and fully shared, so other programs are not disturbed.
    ///
    /// Caution: opening does not stop a cloud provider from downloading a
    /// dehydrated placeholder once its data is read. Not even
    /// `FILE_FLAG_OPEN_NO_RECALL` stops it (measured). Callers must
    /// check [`attribute_tag`] on the returned handle and refuse to read cloud
    /// placeholders.
    pub fn open_file_by_id(&self, id: FileId) -> Result<Handle> {
        self.open_by_id(id, FILE_READ_DATA.0 | FILE_READ_ATTRIBUTES.0)
    }

    fn open_by_id(&self, id: FileId, access: u32) -> Result<Handle> {
        self.open_by_id_shared(id, access, FILE_SHARE_READ | FILE_SHARE_WRITE | FILE_SHARE_DELETE)
    }

    fn open_by_id_shared(&self, id: FileId, access: u32, share: FILE_SHARE_MODE) -> Result<Handle> {
        let desc = FILE_ID_DESCRIPTOR {
            dwSize: size_of::<FILE_ID_DESCRIPTOR>() as u32,
            Type: ExtendedFileIdType,
            Anonymous: FILE_ID_DESCRIPTOR_0 { ExtendedFileId: FILE_ID_128 { Identifier: id.to_le_bytes() } },
        };
        // SAFETY: `desc` is fully initialised; the hint handle is a valid handle on the volume.
        let h = unsafe {
            OpenFileById(
                self.hint.raw(),
                &desc,
                access,
                share,
                None,
                FILE_FLAG_BACKUP_SEMANTICS | FILE_FLAG_OPEN_REPARSE_POINT,
            )
        };
        win("OpenFileById", h).map(Handle)
    }
}

/// The metadata of an open item, in directory-listing shape (see
/// [`Volume::stat_by_id`]). `Ok(None)` if it is being deleted.
pub fn stat_handle(h: &Handle, id: FileId) -> Result<Option<DirEntry>> {
    let basic: FILE_BASIC_INFO = handle_info(h, FileBasicInfo, "FileBasicInfo")?;
    let standard: FILE_STANDARD_INFO = handle_info(h, FileStandardInfo, "FileStandardInfo")?;
    if standard.DeletePending {
        return Ok(None);
    }
    let (_, reparse_tag) = attribute_tag(h)?;
    // FILE_NAME_INFO: a u32 byte length, then that many bytes of UTF-16.
    // Paths are at most 32,767 UTF-16 units; u32 words keep the buffer
    // aligned for the struct.
    let mut words = vec![0u32; 1 + 32_768 / 2 + 1];
    // SAFETY: `words` is a valid, aligned, writable buffer of the stated size.
    win("GetFileInformationByHandleEx(FileNameInfo)", unsafe {
        GetFileInformationByHandleEx(
            h.raw(),
            FileNameInfo,
            words.as_mut_ptr().cast::<c_void>(),
            (words.len() * 4) as u32,
        )
    })?;
    let bytes: Vec<u8> = words.iter().flat_map(|w| w.to_le_bytes()).collect();
    let (name, name_lossy) = crate::dirent::last_name_of_file_name_info(&bytes).unwrap_or_default();
    Ok(Some(DirEntry {
        file_id: id,
        name,
        name_lossy,
        size: u64::try_from(standard.EndOfFile).unwrap_or(0),
        attributes: basic.FileAttributes,
        reparse_tag,
        created: basic.CreationTime,
        modified: basic.LastWriteTime,
        changed: basic.ChangeTime,
    }))
}

/// Renames or moves the open item to `target` (a full path on the same
/// volume). **Never replaces** an existing item: if `target` exists, this
/// fails ([`Error::is_exists`]). The handle must have `DELETE` access
/// ([`Volume::open_for_change`]). Because it acts on the handle, it moves
/// exactly the item that was opened and checked, whatever happened to its
/// path meanwhile.
pub fn rename_handle(h: &Handle, target: &Path) -> Result<()> {
    let name = nt_path(target);
    let name_off = std::mem::offset_of!(FILE_RENAME_INFO, FileName);
    let size = name_off + (name.len() + 1) * 2;
    // u64 words: aligned for the struct, which holds a pointer-sized handle.
    let mut buf = vec![0u64; size.div_ceil(8)];
    let info = buf.as_mut_ptr().cast::<FILE_RENAME_INFO>();
    // SAFETY: `buf` is zeroed, aligned and at least `size` bytes, so every
    // field written lies inside it; the name (plus the zeroed terminator)
    // fits from `FileName` onwards by construction of `size`. Zeroed memory
    // means ReplaceIfExists = false and RootDirectory = NULL.
    unsafe {
        (*info).FileNameLength = (name.len() * 2) as u32;
        std::ptr::copy_nonoverlapping(
            name.as_ptr(),
            buf.as_mut_ptr().cast::<u8>().add(name_off).cast::<u16>(),
            name.len(),
        );
    }
    // SAFETY: `info` points to a valid FILE_RENAME_INFO of `size` bytes.
    win("SetFileInformationByHandle(FileRenameInfo)", unsafe {
        SetFileInformationByHandle(h.raw(), FileRenameInfo, info.cast::<c_void>(), size as u32)
    })
}

/// Deletes the open item: it is gone once the handle is closed. Read-only
/// files are deleted too. The handle must have `DELETE` access
/// ([`Volume::open_for_change`]); because it acts on the handle, it deletes
/// exactly the item that was opened and checked. Callers in OneUpDrive use
/// it only through the effects layer's gated operations.
pub fn delete_handle(h: &Handle) -> Result<()> {
    let flags = FILE_DISPOSITION_FLAG_DELETE.0 | FILE_DISPOSITION_FLAG_IGNORE_READONLY_ATTRIBUTE.0;
    let ex = FILE_DISPOSITION_INFO_EX { Flags: FILE_DISPOSITION_INFO_EX_FLAGS(flags) };
    // SAFETY: `ex` is a valid FILE_DISPOSITION_INFO_EX of the stated size.
    let r = unsafe {
        SetFileInformationByHandle(
            h.raw(),
            FileDispositionInfoEx,
            std::ptr::from_ref(&ex).cast::<c_void>(),
            size_of::<FILE_DISPOSITION_INFO_EX>() as u32,
        )
    };
    if r.is_ok() {
        return Ok(());
    }
    // File systems without the extended class (not NTFS) take the classic one.
    let classic = FILE_DISPOSITION_INFO { DeleteFile: true };
    // SAFETY: `classic` is a valid FILE_DISPOSITION_INFO of the stated size.
    win("SetFileInformationByHandle(FileDispositionInfo)", unsafe {
        SetFileInformationByHandle(
            h.raw(),
            FileDispositionInfo,
            std::ptr::from_ref(&classic).cast::<c_void>(),
            size_of::<FILE_DISPOSITION_INFO>() as u32,
        )
    })
}

/// `FileRenameInfo` goes to the kernel as is, so a full target path must be
/// in NT form (`\??\C:\dir\name`); a Win32 path is "invalid parameter"
/// (tested). Callers pass absolute, normalised paths (we build them from
/// the sync root), so only the prefix needs translating.
fn nt_path(p: &Path) -> Vec<u16> {
    let s: Vec<u16> = p.as_os_str().encode_wide().collect();
    let w = |t: &str| t.encode_utf16().collect::<Vec<u16>>();
    let (prefix, rest) = if s.starts_with(&w(r"\\?\")) || s.starts_with(&w(r"\??\")) {
        (w(r"\??\"), &s[4..])
    } else if s.starts_with(&w(r"\\")) {
        (w(r"\??\UNC\"), &s[2..])
    } else {
        (w(r"\??\"), &s[..])
    };
    prefix.into_iter().chain(rest.iter().copied()).collect()
}

/// One fixed-size `GetFileInformationByHandleEx` query into a `T`.
fn handle_info<T: Default>(h: &Handle, class: FILE_INFO_BY_HANDLE_CLASS, op: &'static str) -> Result<T> {
    let mut out = T::default();
    // SAFETY: `out` is a properly sized, writable `T`, and the caller pairs
    // each class with its documented struct.
    win(op, unsafe {
        GetFileInformationByHandleEx(
            h.raw(),
            class,
            std::ptr::from_mut(&mut out).cast::<c_void>(),
            size_of::<T>() as u32,
        )
    })?;
    Ok(out)
}

/// The attributes and reparse tag of an open handle (the same fields a
/// directory listing reports for its entries).
pub fn attribute_tag(h: &Handle) -> Result<(u32, u32)> {
    let mut info = FILE_ATTRIBUTE_TAG_INFO::default();
    // SAFETY: output buffer is a properly sized, writable struct.
    win("GetFileInformationByHandleEx(FileAttributeTagInfo)", unsafe {
        GetFileInformationByHandleEx(
            h.raw(),
            FileAttributeTagInfo,
            std::ptr::from_mut(&mut info).cast::<c_void>(),
            size_of::<FILE_ATTRIBUTE_TAG_INFO>() as u32,
        )
    })?;
    Ok((info.FileAttributes, info.ReparseTag))
}

/// The normalized path of an open handle (`\\?\C:\...` prefix stripped).
pub fn final_path(h: &Handle) -> Result<PathBuf> {
    let mut buf = vec![0u16; 512];
    loop {
        // SAFETY: `buf` is a valid writable buffer of the given length.
        let n = unsafe { GetFinalPathNameByHandleW(h.raw(), &mut buf, FILE_NAME_NORMALIZED) } as usize;
        if n == 0 {
            return Err(Error::Win32 { op: "GetFinalPathNameByHandleW", source: windows::core::Error::from_thread() });
        }
        if n < buf.len() {
            let s = String::from_utf16_lossy(&buf[..n]);
            return Ok(PathBuf::from(s.strip_prefix(r"\\?\").unwrap_or(&s)));
        }
        buf.resize(n + 1, 0);
    }
}

/// The 128-bit file ID and volume serial of an open handle.
pub fn file_id(h: &Handle) -> Result<(u64, FileId)> {
    let mut info = FILE_ID_INFO::default();
    // SAFETY: output buffer is a properly sized, writable struct.
    win("GetFileInformationByHandleEx(FileIdInfo)", unsafe {
        GetFileInformationByHandleEx(
            h.raw(),
            FileIdInfo,
            std::ptr::from_mut(&mut info).cast::<c_void>(),
            size_of::<FILE_ID_INFO>() as u32,
        )
    })?;
    Ok((info.VolumeSerialNumber, FileId::from_le_bytes(info.FileId.Identifier)))
}

/// Lists one directory using large batched `FileIdExtdDirectoryInfo` reads.
/// `buf` is reused across calls so enumeration allocates almost nothing.
pub fn list_dir(dir: &Handle, buf: &mut [u8], out: &mut Vec<DirEntry>) -> Result<()> {
    let mut class = FileIdExtdDirectoryRestartInfo;
    loop {
        // SAFETY: `buf` is valid and writable for `buf.len()` bytes.
        let r = unsafe {
            GetFileInformationByHandleEx(dir.raw(), class, buf.as_mut_ptr().cast::<c_void>(), buf.len() as u32)
        };
        class = FileIdExtdDirectoryInfo;
        match r {
            Ok(()) => out.extend(parse_dir_buffer(buf)?.into_iter().filter(|e| !e.is_dot())),
            Err(e) if e.code() == ERROR_NO_MORE_FILES.to_hresult() || e.code() == ERROR_HANDLE_EOF.to_hresult() => {
                return Ok(());
            }
            Err(source) => return Err(Error::Win32 { op: "GetFileInformationByHandleEx(dir)", source }),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::mem::offset_of;
    use windows::Win32::Storage::FileSystem::FILE_ID_EXTD_DIR_INFO;
    use windows::Win32::System::Ioctl::{USN_RECORD_V2, USN_RECORD_V3};

    /// The pure parsers hard-code layouts; prove they match the SDK.
    #[test]
    fn layouts_match_sdk() {
        assert_eq!(offset_of!(FILE_ID_EXTD_DIR_INFO, FileName), crate::dirent::NAME);
        assert_eq!(offset_of!(FILE_ID_EXTD_DIR_INFO, FileId), 72);
        assert_eq!(offset_of!(FILE_ID_EXTD_DIR_INFO, ReparsePointTag), 68);
        assert_eq!(offset_of!(USN_RECORD_V2, FileName), 60);
        assert_eq!(offset_of!(USN_RECORD_V2, Usn), 24);
        assert_eq!(offset_of!(USN_RECORD_V3, FileName), 76);
        assert_eq!(offset_of!(USN_RECORD_V3, Usn), 40);
        assert_eq!(offset_of!(USN_RECORD_V3, FileAttributes), 68);
    }

    #[test]
    fn list_and_identify_temp_dir() {
        let dir = std::env::temp_dir().join(format!("oneup-ntfs-test-{}", std::process::id()));
        std::fs::create_dir_all(dir.join("sub")).unwrap();
        std::fs::write(dir.join("a.txt"), b"hello").unwrap();

        let h = open_dir(&dir).unwrap();
        let mut buf = vec![0u8; 64 * 1024];
        let mut entries = Vec::new();
        list_dir(&h, &mut buf, &mut entries).unwrap();
        entries.sort_by(|a, b| a.name.cmp(&b.name));
        assert_eq!(entries.iter().map(|e| e.name.as_str()).collect::<Vec<_>>(), ["a.txt", "sub"]);
        assert_eq!(entries[0].size, 5);
        assert!(entries[1].is_dir());

        let vol = Volume::open_containing(&dir).unwrap();
        let resolved = vol.path_of(entries[0].file_id).unwrap();
        assert!(resolved.ends_with("a.txt"), "{resolved:?}");

        // Stat by ID agrees with the listing (same shape, same values).
        let st = vol.stat_by_id(entries[0].file_id).unwrap().unwrap();
        assert_eq!((st.name.as_str(), st.size, st.modified), ("a.txt", 5, entries[0].modified));
        assert_eq!(st.attributes, entries[0].attributes);
        assert!(vol.stat_by_id(entries[1].file_id).unwrap().unwrap().is_dir());
        assert_eq!(vol.stat_by_id(FileId(u128::MAX - 7)).unwrap(), None);

        // Open `sub` by ID and list it; then check its attributes via the handle.
        std::fs::write(dir.join("sub").join("b.txt"), b"x").unwrap();
        let sub = vol.open_dir_by_id(entries[1].file_id).unwrap();
        let mut inner = Vec::new();
        list_dir(&sub, &mut buf, &mut inner).unwrap();
        assert_eq!(inner.iter().map(|e| e.name.as_str()).collect::<Vec<_>>(), ["b.txt"]);
        let (attrs, tag) = attribute_tag(&sub).unwrap();
        assert!(attrs & crate::dirent::FILE_ATTRIBUTE_DIRECTORY != 0);
        assert_eq!(tag, 0);
        assert!(vol.open_dir_by_id(FileId(u128::MAX - 7)).is_err());
        drop((h, sub));
        std::fs::remove_dir_all(&dir).unwrap();
    }

    fn id_of(dir: &Path, name: &str) -> FileId {
        let h = open_dir(dir).unwrap();
        let mut buf = vec![0u8; 64 * 1024];
        let mut entries = Vec::new();
        list_dir(&h, &mut buf, &mut entries).unwrap();
        entries.into_iter().find(|e| e.name == name).unwrap().file_id
    }

    #[test]
    fn change_handles_are_exclusive_and_never_replace() {
        let dir = std::env::temp_dir().join(format!("oneup-ntfs-change-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(dir.join("sub")).unwrap();
        std::fs::write(dir.join("a.txt"), b"hello").unwrap();
        std::fs::write(dir.join("b.txt"), b"other").unwrap();
        let vol = Volume::open_containing(&dir).unwrap();
        let a = id_of(&dir, "a.txt");

        // Exclusive: while we hold it, nobody else can open the file...
        let h = vol.open_for_change(a, false).unwrap();
        let other = std::fs::OpenOptions::new().read(true).open(dir.join("a.txt"));
        assert_eq!(other.unwrap_err().raw_os_error(), Some(32), "sharing violation expected");
        // ...and it reads like a stat.
        let st = stat_handle(&h, a).unwrap().unwrap();
        assert_eq!((st.name.as_str(), st.size), ("a.txt", 5));

        // Never replaces: renaming onto b.txt fails and changes nothing.
        let err = rename_handle(&h, &dir.join("b.txt")).unwrap_err();
        assert!(err.is_exists(), "{err}");
        assert_eq!(std::fs::read(dir.join("b.txt")).unwrap(), b"other");

        // A move into a subfolder keeps the ID.
        rename_handle(&h, &dir.join("sub").join("moved.txt")).unwrap();
        drop(h);
        assert_eq!(std::fs::read(dir.join("sub").join("moved.txt")).unwrap(), b"hello");
        assert_eq!(id_of(&dir.join("sub"), "moved.txt"), a);

        // Busy: a file someone else has open can't be opened for change.
        let held = std::fs::File::open(dir.join("b.txt")).unwrap();
        let err = vol.open_for_change(id_of(&dir, "b.txt"), false).unwrap_err();
        assert!(err.is_busy(), "{err}");
        drop(held);

        // Folders can be moved while their children are in use for reading.
        let sub = id_of(&dir, "sub");
        let reader = std::fs::File::open(dir.join("sub").join("moved.txt")).unwrap();
        let h = vol.open_for_change(sub, true).unwrap();
        let r = rename_handle(&h, &dir.join("sub2"));
        drop((h, reader));
        // NTFS refuses to rename a folder with open children unless they
        // were opened with delete sharing; either outcome is safe.
        if r.is_ok() {
            assert!(dir.join("sub2").join("moved.txt").exists());
        }

        // Unknown IDs report "not found".
        assert!(vol.open_for_change(FileId(u128::MAX - 7), false).unwrap_err().is_not_found());
        std::fs::remove_dir_all(&dir).unwrap();
    }
}
