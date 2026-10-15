//! The **experimental** Office integration's handler (`oneup_app::officeint`
//! writes the registry keys that point Office here): Microsoft's documented
//! `IStorageProviderHandler` → `IStorageProviderPropertyHandler`, answering
//! the properties Office asks for before opening a file from OneDrive. The
//! answers come from `oneup.exe office-props PATH` (next to this DLL), which
//! knows the file's OneDrive address and whether it's in sync. A file that
//! isn't in sync is reported as not co-authorable, so Office opens it as an
//! ordinary local file and nothing local is ever passed over.
//!
//! Each request is noted, without file names, in
//! `%LOCALAPPDATA%\OneUpDrive\office-integration.log`, so trying it shows
//! whether Office asked at all.

use std::io::Write as _;
use std::os::windows::process::CommandExt as _;

use windows::Win32::Foundation::{E_FAIL, E_NOTIMPL, PROPERTYKEY};
use windows::Win32::Storage::EnhancedStorage::{
    PKEY_StorageProviderFileChecksum, PKEY_StorageProviderFileFlags, PKEY_StorageProviderFileRemoteUri,
};
use windows::Win32::System::Com::StructuredStorage::{InitPropVariantFromBuffer, PROPVARIANT};
use windows::Win32::UI::Shell::PropertiesSystem::{IPropertyStore, PSCreateMemoryPropertyStore};
use windows::Win32::UI::Shell::{
    IStorageProviderHandler, IStorageProviderHandler_Impl, IStorageProviderPropertyHandler,
    IStorageProviderPropertyHandler_Impl,
};
use windows::core::{Interface, PCWSTR, Ref, Result, implement};

/// What `oneup office-props` said.
struct Props {
    uri: String,
    in_sync: bool,
    sha256: Vec<u8>,
}

fn note(what: &str) {
    let Some(base) = std::env::var_os("LOCALAPPDATA") else { return };
    let dir = std::path::PathBuf::from(base).join("OneUpDrive");
    let _ = std::fs::create_dir_all(&dir);
    if let Ok(mut f) = std::fs::OpenOptions::new().create(true).append(true).open(dir.join("office-integration.log")) {
        let secs = std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).map_or(0, |d| d.as_secs());
        let _ = writeln!(f, "{secs} {what}");
    }
}

fn parse(text: &str) -> Option<Props> {
    let get = |k: &str| text.lines().find_map(|l| l.strip_prefix(k)?.strip_prefix('=')).map(str::trim);
    let hex = get("sha256")?;
    let sha256 = (0..hex.len())
        .step_by(2)
        .map(|i| hex.get(i..i + 2).and_then(|b| u8::from_str_radix(b, 16).ok()))
        .collect::<Option<Vec<u8>>>()?;
    let uri = get("uri")?.to_string();
    if !uri.starts_with("https://") || sha256.len() != 32 {
        return None;
    }
    Some(Props { uri, in_sync: get("insync") == Some("1"), sha256 })
}

fn ask(path: &str) -> Option<Props> {
    const CREATE_NO_WINDOW: u32 = 0x0800_0000;
    let exe = crate::folder()?.join("oneup.exe");
    let out = std::process::Command::new(exe)
        .args(["office-props", path])
        .creation_flags(CREATE_NO_WINDOW)
        .stdin(std::process::Stdio::null())
        .output()
        .ok()?;
    if !out.status.success() {
        return None;
    }
    parse(&String::from_utf8_lossy(&out.stdout))
}

#[implement(IStorageProviderPropertyHandler)]
struct Properties {
    path: String,
}

impl IStorageProviderPropertyHandler_Impl for Properties_Impl {
    fn RetrieveProperties(&self, keys: *const PROPERTYKEY, count: u32) -> Result<IPropertyStore> {
        let Some(p) = ask(&self.path) else {
            note("asked; the file isn't one OneUpDrive has in OneDrive (or the answer failed)");
            return Err(E_FAIL.into());
        };
        note(&format!("asked for {count} properties; in sync: {}", p.in_sync));
        let mut store: Option<IPropertyStore> = None;
        // SAFETY: COM's contract for the out-pointer; `keys` holds `count`
        // keys for the call.
        let (store, wanted) = unsafe {
            PSCreateMemoryPropertyStore(&IPropertyStore::IID, &mut store as *mut _ as *mut _)?;
            let wanted = if keys.is_null() { &[][..] } else { std::slice::from_raw_parts(keys, count as usize) };
            (store.ok_or(E_FAIL)?, wanted)
        };
        let wants = |k: &PROPERTYKEY| wanted.is_empty() || wanted.contains(k);
        // SAFETY: plain COM calls on our own store with values we own.
        unsafe {
            if wants(&PKEY_StorageProviderFileRemoteUri) {
                store.SetValue(&PKEY_StorageProviderFileRemoteUri, &PROPVARIANT::from(p.uri.as_str()))?;
            }
            if wants(&PKEY_StorageProviderFileChecksum) {
                let v = InitPropVariantFromBuffer(p.sha256.as_ptr().cast(), p.sha256.len() as u32)?;
                store.SetValue(&PKEY_StorageProviderFileChecksum, &v)?;
            }
            if wants(&PKEY_StorageProviderFileFlags) {
                // 0x1: co-authoring supported (only for a file in sync).
                store.SetValue(&PKEY_StorageProviderFileFlags, &PROPVARIANT::from(u32::from(p.in_sync)))?;
            }
        }
        Ok(store)
    }

    fn SaveProperties(&self, _: Ref<IPropertyStore>) -> Result<()> {
        Err(E_NOTIMPL.into())
    }
}

#[implement(IStorageProviderHandler)]
pub struct Handler;

impl IStorageProviderHandler_Impl for Handler_Impl {
    fn GetPropertyHandlerFromPath(&self, path: &PCWSTR) -> Result<IStorageProviderPropertyHandler> {
        // SAFETY: COM passes a valid NUL-terminated string.
        let path = unsafe { path.to_string() }.map_err(|_| E_FAIL)?;
        note("Office looked up a file");
        Ok(Properties { path }.into())
    }
    fn GetPropertyHandlerFromUri(&self, _: &PCWSTR) -> Result<IStorageProviderPropertyHandler> {
        Err(E_NOTIMPL.into())
    }
    fn GetPropertyHandlerFromFileId(&self, _: &PCWSTR) -> Result<IStorageProviderPropertyHandler> {
        Err(E_NOTIMPL.into())
    }
}

#[cfg(test)]
mod tests {
    #[test]
    fn answers_are_read_strictly() {
        let hex = "ab".repeat(32);
        let p = super::parse(&format!("uri=https://d.docs.live.net/x/a.docx\ninsync=1\nsha256={hex}\n")).unwrap();
        assert!(p.in_sync && p.sha256.len() == 32);
        assert!(super::parse(&format!("uri=file://x\ninsync=1\nsha256={hex}")).is_none(), "https only");
        assert!(super::parse("uri=https://x\ninsync=1\nsha256=zz").is_none());
    }
}
