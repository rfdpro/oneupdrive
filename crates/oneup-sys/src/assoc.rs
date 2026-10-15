//! File types and opening things the way Explorer does: what OneUpDrive's
//! optional Office opener needs (`oneup_app::officeopen`). Reads
//! `HKEY_CLASSES_ROOT`, copies a file type's registration into the user's
//! own classes, and opens a file with a named file type's verb, so a file
//! can go to Office's own "open" while OneUpDrive is the default program.

use std::ffi::c_void;

use windows::Win32::Foundation::{ERROR_FILE_NOT_FOUND, ERROR_NO_MORE_ITEMS, ERROR_SUCCESS, WIN32_ERROR};
use windows::Win32::System::Registry::{
    HKEY, HKEY_CLASSES_ROOT, HKEY_CURRENT_USER, HKEY_LOCAL_MACHINE, KEY_READ, KEY_WRITE, REG_OPTION_NON_VOLATILE,
    REG_VALUE_TYPE, RRF_RT_REG_SZ, RegCloseKey, RegCreateKeyExW, RegEnumKeyExW, RegEnumValueW, RegGetValueW,
    RegOpenKeyExW, RegSetValueExW,
};
use windows::Win32::UI::Shell::{
    SEE_MASK_CLASSNAME, SEE_MASK_NOASYNC, SHCNE_ASSOCCHANGED, SHCNF_IDLIST, SHChangeNotify, SHELLEXECUTEINFOW,
    ShellExecuteExW,
};
use windows::Win32::UI::WindowsAndMessaging::SW_SHOWNORMAL;
use windows::core::{HSTRING, PCWSTR, PWSTR, w};

fn check(op: &'static str, r: WIN32_ERROR) -> std::io::Result<()> {
    if r == ERROR_SUCCESS {
        Ok(())
    } else {
        Err(std::io::Error::new(std::io::Error::from_raw_os_error(r.0 as i32).kind(), format!("{op}: {r:?}")))
    }
}

/// The default value of `HKEY_CLASSES_ROOT\<key>` (the user's and the
/// machine's classes together), if there is one.
pub fn classes_default(key: &str) -> Option<String> {
    let k = HSTRING::from(key);
    let mut buf = vec![0u16; 1024];
    let mut bytes = (buf.len() * 2) as u32;
    // SAFETY: `buf` is writable and `bytes` long; the key name outlives the call.
    let r = unsafe {
        RegGetValueW(
            HKEY_CLASSES_ROOT,
            &k,
            PCWSTR::null(),
            RRF_RT_REG_SZ,
            None,
            Some(buf.as_mut_ptr().cast::<c_void>()),
            Some(&mut bytes),
        )
    };
    if r != ERROR_SUCCESS {
        return None;
    }
    let end = buf.iter().position(|&c| c == 0).unwrap_or(buf.len());
    Some(String::from_utf16_lossy(&buf[..end])).filter(|s| !s.is_empty())
}

/// Copies file type `from` with everything readable in it to
/// `HKEY_CURRENT_USER\<to>` (created if missing, existing values replaced):
/// the machine's registration (`HKLM\Software\Classes`, where Office
/// registers), else the user's. Sub-keys it can't read are left out
/// (`RegCopyTreeW` fails outright on Office's, "access denied", measured).
pub fn copy_class(from: &str, to: &str) -> std::io::Result<()> {
    let mut src = HKEY::default();
    let mut r = ERROR_FILE_NOT_FOUND;
    for root in [HKEY_LOCAL_MACHINE, HKEY_CURRENT_USER] {
        // SAFETY: the name outlives the call; `src` is a valid out-pointer.
        r = unsafe {
            RegOpenKeyExW(root, &HSTRING::from(format!(r"Software\Classes\{from}")), None, KEY_READ, &mut src)
        };
        if r != ERROR_FILE_NOT_FOUND {
            break;
        }
    }
    if r == ERROR_FILE_NOT_FOUND {
        return Err(std::io::Error::new(std::io::ErrorKind::NotFound, format!("no file type {from}")));
    }
    check("RegOpenKeyExW", r)?;
    let result = create(HKEY_CURRENT_USER, &HSTRING::from(to)).and_then(|dst| {
        let r = copy_key(src, dst);
        // SAFETY: closing the key `create` opened, once.
        let _ = unsafe { RegCloseKey(dst) };
        r
    });
    // SAFETY: closing the key opened above, once.
    let _ = unsafe { RegCloseKey(src) };
    result
}

fn create(parent: HKEY, name: &HSTRING) -> std::io::Result<HKEY> {
    let mut key = HKEY::default();
    // SAFETY: the name outlives the call; no security attributes, no
    // disposition wanted; `key` is a valid out-pointer.
    let r = unsafe {
        RegCreateKeyExW(
            parent,
            name,
            None,
            PCWSTR::null(),
            REG_OPTION_NON_VOLATILE,
            KEY_READ | KEY_WRITE,
            None,
            &mut key,
            None,
        )
    };
    check("RegCreateKeyExW", r).map(|()| key)
}

/// Copies `src`'s values and (readable) sub-keys into `dst`.
fn copy_key(src: HKEY, dst: HKEY) -> std::io::Result<()> {
    for i in 0.. {
        let mut name = vec![0u16; 16_384];
        let mut name_len = name.len() as u32;
        let mut kind = 0u32;
        let mut data = vec![0u8; 64 * 1024];
        let mut data_len = data.len() as u32;
        // SAFETY: every buffer is writable for the length given with it.
        let r = unsafe {
            RegEnumValueW(
                src,
                i,
                Some(PWSTR(name.as_mut_ptr())),
                &mut name_len,
                None,
                Some(&mut kind),
                Some(data.as_mut_ptr()),
                Some(&mut data_len),
            )
        };
        if r == ERROR_NO_MORE_ITEMS {
            break;
        }
        if r != ERROR_SUCCESS {
            continue;
        }
        name.truncate(name_len as usize);
        name.push(0);
        // SAFETY: `name` is NUL-terminated; `data` holds `data_len` bytes.
        let r = unsafe {
            RegSetValueExW(dst, PCWSTR(name.as_ptr()), None, REG_VALUE_TYPE(kind), Some(&data[..data_len as usize]))
        };
        check("RegSetValueExW", r)?;
    }
    for i in 0.. {
        let mut name = vec![0u16; 256];
        let mut name_len = name.len() as u32;
        // SAFETY: `name` is writable for `name_len` characters.
        let r = unsafe { RegEnumKeyExW(src, i, Some(PWSTR(name.as_mut_ptr())), &mut name_len, None, None, None, None) };
        if r == ERROR_NO_MORE_ITEMS {
            break;
        }
        if r != ERROR_SUCCESS {
            continue;
        }
        let sub = HSTRING::from_wide(&name[..name_len as usize]);
        let mut s = HKEY::default();
        // SAFETY: the name outlives the call; `s` is a valid out-pointer.
        if unsafe { RegOpenKeyExW(src, &sub, None, KEY_READ, &mut s) } != ERROR_SUCCESS {
            continue;
        }
        let result = create(dst, &sub).and_then(|d| {
            let r = copy_key(s, d);
            // SAFETY: closing the key `create` opened, once.
            let _ = unsafe { RegCloseKey(d) };
            r
        });
        // SAFETY: closing the key opened above, once.
        let _ = unsafe { RegCloseKey(s) };
        result?;
    }
    Ok(())
}

fn execute(file: &str, class: Option<&str>) -> std::io::Result<()> {
    let file = HSTRING::from(file);
    let class = class.map(HSTRING::from);
    let mut info = SHELLEXECUTEINFOW {
        cbSize: size_of::<SHELLEXECUTEINFOW>() as u32,
        // The caller exits right after: let Windows finish (DDE included).
        fMask: SEE_MASK_NOASYNC | if class.is_some() { SEE_MASK_CLASSNAME } else { Default::default() },
        lpVerb: w!("open"),
        lpFile: PCWSTR(file.as_ptr()),
        nShow: SW_SHOWNORMAL.0,
        ..Default::default()
    };
    if let Some(c) = &class {
        info.lpClass = PCWSTR(c.as_ptr());
    }
    // SAFETY: `info` is initialised, and the strings it points to live
    // through the call.
    unsafe { ShellExecuteExW(&mut info) }.map_err(|e| std::io::Error::other(format!("ShellExecuteExW: {e}")))
}

/// Opens `path` with file type `class`'s "open" verb, whatever the default
/// program for its extension is.
pub fn open_as(path: &str, class: &str) -> std::io::Result<()> {
    execute(path, Some(class))
}

/// Opens a file or an address (`ms-word:…`) with its default program.
pub fn open(target: &str) -> std::io::Result<()> {
    execute(target, None)
}

/// Tells Explorer that file types changed (icons, default programs).
pub fn changed() {
    // SAFETY: SHCNE_ASSOCCHANGED takes no items.
    unsafe { SHChangeNotify(SHCNE_ASSOCCHANGED, SHCNF_IDLIST, None, None) };
}
