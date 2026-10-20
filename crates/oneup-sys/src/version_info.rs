//! A program's own names from its version resource (`OriginalFilename`,
//! `InternalName`), so a renamed copy of an interpreter (`powershell.exe`
//! copied to `run.exe`) is still recognised as one (`oneup_hooks::program`).

use std::ffi::c_void;
use std::os::windows::ffi::OsStrExt as _;
use std::path::Path;

use windows::Win32::Storage::FileSystem::{GetFileVersionInfoSizeW, GetFileVersionInfoW, VerQueryValueW};
use windows::core::{HSTRING, PCWSTR};

/// Reads a string value at `sub` (e.g. `\StringFileInfo\040904b0\OriginalFilename`).
fn query(block: &[u8], sub: &str) -> Option<String> {
    let mut ptr: *mut c_void = std::ptr::null_mut();
    let mut len = 0u32;
    let sub = HSTRING::from(sub);
    // SAFETY: `block` is a version block GetFileVersionInfoW filled; `ptr`
    // and `len` are valid out locations. The returned pointer points into
    // `block`, which outlives its use here.
    let ok = unsafe { VerQueryValueW(block.as_ptr().cast(), &sub, &mut ptr, &mut len) }.as_bool();
    if !ok || ptr.is_null() || len == 0 {
        return None;
    }
    // SAFETY: for a string value, `ptr` points to `len` UTF-16 units inside
    // `block` (including the terminating NUL).
    let units = unsafe { std::slice::from_raw_parts(ptr.cast::<u16>(), len as usize) };
    let end = units.iter().position(|&c| c == 0).unwrap_or(units.len());
    Some(String::from_utf16_lossy(&units[..end]))
}

/// The `OriginalFilename` and `InternalName` the file's version resource
/// gives, for each language it lists; empty if it has none.
pub fn original_names(path: &Path) -> Vec<String> {
    let wide: Vec<u16> = path.as_os_str().encode_wide().chain(std::iter::once(0)).collect();
    // SAFETY: `wide` is NUL-terminated and outlives the call.
    let size = unsafe { GetFileVersionInfoSizeW(PCWSTR(wide.as_ptr()), None) };
    if size == 0 {
        return Vec::new();
    }
    let mut block = vec![0u8; size as usize];
    // SAFETY: `block` is writable for `size` bytes.
    if unsafe { GetFileVersionInfoW(PCWSTR(wide.as_ptr()), None, size, block.as_mut_ptr().cast()) }.is_err() {
        return Vec::new();
    }
    // The languages and code pages the resource has strings for.
    let mut ptr: *mut c_void = std::ptr::null_mut();
    let mut len = 0u32;
    let sub = HSTRING::from(r"\VarFileInfo\Translation");
    // SAFETY: as in `query`.
    let ok = unsafe { VerQueryValueW(block.as_ptr().cast(), &sub, &mut ptr, &mut len) }.as_bool();
    let mut langs: Vec<(u16, u16)> = Vec::new();
    if ok && !ptr.is_null() {
        // SAFETY: the translation table is `len` bytes of (language, code
        // page) u16 pairs inside `block`.
        let pairs = unsafe { std::slice::from_raw_parts(ptr.cast::<u16>(), len as usize / 2) };
        langs.extend(pairs.as_chunks::<2>().0.iter().map(|p| (p[0], p[1])));
    }
    // Common fallbacks when the table is missing.
    langs.extend([(0x0409, 0x04b0), (0x0409, 0x04e4), (0x0000, 0x04b0)]);
    let mut out = Vec::new();
    for (lang, cp) in langs {
        for key in ["OriginalFilename", "InternalName"] {
            if let Some(v) = query(&block, &format!(r"\StringFileInfo\{lang:04x}{cp:04x}\{key}"))
                && !v.trim().is_empty()
                && !out.contains(&v)
            {
                out.push(v);
            }
        }
    }
    out
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used)]
    use super::*;

    #[test]
    fn a_copied_system_program_keeps_its_original_name() {
        let cmd = std::path::PathBuf::from(std::env::var_os("SystemRoot").unwrap()).join("System32").join("cmd.exe");
        let names = original_names(&cmd);
        assert!(names.iter().any(|n| n.to_ascii_lowercase().starts_with("cmd")), "{names:?}");
        let dir = std::env::temp_dir().join(format!("oneup-verinfo-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        let copy = dir.join("innocent.exe");
        std::fs::copy(&cmd, &copy).unwrap();
        let names = original_names(&copy);
        assert!(names.iter().any(|n| n.to_ascii_lowercase().starts_with("cmd")), "{names:?}");
        let _ = std::fs::remove_dir_all(&dir);
    }
}
