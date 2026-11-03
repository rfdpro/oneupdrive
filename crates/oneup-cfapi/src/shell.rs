//! Registering OneUpDrive's sync root with the Windows shell
//! (`StorageProviderSyncRootManager`), which gives it a place in File
//! Explorer's navigation pane, with its name and icon. Needs package
//! identity (`oneup_sys::identity`); without it, [`crate::provider::register`]
//! (Cloud Files alone) is used, as before. The policies are the same as
//! there: whole-file hydration, folders fully populated by OneUpDrive,
//! in-sync tracked for everything.

use std::path::Path;

use windows::Security::Cryptography::{BinaryStringEncoding, CryptographicBuffer};
use windows::Storage::Provider::{
    StorageProviderHardlinkPolicy, StorageProviderHydrationPolicy, StorageProviderHydrationPolicyModifier,
    StorageProviderInSyncPolicy, StorageProviderPopulationPolicy, StorageProviderProtectionMode,
    StorageProviderSyncRootInfo, StorageProviderSyncRootManager,
};
use windows::Storage::StorageFolder;
use windows::core::HSTRING;

use crate::provider::{Error, Result};

fn win(what: &'static str) -> impl FnOnce(windows::core::Error) -> Error {
    move |source| Error::Win { what, source }
}

/// The shell's ID for the pair with `token`: `OneUpDrive!<user>!<token>`.
pub fn root_id(user_sid: &str, token: &str) -> String {
    format!("{}!{user_sid}!{}", crate::PROVIDER_NAME, &token[..token.len().min(16)])
}

/// Registers `root` with the shell. `icon`: a program with the icon
/// (`C:\…\oneupd.exe,0`). The pair's token becomes the sync root's
/// identity, as with [`crate::provider::register`].
pub fn register(root: &Path, id: &str, token: &str, display: &str, icon: &str, version: &str) -> Result<()> {
    let folder = StorageFolder::GetFolderFromPathAsync(&HSTRING::from(root.as_os_str()))
        .and_then(|op| op.join())
        .map_err(win("StorageFolder::GetFolderFromPathAsync"))?;
    let info = StorageProviderSyncRootInfo::new().map_err(win("StorageProviderSyncRootInfo"))?;
    let set = || -> windows::core::Result<()> {
        info.SetId(&HSTRING::from(id))?;
        info.SetPath(&folder)?;
        info.SetDisplayNameResource(&HSTRING::from(display))?;
        info.SetIconResource(&HSTRING::from(icon))?;
        info.SetHydrationPolicy(StorageProviderHydrationPolicy::Full)?;
        info.SetHydrationPolicyModifier(StorageProviderHydrationPolicyModifier::None)?;
        info.SetPopulationPolicy(StorageProviderPopulationPolicy::AlwaysFull)?;
        info.SetInSyncPolicy(
            StorageProviderInSyncPolicy::FileCreationTime
                | StorageProviderInSyncPolicy::DirectoryCreationTime
                | StorageProviderInSyncPolicy::FileReadOnlyAttribute
                | StorageProviderInSyncPolicy::DirectoryReadOnlyAttribute
                | StorageProviderInSyncPolicy::FileHiddenAttribute
                | StorageProviderInSyncPolicy::DirectoryHiddenAttribute
                | StorageProviderInSyncPolicy::FileSystemAttribute
                | StorageProviderInSyncPolicy::DirectorySystemAttribute
                | StorageProviderInSyncPolicy::FileLastWriteTime
                | StorageProviderInSyncPolicy::DirectoryLastWriteTime,
        )?;
        info.SetHardlinkPolicy(StorageProviderHardlinkPolicy::None)?;
        info.SetProtectionMode(StorageProviderProtectionMode::Personal)?;
        info.SetVersion(&HSTRING::from(version))?;
        info.SetAllowPinning(true)?;
        info.SetShowSiblingsAsGroup(false)?;
        let ctx = CryptographicBuffer::ConvertStringToBinary(&HSTRING::from(token), BinaryStringEncoding::Utf8)?;
        info.SetContext(&ctx)?;
        // The custom state the shell extension can show (a lock on items in
        // a vault's folder; `oneup_shell::status::STATE_VAULT`).
        let vault = windows::Storage::Provider::StorageProviderItemPropertyDefinition::new()?;
        vault.SetId(1)?;
        vault.SetDisplayNameResource(&HSTRING::from("Encrypted vault"))?;
        info.StorageProviderItemPropertyDefinitions()?.Append(&vault)?;
        StorageProviderSyncRootManager::Register(&info)
    };
    set().map_err(win("StorageProviderSyncRootManager::Register"))
}

/// What the shell has stored for a sync root.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Registered {
    pub display: String,
    pub icon: String,
    pub version: String,
}

/// The shell registration with `id`, if there is one.
pub fn registered(id: &str) -> Option<Registered> {
    let info = StorageProviderSyncRootManager::GetSyncRootInformationForId(&HSTRING::from(id)).ok()?;
    Some(Registered {
        display: info.DisplayNameResource().ok()?.to_string(),
        icon: info.IconResource().ok()?.to_string(),
        version: info.Version().ok()?.to_string(),
    })
}

/// Removes the shell registration with `id` (and with it the sync root).
pub fn unregister(id: &str) -> Result<()> {
    StorageProviderSyncRootManager::Unregister(&HSTRING::from(id))
        .map_err(win("StorageProviderSyncRootManager::Unregister"))
}

/// Whether `path` is (inside) a folder the shell has registered as one of
/// OneUpDrive's sync roots, by its record in
/// `HKLM\...\Explorer\SyncRootManager\OneUpDrive!...\UserSyncRoots`. Reads
/// the registry only, so it works without package identity.
pub fn is_ours(path: &Path) -> bool {
    let target = norm(&path.to_string_lossy());
    our_roots().iter().any(|(_, r)| {
        let r = norm(r);
        target == r || target.starts_with(&format!("{r}\\"))
    })
}

/// Whether the shell has `root` registered as the sync root of the pair
/// with `token` (its ID ends with the token's prefix, [`root_id`]). What
/// still tells our folder when its Cloud Files registration lost its
/// identity (see `crate::other_sync_root`).
pub fn is_pairs_root(root: &Path, token: &str) -> bool {
    let suffix = format!("!{}", &token[..token.len().min(16)]);
    let target = norm(&root.to_string_lossy());
    our_roots().iter().any(|(id, r)| id.ends_with(&suffix) && norm(r) == target)
}

/// `p` without `\\?\` or a trailing `\`, names folded as Windows compares
/// them (`oneup_sync::fold`).
fn norm(p: &str) -> String {
    oneup_sync::fold(p.strip_prefix(r"\\?\").unwrap_or(p).trim_end_matches('\\'))
}

/// The shell IDs registered under our provider name (`OneUpDrive!...`)
/// and their folders, for every user listed.
/// A folder registered with the shell for this user, by any program.
pub struct UserRoot {
    /// The shell's ID.
    pub id: String,
    /// `None` if the shell's record doesn't say.
    pub root: Option<std::path::PathBuf>,
    /// The package family that registered it; `None` if that can't be told.
    pub owner: Option<String>,
}

/// The package family in an application ID (`Family!App`).
fn family_of(aumid: &str) -> Option<&str> {
    aumid.split_once('!').map(|(family, _)| family).filter(|f| !f.is_empty())
}

/// Whether a record's ID (`Provider!<user>!<account>`) isn't another user's.
/// An ID that names no user counts as this user's: only what is certainly
/// someone else's is left out.
fn is_users(id: &str, user_sid: &str) -> bool {
    id.split('!').find(|part| part.starts_with("S-1-")).is_none_or(|sid| sid.eq_ignore_ascii_case(user_sid))
}

/// Every folder registered with the shell for this user, whichever program
/// registered it, complete or an error. Windows removes a package's sync
/// roots with the package and deletes their online-only files (measured), so
/// deciding that a package owns nothing takes the whole list: a record that
/// can't be read is an error here, never a shorter list.
pub fn roots_of_user(user_sid: &str) -> Result<Vec<UserRoot>> {
    use windows::Win32::Foundation::{ERROR_FILE_NOT_FOUND, ERROR_NO_MORE_ITEMS, ERROR_SUCCESS};
    use windows::Win32::System::Registry::{
        HKEY, HKEY_LOCAL_MACHINE, KEY_READ, RegCloseKey, RegEnumKeyExW, RegOpenKeyExW,
    };
    use windows::core::{PCWSTR, PWSTR};

    const BASE: &str = r"SOFTWARE\Microsoft\Windows\CurrentVersion\Explorer\SyncRootManager";
    let wide = |s: &str| s.encode_utf16().chain([0]).collect::<Vec<u16>>();
    let failed = |what, code: windows::Win32::Foundation::WIN32_ERROR| Error::Win {
        what,
        source: windows::core::Error::from(code.to_hresult()),
    };
    let base = wide(BASE);
    let mut h = HKEY::default();
    // SAFETY: a NUL-terminated key path; `h` receives the opened key.
    let opened = unsafe { RegOpenKeyExW(HKEY_LOCAL_MACHINE, PCWSTR(base.as_ptr()), None, KEY_READ, &mut h) };
    if opened == ERROR_FILE_NOT_FOUND {
        return Ok(Vec::new()); // no sync roots on this PC at all
    }
    if opened != ERROR_SUCCESS {
        return Err(failed("opening the shell's sync root records", opened));
    }
    let mut ids = Vec::new();
    let mut ended = ERROR_SUCCESS;
    for i in 0.. {
        let mut name = [0u16; 512];
        let mut len = name.len() as u32;
        // SAFETY: `name` is writable for `len` characters; `h` is open.
        let r = unsafe { RegEnumKeyExW(h, i, Some(PWSTR(name.as_mut_ptr())), &mut len, None, None, None, None) };
        if r != ERROR_SUCCESS {
            ended = r;
            break;
        }
        ids.push(String::from_utf16_lossy(&name[..len as usize]));
    }
    // SAFETY: closes the key opened above.
    unsafe {
        let _ = RegCloseKey(h);
    }
    if ended != ERROR_NO_MORE_ITEMS {
        return Err(failed("listing the shell's sync root records", ended));
    }
    let mut out = Vec::new();
    for id in ids.into_iter().filter(|id| is_users(id, user_sid)) {
        let record = format!(r"{BASE}\{id}");
        let aumid = read_string(&record, "AUMID").map_err(|e| failed("reading a sync root record", e))?;
        let root = read_string(&format!(r"{record}\UserSyncRoots"), user_sid)
            .map_err(|e| failed("reading a sync root record", e))?;
        let owner = aumid.as_deref().and_then(family_of).map(str::to_owned);
        out.push(UserRoot { id, root: root.map(Into::into), owner });
    }
    Ok(out)
}

/// A string value under `HKEY_LOCAL_MACHINE`; `None` if it isn't there.
fn read_string(key: &str, value: &str) -> std::result::Result<Option<String>, windows::Win32::Foundation::WIN32_ERROR> {
    use windows::Win32::Foundation::{ERROR_FILE_NOT_FOUND, ERROR_MORE_DATA, ERROR_SUCCESS};
    use windows::Win32::System::Registry::{HKEY_LOCAL_MACHINE, RRF_RT_REG_SZ, RegGetValueW};
    use windows::core::PCWSTR;

    let wide = |s: &str| s.encode_utf16().chain([0]).collect::<Vec<u16>>();
    let (key, value) = (wide(key), wide(value));
    let mut buf = vec![0u16; 1024];
    loop {
        let mut size = (buf.len() * 2) as u32;
        // SAFETY: both strings are NUL-terminated; `buf` is writable for `size` bytes.
        let r = unsafe {
            RegGetValueW(
                HKEY_LOCAL_MACHINE,
                PCWSTR(key.as_ptr()),
                PCWSTR(value.as_ptr()),
                RRF_RT_REG_SZ,
                None,
                Some(buf.as_mut_ptr().cast()),
                Some(&mut size),
            )
        };
        if r == ERROR_MORE_DATA {
            buf.resize((size as usize).div_ceil(2) + 1, 0);
            continue;
        }
        if r == ERROR_FILE_NOT_FOUND {
            return Ok(None);
        }
        if r != ERROR_SUCCESS {
            return Err(r);
        }
        return Ok(Some(String::from_utf16_lossy(&buf[..(size as usize / 2).saturating_sub(1)])));
    }
}

fn our_roots() -> Vec<(String, String)> {
    use windows::Win32::Foundation::ERROR_SUCCESS;
    use windows::Win32::System::Registry::{
        HKEY, HKEY_LOCAL_MACHINE, KEY_READ, RRF_RT_REG_SZ, RegCloseKey, RegEnumKeyExW, RegEnumValueW, RegGetValueW,
        RegOpenKeyExW,
    };
    use windows::core::{PCWSTR, PWSTR};

    const BASE: &str = r"SOFTWARE\Microsoft\Windows\CurrentVersion\Explorer\SyncRootManager";
    let wide = |s: &str| s.encode_utf16().chain([0]).collect::<Vec<u16>>();
    let mut out = Vec::new();
    let base = wide(BASE);
    let mut h = HKEY::default();
    // SAFETY: a NUL-terminated key path; `h` receives the opened key.
    if unsafe { RegOpenKeyExW(HKEY_LOCAL_MACHINE, PCWSTR(base.as_ptr()), None, KEY_READ, &mut h) } != ERROR_SUCCESS {
        return out;
    }
    let prefix = format!("{}!", crate::PROVIDER_NAME);
    for i in 0.. {
        let mut name = [0u16; 512];
        let mut len = name.len() as u32;
        // SAFETY: `name` is writable for `len` characters; `h` is open.
        let r = unsafe { RegEnumKeyExW(h, i, Some(PWSTR(name.as_mut_ptr())), &mut len, None, None, None, None) };
        if r != ERROR_SUCCESS {
            break;
        }
        let id = String::from_utf16_lossy(&name[..len as usize]);
        if !id.starts_with(&prefix) {
            continue;
        }
        let sub = wide(&format!(r"{BASE}\{id}\UserSyncRoots"));
        let mut u = HKEY::default();
        // SAFETY: as above.
        if unsafe { RegOpenKeyExW(HKEY_LOCAL_MACHINE, PCWSTR(sub.as_ptr()), None, KEY_READ, &mut u) } != ERROR_SUCCESS {
            continue;
        }
        for j in 0.. {
            let mut vname = [0u16; 256];
            let mut vlen = vname.len() as u32;
            // SAFETY: `vname` is writable for `vlen` characters; `u` is open.
            let r = unsafe { RegEnumValueW(u, j, Some(PWSTR(vname.as_mut_ptr())), &mut vlen, None, None, None, None) };
            if r != ERROR_SUCCESS {
                break;
            }
            let mut buf = [0u16; 1024];
            let mut size = (buf.len() * 2) as u32;
            // SAFETY: `vname` is NUL-terminated (written into a zeroed buffer,
            // shorter than it); `buf` is writable for `size` bytes.
            let r = unsafe {
                RegGetValueW(
                    u,
                    None,
                    PCWSTR(vname.as_ptr()),
                    RRF_RT_REG_SZ,
                    None,
                    Some(buf.as_mut_ptr().cast()),
                    Some(&mut size),
                )
            };
            if r == ERROR_SUCCESS {
                out.push((id.clone(), String::from_utf16_lossy(&buf[..(size as usize / 2).saturating_sub(1)])));
            }
        }
        // SAFETY: closes the key opened above.
        unsafe {
            let _ = RegCloseKey(u);
        }
    }
    // SAFETY: closes the key opened above.
    unsafe {
        let _ = RegCloseKey(h);
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn only_what_is_certainly_another_users_is_left_out() {
        assert!(is_users("OneUpDrive!S-1-5-21-1!abc", "S-1-5-21-1"));
        assert!(is_users("OneUpDrive!s-1-5-21-1!abc", "S-1-5-21-1"));
        assert!(!is_users("OneUpDrive!S-1-5-21-2!abc", "S-1-5-21-1"));
        assert!(is_users("SomeProvider", "S-1-5-21-1"), "names no user");
    }

    /// Whatever this PC has registered, the list is complete or an error.
    #[test]
    fn the_inventory_reads_without_error() {
        assert!(roots_of_user("S-1-5-21-0").is_ok());
    }

    #[test]
    fn the_owner_is_the_family_in_the_application_id() {
        assert_eq!(family_of("OneUpDrive.Sync_g97pqcxf32cxg!OneUpDrive"), Some("OneUpDrive.Sync_g97pqcxf32cxg"));
        assert_eq!(family_of("!OneUpDrive"), None);
        assert_eq!(family_of("no separator"), None);
    }

    #[test]
    fn paths_compare_as_windows_compares_them() {
        assert_eq!(norm(r"\\?\C:\Users\Me\ΟΔΟΣ\"), norm(r"c:\users\me\οδος"));
        assert_ne!(norm("C:\\\u{212A}it"), norm(r"C:\kit"), "Kelvin sign");
    }
}
