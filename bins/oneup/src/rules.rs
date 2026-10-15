//! `oneup rules` (show and change the rules in `config.toml`) and
//! `oneup explain` (why a file or folder does or doesn't sync).
//! See `docs/architecture/rules.md` and `docs/user-guide/excluding-files.md`.

use std::path::Path;

use anyhow::{Context, Result, bail};
use oneup_app::config::{self, Rules};
use oneup_app::rulefiles;
use oneup_plan::names;
use oneup_rules::{ENGINE_DIR, PRESETS, Source, Verdict, preset};
use oneup_store::{LocalKind, LocalRow, Skip, Store};

/// The changes asked for on the command line.
#[derive(Debug, Default)]
pub struct Changes {
    /// `keep`, `prune` or `pull` (`[rules] cloud_copies`).
    pub cloud: Option<String>,
    pub on: Vec<String>,
    pub off: Vec<String>,
    pub add: Vec<String>,
    pub remove: Vec<String>,
}

impl Changes {
    fn is_empty(&self) -> bool {
        self.cloud.is_none()
            && self.on.is_empty()
            && self.off.is_empty()
            && self.add.is_empty()
            && self.remove.is_empty()
    }
}

/// `oneup rules [--on P] [--off P] [--add R] [--remove R]`.
pub async fn run(data_dir: &Path, store: &Store, changes: &Changes) -> Result<()> {
    let mut rules = config::load_rules(data_dir, store)?;
    if !changes.is_empty() {
        let before = rules.clone();
        apply(&mut rules, changes)?;
        if rules == before {
            println!("Nothing to change.");
        } else {
            config::save_rules(data_dir, &rules)?;
            println!("Saved in {}.", config::path(data_dir).display());
            if crate::agent::nudge(data_dir).await {
                println!("The background agent applies them now.");
            }
        }
    }
    show(&rules)
}

fn apply(rules: &mut Rules, c: &Changes) -> Result<()> {
    if let Some(m) = &c.cloud {
        rules.cloud = config::CloudCopies::from_name(m)
            .with_context(|| format!("`{m}`: OneDrive's copies of excluded items can be keep, prune or pull"))?;
    }
    for name in &c.on {
        let p = preset(name).with_context(|| format!("no preset called `{name}` (see `oneup rules`)"))?;
        if !rules.presets.iter().any(|x| x.eq_ignore_ascii_case(p.name)) {
            rules.presets.push(p.name.to_string());
        }
    }
    for name in &c.off {
        rules.presets.retain(|x| !x.eq_ignore_ascii_case(name));
    }
    for r in &c.add {
        config::check_rule(r)?;
        if !rules.exclude.contains(r) {
            rules.exclude.push(r.clone());
        }
    }
    for r in &c.remove {
        let n = rules.exclude.len();
        rules.exclude.retain(|x| x != r);
        if rules.exclude.len() == n {
            bail!("`{r}` isn't one of your rules (`oneup rules` lists them)");
        }
    }
    Ok(())
}

fn show(rules: &Rules) -> Result<()> {
    println!("Presets (none is on unless you turn it on: oneup rules --on NAME / --off NAME):");
    for p in PRESETS {
        let on = rules.presets.iter().any(|x| x.eq_ignore_ascii_case(p.name));
        println!("  {} {:<8} {}", if on { "[on] " } else { "[off]" }, p.name, p.summary);
        println!("        {}", p.lines.join("  "));
    }
    println!("Your rules (config.toml; oneup rules --add RULE / --remove RULE):");
    if rules.exclude.is_empty() {
        println!("  none");
    }
    for r in &rules.exclude {
        println!("  {r}");
    }
    println!("Rule files: a .oneupignore in any synced folder adds rules for that folder and below.");
    println!(
        "OneDrive's copies of excluded items: {} (oneup rules --cloud keep|prune|pull)",
        match rules.cloud {
            config::CloudCopies::Keep =>
                "keep: left alone, not brought here either; re-including an item syncs it again from where it was",
            config::CloudCopies::Prune =>
                "prune: brought here, then removed from OneDrive (to its recycle bin) automatically; copies here stay",
            config::CloudCopies::Pull =>
                "pull (the default): brought here from OneDrive, never sent; a change here first becomes a conflict",
        }
    );

    let pruned = rules.rule_set()?.pruning().patterns();
    if !pruned.is_empty() {
        println!("Folders not even looked inside (what makes them cheap): {}", pruned.join(", "));
    }
    Ok(())
}

/// `oneup priority [--first RULE] [--last RULE] [--remove RULE]`.
pub async fn priority(data_dir: &Path, first: &[String], last: &[String], remove: &[String]) -> Result<()> {
    let (mut f, mut l) = config::load_priority(data_dir)?;
    let (before_f, before_l) = (f.clone(), l.clone());
    for r in first.iter().chain(last) {
        config::check_rule(r)?;
    }
    for r in remove {
        if !f.contains(r) && !l.contains(r) {
            bail!("`{r}` isn't one of the priority rules (`oneup priority` lists them)");
        }
        f.retain(|x| x != r);
        l.retain(|x| x != r);
    }
    for r in first {
        l.retain(|x| x != r);
        if !f.contains(r) {
            f.push(r.clone());
        }
    }
    for r in last {
        f.retain(|x| x != r);
        if !l.contains(r) {
            l.push(r.clone());
        }
    }
    if (f.clone(), l.clone()) != (before_f, before_l) {
        config::save_priority(data_dir, &f, &l)?;
        println!("Saved in {}.", config::path(data_dir).display());
        if crate::agent::nudge(data_dir).await {
            println!("The background agent uses it from its next round.");
        }
    }
    println!("Sent or fetched first (oneup priority --first RULE):");
    for r in &f {
        println!("  {r}");
    }
    if f.is_empty() {
        println!("  nothing in particular");
    }
    println!("Sent or fetched last (oneup priority --last RULE):");
    for r in &l {
        println!("  {r}");
    }
    if l.is_empty() {
        println!("  nothing in particular");
    }
    Ok(())
}

/// `oneup pin PATH` ("Always keep on this device", `keep`) / `oneup free
/// PATH` ("Free up space"): sets the pin state, of a folder and everything
/// in it too, as File Explorer does; the background agent downloads or
/// frees accordingly.
pub async fn pin(data_dir: &Path, store: &Store, path: &str, keep: bool) -> Result<()> {
    use oneup_cfapi::provider;
    let pair = oneup_app::sync::pairing(store)?;
    let token = store.read(|o| o.meta_get("sync.token"))?.unwrap_or_default();
    if !oneup_app::ondemand::is_on(&pair.local, &token) {
        bail!("Files On-Demand is off for this folder: every file is kept on this PC already (`oneup on-demand on`)");
    }
    let rel = crate::activity::normalize(&crate::activity::within(&pair.local, path)?);
    let full =
        rel.trim_start_matches('/').split('/').filter(|p| !p.is_empty()).fold(pair.local.clone(), |p, x| p.join(x));
    let f = provider::open(&full).with_context(|| format!("{} isn't in the synced folder", full.display()))?;
    let pin = if keep { provider::Pin::Pinned } else { provider::Pin::Unpinned };
    if full.is_dir() { provider::set_pin_tree(&f, pin) } else { provider::set_pin(&f, pin) }
        .with_context(|| format!("{rel} isn't an online-only-capable item"))?;
    if keep {
        println!("{rel}: always kept on this device; the background agent downloads what isn't here yet.");
    } else {
        println!("{rel}: online-only; the background agent frees the space (what's in OneDrive stays there).");
    }
    if !crate::agent::nudge(data_dir).await {
        println!("(The background agent isn't running; it does this when it starts: `oneup start`.)");
    }
    Ok(())
}

/// `oneup pause --folder PATH` (`pause`) / `oneup resume --folder PATH`.
pub async fn pause_folder(data_dir: &Path, store: &Store, path: &str, pause: bool) -> Result<()> {
    let pair = oneup_app::sync::pairing(store)?;
    let rel = crate::activity::normalize(&crate::activity::within(&pair.local, path)?);
    let rel = rel.trim_start_matches('/').to_string();
    if rel.is_empty() {
        bail!("that's the whole synced folder: use `oneup pause` without --folder");
    }
    let mut rules = config::load_rules(data_dir, store)?;
    let same = |p: &String| p.trim_matches(['/', '\\']).replace('\\', "/").eq_ignore_ascii_case(&rel);
    let had = rules.paused.iter().any(same);
    if pause && !had {
        rules.paused.push(rel.clone());
    } else if !pause && had {
        rules.paused.retain(|p| !same(p));
    } else {
        println!("/{rel} {}.", if pause { "is already paused" } else { "wasn't paused" });
        return Ok(());
    }
    config::save_rules(data_dir, &rules)?;
    if pause {
        println!("Paused /{rel}: changes there wait until `oneup resume --folder` (nothing is deleted).");
    } else {
        println!("Resumed /{rel}: what changed meanwhile syncs now.");
    }
    if crate::agent::nudge(data_dir).await {
        println!("The background agent applies it now.");
    }
    Ok(())
}

/// `oneup explain PATH`. Brings the Local tree up to date first, unless
/// the agent is running (then it keeps the tree current itself).
pub async fn explain(data_dir: &Path, store: Store, path: &str) -> Result<()> {
    let store = match oneup_app::setup::DirLock::acquire(data_dir) {
        Ok(_lock) => {
            let pruning = config::load_rules(data_dir, &store)?.rule_set()?.pruning();
            oneup_app::sync::observe(store, &pruning).await?
        }
        Err(_) => store,
    };
    let store = &store;
    let pair = oneup_app::sync::pairing(store)?;
    let rel = crate::activity::normalize(&crate::activity::within(&pair.local, path)?);
    let rel = rel.trim_start_matches('/');
    let shown = if rel.is_empty() { "the synced folder".to_string() } else { format!("/{rel}") };
    let parts: Vec<&str> = rel.split('/').filter(|p| !p.is_empty()).collect();

    // Walk the Local tree down the path, collecting the rule files on the
    // way and noting a folder that listing skipped.
    let root = store.read(|o| o.local_root())?.context("no local scan")?;
    let mut set = config::load_rules(data_dir, store)?.rule_set()?;
    rulefiles::add_separate(data_dir, store, &mut set)?;
    rulefiles::add_onedrive_app(data_dir, store, &mut set)?;
    let mut verifier = oneup_local::verify::Verifier::open(&root, 64 * 1024)?;
    let mut problems = Vec::new();
    let mut row: Option<LocalRow> = store.read(|o| o.local_get(root.root_file_id))?;
    let mut skipped_at: Option<String> = None;
    for (i, part) in parts.iter().enumerate() {
        let Some(dir) = row.clone() else { break };
        if dir.kind != LocalKind::Folder && dir.kind != LocalKind::Root {
            row = None;
            break;
        }
        if dir.skip == Skip::Excluded {
            skipped_at = Some(parts[..i].join("/"));
            row = None;
            break;
        }
        let dir_rel = parts[..i].join("/");
        if let Some(f) = store.read(|o| rulefiles::find(o, dir.file_id))? {
            if let Err(p) = rulefiles::add(&mut set, &mut verifier, &dir_rel, &f) {
                problems.push(format!("{} ignored: {p}", Path::new(&dir_rel).join(".oneupignore").display()));
            }
        }
        row = store.read(|o| o.local_children(dir.file_id))?.into_iter().find(|c| c.name.eq_ignore_ascii_case(part));
    }
    if row.as_ref().is_some_and(|r| r.skip == Skip::Excluded) && skipped_at.is_none() && !parts.is_empty() {
        skipped_at = Some(rel.to_string());
    }
    let is_dir = match &row {
        Some(r) => r.kind != LocalKind::File,
        // Not here: a folder if this PC or OneDrive has one there.
        None => {
            pair.local.join(rel).is_dir()
                || oneup_app::sync::find_remote(store, &format!("{}/{rel}", pair.remote.trim_end_matches('/')))?
                    .is_some()
        }
    };
    let name = parts.last().copied().unwrap_or_default();

    println!("{shown}");
    for p in &problems {
        println!("  note: {p}");
    }
    // Built-in reasons first: no rule can change them.
    let engine = parts.first().is_some_and(|p| p.eq_ignore_ascii_case(ENGINE_DIR));
    let builtin = if engine {
        Some("OneUpDrive's own folder (version store and staging); never synced".to_string())
    } else if !is_dir && names::not_synced(name).is_some() {
        Some("Office lock files, *.tmp, desktop.ini and the like are never synced".into())
    } else if names::is_invalid(name) {
        Some("OneDrive doesn't accept this name, so it stays on this PC".into())
    } else if row.as_ref().is_some_and(|r| r.kind == LocalKind::File && r.size > names::MAX_FILE_BYTES) {
        Some("larger than OneDrive's 250 GB limit, so it stays on this PC".into())
    } else {
        None
    };
    if let Some(b) = builtin {
        println!("  not synced: {b}");
    }
    let verdict = set.verdict(rel, is_dir);
    let excluded = verdict.is_excluded();
    let paused =
        matches!(&verdict, Verdict::Excluded(m) | Verdict::InsideExcluded { by: m, .. } if m.source == Source::Pause);
    match verdict {
        Verdict::Excluded(m) | Verdict::InsideExcluded { by: m, .. } if m.source == Source::Pause => {
            println!("  paused: {} (oneup resume --folder to continue)", m.rule.trim_end_matches('/'));
        }
        Verdict::Excluded(m) => println!("  not synced: excluded by {} (line {}: {})", m.source, m.line, m.rule),
        Verdict::InsideExcluded { folder, by } => {
            println!("  not synced: inside /{folder}, excluded by {} (line {}: {})", by.source, by.line, by.rule);
        }
        Verdict::Included(Some(m)) => {
            println!("  rules: included again by {} (line {}: {})", m.source, m.line, m.rule);
        }
        Verdict::Included(None) => println!("  rules: no rule matches"),
    }
    if engine {
        return Ok(());
    }
    let listed = skipped_at.is_none();
    if let Some(at) = skipped_at {
        let at = if at.is_empty() { "the synced folder".into() } else { format!("/{at}") };
        println!("  {at} is skipped while listing: its contents aren't looked at");
    }
    match &row {
        None if !listed => {}
        None => println!("  not on this PC"),
        Some(r) => {
            let synced = store.read(|o| o.synced_by_local(r.file_id))?.is_some();
            let state = match (synced, excluded) {
                (true, true) if paused => "synced; changes on either side wait until the folder is resumed",
                (true, true) => {
                    "synced before the rule; changes on either side wait until it's removed (nothing is deleted)"
                }
                (true, false) => "on this PC and synced",
                (false, true) => "on this PC only",
                (false, false) => "on this PC, not synced (yet)",
            };
            println!("  {state}");
        }
    }
    let last = store.read(|o| o.activity_recent(Some(&format!("/{rel}")), 1))?;
    if let Some(e) = last.first() {
        println!("  last activity: {}", crate::activity::line(e, crate::activity::local_offset()));
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used)]
    use super::*;

    #[test]
    fn changes_turn_presets_on_and_off_and_edit_rules() {
        let mut r = Rules::default();
        let c = Changes { on: vec!["DEV".into(), "temp".into()], add: vec!["*.iso".into()], ..Changes::default() };
        apply(&mut r, &c).unwrap();
        assert_eq!(r.presets, ["dev", "temp"]);
        assert_eq!(r.exclude, ["*.iso"]);
        apply(&mut r, &Changes { on: vec!["dev".into()], ..Changes::default() }).unwrap();
        assert_eq!(r.presets, ["dev", "temp"], "already on");
        apply(&mut r, &Changes { off: vec!["Dev".into()], remove: vec!["*.iso".into()], ..Changes::default() })
            .unwrap();
        assert_eq!((r.presets.as_slice(), r.exclude.len()), (&["temp".to_string()][..], 0));
        assert!(apply(&mut r, &Changes { on: vec!["all".into()], ..Changes::default() }).is_err());
        assert!(apply(&mut r, &Changes { remove: vec!["x".into()], ..Changes::default() }).is_err());
        assert!(apply(&mut r, &Changes { add: vec!["# no".into()], ..Changes::default() }).is_err());
    }
}
