//! OneDrive's naming and size limits, as published by Microsoft in
//! "Restrictions and limitations in OneDrive and SharePoint". Pure string
//! checks, applied to items that exist only on this PC: those are the ones
//! a sync would try to upload.

use crate::report::NotSynced;

/// Longest path OneDrive accepts, in UTF-16 code units, counted from the
/// drive's root.
pub const MAX_PATH_UNITS: usize = 400;

/// Largest file OneDrive accepts: 250 GB. Taken as decimal gigabytes, the
/// smaller reading, so a file between the two readings is reported as "too
/// large" rather than failing halfway through an upload.
pub const MAX_FILE_BYTES: i64 = 250_000_000_000;

/// Characters OneDrive refuses anywhere in a name. Windows forbids them too,
/// but NTFS can still hold them (e.g. files created by WSL or by POSIX tools).
const FORBIDDEN_CHARS: &[char] = &['"', '*', ':', '<', '>', '?', '/', '\\', '|'];

/// Names OneDrive refuses outright (compared case-insensitively).
const RESERVED: &[&str] = &[".lock", "con", "prn", "aux", "nul"];

/// Whether OneDrive would refuse `name` for a file or folder.
///
/// Not checked: the SharePoint-only rule about `forms` at a library's root
/// (doesn't apply to OneDrive Personal), and characters SharePoint admins can
/// disable (`#`, `%`), which OneDrive Personal accepts.
pub fn is_invalid(name: &str) -> bool {
    let lower = name.to_lowercase();
    name.is_empty()
        || name.contains(FORBIDDEN_CHARS)
        || name.contains(char::is_control)
        // "Leading and trailing spaces ... aren't allowed." A trailing dot is
        // not on Microsoft's list, but Windows apps can't open such names
        // (they need `\\?\` paths), so another PC couldn't use the file
        // either. Reporting it is the cautious choice.
        || name.starts_with(' ')
        || name.ends_with([' ', '.'])
        || RESERVED.contains(&lower.as_str())
        || is_numbered_device(&lower)
        || lower.contains("_vti_")
}

/// `com0`..`com9` and `lpt0`..`lpt9`.
fn is_numbered_device(lower: &str) -> bool {
    let b = lower.as_bytes();
    b.len() == 4 && (b.starts_with(b"com") || b.starts_with(b"lpt")) && b[3].is_ascii_digit()
}

/// Whether the official client deliberately leaves `name` out of sync. These
/// files are local by nature, so it isn't a problem that they're missing from
/// OneDrive, and they shouldn't be counted as "would upload".
pub fn not_synced(name: &str) -> Option<NotSynced> {
    let lower = name.to_lowercase();
    if lower == "desktop.ini" {
        Some(NotSynced::SystemFile)
    } else if lower.starts_with("~$") {
        Some(NotSynced::OfficeLock)
    } else if lower.ends_with(".tmp") {
        Some(NotSynced::Temporary)
    } else {
        None
    }
}

/// Whether a name is the OneDrive app's hidden, locked system file: a dot
/// followed by a GUID. The name `.849C9593-D756-4E56-8D6E-42412F2A707B` is the
/// same on every installation (found on a real OneDrive folder, and
/// widely reported in Microsoft Q&A); it can appear in any folder the app
/// syncs and never exists in OneDrive itself. The shape is matched rather than
/// the exact GUID, in case it changes; a user's own file named like that is
/// improbable.
pub fn is_onedrive_marker(name: &str) -> bool {
    let Some(guid) = name.strip_prefix('.') else { return false };
    let groups: Vec<&str> = guid.split('-').collect();
    groups.iter().map(|g| g.len()).eq([8, 4, 4, 4, 12])
        && groups.iter().all(|g| g.bytes().all(|b| b.is_ascii_hexdigit()))
}

/// Length of a path as OneDrive counts it (UTF-16 code units, without the
/// leading slash of our "/a/b" form).
pub fn path_units(path: &str) -> usize {
    path.trim_start_matches('/').encode_utf16().count()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn invalid_names() {
        for bad in [
            "a:b",
            "what?",
            "pipe|",
            "q\"uote",
            " lead",
            "trail ",
            "dot.",
            "CON",
            "con",
            "Lpt9",
            "COM0",
            ".lock",
            "x_vti_y",
            "",
            "tab\tname",
        ] {
            assert!(is_invalid(bad), "{bad:?} should be invalid");
        }
        for ok in [
            "CON.txt",
            "CON_not_reserved.txt",
            "COM10",
            "comma,ok",
            "naïve café",
            "日本語",
            "emoji 😀",
            ".gitignore",
            "#hash%",
            "a.b.c",
        ] {
            assert!(!is_invalid(ok), "{ok:?} should be valid");
        }
    }

    #[test]
    fn not_synced_names() {
        assert_eq!(not_synced("Desktop.ini"), Some(NotSynced::SystemFile));
        assert_eq!(not_synced("~$Report.docx"), Some(NotSynced::OfficeLock));
        assert_eq!(not_synced("build.TMP"), Some(NotSynced::Temporary));
        assert_eq!(not_synced("report.docx"), None);
    }

    #[test]
    fn onedrive_marker() {
        assert!(is_onedrive_marker(".849C9593-D756-4E56-8D6E-42412F2A707B"));
        assert!(!is_onedrive_marker("849C9593-D756-4E56-8D6E-42412F2A707B"), "needs the dot");
        assert!(!is_onedrive_marker(".849C9593-D756-4E56-8D6E-42412F2A707"), "short last group");
        assert!(!is_onedrive_marker(".gitignore"));
        assert!(!is_onedrive_marker(".849C9593-D756-4E56-8D6E-42412F2A70ZZ"));
    }

    #[test]
    fn path_lengths_count_utf16_units() {
        assert_eq!(path_units("/ab/c"), 4);
        assert_eq!(path_units("/😀"), 2, "astral characters count twice, as in Windows");
    }
}
