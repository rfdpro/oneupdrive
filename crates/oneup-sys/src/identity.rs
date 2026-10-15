//! Package identity: whether this program runs with OneUpDrive's identity
//! package registered (`packaging/identity`), which Explorer integration
//! needs.

use windows::Win32::Foundation::{APPMODEL_ERROR_NO_PACKAGE, ERROR_INSUFFICIENT_BUFFER, WIN32_ERROR};
use windows::Win32::Storage::Packaging::Appx::GetCurrentPackageFullName;
use windows::core::PWSTR;

/// This process's package full name (`OneUpDrive.Sync_1.2.3.0_neutral__…`),
/// or `None` without package identity.
pub fn package_name() -> Option<String> {
    let mut len = 0u32;
    // SAFETY: a size query (no buffer).
    let r = unsafe { GetCurrentPackageFullName(&mut len, None) };
    if r == APPMODEL_ERROR_NO_PACKAGE || r != ERROR_INSUFFICIENT_BUFFER {
        return None;
    }
    let mut buf = vec![0u16; len as usize];
    // SAFETY: `buf` holds `len` characters.
    let r: WIN32_ERROR = unsafe { GetCurrentPackageFullName(&mut len, Some(PWSTR(buf.as_mut_ptr()))) };
    if r.0 != 0 {
        return None;
    }
    let end = buf.iter().position(|&c| c == 0).unwrap_or(buf.len());
    Some(String::from_utf16_lossy(&buf[..end]))
}

/// The identity package's name (`packaging/identity/AppxManifest.xml`).
pub const PACKAGE: &str = "OneUpDrive.Sync";

fn deploy(
    what: &'static str,
    r: windows::core::Result<windows::Management::Deployment::DeploymentResult>,
) -> crate::Result<()> {
    let r = r.map_err(|source| crate::Error::Win32 { op: what, source })?;
    let code = r.ExtendedErrorCode().map_err(|source| crate::Error::Win32 { op: what, source })?;
    if code.is_err() {
        let text = r.ErrorText().map(|t| t.to_string()).unwrap_or_default();
        return Err(crate::Error::Win32 { op: what, source: windows::core::Error::new(code, text) });
    }
    Ok(())
}

fn uri(p: &std::path::Path) -> windows::core::Result<windows::Foundation::Uri> {
    let s = p.to_string_lossy();
    let s = s.strip_prefix(r"\\?\").unwrap_or(&s);
    windows::Foundation::Uri::CreateUri(&windows::core::HSTRING::from(format!("file:///{}", s.replace('\\', "/"))))
}

/// Adds the signed identity package `msix` for this user, its programs
/// being in `programs` (the install folder). Replaces an older version.
pub fn add(msix: &std::path::Path, programs: &std::path::Path) -> crate::Result<()> {
    use windows::Management::Deployment::{AddPackageOptions, PackageManager};
    let go = || -> windows::core::Result<windows::Management::Deployment::DeploymentResult> {
        let options = AddPackageOptions::new()?;
        options.SetExternalLocationUri(&uri(programs)?)?;
        options.SetForceUpdateFromAnyVersion(true)?;
        // The caller is one of our programs, so it runs with the package
        // being replaced: Windows finishes once it (and the others) exit.
        options.SetDeferRegistrationWhenPackagesAreInUse(true)?;
        PackageManager::new()?.AddPackageByUriAsync(&uri(msix)?, &options)?.join()
    };
    deploy("adding the identity package", go())
}

/// Registers the identity package from an unpacked, unsigned manifest
/// (Windows' Developer Mode only): for trying Explorer integration with a
/// development build in `programs`. Must run in a process that doesn't
/// have the identity (otherwise Windows wants it closed first).
pub fn register_unsigned(manifest: &std::path::Path, programs: &std::path::Path) -> crate::Result<()> {
    use windows::Management::Deployment::{PackageManager, RegisterPackageOptions};
    let go = || -> windows::core::Result<windows::Management::Deployment::DeploymentResult> {
        let options = RegisterPackageOptions::new()?;
        options.SetExternalLocationUri(&uri(programs)?)?;
        options.SetDeveloperMode(true)?;
        options.SetForceUpdateFromAnyVersion(true)?;
        // Not deferred: Windows can't finish a deferred registration of an
        // unpacked manifest (measured: it then blocks every launch of our
        // programs). Call this from a process without the identity.
        PackageManager::new()?.RegisterPackageByUriAsync(&uri(manifest)?, &options)?.join()
    };
    deploy("registering the identity package", go())
}

/// The publisher of the identity package installed for this user, if
/// there is one.
pub fn installed_publisher() -> crate::Result<Option<String>> {
    use windows::Management::Deployment::PackageManager;
    let err = |source| crate::Error::Win32 { op: "looking for the identity package", source };
    let pm = PackageManager::new().map_err(err)?;
    for p in pm.FindPackagesByUserSecurityId(&windows::core::HSTRING::new()).map_err(err)? {
        let id = p.Id().map_err(err)?;
        if id.Name().map_err(err)? == PACKAGE {
            return Ok(Some(id.Publisher().map_err(err)?.to_string()));
        }
    }
    Ok(None)
}

/// Where the identity package installed for this user reads its manifest
/// from, if it's installed and that can be told.
pub fn installed_location() -> crate::Result<Option<std::path::PathBuf>> {
    use windows::Management::Deployment::PackageManager;
    let err = |source| crate::Error::Win32 { op: "looking for the identity package", source };
    let pm = PackageManager::new().map_err(err)?;
    for p in pm.FindPackagesByUserSecurityId(&windows::core::HSTRING::new()).map_err(err)? {
        if p.Id().and_then(|id| id.Name()).map_err(err)? == PACKAGE {
            // A package whose folder is gone has no location to give.
            return Ok(p.InstalledPath().ok().map(|s| std::path::PathBuf::from(s.to_string())));
        }
    }
    Ok(None)
}

/// Removes the identity package for this user; `false` if it wasn't there.
pub fn remove() -> crate::Result<bool> {
    use windows::Management::Deployment::PackageManager;
    let err = |source| crate::Error::Win32 { op: "removing the identity package", source };
    let pm = PackageManager::new().map_err(err)?;
    let mut found = Vec::new();
    for p in pm.FindPackagesByUserSecurityId(&windows::core::HSTRING::new()).map_err(err)? {
        let id = p.Id().map_err(err)?;
        if id.Name().map_err(err)? == PACKAGE {
            found.push(id.FullName().map_err(err)?);
        }
    }
    for full in &found {
        deploy("removing the identity package", pm.RemovePackageAsync(full).and_then(|op| op.join()))?;
    }
    Ok(!found.is_empty())
}
