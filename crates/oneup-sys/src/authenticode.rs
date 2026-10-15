//! Who signed a program (Authenticode): for accepting an update only if
//! it's signed by the same publisher as the running program.

use std::path::Path;

use windows::Win32::Foundation::{HANDLE, HWND};
use windows::Win32::Security::Cryptography::{CERT_NAME_RDN_TYPE, CERT_X500_NAME_STR, CertGetNameStringW};
use windows::Win32::Security::WinTrust::{
    WINTRUST_ACTION_GENERIC_VERIFY_V2, WINTRUST_DATA, WINTRUST_DATA_0, WINTRUST_FILE_INFO,
    WTD_CACHE_ONLY_URL_RETRIEVAL, WTD_CHOICE_FILE, WTD_REVOKE_NONE, WTD_STATEACTION_CLOSE, WTD_STATEACTION_VERIFY,
    WTD_UI_NONE, WTHelperGetProvSignerFromChain, WTHelperProvDataFromStateData, WinVerifyTrust,
};
use windows::core::{HSTRING, PCWSTR};

/// The full subject (`CN=…, O=…, …`) of the certificate that validly
/// signed `path`, or `None` if it isn't validly signed (no signature, a
/// broken one, or an untrusted certificate).
pub fn signer(path: &Path) -> std::io::Result<Option<String>> {
    let file = HSTRING::from(path.as_os_str());
    let mut info = WINTRUST_FILE_INFO {
        cbStruct: size_of::<WINTRUST_FILE_INFO>() as u32,
        pcwszFilePath: PCWSTR(file.as_ptr()),
        hFile: HANDLE::default(),
        pgKnownSubject: std::ptr::null_mut(),
    };
    let mut data = WINTRUST_DATA {
        cbStruct: size_of::<WINTRUST_DATA>() as u32,
        dwUIChoice: WTD_UI_NONE,
        // Offline: revocation isn't checked over the network (the update is
        // compared with the running program's own signer anyway).
        fdwRevocationChecks: WTD_REVOKE_NONE,
        dwUnionChoice: WTD_CHOICE_FILE,
        Anonymous: WINTRUST_DATA_0 { pFile: &mut info },
        dwStateAction: WTD_STATEACTION_VERIFY,
        dwProvFlags: WTD_CACHE_ONLY_URL_RETRIEVAL,
        ..Default::default()
    };
    let mut action = WINTRUST_ACTION_GENERIC_VERIFY_V2;
    // SAFETY: `data` and what it points to (`info`, `file`) live through
    // both calls; the state the first call opens is closed by the second.
    let (status, subject) = unsafe {
        let status = WinVerifyTrust(HWND(std::ptr::dangling_mut()), &mut action, std::ptr::from_mut(&mut data).cast());
        let subject = if status == 0 { subject_of(data.hWVTStateData) } else { None };
        data.dwStateAction = WTD_STATEACTION_CLOSE;
        let _ = WinVerifyTrust(HWND(std::ptr::dangling_mut()), &mut action, std::ptr::from_mut(&mut data).cast());
        (status, subject)
    };
    Ok(if status == 0 { subject } else { None })
}

/// The signing certificate's subject, from a verification's state.
///
/// # Safety
/// `state` is the open state of a successful `WinVerifyTrust` call.
unsafe fn subject_of(state: HANDLE) -> Option<String> {
    // SAFETY: as the caller promises; each pointer is checked before use
    // and stays valid until the state is closed.
    unsafe {
        let prov = WTHelperProvDataFromStateData(state);
        if prov.is_null() {
            return None;
        }
        let sgnr = WTHelperGetProvSignerFromChain(prov, 0, false, 0);
        if sgnr.is_null() || (*sgnr).csCertChain == 0 || (*sgnr).pasCertChain.is_null() {
            return None;
        }
        let cert = (*(*sgnr).pasCertChain).pCert;
        if cert.is_null() {
            return None;
        }
        let kind = CERT_X500_NAME_STR;
        let len = CertGetNameStringW(cert, CERT_NAME_RDN_TYPE, 0, Some(std::ptr::from_ref(&kind).cast()), None);
        if len <= 1 {
            return None;
        }
        let mut buf = vec![0u16; len as usize];
        CertGetNameStringW(cert, CERT_NAME_RDN_TYPE, 0, Some(std::ptr::from_ref(&kind).cast()), Some(&mut buf));
        let end = buf.iter().position(|&c| c == 0).unwrap_or(buf.len());
        Some(String::from_utf16_lossy(&buf[..end]))
    }
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used)]

    #[test]
    fn signed_programs_name_their_signer_and_test_builds_are_unsigned() {
        let notepad = std::path::Path::new(r"C:\Windows\System32\notepad.exe");
        // Catalog-signed system files have no embedded signature; a
        // signed driver tool is a better stand-in when there is one.
        let _ = super::signer(notepad).unwrap();
        // A program with an embedded signature, where there is one.
        for p in [r"C:\Program Files\dotnet\dotnet.exe", r"C:\Program Files\Git\cmd\git.exe"] {
            let p = std::path::Path::new(p);
            if p.exists() {
                let s = super::signer(p).unwrap();
                assert!(s.as_deref().is_some_and(|s| s.contains("CN=")), "{p:?}: {s:?}");
                eprintln!("{} signed by {}", p.display(), s.unwrap_or_default());
            }
        }
        let me = std::env::current_exe().unwrap();
        assert_eq!(super::signer(&me).unwrap(), None, "test builds aren't signed");
    }
}
