//! Shortcut (`.lnk`) files, through the shell's own `IShellLinkW`: reading
//! where one leads, and pointing it somewhere else while keeping everything
//! else about it (icon, arguments, hotkey, comment).
//!
//! Neither call touches the shortcut's target: loading a shortcut only
//! parses the file, and `Resolve` (which searches for a moved target) is
//! never called.

use std::path::{Path, PathBuf};

use windows::Win32::Foundation::RPC_E_CHANGED_MODE;
use windows::Win32::System::Com::{
    CLSCTX_INPROC_SERVER, COINIT_APARTMENTTHREADED, CoCreateInstance, CoInitializeEx, CoUninitialize, IPersistFile,
    STGM_READ,
};
use windows::Win32::UI::Shell::{IShellLinkW, ShellLink};
use windows::core::{HSTRING, Interface as _};

/// Where a shortcut leads.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Link {
    /// The file or folder it opens, as the shell follows it; `None` for
    /// shortcuts without a path (Windows Installer's advertised shortcuts,
    /// shell places such as Control Panel) and for those whose chain of
    /// shell items the shell can't follow (one starting at a cloud
    /// provider's Explorer entry that's gone).
    pub target: Option<PathBuf>,
    /// The path the file keeps in its LinkInfo, whatever the shell makes of
    /// the chain ([`stored_path`]).
    pub stored: Option<PathBuf>,
    /// The folder the target starts in, if set.
    pub working_dir: Option<PathBuf>,
}

/// COM for this thread while it lives. A thread that already has COM in the
/// other threading model keeps it: the shell's link object works in both.
struct Com(bool);

impl Com {
    fn init() -> crate::Result<Com> {
        // SAFETY: no reserved pointer; paired with CoUninitialize in Drop
        // when (and only when) it succeeded.
        let hr = unsafe { CoInitializeEx(None, COINIT_APARTMENTTHREADED) };
        if hr == RPC_E_CHANGED_MODE {
            return Ok(Com(false));
        }
        hr.ok().map_err(|source| crate::Error::Win32 { op: "CoInitializeEx", source })?;
        Ok(Com(true))
    }
}

impl Drop for Com {
    fn drop(&mut self) {
        if self.0 {
            // SAFETY: pairs with the successful CoInitializeEx in `init`, on
            // the same thread (the guard isn't Send).
            unsafe { CoUninitialize() };
        }
    }
}

fn win(op: &'static str) -> impl FnOnce(windows::core::Error) -> crate::Error {
    move |source| crate::Error::Win32 { op, source }
}

fn new() -> crate::Result<(IShellLinkW, IPersistFile)> {
    // SAFETY: ShellLink is the shell's documented in-process link object;
    // the returned interface is owned and released on drop.
    let link: IShellLinkW =
        unsafe { CoCreateInstance(&ShellLink, None, CLSCTX_INPROC_SERVER) }.map_err(win("CoCreateInstance"))?;
    let file: IPersistFile = link.cast().map_err(win("IPersistFile"))?;
    Ok((link, file))
}

fn load(file: &IPersistFile, path: &Path) -> crate::Result<()> {
    // SAFETY: the path is a NUL-terminated wide string that outlives the call.
    unsafe { file.Load(&HSTRING::from(path.as_os_str()), STGM_READ) }.map_err(win("IPersistFile::Load"))
}

fn text(buf: &[u16]) -> Option<PathBuf> {
    let end = buf.iter().position(|&c| c == 0).unwrap_or(buf.len());
    (end > 0).then(|| PathBuf::from(String::from_utf16_lossy(&buf[..end])))
}

/// Reads many shortcuts with one link object (and COM set up once).
pub struct Reader {
    link: IShellLinkW,
    file: IPersistFile,
    buf: Vec<u16>,
    // Last: the interfaces are released before COM goes.
    _com: Com,
}

impl Reader {
    pub fn new() -> crate::Result<Reader> {
        let com = Com::init()?;
        let (link, file) = new()?;
        // Long paths: the shell stores up to 32,767 characters.
        Ok(Reader { link, file, buf: vec![0u16; 32_768], _com: com })
    }

    /// Reads where the shortcut at `path` leads.
    pub fn read(&mut self, path: &Path) -> crate::Result<Link> {
        let stored = std::fs::read(path).ok().and_then(|b| stored_path(&b));
        load(&self.file, path)?;
        self.buf.fill(0);
        // SAFETY: the buffer is writable for its length; no find data is
        // asked for (null). Flags 0: the full, expanded path. S_FALSE (no
        // path) leaves the buffer empty.
        unsafe { self.link.GetPath(&mut self.buf, std::ptr::null_mut(), 0) }.map_err(win("IShellLinkW::GetPath"))?;
        let target = text(&self.buf);
        self.buf.fill(0);
        // SAFETY: as above.
        unsafe { self.link.GetWorkingDirectory(&mut self.buf) }.map_err(win("IShellLinkW::GetWorkingDirectory"))?;
        Ok(Link { target, stored, working_dir: text(&self.buf) })
    }
}

/// Reads where the shortcut at `path` leads.
pub fn read(path: &Path) -> crate::Result<Link> {
    Reader::new()?.read(path)
}

/// Points the shortcut at `path` to `target` (and, if given, starts it in
/// `working_dir`), saving it in place. Everything else stays as it was.
pub fn retarget(path: &Path, target: &Path, working_dir: Option<&Path>) -> crate::Result<()> {
    retarget_to(path, path, target, working_dir)
}

/// [`retarget`], saving the result to `to` and leaving `from` as it is.
pub fn retarget_to(from: &Path, to: &Path, target: &Path, working_dir: Option<&Path>) -> crate::Result<()> {
    let _com = Com::init()?;
    let (link, file) = new()?;
    load(&file, from)?;
    save(&link, &file, to, target, working_dir)
}

/// Puts `replacement` in the place of `replaced` in one step, keeping
/// `replaced`'s permissions, attributes and other streams (`ReplaceFileW`).
/// `replacement` is gone afterwards. When `ReplaceFileW` fails partway with
/// `replaced` gone (`ERROR_UNABLE_TO_MOVE_REPLACEMENT`, `_2`), `replacement`
/// is moved to its name if nothing has taken it; if that fails too, it is
/// left where it is, never deleted.
pub fn replace(replaced: &Path, replacement: &Path) -> crate::Result<()> {
    use windows::Win32::Foundation::{ERROR_UNABLE_TO_MOVE_REPLACEMENT, ERROR_UNABLE_TO_MOVE_REPLACEMENT_2};
    use windows::Win32::Storage::FileSystem::{MOVEFILE_WRITE_THROUGH, MoveFileExW, REPLACE_FILE_FLAGS, ReplaceFileW};
    use windows::core::{HRESULT, PCWSTR};
    // SAFETY: both paths are NUL-terminated wide strings that outlive the
    // call; no backup file, and the reserved pointers are null. (Its
    // write-through flag is documented as unsupported, so none.)
    unsafe {
        ReplaceFileW(
            &HSTRING::from(replaced.as_os_str()),
            &HSTRING::from(replacement.as_os_str()),
            PCWSTR::null(),
            REPLACE_FILE_FLAGS(0),
            None,
            None,
        )
    }
    .inspect_err(|e| {
        let partial = [ERROR_UNABLE_TO_MOVE_REPLACEMENT, ERROR_UNABLE_TO_MOVE_REPLACEMENT_2]
            .iter()
            .any(|c| e.code() == HRESULT::from_win32(c.0));
        if partial && !replaced.exists() {
            // SAFETY: both paths are NUL-terminated wide strings that
            // outlive the call. No replace flag: it fails if the name is
            // taken.
            let _ = unsafe {
                MoveFileExW(
                    &HSTRING::from(replacement.as_os_str()),
                    &HSTRING::from(replaced.as_os_str()),
                    MOVEFILE_WRITE_THROUGH,
                )
            };
        }
    })
    .map_err(win("ReplaceFileW"))
}

/// Moves `from` to `to`, replacing it, and returns only once the move is on
/// disk (`MoveFileExW` with `MOVEFILE_WRITE_THROUGH`).
pub fn move_over(from: &Path, to: &Path) -> crate::Result<()> {
    use windows::Win32::Storage::FileSystem::{MOVEFILE_REPLACE_EXISTING, MOVEFILE_WRITE_THROUGH, MoveFileExW};
    // SAFETY: both paths are NUL-terminated wide strings that outlive the call.
    unsafe {
        MoveFileExW(
            &HSTRING::from(from.as_os_str()),
            &HSTRING::from(to.as_os_str()),
            MOVEFILE_REPLACE_EXISTING | MOVEFILE_WRITE_THROUGH,
        )
    }
    .map_err(win("MoveFileExW"))
}

/// Makes a new shortcut at `path` to `target`.
pub fn create(path: &Path, target: &Path, working_dir: Option<&Path>) -> crate::Result<()> {
    let _com = Com::init()?;
    let (link, file) = new()?;
    save(&link, &file, path, target, working_dir)
}

fn save(link: &IShellLinkW, file: &IPersistFile, path: &Path, target: &Path, wd: Option<&Path>) -> crate::Result<()> {
    // SAFETY: NUL-terminated wide strings that outlive each call.
    unsafe {
        link.SetPath(&HSTRING::from(target.as_os_str())).map_err(win("IShellLinkW::SetPath"))?;
        if let Some(wd) = wd {
            link.SetWorkingDirectory(&HSTRING::from(wd.as_os_str()))
                .map_err(win("IShellLinkW::SetWorkingDirectory"))?;
        }
        file.Save(&HSTRING::from(path.as_os_str()), true).map_err(win("IPersistFile::Save"))
    }
}

/// The path a shortcut file keeps in its LinkInfo block (MS-SHLLINK
/// §2.3), whatever its chain of shell items says: the Unicode local base
/// path if there is one, else the ANSI one, with the common path suffix.
/// An ANSI path outside ASCII is refused: its code page is the one of the
/// PC that made the shortcut, which the file doesn't say.
pub fn stored_path(b: &[u8]) -> Option<PathBuf> {
    use std::ffi::OsString;
    use std::os::windows::ffi::OsStringExt as _;
    let u16_at = |o: usize| Some(u16::from_le_bytes(b.get(o..o + 2)?.try_into().ok()?));
    let u32_at = |o: usize| Some(u32::from_le_bytes(b.get(o..o + 4)?.try_into().ok()?) as usize);
    const HAS_ID_LIST: usize = 0x1;
    const HAS_LINK_INFO: usize = 0x2;
    const VOLUME_AND_LOCAL_PATH: usize = 0x1;
    if u32_at(0)? != 0x4C {
        return None;
    }
    let flags = u32_at(0x14)?;
    if flags & HAS_LINK_INFO == 0 {
        return None;
    }
    let mut at = 0x4C;
    if flags & HAS_ID_LIST != 0 {
        at += 2 + u16_at(at)? as usize;
    }
    let info = b.get(at..at + u32_at(at)?)?;
    let field = |o: usize| u32_at(at + o);
    if field(8)? & VOLUME_AND_LOCAL_PATH == 0 {
        return None;
    }
    let ansi = |o: usize| -> Option<OsString> {
        let s = info.get(o..)?;
        let s = &s[..s.iter().position(|&c| c == 0)?];
        s.is_ascii().then(|| OsString::from(String::from_utf8_lossy(s).into_owned()))
    };
    let wide = |o: usize| -> Option<OsString> {
        let s: Vec<u16> = info.get(o..)?.as_chunks::<2>().0.iter().map(|c| u16::from_le_bytes(*c)).collect();
        Some(OsString::from_wide(&s[..s.iter().position(|&c| c == 0)?]))
    };
    let header = field(4)?;
    let (base, suffix) = if header >= 0x24 {
        (wide(field(0x1C)?)?, wide(field(0x20)?)?)
    } else {
        (ansi(field(0x10)?)?, ansi(field(0x18)?)?)
    };
    let mut path = base;
    path.push(suffix);
    (!path.is_empty()).then(|| PathBuf::from(path))
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used)]
    use super::*;

    /// The temp folder by its long name: CI's is an 8.3 short path
    /// (`RUNNER~1`), and the shell stores shortcut paths long.
    fn long_temp() -> PathBuf {
        let t = std::fs::canonicalize(std::env::temp_dir()).unwrap();
        let s = t.to_string_lossy();
        PathBuf::from(s.strip_prefix(r"\\?\").unwrap_or(&s))
    }

    #[test]
    fn a_shortcut_can_be_read_and_pointed_elsewhere() {
        let dir = long_temp().join(format!("oneup-lnk-{}", std::process::id()));
        let (old, new) = (dir.join("old"), dir.join("new"));
        std::fs::create_dir_all(&old).unwrap();
        std::fs::create_dir_all(&new).unwrap();
        std::fs::write(old.join("a.txt"), b"a").unwrap();
        std::fs::write(new.join("a.txt"), b"a").unwrap();
        let lnk = dir.join("a.lnk");
        create(&lnk, &old.join("a.txt"), Some(&old)).unwrap();
        let got = read(&lnk).unwrap();
        let a = Some(old.join("a.txt"));
        assert_eq!(got, Link { target: a.clone(), stored: a, working_dir: Some(old.clone()) });

        retarget(&lnk, &new.join("a.txt"), Some(&new)).unwrap();
        let got = read(&lnk).unwrap();
        let a = Some(new.join("a.txt"));
        assert_eq!(got, Link { target: a.clone(), stored: a, working_dir: Some(new.clone()) });
        std::fs::remove_dir_all(&dir).unwrap();
    }

    #[test]
    fn a_chain_the_shell_cant_follow_still_has_its_stored_path() {
        let dir = long_temp().join(format!("oneup-lnk-dead-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let (target, lnk) = (dir.join("tool.exe"), dir.join("tool.lnk"));
        std::fs::write(&target, b"x").unwrap();
        create(&lnk, &target, None).unwrap();
        let mut b = std::fs::read(&lnk).unwrap();
        // The first item is a root folder by CLSID (its size, type 0x1F,
        // sort, then the CLSID): make it one nothing registers.
        let item = 0x4C + 2;
        assert_eq!(b[item + 2], 0x1F, "a root item first");
        b[item + 4..item + 20].copy_from_slice(&[0xA5; 16]);
        std::fs::write(&lnk, &b).unwrap();
        let got = read(&lnk).unwrap();
        assert_eq!((got.target, got.stored), (None, Some(target.clone())));
        std::fs::remove_dir_all(&dir).unwrap();
    }

    /// An ANSI LinkInfo path outside ASCII: its code page is unknown.
    #[test]
    fn a_stored_ansi_path_outside_ascii_is_refused() {
        let dir = long_temp().join(format!("oneup-lnk-ansi-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let (target, lnk) = (dir.join("tool.exe"), dir.join("tool.lnk"));
        std::fs::write(&target, b"x").unwrap();
        create(&lnk, &target, None).unwrap();
        let mut b = std::fs::read(&lnk).unwrap();
        assert_eq!(stored_path(&b), Some(target.clone()));
        // The ANSI path's "tool.exe" in the LinkInfo (after the chain of
        // items), with one byte outside ASCII.
        let info = 0x4C + 2 + u16::from_le_bytes([b[0x4C], b[0x4D]]) as usize;
        let at = info + b[info..].windows(8).position(|w| w == b"tool.exe").unwrap();
        b[at] = 0xE9;
        assert_eq!(stored_path(&b), None);
        std::fs::remove_dir_all(&dir).unwrap();
    }

    #[test]
    fn a_shortcut_is_replaced_whole_keeping_its_streams() {
        let dir = long_temp().join(format!("oneup-lnk-replace-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let (a, b, lnk, tmp) = (dir.join("a.txt"), dir.join("b.txt"), dir.join("x.lnk"), dir.join("x.tmp"));
        std::fs::write(&a, b"a").unwrap();
        std::fs::write(&b, b"b").unwrap();
        create(&lnk, &a, None).unwrap();
        let mut zone = lnk.as_os_str().to_owned();
        zone.push(":Zone.Identifier");
        std::fs::write(&zone, b"[ZoneTransfer]").unwrap();
        retarget_to(&lnk, &tmp, &b, None).unwrap();
        assert_eq!(read(&lnk).unwrap().target, Some(a.clone()), "the original is untouched until replaced");
        replace(&lnk, &tmp).unwrap();
        assert_eq!(read(&lnk).unwrap().target, Some(b.clone()));
        assert!(!tmp.exists());
        assert_eq!(std::fs::read(&zone).unwrap(), b"[ZoneTransfer]", "other streams stay");
        std::fs::remove_dir_all(&dir).unwrap();
    }

    #[test]
    fn stored_paths_need_a_link_info() {
        assert_eq!(stored_path(b""), None);
        assert_eq!(stored_path(&[0u8; 0x4C]), None);
    }
}
