//! Windows resources for every executable: the icon (`packaging/
//! oneupdrive.ico`, drawn by `tray::ico`), version information, and the
//! application manifest (per-monitor DPI, long paths, the modern common
//! controls, run as the user).

/// The identity package's publisher in development builds (any name works
/// there: Developer Mode registers it unsigned).
const PUBLISHER_DEV: &str = "CN=OneUpDrive Development";

fn main() {
    println!("cargo:rerun-if-changed=build.rs");
    println!("cargo:rerun-if-changed=../../packaging/oneupdrive.ico");
    println!("cargo:rerun-if-changed=../../packaging/app.manifest");
    println!("cargo:rerun-if-env-changed=ONEUP_PUBLISHER");
    if std::env::var("CARGO_CFG_TARGET_OS").as_deref() != Ok("windows") {
        return;
    }
    let dir = std::path::PathBuf::from(std::env::var("CARGO_MANIFEST_DIR").unwrap_or_default());
    let packaging = dir.join("../../packaging");
    let out = std::path::PathBuf::from(std::env::var("OUT_DIR").unwrap_or_default());
    let version = std::env::var("CARGO_PKG_VERSION").unwrap_or_default();
    let mut parts: Vec<u16> = version.split(['.', '-', '+']).filter_map(|p| p.parse().ok()).take(3).collect();
    parts.resize(3, 0);
    let (a, b, c) = (parts[0], parts[1], parts[2]);
    let path = |p: std::path::PathBuf| p.display().to_string().replace('\\', "/");
    // The manifest, linked to the identity package (packaging/identity):
    // the programs get package identity once that's registered, and run as
    // before when it isn't. Its publisher must equal the signing
    // certificate's subject (set by the release build).
    //
    // Only the programs that need it: registering a folder with the shell
    // (the CLI, the settings window) and Office's link opener. The agent and
    // the tray run without it: Windows ends every process of a package at
    // once, without a word to any of them (the agent, tray and window were
    // seen vanishing together), and they need
    // nothing the identity gives.
    let publisher = std::env::var("ONEUP_PUBLISHER").unwrap_or_else(|_| PUBLISHER_DEV.to_string());
    let xml = |s: &str| s.replace('&', "&amp;").replace('"', "&quot;").replace('<', "&lt;");
    let base = std::fs::read_to_string(packaging.join("app.manifest")).unwrap_or_default();
    let msix = format!(
        "  <msix xmlns=\"urn:schemas-microsoft-com:msix.v1\" publisher=\"{}\" packageName=\"OneUpDrive.Sync\" applicationId=\"OneUpDrive\"/>
</assembly>",
        xml(&publisher)
    );
    let with_identity = out.join("app-identity.manifest");
    let plain = out.join("app-plain.manifest");
    if std::fs::write(&with_identity, base.replace("</assembly>", &msix)).is_err()
        || std::fs::write(&plain, &base).is_err()
    {
        return;
    }
    let rc = format!(
        r#"#include <winver.h>
1 ICON "{icon}"
1 24 "{{{{MANIFEST}}}}"
VS_VERSION_INFO VERSIONINFO
FILEVERSION {a},{b},{c},0
PRODUCTVERSION {a},{b},{c},0
FILEOS VOS_NT_WINDOWS32
FILETYPE VFT_APP
BEGIN
  BLOCK "StringFileInfo"
  BEGIN
    BLOCK "040904B0"
    BEGIN
      VALUE "CompanyName", "OneUpDrive contributors"
      VALUE "FileDescription", "OneUpDrive"
      VALUE "FileVersion", "{version}"
      VALUE "ProductName", "OneUpDrive"
      VALUE "ProductVersion", "{version}"
      VALUE "LegalCopyright", "Copyright (c) 2026 OneUpDrive contributors. MIT License"
    END
  END
  BLOCK "VarFileInfo"
  BEGIN
    VALUE "Translation", 0x409, 1200
  END
END
"#,
        icon = path(packaging.join("oneupdrive.ico")),
    );
    for (name, manifest, bins) in [
        ("oneup-identity.rc", &with_identity, &["oneup", "oneup-settings", "oneup-open"][..]),
        ("oneup-plain.rc", &plain, &["oneupd", "oneupd-start", "oneup-tray"][..]),
    ] {
        let file = out.join(name);
        if std::fs::write(&file, rc.replace("{{MANIFEST}}", &path(manifest.clone()))).is_err() {
            return;
        }
        // Without the Windows SDK's resource compiler (rare on a Windows
        // build machine), the programs still build, just without these.
        if let Err(e) = embed_resource::compile_for(&file, bins, embed_resource::NONE).manifest_optional() {
            println!("cargo:warning=Windows resources not embedded: {e}");
        }
    }
}
