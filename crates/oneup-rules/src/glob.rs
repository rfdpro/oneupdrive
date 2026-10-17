//! Wildcards within one name: `*` (any run of characters), `?` (one
//! character) and `[...]` (one of a set: `[abc]`, ranges `[a-z]`, negated
//! `[!a-z]` or `[^a-z]`). Compared case-insensitively, as Windows and
//! OneDrive compare names ([`oneup_sync::fold`]; ranges apply to the folded
//! characters). A name never contains a folder separator, so none of these
//! cross one.

#[derive(Debug, Clone, PartialEq, Eq)]
enum Tok {
    Char(char),
    Any,
    Star,
    Set { negated: bool, items: Vec<(char, char)> },
}

/// A compiled name pattern.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct NameGlob {
    toks: Vec<Tok>,
}

/// Folds one character the way names are compared ([`oneup_sync::fold`]).
pub(crate) fn fold(c: char) -> char {
    oneup_sync::fold_char(c)
}

/// `name` as patterns compare it ([`NameGlob::matches_folded`]).
pub fn folded(name: &str) -> Vec<char> {
    name.chars().map(fold).collect()
}

impl NameGlob {
    /// Compiles `pattern`. An unclosed `[` is taken literally, as git does.
    pub fn new(pattern: &str) -> Self {
        let chars: Vec<char> = pattern.chars().collect();
        let mut toks = Vec::new();
        let mut i = 0;
        while i < chars.len() {
            match chars[i] {
                '*' => {
                    // `**` within a name is the same as `*`.
                    if toks.last() != Some(&Tok::Star) {
                        toks.push(Tok::Star);
                    }
                    i += 1;
                }
                '?' => {
                    toks.push(Tok::Any);
                    i += 1;
                }
                '[' => match parse_set(&chars, i) {
                    Some((tok, next)) => {
                        toks.push(tok);
                        i = next;
                    }
                    None => {
                        toks.push(Tok::Char('['));
                        i += 1;
                    }
                },
                c => {
                    toks.push(Tok::Char(fold(c)));
                    i += 1;
                }
            }
        }
        Self { toks }
    }

    /// Whether the pattern has no wildcards: then it matches one name only.
    pub fn literal(&self) -> Option<String> {
        self.toks.iter().map(|t| if let Tok::Char(c) = t { Some(*c) } else { None }).collect()
    }

    /// Whether `name` matches. Iterative with one backtrack point per `*`
    /// (the classic linear-space wildcard algorithm), so no pattern can
    /// make it slow.
    pub fn matches(&self, name: &str) -> bool {
        self.matches_folded(&folded(name))
    }

    /// [`NameGlob::matches`] for a name already [`folded`]: checking one
    /// name against many patterns folds it once (planning checks every
    /// item against every rule; folding per pattern was most of the cost).
    pub fn matches_folded(&self, s: &[char]) -> bool {
        let (mut p, mut i) = (0usize, 0usize);
        let mut star: Option<(usize, usize)> = None;
        while i < s.len() {
            match self.toks.get(p) {
                Some(Tok::Star) => {
                    star = Some((p, i));
                    p += 1;
                }
                Some(t) if one(t, s[i]) => {
                    p += 1;
                    i += 1;
                }
                _ => match star {
                    Some((sp, si)) => {
                        p = sp + 1;
                        i = si + 1;
                        star = Some((sp, si + 1));
                    }
                    None => return false,
                },
            }
        }
        self.toks[p..].iter().all(|t| *t == Tok::Star)
    }
}

fn one(t: &Tok, c: char) -> bool {
    match t {
        Tok::Char(x) => *x == c,
        Tok::Any => true,
        Tok::Star => false,
        Tok::Set { negated, items } => items.iter().any(|&(lo, hi)| lo <= c && c <= hi) != *negated,
    }
}

/// Parses `[...]` starting at `start`; `None` if it isn't closed.
fn parse_set(chars: &[char], start: usize) -> Option<(Tok, usize)> {
    let mut i = start + 1;
    let negated = matches!(chars.get(i), Some('!' | '^'));
    if negated {
        i += 1;
    }
    let mut items = Vec::new();
    let mut first = true;
    loop {
        let c = *chars.get(i)?;
        // A `]` right after the opening (or the negation) is a member.
        if c == ']' && !first {
            return Some((Tok::Set { negated, items }, i + 1));
        }
        first = false;
        if chars.get(i + 1) == Some(&'-') && chars.get(i + 2).is_some_and(|&h| h != ']') {
            let (lo, hi) = (fold(c), fold(chars[i + 2]));
            items.push((lo.min(hi), lo.max(hi)));
            i += 3;
        } else {
            items.push((fold(c), fold(c)));
            i += 1;
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn m(p: &str, n: &str) -> bool {
        NameGlob::new(p).matches(n)
    }

    #[test]
    fn wildcards_match_within_a_name_without_case() {
        assert!(m("*.tmp", "Report.TMP"));
        assert!(m("*", ""));
        assert!(!m("*.tmp", "a.tmp.txt"));
        assert!(m("a*b*c", "aXXbYYc"));
        assert!(!m("a*b*c", "aXXbYY"));
        assert!(m("?.txt", "x.txt") && !m("?.txt", "xy.txt"));
        assert!(m("**", "anything"));
        assert!(m("Node_Modules", "node_modules"));
    }

    #[test]
    fn sets_ranges_and_negation() {
        assert!(m("[abc].md", "B.md") && !m("[abc].md", "d.md"));
        assert!(m("file[0-9]", "file7") && !m("file[0-9]", "filex"));
        assert!(m("[!0-9]x", "ax") && !m("[!0-9]x", "1x"));
        assert!(m("[^0-9]x", "ax"));
        assert!(m("[]]", "]"));
        assert!(m("[a-]", "-"));
        // Unclosed: literal.
        assert!(m("a[b", "a[b") && !m("a[b", "ab"));
    }

    #[test]
    fn literal_patterns_are_recognised() {
        assert_eq!(NameGlob::new("Node_Modules").literal().as_deref(), Some("NODE_MODULES"));
        assert_eq!(NameGlob::new("*.tmp").literal(), None);
    }

    #[test]
    fn names_compare_as_windows_compares_them() {
        assert!(m("λογος", "ΛΟΓΟΣ") && m("λογοσ", "ΛΟΓΟΣ") && m("*ς", "ΟΔΟΣ"));
        assert!(m("[σ]", "ς") && m("[α-ω]", "Σ"));
        assert!(!m("kit", "\u{212A}it"), "Kelvin sign");
        assert!(!m("ωmega", "\u{2126}mega"), "Ohm sign");
        assert!(!m("i*", "İx") && !m("İ*", "ix"));
        assert!(!m("strasse", "straße") && m("STRAßE", "straße"));
    }

    #[test]
    fn pathological_patterns_stay_fast() {
        let p = NameGlob::new(&"*a".repeat(50));
        assert!(!p.matches(&"a".repeat(49)));
        assert!(p.matches(&"a".repeat(60)));
    }
}
