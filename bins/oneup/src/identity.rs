//! OneUpDrive's identity package (`packaging/identity`), which gives the
//! programs package identity for Explorer integration
//! (`docs/architecture/explorer.md`).
//!
//! * Installed copies: the release ships it signed (`OneUpDrive.Sync.msix`,
//!   next to the programs); installing and updating add it, uninstalling
//!   removes it (`crate::install`).
//! * Development: `oneup dev-identity --register` registers it unsigned
//!   from the manifest (Windows' Developer Mode), with the development
//!   build's folder as its programs.

use std::path::{Path, PathBuf};

use anyhow::{Context, Result};

const MANIFEST: &str = include_str!("../../../packaging/identity/AppxManifest.xml");
const ASSETS: &[(&str, &[u8])] = &[
    ("StoreLogo.png", include_bytes!("../../../packaging/identity/Assets/StoreLogo.png")),
    ("Square44x44Logo.png", include_bytes!("../../../packaging/identity/Assets/Square44x44Logo.png")),
    ("Square150x150Logo.png", include_bytes!("../../../packaging/identity/Assets/Square150x150Logo.png")),
];
/// The signed package's file name, in the install folder.
pub const MSIX: &str = "OneUpDrive.Sync.msix";
/// What development builds use as publisher (the programs' manifests say
/// the same, `bins/oneup/build.rs`).
pub const PUBLISHER_DEV: &str = "CN=OneUpDrive Development";

/// `1.2.3` (or `1.2.3-beta.4`) as a package version, `1.2.3.0`.
pub fn package_version(v: &str) -> String {
    let mut parts: Vec<u16> =
        v.split(['-', '+']).next().unwrap_or("").split('.').filter_map(|p| p.parse().ok()).collect();
    parts.resize(4, 0);
    parts.iter().map(u16::to_string).collect::<Vec<_>>().join(".")
}

/// The manifest for `publisher` and `version`.
pub fn manifest(publisher: &str, version: &str) -> String {
    let xml = |s: &str| s.replace('&', "&amp;").replace('"', "&quot;").replace('<', "&lt;");
    MANIFEST.replace("{{PUBLISHER}}", &xml(publisher)).replace("{{VERSION}}", &package_version(version))
}

/// The development manifest. Windows re-registers a package in
/// Developer Mode only with a higher version, so the last part grows with
/// time (tens of minutes since 2026; good until 2038).
fn manifest_dev() -> String {
    let base = package_version(env!("CARGO_PKG_VERSION"));
    let since = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map_or(0, |d| d.as_secs().saturating_sub(1_767_225_600) / 600);
    let revision = u16::try_from(since).unwrap_or(u16::MAX);
    let base = base.rsplit_once('.').map_or(base.as_str(), |(b, _)| b).to_string();
    let xml = |s: &str| s.replace('&', "&amp;").replace('"', "&quot;").replace('<', "&lt;");
    MANIFEST.replace("{{PUBLISHER}}", &xml(PUBLISHER_DEV)).replace("{{VERSION}}", &format!("{base}.{revision}"))
}

/// Where [`register_dev`] puts the unpacked package for the programs in
/// `programs`. Windows keeps reading it there, so it must outlive updates
/// and reinstalls. In an installed copy, every update replaces the
/// programs' folder (`current`; measured: an update left the registration
/// without its manifest), and installing over an existing copy deletes the
/// whole install folder, after which Windows dropped the folder's Files
/// On-Demand registration (measured over several releases; updating
/// the package alone left it in place). So it goes beside the install
/// folder: `%LOCALAPPDATA%\OneUpDrive.App.identity`.
pub fn stage_dir(programs: &Path) -> PathBuf {
    match programs.parent() {
        Some(install) if programs.ends_with("current") && install.join("Update.exe").exists() => {
            let mut name = install.file_name().unwrap_or_default().to_os_string();
            name.push(".identity");
            install.with_file_name(name)
        }
        _ => programs.join("identity"),
    }
}

/// The manifest without its package version (the `Version` attribute of
/// `<Identity>`), for comparing what a registration would change.
fn without_version(manifest: &str) -> String {
    let Some(at) = manifest.find("<Identity") else { return manifest.to_string() };
    let Some(v) = manifest[at..].find(" Version=\"").map(|i| at + i) else { return manifest.to_string() };
    let Some(end) = manifest[v + 10..].find('"').map(|i| v + 10 + i + 1) else { return manifest.to_string() };
    format!("{}{}", &manifest[..v], &manifest[end..])
}

/// Whether the development identity registered now is already this
/// one: read from the folder [`register_dev`] would use for `programs`,
/// with the same manifest but for its version. Then registering again
/// changes nothing but the version, and updating the package while the
/// agent is stopped (as every install and update does) made Windows drop
/// the folder's Files On-Demand registration (measured: still
/// with the package beside the install folder; the agent's start repairs
/// it, but better not to).
pub fn dev_is_current(programs: &Path) -> bool {
    let stage = stage_dir(programs);
    let plain = |p: &Path| p.to_string_lossy().trim_start_matches(r"\\?\").trim_end_matches('\\').to_lowercase();
    let registered = oneup_sys::identity::installed_location().ok().flatten();
    if registered.is_none_or(|r| plain(&r) != plain(&stage)) {
        return false;
    }
    std::fs::read_to_string(stage.join("AppxManifest.xml"))
        .is_ok_and(|now| without_version(&now) == without_version(&manifest_dev()))
}

/// What [`register_dev`] did.
pub enum DevRegistration {
    /// Registered now.
    Done(PathBuf),
    /// This program has the identity being replaced: a detached PowerShell
    /// registers it once every OneUpDrive program has exited.
    Pending(PathBuf),
}

/// Registers the package unsigned for the development build in this
/// program's folder (Developer Mode must be on).
pub fn register_dev() -> Result<DevRegistration> {
    let programs = std::env::current_exe()?.parent().context("no program folder")?.to_path_buf();
    let stage = stage_dir(&programs);
    std::fs::create_dir_all(stage.join("Assets"))?;
    std::fs::write(stage.join("AppxManifest.xml"), manifest_dev())?;
    for (name, bytes) in ASSETS {
        std::fs::write(stage.join("Assets").join(name), bytes)?;
    }
    let manifest = stage.join("AppxManifest.xml");
    if oneup_sys::identity::package_name().is_none() {
        // Explorer's COM surrogate serving `oneup_shell.dll` runs with the
        // package, and Windows won't replace it while it lives (measured: the
        // install hook's registration failed with "apps need to be closed").
        // Explorer starts a new one when it needs it.
        stop_shell_surrogate();
        oneup_sys::identity::register_unsigned(&manifest, &programs)
            .context("registering the identity package (is Developer Mode on?)")?;
        return Ok(DevRegistration::Done(programs));
    }
    // This program runs with the package being replaced, and Windows refuses
    // to replace it while any of our programs with the identity is alive
    // (measured: "apps need to be closed", after which our programs can't
    // start until the registration completes). The agent and the tray run
    // without it (bins/oneup/build.rs), so syncing goes on meanwhile. A
    // detached PowerShell waits for the others to exit,
    // stops the COM surrogate serving Explorer `oneup_shell.dll` (it runs
    // with the package too, and Explorer starts a new one when it needs it),
    // then registers.
    let quote = |p: &std::path::Path| format!("'{}'", p.display().to_string().replace('\'', "''"));
    let script = format!(
        "$names = 'oneup','oneup-settings','oneup-open'; \
         Wait-Process -Id {pid} -ErrorAction SilentlyContinue; \
         while (Get-Process -Name $names -ErrorAction SilentlyContinue) {{ Start-Sleep -Seconds 1 }}; \
         {STOP_SURROGATE}; \
         Add-AppxPackage -Register {} -ExternalLocation {} -ForceUpdateFromAnyVersion",
        quote(&manifest),
        quote(&programs),
        pid = std::process::id()
    );
    std::process::Command::new("powershell.exe")
        .args(["-NoProfile", "-NonInteractive", "-WindowStyle", "Hidden", "-Command", &script])
        .stdin(std::process::Stdio::null())
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::null())
        .spawn()
        .context("starting PowerShell")?;
    Ok(DevRegistration::Pending(programs))
}

/// The PowerShell that stops the COM surrogates serving `oneup_shell.dll`.
const STOP_SURROGATE: &str = "Get-Process -Name dllhost -ErrorAction SilentlyContinue | \
     Where-Object { $_.Modules.ModuleName -contains 'oneup_shell.dll' } | Stop-Process -Force";

/// Stops Explorer's COM surrogates serving `oneup_shell.dll`, and waits.
/// Best effort: only our extension's surrogates, which hold no data.
fn stop_shell_surrogate() {
    use std::os::windows::process::CommandExt as _;
    const CREATE_NO_WINDOW: u32 = 0x0800_0000;
    let stopped = std::process::Command::new("powershell.exe")
        .args(["-NoProfile", "-NonInteractive", "-Command", STOP_SURROGATE])
        .stdin(std::process::Stdio::null())
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::null())
        .creation_flags(CREATE_NO_WINDOW)
        .status();
    if let Err(e) = stopped {
        tracing::warn!(error = %e, "Explorer's helper for our menu wasn't stopped");
    }
}

/// Adds the signed package shipped next to this program, if there is one.
pub fn add_installed() -> Result<bool> {
    let programs = std::env::current_exe()?.parent().context("no program folder")?.to_path_buf();
    let msix = programs.join(MSIX);
    if !msix.exists() {
        return Ok(false);
    }
    oneup_sys::identity::add(&msix, &programs)?;
    Ok(true)
}

/// Removes the package; `false` if it wasn't there.
pub fn remove() -> Result<bool> {
    Ok(oneup_sys::identity::remove()?)
}

#[cfg(test)]
mod tests {
    #[test]
    fn versions_and_publishers_fill_the_manifest() {
        assert_eq!(super::package_version("1.2.3"), "1.2.3.0");
        assert_eq!(super::package_version("0.2.0-beta.1"), "0.2.0.0");
        let m = super::manifest("CN=A & B, O=\"X\"", "1.2.3");
        assert!(m.contains(r#"Publisher="CN=A &amp; B, O=&quot;X&quot;""#));
        assert!(m.contains(r#"Version="1.2.3.0""#));
        assert!(!m.contains("{{"));
    }

    /// Two releases' manifests differ only in the version: no update.
    #[test]
    fn manifests_compare_without_their_version() {
        let (a, b) = (super::manifest("CN=X", "0.1.21"), super::manifest("CN=X", "0.1.22"));
        assert_ne!(a, b);
        assert_eq!(super::without_version(&a), super::without_version(&b));
        assert!(!super::without_version(&a).contains("0.1.21"));
        assert_ne!(super::without_version(&a), super::without_version(&super::manifest("CN=Y", "0.1.21")));
    }

    #[test]
    #[allow(clippy::unwrap_used)]
    fn an_installed_copy_keeps_its_unpacked_package_out_of_the_updated_folder() {
        let install = std::env::temp_dir().join(format!("oneup-identity-stage-{}", std::process::id()));
        let current = install.join("current");
        std::fs::create_dir_all(&current).unwrap();
        // A development build's folder (no updater next to it).
        assert_eq!(super::stage_dir(&current), current.join("identity"));
        std::fs::write(install.join("Update.exe"), b"").unwrap();
        let beside = install.with_file_name(format!("{}.identity", install.file_name().unwrap().to_string_lossy()));
        assert_eq!(super::stage_dir(&current), beside, "outside the install folder, which a reinstall deletes");
        std::fs::remove_dir_all(&install).unwrap();
    }
}
