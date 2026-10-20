//! Rules: what syncs (see `docs/architecture/rules.md`).
//!
//! * [`RuleSet`]: the user's rules (opt-in presets, `config.toml`,
//!   `.oneupignore` files) in gitignore syntax, with `explain` data.
//!   Planning applies it in full.
//! * [`ExcludeSet`]: the part enumeration may apply by folder name alone
//!   ([`RuleSet::pruning`]), so excluded folders are never listed.
//!
//! * [`database`]: which files are databases, and when a copy of one is
//!   consistent.
//!
//! Pure: callers read the rule files.

use std::collections::BTreeMap;

pub mod database;
mod glob;
mod pattern;
mod pin;
mod presets;
mod set;

pub use glob::NameGlob;
pub use pattern::Rule;
pub use pin::PinSet;
pub use presets::{DEV_PRESET, PRESETS, Preset, preset};
pub use set::{IGNORE_FILE, Match, RuleSet, Source, UnknownPreset, Verdict, literal_path_rules};

/// OneUpDrive's own folder directly inside the sync folder: the local
/// version store, download staging and the sync-folder marker. Always
/// excluded at the top level on **both** sides (a cloud folder of that name
/// in the drive root is never synced either), whatever the rules say, so
/// our bookkeeping never syncs and a cloud folder can never be merged into
/// it. Deeper folders of the same name are ordinary.
pub const ENGINE_DIR: &str = ".oneup";

/// Whether `name`, directly inside the sync folder (or the drive root), is
/// [`ENGINE_DIR`]. Case-insensitive, like Windows and OneDrive names.
pub fn is_engine_dir(name: &str) -> bool {
    name.eq_ignore_ascii_case(ENGINE_DIR)
}

/// Folder names (or name wildcards) whose whole subtree is excluded,
/// wherever such a folder is.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct ExcludeSet {
    /// Plain names, folded ([`oneup_sync::fold`]), each with the name as
    /// shown (lower-cased).
    folder_names: BTreeMap<String, String>,
    /// Wildcard patterns: folded, as shown, and compiled.
    globs: Vec<(String, String, NameGlob)>,
}

impl ExcludeSet {
    /// Excludes nothing.
    pub fn none() -> Self {
        Self::default()
    }

    /// The `dev` preset.
    pub fn dev() -> Self {
        Self::from_names(DEV_PRESET.iter().copied())
    }

    pub fn from_names<'a>(names: impl IntoIterator<Item = &'a str>) -> Self {
        let mut set = Self::default();
        for n in names {
            set.folder_names.entry(oneup_sync::fold(n)).or_insert_with(|| n.to_lowercase());
        }
        set
    }

    /// Names, some of which may be wildcards (`*.egg-info`).
    pub fn from_patterns<'a>(patterns: impl IntoIterator<Item = &'a str>) -> Self {
        let mut set = Self::default();
        for p in patterns {
            let g = NameGlob::new(p);
            let key = oneup_sync::fold(p);
            match g.literal() {
                Some(name) => _ = set.folder_names.entry(name).or_insert_with(|| p.to_lowercase()),
                None if !set.globs.iter().any(|(k, ..)| *k == key) => set.globs.push((key, p.to_lowercase(), g)),
                None => {}
            }
        }
        set.globs.sort_by(|a, b| a.0.cmp(&b.0));
        set
    }

    /// Whether a folder with this name (and therefore everything below it) is excluded.
    pub fn excludes_folder(&self, name: &str) -> bool {
        if self.is_empty() {
            return false;
        }
        if self.folder_names.contains_key(&oneup_sync::fold(name)) {
            return true;
        }
        if self.globs.is_empty() {
            return false;
        }
        let name = glob::folded(name);
        self.globs.iter().any(|(.., g)| g.matches_folded(&name))
    }

    pub fn is_empty(&self) -> bool {
        self.folder_names.is_empty() && self.globs.is_empty()
    }

    /// The names and wildcards, lower-cased and sorted.
    pub fn patterns(&self) -> Vec<String> {
        let mut all: Vec<String> = self.folder_names.values().cloned().collect();
        all.extend(self.globs.iter().map(|(_, shown, _)| shown.clone()));
        all.sort_unstable();
        all
    }

    /// Stable description of the rules. When it changes between runs, stored
    /// state must be re-evaluated.
    pub fn fingerprint(&self) -> String {
        let mut all: Vec<&str> = self.folder_names.keys().map(String::as_str).collect();
        all.extend(self.globs.iter().map(|(k, ..)| k.as_str()));
        all.sort_unstable();
        // v2: the engine folder (`ENGINE_DIR`) became excluded at the top
        // level; stored trees made under v1 must be re-evaluated. v3: names
        // fold as Windows compares them (`oneup_sync::fold`), so a folder
        // skipped under v2 may now be listed, or the reverse. An empty set
        // matches nothing either way, so it keeps its v2 fingerprint: a data
        // folder without folder rules has nothing to re-evaluate.
        if all.is_empty() { "v2:folders=".into() } else { format!("v3:folders={}", all.join("/")) }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn dev_preset_matches_case_insensitively() {
        let r = ExcludeSet::dev();
        assert!(r.excludes_folder("node_modules"));
        assert!(r.excludes_folder("Node_Modules"));
        assert!(r.excludes_folder("__pycache__"));
        assert!(!r.excludes_folder("src"));
        assert!(!r.excludes_folder("node_modules_backup"));
    }

    #[test]
    fn wildcard_folder_names() {
        let r = ExcludeSet::from_patterns(["*.egg-info", "build"]);
        assert!(r.excludes_folder("Pkg.Egg-Info") && r.excludes_folder("BUILD") && !r.excludes_folder("src"));
        assert_eq!(r.fingerprint(), "v3:folders=*.EGG-INFO/BUILD");
        assert_eq!(r.patterns(), ["*.egg-info", "build"]);
        assert_eq!(ExcludeSet::none().fingerprint(), "v2:folders=", "unchanged: nothing to re-evaluate");
    }

    #[test]
    fn none_excludes_nothing() {
        assert!(!ExcludeSet::none().excludes_folder("node_modules"));
    }

    #[test]
    fn fingerprint_is_order_independent_and_distinguishes_sets() {
        let a = ExcludeSet::from_names(["b", "A"]);
        let b = ExcludeSet::from_names(["a", "B"]);
        assert_eq!(a.fingerprint(), b.fingerprint());
        assert_ne!(ExcludeSet::dev().fingerprint(), ExcludeSet::none().fingerprint());
    }
}
