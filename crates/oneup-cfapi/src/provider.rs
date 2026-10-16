//! The provider side of the Cloud Files API: OneUpDrive's own sync root and
//! its placeholders (`docs/architecture/files-on-demand.md`).
//!
//! Binding rules (`data-safety.md` invariants 11–14):
//! * There is no plain dehydrate here. Content is freed only through
//!   [`Exclusive::dehydrate`]: an exclusive (share-none) handle the caller
//!   has used to verify the content, then
//!   `CfUpdatePlaceholder(DEHYDRATE | VERIFY_IN_SYNC)` through that handle.
//! * A [`Fetch`] that is dropped without [`Fetch::complete`] **fails** its
//!   request, so a reader never hangs and never gets zero-filled data.
//! * Callbacks run on Windows' threads. They only hand the request over
//!   ([`Provider`]); the work happens elsewhere.
//! * **Opening a dehydrated placeholder with data access downloads it**,
//!   even without a read and even with `FILE_FLAG_OPEN_NO_RECALL` (measured).
//!   So OneUpDrive opens its dehydrated placeholders with
//!   attribute access only, which Windows doesn't let lock out other
//!   programs. Where that matters (removing one, or changing its version),
//!   [`hold`] makes every fetch of it fail meanwhile: as the provider, we
//!   are the only way its content can arrive, so it can't change under us.

use std::ffi::c_void;
use std::fs::File;
use std::os::windows::ffi::OsStrExt;
use std::os::windows::fs::OpenOptionsExt;
use std::os::windows::io::AsRawHandle;
use std::path::Path;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering::Relaxed};

use windows::Win32::Foundation::{HANDLE, NTSTATUS};
use windows::Win32::Storage::CloudFilters::*;
use windows::Win32::Storage::FileSystem::{FILE_ATTRIBUTE_DIRECTORY, FILE_ATTRIBUTE_NORMAL, FILE_BASIC_INFO};
use windows::Win32::System::CorrelationVector::CORRELATION_VECTOR;
use windows::core::{GUID, HSTRING, PCWSTR};

pub use crate::PROVIDER_NAME;
const PROVIDER_VERSION: &str = env!("CARGO_PKG_VERSION");
/// OneUpDrive's provider ID (stable; not the spike's).
const PROVIDER_ID: GUID = GUID::from_u128(0x1b8e_4c55_0d7a_4f0e_9a31_6c2d_8e74_b5a9);

/// `STATUS_CLOUD_FILE_NOT_IN_SYNC`: what a refused fetch reports.
pub const NOT_IN_SYNC: i32 = 0xC000_CF03_u32 as i32;
/// `STATUS_CLOUD_FILE_UNSUCCESSFUL`.
pub const UNSUCCESSFUL: i32 = 0xC000_CF06_u32 as i32;

#[derive(Debug, thiserror::Error)]
pub enum Error {
    #[error("{what}: {source}")]
    Win { what: &'static str, source: windows::core::Error },
    #[error(transparent)]
    Io(#[from] std::io::Error),
    #[error("not one of our placeholders")]
    NotPlaceholder,
    #[error("the placeholder has content on disk; refusing to change its size or identity without dehydrating")]
    Hydrated,
}

impl Error {
    /// Something of that name already exists (`ERROR_ALREADY_EXISTS`,
    /// `ERROR_FILE_EXISTS`).
    pub fn is_exists(&self) -> bool {
        matches!(self, Error::Win { source, .. } if matches!(source.code().0 as u32 & 0xFFFF, 183 | 80))
    }
}

pub type Result<T> = std::result::Result<T, Error>;

fn win<T>(what: &'static str, r: windows::core::Result<T>) -> Result<T> {
    r.map_err(|source| Error::Win { what, source })
}

fn wide(p: &Path) -> HSTRING {
    HSTRING::from(p.as_os_str())
}

// ------------------------------------------------------------- the sync root

/// Registers `root` as a OneUpDrive sync root: whole-file hydration, folders
/// fully populated by us, in-sync tracked for everything. `identity`
/// identifies the folder pair (its token).
pub fn register(root: &Path, identity: &[u8]) -> Result<()> {
    register_as(root, PROVIDER_NAME, PROVIDER_ID.to_u128(), identity, true)
}

/// Registers `root` as another provider would. Tests use it to stand in for
/// the OneDrive app, to measure what its unlinking leaves behind
/// (`files-on-demand.md` § Migration). `full`: folders are
/// always fully populated (ours); otherwise populated when listed, as the
/// OneDrive app's are.
#[doc(hidden)]
pub fn register_as(root: &Path, provider_name: &str, provider_id: u128, identity: &[u8], full: bool) -> Result<()> {
    let name = HSTRING::from(provider_name);
    let version = HSTRING::from(PROVIDER_VERSION);
    let reg = CF_SYNC_REGISTRATION {
        StructSize: size_of::<CF_SYNC_REGISTRATION>() as u32,
        ProviderName: PCWSTR(name.as_ptr()),
        ProviderVersion: PCWSTR(version.as_ptr()),
        SyncRootIdentity: identity.as_ptr().cast(),
        SyncRootIdentityLength: identity.len() as u32,
        FileIdentity: std::ptr::null(),
        FileIdentityLength: 0,
        ProviderId: GUID::from_u128(provider_id),
    };
    let policies = CF_SYNC_POLICIES {
        StructSize: size_of::<CF_SYNC_POLICIES>() as u32,
        Hydration: CF_HYDRATION_POLICY {
            Primary: CF_HYDRATION_POLICY_FULL,
            Modifier: CF_HYDRATION_POLICY_MODIFIER_NONE,
        },
        Population: CF_POPULATION_POLICY {
            Primary: if full { CF_POPULATION_POLICY_ALWAYS_FULL } else { CF_POPULATION_POLICY_PARTIAL },
            Modifier: CF_POPULATION_POLICY_MODIFIER_NONE,
        },
        InSync: CF_INSYNC_POLICY_TRACK_ALL,
        HardLink: CF_HARDLINK_POLICY_NONE,
        PlaceholderManagement: CF_PLACEHOLDER_MANAGEMENT_POLICY_DEFAULT,
    };
    // SAFETY: every pointer references a local that outlives the call.
    let r = unsafe {
        CfRegisterSyncRoot(&wide(root), &reg, &policies, CF_REGISTER_FLAG_DISABLE_ON_DEMAND_POPULATION_ON_ROOT)
    };
    win("CfRegisterSyncRoot", r)
}

/// Removes the registration. Windows then deletes every placeholder left
/// dehydrated (its content is only in the cloud) and turns the others into
/// ordinary files (measured), so callers hydrate everything the
/// user should keep first (files-on-demand.md § Turning it on and off).
pub fn unregister(root: &Path) -> Result<()> {
    // SAFETY: a NUL-terminated path that outlives the call.
    win("CfUnregisterSyncRoot", unsafe { CfUnregisterSyncRoot(&wide(root)) })
}

/// Who registered the sync root at (or above) `path`, and with what
/// identity; `None` outside any sync root.
pub fn registration(path: &Path) -> Option<(String, Vec<u8>)> {
    let mut buf = vec![0u8; size_of::<CF_SYNC_ROOT_STANDARD_INFO>() + 4096];
    let mut got = 0u32;
    // SAFETY: `buf` is writable for its full length.
    let r = unsafe {
        CfGetSyncRootInfoByPath(
            &wide(path),
            CF_SYNC_ROOT_INFO_STANDARD,
            buf.as_mut_ptr().cast(),
            buf.len() as u32,
            Some(&mut got),
        )
    };
    r.ok()?;
    // SAFETY: the buffer begins with the structure, which may be unaligned.
    let info: CF_SYNC_ROOT_STANDARD_INFO = unsafe { std::ptr::read_unaligned(buf.as_ptr().cast()) };
    let name = String::from_utf16_lossy(
        &info.ProviderName[..info.ProviderName.iter().position(|&c| c == 0).unwrap_or(info.ProviderName.len())],
    );
    let start = std::mem::offset_of!(CF_SYNC_ROOT_STANDARD_INFO, SyncRootIdentity);
    let len = info.SyncRootIdentityLength as usize;
    let identity = buf.get(start..start + len)?.to_vec();
    Some((name, identity))
}

// --------------------------------------------------------------------- holds

/// Identities whose fetches are refused for now (see [`hold`]).
static HELD: std::sync::Mutex<Vec<Vec<u8>>> = std::sync::Mutex::new(Vec::new());

/// While the returned guard lives, every `FETCH_DATA` for a placeholder
/// with this identity fails at once, before any [`Provider`] sees it: its
/// content can't arrive, so a dehydrated placeholder stays dehydrated.
pub fn hold(identity: &[u8]) -> Hold {
    HELD.lock().unwrap_or_else(|p| p.into_inner()).push(identity.to_vec());
    Hold(identity.to_vec())
}

/// See [`hold`].
pub struct Hold(Vec<u8>);

impl Drop for Hold {
    fn drop(&mut self) {
        let mut held = HELD.lock().unwrap_or_else(|p| p.into_inner());
        if let Some(i) = held.iter().position(|h| *h == self.0) {
            held.swap_remove(i);
        }
    }
}

fn is_held(identity: &[u8]) -> bool {
    HELD.lock().unwrap_or_else(|p| p.into_inner()).iter().any(|h| h == identity)
}

// ----------------------------------------------------------------- callbacks

/// The requesting program, from the callback.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Process {
    pub pid: u32,
    /// Its executable's path.
    pub image: String,
}

/// One `FETCH_DATA` request: a reader wants this placeholder's content.
pub struct Fetch {
    key: CF_CONNECTION_KEY,
    transfer_key: i64,
    request_key: i64,
    cv: CORRELATION_VECTOR,
    pub identity: Vec<u8>,
    /// The placeholder's size.
    pub file_size: i64,
    /// What the reader needs (the whole file is transferred anyway).
    pub offset: i64,
    pub length: i64,
    /// The placeholder's path within the volume (as Windows reports it).
    pub path: String,
    /// The placeholder's NTFS file ID (64-bit; Cloud Files runs on NTFS).
    pub file_id: i64,
    pub process: Option<Process>,
    done: AtomicBool,
}

// SAFETY: the connection key and transfer keys are plain integers that
// Cloud Files accepts from any thread; the correlation vector is a copy.
unsafe impl Send for Fetch {}

impl Fetch {
    fn op(&self, ty: CF_OPERATION_TYPE) -> CF_OPERATION_INFO {
        CF_OPERATION_INFO {
            StructSize: size_of::<CF_OPERATION_INFO>() as u32,
            Type: ty,
            ConnectionKey: self.key,
            TransferKey: self.transfer_key,
            CorrelationVector: &self.cv,
            SyncStatus: std::ptr::null(),
            RequestKey: self.request_key,
        }
    }

    /// A transfer of `data` at `offset`, or (with `data` `None`) the
    /// failure of the requested range with `status`.
    fn execute(&self, data: Option<&[u8]>, offset: i64, status: NTSTATUS) -> Result<()> {
        let op = self.op(CF_OPERATION_TYPE_TRANSFER_DATA);
        let (buffer, length) = match data {
            Some(d) => (d.as_ptr().cast::<c_void>(), d.len() as i64),
            // A failure names the range it fails, with no buffer; a zero
            // length leaves the reader waiting for the rest.
            None => (std::ptr::null(), self.length),
        };
        let mut params = CF_OPERATION_PARAMETERS {
            ParamSize: size_of::<CF_OPERATION_PARAMETERS>() as u32,
            Anonymous: CF_OPERATION_PARAMETERS_0 {
                TransferData: CF_OPERATION_PARAMETERS_0_0 {
                    Flags: CF_OPERATION_TRANSFER_DATA_FLAG_NONE,
                    CompletionStatus: status,
                    Buffer: buffer,
                    Offset: offset,
                    Length: length,
                },
            },
        };
        // SAFETY: `op` and `params` point at locals and `data`, all of which
        // outlive this synchronous call.
        win("CfExecute(TRANSFER_DATA)", unsafe { CfExecute(&op, &mut params) })
    }

    /// Sends bytes `offset..offset + data.len()` of the file. Offsets and
    /// lengths are multiples of 4 KiB, except for a chunk ending at the end
    /// of the file.
    pub fn transfer(&self, offset: i64, data: &[u8]) -> Result<()> {
        self.execute(Some(data), offset, NTSTATUS(0))
    }

    /// Identifies this transfer (as a later [`Cancel`] does).
    pub fn transfer_key(&self) -> i64 {
        self.transfer_key
    }

    /// Tells Windows how far the download is (for Explorer's progress).
    pub fn progress(&self, total: i64, completed: i64) {
        // SAFETY: plain integers; a failure only costs the progress display.
        let _ = unsafe { CfReportProviderProgress(self.key, self.transfer_key, total, completed) };
    }

    /// Every byte was transferred: the request is done.
    pub fn complete(self) {
        self.done.store(true, Relaxed);
    }

    /// Refuses the request with `status` (e.g. [`NOT_IN_SYNC`]): the reader
    /// gets an error, and nothing is written into the file.
    pub fn fail(self, status: i32) {
        let _ = self.execute(None, self.offset, NTSTATUS(status));
        self.done.store(true, Relaxed);
    }
}

impl Drop for Fetch {
    fn drop(&mut self) {
        if !self.done.load(Relaxed) {
            let _ = self.execute(None, self.offset, NTSTATUS(UNSUCCESSFUL));
        }
    }
}

/// A `CANCEL_FETCH_DATA`: the reader gave up.
#[derive(Debug, Clone)]
pub struct Cancel {
    pub transfer_key: i64,
    pub path: String,
}

/// What the provider does with requests. Implementations must return
/// quickly: do the work elsewhere and complete the [`Fetch`] from there.
pub trait Provider: Send + Sync {
    fn fetch_data(&self, fetch: Fetch);
    fn cancel_fetch(&self, _cancel: Cancel) {}
}

/// A live connection; dropping it disconnects.
pub struct Connection {
    key: CF_CONNECTION_KEY,
    context: *mut Arc<dyn Provider>,
}

// SAFETY: the key is an integer handle valid on any thread; the context box
// is only touched again in `drop`, after Windows stopped calling back.
unsafe impl Send for Connection {}
// SAFETY: as above; `&Connection` exposes nothing mutable.
unsafe impl Sync for Connection {}

impl Drop for Connection {
    fn drop(&mut self) {
        // SAFETY: the key came from a successful CfConnectSyncRoot; after
        // the disconnect returns, no callback can see the context, so the
        // box is freed exactly once.
        unsafe {
            let _ = CfDisconnectSyncRoot(self.key);
            drop(Box::from_raw(self.context));
        }
    }
}

/// Connects `provider` to the sync root at `root`.
pub fn connect(root: &Path, provider: Arc<dyn Provider>) -> Result<Connection> {
    let table = [
        CF_CALLBACK_REGISTRATION { Type: CF_CALLBACK_TYPE_FETCH_DATA, Callback: Some(on_fetch_data) },
        CF_CALLBACK_REGISTRATION { Type: CF_CALLBACK_TYPE_CANCEL_FETCH_DATA, Callback: Some(on_cancel_fetch_data) },
        // Who closed a placeholder, for the program tracker (`closes`).
        CF_CALLBACK_REGISTRATION {
            Type: CF_CALLBACK_TYPE_NOTIFY_FILE_CLOSE_COMPLETION,
            Callback: Some(on_close_completion),
        },
        CF_CALLBACK_REGISTRATION { Type: CF_CALLBACK_TYPE_NONE, Callback: None },
    ];
    let context = Box::into_raw(Box::new(provider));
    // SAFETY: the table ends with CF_CALLBACK_TYPE_NONE and the callbacks are
    // `extern "system"`; the context outlives the connection (freed in drop).
    let key = unsafe {
        CfConnectSyncRoot(
            &wide(root),
            table.as_ptr(),
            Some(context.cast_const().cast()),
            CF_CONNECT_FLAG_REQUIRE_PROCESS_INFO | CF_CONNECT_FLAG_REQUIRE_FULL_FILE_PATH,
        )
    };
    match key {
        Ok(key) => Ok(Connection { key, context }),
        Err(source) => {
            // SAFETY: never handed to Windows successfully: ours to free.
            drop(unsafe { Box::from_raw(context) });
            Err(Error::Win { what: "CfConnectSyncRoot", source })
        }
    }
}

/// # Safety
/// `info` must be a valid callback info whose context is a live
/// `Arc<dyn Provider>` box (set up by [`connect`]).
unsafe fn provider_of(info: &CF_CALLBACK_INFO) -> Option<&Arc<dyn Provider>> {
    // SAFETY: per the function's contract.
    unsafe { info.CallbackContext.cast::<Arc<dyn Provider>>().as_ref() }
}

fn pcwstr(p: PCWSTR) -> String {
    if p.is_null() {
        return String::new();
    }
    // SAFETY: Cloud Files strings in callback info are NUL-terminated.
    unsafe { p.to_string() }.unwrap_or_default()
}

unsafe extern "system" fn on_fetch_data(info: *const CF_CALLBACK_INFO, params: *const CF_CALLBACK_PARAMETERS) {
    let _ = std::panic::catch_unwind(|| {
        // SAFETY: Windows passes valid pointers for the callback's duration;
        // for FETCH_DATA, `FetchData` is the active union member.
        let (info, fd) = unsafe { (&*info, (*params).Anonymous.FetchData) };
        // SAFETY: the context was set by `connect`.
        let Some(provider) = (unsafe { provider_of(info) }) else { return };
        let identity = if info.FileIdentity.is_null() {
            Vec::new()
        } else {
            // SAFETY: FileIdentity points at FileIdentityLength bytes.
            unsafe { std::slice::from_raw_parts(info.FileIdentity.cast::<u8>(), info.FileIdentityLength as usize) }
                .to_vec()
        };
        let process = (!info.ProcessInfo.is_null()).then(|| {
            // SAFETY: a non-null ProcessInfo is valid during the callback.
            let p = unsafe { &*info.ProcessInfo };
            Process { pid: p.ProcessId, image: pcwstr(p.ImagePath) }
        });
        let cv = if info.CorrelationVector.is_null() {
            CORRELATION_VECTOR::default()
        } else {
            // SAFETY: valid during the callback; copied out.
            unsafe { *info.CorrelationVector }
        };
        let fetch = Fetch {
            key: info.ConnectionKey,
            transfer_key: info.TransferKey,
            request_key: info.RequestKey,
            cv,
            identity,
            file_size: info.FileSize,
            offset: fd.RequiredFileOffset,
            length: fd.RequiredLength,
            path: pcwstr(info.NormalizedPath),
            file_id: info.FileId,
            process,
            done: AtomicBool::new(false),
        };
        if is_held(&fetch.identity) {
            fetch.fail(NOT_IN_SYNC);
            return;
        }
        provider.fetch_data(fetch);
    });
}

unsafe extern "system" fn on_cancel_fetch_data(info: *const CF_CALLBACK_INFO, _params: *const CF_CALLBACK_PARAMETERS) {
    let _ = std::panic::catch_unwind(|| {
        // SAFETY: as in `on_fetch_data`.
        let info = unsafe { &*info };
        // SAFETY: the context was set by `connect`.
        let Some(provider) = (unsafe { provider_of(info) }) else { return };
        provider.cancel_fetch(Cancel { transfer_key: info.TransferKey, path: pcwstr(info.NormalizedPath) });
    });
}

/// A notification only: Windows doesn't wait for an answer. Notes the
/// closing program, except this process (our own writes are `ours`).
unsafe extern "system" fn on_close_completion(info: *const CF_CALLBACK_INFO, _params: *const CF_CALLBACK_PARAMETERS) {
    let _ = std::panic::catch_unwind(|| {
        // SAFETY: Windows passes a valid pointer for the callback's duration.
        let info = unsafe { &*info };
        if info.ProcessInfo.is_null() {
            return;
        }
        // SAFETY: a non-null ProcessInfo is valid during the callback.
        let p = unsafe { &*info.ProcessInfo };
        if p.ProcessId != std::process::id() {
            crate::closes::note(u128::from(info.FileId as u64), p.ProcessId, &pcwstr(p.ImagePath));
        }
    });
}

// -------------------------------------------------------------- placeholders

/// A placeholder to create.
#[derive(Debug, Clone)]
pub struct NewPlaceholder<'a> {
    pub name: &'a str,
    pub identity: &'a [u8],
    pub size: i64,
    /// Last-write time as a Windows FILETIME (100 ns since 1601).
    pub modified: i64,
    pub folder: bool,
}

/// Creates dehydrated placeholders in `dir`, marked in sync. Folders are
/// created fully populated (we create their contents ourselves). Returns how
/// many were created; one that already exists fails the call.
pub fn create(dir: &Path, items: &[NewPlaceholder<'_>]) -> Result<u32> {
    let names: Vec<Vec<u16>> =
        items.iter().map(|i| std::ffi::OsStr::new(i.name).encode_wide().chain(Some(0)).collect()).collect();
    let mut infos: Vec<CF_PLACEHOLDER_CREATE_INFO> = items
        .iter()
        .zip(&names)
        .map(|(i, name)| {
            let mut flags = CF_PLACEHOLDER_CREATE_FLAG_MARK_IN_SYNC;
            if i.folder {
                flags |= CF_PLACEHOLDER_CREATE_FLAG_DISABLE_ON_DEMAND_POPULATION;
            }
            CF_PLACEHOLDER_CREATE_INFO {
                RelativeFileName: PCWSTR(name.as_ptr()),
                FsMetadata: metadata(i.size, i.modified, i.folder),
                FileIdentity: i.identity.as_ptr().cast(),
                FileIdentityLength: i.identity.len() as u32,
                Flags: flags,
                ..Default::default()
            }
        })
        .collect();
    let mut done = 0u32;
    // SAFETY: `infos` points into `names` and `items`, all alive across the
    // synchronous call.
    let r = unsafe { CfCreatePlaceholders(&wide(dir), &mut infos, CF_CREATE_FLAG_NONE, Some(&mut done)) };
    win("CfCreatePlaceholders", r)?;
    Ok(done)
}

/// Creates a folder placeholder whose contents are still in the cloud
/// (unpopulated), as other providers leave them. Tests only: OneUpDrive's
/// own folders are always fully populated.
#[doc(hidden)]
pub fn create_unpopulated_folder(dir: &Path, name: &str, identity: &[u8]) -> Result<()> {
    let wname: Vec<u16> = std::ffi::OsStr::new(name).encode_wide().chain(Some(0)).collect();
    let mut infos = [CF_PLACEHOLDER_CREATE_INFO {
        RelativeFileName: PCWSTR(wname.as_ptr()),
        FsMetadata: metadata(0, 0, true),
        FileIdentity: identity.as_ptr().cast(),
        FileIdentityLength: identity.len() as u32,
        Flags: CF_PLACEHOLDER_CREATE_FLAG_MARK_IN_SYNC,
        ..Default::default()
    }];
    let mut done = 0u32;
    // SAFETY: `infos` points into `wname` and `identity`, alive across the call.
    let r = unsafe { CfCreatePlaceholders(&wide(dir), &mut infos, CF_CREATE_FLAG_NONE, Some(&mut done)) };
    win("CfCreatePlaceholders", r)
}

fn metadata(size: i64, modified: i64, folder: bool) -> CF_FS_METADATA {
    CF_FS_METADATA {
        BasicInfo: FILE_BASIC_INFO {
            CreationTime: modified,
            LastAccessTime: modified,
            LastWriteTime: modified,
            ChangeTime: modified,
            FileAttributes: if folder { FILE_ATTRIBUTE_DIRECTORY.0 } else { FILE_ATTRIBUTE_NORMAL.0 },
        },
        FileSize: if folder { 0 } else { size },
    }
}

/// "Always keep on this device" and "Free up space", as Windows stores them.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Pin {
    Unspecified,
    Pinned,
    Unpinned,
    Excluded,
    Inherit,
}

impl Pin {
    fn from_raw(p: CF_PIN_STATE) -> Self {
        match p {
            CF_PIN_STATE_PINNED => Pin::Pinned,
            CF_PIN_STATE_UNPINNED => Pin::Unpinned,
            CF_PIN_STATE_EXCLUDED => Pin::Excluded,
            CF_PIN_STATE_INHERIT => Pin::Inherit,
            _ => Pin::Unspecified,
        }
    }

    fn raw(self) -> CF_PIN_STATE {
        match self {
            Pin::Unspecified => CF_PIN_STATE_UNSPECIFIED,
            Pin::Pinned => CF_PIN_STATE_PINNED,
            Pin::Unpinned => CF_PIN_STATE_UNPINNED,
            Pin::Excluded => CF_PIN_STATE_EXCLUDED,
            Pin::Inherit => CF_PIN_STATE_INHERIT,
        }
    }
}

/// A placeholder's state, from its handle.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Info {
    pub in_sync: bool,
    pub pin: Pin,
    /// Bytes of content on disk (0: dehydrated).
    pub on_disk: i64,
    /// Of those, bytes written here since the last sync: Windows keeps them
    /// even through a dehydration (see [`Exclusive::revalidate`]).
    pub modified: i64,
    pub identity: Vec<u8>,
}

fn handle(f: &File) -> HANDLE {
    HANDLE(f.as_raw_handle())
}

/// Opens `path` for reading its placeholder state (attributes only: opening
/// never hydrates).
pub fn open(path: &Path) -> Result<File> {
    use windows::Win32::Storage::FileSystem::{
        FILE_FLAG_BACKUP_SEMANTICS, FILE_READ_ATTRIBUTES, FILE_SHARE_DELETE, FILE_SHARE_READ, FILE_SHARE_WRITE,
        FILE_WRITE_ATTRIBUTES,
    };
    Ok(std::fs::OpenOptions::new()
        .access_mode(FILE_READ_ATTRIBUTES.0 | FILE_WRITE_ATTRIBUTES.0)
        .share_mode((FILE_SHARE_READ | FILE_SHARE_WRITE | FILE_SHARE_DELETE).0)
        .custom_flags(FILE_FLAG_BACKUP_SEMANTICS.0)
        .open(path)?)
}

/// The placeholder state of an open file; `None` only if Windows says it
/// isn't a cloud file at all. Any other failure is an error, never "not a
/// placeholder": an online-only placeholder no sync root holds any more
/// isn't described (`ERROR_CLOUD_FILE_NOT_UNDER_SYNC_ROOT`, measured), and
/// taking it for an ordinary file would take it for all its content.
pub fn info(f: &File) -> Result<Option<Info>> {
    let mut buf = vec![0u8; size_of::<CF_PLACEHOLDER_STANDARD_INFO>() + 4096];
    let mut got = 0u32;
    // SAFETY: `buf` is writable for its full length.
    let r = unsafe {
        CfGetPlaceholderInfo(
            handle(f),
            CF_PLACEHOLDER_INFO_STANDARD,
            buf.as_mut_ptr().cast(),
            buf.len() as u32,
            Some(&mut got),
        )
    };
    match r {
        Err(e) if is_not_a_cloud_file(e.code()) => return Ok(None),
        r => win("CfGetPlaceholderInfo", r)?,
    }
    // SAFETY: the buffer begins with the structure, which may be unaligned.
    let s: CF_PLACEHOLDER_STANDARD_INFO = unsafe { std::ptr::read_unaligned(buf.as_ptr().cast()) };
    let start = std::mem::offset_of!(CF_PLACEHOLDER_STANDARD_INFO, FileIdentity);
    let identity = buf.get(start..start + s.FileIdentityLength as usize).map(<[u8]>::to_vec).unwrap_or_default();
    Ok(Some(Info {
        in_sync: s.InSyncState == CF_IN_SYNC_STATE_IN_SYNC,
        pin: Pin::from_raw(s.PinState),
        on_disk: s.OnDiskDataSize,
        modified: s.ModifiedDataSize,
        identity,
    }))
}

/// What `CfGetPlaceholderInfo` reports for an ordinary file or folder, in a
/// sync root or not (measured): the one failure that means "not a
/// placeholder".
fn is_not_a_cloud_file(code: windows::core::HRESULT) -> bool {
    code == windows::Win32::Foundation::ERROR_NOT_A_CLOUD_FILE.to_hresult()
}

/// Gives a **dehydrated** placeholder a new identity, size and time (a cloud
/// edit), keeping it dehydrated and in sync. Refused if it has content on
/// disk: changing a hydrated file's size would corrupt it; those go through
/// [`Exclusive::dehydrate`] with the new identity.
pub fn update_dehydrated(f: &File, identity: &[u8], size: i64, modified: i64) -> Result<()> {
    let i = info(f)?.ok_or(Error::NotPlaceholder)?;
    if i.on_disk != 0 {
        return Err(Error::Hydrated);
    }
    let md = metadata(size, modified, false);
    // SAFETY: `md` and `identity` outlive the synchronous call.
    let r = unsafe {
        CfUpdatePlaceholder(
            handle(f),
            Some(&md),
            Some(identity.as_ptr().cast()),
            identity.len() as u32,
            None,
            // VERIFY_IN_SYNC as well: should it have been hydrated and edited
            // since the check above, the edit is never dehydrated away.
            CF_UPDATE_FLAG_MARK_IN_SYNC | CF_UPDATE_FLAG_DEHYDRATE | CF_UPDATE_FLAG_VERIFY_IN_SYNC,
            None,
            None,
        )
    };
    win("CfUpdatePlaceholder", r)
}

/// Marks the placeholder in sync without touching its content or identity:
/// after one of our own changes (a rename clears the bit), and only where
/// its content can't have changed meanwhile (dehydrated under a [`hold`],
/// or through a handle no one else can have open).
pub fn set_in_sync(f: &File) -> Result<()> {
    // SAFETY: a valid handle; no USN precondition.
    win("CfSetInSyncState", unsafe {
        CfSetInSyncState(handle(f), CF_IN_SYNC_STATE_IN_SYNC, CF_SET_IN_SYNC_FLAG_NONE, None)
    })
}

/// Sets the pin state of a folder and everything in it.
pub fn set_pin_tree(f: &File, pin: Pin) -> Result<()> {
    // SAFETY: a valid handle; synchronous.
    win("CfSetPinState", unsafe { CfSetPinState(handle(f), pin.raw(), CF_SET_PIN_FLAG_RECURSE, None) })
}

/// Sets the pin state ("Always keep on this device" / "Free up space").
pub fn set_pin(f: &File, pin: Pin) -> Result<()> {
    // SAFETY: a valid handle; synchronous.
    win("CfSetPinState", unsafe { CfSetPinState(handle(f), pin.raw(), CF_SET_PIN_FLAG_NONE, None) })
}

/// Downloads the whole content (through our own `FETCH_DATA`). Blocks until
/// done: never call it from a callback, or from the thread that serves them.
pub fn hydrate(f: &File) -> Result<()> {
    // SAFETY: a valid handle; whole file; synchronous.
    win("CfHydratePlaceholder", unsafe { CfHydratePlaceholder(handle(f), 0, -1, CF_HYDRATE_FLAG_NONE, None) })
}

/// Turns a hydrated placeholder back into an ordinary file (turning
/// on-demand off). Refused for one without all its content on disk.
pub fn revert(f: &File, size: i64) -> Result<()> {
    let i = info(f)?.ok_or(Error::NotPlaceholder)?;
    if i.on_disk < size {
        return Err(Error::Hydrated);
    }
    // SAFETY: a valid handle; synchronous.
    win("CfRevertPlaceholder", unsafe { CfRevertPlaceholder(handle(f), CF_REVERT_FLAG_NONE, None) })
}

/// Makes a placeholder no sync root holds any more (one another sync app
/// left behind) an ordinary file or folder with what it has here. Unlike
/// [`revert`], it doesn't ask [`info`] first: Windows won't describe such a
/// placeholder (measured). The caller checked it has all its
/// content here (or is a folder).
pub fn revert_orphan(f: &File) -> Result<()> {
    // SAFETY: a valid handle; synchronous.
    win("CfRevertPlaceholder", unsafe { CfRevertPlaceholder(handle(f), CF_REVERT_FLAG_NONE, None) })
}

/// A file opened so no other program can open it meanwhile (share none):
/// what every change of placeholder content goes through, after the caller
/// verified the content through it (invariants 5 and 11).
pub struct Exclusive(File);

impl Exclusive {
    /// Fails with a sharing violation while any other program has it open.
    pub fn open(path: &Path) -> Result<Self> {
        use windows::Win32::Foundation::GENERIC_READ;
        use windows::Win32::Storage::FileSystem::{FILE_FLAG_BACKUP_SEMANTICS, FILE_WRITE_ATTRIBUTES};
        Ok(Self(
            std::fs::OpenOptions::new()
                .access_mode(GENERIC_READ.0 | FILE_WRITE_ATTRIBUTES.0)
                .share_mode(0)
                .custom_flags(FILE_FLAG_BACKUP_SEMANTICS.0)
                .open(path)?,
        ))
    }

    /// Wraps a file the caller opened **share-none** (e.g. by file ID, to
    /// prove it's the planned item) with attribute-write access.
    pub fn from_file(f: File) -> Self {
        Self(f)
    }

    /// The file, for verifying it (reading a dehydrated placeholder would
    /// hydrate it: check [`info`] first).
    pub fn file(&self) -> &File {
        &self.0
    }

    /// Frees the content (invariant 11): only if Windows still considers it
    /// in sync (`VERIFY_IN_SYNC`, which an edit by any other program clears),
    /// and through this handle, which no one else can have open. With `new`,
    /// the placeholder also takes a new identity, size and time (a cloud
    /// edit) and stays in sync.
    pub fn dehydrate(&self, new: Option<(&[u8], i64, i64)>) -> Result<()> {
        let md = new.map(|(_, size, modified)| metadata(size, modified, false));
        let (id, len) = new.map_or((None, 0), |(id, _, _)| (Some(id.as_ptr().cast::<c_void>()), id.len() as u32));
        let mut flags = CF_UPDATE_FLAG_DEHYDRATE | CF_UPDATE_FLAG_VERIFY_IN_SYNC;
        if new.is_some() {
            flags |= CF_UPDATE_FLAG_MARK_IN_SYNC;
        }
        // SAFETY: `md` and the identity bytes outlive the synchronous call.
        let r = unsafe {
            CfUpdatePlaceholder(handle(&self.0), md.as_ref().map(std::ptr::from_ref), id, len, None, flags, None, None)
        };
        win("CfUpdatePlaceholder(DEHYDRATE | VERIFY_IN_SYNC)", r)
    }

    /// Marks the content in sync with a new identity (after its upload was
    /// verified), without touching the content.
    pub fn mark_in_sync(&self, identity: &[u8]) -> Result<()> {
        // SAFETY: the identity outlives the synchronous call.
        let r = unsafe {
            CfUpdatePlaceholder(
                handle(&self.0),
                None,
                Some(identity.as_ptr().cast()),
                identity.len() as u32,
                None,
                CF_UPDATE_FLAG_MARK_IN_SYNC,
                None,
                None,
            )
        };
        win("CfUpdatePlaceholder(MARK_IN_SYNC)", r)
    }

    /// After an edit made here was sent: makes the whole content the synced
    /// version with `identity`. Marking it in sync isn't enough, because
    /// bytes a program wrote stay "modified" and survive a dehydration
    /// (measured); reverting to an ordinary file and converting again,
    /// through this one handle, validates them. Needs all `size` bytes here.
    pub fn revalidate(&self, identity: &[u8], size: i64) -> Result<()> {
        revert(&self.0, size)?;
        self.convert(identity)
    }

    /// Turns an ordinary file into a placeholder that keeps its content and
    /// is in sync (its content was verified to be `identity`'s version).
    pub fn convert(&self, identity: &[u8]) -> Result<()> {
        // SAFETY: the identity outlives the synchronous call.
        let r = unsafe {
            CfConvertToPlaceholder(
                handle(&self.0),
                Some(identity.as_ptr().cast()),
                identity.len() as u32,
                CF_CONVERT_FLAG_MARK_IN_SYNC,
                None,
                None,
            )
        };
        win("CfConvertToPlaceholder", r)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn only_not_a_cloud_file_means_not_a_placeholder() {
        use windows::Win32::Foundation::{
            ERROR_ACCESS_DENIED, ERROR_CLOUD_FILE_NOT_UNDER_SYNC_ROOT, ERROR_CLOUD_FILE_PROVIDER_NOT_RUNNING,
            ERROR_NOT_A_CLOUD_FILE,
        };
        assert_eq!(ERROR_NOT_A_CLOUD_FILE.to_hresult().0 as u32, 0x8007_0178);
        assert!(is_not_a_cloud_file(ERROR_NOT_A_CLOUD_FILE.to_hresult()));
        // An orphaned online-only placeholder, and any other failure.
        for e in [ERROR_CLOUD_FILE_NOT_UNDER_SYNC_ROOT, ERROR_CLOUD_FILE_PROVIDER_NOT_RUNNING, ERROR_ACCESS_DENIED] {
            assert!(!is_not_a_cloud_file(e.to_hresult()), "{e:?}");
        }
    }
}
