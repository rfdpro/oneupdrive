//! One rule line in gitignore syntax (see `docs/architecture/rules.md`),
//! and whether it matches a path relative to the folder it belongs to.

use crate::glob::NameGlob;

#[derive(Debug, Clone, PartialEq, Eq)]
enum Seg {
    Name(NameGlob),
    /// `**`: any number of folders.
    Deep,
}

#[derive(Debug, Clone, PartialEq, Eq)]
enum Shape {
    /// No `/` except a trailing one: matches an item's own name, at any
    /// depth below the rule's folder.
    Name(NameGlob),
    /// Matched against the whole path from the rule's folder.
    Anchored(Vec<Seg>),
}

/// A parsed rule line.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Rule {
    /// The line as written (trimmed), for `explain`.
    pub text: String,
    /// `!`: re-includes.
    pub negated: bool,
    /// Trailing `/`: folders only.
    pub dir_only: bool,
    shape: Shape,
}

impl Rule {
    /// Parses one line; `None` for blank lines and comments (and lines that
    /// are nothing but `/` or `!`).
    pub fn parse(line: &str) -> Option<Self> {
        let text = line.trim();
        if text.is_empty() || text.starts_with('#') {
            return None;
        }
        let (negated, rest) = match text.strip_prefix('!') {
            Some(r) => (true, r),
            None => (false, text),
        };
        // `\` is a folder separator here (rules.md), like `/`.
        let rest = rest.replace('\\', "/");
        let dir_only = rest.ends_with('/');
        let body = rest.trim_end_matches('/');
        let shape = if body.contains('/') {
            let segs: Vec<Seg> = body
                .split('/')
                .filter(|s| !s.is_empty())
                .map(|s| if s == "**" { Seg::Deep } else { Seg::Name(NameGlob::new(s)) })
                .collect();
            if segs.is_empty() {
                return None;
            }
            Shape::Anchored(segs)
        } else {
            if body.is_empty() {
                return None;
            }
            Shape::Name(NameGlob::new(body))
        };
        Some(Self { text: text.to_string(), negated, dir_only, shape })
    }

    /// Whether the rule matches the item at `path` (names from the rule's
    /// folder down to the item, non-empty).
    pub fn matches(&self, path: &[&str], is_dir: bool) -> bool {
        if self.dir_only && !is_dir {
            return false;
        }
        match &self.shape {
            Shape::Name(g) => path.last().is_some_and(|n| g.matches(n)),
            Shape::Anchored(segs) => anchored(segs, path),
        }
    }

    /// [`Rule::matches`], with `path`'s last part already folded
    /// (`crate::glob::folded`), for checking one path against many rules.
    pub fn matches_folded_last(&self, path: &[&str], last: &[char], is_dir: bool) -> bool {
        if self.dir_only && !is_dir {
            return false;
        }
        match &self.shape {
            Shape::Name(g) => !path.is_empty() && g.matches_folded(last),
            Shape::Anchored(segs) => anchored(segs, path),
        }
    }

    /// The folder name this rule excludes wherever it appears, if that can
    /// be told from the name alone (an unanchored, non-negated folder rule):
    /// what enumeration may skip without listing.
    pub(crate) fn name_glob(&self) -> Option<&NameGlob> {
        match &self.shape {
            Shape::Name(g) if !self.negated => Some(g),
            _ => None,
        }
    }

    /// Whether this rule could match an item called `name` somewhere, judged
    /// by its last part only. Used to keep a folder name out of pruning when
    /// a `!` rule might re-include such a folder.
    pub(crate) fn could_match_name(&self, name: &str) -> bool {
        match &self.shape {
            Shape::Name(g) => g.matches(name),
            Shape::Anchored(segs) => match segs.last() {
                Some(Seg::Name(g)) => g.matches(name),
                Some(Seg::Deep) | None => true,
            },
        }
    }
}

/// Matches segments against path parts. `**` matches any number of parts,
/// except at the end, where it needs at least one (`a/**` is what's
/// *inside* `a`, as in git).
fn anchored(segs: &[Seg], path: &[&str]) -> bool {
    match segs.split_first() {
        None => path.is_empty(),
        Some((Seg::Deep, rest)) => {
            let min = usize::from(rest.is_empty());
            (min..=path.len()).any(|skip| anchored(rest, &path[skip..]))
        }
        Some((Seg::Name(g), rest)) => match path.split_first() {
            Some((first, more)) => g.matches(first) && anchored(rest, more),
            None => false,
        },
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn r(line: &str) -> Rule {
        Rule::parse(line).unwrap_or_else(|| panic!("{line} should parse"))
    }
    fn p(s: &str) -> Vec<&str> {
        s.split('/').collect()
    }

    #[test]
    fn comments_and_blanks_are_not_rules() {
        for l in ["", "   ", "# note", "/", "!", "!/"] {
            assert_eq!(Rule::parse(l), None, "{l:?}");
        }
    }

    #[test]
    fn a_plain_name_matches_at_any_depth_and_slash_means_folders_only() {
        let any = r("build");
        assert!(any.matches(&p("build"), true) && any.matches(&p("a/b/build"), false));
        let dir = r("node_modules/");
        assert!(dir.matches(&p("app/node_modules"), true));
        assert!(!dir.matches(&p("app/node_modules"), false), "a file of that name");
        assert!(r("*.iso").matches(&p("x/Disk.ISO"), false));
    }

    #[test]
    fn a_slash_anchors_to_the_rules_folder() {
        let top = r("/Downloads/");
        assert!(top.matches(&p("Downloads"), true));
        assert!(!top.matches(&p("x/Downloads"), true));
        let mid = r("docs/drafts");
        assert!(mid.matches(&p("docs/drafts"), true) && !mid.matches(&p("x/docs/drafts"), true));
        // Backslash is a separator too.
        assert!(r(r"docs\drafts").matches(&p("docs/drafts"), false));
    }

    #[test]
    fn double_star_spans_folders() {
        let lead = r("**/cache");
        assert!(lead.matches(&p("cache"), true) && lead.matches(&p("a/b/cache"), true));
        let inside = r("logs/**");
        assert!(inside.matches(&p("logs/a.txt"), false) && inside.matches(&p("logs/x/y"), false));
        assert!(!inside.matches(&p("logs"), true), "the folder itself isn't inside itself");
        let mid = r("a/**/z");
        assert!(mid.matches(&p("a/z"), false) && mid.matches(&p("a/b/c/z"), false));
        assert!(!mid.matches(&p("b/z"), false));
    }

    #[test]
    fn negation_is_recorded_not_applied_here() {
        let n = r("!keep.txt");
        assert!(n.negated && n.matches(&p("x/keep.txt"), false));
        assert_eq!(n.text, "!keep.txt");
    }

    #[test]
    fn pruning_helpers() {
        assert!(r("node_modules/").name_glob().is_some());
        assert!(r("/top/").name_glob().is_none(), "anchored");
        assert!(r("!x/").name_glob().is_none(), "negated");
        assert!(r("!app/node_modules/").could_match_name("node_modules"));
        assert!(r("!a/**").could_match_name("anything"));
        assert!(!r("!a/keep").could_match_name("node_modules"));
    }
}
