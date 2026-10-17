//! Enumeration hints: where excluded folders are expected to be.
//!
//! Enumeration cost is dominated by junk that is downloaded and then dropped
//! (78% of all entries on a measured developer drive). Listing a folder with
//! `children` costs one request but lets the mirror *skip* an excluded child
//! without downloading anything inside it; a folder-scoped delta downloads the
//! whole subtree, junk included. So the ideal split lists exactly the folders
//! that lead to excluded folders and uses delta everywhere else.
//!
//! A [`SplitHint`] says which folders those are, learned from a local scan of
//! the user's OneDrive folder or from a previous mirror. **Hints only change
//! how the drive is enumerated, never what ends up in the mirror**: a wrong or
//! stale hint costs speed, not correctness (the randomized simulation feeds
//! random hints to prove it).

use std::collections::HashSet;

/// Upper bound on stored hint paths, so a pathological drive can't make the
/// hint unbounded in memory. Beyond it, the remaining excluded folders are
/// simply not hinted (they get the size heuristic's treatment: delta).
/// 250k short paths is a few tens of MiB at most; real drives need far fewer
/// (a large developer drive: ~1,000 excluded folders).
pub const MAX_HINT_PATHS: usize = 250_000;

/// The set of folders (as normalized drive-relative paths) that have an
/// excluded folder somewhere below them.
#[derive(Clone, Default, PartialEq, Eq)]
pub struct SplitHint {
    ancestors: HashSet<String>,
    /// How many excluded paths were offered (for reporting).
    sources: usize,
    truncated: bool,
}

impl std::fmt::Debug for SplitHint {
    // A hint can hold many paths; tests print `Config` on failure, so keep it short.
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(
            f,
            "SplitHint({} excluded, {} folders to list{})",
            self.sources,
            self.ancestors.len(),
            if self.truncated { ", truncated" } else { "" }
        )
    }
}

/// Folds and normalizes a drive-relative path: always a leading `/`, never a
/// trailing one (except the root itself, `/`), backslashes treated as
/// separators. Names fold as OneDrive and Windows compare them
/// ([`oneup_sync::fold`]).
fn normalize(path: &str) -> String {
    let parts: Vec<String> = path.split(['/', '\\']).filter(|p| !p.is_empty()).map(oneup_sync::fold).collect();
    format!("/{}", parts.join("/"))
}

impl SplitHint {
    /// Builds a hint from the paths of excluded folders (e.g.
    /// `/Code/app/node_modules`). Every ancestor of each path is recorded.
    pub fn from_excluded_paths<S: AsRef<str>>(paths: impl IntoIterator<Item = S>) -> Self {
        let mut h = SplitHint::default();
        h.add(paths);
        h
    }

    /// Adds more excluded paths (e.g. from a second source).
    pub fn add<S: AsRef<str>>(&mut self, paths: impl IntoIterator<Item = S>) {
        for p in paths {
            self.sources += 1;
            let p = normalize(p.as_ref());
            // Walk up from the excluded folder's parent to the root. Stop early
            // when an ancestor is already known: its own ancestors are too.
            let mut cur = p.as_str();
            while let Some(i) = cur.rfind('/') {
                let parent = if i == 0 { "/" } else { &cur[..i] };
                if self.ancestors.len() >= MAX_HINT_PATHS {
                    self.truncated = true;
                    return;
                }
                if !self.ancestors.insert(parent.to_string()) || parent == "/" {
                    break;
                }
                cur = parent;
            }
        }
    }

    /// Whether listing `folder_path` is expected to reveal an excluded folder
    /// below it (so it should be listed rather than delta-enumerated).
    pub fn leads_to_excluded(&self, folder_path: &str) -> bool {
        self.ancestors.contains(&normalize(folder_path))
    }

    pub fn is_empty(&self) -> bool {
        self.sources == 0
    }

    /// Number of excluded paths this hint was built from.
    pub fn excluded_count(&self) -> usize {
        self.sources
    }

    /// Number of folders the hint marks for listing.
    pub fn folders_to_list(&self) -> usize {
        self.ancestors.len()
    }

    pub fn is_truncated(&self) -> bool {
        self.truncated
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn ancestors_are_hinted_case_insensitively() {
        let h = SplitHint::from_excluded_paths(["/Code/App/node_modules", "/Code/Other/.venv", r"\Docs\x\target\"]);
        for p in ["/", "/code", "/CODE/app", "/code/other", "/docs/x", "/Docs/X/"] {
            assert!(h.leads_to_excluded(p), "{p}");
        }
        // The excluded folders themselves and unrelated folders are not.
        for p in ["/code/app/node_modules", "/code/app/src", "/music", "/codex"] {
            assert!(!h.leads_to_excluded(p), "{p}");
        }
        assert_eq!((h.excluded_count(), h.folders_to_list()), (3, 6));
        assert!(!h.is_empty());
    }

    #[test]
    fn names_fold_as_windows_compares_them() {
        let h = SplitHint::from_excluded_paths(["/Λογος/node_modules", "/\u{212A}it/node_modules"]);
        assert!(h.leads_to_excluded("/ΛΟΓΟΣ") && h.leads_to_excluded("/λογοσ"));
        assert!(!h.leads_to_excluded("/kit"), "Kelvin sign");
    }

    #[test]
    fn empty_and_top_level_hints() {
        assert!(SplitHint::default().is_empty());
        let h = SplitHint::from_excluded_paths(["/.cache"]);
        assert!(h.leads_to_excluded("/"));
        assert!(!h.leads_to_excluded("/.cache"));
    }

    #[test]
    fn size_is_bounded() {
        let paths: Vec<String> = (0..MAX_HINT_PATHS + 10).map(|i| format!("/p{i}/node_modules")).collect();
        let h = SplitHint::from_excluded_paths(&paths);
        assert!(h.is_truncated());
        assert!(h.folders_to_list() <= MAX_HINT_PATHS);
    }
}
