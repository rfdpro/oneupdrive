//! Rule lines (`config.toml`) and `.oneupignore` files, which sync from
//! OneDrive and so are untrusted text: `oneup_rules` must parse any of it
//! without panicking and evaluate any rule against any path quickly. The
//! input is rule text, then a NUL, then one path per line to check.
//!
//! Also checked, because a wrong answer here skips or syncs the wrong
//! folders:
//! * a folder name [`RuleSet::pruning`] lets enumeration skip by name is
//!   excluded by the full rules wherever it appears;
//! * [`literal_path_rules`] matches exactly its path and what's inside it.
#![no_main]

use libfuzzer_sys::fuzz_target;
use oneup_rules::{ExcludeSet, Rule, RuleSet, literal_path_rules};

fuzz_target!(|data: &[u8]| {
    let Ok(text) = std::str::from_utf8(data) else { return };
    let (rules, paths) = text.split_once('\0').unwrap_or((text, "a/b"));
    let lines: Vec<&str> = rules.lines().collect();
    // Bound the work per input: libFuzzer's timeout should flag slow
    // matching, not sheer input size.
    if lines.len() > 32 || paths.lines().count() > 8 {
        return;
    }
    let paths: Vec<&str> = paths.lines().collect();

    for line in &lines {
        if let Some(rule) = Rule::parse(line) {
            for p in &paths {
                let parts: Vec<&str> = p.split('/').filter(|s| !s.is_empty()).collect();
                let _ = rule.matches(&parts, true);
                let _ = rule.matches(&parts, false);
            }
        }
    }

    let Ok(mut set) = RuleSet::new::<&str>(&[], &lines) else { return };
    set.add_ignore_file("sub", rules);
    set.set_paused(&paths[..paths.len().min(2)]);
    let _ = set.fingerprint();
    let _ = (set.only_paused(), set.upload_only());
    let excl = ExcludeSet::from_patterns(lines.iter().copied());
    let _ = (excl.fingerprint(), excl.patterns());

    let pruning = set.pruning();
    for p in &paths {
        for is_dir in [true, false] {
            let _ = set.verdict(p, is_dir);
            let _ = set.is_vault(p);
            let _ = set.is_onedrive_app(p, is_dir);
            if let Some((parent, name)) = p.rsplit_once('/') {
                let _ = set.excludes_child(parent, name, is_dir);
            }
        }
        let name = p.rsplit(['/', '\\']).next().unwrap_or_default();
        let _ = excl.excludes_folder(name);
        if pruning.excludes_folder(name) && !name.is_empty() && name != "." {
            assert!(set.excludes(p, true), "pruning skips {name:?} but the rules include {p:?}");
        }
    }

    // An item's own rules: they match it and everything inside it, and
    // nothing beside it. (A name with whitespace at its end can't be
    // written as a rule line, whose ends are trimmed; Windows strips such
    // names too.)
    for p in &paths {
        let parts: Vec<&str> = p.split(['/', '\\']).filter(|s| !s.is_empty() && *s != ".").collect();
        if parts.is_empty() || parts.iter().any(|s| s.trim_end() != *s) {
            continue;
        }
        let own = literal_path_rules(p);
        let Ok(only) = RuleSet::new::<String>(&[], &own) else { continue };
        let joined = parts.join("/");
        assert!(only.excludes(&joined, true) && only.excludes(&joined, false), "{own:?} vs {joined:?}");
        assert!(only.excludes(&format!("{joined}/x"), false), "{own:?} vs inside {joined:?}");
        assert!(!only.excludes(&format!("{joined}x"), true), "{own:?} vs beside {joined:?}");
    }
});
