//! The data folder's `config.toml`: the user's settings, readable and
//! editable by hand. Sections: `[rules]` and `[pause]`
//! (`docs/architecture/rules.md`), `[agent]` (how the background agent
//! uses the PC). Edits made through `oneup` keep the user's comments and
//! layout.

use std::path::{Path, PathBuf};

use anyhow::{Context, Result, bail};
use oneup_rules::{Rule, RuleSet};
use oneup_store::Store;
use toml_edit::{Array, DocumentMut, Item, Table, value};

use crate::setup::KEY_PRESET;

pub const FILE: &str = "config.toml";

/// What a new `config.toml` starts with, before the user's choices.
const HEADER: &str = "# OneUpDrive settings for this data folder. Edit by hand or with `oneup rules`.\n";

/// The `[rules]` section.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Rules {
    /// Presets turned on (`dev`, `temp`, `os-junk`).
    pub presets: Vec<String>,
    /// The user's own rules, gitignore style, relative to the synced folder.
    pub exclude: Vec<String>,
    /// Paused folders (`[pause] folders`), relative to the synced folder.
    pub paused: Vec<String>,
    /// What happens to the OneDrive copies of excluded items.
    pub cloud: CloudCopies,
}

/// `[rules] cloud_copies`: what happens to the OneDrive copies of excluded
/// items (`docs/architecture/rules.md` § Cloud cleanup). Excluding always
/// stops uploads; this decides what comes the other way.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub enum CloudCopies {
    /// Left alone, and not brought here either. They and the copies here
    /// may drift apart; re-including an item syncs it again from where it
    /// was.
    Keep,
    /// Brought here and then removed from OneDrive (to its recycle bin)
    /// automatically, with the usual safety stops.
    Prune,
    /// Brought here from OneDrive, never sent (the default: excluding means
    /// "don't upload"): they appear here (online-only with Files
    /// On-Demand), a cloud change replaces the copy here as usual, a change
    /// here first becomes a conflict once the cloud copy changes, and
    /// what's only here stays only here.
    #[default]
    Pull,
}

impl CloudCopies {
    pub fn name(self) -> &'static str {
        match self {
            CloudCopies::Keep => "keep",
            CloudCopies::Prune => "prune",
            CloudCopies::Pull => "pull",
        }
    }

    pub fn from_name(s: &str) -> Option<Self> {
        [CloudCopies::Keep, CloudCopies::Prune, CloudCopies::Pull].into_iter().find(|c| c.name() == s)
    }
}

impl Rules {
    /// The rule set these describe (without `.oneupignore` files).
    pub fn rule_set(&self) -> Result<RuleSet> {
        let mut set = RuleSet::new(&self.presets, &self.exclude)?;
        set.set_paused(&self.paused);
        Ok(set)
    }
}

/// `[onedrive_app]`: OneDrive folders left to the OneDrive app, for Office
/// AutoSave there (`docs/user-guide/office.md`). OneUpDrive treats them as
/// excluded (whatever other rules say) and never removes them from OneDrive.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct OneDriveApp {
    /// OneDrive paths (`/Office`).
    pub folders: Vec<String>,
    /// The files at the top of OneDrive too (the OneDrive app always syncs
    /// those). Applies only when the synced folder is the whole drive.
    pub top_files: bool,
}

pub fn load_onedrive_app(data_dir: &Path) -> Result<OneDriveApp> {
    let doc = read_doc(data_dir)?;
    let Some(t) = doc.as_ref().map(|d| section(d, "onedrive_app")).transpose()?.flatten() else {
        return Ok(OneDriveApp::default());
    };
    Ok(OneDriveApp {
        folders: strings(t, "folders")?.unwrap_or_default(),
        top_files: t.get("top_files").and_then(|v| v.as_bool()).unwrap_or(false),
    })
}

pub fn save_onedrive_app(data_dir: &Path, a: &OneDriveApp) -> Result<()> {
    save(
        data_dir,
        "onedrive_app",
        "# OneDrive folders left to the OneDrive app (for Office AutoSave there). OneUpDrive\n\
         # leaves them alone; set the OneDrive app to sync only these (Settings > Account >\n\
         # Choose folders), into a folder of its own.\n",
        vec![("folders", value(a.folders.iter().collect::<Array>())), ("top_files", value(a.top_files))],
    )
}

/// The `[agent]` section: how the background agent uses the PC.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Agent {
    /// Upload and download caps in KiB/s; 0 = no cap.
    pub upload_kib: u64,
    pub download_kib: u64,
    /// `minimal`, `balanced` or `performance` (database cache size).
    pub memory: String,
    /// Run at background priority (CPU, disk and memory) and let Windows
    /// run it power-efficiently (EcoQoS).
    pub background: bool,
    /// Pause by itself on a metered connection / with battery saver on.
    pub pause_on_metered: bool,
    pub pause_on_battery_saver: bool,
    /// Pause by itself while a game, a presentation or another full-screen
    /// program (not a video) has the screen.
    pub pause_on_full_screen: bool,
    /// Pause by itself while a video plays full screen (in a browser or a
    /// video player). Apart from games, so someone can pause for games but
    /// not for videos.
    pub pause_on_full_screen_video: bool,
    /// Pause by itself in these hours, `HH:MM-HH:MM` local time.
    pub quiet_hours: Option<String>,
    /// The bandwidth caps apply only in these hours (always when unset).
    pub cap_hours: Option<String>,
    /// Show the notification-area icon (`oneup-tray`) when the agent starts.
    pub tray_icon: bool,
    /// Note which programs change and open files in the synced folder
    /// (counts only, kept on this PC; `crate::apps`).
    pub track_apps: bool,
}

impl Default for Agent {
    fn default() -> Self {
        Self {
            upload_kib: 0,
            download_kib: 0,
            memory: "balanced".into(),
            background: true,
            pause_on_metered: true,
            pause_on_battery_saver: true,
            pause_on_full_screen: true,
            pause_on_full_screen_video: true,
            quiet_hours: None,
            cap_hours: None,
            tray_icon: true,
            track_apps: true,
        }
    }
}

/// The memory profiles, by name, with their database cache sizes (KiB).
pub const MEMORY_PROFILES: &[(&str, u32)] =
    &[("minimal", 4 * 1024), ("balanced", 16 * 1024), ("performance", 64 * 1024)];

impl Agent {
    pub fn cache_kib(&self) -> u32 {
        MEMORY_PROFILES.iter().find(|(n, _)| *n == self.memory).map_or(16 * 1024, |(_, k)| *k)
    }
}

pub fn path(data_dir: &Path) -> PathBuf {
    data_dir.join(FILE)
}

fn read_doc(data_dir: &Path) -> Result<Option<DocumentMut>> {
    let p = path(data_dir);
    match std::fs::read_to_string(&p) {
        Ok(text) => Ok(Some(text.parse::<DocumentMut>().with_context(|| format!("reading {}", p.display()))?)),
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(None),
        Err(e) => Err(e).with_context(|| format!("reading {}", p.display())),
    }
}

fn section<'d>(doc: &'d DocumentMut, name: &str) -> Result<Option<&'d Table>> {
    match doc.get(name) {
        Some(Item::Table(t)) => Ok(Some(t)),
        Some(_) => bail!("{FILE}: `{name}` must be a section ([{name}])"),
        None => Ok(None),
    }
}

fn strings(t: &Table, key: &str) -> Result<Option<Vec<String>>> {
    let Some(item) = t.get(key) else { return Ok(None) };
    let bad = || format!("{FILE}: {key} must be a list of strings");
    let Some(arr) = item.as_array() else { bail!(bad()) };
    arr.iter().map(|v| v.as_str().map(str::to_owned).with_context(bad)).collect::<Result<Vec<_>>>().map(Some)
}

fn number(t: &Table, key: &str) -> Result<Option<u64>> {
    let Some(item) = t.get(key) else { return Ok(None) };
    let n = item.as_integer().with_context(|| format!("{FILE}: {key} must be a whole number"))?;
    Ok(Some(u64::try_from(n).with_context(|| format!("{FILE}: {key} can't be negative"))?))
}

/// Sets `entries` in section `name` (created with comment `intro` if
/// missing), in place where a key exists so the comments around it stay,
/// and writes the file: whole, then renamed into place.
fn save(data_dir: &Path, name: &str, intro: &str, entries: Vec<(&str, Item)>) -> Result<()> {
    let existing = read_doc(data_dir)?;
    let fresh = existing.is_none();
    let mut doc = existing.unwrap_or_default();
    if !doc.contains_table(name) {
        let mut t = Table::new();
        t.decor_mut().set_prefix(if fresh { format!("{HEADER}\n{intro}") } else { format!("\n{intro}") });
        doc.insert(name, Item::Table(t));
    }
    let t = doc[name].as_table_mut().with_context(|| format!("[{name}] must be a section"))?;
    for (key, v) in entries {
        match t.get_mut(key) {
            Some(item) => *item = v,
            None => _ = t.insert(key, v),
        }
    }
    let p = path(data_dir);
    let tmp = data_dir.join(format!("{FILE}.tmp"));
    std::fs::write(&tmp, doc.to_string()).with_context(|| format!("writing {}", tmp.display()))?;
    std::fs::rename(&tmp, &p).with_context(|| format!("writing {}", p.display()))?;
    Ok(())
}

/// The rules. Without a `[rules]` section, the preset chosen when the
/// folder was first scanned or synced (stored in the database) is the
/// starting point, so existing data folders keep what they had.
pub fn load_rules(data_dir: &Path, store: &Store) -> Result<Rules> {
    let doc = read_doc(data_dir)?;
    let rules_table = doc.as_ref().map(|d| section(d, "rules")).transpose()?.flatten();
    let listed = match rules_table {
        Some(t) => Some((strings(t, "presets")?, strings(t, "exclude")?)),
        None => None,
    };
    let cloud = match rules_table.and_then(|t| t.get("cloud_copies")) {
        Some(v) => v
            .as_str()
            .and_then(CloudCopies::from_name)
            .with_context(|| format!("{FILE}: cloud_copies must be \"keep\", \"prune\" or \"pull\""))?,
        None => CloudCopies::default(),
    };
    let paused = match doc.as_ref().map(|d| section(d, "pause")).transpose()?.flatten() {
        Some(t) => strings(t, "folders")?.unwrap_or_default(),
        None => Vec::new(),
    };
    let stored = || -> Result<Vec<String>> {
        Ok(match store.read(|o| o.meta_get(KEY_PRESET))?.as_deref() {
            Some("dev") => vec!["dev".to_string()],
            _ => Vec::new(),
        })
    };
    let rules = match listed {
        Some((presets, exclude)) => {
            Rules { presets: presets.map_or_else(stored, Ok)?, exclude: exclude.unwrap_or_default(), paused, cloud }
        }
        None => Rules { presets: stored()?, exclude: Vec::new(), paused, cloud },
    };
    rules.rule_set()?; // unknown presets are reported here, not later
    Ok(rules)
}

/// Writes `rules` into `config.toml`, keeping everything else in the file
/// as it was.
pub fn save_rules(data_dir: &Path, rules: &Rules) -> Result<()> {
    let list = |l: &Vec<String>| value(l.iter().collect::<Array>());
    save(
        data_dir,
        "rules",
        "# What syncs. presets: dev, temp, os-junk (`oneup rules` explains them).\n\
         # exclude: your own rules, gitignore style, relative to the synced folder.\n",
        vec![
            ("presets", list(&rules.presets)),
            ("exclude", list(&rules.exclude)),
            ("cloud_copies", value(rules.cloud.name())),
        ],
    )?;
    let has_pause = read_doc(data_dir)?.is_some_and(|d| d.contains_table("pause"));
    if !rules.paused.is_empty() || has_pause {
        save(
            data_dir,
            "pause",
            "# Paused folders: changes there wait until resumed (`oneup resume --folder`).\n",
            vec![("folders", list(&rules.paused))],
        )?;
    }
    Ok(())
}

/// The `[agent]` section (defaults for what's missing).
pub fn load_agent(data_dir: &Path) -> Result<Agent> {
    let mut a = Agent::default();
    let Some(doc) = read_doc(data_dir)? else { return Ok(a) };
    let Some(t) = section(&doc, "agent")? else { return Ok(a) };
    if let Some(n) = number(t, "upload_kib_per_s")? {
        a.upload_kib = n;
    }
    if let Some(n) = number(t, "download_kib_per_s")? {
        a.download_kib = n;
    }
    if let Some(m) = t.get("memory_profile") {
        let m = m.as_str().with_context(|| format!("{FILE}: memory_profile must be a string"))?;
        if !MEMORY_PROFILES.iter().any(|(n, _)| *n == m) {
            bail!("{FILE}: memory_profile must be minimal, balanced or performance");
        }
        a.memory = m.to_string();
    }
    let flag = |key: &str| -> Result<Option<bool>> {
        t.get(key).map(|b| b.as_bool().with_context(|| format!("{FILE}: {key} must be true or false"))).transpose()
    };
    if let Some(b) = flag("background")? {
        a.background = b;
    }
    if let Some(b) = flag("pause_on_metered")? {
        a.pause_on_metered = b;
    }
    if let Some(b) = flag("pause_on_battery_saver")? {
        a.pause_on_battery_saver = b;
    }
    if let Some(b) = flag("pause_on_full_screen")? {
        a.pause_on_full_screen = b;
    }
    // Before it was its own setting, videos followed the one above.
    a.pause_on_full_screen_video = flag("pause_on_full_screen_video")?.unwrap_or(a.pause_on_full_screen);
    if let Some(b) = flag("tray_icon")? {
        a.tray_icon = b;
    }
    if let Some(b) = flag("track_apps")? {
        a.track_apps = b;
    }
    if let Some(q) = t.get("cap_hours") {
        let q = q.as_str().with_context(|| format!("{FILE}: cap_hours must be a string like \"09:00-18:00\""))?;
        if !q.is_empty() {
            crate::autopause::parse_quiet_hours(q).with_context(|| format!("{FILE}: cap_hours"))?;
            a.cap_hours = Some(q.to_string());
        }
    }
    if let Some(q) = t.get("quiet_hours") {
        let q = q.as_str().with_context(|| format!("{FILE}: quiet_hours must be a string like \"22:00-07:00\""))?;
        if !q.is_empty() {
            crate::autopause::parse_quiet_hours(q).with_context(|| format!("{FILE}: quiet_hours"))?;
            a.quiet_hours = Some(q.to_string());
        }
    }
    Ok(a)
}

pub fn save_agent(data_dir: &Path, a: &Agent) -> Result<()> {
    let n = |v: u64| value(i64::try_from(v).unwrap_or(i64::MAX));
    save(
        data_dir,
        "agent",
        "# How the background agent uses this PC. Caps in KiB/s, 0 = none.\n",
        vec![
            ("upload_kib_per_s", n(a.upload_kib)),
            ("download_kib_per_s", n(a.download_kib)),
            ("memory_profile", value(a.memory.as_str())),
            ("background", value(a.background)),
            ("pause_on_metered", value(a.pause_on_metered)),
            ("pause_on_battery_saver", value(a.pause_on_battery_saver)),
            ("pause_on_full_screen", value(a.pause_on_full_screen)),
            ("pause_on_full_screen_video", value(a.pause_on_full_screen_video)),
            ("quiet_hours", value(a.quiet_hours.as_deref().unwrap_or(""))),
            ("cap_hours", value(a.cap_hours.as_deref().unwrap_or(""))),
            ("tray_icon", value(a.tray_icon)),
            ("track_apps", value(a.track_apps)),
        ],
    )
}

/// The `[space]` section: freeing up space by itself, with Files
/// On-Demand (`crate::space`). Freed files become online-only; their
/// content stays in OneDrive.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Space {
    /// Free files not used for this many days; 0 = never.
    pub free_unused_days: u32,
    /// When the disk has less than `low_disk_percent` free, free the files
    /// unused for longest (at least a day) until it has `low_disk_percent`
    /// plus 5 again.
    pub free_when_low: bool,
    pub low_disk_percent: u32,
}

impl Default for Space {
    fn default() -> Self {
        Self { free_unused_days: 0, free_when_low: false, low_disk_percent: 10 }
    }
}

pub fn load_space(data_dir: &Path) -> Result<Space> {
    let mut s = Space::default();
    let Some(doc) = read_doc(data_dir)? else { return Ok(s) };
    let Some(t) = section(&doc, "space")? else { return Ok(s) };
    if let Some(n) = number(t, "free_unused_days")? {
        s.free_unused_days =
            u32::try_from(n).with_context(|| format!("{FILE}: space.free_unused_days is too large"))?;
    }
    if let Some(b) = t.get("free_when_low") {
        s.free_when_low = b.as_bool().with_context(|| format!("{FILE}: space.free_when_low must be true or false"))?;
    }
    if let Some(n) = number(t, "low_disk_percent")? {
        if !(1..=50).contains(&n) {
            bail!("{FILE}: space.low_disk_percent must be between 1 and 50");
        }
        s.low_disk_percent = n as u32;
    }
    Ok(s)
}

pub fn save_space(data_dir: &Path, s: &Space) -> Result<()> {
    save(
        data_dir,
        "space",
        "# Freeing up space by itself (Files On-Demand): files become online-only, their content stays in OneDrive.\n",
        vec![
            ("free_unused_days", value(i64::from(s.free_unused_days))),
            ("free_when_low", value(s.free_when_low)),
            ("low_disk_percent", value(i64::from(s.low_disk_percent))),
        ],
    )
}

/// The `[updates]` section: OneUpDrive updating itself from its releases
/// (`crate::updates`).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Updates {
    /// Download and install updates by themselves.
    pub auto: bool,
    /// `stable`, or `preview` for pre-releases too.
    pub channel: String,
    /// The GitHub repository releases come from (`https://github.com/owner/repo`);
    /// empty: the one this build was made for.
    pub repo: String,
}

impl Default for Updates {
    fn default() -> Self {
        Self { auto: true, channel: "stable".into(), repo: String::new() }
    }
}

pub fn load_updates(data_dir: &Path) -> Result<Updates> {
    let mut u = Updates::default();
    let Some(doc) = read_doc(data_dir)? else { return Ok(u) };
    let Some(t) = section(&doc, "updates")? else { return Ok(u) };
    if let Some(b) = t.get("auto") {
        u.auto = b.as_bool().with_context(|| format!("{FILE}: updates.auto must be true or false"))?;
    }
    if let Some(c) = t.get("channel") {
        let c = c.as_str().unwrap_or_default();
        if c != "stable" && c != "preview" {
            bail!("{FILE}: updates.channel must be \"stable\" or \"preview\"");
        }
        u.channel = c.to_string();
    }
    if let Some(r) = t.get("repo") {
        u.repo = r.as_str().with_context(|| format!("{FILE}: updates.repo must be a string"))?.to_string();
    }
    Ok(u)
}

pub fn save_updates(data_dir: &Path, u: &Updates) -> Result<()> {
    save(
        data_dir,
        "updates",
        "# OneUpDrive updating itself: auto = true/false, channel = \"stable\" or \"preview\".
",
        vec![("auto", value(u.auto)), ("channel", value(u.channel.as_str())), ("repo", value(u.repo.as_str()))],
    )
}

/// The `[priority]` section: rules for what to transfer first and last.
pub fn load_priority(data_dir: &Path) -> Result<(Vec<String>, Vec<String>)> {
    let Some(doc) = read_doc(data_dir)? else { return Ok(Default::default()) };
    let Some(t) = section(&doc, "priority")? else { return Ok(Default::default()) };
    Ok((strings(t, "first")?.unwrap_or_default(), strings(t, "last")?.unwrap_or_default()))
}

pub fn save_priority(data_dir: &Path, first: &[String], last: &[String]) -> Result<()> {
    let list = |l: &[String]| value(l.iter().collect::<Array>());
    save(
        data_dir,
        "priority",
        "# What to send or fetch first, and last, within a round (rules as in [rules]).\n",
        vec![("first", list(first)), ("last", list(last))],
    )
}

/// `[dev] experimental`: show and allow experimental options (the Office
/// integration), off by default. Set by hand or with the hidden `oneup
/// settings --experimental on`; the settings window has no switch for it.
pub fn load_experimental(data_dir: &Path) -> Result<bool> {
    let Some(doc) = read_doc(data_dir)? else { return Ok(false) };
    let Some(t) = section(&doc, "dev")? else { return Ok(false) };
    t.get("experimental")
        .map(|b| b.as_bool().with_context(|| format!("{FILE}: experimental must be true or false")))
        .transpose()
        .map(Option::unwrap_or_default)
}

pub fn save_experimental(data_dir: &Path, on: bool) -> Result<()> {
    save(
        data_dir,
        "dev",
        "# For developers: experimental options, which may not work (`oneup settings --experimental`).\n",
        vec![("experimental", value(on))],
    )
}

/// Checks a rule the user typed: it must be one rule line.
pub fn check_rule(line: &str) -> Result<()> {
    if line.contains(['\n', '\r']) {
        bail!("one rule at a time");
    }
    if Rule::parse(line).is_none() {
        bail!("`{line}` isn't a rule (empty, a comment, or only a separator)");
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used)]
    use super::*;

    fn tmp(tag: &str) -> PathBuf {
        let d = std::env::temp_dir().join(format!("oneup-config-{tag}-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&d);
        std::fs::create_dir_all(&d).unwrap();
        d
    }

    #[test]
    fn without_a_file_the_stored_preset_applies() {
        let d = tmp("default");
        let store = Store::open_in_memory().unwrap();
        assert_eq!(load_rules(&d, &store).unwrap(), Rules::default());
        let mut store = store;
        store.write(|o| o.meta_set(KEY_PRESET, "dev")).unwrap();
        assert_eq!(load_rules(&d, &store).unwrap().presets, ["dev"]);
        let _ = std::fs::remove_dir_all(&d);
    }

    #[test]
    fn saving_keeps_the_users_comments_and_other_settings() {
        let d = tmp("save");
        std::fs::write(path(&d), "# mine\n[other]\nx = 1 # keep me\n\n[rules]\n# my rules\nexclude = [\"*.iso\"]\n")
            .unwrap();
        let store = Store::open_in_memory().unwrap();
        let mut r = load_rules(&d, &store).unwrap();
        assert_eq!(r, Rules { exclude: vec!["*.iso".into()], ..Rules::default() });
        r.presets.push("temp".into());
        r.exclude.push("/Downloads/".into());
        save_rules(&d, &r).unwrap();
        let text = std::fs::read_to_string(path(&d)).unwrap();
        assert!(text.contains("# mine") && text.contains("x = 1 # keep me") && text.contains("# my rules"), "{text}");
        assert_eq!(load_rules(&d, &store).unwrap(), r);
        let _ = std::fs::remove_dir_all(&d);
    }

    #[test]
    fn a_new_file_gets_a_header_and_bad_values_are_reported() {
        let d = tmp("new");
        let store = Store::open_in_memory().unwrap();
        save_rules(&d, &Rules { presets: vec!["dev".into()], cloud: CloudCopies::Keep, ..Rules::default() }).unwrap();
        assert_eq!(load_rules(&d, &store).unwrap().cloud, CloudCopies::Keep);
        let text = std::fs::read_to_string(path(&d)).unwrap();
        assert!(text.starts_with("# OneUpDrive settings") && text.contains("presets = [\"dev\"]"), "{text}");
        std::fs::write(path(&d), "[rules]\npresets = [\"everything\"]\n").unwrap();
        assert!(load_rules(&d, &store).is_err());
        std::fs::write(path(&d), "[rules]\nexclude = \"*.iso\"\n").unwrap();
        assert!(load_rules(&d, &store).is_err());
        let _ = std::fs::remove_dir_all(&d);
    }

    #[test]
    fn paused_folders_and_agent_settings_round_trip() {
        let d = tmp("pause-agent");
        let store = Store::open_in_memory().unwrap();
        assert_eq!(load_agent(&d).unwrap(), Agent::default());
        let mut r = load_rules(&d, &store).unwrap();
        r.paused.push("Videos".into());
        save_rules(&d, &r).unwrap();
        let a = Agent {
            upload_kib: 512,
            memory: "minimal".into(),
            background: false,
            pause_on_metered: false,
            quiet_hours: Some("22:00-07:00".into()),
            ..Agent::default()
        };
        save_agent(&d, &a).unwrap();
        assert_eq!(load_rules(&d, &store).unwrap().paused, ["Videos"]);
        assert!(load_rules(&d, &store).unwrap().rule_set().unwrap().excludes("videos/a.mp4", false));
        assert_eq!(load_agent(&d).unwrap(), a);
        assert_eq!(a.cache_kib(), 4 * 1024);
        std::fs::write(path(&d), "[agent]\nupload_kib_per_s = -1\n").unwrap();
        assert!(load_agent(&d).is_err());
        std::fs::write(path(&d), "[agent]\nmemory_profile = \"huge\"\n").unwrap();
        assert!(load_agent(&d).is_err());
        let _ = std::fs::remove_dir_all(&d);
    }

    #[test]
    fn experimental_options_are_off_until_turned_on() {
        let d = tmp("experimental");
        assert!(!load_experimental(&d).unwrap());
        save_agent(&d, &Agent::default()).unwrap();
        assert!(!load_experimental(&d).unwrap());
        save_experimental(&d, true).unwrap();
        assert!(load_experimental(&d).unwrap());
        assert_eq!(load_agent(&d).unwrap(), Agent::default());
        std::fs::write(path(&d), "[dev]\nexperimental = \"yes\"\n").unwrap();
        assert!(load_experimental(&d).is_err());
        let _ = std::fs::remove_dir_all(&d);
    }

    /// Full-screen video became its own setting: a config.toml from before,
    /// which turned full-screen pausing off, keeps it off for videos too;
    /// otherwise each stands on its own (games yes, videos no).
    #[test]
    fn video_pausing_follows_the_older_setting_until_set_itself() {
        let d = tmp("video-pause");
        std::fs::write(path(&d), "[agent]\npause_on_full_screen = false\n").unwrap();
        let a = load_agent(&d).unwrap();
        assert!(!a.pause_on_full_screen && !a.pause_on_full_screen_video);
        let games_only = Agent { pause_on_full_screen: true, pause_on_full_screen_video: false, ..Agent::default() };
        save_agent(&d, &games_only).unwrap();
        assert_eq!(load_agent(&d).unwrap(), games_only);
        let _ = std::fs::remove_dir_all(&d);
    }

    #[test]
    fn typed_rules_are_checked() {
        assert!(check_rule("*.iso").is_ok() && check_rule("!keep/").is_ok());
        assert!(check_rule("# note").is_err() && check_rule("  ").is_err() && check_rule("a\nb").is_err());
    }
}
