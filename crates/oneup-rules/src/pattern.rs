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

    /// For every prefix of a path at once, whether the rule matches it as a
    /// folder: entry `i` is [`Rule::matches`] of the first `i + 1` parts,
    /// given folded (`crate::glob::folded`), with `is_dir`. One
    /// pass over the path, where asking each prefix in turn costs the
    /// path's depth squared: a rule set's verdict on an item asks every
    /// folder above it too.
    pub(crate) fn prefix_matches(&self, folded: &[Vec<char>]) -> Vec<bool> {
        match &self.shape {
            Shape::Name(g) => folded.iter().map(|f| g.matches_folded(f)).collect(),
            Shape::Anchored(segs) => anchored_prefixes(segs, folded),
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

    /// The folder (folded parts, relative to the rule's folder) holding
    /// everything this rule can match: the literal parts before the first
    /// wildcard, less the item's own name when every part is literal.
    /// `None` for a name rule, which can match at any depth.
    pub(crate) fn within(&self) -> Option<Vec<String>> {
        let Shape::Anchored(segs) = &self.shape else { return None };
        let mut out = Vec::new();
        for s in segs {
            match s {
                Seg::Name(g) => match g.literal() {
                    Some(name) => out.push(name),
                    None => return Some(out),
                },
                Seg::Deep => return Some(out),
            }
        }
        out.pop();
        Some(out)
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
///
/// Rule lines come from `.oneupignore` files, which sync from OneDrive, so
/// no arrangement of `**`s may make this slow. Trying every split, as
/// [`anchored_one_deep`] does, is exponential in the number of `**`s (12
/// against a 60-folder path ran for minutes), so with more than one the
/// match goes through [`anchored_table`], which is O(segments × parts).
/// With at most one (nearly every real rule), trying every split is linear
/// and allocates nothing, which matters when every item is checked against
/// every rule.
fn anchored(segs: &[Seg], path: &[&str]) -> bool {
    if segs.iter().filter(|s| matches!(s, Seg::Deep)).count() <= 1 {
        anchored_one_deep(segs, path)
    } else {
        anchored_table(segs, path)
    }
}

/// [`anchored`] by trying every split at each `**`.
fn anchored_one_deep(segs: &[Seg], path: &[&str]) -> bool {
    match segs.split_first() {
        None => path.is_empty(),
        Some((Seg::Deep, rest)) => {
            let min = usize::from(rest.is_empty());
            (min..=path.len()).any(|skip| anchored_one_deep(rest, &path[skip..]))
        }
        Some((Seg::Name(g), rest)) => match path.split_first() {
            Some((first, more)) => g.matches(first) && anchored_one_deep(rest, more),
            None => false,
        },
    }
}

/// [`anchored`] in one pass per segment, from the last: `next[j]` says
/// whether the segments after the current one match `path[j..]`.
fn anchored_table(segs: &[Seg], path: &[&str]) -> bool {
    let n = path.len();
    // No segments left: only the empty rest of the path matches.
    let mut next: Vec<bool> = (0..=n).map(|j| j == n).collect();
    let mut any = vec![false; n + 2];
    for (i, seg) in segs.iter().enumerate().rev() {
        next = match seg {
            Seg::Deep => {
                // Skip at least `min` parts, then the rest must match:
                // `next[k]` for some `k >= j + min`, a suffix OR.
                let min = usize::from(i + 1 == segs.len());
                for j in (0..=n).rev() {
                    any[j] = next[j] || any[j + 1];
                }
                (0..=n).map(|j| any[j + min]).collect()
            }
            Seg::Name(g) => (0..=n).map(|j| j < n && next[j + 1] && g.matches(path[j])).collect(),
        };
    }
    next[0]
}

/// [`anchored`] of every prefix of a path (its parts `folded`), in one pass. `at[k]`: the first
/// `k` segments have matched the parts read so far. A `**` may match no
/// part (move on without reading one), except at the end, where it needs
/// at least one; reading a part keeps a `**` where it is.
fn anchored_prefixes(segs: &[Seg], folded: &[Vec<char>]) -> Vec<bool> {
    let last = segs.len();
    let close = |at: &mut Vec<bool>| {
        for k in 0..last.saturating_sub(1) {
            if at[k] && matches!(segs[k], Seg::Deep) {
                at[k + 1] = true;
            }
        }
    };
    let mut at = vec![false; last + 1];
    at[0] = true;
    close(&mut at);
    let mut out = Vec::with_capacity(folded.len());
    for part in folded {
        let mut next = vec![false; last + 1];
        for k in (0..last).filter(|&k| at[k]) {
            match &segs[k] {
                Seg::Deep => {
                    next[k] = true;
                    if k + 1 == last {
                        next[last] = true;
                    }
                }
                Seg::Name(g) => {
                    if g.matches_folded(part) {
                        next[k + 1] = true;
                    }
                }
            }
        }
        close(&mut next);
        out.push(next[last]);
        at = next;
    }
    out
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

    /// Rules come from `.oneupignore` files, which sync from OneDrive: no
    /// line may make matching slow, whatever its `**`s and however deep
    /// the path.
    #[test]
    fn many_double_stars_match_quickly() {
        let rule = r(&format!("{}x", "**/a/".repeat(12)));
        let deep = vec!["a"; 60];
        let t = std::time::Instant::now();
        assert!(!rule.matches(&deep, false));
        assert!(t.elapsed() < std::time::Duration::from_secs(1), "took {:?}", t.elapsed());
    }

    /// Both ways of matching `**` agree, on every rule and path built from
    /// a few parts.
    #[test]
    fn double_star_table_agrees_with_trying_every_split() {
        let parts = ["**", "a", "b", "*"];
        let mut rules = vec![Vec::new()];
        for _ in 0..4 {
            let longer: Vec<Vec<&str>> = rules
                .iter()
                .flat_map(|r: &Vec<&str>| parts.iter().map(move |p| [r.clone(), vec![*p]].concat()))
                .collect();
            rules.extend(longer);
        }
        rules.sort();
        rules.dedup();
        let mut paths = vec![Vec::new()];
        for _ in 0..5 {
            let longer: Vec<Vec<&str>> =
                paths.iter().flat_map(|p: &Vec<&str>| ["a", "b"].map(|x| [p.clone(), vec![x]].concat())).collect();
            paths.extend(longer);
        }
        paths.sort();
        paths.dedup();
        for rule in rules.iter().filter(|r| !r.is_empty()) {
            let segs: Vec<Seg> =
                rule.iter().map(|s| if *s == "**" { Seg::Deep } else { Seg::Name(NameGlob::new(s)) }).collect();
            for path in &paths {
                assert_eq!(anchored_table(&segs, path), anchored_one_deep(&segs, path), "{rule:?} vs {path:?}");
            }
        }
    }

    /// One pass over a path answers for each of its prefixes what matching
    /// that prefix alone answers, for every rule and path built from a few
    /// parts (names, wildcards and `**`, folders only or not).
    #[test]
    fn prefix_matches_agree_with_matching_each_prefix() {
        let parts = ["**", "a", "B", "*", "a*"];
        let mut bodies = vec![Vec::new()];
        for _ in 0..4 {
            let longer: Vec<Vec<&str>> = bodies
                .iter()
                .flat_map(|r: &Vec<&str>| parts.iter().map(move |p| [r.clone(), vec![*p]].concat()))
                .collect();
            bodies.extend(longer);
        }
        bodies.sort();
        bodies.dedup();
        let mut paths = vec![Vec::new()];
        for _ in 0..4 {
            let longer: Vec<Vec<&str>> = paths
                .iter()
                .flat_map(|p: &Vec<&str>| ["a", "b", "ab"].map(|x| [p.clone(), vec![x]].concat()))
                .collect();
            paths.extend(longer);
        }
        paths.retain(|p| !p.is_empty());
        for body in bodies.iter().filter(|b| !b.is_empty()) {
            let joined = body.join("/");
            for line in [joined.clone(), format!("{joined}/"), format!("/{joined}"), format!("!{joined}")] {
                let Some(rule) = Rule::parse(&line) else { continue };
                for path in &paths {
                    let folded: Vec<Vec<char>> = path.iter().map(|s| crate::glob::folded(s)).collect();
                    let all = rule.prefix_matches(&folded);
                    for (i, hit) in all.iter().enumerate() {
                        assert_eq!(*hit, rule.matches(&path[..=i], true), "{line:?} vs {:?}", &path[..=i]);
                    }
                }
            }
        }
    }
}
