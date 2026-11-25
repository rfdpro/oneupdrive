//! Cancellable Windows subtree notifications. A lost batch invalidates the
//! caller's snapshot because there is no bookmark to replay on an SMB share.

use std::cell::UnsafeCell;
use std::io;
use std::os::windows::io::{AsRawHandle, FromRawHandle, OwnedHandle};
use std::path::Path;
use std::time::Duration;

use windows::Win32::Foundation::{ERROR_IO_INCOMPLETE, ERROR_IO_PENDING, HANDLE, WAIT_OBJECT_0, WAIT_TIMEOUT};
use windows::Win32::Storage::FileSystem::{
    CreateFileW, FILE_FLAG_BACKUP_SEMANTICS, FILE_FLAG_OPEN_REPARSE_POINT, FILE_FLAG_OVERLAPPED, FILE_ID_INFO,
    FILE_LIST_DIRECTORY, FILE_NOTIFY_CHANGE_DIR_NAME, FILE_NOTIFY_CHANGE_FILE_NAME, FILE_NOTIFY_CHANGE_SECURITY,
    FILE_READ_ATTRIBUTES, FILE_SHARE_DELETE, FILE_SHARE_READ, FILE_SHARE_WRITE, FileIdInfo,
    GetFileInformationByHandleEx, OPEN_EXISTING, ReadDirectoryChangesW,
};
use windows::Win32::System::IO::{CancelIoEx, GetOverlappedResult, OVERLAPPED};
use windows::Win32::System::Threading::{CreateEventW, ResetEvent, WaitForMultipleObjects};
use windows::core::HSTRING;

use crate::signal::Signal;

pub struct Watch {
    dir: OwnedHandle,
    event: OwnedHandle,
    request: Box<UnsafeCell<OVERLAPPED>>,
    // The SMB redirector caps this at 64 KiB and requires DWORD alignment.
    words: Box<UnsafeCell<[u32; 16_384]>>,
    pending: bool,
}

#[derive(Debug, PartialEq, Eq)]
pub struct Change {
    pub path: String,
    /// The name was removed or is the old side of a rename.
    pub removed: bool,
}

fn win(e: windows::core::Error) -> io::Error {
    io::Error::from_raw_os_error(e.code().0 & 0xffff)
}

impl Watch {
    pub fn open(path: &Path) -> io::Result<Self> {
        let path = HSTRING::from(path.as_os_str());
        // SAFETY: the path is NUL-terminated and none of the optional pointers are used.
        let dir = unsafe {
            CreateFileW(
                &path,
                FILE_LIST_DIRECTORY.0 | FILE_READ_ATTRIBUTES.0,
                FILE_SHARE_READ | FILE_SHARE_WRITE | FILE_SHARE_DELETE,
                None,
                OPEN_EXISTING,
                FILE_FLAG_BACKUP_SEMANTICS | FILE_FLAG_OPEN_REPARSE_POINT | FILE_FLAG_OVERLAPPED,
                None,
            )
        }
        .map_err(win)?;
        // SAFETY: the successful call gave us ownership of the handle.
        let dir = unsafe { OwnedHandle::from_raw_handle(dir.0) };
        // SAFETY: no optional attributes or name are supplied.
        let event = unsafe { CreateEventW(None, true, false, None) }.map_err(win)?;
        let request = Box::new(UnsafeCell::new(OVERLAPPED::default()));
        // SAFETY: the successful call gave us ownership of the handle.
        let event = unsafe { OwnedHandle::from_raw_handle(event.0) };
        let mut watch = Self { dir, event, request, words: Box::new(UnsafeCell::new([0; 16_384])), pending: false };
        watch.arm()?;
        Ok(watch)
    }

    fn arm(&mut self) -> io::Result<()> {
        let event = HANDLE(self.event.as_raw_handle());
        // SAFETY: the previous request has completed and the event is still owned by self.
        unsafe { ResetEvent(event) }.map_err(win)?;
        // SAFETY: both allocations are stable and kept alive until completion or
        // cancellation. The buffer is aligned, writable and sized in bytes.
        let result = unsafe {
            self.request.get().write(OVERLAPPED { hEvent: event, ..Default::default() });
            ReadDirectoryChangesW(
                HANDLE(self.dir.as_raw_handle()),
                self.words.get().cast(),
                64 * 1024,
                true,
                FILE_NOTIFY_CHANGE_FILE_NAME | FILE_NOTIFY_CHANGE_DIR_NAME | FILE_NOTIFY_CHANGE_SECURITY,
                None,
                Some(self.request.get()),
                None,
            )
        };
        match result {
            Ok(()) => {}
            Err(e) if e.code() == ERROR_IO_PENDING.to_hresult() => {}
            Err(e) => return Err(win(e)),
        }
        self.pending = true;
        Ok(())
    }

    /// Identity of the exact tree that is supplying the notifications.
    pub fn identity(&self) -> io::Result<(u64, u128)> {
        let mut info = FILE_ID_INFO::default();
        // SAFETY: the handle is open and info is a writable buffer of the stated size.
        unsafe {
            GetFileInformationByHandleEx(
                HANDLE(self.dir.as_raw_handle()),
                FileIdInfo,
                (&mut info as *mut FILE_ID_INFO).cast(),
                std::mem::size_of::<FILE_ID_INFO>() as u32,
            )
        }
        .map_err(win)?;
        Ok((info.VolumeSerialNumber, u128::from_le_bytes(info.FileId.Identifier)))
    }

    /// Return the changed relative names and rearm before the caller acts.
    /// A timeout returns None. An interrupted or incomplete stream returns an error.
    pub fn changes(&mut self, stop: &Signal, timeout: Duration) -> io::Result<Option<Vec<Change>>> {
        let handles = [stop.0, HANDLE(self.event.as_raw_handle())];
        let millis = u32::try_from(timeout.as_millis()).unwrap_or(u32::MAX - 1);
        // SAFETY: both handles are valid until this call returns.
        let state = unsafe { WaitForMultipleObjects(&handles, false, millis) };
        if state == WAIT_TIMEOUT {
            return Ok(None);
        }
        if state == WAIT_OBJECT_0 {
            return Err(io::ErrorKind::Interrupted.into());
        }
        if state.0 != WAIT_OBJECT_0.0 + 1 {
            return Err(io::Error::last_os_error());
        }
        let mut bytes = 0;
        // SAFETY: the event signalled completion of our request and the byte count is writable.
        let result =
            unsafe { GetOverlappedResult(HANDLE(self.dir.as_raw_handle()), self.request.get(), &mut bytes, false) };
        if result.as_ref().is_err_and(|e| e.code() == ERROR_IO_INCOMPLETE.to_hresult()) {
            return Err(io::Error::other("Unexpectedly incomplete directory notification"));
        }
        self.pending = false;
        result.map_err(win)?;
        // SAFETY: the kernel finished writing the buffer and we have not rearmed.
        let names = parse(unsafe { &*self.words.get() }, bytes as usize)
            .ok_or_else(|| io::Error::other("Directory notifications were lost"))?;
        self.arm()?;
        Ok(Some(names))
    }
}

impl Drop for Watch {
    fn drop(&mut self) {
        if self.pending {
            // SAFETY: cancel our request, then wait for its completion before
            // releasing the buffer, event or request storage.
            unsafe {
                let dir = HANDLE(self.dir.as_raw_handle());
                let _ = CancelIoEx(dir, Some(self.request.get()));
                let _ = GetOverlappedResult(dir, self.request.get(), &mut 0, true);
            }
        }
    }
}

fn parse(words: &[u32], bytes: usize) -> Option<Vec<Change>> {
    let mut names = Vec::new();
    let mut start = 0;
    if bytes > words.len() * 4 {
        return None;
    }
    loop {
        if start + 12 > bytes {
            return None;
        }
        let index = start / 4;
        let next = words[index] as usize;
        let len = words[index + 2] as usize;
        if len == 0 || !len.is_multiple_of(2) || start + 12 + len > bytes || !(1..=5).contains(&words[index + 1]) {
            return None;
        }
        let name: Vec<u16> = words[index + 3..][..len.div_ceil(4)]
            .iter()
            .flat_map(|&pair| [pair as u16, (pair >> 16) as u16])
            .take(len / 2)
            .collect();
        if let Ok(path) = String::from_utf16(&name) {
            // FILE_ACTION_REMOVED and FILE_ACTION_RENAMED_OLD_NAME.
            names.push(Change { path, removed: matches!(words[index + 1], 2 | 4) });
        }
        if next == 0 {
            return Some(names);
        }
        if !next.is_multiple_of(4) || next < 12 + len || next > bytes - start {
            return None;
        }
        start += next;
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn empty_and_incomplete_notifications_signal_loss() {
        let words = [0, 1, 2, u32::from(b'a')];
        assert_eq!(parse(&words, 14), Some(vec![Change { path: "a".into(), removed: false }]));
        assert_eq!(parse(&words, 0), None);
        assert_eq!(parse(&words, 13), None);
        let words = [12, 1, 2, u32::from(b'a')];
        assert_eq!(parse(&words, 14), None);
    }
}
