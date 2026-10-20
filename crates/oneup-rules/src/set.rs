//! The user's whole rule set: presets, `config.toml` rules and
//! `.oneupignore` files, in precedence order, with git's semantics (see
//! `docs/architecture/rules.md`). Pure: callers load the texts.

use std::fmt;

use crate::ExcludeSet;
use crate::glob::fold;
use crate::pattern::Rule;
use crate::presets::{PRESETS, preset};

/// The name of the per-folder rule file.
pub const IGNORE_FILE: &str = ".oneupignore";

/// Where a rule came from.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Source {
    Preset(&'static str),
    /// The data folder's `config.toml`.
    Config,
    /// A `.oneupignore` in this folder (relative to the synced folder, `/`
    /// separated, empty for the synced folder itself).
    IgnoreFile(String),
    /// A folder the user paused (`oneup pause --folder`).
    Pause,
    /// A vault synced as its own pair: the pair holding its encrypted files
    /// leaves them alone.
    Vault,
    /// Left to the OneDrive app (for Office AutoSave in those folders).
    OneDriveApp,
}

impl fmt::Display for Source {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Source::Preset(n) => write!(f, "preset `{n}`"),
            Source::Config => f.write_str("config.toml"),
            Source::IgnoreFile(dir) if dir.is_empty() => write!(f, "{IGNORE_FILE}"),
            Source::IgnoreFile(dir) => write!(f, "{dir}/{IGNORE_FILE}"),
            Source::Pause => f.write_str("a paused folder (`oneup resume --folder`)"),
            Source::Vault => {
                f.write_str("a vault or folder pair synced on its own (`oneup vault list`, `oneup pair list`)")
            }
            Source::OneDriveApp => f.write_str("left to the OneDrive app (`oneup onedrive-app`)"),
        }
    }
}

/// The rule that decided.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Match {
    pub source: Source,
    /// 1-based line in the source (for presets and config: the rule's
    /// position in the list).
    pub line: usize,
    pub rule: String,
    pub negated: bool,
}

/// What the rules say about one item.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Verdict {
    /// Synced. `Some` if a `!` rule re-included it (or nothing matched).
    Included(Option<Match>),
    /// Excluded by this rule.
    Excluded(Match),
    /// Inside a folder that's excluded: nothing below one can be included.
    InsideExcluded { folder: String, by: Match },
}

impl Verdict {
    pub fn is_excluded(&self) -> bool {
        !matches!(self, Verdict::Included(_))
    }
}

#[derive(Debug, Clone)]
struct Layer {
    /// The rules' folder, folded, as parts (empty: the synced folder).
    base: Vec<String>,
    source: Source,
    rules: Vec<(usize, Rule)>,
}

impl Layer {
    fn new(base: &str, source: Source, lines: impl IntoIterator<Item = impl AsRef<str>>) -> Self {
        let rules =
            lines.into_iter().enumerate().filter_map(|(i, l)| Rule::parse(l.as_ref()).map(|r| (i + 1, r))).collect();
        Self { base: parts(base).map(|p| p.chars().map(fold).collect()).collect(), source, rules }
    }

    /// The part of `path` below this layer's folder, if `path` is inside it.
    fn relative<'p>(&self, path: &'p [&'p str]) -> Option<&'p [&'p str]> {
        if path.len() <= self.base.len() {
            return None;
        }
        let inside = self.base.iter().zip(path).all(|(b, p)| p.chars().map(fold).eq(b.chars()));
        inside.then(|| &path[self.base.len()..])
    }
}

/// Rules matching exactly the item at `path` (relative to the synced
/// folder) and everything inside it, names taken literally.
pub fn literal_path_rules(path: &str) -> Vec<String> {
    let p: Vec<String> = parts(path).map(literal).collect();
    if p.is_empty() {
        return Vec::new();
    }
    let at = format!("/{}", p.join("/"));
    vec![format!("{at}/**"), at]
}

/// A name as a pattern matching only itself.
fn literal(name: &str) -> String {
    name.chars()
        .map(|c| match c {
            // `!` and `#` only mean something at the start of a line, and
            // these lines start with `/`.
            '*' | '?' | '[' => format!("[{c}]"),
            c => c.to_string(),
        })
        .collect()
}

/// `a\b/c` → `a`, `b`, `c`.
fn parts(path: &str) -> impl Iterator<Item = &str> {
    path.split(['/', '\\']).filter(|p| !p.is_empty() && *p != ".")
}

/// An unknown preset name.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
#[error("no preset called `{0}` (there are: {list})", list = PRESETS.iter().map(|p| p.name).collect::<Vec<_>>().join(", "))]
pub struct UnknownPreset(pub String);

/// The rules, lowest precedence first.
#[derive(Debug, Clone, Default)]
pub struct RuleSet {
    presets: Vec<Layer>,
    config: Option<Layer>,
    /// Sorted by depth, then path, so a deeper file's rules come later.
    files: Vec<Layer>,
    /// Paused folders: last, so no `!` rule can resume one.
    pause: Option<Layer>,
    /// Vaults synced on their own: last too, for the same reason.
    vaults: Option<Layer>,
    /// Folders left to the OneDrive app: last as well.
    app: Option<Layer>,
    /// Files at the top of the synced folder are left to the OneDrive app
    /// too (it always syncs those, whatever folders it's set to).
    app_top_files: bool,
}

impl RuleSet {
    /// The presets turned on (any order; they apply in [`PRESETS`] order)
    /// and the `config.toml` rule lines.
    pub fn new<S: AsRef<str>>(presets: &[S], config: &[S]) -> Result<Self, UnknownPreset> {
        let mut on = Vec::new();
        for name in presets {
            let p = preset(name.as_ref()).ok_or_else(|| UnknownPreset(name.as_ref().to_string()))?;
            on.push(p.name);
        }
        let presets = PRESETS
            .iter()
            .filter(|p| on.contains(&p.name))
            .map(|p| Layer::new("", Source::Preset(p.name), p.lines.iter()))
            .collect();
        let config = (!config.is_empty()).then(|| Layer::new("", Source::Config, config.iter()));
        Ok(Self { presets, config, files: Vec::new(), pause: None, vaults: None, app: None, app_top_files: false })
    }

    /// Pauses these folders (paths relative to the synced folder): they
    /// read as excluded until resumed, whatever other rules say. Names are
    /// taken literally (no wildcards).
    pub fn set_paused<S: AsRef<str>>(&mut self, folders: &[S]) {
        let lines: Vec<String> = folders
            .iter()
            .map(|f| {
                let parts: Vec<String> = parts(f.as_ref()).map(literal).collect();
                format!("/{}/", parts.join("/"))
            })
            .filter(|l| l != "//")
            .collect();
        self.pause = (!lines.is_empty()).then(|| Layer::new("", Source::Pause, lines.iter()));
    }

    /// Leaves these folders (paths relative to the synced folder, taken
    /// literally) to the vaults synced on their own: they read as excluded,
    /// whatever other rules say.
    pub fn set_vaults<S: AsRef<str>>(&mut self, folders: &[S]) {
        let lines: Vec<String> = folders
            .iter()
            .map(|f| {
                let parts: Vec<String> = parts(f.as_ref()).map(literal).collect();
                format!("/{}/", parts.join("/"))
            })
            .filter(|l| l != "//")
            .collect();
        self.vaults = (!lines.is_empty()).then(|| Layer::new("", Source::Vault, lines.iter()));
    }

    /// Leaves these folders (relative to the synced folder, taken
    /// literally), and with `top_files` the files at the top of the synced
    /// folder, to the OneDrive app: they read as excluded, whatever other
    /// rules say, and nothing ever offers to remove them from OneDrive.
    pub fn set_onedrive_app<S: AsRef<str>>(&mut self, folders: &[S], top_files: bool) {
        let lines: Vec<String> = folders
            .iter()
            .map(|f| {
                let parts: Vec<String> = parts(f.as_ref()).map(literal).collect();
                format!("/{}/", parts.join("/"))
            })
            .filter(|l| l != "//")
            .collect();
        self.app = (!lines.is_empty()).then(|| Layer::new("", Source::OneDriveApp, lines.iter()));
        self.app_top_files = top_files;
    }

    /// Whether the item at `path` is left to the OneDrive app, or inside a
    /// folder that is ([`RuleSet::set_onedrive_app`]).
    pub fn is_onedrive_app(&self, path: &str, is_dir: bool) -> bool {
        let p: Vec<&str> = parts(path).collect();
        if self.app_top_files && p.len() == 1 && !is_dir {
            return true;
        }
        let Some(layer) = &self.app else { return false };
        (1..=p.len()).any(|i| {
            layer
                .relative(&p[..i])
                .is_some_and(|rel| layer.rules.iter().any(|(_, r)| !r.negated && r.matches(rel, true)))
        })
    }

    /// Whether the item at `path` (relative to the synced folder) is a vault
    /// synced on its own, or inside one ([`RuleSet::set_vaults`]). Such an
    /// item isn't junk the user excluded: it belongs to another pair, and
    /// nothing may ever offer to remove it from OneDrive.
    pub fn is_vault(&self, path: &str) -> bool {
        let Some(layer) = &self.vaults else { return false };
        let p: Vec<&str> = parts(path).collect();
        (1..=p.len()).any(|i| {
            layer
                .relative(&p[..i])
                .is_some_and(|rel| layer.rules.iter().any(|(_, r)| !r.negated && r.matches(rel, true)))
        })
    }

    /// Adds the `.oneupignore` of folder `dir` (relative to the synced
    /// folder) with its text.
    pub fn add_ignore_file(&mut self, dir: &str, text: &str) {
        let norm = parts(dir).collect::<Vec<_>>().join("/");
        self.files.push(Layer::new(&norm, Source::IgnoreFile(norm.clone()), text.lines()));
        self.files.sort_by(|a, b| (a.base.len(), &a.base).cmp(&(b.base.len(), &b.base)));
    }

    /// Only the paused folders of this set (and the vaults and folders left
    /// to the OneDrive app, which never sync either way).
    pub fn only_paused(&self) -> RuleSet {
        RuleSet {
            pause: self.pause.clone(),
            vaults: self.vaults.clone(),
            app: self.app.clone(),
            app_top_files: self.app_top_files,
            ..RuleSet::default()
        }
    }

    /// What planning hides when exclusions only stop uploads (the `pull`
    /// and `prune` settings): the paused folders ([`RuleSet::only_paused`]),
    /// and the folders listing skips by name ([`RuleSet::pruning`]), which
    /// neither side lists, so nothing in them can be brought here.
    /// Everything else excluded stays visible, to be brought here.
    pub fn upload_only(&self) -> RuleSet {
        let mut out = self.only_paused();
        let kept = self.pruned_rules();
        // As listing applies them: to folders only (`*.env` skips a folder
        // called `x.env`, never such a file).
        let keep = |layers: &[Layer]| -> Vec<Layer> {
            layers
                .iter()
                .map(|l| Layer {
                    rules: l
                        .rules
                        .iter()
                        .filter(|(_, r)| kept.iter().any(|k| std::ptr::eq(*k, r)))
                        .filter_map(|(line, r)| {
                            Rule::parse(&format!("{}/", r.text.trim_end_matches(['/', '\\']))).map(|r| (*line, r))
                        })
                        .collect(),
                    ..l.clone()
                })
                .filter(|l| !l.rules.is_empty())
                .collect()
        };
        out.presets = keep(&self.presets);
        out.config = keep(self.config.as_slice()).pop();
        out
    }

    /// Whether there are no rules at all.
    pub fn is_empty(&self) -> bool {
        self.layers().all(|l| l.rules.is_empty()) && !self.app_top_files
    }

    fn layers(&self) -> impl Iterator<Item = &Layer> {
        self.presets
            .iter()
            .chain(&self.config)
            .chain(&self.files)
            .chain(&self.pause)
            .chain(&self.vaults)
            .chain(&self.app)
    }

    /// The last rule matching the item itself (ancestors not considered).
    fn decide(&self, path: &[&str], is_dir: bool) -> Option<Match> {
        let mut last = None;
        // Every layer's `rel` ends with the item's own name.
        let name = crate::glob::folded(path.last().copied().unwrap_or_default());
        for layer in self.layers() {
            let Some(rel) = layer.relative(path) else { continue };
            for (line, rule) in &layer.rules {
                if rule.matches_folded_last(rel, &name, is_dir) {
                    last = Some(Match {
                        source: layer.source.clone(),
                        line: *line,
                        rule: rule.text.clone(),
                        negated: rule.negated,
                    });
                }
            }
        }
        if self.app_top_files && path.len() == 1 && !is_dir {
            last = Some(Match {
                source: Source::OneDriveApp,
                line: 0,
                rule: "files at the top of your OneDrive".into(),
                negated: false,
            });
        }
        last
    }

    /// What the rules say about the item at `path` (relative to the synced
    /// folder, `/` or `\` separated).
    pub fn verdict(&self, path: &str, is_dir: bool) -> Verdict {
        let p: Vec<&str> = parts(path).collect();
        if p.is_empty() {
            return Verdict::Included(None); // the synced folder itself
        }
        for i in 1..p.len() {
            if let Some(m) = self.decide(&p[..i], true).filter(|m| !m.negated) {
                return Verdict::InsideExcluded { folder: p[..i].join("/"), by: m };
            }
        }
        match self.decide(&p, is_dir) {
            Some(m) if !m.negated => Verdict::Excluded(m),
            m => Verdict::Included(m),
        }
    }

    /// Whether the folder `parent` (known *not* excluded, with everything
    /// above it) excludes its child `name`. What planning asks, walking down
    /// the tree, so ancestors aren't evaluated again for every item.
    pub fn excludes_child(&self, parent: &str, name: &str, is_dir: bool) -> bool {
        let mut p: Vec<&str> = parts(parent).collect();
        p.push(name);
        self.decide(&p, is_dir).is_some_and(|m| !m.negated)
    }

    /// Whether the item at `path` is excluded.
    pub fn excludes(&self, path: &str, is_dir: bool) -> bool {
        self.verdict(path, is_dir).is_excluded()
    }

    /// The folder rules enumeration may apply by name alone, wherever a
    /// folder is (`docs/architecture/rules.md` § Where rules apply): the
    /// unanchored, non-negated rules of the presets and `config.toml`.
    /// A name that any `!` rule (in any source) might re-include is left
    /// out, and with any `!` rule at all, wildcard names are left out too.
    pub fn pruning(&self) -> ExcludeSet {
        let patterns: Vec<String> =
            self.pruned_rules().iter().map(|r| r.text.trim_end_matches(['/', '\\']).to_string()).collect();
        ExcludeSet::from_patterns(patterns.iter().map(String::as_str))
    }

    /// The rules [`RuleSet::pruning`] is made of.
    fn pruned_rules(&self) -> Vec<&Rule> {
        let negations: Vec<&Rule> =
            self.layers().flat_map(|l| l.rules.iter().map(|(_, r)| r)).filter(|r| r.negated).collect();
        let mut out = Vec::new();
        for layer in self.presets.iter().chain(&self.config) {
            for (_, rule) in &layer.rules {
                let Some(g) = rule.name_glob() else { continue };
                let keep = match g.literal() {
                    Some(name) => !negations.iter().any(|n| n.could_match_name(&name)),
                    None => negations.is_empty(),
                };
                if keep {
                    out.push(rule);
                }
            }
        }
        out
    }

    /// Whether a rule meant for somewhere inside the folder at `folder`
    /// (relative to the synced folder) excludes something there: an
    /// anchored rule of yours below it (`/Docs/drafts/` for `Docs`), a
    /// `.oneupignore` in it or below, a paused folder or one synced on its
    /// own below it. What the file tree shows as a partly ticked folder.
    /// Rules that apply everywhere (presets, `*.iso`) don't count: they
    /// would mark every folder. Pure; a pass over the rules, not the tree.
    pub fn has_rules_inside(&self, folder: &str) -> bool {
        let want: Vec<Vec<char>> = parts(folder).map(crate::glob::folded).collect();
        let covers =
            |at: &[String]| at.len() >= want.len() && want.iter().zip(at).all(|(w, a)| a.chars().eq(w.iter().copied()));
        let everywhere = |l: &Layer| matches!(l.source, Source::Preset(_) | Source::Config);
        self.layers().any(|l| {
            l.rules.iter().any(|(_, r)| {
                if r.negated {
                    return false;
                }
                let inside = match r.within() {
                    Some(within) => within,
                    None if everywhere(l) => return false,
                    None => Vec::new(),
                };
                let at: Vec<String> = l.base.iter().cloned().chain(inside).collect();
                covers(&at)
            })
        })
    }

    /// Changes whenever any rule changes, so stored decisions can be
    /// re-evaluated (the planning fingerprint of rules.md).
    pub fn fingerprint(&self) -> String {
        let mut h = Fnv::default();
        for layer in self.layers() {
            h.write(layer.source.to_string().as_bytes());
            for (line, rule) in &layer.rules {
                h.write(&line.to_le_bytes());
                h.write(rule.text.as_bytes());
            }
            h.write(&[0xff]);
        }
        if self.app_top_files {
            h.write(b"app top files");
        }
        format!("r1:{:016x}", h.0)
    }

    /// The `.oneupignore` folders loaded, for `explain` and status.
    pub fn ignore_files(&self) -> impl Iterator<Item = &Source> {
        self.files.iter().map(|l| &l.source)
    }
}

/// FNV-1a, 64-bit: a stable fingerprint, not a security measure.
struct Fnv(u64);

impl Default for Fnv {
    fn default() -> Self {
        Self(0xcbf2_9ce4_8422_2325)
    }
}

impl Fnv {
    fn write(&mut self, bytes: &[u8]) {
        for b in bytes {
            self.0 ^= u64::from(*b);
            self.0 = self.0.wrapping_mul(0x0000_0100_0000_01b3);
        }
    }
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used)]
    use super::*;

    fn set(presets: &[&str], config: &[&str]) -> RuleSet {
        RuleSet::new(presets, config).unwrap()
    }

    #[test]
    fn presets_are_off_unless_chosen() {
        let none = set(&[], &[]);
        assert!(!none.excludes("app/node_modules", true));
        let dev = set(&["dev"], &[]);
        assert!(dev.excludes("app/node_modules", true));
        assert!(matches!(RuleSet::new(&["everything"], &[]), Err(UnknownPreset(_))));
    }

    #[test]
    fn the_last_match_wins_and_deeper_files_come_last() {
        let mut s = set(&[], &["*.log"]);
        assert!(s.excludes("a/x.log", false));
        s.add_ignore_file("a", "!keep.log");
        assert!(!s.excludes("a/keep.log", false));
        assert!(s.excludes("a/other.log", false));
        assert!(s.excludes("b/keep.log", false), "a's file applies only inside a");
        s.add_ignore_file("", "!*.log\n");
        assert!(!s.excludes("b/x.log", false), "the root file comes after config");
        assert!(!s.excludes("a/keep.log", false));
    }

    #[test]
    fn nothing_inside_an_excluded_folder_can_be_re_included() {
        let mut s = set(&["dev"], &[]);
        s.add_ignore_file("app", "!node_modules/keep.js");
        match s.verdict("app/node_modules/keep.js", false) {
            Verdict::InsideExcluded { folder, by } => {
                assert_eq!(folder, "app/node_modules");
                assert_eq!(by.source, Source::Preset("dev"));
                assert_eq!(by.rule, "node_modules/");
            }
            v => panic!("{v:?}"),
        }
        // Re-including the folder itself works.
        s.add_ignore_file("app", "!node_modules/");
        assert!(!s.excludes("app/node_modules/keep.js", false));
    }

    #[test]
    fn explain_names_the_source_and_line() {
        let mut s = set(&[], &["# media", "*.iso"]);
        assert_eq!(
            s.verdict("x/disk.ISO", false),
            Verdict::Excluded(Match { source: Source::Config, line: 2, rule: "*.iso".into(), negated: false })
        );
        s.add_ignore_file(r"x\y", "\n!disk.iso\n");
        match s.verdict("x/y/disk.iso", false) {
            Verdict::Included(Some(m)) => {
                assert_eq!((m.source.to_string().as_str(), m.line), ("x/y/.oneupignore", 2));
            }
            v => panic!("{v:?}"),
        }
        assert_eq!(s.verdict("", true), Verdict::Included(None));
    }

    #[test]
    fn folder_case_is_ignored_for_rule_files() {
        let mut s = set(&[], &[]);
        s.add_ignore_file("Docs", "drafts/");
        assert!(s.excludes("docs/DRAFTS", true));
        assert!(s.excludes_child("DOCS", "drafts", true));
        assert!(!s.excludes_child("DOCS", "drafts", false));
    }

    #[test]
    fn pruning_keeps_dev_compatible_and_respects_negations() {
        // The dev preset prunes exactly what the built-in list did: same fingerprint,
        // so existing data folders don't rescan.
        assert_eq!(set(&["dev"], &[]).pruning().fingerprint(), ExcludeSet::dev().fingerprint());
        assert_eq!(set(&[], &[]).pruning().fingerprint(), ExcludeSet::none().fingerprint());
        // Anchored rules and .oneupignore files never prune.
        let mut s = set(&[], &["/Downloads/", "*.egg-info/"]);
        s.add_ignore_file("a", "build/");
        let p = s.pruning();
        assert!(p.excludes_folder("pkg.egg-info") && !p.excludes_folder("Downloads") && !p.excludes_folder("build"));
        // A `!` anywhere that might re-include a name keeps it from pruning.
        s.add_ignore_file("app", "!target/");
        let mut d = set(&["dev"], &[]);
        d.add_ignore_file("app", "!target/");
        let p = d.pruning();
        assert!(!p.excludes_folder("target") && p.excludes_folder("node_modules"));
        assert!(!s.pruning().excludes_folder("pkg.egg-info"), "wildcards: none with any `!` rule");
    }

    #[test]
    fn paused_folders_win_over_everything_and_never_prune() {
        let mut s = set(&["dev"], &[]);
        s.add_ignore_file("", "!videos/");
        s.set_paused(&["Videos", "odd [1]*", "!x #y"]);
        match s.verdict("videos/a.mp4", false) {
            Verdict::InsideExcluded { folder, by } => {
                assert_eq!((folder.as_str(), by.source), ("videos", Source::Pause));
            }
            v => panic!("{v:?}"),
        }
        assert!(s.excludes("odd [1]*/x", false), "taken literally");
        assert!(!s.excludes("odd 1x/x", false), "not a wildcard");
        assert!(s.excludes("!x #y/z", false));
        assert!(!s.excludes("other/videos/x", false), "anchored at the top");
        assert_eq!(s.pruning().fingerprint(), ExcludeSet::dev().fingerprint());
        assert!(s.upload_only().excludes("Videos/a.mp4", false), "paused: hidden both ways");
        let before = s.fingerprint();
        s.set_paused::<&str>(&[]);
        assert_ne!(s.fingerprint(), before, "resuming re-plans");
    }

    /// With exclusions that only stop uploads, planning still hides what
    /// listing skips by name (nothing in it is listed on either side), and
    /// nothing else.
    #[test]
    fn upload_only_hides_the_folders_skipped_by_name_and_nothing_else() {
        let s = set(&["dev"], &["*.env", "/Downloads/", "build/"]);
        let u = s.upload_only();
        assert!(u.excludes("app/node_modules/x.js", false) && u.excludes("build", true));
        assert!(!u.excludes("app/.env", false) && !u.excludes("Downloads/a.iso", false));
        assert!(u.excludes("app/x.env/a", false), "a folder listing skips by name");
        assert_eq!(u.pruning().fingerprint(), s.pruning().fingerprint());
        // A name a `!` rule might bring back isn't skipped, so not hidden.
        let mut n = set(&["dev"], &[]);
        n.add_ignore_file("app", "!node_modules/");
        assert!(!n.upload_only().excludes("app/node_modules/x.js", false));
        assert!(n.upload_only().excludes("app/.git/x", false));
    }

    #[test]
    fn folders_left_to_the_onedrive_app_win_over_everything() {
        let mut s = RuleSet::new(&["dev"], &["!Office/"]).unwrap();
        let before = s.fingerprint();
        s.set_onedrive_app(&["Office"], true);
        assert_ne!(s.fingerprint(), before);
        assert!(s.excludes("Office/a.docx", false) && s.excludes("Office", true));
        assert!(s.excludes("top.docx", false), "files at the top go with it");
        assert!(!s.excludes("Docs/a.docx", false) && !s.excludes("Docs", true));
        assert!(s.is_onedrive_app("Office/x", false) && s.is_onedrive_app("top.txt", false));
        assert!(!s.is_onedrive_app("Docs", true) && !s.is_onedrive_app("Docs/top.txt", false));
        match s.verdict("Office/a.docx", false) {
            Verdict::InsideExcluded { by, .. } => assert_eq!(by.source, Source::OneDriveApp),
            v => panic!("{v:?}"),
        }
        assert!(s.only_paused().excludes("Office/a", false), "pull mode keeps them out too");
    }

    #[test]
    fn vaults_synced_on_their_own_win_over_everything_and_never_prune() {
        let mut s = set(&["dev"], &[]);
        s.add_ignore_file("Docs", "!My Vault/");
        s.set_vaults(&["Docs/My Vault"]);
        match s.verdict("Docs/My Vault/d/AB/x.c9r", false) {
            Verdict::InsideExcluded { folder, by } => {
                assert_eq!((folder.as_str(), by.source), ("Docs/My Vault", Source::Vault));
            }
            v => panic!("{v:?}"),
        }
        assert!(!s.excludes("Docs/My Vaults/x", false));
        assert!(s.only_paused().excludes("Docs/My Vault/x", false), "pull mode keeps them out too");
        assert!(s.is_vault("Docs/My Vault") && s.is_vault("Docs/My Vault/d/AB/x.c9r"));
        assert!(!s.is_vault("Docs") && !s.is_vault("Docs/My Vaults/x") && !s.is_vault("node_modules"));
        assert_eq!(s.pruning().fingerprint(), ExcludeSet::dev().fingerprint(), "still listed in the mirror");
    }

    #[test]
    fn literal_path_rules_match_the_item_and_its_contents_only() {
        let s = set(&[], &literal_path_rules(r"docs\[draft] *").iter().map(String::as_str).collect::<Vec<_>>());
        assert!(s.excludes("docs/[draft] *", true) && s.excludes("docs/[draft] */a/b.txt", false));
        assert!(!s.excludes("docs/d *", true) && !s.excludes("other/docs/[draft] *", true));
        assert!(literal_path_rules("/").is_empty());
    }

    #[test]
    fn listing_and_planning_fold_names_alike() {
        // Listing never skips more than planning excludes: both decide the
        // same for every name, Greek final sigma and look-alike signs too.
        let s = set(&[], &["λογος/", "kit/", "ωmega/", "i*/", "straße/"]);
        let pruning = s.pruning();
        for name in
            ["ΛΟΓΟΣ", "λογοσ", "Λογος", "\u{212A}it", "KIT", "\u{2126}mega", "Ωmega", "İx", "Ix", "STRASSE", "STRAßE"]
        {
            assert_eq!(pruning.excludes_folder(name), s.excludes_child("", name, true), "{name}");
        }
        assert!(pruning.excludes_folder("ΛΟΓΟΣ") && !pruning.excludes_folder("\u{212A}it"));
        assert!(!pruning.excludes_folder("İx") && !pruning.excludes_folder("STRASSE"));
    }

    #[test]
    fn rules_meant_for_inside_a_folder_mark_it_and_its_parents() {
        let mut s = set(&["dev"], &["*.iso", "/Docs/Drafts/", "/Media/*.mov", "!/Keep/x/"]);
        s.add_ignore_file("Code/app", "build/");
        s.set_paused(&["Photos/2019"]);
        for (folder, inside) in [
            ("", true),
            ("docs", true),
            ("Docs/Drafts", false),
            ("Media", true),
            ("Code", true),
            ("Code/App", true),
            ("Code/App/src", false),
            ("Photos", true),
            ("Keep", false),
            ("Music", false),
        ] {
            assert_eq!(s.has_rules_inside(folder), inside, "{folder:?}");
        }
        assert!(!set(&["dev"], &["*.iso"]).has_rules_inside(""), "rules for everywhere don't count");
    }

    #[test]
    fn fingerprints_change_with_any_rule() {
        let a = set(&["dev"], &["*.iso"]);
        let mut b = a.clone();
        assert_eq!(a.fingerprint(), b.fingerprint());
        b.add_ignore_file("x", "y");
        assert_ne!(a.fingerprint(), b.fingerprint());
        assert_ne!(set(&["dev"], &[]).fingerprint(), set(&["temp"], &[]).fingerprint());
    }
}
