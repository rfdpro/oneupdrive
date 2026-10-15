//! Ready-made rule sets the user can turn on. Offered, never imposed: none
//! is on unless the user chose it (`rules.md`).

/// A preset's name and its rule lines.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Preset {
    pub name: &'static str,
    pub summary: &'static str,
    pub lines: &'static [&'static str],
}

/// Folder names of the `dev` preset (also the original built-in exclusion list).
pub const DEV_PRESET: &[&str] = &[
    "node_modules",
    ".venv",
    "venv",
    "__pycache__",
    "site-packages",
    ".git",
    "target",
    ".next",
    ".cache",
    ".tox",
    ".mypy_cache",
    ".pytest_cache",
    ".gradle",
];

const DEV_LINES: &[&str] = &[
    "node_modules/",
    ".venv/",
    "venv/",
    "__pycache__/",
    "site-packages/",
    ".git/",
    "target/",
    ".next/",
    ".cache/",
    ".tox/",
    ".mypy_cache/",
    ".pytest_cache/",
    ".gradle/",
];

/// All presets, in the order their rules apply (a later one wins a tie).
pub const PRESETS: &[Preset] = &[
    Preset { name: "dev", summary: "dependencies, virtual environments, build output and caches", lines: DEV_LINES },
    Preset {
        name: "temp",
        summary: "editor swap and backup files",
        lines: &["*.swp", "*.swo", "*~", ".~lock.*#", "*.bak"],
    },
    Preset {
        name: "os-junk",
        summary: "thumbnail caches and system folders",
        lines: &["Thumbs.db", "ehthumbs.db", ".DS_Store", "._*", "$RECYCLE.BIN/", "System Volume Information/"],
    },
];

/// The preset called `name` (case-insensitive).
pub fn preset(name: &str) -> Option<&'static Preset> {
    PRESETS.iter().find(|p| p.name.eq_ignore_ascii_case(name))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn dev_lines_are_the_dev_folder_names() {
        let names: Vec<String> = DEV_LINES.iter().map(|l| l.trim_end_matches('/').to_string()).collect();
        assert_eq!(names, DEV_PRESET);
    }

    #[test]
    fn presets_are_found_by_name() {
        assert_eq!(preset("DEV").map(|p| p.name), Some("dev"));
        assert!(preset("everything").is_none());
    }
}
