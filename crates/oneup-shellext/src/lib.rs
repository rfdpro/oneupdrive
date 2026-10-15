//! OneUpDrive's commands in File Explorer's context menu, for files and
//! folders in OneUpDrive's synced folders (`docs/architecture/explorer.md`).
//!
//! A COM in-process server that Windows loads in a separate surrogate
//! process (`com:SurrogateServer` in the identity package), never in
//! Explorer itself. It offers one "OneUpDrive" entry with sub-commands, and
//! each only starts `oneup.exe shell-verb VERB PATH` (next to this DLL) with
//! the first selected item: no sync work happens here, and no user content
//! is read or changed (AGENTS.md: the agent and its tools stay separate).
//!
//! A thin FFI crate: `unsafe` only at the COM boundary, each block
//! explained.
#![cfg(windows)]

use std::cell::Cell;
use std::ffi::c_void;
use std::os::windows::process::CommandExt as _;
use std::path::PathBuf;

use windows::Win32::Foundation::{
    CLASS_E_CLASSNOTAVAILABLE, CLASS_E_NOAGGREGATION, E_NOTIMPL, E_POINTER, HMODULE, S_FALSE, S_OK,
};
use windows::Win32::System::Com::{IBindCtx, IClassFactory, IClassFactory_Impl};
use windows::Win32::System::LibraryLoader::{
    GET_MODULE_HANDLE_EX_FLAG_FROM_ADDRESS, GET_MODULE_HANDLE_EX_FLAG_UNCHANGED_REFCOUNT, GetModuleFileNameW,
    GetModuleHandleExW,
};
use windows::Win32::UI::Shell::{
    ECF_DEFAULT, ECF_HASSUBCOMMANDS, ECS_ENABLED, IEnumExplorerCommand, IEnumExplorerCommand_Impl, IExplorerCommand,
    IExplorerCommand_Impl, IShellItemArray, SHStrDupW, SIGDN_FILESYSPATH,
};
use windows_core::{BOOL, GUID, HRESULT, IUnknown, Interface, PCWSTR, PWSTR, Ref, Result, implement};

/// The menu's class (named in `packaging/identity/AppxManifest.xml`).
pub const CLSID_MENU: GUID = GUID::from_u128(0x7d3e_5c1a_0b5e_4f6b_9a41_6f2c_1d8e_9a00);
/// The custom-state handler's class (the vault state, [`status::States`]).
pub const CLSID_STATES: GUID = GUID::from_u128(0x7d3e_5c1a_0b5e_4f6b_9a41_6f2c_1d8e_9a10);
/// The status flyout's factory ([`status::StatusFactory`]).
pub const CLSID_STATUS: GUID = GUID::from_u128(0x7d3e_5c1a_0b5e_4f6b_9a41_6f2c_1d8e_9a20);

/// The experimental Office integration's handler ([`office::Handler`]).
pub const CLSID_OFFICE: GUID = GUID::from_u128(0x7d3e_5c1a_0b5e_4f6b_9a41_6f2c_1d8e_9a30);

pub mod office;
pub mod status;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Verb {
    Explain,
    Activity,
    First,
    Online,
    Versions,
    Share,
    Office,
}

const VERBS: [Verb; 7] =
    [Verb::Office, Verb::Share, Verb::Versions, Verb::Online, Verb::First, Verb::Explain, Verb::Activity];

impl Verb {
    fn title(self) -> &'static str {
        match self {
            Verb::Explain => "Why does this sync?",
            Verb::Activity => "Sync activity",
            Verb::First => "Sync this first",
            Verb::Online => "View online",
            Verb::Versions => "Version history",
            Verb::Share => "Copy share link…",
            Verb::Office => "Open with AutoSave",
        }
    }
    fn arg(self) -> &'static str {
        match self {
            Verb::Explain => "explain",
            Verb::Activity => "activity",
            Verb::First => "first",
            Verb::Online => "online",
            Verb::Versions => "versions",
            Verb::Share => "share",
            Verb::Office => "office",
        }
    }
    fn guid(self) -> GUID {
        GUID::from_u128(CLSID_MENU.to_u128() + 1 + self as u128)
    }
}

/// This DLL's folder (where `oneup.exe` is too).
pub(crate) fn folder() -> Option<PathBuf> {
    let mut module = HMODULE::default();
    let here = folder as *const ();
    // SAFETY: looks up the module containing this function's code; the
    // handle isn't reference-counted (UNCHANGED_REFCOUNT), so nothing to free.
    unsafe {
        GetModuleHandleExW(
            GET_MODULE_HANDLE_EX_FLAG_FROM_ADDRESS | GET_MODULE_HANDLE_EX_FLAG_UNCHANGED_REFCOUNT,
            PCWSTR(here.cast()),
            &mut module,
        )
        .ok()?;
    }
    let mut buf = vec![0u16; 32_768];
    // SAFETY: `buf` is writable for its length.
    let n = unsafe { GetModuleFileNameW(Some(module), &mut buf) } as usize;
    if n == 0 {
        return None;
    }
    PathBuf::from(String::from_utf16_lossy(&buf[..n])).parent().map(PathBuf::from)
}

/// A string for the shell (allocated with the COM allocator; the shell frees it).
fn dup(s: &str) -> Result<PWSTR> {
    let wide: Vec<u16> = s.encode_utf16().chain([0]).collect();
    // SAFETY: a NUL-terminated string that outlives the call, which copies it.
    unsafe { SHStrDupW(PCWSTR(wide.as_ptr())) }
}

fn icon() -> Result<PWSTR> {
    match folder() {
        Some(f) => dup(&format!("{},0", f.join("oneupd.exe").display())),
        None => Err(E_NOTIMPL.into()),
    }
}

/// The first selected item's path.
fn first_path(items: &IShellItemArray) -> Option<String> {
    // SAFETY: plain COM calls on a live array; the returned string is
    // copied, then freed with the COM allocator as documented.
    unsafe {
        let item = items.GetItemAt(0).ok()?;
        let name = item.GetDisplayName(SIGDN_FILESYSPATH).ok()?;
        let s = name.to_string().ok();
        windows::Win32::System::Com::CoTaskMemFree(Some(name.0.cast()));
        s
    }
}

/// Every command answers in the settings window or Explorer, never in a
/// console.
fn run(verb: Verb, path: &str) {
    const CREATE_NO_WINDOW: u32 = 0x0800_0000;
    let Some(dir) = folder() else { return };
    let mut cmd = std::process::Command::new(dir.join("oneup.exe"));
    cmd.arg("shell-verb").arg(verb.arg()).arg(path);
    let _ = spawn_outside_job(&mut cmd, CREATE_NO_WINDOW);
}

/// Starts `cmd` outside this surrogate's Windows job where allowed: Windows
/// ends the job, and everything in it, when the surrogate goes, and a
/// program started from here (the settings window, a sync started first)
/// must outlive it.
pub(crate) fn spawn_outside_job(cmd: &mut std::process::Command, flags: u32) -> std::io::Result<std::process::Child> {
    const CREATE_BREAKAWAY_FROM_JOB: u32 = 0x0100_0000;
    match cmd.creation_flags(flags | CREATE_BREAKAWAY_FROM_JOB).spawn() {
        Err(e) if e.raw_os_error() == Some(5) => cmd.creation_flags(flags).spawn(),
        r => r,
    }
}

/// The "OneUpDrive" entry, holding the commands.
#[implement(IExplorerCommand)]
struct Menu;

impl IExplorerCommand_Impl for Menu_Impl {
    fn GetTitle(&self, _: Ref<IShellItemArray>) -> Result<PWSTR> {
        dup("OneUpDrive")
    }
    fn GetIcon(&self, _: Ref<IShellItemArray>) -> Result<PWSTR> {
        icon()
    }
    fn GetToolTip(&self, _: Ref<IShellItemArray>) -> Result<PWSTR> {
        Err(E_NOTIMPL.into())
    }
    fn GetCanonicalName(&self) -> Result<GUID> {
        Ok(CLSID_MENU)
    }
    fn GetState(&self, _: Ref<IShellItemArray>, _: BOOL) -> Result<u32> {
        Ok(ECS_ENABLED.0 as u32)
    }
    fn Invoke(&self, _: Ref<IShellItemArray>, _: Ref<IBindCtx>) -> Result<()> {
        Ok(())
    }
    fn GetFlags(&self) -> Result<u32> {
        Ok(ECF_HASSUBCOMMANDS.0 as u32)
    }
    fn EnumSubCommands(&self) -> Result<IEnumExplorerCommand> {
        Ok(Commands { next: Cell::new(0) }.into())
    }
}

/// One command.
#[implement(IExplorerCommand)]
struct Command(Verb);

impl IExplorerCommand_Impl for Command_Impl {
    fn GetTitle(&self, _: Ref<IShellItemArray>) -> Result<PWSTR> {
        dup(self.0.title())
    }
    fn GetIcon(&self, _: Ref<IShellItemArray>) -> Result<PWSTR> {
        Err(E_NOTIMPL.into())
    }
    fn GetToolTip(&self, _: Ref<IShellItemArray>) -> Result<PWSTR> {
        Err(E_NOTIMPL.into())
    }
    fn GetCanonicalName(&self) -> Result<GUID> {
        Ok(self.0.guid())
    }
    fn GetState(&self, _: Ref<IShellItemArray>, _: BOOL) -> Result<u32> {
        Ok(ECS_ENABLED.0 as u32)
    }
    fn Invoke(&self, items: Ref<IShellItemArray>, _: Ref<IBindCtx>) -> Result<()> {
        if let Some(items) = items.as_ref()
            && let Some(path) = first_path(items)
        {
            run(self.0, &path);
        }
        Ok(())
    }
    fn GetFlags(&self) -> Result<u32> {
        Ok(ECF_DEFAULT.0 as u32)
    }
    fn EnumSubCommands(&self) -> Result<IEnumExplorerCommand> {
        Err(E_NOTIMPL.into())
    }
}

#[implement(IEnumExplorerCommand)]
struct Commands {
    next: Cell<usize>,
}

impl IEnumExplorerCommand_Impl for Commands_Impl {
    fn Next(&self, celt: u32, out: *mut Option<IExplorerCommand>, fetched: *mut u32) -> HRESULT {
        if out.is_null() {
            return E_POINTER;
        }
        let mut n = 0u32;
        while n < celt && self.next.get() < VERBS.len() {
            let c: IExplorerCommand = Command(VERBS[self.next.get()]).into();
            // SAFETY: the caller provides room for `celt` commands.
            unsafe { out.add(n as usize).write(Some(c)) };
            self.next.set(self.next.get() + 1);
            n += 1;
        }
        if !fetched.is_null() {
            // SAFETY: checked non-null; the caller's out-parameter.
            unsafe { fetched.write(n) };
        }
        if n == celt { S_OK } else { S_FALSE }
    }
    fn Skip(&self, celt: u32) -> Result<()> {
        self.next.set((self.next.get() + celt as usize).min(VERBS.len()));
        Ok(())
    }
    fn Reset(&self) -> Result<()> {
        self.next.set(0);
        Ok(())
    }
    fn Clone(&self) -> Result<IEnumExplorerCommand> {
        Ok(Commands { next: Cell::new(self.next.get()) }.into())
    }
}

#[implement(IClassFactory)]
struct Factory(GUID);

impl IClassFactory_Impl for Factory_Impl {
    fn CreateInstance(&self, outer: Ref<IUnknown>, riid: *const GUID, out: *mut *mut c_void) -> Result<()> {
        if outer.is_some() {
            return Err(CLASS_E_NOAGGREGATION.into());
        }
        let object: IUnknown = match self.0 {
            c if c == CLSID_STATUS => status::StatusFactory.into(),
            c if c == CLSID_STATES => status::States.into(),
            c if c == CLSID_OFFICE => office::Handler.into(),
            _ => Menu.into(),
        };
        // SAFETY: COM's contract: `riid` and `out` are valid for the call.
        unsafe { object.query(riid, out).ok() }
    }
    fn LockServer(&self, _: BOOL) -> Result<()> {
        Ok(())
    }
}

/// COM's entry point: the class factories for [`CLSID_MENU`],
/// [`CLSID_STATES`] and [`CLSID_STATUS`].
///
/// # Safety
/// Called by COM with valid pointers.
#[unsafe(no_mangle)]
pub unsafe extern "system" fn DllGetClassObject(
    clsid: *const GUID,
    riid: *const GUID,
    out: *mut *mut c_void,
) -> HRESULT {
    if clsid.is_null() || out.is_null() {
        return E_POINTER;
    }
    // SAFETY: checked non-null; COM passes a valid CLSID.
    let clsid = unsafe { *clsid };
    if ![CLSID_MENU, CLSID_STATES, CLSID_STATUS, CLSID_OFFICE].contains(&clsid) {
        return CLASS_E_CLASSNOTAVAILABLE;
    }
    let factory: IClassFactory = Factory(clsid).into();
    // SAFETY: COM's contract: `riid` and `out` are valid for the call.
    unsafe { factory.query(riid, out) }
}

/// Unloading is left to the surrogate process ending.
#[unsafe(no_mangle)]
pub extern "system" fn DllCanUnloadNow() -> HRESULT {
    S_FALSE
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used)]
    use super::*;

    #[test]
    fn the_menu_lists_its_commands_once_each() {
        let menu: IExplorerCommand = Menu.into();
        // SAFETY: plain COM calls on our own objects.
        unsafe {
            assert_eq!(menu.GetFlags().unwrap(), ECF_HASSUBCOMMANDS.0 as u32);
            let e = menu.EnumSubCommands().unwrap();
            let mut got = Vec::new();
            loop {
                let mut c = [None];
                let mut n = 0;
                let _ = e.Next(&mut c, Some(&mut n));
                if n == 0 {
                    break;
                }
                got.push(c[0].take().unwrap().GetCanonicalName().unwrap());
            }
            assert_eq!(got.len(), VERBS.len());
            assert!(got.iter().all(|g| *g != CLSID_MENU));
        }
    }

    #[test]
    fn only_our_classes_are_served() {
        let mut out = std::ptr::null_mut();
        let other = GUID::from_u128(1);
        // SAFETY: valid pointers to locals.
        unsafe {
            assert_eq!(DllGetClassObject(&other, &IClassFactory::IID, &mut out), CLASS_E_CLASSNOTAVAILABLE);
            assert_eq!(DllGetClassObject(&CLSID_MENU, &IClassFactory::IID, &mut out), S_OK);
            let f = IClassFactory::from_raw(out);
            let menu: IExplorerCommand = f.CreateInstance(None).unwrap();
            assert_eq!(menu.GetCanonicalName().unwrap(), CLSID_MENU);
        }
    }
}
