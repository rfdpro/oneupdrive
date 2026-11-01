//! Reading a file without getting in another program's way.
//!
//! A file is read through one handle holding a Read-Handle oplock
//! (`OPLOCK_LEVEL_CACHE_READ | OPLOCK_LEVEL_CACHE_HANDLE`). Windows breaks
//! it when another handle writes the file, changes its size or maps it for
//! writing (the writer doesn't wait), and when another program needs every
//! handle on the file closed: a save that renames another file over it
//! (`MoveFileEx`, `cmd`'s `move /y`, POSIX renames too), a delete, an open
//! that doesn't share reading, a rename of a folder above it. That program
//! then waits until we close the handle, and goes ahead. While we hold any
//! other handle on the file, a `MoveFileEx` save fails with "access
//! denied" instead (measured, `a_rename_over_save_and_the_handles_we_hold`),
//! so the file is opened and its oplock requested as one step
//! (`FILE_FLAG_OPEN_REQUIRING_OPLOCK`), and the reads go through that
//! handle.
//!
//! A break is acknowledged by closing the handle, never by the reader,
//! which may be busy elsewhere. One thread of its own (the acknowledger)
//! takes the breaks off a completion port the handles are bound to, which
//! also makes the oplock requests outlive the threads that made them:
//! Windows cancels a thread's pending requests when it ends, but not those
//! on a handle bound to a port (measured). The acknowledger never waits for
//! a file: if the file's handle is in use (a read being started, a metadata
//! query), a thread of the break's own closes it once it's free.
//!
//! Ownership: each oplock request is owned by a process-wide table under
//! its handle's completion key, which is never reused, until its own
//! completion (that key *and* its `OVERLAPPED`) comes off the port, or
//! until it's certain none will come. A packet that matches nothing (I/O
//! other code made through a bound handle, or anything stale) is ignored.

use std::cell::UnsafeCell;
use std::collections::HashMap;
use std::ffi::c_void;
use std::path::Path;
use std::ptr::NonNull;
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use std::sync::mpsc;
use std::sync::{Arc, Mutex, MutexGuard, OnceLock, PoisonError};
use std::time::Duration;

use windows::Wdk::Storage::FileSystem::NtFsControlFile;
use windows::Win32::Foundation::{
    ERROR_IO_PENDING, ERROR_OPLOCK_NOT_GRANTED, HANDLE, NTSTATUS, RtlNtStatusToDosError, STATUS_END_OF_FILE,
    STATUS_PENDING, STATUS_SUCCESS, WIN32_ERROR,
};
use windows::Win32::Storage::FileSystem::{
    FILE_FLAG_BACKUP_SEMANTICS, FILE_FLAG_OPEN_REPARSE_POINT, FILE_FLAG_OVERLAPPED, FILE_FLAGS_AND_ATTRIBUTES,
    FILE_READ_ATTRIBUTES, FILE_READ_DATA, FILE_SHARE_DELETE, FILE_SHARE_READ, FILE_SHARE_WRITE, ReadFile,
    SetFileCompletionNotificationModes,
};
use windows::Win32::System::IO::{
    CancelIoEx, CreateIoCompletionPort, GetQueuedCompletionStatus, IO_STATUS_BLOCK, OVERLAPPED, OVERLAPPED_0,
    OVERLAPPED_0_0, PostQueuedCompletionStatus,
};
use windows::Win32::System::Ioctl::{
    FSCTL_REQUEST_OPLOCK, OPLOCK_LEVEL_CACHE_HANDLE, OPLOCK_LEVEL_CACHE_READ, REQUEST_OPLOCK_CURRENT_VERSION,
    REQUEST_OPLOCK_INPUT_BUFFER, REQUEST_OPLOCK_INPUT_FLAG_REQUEST, REQUEST_OPLOCK_OUTPUT_BUFFER,
};
use windows::Win32::System::Threading::{CreateEventW, INFINITE, WaitForSingleObject};

use super::{Error, Handle, Result, win};

/// The most one read asks for.
const CHUNK: usize = 1 << 20;

/// `CreateFile` flag: open and request an oplock as one step. Other opens
/// that would break the oplock wait until it's requested; our open fails
/// (`ERROR_CANNOT_BREAK_OPLOCK`) if it would break another's.
const FILE_FLAG_OPEN_REQUIRING_OPLOCK: u32 = 0x0004_0000;

/// `SetFileCompletionNotificationModes` flag: a request that succeeds at
/// once queues nothing on the port.
const FILE_SKIP_COMPLETION_PORT_ON_SUCCESS: u8 = 1;

/// The completion key of a reader waiting for the acknowledger to catch up
/// ([`WatchedFile::broken`]). Handles are bound with keys from
/// [`NEXT_KEY`], above it.
const CAUGHT_UP: usize = 1;
static NEXT_KEY: AtomicUsize = AtomicUsize::new(16);

/// How long a reader waits for the acknowledger to catch up before it
/// takes the file as changed.
const CATCH_UP_WAIT: Duration = Duration::from_secs(10);

/// A file opened for reading ([`super::Volume::open_watched`]) that other
/// programs may still write, replace, rename and delete. Instead of
/// keeping them out, the reader checks [`WatchedFile::broken`] after
/// reading and drops what it read if the file was written (or another
/// program needed it closed) since it was opened.
///
/// Fully shared and read-only. Once broken, the handle is closed and every
/// read fails.
pub struct WatchedFile {
    shared: Arc<Shared>,
    /// Its handle's completion key.
    #[cfg_attr(not(test), allow(dead_code))]
    key: usize,
    /// Where the next read starts: reads give their offset (the handle is
    /// asynchronous, so it keeps no position).
    pos: u64,
    /// Signalled when this reader's read finishes.
    event: Handle,
}

/// What the reader and the acknowledger share.
struct Shared {
    /// `None` once closed. Held only for calls that don't wait: starting a
    /// read (never waiting for it), metadata queries, closing.
    handle: Mutex<Option<Handle>>,
    /// Set by the acknowledger as it takes the break off the port.
    broken: AtomicBool,
}

// SAFETY: kernel handles may be used from any thread, and the mutex
// serialises every use of the file's.
unsafe impl Send for Shared {}
// SAFETY: as above.
unsafe impl Sync for Shared {}

// SAFETY: the event is used only by `read` (`&mut self`); the rest is `Shared`.
unsafe impl Send for WatchedFile {}
// SAFETY: `&self` methods only go through `Shared`.
unsafe impl Sync for WatchedFile {}

impl Shared {
    fn lock(&self) -> MutexGuard<'_, Option<Handle>> {
        self.handle.lock().unwrap_or_else(PoisonError::into_inner)
    }

    /// Closes the handle (once): that acknowledges a break, and a program
    /// waiting for it goes ahead. Closing also ends a pending oplock
    /// request (its completion still comes). `cancel_reads`: a read may be
    /// under way, so it's cancelled first; never otherwise, as cancelling
    /// would also end a pending oplock request and leave a plain handle
    /// open until it's closed, which a `MoveFileEx` save fails against
    /// (measured, `saves_racing_opens_never_fail`).
    fn close(guard: &mut MutexGuard<'_, Option<Handle>>, cancel_reads: bool) {
        if let Some(h) = guard.take() {
            if cancel_reads {
                // SAFETY: cancels I/O on our own handle, still open: a
                // read under way ends, and its reader learns that from its
                // own event (its buffers stay with it until then). The
                // oplock request has finished already (it broke).
                unsafe {
                    let _ = CancelIoEx(h.raw(), None);
                }
            }
            drop(h);
        }
    }

    /// Acknowledges a break without waiting: closes the handle now if it's
    /// free, else on a thread of its own once it is.
    fn acknowledge(self: Arc<Self>) {
        self.broken.store(true, Ordering::SeqCst);
        if let Ok(mut guard) = self.handle.try_lock() {
            Self::close(&mut guard, true);
            return;
        }
        let me = Arc::clone(&self);
        let spawned = std::thread::Builder::new().name("oneup-oplock-close".into()).spawn(move || {
            Self::close(&mut me.lock(), true);
        });
        if spawned.is_err() {
            Self::close(&mut self.lock(), true);
        }
    }
}

/// One oplock request. Windows writes into it until the request finishes.
#[repr(C)]
struct Request {
    overlapped: UnsafeCell<OVERLAPPED>,
    output: UnsafeCell<REQUEST_OPLOCK_OUTPUT_BUFFER>,
    shared: Arc<Shared>,
}

/// A [`Request`], owned by the table of requests ([`requests`]): freed when
/// taken off it and dropped.
struct Owned {
    request: NonNull<Request>,
    /// Finished at once with a status that may still queue its completion
    /// (counted in [`RETAINED`]).
    retained: bool,
}

// SAFETY: the request is reached only through the table's mutex, and its
// cells only by Windows while it's pending.
unsafe impl Send for Owned {}

impl Owned {
    fn overlapped(&self) -> *mut OVERLAPPED {
        // SAFETY: the request is alive while owned.
        unsafe { self.request.as_ref().overlapped.get() }
    }

    fn shared(&self) -> Arc<Shared> {
        // SAFETY: as above.
        Arc::clone(unsafe { &self.request.as_ref().shared })
    }
}

impl Drop for Owned {
    fn drop(&mut self) {
        // SAFETY: from `Box::into_raw` in `WatchedFile::open`, dropped once:
        // only when taken off the table, which happens once its request
        // has finished.
        drop(unsafe { Box::from_raw(self.request.as_ptr()) });
    }
}

/// The oplock requests whose completion may still come, by their handle's
/// completion key.
fn requests() -> MutexGuard<'static, HashMap<usize, Owned>> {
    static REQUESTS: OnceLock<Mutex<HashMap<usize, Owned>>> = OnceLock::new();
    REQUESTS.get_or_init(Mutex::default).lock().unwrap_or_else(PoisonError::into_inner)
}

/// Requests kept in the table though they finished at once ([`Owned::
/// retained`]); changed only under the table's lock. Their completion
/// should come, but nothing promises it, so at most [`MAX_RETAINED`] are
/// kept: past that, files aren't opened to be read (busy) until some
/// complete, rather than the table growing without bound.
static RETAINED: AtomicUsize = AtomicUsize::new(0);
const MAX_RETAINED: usize = 16;

/// A place counted in [`RETAINED`], given back when dropped unless kept.
struct Slot(bool);

impl Slot {
    fn take() -> Option<Self> {
        let mut n = RETAINED.load(Ordering::SeqCst);
        loop {
            if n >= MAX_RETAINED {
                return None;
            }
            match RETAINED.compare_exchange_weak(n, n + 1, Ordering::SeqCst, Ordering::SeqCst) {
                Ok(_) => return Some(Self(true)),
                Err(now) => n = now,
            }
        }
    }

    /// Kept by a retained request (given back as its completion comes).
    fn keep(mut self) {
        self.0 = false;
    }
}

impl Drop for Slot {
    fn drop(&mut self) {
        if self.0 {
            RETAINED.fetch_sub(1, Ordering::SeqCst);
        }
    }
}

#[cfg(test)]
fn injected_status() -> Option<NTSTATUS> {
    tests::INJECT_STATUS.get()
}

#[cfg(not(test))]
fn injected_status() -> Option<NTSTATUS> {
    None
}

fn status_error(status: NTSTATUS) -> windows::core::Error {
    // SAFETY: a pure conversion.
    WIN32_ERROR(unsafe { RtlNtStatusToDosError(status) }).into()
}

impl WatchedFile {
    /// Opens the file at `path` for reading, fully shared, with its oplock
    /// requested as part of the open. Fails with [`Error::is_busy`] if the
    /// file is mapped for writing (writes through a mapping that exists
    /// already wouldn't break the oplock, and Windows doesn't grant it
    /// then) or if opening it would break another program's oplock.
    pub(super) fn open(path: &Path) -> Result<Self> {
        let port = port()?;
        // A place among the kept requests, taken before the request is made
        // so that opens at once can't overshoot; given back unless kept.
        let Some(slot) = Slot::take() else {
            static SAID: AtomicBool = AtomicBool::new(false);
            if !SAID.swap(true, Ordering::Relaxed) {
                tracing::warn!(
                    kept = MAX_RETAINED,
                    "oplock requests that finished at once never completed; files can't be read until they do"
                );
            }
            return Err(Error::Win32 { op: "FSCTL_REQUEST_OPLOCK", source: ERROR_OPLOCK_NOT_GRANTED.into() });
        };
        let flags = FILE_FLAG_BACKUP_SEMANTICS
            | FILE_FLAG_OPEN_REPARSE_POINT
            | FILE_FLAG_OVERLAPPED
            | FILE_FLAGS_AND_ATTRIBUTES(FILE_FLAG_OPEN_REQUIRING_OPLOCK);
        let handle = super::open_shared(
            path,
            FILE_READ_DATA.0 | FILE_READ_ATTRIBUTES.0,
            FILE_SHARE_READ | FILE_SHARE_WRITE | FILE_SHARE_DELETE,
            flags,
        )?;
        let raw = handle.raw();
        let key = NEXT_KEY.fetch_add(1, Ordering::Relaxed);
        // SAFETY: both are valid handles; binding routes the file's
        // completions to the port under `key`.
        win("CreateIoCompletionPort", unsafe { CreateIoCompletionPort(raw, Some(port), key, 0) })?;
        // SAFETY: a valid handle.
        win("SetFileCompletionNotificationModes", unsafe {
            SetFileCompletionNotificationModes(raw, FILE_SKIP_COMPLETION_PORT_ON_SUCCESS)
        })?;
        // SAFETY: no name or security attributes; the handle is owned below.
        let event = Handle(win("CreateEventW", unsafe { CreateEventW(None, true, false, None) })?);
        let shared = Arc::new(Shared { handle: Mutex::new(Some(handle)), broken: AtomicBool::new(false) });
        let request = NonNull::from(Box::leak(Box::new(Request {
            overlapped: UnsafeCell::new(OVERLAPPED { Internal: STATUS_PENDING.0 as usize, ..Default::default() }),
            output: UnsafeCell::new(REQUEST_OPLOCK_OUTPUT_BUFFER {
                StructureVersion: REQUEST_OPLOCK_CURRENT_VERSION as u16,
                StructureLength: size_of::<REQUEST_OPLOCK_OUTPUT_BUFFER>() as u16,
                ..Default::default()
            }),
            shared: Arc::clone(&shared),
        })));
        let owned = Owned { request, retained: false };
        let (ov, out) = (owned.overlapped(), {
            // SAFETY: the request is alive (owned).
            unsafe { request.as_ref().output.get() }
        });
        // In the table before the request is made: its completion may come
        // off the port before the call returns.
        requests().insert(key, owned);
        let input = REQUEST_OPLOCK_INPUT_BUFFER {
            StructureVersion: REQUEST_OPLOCK_CURRENT_VERSION as u16,
            StructureLength: size_of::<REQUEST_OPLOCK_INPUT_BUFFER>() as u16,
            RequestedOplockLevel: OPLOCK_LEVEL_CACHE_READ | OPLOCK_LEVEL_CACHE_HANDLE,
            Flags: REQUEST_OPLOCK_INPUT_FLAG_REQUEST,
        };
        // The first call on the handle, as `FILE_FLAG_OPEN_REQUIRING_OPLOCK`
        // wants (the two above are the I/O manager's, not the file
        // system's). As `DeviceIoControl` makes it, with the status block
        // in the `OVERLAPPED` and the `OVERLAPPED` as the completion's
        // context, but returning the status itself.
        // A test can make it finish at once with a given status instead.
        let status = injected_status().unwrap_or_else(|| {
            // SAFETY: the input is copied during the call; the request stays
            // allocated until its completion comes off the port, or below if
            // none will; the handle is open (`shared` holds it).
            unsafe {
                NtFsControlFile(
                    raw,
                    None,
                    None,
                    Some(ov.cast_const().cast::<c_void>()),
                    ov.cast::<IO_STATUS_BLOCK>(),
                    FSCTL_REQUEST_OPLOCK,
                    Some(std::ptr::from_ref(&input).cast::<c_void>()),
                    size_of::<REQUEST_OPLOCK_INPUT_BUFFER>() as u32,
                    Some(out.cast::<c_void>()),
                    size_of::<REQUEST_OPLOCK_OUTPUT_BUFFER>() as u32,
                )
            }
        });
        if status == STATUS_PENDING {
            // Granted: the request stays pending until the oplock breaks.
            return Ok(Self { shared, key, pos: 0, event });
        }
        // Finished at once: nothing to watch with. A success queues
        // nothing (`FILE_SKIP_COMPLETION_PORT_ON_SUCCESS`), nor does a
        // request that fails at once (an error status; measured, a refusal
        // queues nothing): the request is freed. Any other status (a
        // warning) might still be queued: the table keeps the request until
        // it comes.
        #[cfg(test)]
        tests::LAST_REFUSED.set(Some(key));
        let is_error = (status.0 as u32) >> 30 == 3;
        let mut table = requests();
        if status == STATUS_SUCCESS || is_error {
            drop(table.remove(&key));
        } else if let Some(owned) = table.get_mut(&key) {
            // Still there (its completion hasn't come yet): it keeps its
            // place, given back when the completion comes.
            owned.retained = true;
            slot.keep();
        }
        drop(table);
        Shared::close(&mut shared.lock(), false);
        let source = if status == STATUS_SUCCESS { ERROR_OPLOCK_NOT_GRANTED.into() } else { status_error(status) };
        Err(Error::Win32 { op: "FSCTL_REQUEST_OPLOCK", source })
    }

    /// Whether the file may have been written since it was opened, or
    /// another program needed it closed (the handle is then closed, and
    /// reads fail). What was read so far is the version that was opened
    /// only if this is false *after* the reads.
    ///
    /// A break is queued on the acknowledger's port before the write that
    /// caused it changes anything, so waiting for the acknowledger to
    /// work through the port up to now covers every write that the bytes
    /// already read could include.
    pub fn broken(&self) -> bool {
        self.shared.broken.load(Ordering::SeqCst) || !caught_up() || self.shared.broken.load(Ordering::SeqCst)
    }

    /// Lends the file's handle to `f`, unless it was closed, for metadata
    /// queries: `GetFileInformationByHandleEx` and the like, and Cloud
    /// Files placeholder queries. Briefly: closing the handle after a break
    /// waits for `f`. Never to read data (read through `Read`).
    pub fn with_handle<T>(&self, f: impl FnOnce(&Handle) -> T) -> Option<T> {
        self.shared.lock().as_ref().map(f)
    }

    /// Reads from the start again.
    pub fn rewind(&mut self) {
        self.pos = 0;
    }
}

/// What reads give once the file is closed: callers tell it apart with
/// [`WatchedFile::broken`].
fn closed() -> std::io::Error {
    std::io::Error::other("the file was written or needed by another program while it was read")
}

impl std::io::Read for WatchedFile {
    fn read(&mut self, buf: &mut [u8]) -> std::io::Result<usize> {
        if self.shared.broken.load(Ordering::SeqCst) {
            return Err(closed());
        }
        let want = buf.len().min(CHUNK);
        let mut ov = OVERLAPPED {
            Anonymous: OVERLAPPED_0 {
                Anonymous: OVERLAPPED_0_0 { Offset: self.pos as u32, OffsetHigh: (self.pos >> 32) as u32 },
            },
            // The low bit set keeps this read's completion off the port.
            hEvent: HANDLE((self.event.raw().0 as usize | 1) as *mut c_void),
            ..Default::default()
        };
        // Started under the lock, so the handle can't be closed under it,
        // but waited for without: closing it meanwhile cancels the read.
        let started = {
            let guard = self.shared.lock();
            let Some(h) = guard.as_ref() else { return Err(closed()) };
            // SAFETY: `buf` and `ov` outlive the read: a read that started
            // is waited for below, before either goes away.
            unsafe { ReadFile(h.raw(), Some(&mut buf[..want]), None, Some(&mut ov)) }
        };
        match started {
            Ok(()) => {}
            Err(e) if e.code() == ERROR_IO_PENDING.to_hresult() => {
                // SAFETY: our own event, signalled when the read finishes.
                unsafe { WaitForSingleObject(self.event.raw(), INFINITE) };
            }
            Err(e) => {
                // Not started. `ERROR_HANDLE_EOF`: past the end.
                return if e.code() == windows::Win32::Foundation::ERROR_HANDLE_EOF.to_hresult() {
                    Ok(0)
                } else {
                    Err(e.into())
                };
            }
        }
        // SAFETY: the read has finished; Windows wrote its status block.
        let (status, n) = unsafe {
            (std::ptr::read_volatile(&raw const ov.Internal), std::ptr::read_volatile(&raw const ov.InternalHigh))
        };
        let status = NTSTATUS(status as i32);
        if status == STATUS_END_OF_FILE {
            return Ok(0);
        }
        if (status.0 as u32) >> 30 == 3 {
            return Err(status_error(status).into());
        }
        self.pos += n as u64;
        Ok(n)
    }
}

impl Drop for WatchedFile {
    fn drop(&mut self) {
        // Its request's completion (ended by the close, or the break)
        // still comes off the port and frees it.
        Shared::close(&mut self.shared.lock(), false);
    }
}

impl std::fmt::Debug for WatchedFile {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("WatchedFile").field("broken", &self.shared.broken.load(Ordering::SeqCst)).finish()
    }
}

/// The acknowledger's completion port, made (and its thread started) on
/// first use. It lives as long as the process.
fn port() -> Result<HANDLE> {
    static PORT: OnceLock<std::result::Result<usize, windows::core::HRESULT>> = OnceLock::new();
    let port = PORT.get_or_init(|| {
        // SAFETY: makes a new port, not bound to any file.
        let port =
            unsafe { CreateIoCompletionPort(HANDLE(-1isize as *mut c_void), None, 0, 1) }.map_err(|e| e.code())?;
        let raw = port.0 as usize;
        std::thread::Builder::new()
            .name("oneup-oplock-ack".into())
            .spawn(move || acknowledge(HANDLE(raw as *mut c_void)))
            .map_err(|_| ERROR_OPLOCK_NOT_GRANTED.to_hresult())?;
        Ok(raw)
    });
    match port {
        Ok(raw) => Ok(HANDLE(*raw as *mut c_void)),
        Err(code) => Err(Error::Win32 { op: "CreateIoCompletionPort", source: (*code).into() }),
    }
}

/// Packets taken off the port that matched no request of ours (for tests).
static IGNORED: AtomicUsize = AtomicUsize::new(0);

/// The acknowledger: takes completions off the port, in the order they
/// were queued, and never waits for anything else.
fn acknowledge(port: HANDLE) {
    loop {
        let (mut n, mut key, mut ov) = (0u32, 0usize, std::ptr::null_mut::<OVERLAPPED>());
        // SAFETY: valid out-pointers; the port lives as long as the process.
        let r = unsafe { GetQueuedCompletionStatus(port, &mut n, &mut key, &mut ov, INFINITE) };
        if ov.is_null() {
            if r.is_err() {
                return; // The port is gone.
            }
            IGNORED.fetch_add(1, Ordering::Relaxed);
            continue;
        }
        if key == CAUGHT_UP {
            // SAFETY: posted by `caught_up` from `Box::into_raw`, once.
            let tx = unsafe { Box::from_raw(ov.cast::<mpsc::SyncSender<()>>()) };
            let _ = tx.send(());
            continue;
        }
        let finished = {
            let mut table = requests();
            match table.get(&key) {
                Some(owned) if owned.overlapped() == ov => {
                    let owned = table.remove(&key);
                    if owned.as_ref().is_some_and(|o| o.retained) {
                        RETAINED.fetch_sub(1, Ordering::SeqCst);
                    }
                    owned
                }
                _ => None,
            }
        };
        match finished {
            Some(owned) => {
                let shared = owned.shared();
                // Finished, so Windows is done with it.
                drop(owned);
                shared.acknowledge();
            }
            None => {
                IGNORED.fetch_add(1, Ordering::Relaxed);
            }
        }
    }
}

/// Waits until the acknowledger has handled everything queued on its port
/// before now. False if that couldn't be made sure of.
fn caught_up() -> bool {
    let Ok(port) = port() else { return false };
    let (tx, rx) = mpsc::sync_channel::<()>(1);
    let ptr = Box::into_raw(Box::new(tx));
    // SAFETY: posts our own allocation, which the acknowledger turns back
    // into its `Box` (it isn't touched as an `OVERLAPPED`).
    if unsafe { PostQueuedCompletionStatus(port, 0, CAUGHT_UP, Some(ptr.cast::<OVERLAPPED>())) }.is_err() {
        // SAFETY: not posted, so still ours alone.
        drop(unsafe { Box::from_raw(ptr) });
        return false;
    }
    rx.recv_timeout(CATCH_UP_WAIT).is_ok()
}

/// How many completions the acknowledger took off its port that weren't
/// one of its requests' (I/O other code made through a bound handle).
#[doc(hidden)]
pub fn ignored_completions() -> usize {
    IGNORED.load(Ordering::Relaxed)
}

#[cfg(test)]
pub(super) mod tests {
    use std::io::{Read, Write};
    use std::path::PathBuf;
    use std::time::{Duration, Instant};

    use super::super::Volume;
    use super::super::tests::{id_of, wide_of};
    use super::*;

    thread_local! {
        /// The completion key of the last request refused at once on this thread.
        pub(super) static LAST_REFUSED: std::cell::Cell<Option<usize>> = const { std::cell::Cell::new(None) };
        /// Requests made on this thread finish at once with this status
        /// instead of being made.
        pub(super) static INJECT_STATUS: std::cell::Cell<Option<NTSTATUS>> = const { std::cell::Cell::new(None) };
    }

    /// Tests that open files to be read share it; one that fills the table
    /// of kept requests (which then refuses every open) takes it alone.
    pub(in super::super) static OPENS: std::sync::RwLock<()> = std::sync::RwLock::new(());

    pub(in super::super) fn opens() -> std::sync::RwLockReadGuard<'static, ()> {
        OPENS.read().unwrap_or_else(PoisonError::into_inner)
    }

    struct Dir(PathBuf, #[allow(dead_code)] Option<std::sync::RwLockReadGuard<'static, ()>>);

    impl Dir {
        fn new(name: &str) -> Self {
            let mut me = Self::alone(name);
            me.1 = Some(opens());
            me
        }

        /// [`Dir::new`] for a test that holds [`OPENS`] itself.
        fn alone(name: &str) -> Self {
            let dir = std::env::temp_dir().join(format!("oneup-ntfs-{name}-{}", std::process::id()));
            let _ = std::fs::remove_dir_all(&dir);
            std::fs::create_dir_all(&dir).unwrap();
            Self(dir, None)
        }

        fn p(&self, name: &str) -> PathBuf {
            self.0.join(name)
        }

        /// `doc.txt` written with `content`, opened to be read.
        fn watched(&self, vol: &Volume, content: &[u8]) -> WatchedFile {
            std::fs::write(self.p("doc.txt"), content).unwrap();
            vol.open_watched(id_of(&self.0, "doc.txt"), None).unwrap().0
        }

        /// `name` written with some content, opened to be read.
        fn watched_as(&self, vol: &Volume, name: &str) -> WatchedFile {
            std::fs::write(self.p(name), name).unwrap();
            vol.open_watched(id_of(&self.0, name), None).unwrap().0
        }

        /// A save by another program, `cmd`'s `move /y` (`MoveFileEx`),
        /// of `new` over `doc.txt`. Whether it went through.
        fn save_by_move(&self, new: &[u8]) -> bool {
            std::fs::write(self.p("doc.tmp"), new).unwrap();
            let out = std::process::Command::new("cmd.exe")
                .current_dir(&self.0)
                .args(["/c", "move", "/y", "doc.tmp", "doc.txt"])
                .output()
                .unwrap();
            out.status.success() && std::fs::read(self.p("doc.txt")).unwrap() == new
        }
    }

    impl Drop for Dir {
        fn drop(&mut self) {
            let _ = std::fs::remove_dir_all(&self.0);
        }
    }

    fn read_all(f: &mut WatchedFile) -> std::io::Result<Vec<u8>> {
        let mut got = Vec::new();
        f.read_to_end(&mut got)?;
        Ok(got)
    }

    #[test]
    fn a_watched_file_sees_writes_and_keeps_no_program_waiting() {
        let d = Dir::new("watched");
        let vol = Volume::open_containing(&d.0).unwrap();
        let doc = d.p("doc.txt");

        let mut f = d.watched(&vol, b"hello");
        assert_eq!(read_all(&mut f).unwrap(), b"hello");
        assert!(std::fs::File::open(&doc).is_ok(), "others read it meanwhile");
        f.rewind();
        assert_eq!(read_all(&mut f).unwrap(), b"hello");
        assert!(!f.broken());

        // A save in place: opening for writing alone doesn't break it, the
        // write does, the writer isn't kept waiting, and reads then fail.
        let mut w = std::fs::OpenOptions::new().write(true).open(&doc).unwrap();
        assert!(!f.broken());
        w.write_all(b"HELLO").unwrap();
        assert!(f.broken());
        f.rewind();
        assert!(read_all(&mut f).is_err());
        drop((w, f));

        let f = d.watched(&vol, b"hello");
        std::fs::OpenOptions::new().write(true).truncate(true).open(&doc).unwrap();
        assert!(f.broken());

        // Setting its length breaks it, even to the length it has.
        let f = d.watched(&vol, b"hello");
        std::fs::OpenOptions::new().write(true).open(&doc).unwrap().set_len(5).unwrap();
        assert!(f.broken());

        // Programs that need it closed get it closed, and go ahead: a save
        // by renaming over it (POSIX semantics, and `MoveFileEx` from
        // another program), a delete, an open that shares nothing.
        let mut f = d.watched(&vol, b"hello");
        std::fs::write(d.p("doc.tmp"), b"new").unwrap();
        std::fs::rename(d.p("doc.tmp"), &doc).unwrap();
        assert!(f.broken());
        assert!(read_all(&mut f).is_err());
        assert_eq!(std::fs::read(&doc).unwrap(), b"new");

        let f = d.watched(&vol, b"hello");
        assert!(d.save_by_move(b"saved"), "the save went through");
        assert!(f.broken());

        let f = d.watched(&vol, b"hello");
        std::fs::remove_file(&doc).unwrap();
        assert!(!doc.exists());
        assert!(f.broken());

        let f = d.watched(&vol, b"hello");
        {
            use std::os::windows::fs::OpenOptionsExt;
            let only = std::fs::OpenOptions::new().read(true).write(true).share_mode(0).open(&doc);
            assert!(only.is_ok(), "{only:?}");
        }
        assert!(f.broken());

        // A program that had it open for writing before: its later writes
        // break it too.
        std::fs::write(&doc, b"hello").unwrap();
        let mut held = std::fs::OpenOptions::new().write(true).open(&doc).unwrap();
        let f = vol.open_watched(id_of(&d.0, "doc.txt"), None).unwrap().0;
        assert!(!f.broken());
        held.write_all(b"HELLO").unwrap();
        assert!(f.broken());
        drop((f, held));

        // Writes through a mapping made before would go unseen, so a file
        // mapped for writing can't be watched. One mapped after breaks it
        // as soon as the mapping is made.
        {
            use std::os::windows::io::AsRawHandle;
            use windows::Win32::System::Memory::{CreateFileMappingW, PAGE_READWRITE};
            let map = |f: &std::fs::File| {
                // SAFETY: a valid file handle; the mapping is closed below.
                Handle(
                    unsafe { CreateFileMappingW(HANDLE(f.as_raw_handle()), None, PAGE_READWRITE, 0, 0, None) }.unwrap(),
                )
            };
            let w = std::fs::OpenOptions::new().read(true).write(true).open(&doc).unwrap();
            let m = map(&w);
            drop(w);
            let err = vol.open_watched(id_of(&d.0, "doc.txt"), None).unwrap_err();
            assert!(err.is_busy(), "{err}");
            drop(m);
            let f = vol.open_watched(id_of(&d.0, "doc.txt"), None).unwrap().0;
            let w = std::fs::OpenOptions::new().read(true).write(true).open(&doc).unwrap();
            assert!(!f.broken());
            let m = map(&w);
            assert!(f.broken());
            drop((m, w, f));
        }
    }

    /// A write is seen as soon as it's made, every time: the reader never
    /// takes bytes that include it for the version it opened.
    #[test]
    fn a_write_is_seen_as_soon_as_it_is_made() {
        let d = Dir::new("watched-seen");
        let vol = Volume::open_containing(&d.0).unwrap();
        for _ in 0..200 {
            let f = d.watched(&vol, b"hello");
            std::fs::OpenOptions::new().write(true).open(d.p("doc.txt")).unwrap().write_all(b"J").unwrap();
            assert!(f.broken());
        }
    }

    /// The executor opens an upload's file on one thread and reads it on
    /// others, which end when idle: the oplock outlives them all.
    #[test]
    fn a_watched_file_outlives_the_thread_that_opened_it() {
        let d = Dir::new("watched-threads");
        let vol = Volume::open_containing(&d.0).unwrap();
        std::fs::write(d.p("doc.txt"), b"hello").unwrap();
        let id = id_of(&d.0, "doc.txt");
        let mut f = std::thread::scope(|s| s.spawn(|| vol.open_watched(id, None).unwrap().0).join().unwrap());
        std::thread::sleep(Duration::from_millis(100));
        assert!(!f.broken(), "its thread ended, the oplock stays");
        let mut f = std::thread::spawn(move || {
            assert_eq!(read_all(&mut f).unwrap(), b"hello");
            f
        })
        .join()
        .unwrap();
        let doc = d.p("doc.txt");
        std::thread::spawn(move || std::fs::OpenOptions::new().write(true).open(doc).unwrap().write_all(b"J").unwrap())
            .join()
            .unwrap();
        assert!(f.broken());
        f.rewind();
        assert!(read_all(&mut f).is_err());
    }

    /// A program saving over a file that's being read waits for the read
    /// under way (one chunk at most) and for us to close the handle.
    #[test]
    fn a_save_waits_only_briefly_for_a_read_under_way() {
        let d = Dir::new("watched-wait");
        let vol = Volume::open_containing(&d.0).unwrap();
        let move_over = |from: &str, to: &str| {
            let (from, to) = (wide_of(&d.p(from)), wide_of(&d.p(to)));
            let t = Instant::now();
            // SAFETY: NUL-terminated paths that outlive the call.
            let moved = unsafe {
                windows::Win32::Storage::FileSystem::MoveFileExW(
                    windows::core::PCWSTR(from.as_ptr()),
                    windows::core::PCWSTR(to.as_ptr()),
                    windows::Win32::Storage::FileSystem::MOVEFILE_REPLACE_EXISTING,
                )
            };
            (moved, t.elapsed())
        };
        // The same save with nothing open, for comparison.
        std::fs::write(d.p("doc.txt"), vec![7u8; 64 << 20]).unwrap();
        std::fs::write(d.p("doc.tmp"), b"new").unwrap();
        let (moved, alone) = move_over("doc.tmp", "doc.txt");
        assert!(moved.is_ok(), "{moved:?}");

        let mut f = d.watched(&vol, &vec![7u8; 64 << 20]);
        // The save starts once a read is under way, not after a guessed
        // delay: under load the reader may not have started by then.
        let (reading, started) = std::sync::mpsc::channel();
        let reader = std::thread::spawn(move || {
            let mut buf = vec![0u8; 4 << 20];
            let mut total = 0usize;
            loop {
                match f.read(&mut buf) {
                    Ok(0) => f.rewind(),
                    Ok(n) => {
                        total += n;
                        let _ = reading.send(());
                    }
                    Err(_) => return (total, f.broken()),
                }
            }
        });
        started.recv_timeout(Duration::from_secs(30)).unwrap();
        std::fs::write(d.p("doc.tmp"), b"new").unwrap();
        let (moved, waited) = move_over("doc.tmp", "doc.txt");
        let (total, broken) = reader.join().unwrap();
        eprintln!("MoveFileEx over a 64 MiB file being read took {waited:?}, with nothing open {alone:?}");
        assert!(moved.is_ok(), "{moved:?}");
        assert!(total > 0 && broken);
        assert!(waited < Duration::from_secs(2), "{waited:?}");
        assert_eq!(std::fs::read(d.p("doc.txt")).unwrap(), b"new");
    }

    /// Every oplock request is given back, whether it broke or the file
    /// was closed first.
    #[test]
    fn requests_are_freed_however_they_end() {
        let d = Dir::new("watched-freed");
        let vol = Volume::open_containing(&d.0).unwrap();
        let mut gone = Vec::new();
        for i in 0..50 {
            let f = d.watched(&vol, b"hello");
            gone.push(Arc::downgrade(&f.shared));
            if i % 2 == 0 {
                std::fs::OpenOptions::new().write(true).open(d.p("doc.txt")).unwrap().write_all(b"J").unwrap();
                assert!(f.broken());
            }
        }
        // A break closes the handle at once, or on a thread of its own if
        // the handle is in use: give that a moment.
        let t = Instant::now();
        while gone.iter().any(|w| w.upgrade().is_some()) && t.elapsed() < Duration::from_secs(5) {
            assert!(caught_up());
            std::thread::sleep(Duration::from_millis(10));
        }
        assert!(gone.iter().all(|w| w.upgrade().is_none()), "all freed");
    }

    /// Packets that aren't a request's own completion are ignored, whatever
    /// they carry: another `OVERLAPPED` under a live file's key (I/O other
    /// code made through the handle), a file's old key and address after
    /// it's gone. A refused request leaves nothing behind.
    #[test]
    fn only_a_requests_own_completion_touches_it() {
        let d = Dir::new("watched-stale");
        let vol = Volume::open_containing(&d.0).unwrap();
        let port = port().unwrap();
        let post = |key: usize, ov: *const OVERLAPPED| {
            // SAFETY: a packet the acknowledger must not dereference.
            unsafe { PostQueuedCompletionStatus(port, 0, key, Some(ov)) }.unwrap();
        };
        let foreign = Box::new(OVERLAPPED::default());
        let live = d.watched_as(&vol, "live.txt");
        let ignored = ignored_completions();
        post(live.key, &*foreign);
        assert!(!live.broken(), "another OVERLAPPED under its key");

        let gone = d.watched_as(&vol, "gone.txt");
        let (key, ov) = (gone.key, requests()[&gone.key].overlapped());
        drop(gone);
        let t = Instant::now();
        while requests().contains_key(&key) && t.elapsed() < Duration::from_secs(5) {
            std::thread::sleep(Duration::from_millis(5));
        }
        assert!(!requests().contains_key(&key), "freed once its completion came");
        // A new file's request may now sit at the old address: a stale
        // packet carrying it, under the old key, still matches nothing.
        let newer: Vec<_> = (0..20).map(|i| d.watched_as(&vol, &format!("new{i}.txt"))).collect();
        post(key, ov);
        assert!(newer.iter().all(|f| !f.broken()) && !live.broken());
        assert!(ignored_completions() >= ignored + 2);

        // Refused at once (mapped for writing): no request is kept.
        use std::os::windows::io::AsRawHandle;
        use windows::Win32::System::Memory::{CreateFileMappingW, PAGE_READWRITE};
        drop((live, newer));
        std::fs::write(d.p("doc.txt"), b"hello").unwrap();
        let w = std::fs::OpenOptions::new().read(true).write(true).open(d.p("doc.txt")).unwrap();
        // SAFETY: a valid file handle; the mapping is closed below.
        let m =
            Handle(unsafe { CreateFileMappingW(HANDLE(w.as_raw_handle()), None, PAGE_READWRITE, 0, 0, None) }.unwrap());
        LAST_REFUSED.set(None);
        assert!(vol.open_watched(id_of(&d.0, "doc.txt"), None).unwrap_err().is_busy());
        let refused = LAST_REFUSED.get().expect("refused at once");
        assert!(!requests().contains_key(&refused));
        drop((m, w));
    }

    /// The acknowledger never waits for a file: while one file's handle is
    /// held up (here, a long metadata query; in use, a read being started
    /// on a slow disk), a break of another is still acknowledged at once,
    /// and the held-up file's break is seen at once and its handle closed
    /// once it's free.
    #[test]
    fn a_held_up_file_keeps_no_other_save_waiting() {
        let d = Dir::new("watched-held");
        let vol = Volume::open_containing(&d.0).unwrap();
        std::fs::write(d.p("a.txt"), b"a").unwrap();
        let a = vol.open_watched(id_of(&d.0, "a.txt"), None).unwrap().0;
        let b = d.watched(&vol, b"b");
        std::thread::scope(|s| {
            let (held_tx, held_rx) = mpsc::channel();
            let a = &a;
            s.spawn(move || {
                a.with_handle(|_| {
                    held_tx.send(()).unwrap();
                    std::thread::sleep(Duration::from_millis(1500));
                })
            });
            held_rx.recv().unwrap();
            std::fs::OpenOptions::new().write(true).open(d.p("a.txt")).unwrap().write_all(b"A").unwrap();
            let t = Instant::now();
            assert!(a.broken(), "its break is seen while its handle is held");
            assert!(d.save_by_move(b"saved"), "the other file's save goes through");
            let took = t.elapsed();
            assert!(b.broken());
            assert!(took < Duration::from_millis(1000), "{took:?}");
        });
        // Free again: its handle was closed, so a save over it goes through.
        let t = Instant::now();
        while a.with_handle(|_| ()).is_some() && t.elapsed() < Duration::from_secs(5) {
            std::thread::sleep(Duration::from_millis(5));
        }
        assert!(a.with_handle(|_| ()).is_none(), "closed once free");
    }

    /// Saves by `MoveFileEx` racing opens of the file (1,000 saves a
    /// second against an open every 8 ms): some fail, and as many when
    /// another program merely opens and closes the file (`plain`). A save
    /// fails if a new handle on the file appears while its rename runs,
    /// oplock or not; holding the file afterwards adds nothing. Timing
    /// only: the rates swing between runs.
    #[test]
    #[ignore = "timing only"]
    fn timing_saves_racing_opens() {
        let d = Dir::new("watched-race");
        let vol = Volume::open_containing(&d.0).unwrap();
        std::fs::write(d.p("doc.txt"), b"v").unwrap();
        let at = vol
            .child_path(super::super::file_id(&super::super::open_dir(&d.0).unwrap()).unwrap().1, "doc.txt")
            .unwrap();
        for watched in [false, true, false, true] {
            let stop = AtomicBool::new(false);
            let (mut saves, mut failed, mut opens) = (0, 0, 0);
            std::thread::scope(|s| {
                let opener = s.spawn(|| {
                    let mut n = 0;
                    while !stop.load(Ordering::SeqCst) {
                        if watched {
                            if let Ok((mut f, _)) = vol.open_watched(id_of(&d.0, "doc.txt"), Some(&at)) {
                                let _ = read_all(&mut f);
                                std::thread::sleep(Duration::from_millis(4));
                                n += 1;
                            }
                        } else if std::fs::File::open(&at).is_ok() {
                            n += 1;
                        }
                        std::thread::sleep(Duration::from_millis(4));
                    }
                    n
                });
                let (from, to) = (wide_of(&d.p("doc.tmp")), wide_of(&d.p("doc.txt")));
                let t = Instant::now();
                while t.elapsed() < Duration::from_secs(3) {
                    saves += 1;
                    std::fs::write(d.p("doc.tmp"), format!("v{saves}")).unwrap();
                    // SAFETY: NUL-terminated paths that outlive the call.
                    let moved = unsafe {
                        windows::Win32::Storage::FileSystem::MoveFileExW(
                            windows::core::PCWSTR(from.as_ptr()),
                            windows::core::PCWSTR(to.as_ptr()),
                            windows::Win32::Storage::FileSystem::MOVEFILE_REPLACE_EXISTING,
                        )
                    };
                    failed += usize::from(moved.is_err());
                }
                stop.store(true, Ordering::SeqCst);
                opens = opener.join().unwrap();
            });
            let what = if watched { "open_watched, read, held 4 ms" } else { "plain open and close" };
            eprintln!("{what:30}: {opens} opens, {saves} saves, {failed} failed");
        }
    }

    /// A request that finishes at once with a status that may still queue
    /// its completion (a warning or informational one) is kept until that
    /// comes; at most `MAX_RETAINED` are, and past that files aren't
    /// opened to be read (busy) rather than the table growing. Success and
    /// error statuses keep nothing.
    #[test]
    fn requests_kept_for_a_completion_are_bounded() {
        let _alone = OPENS.write().unwrap_or_else(PoisonError::into_inner);
        /// Gives the kept requests back however the test ends (their
        /// completions would; these never come).
        struct GiveBack;
        impl Drop for GiveBack {
            fn drop(&mut self) {
                INJECT_STATUS.set(None);
                let mut table = requests();
                table.retain(|_, o| !o.retained);
                RETAINED.store(0, Ordering::SeqCst);
            }
        }
        let _give_back = GiveBack;
        let d = Dir::alone("watched-kept");
        let vol = Volume::open_containing(&d.0).unwrap();
        std::fs::write(d.p("doc.txt"), b"hello").unwrap();
        let id = id_of(&d.0, "doc.txt");
        let at = d.p("doc.txt");
        let kept = || RETAINED.load(Ordering::SeqCst);
        let before = kept();
        for status in [STATUS_SUCCESS, NTSTATUS(0xC000_00E2_u32 as i32)] {
            INJECT_STATUS.set(Some(status));
            assert!(WatchedFile::open(&at).is_err());
            assert_eq!(kept(), before, "{status:?} keeps nothing");
        }
        // Warnings (buffer overflow) and informational statuses.
        for i in before..MAX_RETAINED {
            let status = if i % 2 == 0 { 0x8000_0005_u32 } else { 0x4000_0000_u32 };
            INJECT_STATUS.set(Some(NTSTATUS(status as i32)));
            let err = WatchedFile::open(&at).unwrap_err();
            assert!(!err.is_busy(), "{err}");
            assert_eq!(kept(), i + 1);
        }
        INJECT_STATUS.set(None);
        let err = vol.open_watched(id, Some(&at)).unwrap_err();
        assert!(err.is_busy(), "full: refused, {err}");
        assert_eq!(kept(), MAX_RETAINED, "and nothing more kept");

        // Their completions coming frees them (here: given back by hand).
        drop(_give_back);
        let mut f = vol.open_watched(id, Some(&at)).unwrap().0;
        assert_eq!(read_all(&mut f).unwrap(), b"hello");
    }

    /// Opens at once on many threads, all finishing with a status that keeps
    /// their request, never keep more than `MAX_RETAINED`.
    #[test]
    fn opens_at_once_never_keep_more_than_the_bound() {
        let _alone = OPENS.write().unwrap_or_else(PoisonError::into_inner);
        struct GiveBack;
        impl Drop for GiveBack {
            fn drop(&mut self) {
                let mut table = requests();
                table.retain(|_, o| !o.retained);
                RETAINED.store(0, Ordering::SeqCst);
            }
        }
        let _give_back = GiveBack;
        let d = Dir::alone("watched-kept-at-once");
        std::fs::write(d.p("doc.txt"), b"hello").unwrap();
        let at = d.p("doc.txt");
        let most = AtomicUsize::new(0);
        std::thread::scope(|s| {
            for _ in 0..16 {
                s.spawn(|| {
                    INJECT_STATUS.set(Some(NTSTATUS(0x8000_0005_u32 as i32)));
                    for _ in 0..20 {
                        let _ = WatchedFile::open(&at);
                        most.fetch_max(RETAINED.load(Ordering::SeqCst), Ordering::SeqCst);
                    }
                });
            }
        });
        assert_eq!(RETAINED.load(Ordering::SeqCst), MAX_RETAINED);
        assert!(most.load(Ordering::SeqCst) <= MAX_RETAINED, "{most:?}");
        assert_eq!(requests().values().filter(|o| o.retained).count(), MAX_RETAINED);
    }

    /// A busy, unrelated file where the target used to be doesn't stop the
    /// target being found where it is now.
    #[test]
    fn a_busy_file_at_a_stale_hint_doesnt_hide_the_target() {
        use std::os::windows::io::AsRawHandle;
        use windows::Win32::System::Memory::{CreateFileMappingW, PAGE_READWRITE};
        let d = Dir::new("watched-stale-hint");
        let vol = Volume::open_containing(&d.0).unwrap();
        std::fs::write(d.p("a.txt"), b"target").unwrap();
        let id = id_of(&d.0, "a.txt");
        std::fs::rename(d.p("a.txt"), d.p("b.txt")).unwrap();
        std::fs::write(d.p("a.txt"), b"another, mapped for writing").unwrap();
        let w = std::fs::OpenOptions::new().read(true).write(true).open(d.p("a.txt")).unwrap();
        // SAFETY: a valid file handle; the mapping is closed below.
        let m =
            Handle(unsafe { CreateFileMappingW(HANDLE(w.as_raw_handle()), None, PAGE_READWRITE, 0, 0, None) }.unwrap());
        let (mut f, at) = vol.open_watched(id, Some(&d.p("a.txt"))).unwrap();
        assert!(at.ends_with("b.txt"), "{at:?}");
        assert_eq!(read_all(&mut f).unwrap(), b"target");
        // The target itself busy: still busy.
        drop(f);
        let err = vol.open_watched(id_of(&d.0, "a.txt"), Some(&d.p("a.txt"))).unwrap_err();
        assert!(err.is_busy(), "{err}");
        drop((m, w));
    }
}
