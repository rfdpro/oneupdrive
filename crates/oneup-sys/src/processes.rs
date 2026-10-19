//! Finding and ending another program's processes, by where their
//! executable is. Used only to stop the OneDrive app when its unlinking has
//! stalled (`oneup_app::moveover`), and only for processes of this Windows
//! user (others can't be opened without admin rights anyway).

use std::path::{Path, PathBuf};

use windows::Win32::Foundation::{CloseHandle, HANDLE, WAIT_OBJECT_0};
use windows::Win32::System::ProcessStatus::K32EnumProcesses;
use windows::Win32::System::Threading::{
    OpenProcess, PROCESS_ACCESS_RIGHTS, PROCESS_NAME_WIN32, PROCESS_QUERY_LIMITED_INFORMATION, PROCESS_SYNCHRONIZE,
    PROCESS_TERMINATE, QueryFullProcessImageNameW, TerminateProcess, WaitForSingleObject,
};
use windows::core::PWSTR;

use crate::{Error, Result};

/// A running process and its executable.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Process {
    pub pid: u32,
    pub exe: PathBuf,
}

/// Closes the handle when dropped.
struct Owned(HANDLE);

impl Drop for Owned {
    fn drop(&mut self) {
        // SAFETY: a handle this module opened and nothing else closes.
        unsafe {
            let _ = CloseHandle(self.0);
        }
    }
}

fn open(pid: u32, access: PROCESS_ACCESS_RIGHTS) -> Option<Owned> {
    // SAFETY: opening a process by ID; the handle is owned by the result.
    unsafe { OpenProcess(access, false, pid) }.ok().map(Owned)
}

fn image(h: &Owned) -> Option<PathBuf> {
    let mut buf = vec![0u16; 32_768];
    let mut len = buf.len() as u32;
    // SAFETY: `buf` holds `len` UTF-16 units; the handle is valid.
    unsafe { QueryFullProcessImageNameW(h.0, PROCESS_NAME_WIN32, PWSTR(buf.as_mut_ptr()), &mut len) }.ok()?;
    Some(PathBuf::from(String::from_utf16_lossy(&buf[..len as usize])))
}

/// The executable of process `pid`, if it can be asked (a process of this
/// user, or one that allows limited queries).
pub fn image_of(pid: u32) -> Option<PathBuf> {
    image(&open(pid, PROCESS_QUERY_LIMITED_INFORMATION)?)
}

fn inside(exe: &Path, dir: &Path) -> bool {
    let (e, d) = (exe.to_string_lossy().to_lowercase(), dir.to_string_lossy().to_lowercase());
    let d = d.trim_end_matches('\\');
    e.len() > d.len() && e.starts_with(d) && e.as_bytes()[d.len()] == b'\\'
}

/// The processes whose executable is inside `dir` (any depth).
pub fn inside_dir(dir: &Path) -> Result<Vec<Process>> {
    Ok(all()?.into_iter().filter(|p| inside(&p.exe, dir)).collect())
}

/// The processes whose executable is called `file_name` (compared as
/// Windows compares ASCII names; `file_name` is a program's name, like
/// `OneDrive.exe`).
pub fn named(file_name: &str) -> Result<Vec<Process>> {
    Ok(all()?
        .into_iter()
        .filter(|p| p.exe.file_name().is_some_and(|n| n.to_string_lossy().eq_ignore_ascii_case(file_name)))
        .collect())
}

/// Every process whose executable can be asked (this user's, and those that
/// allow limited queries).
fn all() -> Result<Vec<Process>> {
    let mut pids = vec![0u32; 4096];
    loop {
        let mut got = 0u32;
        let bytes = (pids.len() * 4) as u32;
        // SAFETY: `pids` is writable for `bytes` bytes.
        unsafe { K32EnumProcesses(pids.as_mut_ptr(), bytes, &mut got) }
            .ok()
            .map_err(|source| Error::Win32 { op: "EnumProcesses", source })?;
        if got < bytes {
            pids.truncate(got as usize / 4);
            break;
        }
        pids.resize(pids.len() * 2, 0);
    }
    Ok(pids
        .into_iter()
        .filter_map(|pid| {
            let exe = image(&open(pid, PROCESS_QUERY_LIMITED_INFORMATION)?)?;
            Some(Process { pid, exe })
        })
        .collect())
}

/// Ends `p` if it's still that executable (a process ID can be reused),
/// and waits up to `wait_ms` for it to be gone. `Ok(false)`: it had already
/// ended, or the ID is now another program's (left alone).
pub fn terminate(p: &Process, wait_ms: u32) -> Result<bool> {
    let Some(h) = open(p.pid, PROCESS_TERMINATE | PROCESS_SYNCHRONIZE | PROCESS_QUERY_LIMITED_INFORMATION) else {
        return Ok(false);
    };
    if image(&h).as_deref() != Some(p.exe.as_path()) {
        return Ok(false);
    }
    // SAFETY: a valid process handle opened with PROCESS_TERMINATE.
    unsafe { TerminateProcess(h.0, 1) }.map_err(|source| Error::Win32 { op: "TerminateProcess", source })?;
    // SAFETY: a valid handle opened with SYNCHRONIZE.
    if unsafe { WaitForSingleObject(h.0, wait_ms) } != WAIT_OBJECT_0 {
        return Err(Error::Win32 { op: "WaitForSingleObject", source: windows::core::Error::from_thread() });
    }
    Ok(true)
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used)]
    use super::*;

    #[test]
    fn a_process_is_found_by_its_folder_and_ended() {
        let mut child = std::process::Command::new("cmd")
            .args(["/C", "pause"])
            .stdin(std::process::Stdio::piped())
            .stdout(std::process::Stdio::null())
            .spawn()
            .unwrap();
        let system = PathBuf::from(std::env::var_os("SystemRoot").unwrap()).join("System32");
        let found = inside_dir(&system).unwrap();
        let p = found.iter().find(|p| p.pid == child.id()).expect("found by its folder").clone();
        assert!(p.exe.to_string_lossy().to_lowercase().ends_with("cmd.exe"));
        assert!(named("CMD.EXE").unwrap().iter().any(|q| q.pid == p.pid), "found by its name");
        assert!(!named("cmd").unwrap().iter().any(|q| q.pid == p.pid), "a whole name");
        assert!(!inside_dir(&std::env::temp_dir()).unwrap().iter().any(|q| q.pid == p.pid));
        // Another executable under that ID: left alone.
        let wrong = Process { pid: p.pid, exe: system.join("notepad.exe") };
        assert!(!terminate(&wrong, 1000).unwrap());
        assert!(terminate(&p, 5000).unwrap());
        assert!(!child.wait().unwrap().success());
    }

    #[test]
    fn folders_match_whole_names() {
        assert!(inside(Path::new(r"C:\A\OneDrive\x.exe"), Path::new(r"c:\a\onedrive\")));
        assert!(!inside(Path::new(r"C:\A\OneDriveX\x.exe"), Path::new(r"C:\A\OneDrive")));
        assert!(!inside(Path::new(r"C:\A\OneDrive"), Path::new(r"C:\A\OneDrive")));
    }
}
