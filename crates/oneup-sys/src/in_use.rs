//! Which programs have a file open, through Windows' Restart Manager (the
//! API installers use to find what blocks a file). Works without admin
//! rights for the user's own programs. Used to say which program changed a
//! file (`oneup_app::apps`): asked right after the change journal reports
//! a write, while the writer usually still has the file open.

use std::path::Path;

use windows::Win32::Foundation::{CloseHandle, ERROR_MORE_DATA, WIN32_ERROR};
use windows::Win32::System::RestartManager::{
    CCH_RM_SESSION_KEY, RM_PROCESS_INFO, RmEndSession, RmGetList, RmRegisterResources, RmStartSession,
};
use windows::Win32::System::Threading::{
    OpenProcess, PROCESS_NAME_WIN32, PROCESS_QUERY_LIMITED_INFORMATION, QueryFullProcessImageNameW,
};
use windows::core::{HSTRING, PCWSTR, PWSTR};

use crate::{Error, Result};

/// A program that has a file open.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct User {
    pub pid: u32,
    /// Its executable's file name (`WINWORD.EXE`), or `""` if Windows
    /// wouldn't say.
    pub exe: String,
}

fn check(op: &'static str, code: WIN32_ERROR) -> Result<()> {
    if code.0 == 0 { Ok(()) } else { Err(Error::Win32 { op, source: windows::core::Error::from(code.to_hresult()) }) }
}

/// The programs that have `path` open now (at most 16).
pub fn users(path: &Path) -> Result<Vec<User>> {
    let mut session = 0u32;
    let mut key = [0u16; CCH_RM_SESSION_KEY as usize + 1];
    // SAFETY: `key` has room for the session key and its NUL, as required.
    check("RmStartSession", unsafe { RmStartSession(&mut session, None, PWSTR(key.as_mut_ptr())) })?;
    let out = users_in(session, path);
    // SAFETY: ends the session started above, whatever happened in between.
    unsafe {
        let _ = RmEndSession(session);
    }
    out
}

fn users_in(session: u32, path: &Path) -> Result<Vec<User>> {
    let file = HSTRING::from(path.as_os_str());
    let files = [PCWSTR(file.as_ptr())];
    // SAFETY: `files` holds one valid NUL-terminated path that outlives the call.
    check("RmRegisterResources", unsafe { RmRegisterResources(session, Some(&files), None, None) })?;
    let mut infos = [RM_PROCESS_INFO::default(); 16];
    let (mut needed, mut count, mut reasons) = (0u32, infos.len() as u32, 0u32);
    // SAFETY: `infos` has room for `count` entries; the other arguments are
    // valid out-pointers.
    let r = unsafe { RmGetList(session, &mut needed, &mut count, Some(infos.as_mut_ptr()), &mut reasons) };
    if r != ERROR_MORE_DATA {
        check("RmGetList", r)?;
    }
    let n = (count as usize).min(infos.len());
    Ok(infos[..n].iter().map(|i| User { pid: i.Process.dwProcessId, exe: exe_name(i.Process.dwProcessId) }).collect())
}

/// The file name of process `pid`'s executable, or `""`.
fn exe_name(pid: u32) -> String {
    // SAFETY: opening a process for limited queries only; the handle is
    // closed below.
    let Ok(h) = (unsafe { OpenProcess(PROCESS_QUERY_LIMITED_INFORMATION, false, pid) }) else {
        return String::new();
    };
    let mut buf = [0u16; 1024];
    let mut len = buf.len() as u32;
    // SAFETY: `buf` holds `len` UTF-16 units; the handle is valid.
    let ok = unsafe { QueryFullProcessImageNameW(h, PROCESS_NAME_WIN32, PWSTR(buf.as_mut_ptr()), &mut len) }.is_ok();
    // SAFETY: closes the handle opened above.
    unsafe {
        let _ = CloseHandle(h);
    }
    if !ok {
        return String::new();
    }
    let full = String::from_utf16_lossy(&buf[..len as usize]);
    full.rsplit('\\').next().unwrap_or_default().to_string()
}

#[cfg(test)]
mod tests {
    #[test]
    fn a_file_this_test_holds_open_names_this_test() {
        let path = std::env::temp_dir().join(format!("oneup-in-use-{}.txt", std::process::id()));
        let f = std::fs::File::create(&path);
        assert!(f.is_ok());
        let users = super::users(&path).unwrap_or_default();
        let me = std::process::id();
        assert!(users.iter().any(|u| u.pid == me && u.exe.to_lowercase().ends_with(".exe")), "{users:?}");
        drop(f);
        let _ = std::fs::remove_file(&path);
    }
}
