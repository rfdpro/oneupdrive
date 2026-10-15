//! Which names the effects layer will create.
//!
//! The planner's names come from the cloud (or from our own conflict and
//! staging rules), and the effects layer joins them onto real paths. A name
//! that isn't a single, plain Windows path component could escape its folder
//! (`..`, separators), be silently altered by Windows (trailing dots and
//! spaces are stripped, so the file would not have the name we recorded), or
//! address a device (`NUL`, `COM1`). Such names are refused, never "fixed":
//! fixing is a planner decision the user can see.

/// Longest name NTFS allows, in UTF-16 units.
const MAX_NAME_UNITS: usize = 255;

/// Whether `name` can be created as-is in a local folder. `at_root`: the
/// folder is the sync folder itself, where OneUpDrive's own folder lives.
pub fn is_valid_name(name: &str, at_root: bool) -> bool {
    if name.is_empty() || name == "." || name == ".." {
        return false;
    }
    if name.encode_utf16().count() > MAX_NAME_UNITS {
        return false;
    }
    if name.ends_with(' ') || name.ends_with('.') {
        return false;
    }
    if name.chars().any(|c| c < ' ' || matches!(c, '\\' | '/' | ':' | '*' | '?' | '"' | '<' | '>' | '|')) {
        return false;
    }
    if is_device_name(name) {
        return false;
    }
    !(at_root && oneup_rules::is_engine_dir(name))
}

/// `CON`, `nul.txt`, `COM1 .log` and the like: Win32 maps these to devices
/// in every folder, whatever the extension.
fn is_device_name(name: &str) -> bool {
    let stem = name.split('.').next().unwrap_or(name).trim_end_matches(' ').to_ascii_uppercase();
    match stem.as_str() {
        "CON" | "PRN" | "AUX" | "NUL" | "CONIN$" | "CONOUT$" => true,
        s if s.len() == 4 && (s.starts_with("COM") || s.starts_with("LPT")) => {
            // COM0/LPT0 are reserved too since Windows 11; the superscript
            // digits (COM¹) are not ASCII and so can't reach this branch.
            s.as_bytes()[3].is_ascii_digit()
        }
        _ => false,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn plain_names_pass() {
        for n in ["a.txt", "Report (1).docx", ".gitignore", "x.oneup-staging-1", "ünïcödé ☃", "COMMA", "con1", "LPTX"]
        {
            assert!(is_valid_name(n, false), "{n}");
        }
        assert!(is_valid_name(".oneup", false), "only reserved at the top level");
    }

    #[test]
    fn dangerous_names_fail() {
        let long = "a".repeat(256);
        for n in [
            "",
            ".",
            "..",
            "a\\b",
            "a/b",
            "c:x",
            "a*",
            "q?",
            "\"",
            "<",
            ">",
            "|",
            "tab\there",
            "dot.",
            "space ",
            "CON",
            "nul.txt",
            "Com1",
            "lpt9.log",
            "aux .x",
            &long,
        ] {
            assert!(!is_valid_name(n, false), "{n:?}");
        }
        assert!(!is_valid_name(".OneUp", true));
        assert!(is_valid_name(&"a".repeat(255), false));
    }
}
