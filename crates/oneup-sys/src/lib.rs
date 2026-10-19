//! Thin Windows FFI helpers that don't belong to the NTFS or Cloud Files
//! layers: DPAPI encryption for secrets at rest, process metrics, disk
//! capacity, named pipes only the current user can open, and the
//! notification-area icon.
//!
//! Every `unsafe` block lives here, behind a small safe API.

#[derive(Debug, thiserror::Error)]
pub enum Error {
    #[cfg(windows)]
    #[error("{op} failed: {source}")]
    Win32 { op: &'static str, source: windows::core::Error },
    #[error("{op} failed: {source}")]
    Io { op: &'static str, source: std::io::Error },
    #[error("not supported on this platform")]
    Unsupported,
}

pub type Result<T> = std::result::Result<T, Error>;

#[cfg(windows)]
pub mod assoc;
#[cfg(windows)]
pub mod authenticode;
#[cfg(windows)]
pub mod clipboard;
#[cfg(windows)]
pub mod contained;
#[cfg(windows)]
pub mod hello;
#[cfg(windows)]
pub mod identity;
#[cfg(windows)]
pub mod in_use;
#[cfg(windows)]
pub mod known;
#[cfg(windows)]
pub mod locale;
#[cfg(windows)]
pub mod mutex;
#[cfg(windows)]
pub mod processes;
#[cfg(windows)]
pub mod shortcut;
#[cfg(windows)]
pub mod signal;
#[cfg(windows)]
pub mod tray;
#[cfg(windows)]
pub mod webview;

/// Windows DPAPI: encrypts data so that only the current Windows user
/// (on this machine) can decrypt it.
pub mod dpapi {
    use super::Result;
    use zeroize::Zeroizing;

    #[cfg(windows)]
    fn call(
        data: &[u8],
        op: &'static str,
        f: impl FnOnce(
            &windows::Win32::Security::Cryptography::CRYPT_INTEGER_BLOB,
            &mut windows::Win32::Security::Cryptography::CRYPT_INTEGER_BLOB,
        ) -> windows::core::Result<()>,
    ) -> Result<Zeroizing<Vec<u8>>> {
        use windows::Win32::Foundation::{HLOCAL, LocalFree};
        use windows::Win32::Security::Cryptography::CRYPT_INTEGER_BLOB;
        use zeroize::Zeroize as _;
        let len = u32::try_from(data.len()).map_err(|_| super::Error::Unsupported)?;
        let input = CRYPT_INTEGER_BLOB { cbData: len, pbData: data.as_ptr().cast_mut() };
        let mut out = CRYPT_INTEGER_BLOB::default();
        f(&input, &mut out).map_err(|source| super::Error::Win32 { op, source })?;
        // SAFETY: on success DPAPI returns a LocalAlloc'd buffer of `cbData`
        // bytes that only we hold, which we copy, wipe (it may be a decrypted
        // secret) and then free exactly once.
        unsafe {
            let buf = std::slice::from_raw_parts_mut(out.pbData, out.cbData as usize);
            let v = Zeroizing::new(buf.to_vec());
            buf.zeroize();
            let _ = LocalFree(Some(HLOCAL(out.pbData.cast())));
            Ok(v)
        }
    }

    #[cfg(windows)]
    pub fn protect(data: &[u8]) -> Result<Vec<u8>> {
        use windows::Win32::Security::Cryptography::CryptProtectData;
        let mut sealed = call(data, "CryptProtectData", |i, o| {
            // SAFETY: the input blob points at live data that DPAPI only reads.
            unsafe { CryptProtectData(i, None, None, None, None, 0, o) }
        })?;
        // Encrypted, so it needn't be wiped.
        Ok(std::mem::take(&mut *sealed))
    }

    /// The decrypted data, wiped from memory when dropped.
    #[cfg(windows)]
    pub fn unprotect(data: &[u8]) -> Result<Zeroizing<Vec<u8>>> {
        use windows::Win32::Security::Cryptography::CryptUnprotectData;
        call(data, "CryptUnprotectData", |i, o| {
            // SAFETY: as above.
            unsafe { CryptUnprotectData(i, None, None, None, None, 0, o) }
        })
    }

    #[cfg(not(windows))]
    pub fn protect(_: &[u8]) -> Result<Vec<u8>> {
        Err(super::Error::Unsupported)
    }

    #[cfg(not(windows))]
    pub fn unprotect(_: &[u8]) -> Result<Zeroizing<Vec<u8>>> {
        Err(super::Error::Unsupported)
    }
}

/// Process metrics for budgets and progress reporting.
pub mod metrics {
    /// Current and peak working set of this process, in bytes.
    #[derive(Debug, Clone, Copy, Default)]
    pub struct Memory {
        pub working_set: usize,
        pub peak_working_set: usize,
    }

    #[cfg(windows)]
    pub fn memory() -> Memory {
        use windows::Win32::System::ProcessStatus::{GetProcessMemoryInfo, PROCESS_MEMORY_COUNTERS};
        use windows::Win32::System::Threading::GetCurrentProcess;
        let mut m = PROCESS_MEMORY_COUNTERS::default();
        // SAFETY: `m` is a valid, correctly sized out-struct; the pseudo-handle needs no closing.
        let ok =
            unsafe { GetProcessMemoryInfo(GetCurrentProcess(), &mut m, size_of::<PROCESS_MEMORY_COUNTERS>() as u32) };
        if ok.is_ok() {
            Memory { working_set: m.WorkingSetSize, peak_working_set: m.PeakWorkingSetSize }
        } else {
            Memory::default()
        }
    }

    #[cfg(not(windows))]
    pub fn memory() -> Memory {
        Memory::default()
    }
}

/// Disk capacity, for limits expressed as a share of the disk.
pub mod disk {
    use super::Result;
    use std::path::Path;

    /// Total size in bytes of the volume holding `path` (as seen by this
    /// user; disk quotas can make it smaller than the physical disk).
    #[cfg(windows)]
    pub fn total_bytes(path: &Path) -> Result<u64> {
        use windows::Win32::Storage::FileSystem::GetDiskFreeSpaceExW;
        let p = windows::core::HSTRING::from(path.as_os_str());
        let mut total = 0u64;
        // SAFETY: `p` is a valid NUL-terminated wide string that outlives the
        // call, and `total` is a valid out-pointer; the other outputs are
        // optional and not requested.
        unsafe { GetDiskFreeSpaceExW(&p, None, Some(&mut total), None) }
            .map_err(|source| super::Error::Win32 { op: "GetDiskFreeSpaceExW", source })?;
        Ok(total)
    }

    #[cfg(not(windows))]
    pub fn total_bytes(_: &Path) -> Result<u64> {
        Err(super::Error::Unsupported)
    }

    /// Free bytes on the volume holding `path`, available to this user.
    #[cfg(windows)]
    pub fn free_bytes(path: &Path) -> Result<u64> {
        use windows::Win32::Storage::FileSystem::GetDiskFreeSpaceExW;
        let p = windows::core::HSTRING::from(path.as_os_str());
        let mut free = 0u64;
        // SAFETY: as in `total_bytes`.
        unsafe { GetDiskFreeSpaceExW(&p, Some(&mut free), None, None) }
            .map_err(|source| super::Error::Win32 { op: "GetDiskFreeSpaceExW", source })?;
        Ok(free)
    }

    #[cfg(not(windows))]
    pub fn free_bytes(_: &Path) -> Result<u64> {
        Err(super::Error::Unsupported)
    }
}

/// Named pipes for the agent's control API, usable by the current Windows
/// user only (`control-api.md`).
#[cfg(windows)]
pub mod pipe {
    use std::ffi::c_void;
    use tokio::net::windows::named_pipe::{NamedPipeServer, ServerOptions};
    use windows::Win32::Foundation::{CloseHandle, HANDLE, HLOCAL, LocalFree};
    use windows::Win32::Security::Authorization::{
        ConvertSidToStringSidW, ConvertStringSecurityDescriptorToSecurityDescriptorW, SDDL_REVISION_1,
    };
    use windows::Win32::Security::{
        GetTokenInformation, PSECURITY_DESCRIPTOR, SECURITY_ATTRIBUTES, TOKEN_QUERY, TOKEN_USER, TokenUser,
    };
    use windows::Win32::System::Threading::{GetCurrentProcess, OpenProcessToken};
    use windows::core::{HSTRING, PWSTR};

    fn io(op: &'static str, e: windows::core::Error) -> std::io::Error {
        std::io::Error::other(format!("{op}: {e}"))
    }

    /// The current user's SID, as a string (`S-1-5-21-…`).
    pub fn current_user_sid() -> std::io::Result<String> {
        // SAFETY: the pseudo-handle of the current process is always valid.
        user_sid_of(unsafe { GetCurrentProcess() })
    }

    /// The SID of the user running `process` (a handle with at least
    /// limited query access), as a string.
    pub(crate) fn user_sid_of(process: HANDLE) -> std::io::Result<String> {
        let mut token = HANDLE::default();
        // SAFETY: `process` is a valid process handle (the caller's
        // promise); `token` is a valid out-pointer.
        unsafe { OpenProcessToken(process, TOKEN_QUERY, &mut token) }.map_err(|e| io("OpenProcessToken", e))?;
        struct Owned(HANDLE);
        impl Drop for Owned {
            fn drop(&mut self) {
                // SAFETY: we own the token handle and close it once.
                let _ = unsafe { CloseHandle(self.0) };
            }
        }
        let token = Owned(token);
        let mut len = 0u32;
        // SAFETY: a size query (no buffer); it fails with "insufficient
        // buffer" and sets `len`, which is all we want from it.
        let _ = unsafe { GetTokenInformation(token.0, TokenUser, None, 0, &mut len) };
        // u64 words keep the buffer aligned for TOKEN_USER (pointers).
        let mut buf = vec![0u64; (len as usize).div_ceil(8).max(1)];
        // SAFETY: `buf` is writable, aligned and at least `len` bytes.
        unsafe { GetTokenInformation(token.0, TokenUser, Some(buf.as_mut_ptr().cast()), len, &mut len) }
            .map_err(|e| io("GetTokenInformation", e))?;
        // SAFETY: the call filled `buf` with a TOKEN_USER, whose SID points
        // into `buf`, which outlives its use below.
        let sid = unsafe { (*buf.as_ptr().cast::<TOKEN_USER>()).User.Sid };
        let mut text = PWSTR::null();
        // SAFETY: `sid` is valid (above); `text` is a valid out-pointer.
        unsafe { ConvertSidToStringSidW(sid, &mut text) }.map_err(|e| io("ConvertSidToStringSidW", e))?;
        // SAFETY: on success `text` is a NUL-terminated string the OS
        // allocated with LocalAlloc; we copy it here...
        let out = unsafe { text.to_string() };
        // SAFETY: ...and free it once, after the copy.
        unsafe {
            let _ = LocalFree(Some(HLOCAL(text.0.cast())));
        }
        out.map_err(|e| std::io::Error::other(format!("SID text: {e}")))
    }

    /// Creates an instance of the pipe `name` (`\\.\pipe\…`) that only the
    /// current user can open (a protected DACL with one entry), for local
    /// clients only. `first`: fail if the name already exists, so another
    /// process can't have created it first and be listening in our place.
    pub fn create(name: &str, first: bool) -> std::io::Result<NamedPipeServer> {
        let sddl = HSTRING::from(format!("D:P(A;;GA;;;{})", current_user_sid()?));
        let mut sd = PSECURITY_DESCRIPTOR::default();
        // SAFETY: `sddl` is a valid NUL-terminated wide string; `sd` is a
        // valid out-pointer; the size output isn't requested.
        unsafe { ConvertStringSecurityDescriptorToSecurityDescriptorW(&sddl, SDDL_REVISION_1, &mut sd, None) }
            .map_err(|e| io("ConvertStringSecurityDescriptorToSecurityDescriptorW", e))?;
        let mut attrs = SECURITY_ATTRIBUTES {
            nLength: size_of::<SECURITY_ATTRIBUTES>() as u32,
            lpSecurityDescriptor: sd.0,
            bInheritHandle: false.into(),
        };
        let mut opts = ServerOptions::new();
        opts.first_pipe_instance(first).reject_remote_clients(true);
        // SAFETY: `attrs` is a valid SECURITY_ATTRIBUTES whose descriptor
        // stays alive until after the call (freed below); the pipe keeps its
        // own copy.
        let server = unsafe { opts.create_with_security_attributes_raw(name, (&raw mut attrs).cast::<c_void>()) };
        // SAFETY: `sd` was allocated by the conversion above, freed once.
        unsafe {
            let _ = LocalFree(Some(HLOCAL(sd.0)));
        }
        server
    }

    /// The access list (DACL) of an open handle, in SDDL form: lets tests
    /// check what [`create`] set, on the live pipe.
    pub fn dacl_sddl(h: &impl std::os::windows::io::AsRawHandle) -> std::io::Result<String> {
        use windows::Win32::Security::Authorization::{
            ConvertSecurityDescriptorToStringSecurityDescriptorW, GetSecurityInfo, SE_KERNEL_OBJECT,
        };
        use windows::Win32::Security::DACL_SECURITY_INFORMATION;
        let mut sd = PSECURITY_DESCRIPTOR::default();
        // SAFETY: the handle is open for the borrow; `sd` is a valid
        // out-pointer, and the other outputs aren't requested.
        let r = unsafe {
            GetSecurityInfo(
                HANDLE(h.as_raw_handle()),
                SE_KERNEL_OBJECT,
                DACL_SECURITY_INFORMATION,
                None,
                None,
                None,
                None,
                Some(&mut sd),
            )
        };
        if r.0 != 0 {
            return Err(std::io::Error::from_raw_os_error(r.0 as i32));
        }
        let mut text = PWSTR::null();
        // SAFETY: `sd` came from GetSecurityInfo; `text` is a valid out-pointer.
        let conv = unsafe {
            ConvertSecurityDescriptorToStringSecurityDescriptorW(
                sd,
                SDDL_REVISION_1,
                DACL_SECURITY_INFORMATION,
                &mut text,
                None,
            )
        };
        // SAFETY: on success `text` is a NUL-terminated string we copy here...
        let out = conv
            .map_err(|e| io("ConvertSecurityDescriptorToStringSecurityDescriptorW", e))
            .and_then(|()| unsafe { text.to_string() }.map_err(|e| std::io::Error::other(format!("SDDL text: {e}"))));
        // SAFETY: ...and both buffers were allocated by the OS with
        // LocalAlloc; each is freed once (a null one is ignored).
        unsafe {
            let _ = LocalFree(Some(HLOCAL(text.0.cast())));
            let _ = LocalFree(Some(HLOCAL(sd.0)));
        }
        out
    }
}

/// What the PC is doing that may call for a pause (`rules.md` § User controls: auto-pause
/// on a metered connection or battery saver). Each is one cheap query; the
/// agent asks when it wakes anyway, so nothing polls.
#[cfg(windows)]
pub mod conditions {
    use windows::Networking::Connectivity::{NetworkCostType, NetworkInformation};
    use windows::Win32::System::Power::{GetSystemPowerStatus, SYSTEM_POWER_STATUS};
    use windows::Win32::UI::Shell::{
        QUNS_BUSY, QUNS_PRESENTATION_MODE, QUNS_RUNNING_D3D_FULL_SCREEN, SHQueryUserNotificationState,
    };
    use windows::Win32::UI::WindowsAndMessaging::{GetForegroundWindow, GetWindowThreadProcessId};

    /// What has the whole screen, if anything.
    #[derive(Debug, Clone, Copy, PartialEq, Eq)]
    pub enum FullScreen {
        None,
        /// A game, a presentation, or another program that took the screen.
        Program,
        /// A browser or a video player: a video, as far as can be told.
        Video,
    }

    /// Programs whose full screen is a video: browsers and video players
    /// (lowercase file names). A game in a browser counts as a video.
    const VIDEO: &[&str] = &[
        "msedge.exe",
        "chrome.exe",
        "firefox.exe",
        "brave.exe",
        "opera.exe",
        "vivaldi.exe",
        "arc.exe",
        "vlc.exe",
        "mpv.exe",
        "mpc-hc.exe",
        "mpc-hc64.exe",
        "mpc-be.exe",
        "mpc-be64.exe",
        "potplayermini.exe",
        "potplayermini64.exe",
        "wmplayer.exe",
        "video.ui.exe",
        "microsoft.media.player.exe",
        "plex.exe",
        "plex htpc.exe",
        "kodi.exe",
        "netflix.exe",
    ];

    /// Which kind of full screen, from Windows' notification state and
    /// the foreground program's file name. Windows tells exclusive
    /// full-screen DirectX (games) and presentations apart, but reports a
    /// full-screen video and a borderless game alike ("busy"): the program
    /// tells them apart.
    pub fn classify(state: i32, foreground: Option<&str>) -> FullScreen {
        if state == QUNS_RUNNING_D3D_FULL_SCREEN.0 || state == QUNS_PRESENTATION_MODE.0 {
            return FullScreen::Program;
        }
        if state != QUNS_BUSY.0 {
            return FullScreen::None;
        }
        match foreground {
            Some(exe) if VIDEO.contains(&exe.to_lowercase().as_str()) => FullScreen::Video,
            _ => FullScreen::Program,
        }
    }

    /// What has the whole screen now.
    pub fn full_screen() -> FullScreen {
        // SAFETY: no arguments besides the out value the wrapper manages.
        let Ok(state) = (unsafe { SHQueryUserNotificationState() }) else { return FullScreen::None };
        if state != QUNS_BUSY {
            return classify(state.0, None);
        }
        let mut pid = 0u32;
        // SAFETY: plain queries; `pid` is a valid out location.
        unsafe { GetWindowThreadProcessId(GetForegroundWindow(), Some(&mut pid)) };
        let exe = (pid != 0)
            .then(|| crate::processes::image_of(pid))
            .flatten()
            .and_then(|p| p.file_name().map(|n| n.to_string_lossy().into_owned()));
        classify(state.0, exe.as_deref())
    }

    /// Whether Windows' battery saver is on.
    pub fn battery_saver() -> bool {
        let mut s = SYSTEM_POWER_STATUS::default();
        // SAFETY: `s` is a valid, writable structure for the call.
        unsafe { GetSystemPowerStatus(&mut s) }.is_ok() && s.SystemStatusFlag == 1
    }

    /// Whether the internet connection is metered (a cost per byte or a
    /// data cap, roaming, or over its limit). `false` when there's no
    /// connection or it can't be told.
    pub fn metered() -> bool {
        let check = || -> windows::core::Result<bool> {
            let cost = NetworkInformation::GetInternetConnectionProfile()?.GetConnectionCost()?;
            let kind = cost.NetworkCostType()?;
            Ok(kind == NetworkCostType::Fixed
                || kind == NetworkCostType::Variable
                || cost.Roaming()?
                || cost.OverDataLimit()?)
        };
        check().unwrap_or(false)
    }
}

/// How this process competes for the PC (`rules.md` § User controls, CPU).
#[cfg(windows)]
pub mod priority {
    use std::ffi::c_void;
    use windows::Win32::System::Threading::{
        GetCurrentProcess, PROCESS_MODE_BACKGROUND_BEGIN, PROCESS_MODE_BACKGROUND_END,
        PROCESS_POWER_THROTTLING_CURRENT_VERSION, PROCESS_POWER_THROTTLING_EXECUTION_SPEED,
        PROCESS_POWER_THROTTLING_STATE, ProcessPowerThrottling, SetPriorityClass, SetProcessInformation,
    };

    /// `on`: background mode (lower CPU, disk and memory priority, so the
    /// user's programs come first) and EcoQoS (Windows may run it on
    /// efficient cores at lower clock speeds). `off`: normal again.
    pub fn set_background(on: bool) -> std::io::Result<()> {
        let mode = if on { PROCESS_MODE_BACKGROUND_BEGIN } else { PROCESS_MODE_BACKGROUND_END };
        let state = PROCESS_POWER_THROTTLING_STATE {
            Version: PROCESS_POWER_THROTTLING_CURRENT_VERSION,
            ControlMask: PROCESS_POWER_THROTTLING_EXECUTION_SPEED,
            StateMask: if on { PROCESS_POWER_THROTTLING_EXECUTION_SPEED } else { 0 },
        };
        // SAFETY: the pseudo-handle of the current process needs no
        // closing; `state` is a valid, correctly sized structure that
        // outlives the call.
        unsafe {
            let me = GetCurrentProcess();
            // Asking for the mode the process is already in fails with
            // ERROR_PROCESS_MODE_ALREADY_BACKGROUND (or _NOT_BACKGROUND):
            // that's the wanted state, so it isn't an error.
            if let Err(e) = SetPriorityClass(me, mode)
                && !matches!(e.code().0 as u32 & 0xFFFF, 402 | 403)
            {
                return Err(std::io::Error::other(e));
            }
            SetProcessInformation(
                me,
                ProcessPowerThrottling,
                (&raw const state).cast::<c_void>(),
                std::mem::size_of::<PROCESS_POWER_THROTTLING_STATE>() as u32,
            )
            .map_err(std::io::Error::other)
        }
    }

    /// Background mode for the calling thread alone (lower CPU, disk and
    /// memory priority): for threads that only start and watch other
    /// programs, so they never compete with the sync work in the process.
    pub fn set_thread_background() -> std::io::Result<()> {
        use windows::Win32::System::Threading::{GetCurrentThread, SetThreadPriority, THREAD_MODE_BACKGROUND_BEGIN};
        // SAFETY: the pseudo-handle of the current thread needs no closing.
        match unsafe { SetThreadPriority(GetCurrentThread(), THREAD_MODE_BACKGROUND_BEGIN) } {
            // Already in background mode (ERROR_THREAD_MODE_ALREADY_BACKGROUND).
            Err(e) if e.code().0 as u32 & 0xFFFF == 400 => Ok(()),
            r => r.map_err(std::io::Error::other),
        }
    }
}

/// Handles this process passes on to programs it starts.
#[cfg(windows)]
pub mod inherit {
    use windows::Win32::Foundation::{HANDLE_FLAG_INHERIT, HANDLE_FLAGS, SetHandleInformation};
    use windows::Win32::System::Console::{GetStdHandle, STD_ERROR_HANDLE, STD_INPUT_HANDLE, STD_OUTPUT_HANDLE};

    /// Stops this process's standard input, output and error from being
    /// inherited by programs it starts later. Windows hands a child *every*
    /// inheritable handle, not just the ones set as its standard handles; a
    /// long-lived child that holds the terminal's output pipe keeps whoever
    /// reads that pipe waiting until the child ends. Programs started with
    /// `Stdio::inherit` still get them (Rust duplicates them for the child).
    pub fn keep_std_handles() {
        for which in [STD_INPUT_HANDLE, STD_OUTPUT_HANDLE, STD_ERROR_HANDLE] {
            // SAFETY: `GetStdHandle` returns this process's handle (or an
            // error if there is none); clearing a flag on a valid handle we
            // don't close has no other effect. Failures are harmless: the
            // handle then stays as it was.
            unsafe {
                if let Ok(h) = GetStdHandle(which)
                    && !h.is_invalid()
                {
                    let _ = SetHandleInformation(h, HANDLE_FLAG_INHERIT.0, HANDLE_FLAGS(0));
                }
            }
        }
    }
}

/// String values under `HKEY_CURRENT_USER` (per user, no admin rights):
/// what "start at sign-in" needs.
#[cfg(windows)]
pub mod user_registry {
    use std::ffi::c_void;
    use windows::Win32::Foundation::{ERROR_FILE_NOT_FOUND, ERROR_SUCCESS, WIN32_ERROR};
    use windows::Win32::System::Registry::{
        HKEY_CURRENT_USER, REG_SZ, RRF_RT_REG_DWORD, RRF_RT_REG_QWORD, RRF_RT_REG_SZ, RegDeleteKeyValueW,
        RegDeleteTreeW, RegGetValueW, RegSetKeyValueW,
    };
    use windows::core::HSTRING;

    fn check(op: &'static str, r: WIN32_ERROR) -> std::io::Result<()> {
        if r == ERROR_SUCCESS {
            Ok(())
        } else {
            Err(std::io::Error::new(std::io::Error::from_raw_os_error(r.0 as i32).kind(), format!("{op}: {r:?}")))
        }
    }

    /// Sets string value `name` of key `key` (created if missing).
    pub fn set(key: &str, name: &str, value: &str) -> std::io::Result<()> {
        let data: Vec<u16> = value.encode_utf16().chain([0]).collect();
        // SAFETY: the strings are NUL-terminated and outlive the call; `data`
        // is `cbdata` bytes long.
        let r = unsafe {
            RegSetKeyValueW(
                HKEY_CURRENT_USER,
                &HSTRING::from(key),
                &HSTRING::from(name),
                REG_SZ.0,
                Some(data.as_ptr().cast::<c_void>()),
                (data.len() * 2) as u32,
            )
        };
        check("RegSetKeyValueW", r)
    }

    /// String value `name` of key `key`, if there is one.
    pub fn get(key: &str, name: &str) -> std::io::Result<Option<String>> {
        let (k, n) = (HSTRING::from(key), HSTRING::from(name));
        let mut len = 0u32;
        // SAFETY: a size query: no buffer, `len` is a valid out-pointer.
        let r = unsafe { RegGetValueW(HKEY_CURRENT_USER, &k, &n, RRF_RT_REG_SZ, None, None, Some(&mut len)) };
        if r == ERROR_FILE_NOT_FOUND {
            return Ok(None);
        }
        check("RegGetValueW", r)?;
        let mut buf = vec![0u16; (len as usize).div_ceil(2).max(1)];
        let mut bytes = (buf.len() * 2) as u32;
        // SAFETY: `buf` is writable and `bytes` long.
        let r = unsafe {
            RegGetValueW(
                HKEY_CURRENT_USER,
                &k,
                &n,
                RRF_RT_REG_SZ,
                None,
                Some(buf.as_mut_ptr().cast::<c_void>()),
                Some(&mut bytes),
            )
        };
        if r == ERROR_FILE_NOT_FOUND {
            return Ok(None);
        }
        check("RegGetValueW", r)?;
        let end = buf.iter().position(|&c| c == 0).unwrap_or(buf.len());
        Ok(Some(String::from_utf16_lossy(&buf[..end])))
    }

    /// Number (DWORD) value `name` of key `key`, if there is one.
    pub fn get_dword(key: &str, name: &str) -> std::io::Result<Option<u32>> {
        let mut v = 0u32;
        let mut len = size_of::<u32>() as u32;
        // SAFETY: `v` is writable and `len` bytes long.
        let r = unsafe {
            RegGetValueW(
                HKEY_CURRENT_USER,
                &HSTRING::from(key),
                &HSTRING::from(name),
                RRF_RT_REG_DWORD,
                None,
                Some(std::ptr::from_mut(&mut v).cast::<c_void>()),
                Some(&mut len),
            )
        };
        if r == ERROR_FILE_NOT_FOUND {
            return Ok(None);
        }
        check("RegGetValueW", r).map(|()| Some(v))
    }

    /// A number value `name` of key `key`, stored as a QWORD or a DWORD, if
    /// there is one.
    pub fn get_number(key: &str, name: &str) -> std::io::Result<Option<u64>> {
        let mut v = 0u64;
        let mut len = size_of::<u64>() as u32;
        // SAFETY: `v` is writable and `len` bytes long.
        let r = unsafe {
            RegGetValueW(
                HKEY_CURRENT_USER,
                &HSTRING::from(key),
                &HSTRING::from(name),
                RRF_RT_REG_QWORD,
                None,
                Some(std::ptr::from_mut(&mut v).cast::<c_void>()),
                Some(&mut len),
            )
        };
        if r == ERROR_FILE_NOT_FOUND {
            return Ok(None);
        }
        if r == ERROR_SUCCESS {
            return Ok(Some(v));
        }
        // Not a QWORD: maybe a DWORD.
        get_dword(key, name).map(|d| d.map(u64::from))
    }

    /// Every string value of key `key` (name, data); empty if there's no key.
    pub fn values(key: &str) -> std::io::Result<Vec<(String, String)>> {
        values_under(HKEY_CURRENT_USER, key)
    }

    /// [`values`] under `HKEY_LOCAL_MACHINE` (read only), for machine-wide
    /// policies.
    pub fn machine_values(key: &str) -> std::io::Result<Vec<(String, String)>> {
        values_under(windows::Win32::System::Registry::HKEY_LOCAL_MACHINE, key)
    }

    fn values_under(root: windows::Win32::System::Registry::HKEY, key: &str) -> std::io::Result<Vec<(String, String)>> {
        use windows::Win32::System::Registry::{HKEY, KEY_READ, RegCloseKey, RegEnumValueW, RegOpenKeyExW};
        let mut h = HKEY::default();
        // SAFETY: the key name outlives the call; `h` is a valid out-pointer.
        let r = unsafe { RegOpenKeyExW(root, &HSTRING::from(key), None, KEY_READ, &mut h) };
        if r == ERROR_FILE_NOT_FOUND {
            return Ok(Vec::new());
        }
        check("RegOpenKeyExW", r)?;
        let mut out = Vec::new();
        for i in 0.. {
            let mut name = vec![0u16; 16_384];
            let mut name_len = name.len() as u32;
            let mut kind = 0u32;
            let mut data = vec![0u8; 64 * 1024];
            let mut data_len = data.len() as u32;
            // SAFETY: every buffer is writable for the length given with it.
            let r = unsafe {
                RegEnumValueW(
                    h,
                    i,
                    Some(windows::core::PWSTR(name.as_mut_ptr())),
                    &mut name_len,
                    None,
                    Some(&mut kind),
                    Some(data.as_mut_ptr()),
                    Some(&mut data_len),
                )
            };
            if r != ERROR_SUCCESS {
                break;
            }
            if kind != REG_SZ.0 {
                continue;
            }
            let wide: Vec<u16> =
                data[..data_len as usize].as_chunks::<2>().0.iter().map(|&c| u16::from_le_bytes(c)).collect();
            let end = wide.iter().position(|&c| c == 0).unwrap_or(wide.len());
            out.push((String::from_utf16_lossy(&name[..name_len as usize]), String::from_utf16_lossy(&wide[..end])));
        }
        // SAFETY: closing the key we opened, once.
        let _ = unsafe { RegCloseKey(h) };
        Ok(out)
    }

    /// Removes value `name` of key `key`; `false` if it wasn't there.
    pub fn remove(key: &str, name: &str) -> std::io::Result<bool> {
        // SAFETY: the strings are NUL-terminated and outlive the call.
        let r = unsafe { RegDeleteKeyValueW(HKEY_CURRENT_USER, &HSTRING::from(key), &HSTRING::from(name)) };
        if r == ERROR_FILE_NOT_FOUND {
            return Ok(false);
        }
        check("RegDeleteKeyValueW", r).map(|()| true)
    }

    /// Removes key `key` and everything in it (tests clean up with this).
    pub fn remove_key(key: &str) -> std::io::Result<()> {
        // SAFETY: the string is NUL-terminated and outlives the call.
        let r = unsafe { RegDeleteTreeW(HKEY_CURRENT_USER, &HSTRING::from(key)) };
        if r == ERROR_FILE_NOT_FOUND { Ok(()) } else { check("RegDeleteTreeW", r) }
    }

    /// One value of a key as stored: its name (empty for the key's default
    /// value), its registry type (`REG_SZ` is 1, `REG_DWORD` 4…) and its
    /// raw bytes.
    #[derive(Debug, Clone, PartialEq, Eq)]
    pub struct RawValue {
        pub name: String,
        pub kind: u32,
        pub data: Vec<u8>,
    }

    /// A key and everything under it, each list sorted by name.
    #[derive(Debug, Clone, PartialEq, Eq, Default)]
    pub struct Tree {
        pub values: Vec<RawValue>,
        pub keys: Vec<(String, Tree)>,
    }

    impl Tree {
        /// Value `name`'s text, if it's a string value.
        pub fn text(&self, name: &str) -> Option<String> {
            let v = self.values.iter().find(|v| v.name.eq_ignore_ascii_case(name))?;
            if v.kind != REG_SZ.0 && v.kind != windows::Win32::System::Registry::REG_EXPAND_SZ.0 {
                return None;
            }
            let wide: Vec<u16> = v.data.as_chunks::<2>().0.iter().map(|&c| u16::from_le_bytes(c)).collect();
            let end = wide.iter().position(|&c| c == 0).unwrap_or(wide.len());
            Some(String::from_utf16_lossy(&wide[..end]))
        }

        /// Sub-key `name`.
        pub fn key(&self, name: &str) -> Option<&Tree> {
            self.keys.iter().find(|(n, _)| n.eq_ignore_ascii_case(name)).map(|(_, t)| t)
        }
    }

    /// Deeper than this, a key isn't read (the keys this is for are a few
    /// levels deep).
    const MAX_DEPTH: usize = 16;
    /// More values or sub-keys than this in one key, and it isn't read.
    const MAX_ENTRIES: u32 = 4096;

    /// Key `key` with every value and sub-key under it, as stored; `None` if
    /// there's no such key. An error for a key deeper than 16 levels or with
    /// more than 4096 entries in one key.
    pub fn tree(key: &str) -> std::io::Result<Option<Tree>> {
        tree_at(key, 0)
    }

    fn tree_at(key: &str, depth: usize) -> std::io::Result<Option<Tree>> {
        use windows::Win32::Foundation::{ERROR_MORE_DATA, ERROR_NO_MORE_ITEMS};
        use windows::Win32::System::Registry::{
            HKEY, KEY_READ, RegCloseKey, RegEnumKeyExW, RegEnumValueW, RegOpenKeyExW,
        };
        if depth > MAX_DEPTH {
            return Err(std::io::Error::other(format!("{key}: nested too deep")));
        }
        let mut h = HKEY::default();
        // SAFETY: the key name outlives the call; `h` is a valid out-pointer.
        let r = unsafe { RegOpenKeyExW(HKEY_CURRENT_USER, &HSTRING::from(key), None, KEY_READ, &mut h) };
        if r == ERROR_FILE_NOT_FOUND {
            return Ok(None);
        }
        check("RegOpenKeyExW", r)?;
        let read = || -> std::io::Result<(Vec<RawValue>, Vec<String>)> {
            let mut values = Vec::new();
            let mut data = vec![0u8; 4096];
            let mut i = 0u32;
            loop {
                if i >= MAX_ENTRIES {
                    return Err(std::io::Error::other(format!("{key}: too many values")));
                }
                // Value names are at most 16,383 characters.
                let mut name = vec![0u16; 16_384];
                let mut name_len = name.len() as u32;
                let mut kind = 0u32;
                let mut data_len = data.len() as u32;
                // SAFETY: every buffer is writable for the length given with it.
                let r = unsafe {
                    RegEnumValueW(
                        h,
                        i,
                        Some(windows::core::PWSTR(name.as_mut_ptr())),
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
                if r == ERROR_MORE_DATA {
                    // The data is bigger than the buffer: again, with room.
                    data.resize((data_len as usize).max(data.len() * 2), 0);
                    continue;
                }
                check("RegEnumValueW", r)?;
                values.push(RawValue {
                    name: String::from_utf16_lossy(&name[..name_len as usize]),
                    kind,
                    data: data[..data_len as usize].to_vec(),
                });
                i += 1;
            }
            let mut keys = Vec::new();
            for i in 0.. {
                if i >= MAX_ENTRIES {
                    return Err(std::io::Error::other(format!("{key}: too many sub-keys")));
                }
                // Key names are at most 255 characters.
                let mut name = vec![0u16; 256];
                let mut name_len = name.len() as u32;
                // SAFETY: `name` is writable for `name_len` characters.
                let r = unsafe {
                    RegEnumKeyExW(
                        h,
                        i,
                        Some(windows::core::PWSTR(name.as_mut_ptr())),
                        &mut name_len,
                        None,
                        None,
                        None,
                        None,
                    )
                };
                if r == ERROR_NO_MORE_ITEMS {
                    break;
                }
                check("RegEnumKeyExW", r)?;
                keys.push(String::from_utf16_lossy(&name[..name_len as usize]));
            }
            Ok((values, keys))
        };
        let got = read();
        // SAFETY: closing the key we opened, once.
        let _ = unsafe { RegCloseKey(h) };
        let (mut values, names) = got?;
        values.sort_by_key(|v| v.name.to_lowercase());
        let mut keys = Vec::new();
        for name in names {
            // A sub-key removed meanwhile is simply not there.
            if let Some(t) = tree_at(&format!(r"{key}\{name}"), depth + 1)? {
                keys.push((name, t));
            }
        }
        keys.sort_by_key(|k| k.0.to_lowercase());
        Ok(Some(Tree { values, keys }))
    }

    /// Puts `tree` back under key `key`: creates the keys that are missing
    /// and sets the values that are missing, and never changes a value
    /// that's there. Returns how many values it set.
    pub fn restore_missing(key: &str, tree: &Tree) -> std::io::Result<usize> {
        use windows::Win32::System::Registry::{
            HKEY, KEY_WRITE, REG_OPTION_NON_VOLATILE, RRF_NOEXPAND, RRF_RT_ANY, RegCloseKey, RegCreateKeyExW,
        };
        let k = HSTRING::from(key);
        let mut h = HKEY::default();
        // SAFETY: the key name outlives the call; `h` is a valid out-pointer.
        let r = unsafe {
            RegCreateKeyExW(HKEY_CURRENT_USER, &k, None, None, REG_OPTION_NON_VOLATILE, KEY_WRITE, None, &mut h, None)
        };
        check("RegCreateKeyExW", r)?;
        // SAFETY: closing the key we created or opened, once.
        let _ = unsafe { RegCloseKey(h) };
        let mut set = 0;
        for v in &tree.values {
            let n = HSTRING::from(v.name.as_str());
            // SAFETY: a presence query: no buffers.
            let r = unsafe { RegGetValueW(HKEY_CURRENT_USER, &k, &n, RRF_RT_ANY | RRF_NOEXPAND, None, None, None) };
            if r != ERROR_FILE_NOT_FOUND {
                check("RegGetValueW", r)?;
                continue;
            }
            // SAFETY: the strings are NUL-terminated and outlive the call;
            // `v.data` is `cbdata` bytes long.
            let r = unsafe {
                RegSetKeyValueW(
                    HKEY_CURRENT_USER,
                    &k,
                    &n,
                    v.kind,
                    Some(v.data.as_ptr().cast::<c_void>()),
                    u32::try_from(v.data.len()).map_err(std::io::Error::other)?,
                )
            };
            check("RegSetKeyValueW", r)?;
            set += 1;
        }
        for (name, sub) in &tree.keys {
            set += restore_missing(&format!(r"{key}\{name}"), sub)?;
        }
        Ok(set)
    }
}

#[cfg(all(test, windows))]
mod tests {
    #[test]
    fn conditions_can_be_asked() {
        // Values depend on the PC; the calls must just work.
        let _ = (super::conditions::battery_saver(), super::conditions::metered(), super::conditions::full_screen());
    }

    #[test]
    fn videos_are_told_from_games_by_the_program() {
        use super::conditions::{FullScreen, classify};
        use windows::Win32::UI::Shell::{QUNS_ACCEPTS_NOTIFICATIONS, QUNS_BUSY, QUNS_RUNNING_D3D_FULL_SCREEN};
        assert_eq!(classify(QUNS_ACCEPTS_NOTIFICATIONS.0, Some("chrome.exe")), FullScreen::None);
        assert_eq!(classify(QUNS_BUSY.0, Some("Chrome.EXE")), FullScreen::Video);
        assert_eq!(classify(QUNS_BUSY.0, Some("vlc.exe")), FullScreen::Video);
        assert_eq!(classify(QUNS_BUSY.0, Some("eldenring.exe")), FullScreen::Program, "a borderless game");
        assert_eq!(classify(QUNS_BUSY.0, None), FullScreen::Program);
        assert_eq!(classify(QUNS_RUNNING_D3D_FULL_SCREEN.0, Some("chrome.exe")), FullScreen::Program);
    }

    #[test]
    fn background_mode_switches_on_and_off_twice() {
        // Twice each way: asking for the current mode isn't an error.
        for on in [true, true, false, false] {
            super::priority::set_background(on).unwrap();
        }
    }

    #[test]
    fn std_handles_stop_being_inheritable() {
        use windows::Win32::Foundation::{GetHandleInformation, HANDLE_FLAG_INHERIT};
        use windows::Win32::System::Console::{GetStdHandle, STD_ERROR_HANDLE, STD_OUTPUT_HANDLE};
        super::inherit::keep_std_handles();
        for which in [STD_OUTPUT_HANDLE, STD_ERROR_HANDLE] {
            // SAFETY: reads flags of this process's own handle.
            unsafe {
                let Ok(h) = GetStdHandle(which) else { continue };
                if h.is_invalid() {
                    continue;
                }
                let mut flags = 0u32;
                if GetHandleInformation(h, &mut flags).is_ok() {
                    assert_eq!(flags & HANDLE_FLAG_INHERIT.0, 0);
                }
            }
        }
    }

    #[test]
    fn user_registry_values_round_trip_in_a_scratch_key() {
        use super::user_registry as reg;
        let key = format!(r"Software\OneUpDrive-test-{}", std::process::id());
        assert_eq!(reg::get(&key, "v").unwrap(), None);
        reg::set(&key, "v", r#""C:\x y\a.exe" --data-dir "D:\z""#).unwrap();
        assert_eq!(reg::get(&key, "v").unwrap().as_deref(), Some(r#""C:\x y\a.exe" --data-dir "D:\z""#));
        assert!(reg::remove(&key, "v").unwrap());
        assert!(!reg::remove(&key, "v").unwrap());
        reg::remove_key(&key).unwrap();
    }

    #[test]
    fn a_registry_tree_is_read_whole_and_put_back_without_overwriting() {
        use super::user_registry::{self as reg, RawValue, Tree};
        /// Removes the scratch key however the test ends.
        struct Scratch(String);
        impl Drop for Scratch {
            fn drop(&mut self) {
                let _ = reg::remove_key(&self.0);
            }
        }
        let nanos = std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).unwrap().as_nanos();
        let scratch = Scratch(format!(r"Software\OneUpDrive-test-tree-{}-{nanos}", std::process::id()));
        let key = scratch.0.as_str();
        assert_eq!(reg::tree(key).unwrap(), None);
        let wide = |s: &str| s.encode_utf16().flat_map(u16::to_le_bytes).collect::<Vec<u8>>();
        let sz = |s: &str| wide(&format!("{s}\0"));
        let v = |name: &str, kind: u32, data: Vec<u8>| RawValue { name: name.into(), kind, data };
        let big = vec![7u8; 10_000];
        let want = Tree {
            values: vec![
                v("", 1, sz("Default")),
                v("Big", 3, big),
                // Kept as stored: not expanded, every string of the list.
                v("Expand", 2, sz(r"%LOCALAPPDATA%\x")),
                v("Multi", 7, wide("one\0two\0\0")),
                v("n", 4, 5u32.to_le_bytes().to_vec()),
            ],
            keys: vec![
                ("Empty".into(), Tree::default()),
                // A default value with no bytes at all.
                ("EmptyDefault".into(), Tree { values: vec![v("", 1, Vec::new())], keys: vec![] }),
                ("Sub".into(), Tree { values: vec![v("q", 11, 9u64.to_le_bytes().to_vec())], keys: vec![] }),
            ],
        };
        assert_eq!(reg::restore_missing(key, &want).unwrap(), 7);
        let got = reg::tree(key).unwrap().unwrap();
        assert_eq!(got, want);
        assert_eq!(got.text("").as_deref(), Some("Default"));
        assert_eq!(got.text("Expand").as_deref(), Some(r"%LOCALAPPDATA%\x"));
        assert_eq!(got.text("n"), None, "a number isn't text");
        // Putting back again sets nothing, and a value changed since stays.
        reg::set(key, "", "Changed").unwrap();
        assert_eq!(reg::restore_missing(key, &want).unwrap(), 0);
        assert_eq!(reg::get(key, "").unwrap().as_deref(), Some("Changed"));
        reg::remove_key(key).unwrap();
        assert_eq!(reg::tree(key).unwrap(), None);
    }

    #[tokio::test]
    async fn control_pipes_admit_only_the_current_user() {
        let name = format!(r"\\.\pipe\oneup-sys-test-{}", std::process::id());
        let pipe = super::pipe::create(&name, true).unwrap();
        let sid = super::pipe::current_user_sid().unwrap();
        // Protected (no inherited entries) and exactly one entry: allow, for
        // this user. (Windows shows "generic all" as the pipe's "file all".)
        let sddl = super::pipe::dacl_sddl(&pipe).unwrap();
        assert_eq!(sddl, format!("D:P(A;;FA;;;{sid})"));
        // A second "first instance" of the same name is refused.
        assert!(super::pipe::create(&name, true).is_err());
    }

    #[test]
    fn the_current_user_has_a_sid() {
        let sid = super::pipe::current_user_sid().unwrap();
        assert!(sid.starts_with("S-1-"), "{}", sid.len());
    }

    #[test]
    fn disk_size_is_reported() {
        let total = super::disk::total_bytes(&std::env::temp_dir()).unwrap();
        assert!(total > 1 << 30);
    }

    #[test]
    fn dpapi_roundtrip() {
        let secret = b"not a real token";
        let sealed = super::dpapi::protect(secret).unwrap();
        assert_ne!(sealed.as_slice(), secret);
        let plain: zeroize::Zeroizing<Vec<u8>> = super::dpapi::unprotect(&sealed).unwrap();
        assert_eq!(plain.as_slice(), secret);
    }

    #[test]
    fn memory_is_reported() {
        let m = super::metrics::memory();
        assert!(m.working_set > 0 && m.peak_working_set >= m.working_set);
    }
}
