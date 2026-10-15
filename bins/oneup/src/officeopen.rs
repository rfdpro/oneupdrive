//! OneUpDrive as the default program for Office files (optional): Windows
//! then hands every `.docx`, `.xlsx`, `.pptx` (and macro-enabled kin) that
//! is opened to `oneup-open.exe`, which decides ([`choose`]):
//! * a file in a folder OneUpDrive syncs, unchanged since it was last
//!   synced, opens **from OneDrive** in its Office program (as **Open with
//!   AutoSave** does), so AutoSave and co-authoring work on double-click;
//! * anything else (files elsewhere on the PC, changed here and not yet
//!   synced, in a vault or a backup pair, or when OneDrive can't be
//!   reached) opens with **Office's own "open"**, exactly as without
//!   OneUpDrive.
//!
//! Office's own test for this, its documented sync-program registration,
//! ignores programs other than the OneDrive app and Microsoft's storage
//! partners; being the
//! default program is the route Windows leaves open.
//!
//! [`register`] makes OneUpDrive a program Windows offers for those types
//! (per user, no admin rights): a file type per extension, copied from
//! Office's (icons, and its other verbs such as Print and New) with only
//! "open" replaced, listed under the extension's "Open with" programs and
//! in Windows' **Default apps**. Windows lets only the person choose the
//! default, so [`choose_in_settings`] opens that page. Templates and
//! slide shows aren't taken: their default verb isn't "open".

use std::path::Path;

use anyhow::{Context, Result};

/// The extensions, with the file type Office usually registers for each.
pub const TYPES: [(&str, &str); 7] = [
    ("docx", "Word.Document.12"),
    ("docm", "Word.DocumentMacroEnabled.12"),
    ("xlsx", "Excel.Sheet.12"),
    ("xlsm", "Excel.SheetMacroEnabled.12"),
    ("xlsb", "Excel.SheetBinaryMacroEnabled.12"),
    ("pptx", "PowerPoint.Show.12"),
    ("pptm", "PowerPoint.ShowMacroEnabled.12"),
];

/// Our name in `RegisteredApplications` (and Default apps).
const APP: &str = "OneUpDrive";
const CAPABILITIES: &str = r"Software\OneUpDrive\Capabilities";
const REGISTERED: &str = r"Software\RegisteredApplications";
/// On our file type: the Office file type it stands in for.
const OFFICE_TYPE: &str = "OneUpDriveOfficeType";

fn ours(ext: &str) -> String {
    format!("OneUpDrive.Office.{ext}")
}

fn classes(name: &str) -> String {
    format!(r"Software\Classes\{name}")
}

fn ext_of(path: &Path) -> Option<String> {
    let ext = path.extension()?.to_str()?.to_ascii_lowercase();
    TYPES.iter().any(|(e, _)| *e == ext).then_some(ext)
}

/// Office's file type for `ext`: the one recorded when registering, else
/// the extension's own (unless it's ours), else Office's usual name.
pub fn office_type(ext: &str) -> Option<String> {
    oneup_sys::user_registry::get(&classes(&ours(ext)), OFFICE_TYPE)
        .ok()
        .flatten()
        .or_else(|| oneup_sys::assoc::classes_default(&format!(".{ext}")).filter(|c| !c.starts_with("OneUpDrive.")))
        .or_else(|| TYPES.iter().find(|(e, _)| *e == ext).map(|(_, t)| (*t).to_string()))
}

/// Makes OneUpDrive a program Windows offers for Office files, opening
/// them with `exe` (`oneup-open.exe`). Returns the extensions registered
/// (those Office handles on this PC).
pub fn register(exe: &Path) -> Result<Vec<String>> {
    use oneup_sys::user_registry::{remove_key, set};
    let exe = exe.display().to_string();
    let exe = exe.strip_prefix(r"\\?\").unwrap_or(&exe);
    let mut done = Vec::new();
    for (ext, usual) in TYPES {
        let office = oneup_sys::assoc::classes_default(&format!(".{ext}"))
            .filter(|c| !c.starts_with("OneUpDrive."))
            .unwrap_or_else(|| usual.to_string());
        let key = classes(&ours(ext));
        remove_key(&key)?;
        match oneup_sys::assoc::copy_class(&office, &key) {
            Ok(()) => {}
            // Office doesn't handle this type here.
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => continue,
            Err(e) => return Err(e).with_context(|| format!("copying Office's file type for .{ext}")),
        }
        // Only "open" changes; a version-independent pointer would lead past us.
        remove_key(&format!(r"{key}\shell\Open"))?;
        remove_key(&format!(r"{key}\CurVer"))?;
        set(&format!(r"{key}\shell"), "", "open")?;
        set(&format!(r"{key}\shell\open\command"), "", &format!("\"{exe}\" \"%1\""))?;
        set(&key, OFFICE_TYPE, &office)?;
        set(&classes(&format!(r".{ext}\OpenWithProgids")), &ours(ext), "")?;
        set(&format!(r"{CAPABILITIES}\FileAssociations"), &format!(".{ext}"), &ours(ext))?;
        done.push(ext.to_string());
    }
    set(CAPABILITIES, "ApplicationName", "OneUpDrive")?;
    set(
        CAPABILITIES,
        "ApplicationDescription",
        "Opens Office files in OneUpDrive's folders from OneDrive, with AutoSave; others open in Office as usual.",
    )?;
    set(REGISTERED, APP, CAPABILITIES)?;
    oneup_sys::assoc::changed();
    Ok(done)
}

/// Removes everything [`register`] wrote. Returns the extensions that had
/// OneUpDrive as their default: Windows asks which program to use for
/// those the next time one is opened.
pub fn unregister() -> Result<Vec<String>> {
    let was = defaults();
    for (ext, _) in TYPES {
        oneup_sys::user_registry::remove_key(&classes(&ours(ext)))?;
        oneup_sys::user_registry::remove(&classes(&format!(r".{ext}\OpenWithProgids")), &ours(ext))?;
    }
    oneup_sys::user_registry::remove_key(CAPABILITIES)?;
    oneup_sys::user_registry::remove(REGISTERED, APP)?;
    oneup_sys::assoc::changed();
    Ok(was)
}

pub fn is_registered() -> bool {
    oneup_sys::user_registry::get(REGISTERED, APP).ok().flatten().is_some()
}

/// The extensions whose default program the person set to OneUpDrive.
pub fn defaults() -> Vec<String> {
    TYPES
        .iter()
        .filter(|(ext, _)| {
            let key = format!(r"Software\Microsoft\Windows\CurrentVersion\Explorer\FileExts\.{ext}\UserChoice");
            oneup_sys::user_registry::get(&key, "ProgId").ok().flatten().is_some_and(|p| p == ours(ext))
        })
        .map(|(ext, _)| (*ext).to_string())
        .collect()
}

/// Opens Windows' Default apps page for OneUpDrive.
pub fn choose_in_settings() -> Result<()> {
    oneup_sys::assoc::open(&format!("ms-settings:defaultapps?registeredAppUser={APP}"))?;
    Ok(())
}

/// How a file is to be opened.
#[derive(Debug, PartialEq, Eq)]
pub enum Choice {
    /// From OneDrive, with this address (`ms-word:ofe|u|https://…`).
    Cloud(String),
    /// With Office's own "open", for this reason.
    Local(String),
}

/// Decides how `path` opens. Never fails: anything uncertain opens here.
pub async fn choose(path: &Path) -> Choice {
    match cloud(path).await {
        Ok(uri) => Choice::Cloud(uri),
        Err(e) => Choice::Local(format!("{e:#}")),
    }
}

async fn cloud(path: &Path) -> Result<String> {
    ext_of(path).context("not a type OneUpDrive opens from OneDrive")?;
    // In a pre-release build, only test accounts' folders (as for sync
    // itself), so no other data folder's database is opened.
    let dirs =
        crate::datafolders::all().into_iter().filter(|d| crate::setup::require_test_account(d).is_ok()).collect();
    let (dir, rel) = crate::datafolders::for_path_in(path, dirs).context("not in a folder OneUpDrive syncs")?;
    let it = crate::item::Item::in_dir(dir, rel)?;
    if it.folder {
        anyhow::bail!("a folder");
    }
    if !it.unchanged(path)? {
        anyhow::bail!("changed on this PC since it was last synced, so the version here is opened");
    }
    tokio::time::timeout(std::time::Duration::from_secs(15), crate::item::office_uri_of(&it))
        .await
        .context("OneDrive didn't answer in time")?
}

/// Opens `path` with Office's own "open".
pub fn open_here(path: &Path) -> Result<()> {
    let ext = path.extension().and_then(|e| e.to_str()).unwrap_or_default().to_ascii_lowercase();
    let class = office_type(&ext).context("no Office program for this file type")?;
    oneup_sys::assoc::open_as(&path.display().to_string(), &class)?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use std::path::Path;

    #[test]
    fn only_documents_that_open_are_taken() {
        assert_eq!(super::ext_of(Path::new(r"C:\a\Report.DOCX")).as_deref(), Some("docx"));
        assert_eq!(super::ext_of(Path::new(r"C:\a\b.xlsb")).as_deref(), Some("xlsb"));
        // Templates and slide shows open differently; old formats lack AutoSave.
        for f in ["t.dotx", "s.ppsx", "old.doc", "notes.txt", "noext"] {
            assert_eq!(super::ext_of(Path::new(f)), None, "{f}");
        }
    }

    #[tokio::test]
    async fn other_files_open_here() {
        let c = super::choose(Path::new(r"C:\nowhere\notes.txt")).await;
        assert!(matches!(c, super::Choice::Local(_)), "{c:?}");
    }
}
