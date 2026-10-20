//! The hard links (directory entries) of a file. A hook's program or script
//! that has a second name inside a synced folder could be replaced through
//! that name by a cloud change, so [`oneup_hooks`] enumerates a file's links
//! and refuses any inside a synced folder (`FindFirstFileNameW`).

use std::ffi::OsString;
use std::os::windows::ffi::{OsStrExt as _, OsStringExt as _};
use std::path::{Path, PathBuf};

use windows::Win32::Foundation::{ERROR_HANDLE_EOF, ERROR_MORE_DATA, HANDLE};
use windows::Win32::Storage::FileSystem::{FindClose, FindFirstFileNameW, FindNextFileNameW, GetVolumePathNameW};
use windows::core::PCWSTR;

fn wide(p: &Path) -> Vec<u16> {
    p.as_os_str().encode_wide().chain(std::iter::once(0)).collect()
}

/// The volume root of `path` (e.g. `C:\`), so a volume-relative link name
/// (`\Tools\x.exe`) can be made absolute.
fn volume_root(path: &Path) -> std::io::Result<PathBuf> {
    let w = wide(path);
    let mut buf = vec![0u16; 260];
    // SAFETY: `w` is NUL-terminated and outlives the call; `buf` is writable
    // for its length.
    unsafe { GetVolumePathNameW(PCWSTR(w.as_ptr()), &mut buf) }
        .map_err(|e| std::io::Error::other(format!("GetVolumePathNameW: {e}")))?;
    let len = buf.iter().position(|&c| c == 0).unwrap_or(buf.len());
    Ok(PathBuf::from(OsString::from_wide(&buf[..len])))
}

/// Reads one link name into a right-sized buffer, growing on `ERROR_MORE_DATA`.
/// `first` is the `FindFirstFileNameW` handle; `start` opens it.
fn link_name(read: impl Fn(&mut Vec<u16>, &mut u32) -> windows::core::Result<()>) -> std::io::Result<Option<Vec<u16>>> {
    let mut cap = 260u32;
    loop {
        let mut buf = vec![0u16; cap as usize];
        let mut len = cap;
        match read(&mut buf, &mut len) {
            Ok(()) => {
                let n = buf.iter().position(|&c| c == 0).unwrap_or(len as usize);
                return Ok(Some(buf[..n].to_vec()));
            }
            Err(e) if e.code() == ERROR_MORE_DATA.to_hresult() => cap = len.max(cap * 2),
            Err(e) if e.code() == ERROR_HANDLE_EOF.to_hresult() => return Ok(None),
            Err(e) => return Err(std::io::Error::other(format!("link name: {e}"))),
        }
    }
}

/// Every hard link (full path) of the file at `path`. At least the path
/// itself; more when the file has several names. Errors rather than guess,
/// so the caller can refuse the hook.
pub fn all_links(path: &Path) -> std::io::Result<Vec<PathBuf>> {
    let root = volume_root(path)?;
    let w = wide(path);
    let mut first_buf = vec![0u16; 260];
    let mut len = first_buf.len() as u32;
    let handle = loop {
        // SAFETY: `w` is NUL-terminated; `first_buf` is writable for `len`.
        let r = unsafe {
            FindFirstFileNameW(PCWSTR(w.as_ptr()), 0, &mut len, windows::core::PWSTR(first_buf.as_mut_ptr()))
        };
        match r {
            Ok(h) => break h,
            Err(e) if e.code() == ERROR_MORE_DATA.to_hresult() => {
                first_buf = vec![0u16; len as usize];
            }
            Err(e) => return Err(std::io::Error::other(format!("FindFirstFileNameW: {e}"))),
        }
    };
    struct Find(HANDLE);
    impl Drop for Find {
        fn drop(&mut self) {
            // SAFETY: a valid find handle this function opened.
            let _ = unsafe { FindClose(self.0) };
        }
    }
    let find = Find(handle);
    let mut out = Vec::new();
    let add = |out: &mut Vec<PathBuf>, name: &[u16]| {
        // The names are volume-relative (start with `\`); join onto the root.
        let rel = PathBuf::from(OsString::from_wide(name));
        let joined = root.join(rel.strip_prefix(std::path::MAIN_SEPARATOR.to_string()).unwrap_or(&rel));
        out.push(joined);
    };
    let n = first_buf.iter().position(|&c| c == 0).unwrap_or(first_buf.len());
    add(&mut out, &first_buf[..n]);
    loop {
        let name = link_name(|buf, len| {
            // SAFETY: the find handle is valid; `buf` is writable for `len`.
            unsafe { FindNextFileNameW(find.0, len, windows::core::PWSTR(buf.as_mut_ptr())) }
        })?;
        match name {
            Some(name) => add(&mut out, &name),
            None => break,
        }
    }
    Ok(out)
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used)]
    use super::*;

    #[test]
    fn a_file_with_two_names_lists_both() {
        let dir = std::env::temp_dir().join(format!("oneup-links-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        let a = dir.join("a.txt");
        std::fs::write(&a, b"x").unwrap();
        let b = dir.join("b.txt");
        std::fs::hard_link(&a, &b).unwrap();
        let a = std::fs::canonicalize(&a).unwrap();
        let links = all_links(&a).unwrap();
        assert_eq!(links.len(), 2, "{links:?}");
        let names: Vec<String> =
            links.iter().map(|p| p.file_name().unwrap().to_string_lossy().to_lowercase()).collect();
        assert!(names.contains(&"a.txt".to_string()) && names.contains(&"b.txt".to_string()), "{names:?}");
        // One name again.
        std::fs::remove_file(&b).unwrap();
        assert_eq!(all_links(&a).unwrap().len(), 1);
        let _ = std::fs::remove_dir_all(&dir);
    }
}
