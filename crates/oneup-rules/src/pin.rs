//! Pin rules (`config.toml` `[rules] pin`): what is always kept on this PC
//! with Files On-Demand ("Always keep on this device"), whatever the space
//! policies would free. Gitignore syntax and the same pattern matcher as
//! the exclusion rules, anchored at the synced folder, but their own
//! precedence: **the last rule matching the item or any folder above it
//! wins**. So a rule on a folder pins everything in it (as Windows has a
//! placeholder created in a pinned folder inherit its pin), and a later
//! `!` rule takes out part of it: `/Work/` then `!/Work/archive/*.zip`
//! keeps `Work` here except those archives. (Exclusion rules can't do that:
//! nothing inside an excluded folder can be re-included, which is what
//! lets listing skip such folders. Pins skip no listing.)
//! Pure: callers load the lines.

use crate::pattern::Rule;

/// The pin rules.
#[derive(Debug, Clone, Default)]
pub struct PinSet {
    lines: Vec<String>,
    /// Parsed, with their text.
    rules: Vec<Rule>,
}

impl PinSet {
    pub fn new<S: AsRef<str>>(lines: &[S]) -> Self {
        let lines: Vec<String> = lines.iter().map(|l| l.as_ref().trim().to_string()).collect();
        let rules = lines.iter().filter_map(|l| Rule::parse(l)).collect();
        Self { lines, rules }
    }

    /// The rule that keeps the item at `path` (relative to the synced
    /// folder) on this PC, if one does: the rule's text. `None` also when
    /// the last rule that matches is a `!` rule.
    pub fn pinned_by(&self, path: &str, is_dir: bool) -> Option<String> {
        if self.rules.is_empty() {
            return None;
        }
        let parts: Vec<&str> = path.split(['/', '\\']).filter(|p| !p.is_empty() && *p != ".").collect();
        if parts.is_empty() {
            return None;
        }
        // The last rule (in order) matching the item, or a folder above it.
        let last = self
            .rules
            .iter()
            .rev()
            .find(|r| (1..=parts.len()).any(|n| r.matches(&parts[..n], n < parts.len() || is_dir)))?;
        (!last.negated).then(|| last.text.clone())
    }

    pub fn pins(&self, path: &str, is_dir: bool) -> bool {
        self.pinned_by(path, is_dir).is_some()
    }

    pub fn is_empty(&self) -> bool {
        !self.rules.iter().any(|r| !r.negated)
    }

    pub fn lines(&self) -> &[String] {
        &self.lines
    }

    /// Changes whenever a pin rule changes, so the agent applies the pins
    /// again. `p2`: how pins match (`p1` stopped at a pinned folder, so a
    /// `!` rule inside it had no effect); a change there must change the
    /// version so stored pins are applied again.
    pub fn fingerprint(&self) -> String {
        let mut h: u64 = 0xcbf2_9ce4_8422_2325;
        for r in &self.rules {
            for b in r.text.bytes().chain([0xff]) {
                h ^= u64::from(b);
                h = h.wrapping_mul(0x0000_0100_0000_01b3);
            }
        }
        format!("p2:{h:016x}")
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn pins_match_like_exclusions_and_a_pinned_folder_pins_its_contents() {
        let p = PinSet::new(&["*.psd", "/Work/", "!/art/huge.psd"]);
        assert_eq!(p.pinned_by("art/x.PSD", false).as_deref(), Some("*.psd"));
        assert!(!p.pins("art/huge.psd", false), "un-pinned by `!`");
        assert!(p.pins("Work", true) && p.pins("work/a/b.txt", false));
        assert!(!p.pins("Other/Work/x", false), "anchored");
        assert!(!p.pins("notes.txt", false));
        assert!(PinSet::new::<&str>(&[]).is_empty() && !PinSet::new::<&str>(&[]).pins("x", false));
        assert!(PinSet::new(&["!x"]).is_empty(), "only `!` rules pin nothing");
    }

    /// A later `!` rule takes part of a pinned folder out: the last rule
    /// matching the item or a folder above it wins, whichever it is.
    #[test]
    fn a_later_negation_unpins_part_of_a_pinned_folder() {
        let p = PinSet::new(&["/Work/", "!/Work/archive/*.zip"]);
        assert!(!p.pins("Work/archive/old.zip", false), "taken out");
        assert!(p.pins("Work/archive/notes.txt", false) && p.pins("Work/archive", true));
        assert!(p.pins("Work/a.zip", false), "only in archive");
        // Folder-wide negation: everything inside it too.
        let p = PinSet::new(&["/Work/", "!/Work/cache/"]);
        assert!(!p.pins("Work/cache", true) && !p.pins("Work/cache/deep/x.bin", false));
        assert!(p.pins("Work/x.bin", false));
        // Order matters: a pin after the negation wins again.
        let p = PinSet::new(&["!/Work/cache/", "/Work/"]);
        assert!(p.pins("Work/cache/x.bin", false));
        // A file rule matching a folder above doesn't pin it as a folder.
        let p = PinSet::new(&["drafts/"]);
        assert!(p.pins("a/drafts/x.txt", false) && !p.pins("a/drafts", false));
    }

    #[test]
    fn the_fingerprint_follows_the_rules() {
        let a = PinSet::new(&["*.psd"]);
        assert_eq!(a.fingerprint(), PinSet::new(&[" *.psd ", "# note"]).fingerprint());
        assert_ne!(a.fingerprint(), PinSet::new(&["*.psb"]).fingerprint());
        assert_ne!(a.fingerprint(), PinSet::new::<&str>(&[]).fingerprint());
        assert!(a.fingerprint().starts_with("p2:"));
    }
}
