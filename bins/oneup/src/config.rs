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
    /// Pin rules (`[rules] pin`), same syntax: matching items are always
    /// kept on this PC with Files On-Demand (`crate::pinrules`).
    pub pin: Vec<String>,
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

    /// The pin rules.
    pub fn pin_set(&self) -> oneup_rules::PinSet {
        oneup_rules::PinSet::new(&self.pin)
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

/// `[databases]`: database files that sync only in a consistent state
/// (`docs/architecture/databases.md`, `crate::databases`).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DatabaseSettings {
    /// Databases other than SQLite wait for their programs to close them,
    /// both ways (on by default). SQLite databases always wait until
    /// they're consistent, and nothing is ever replaced or moved under a
    /// program, whatever this says.
    pub wait: bool,
    /// Database presets turned on (`oneup_rules::database::DATABASE_PRESETS`).
    pub presets: Vec<String>,
    /// The user's own patterns, gitignore style: files to treat as
    /// databases (`!` for one that isn't).
    pub patterns: Vec<String>,
    /// Outlook data files and virtual disks the user chose to keep
    /// syncing: no warning for them (paths relative to the synced folder).
    pub keep_syncing: Vec<String>,
}

impl Default for DatabaseSettings {
    fn default() -> Self {
        Self { wait: true, presets: Vec::new(), patterns: Vec::new(), keep_syncing: Vec::new() }
    }
}

impl DatabaseSettings {
    /// What the executor works with.
    pub fn databases(&self) -> Result<oneup_rules::database::Databases> {
        Ok(oneup_rules::database::Databases::new(self.wait, &self.presets, &self.patterns)?)
    }
}

pub fn load_databases(data_dir: &Path) -> Result<DatabaseSettings> {
    let mut d = DatabaseSettings::default();
    let Some(doc) = read_doc(data_dir)? else { return Ok(d) };
    let Some(t) = section(&doc, "databases")? else { return Ok(d) };
    if let Some(v) = t.get("wait") {
        d.wait = v.as_bool().with_context(|| format!("{FILE}: [databases] wait must be true or false"))?;
    }
    d.presets = strings(t, "presets")?.unwrap_or_default();
    d.patterns = strings(t, "patterns")?.unwrap_or_default();
    d.keep_syncing = strings(t, "keep_syncing")?.unwrap_or_default();
    d.databases()?; // an unknown preset is reported here, not later
    Ok(d)
}

pub fn save_databases(data_dir: &Path, d: &DatabaseSettings) -> Result<()> {
    let list = |l: &[String]| value(l.iter().collect::<Array>());
    save(
        data_dir,
        "databases",
        "# Database files sync only in a consistent state (`oneup databases`). presets: finance,\n\
         # firebird, filemaker, dbase, lotus-notes. patterns: your own, gitignore style.\n",
        vec![
            ("wait", value(d.wait)),
            ("presets", list(&d.presets)),
            ("patterns", list(&d.patterns)),
            ("keep_syncing", list(&d.keep_syncing)),
        ],
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
    /// Hold the uploads of files that keep changing (`crate::busy`).
    pub busy: crate::busy::Settings,
    /// Compact the database at the agent's start when free pages are most
    /// of it (`crate::sync::compact`).
    pub compact_database: bool,
    /// What the memory profile sets, set here instead.
    pub overrides: Overrides,
    /// Files read whole to hash them at once (`oneup_exec::Parallel::hashes`).
    pub hash_files_at_once: u32,
}

/// The numbers the memory profile sets, each of which `[agent]` can set
/// itself; `None`: the profile's.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct Overrides {
    pub database_cache_mib: Option<u32>,
    pub operations_at_once: Option<u32>,
    pub large_uploads_at_once: Option<u32>,
    pub upload_fragment_mib: Option<u32>,
}

/// A number in `[agent]`, with its bounds: the same checks for
/// `config.toml`, `oneup settings` and the settings window.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Knob {
    pub key: &'static str,
    /// For people (the settings window, `oneup settings`).
    pub label: &'static str,
    pub min: u32,
    pub max: u32,
}

impl Knob {
    /// The label inside a sentence: lower case, the unit as it is.
    pub fn name(&self) -> String {
        let mut s = self.label.to_string();
        s[..1].make_ascii_lowercase();
        s
    }

    pub fn check(&self, n: u64) -> Result<u32> {
        match u32::try_from(n) {
            Ok(v) if (self.min..=self.max).contains(&v) => Ok(v),
            _ => bail!("{} must be from {} to {}", self.key, self.min, self.max),
        }
    }
}

/// SQLite's page cache. The agent also maps four times this much of the
/// database while it runs at normal priority.
pub const DATABASE_CACHE_MIB: Knob =
    Knob { key: "database_cache_mib", label: "Database cache (MiB)", min: 1, max: 1024 };
/// Transfers and other operations under way. The Graph client's requests in
/// flight follow it, and stop at 32 (`oneup_exec::Parallel::requests`).
pub const OPERATIONS_AT_ONCE: Knob = Knob { key: "operations_at_once", label: "Operations at once", min: 1, max: 32 };
/// Each holds up to a fragment in memory: 8 of the largest is about 480 MiB.
pub const LARGE_UPLOADS_AT_ONCE: Knob =
    Knob { key: "large_uploads_at_once", label: "Large uploads at once", min: 1, max: 8 };
/// The largest upload fragment. Graph takes multiples of 320 KiB up to just
/// under 60 MiB, and the fragment sizer goes no lower than 1.25 MiB
/// (`oneup_graph::fragment`).
pub const UPLOAD_FRAGMENT_MIB: Knob =
    Knob { key: "upload_fragment_mib", label: "Largest upload fragment (MiB)", min: 2, max: 59 };
/// Each reads a file from disk as fast as the disk allows.
pub const HASH_FILES_AT_ONCE: Knob = Knob { key: "hash_files_at_once", label: "Files hashed at once", min: 1, max: 16 };

/// The keys of [`Overrides`], in the order they're shown.
pub const OVERRIDE_KEYS: [&str; 4] =
    ["database_cache_mib", "operations_at_once", "large_uploads_at_once", "upload_fragment_mib"];

/// A number the agent runs with, and where it comes from.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct Tuned {
    pub knob: Knob,
    /// What applies, in the knob's unit.
    pub value: f64,
    /// What it is without a value of its own (the memory profile's).
    pub default: f64,
    /// The value set in `[agent]`, if any (`value` can differ: a fragment
    /// is rounded down to whole units).
    pub set: Option<u32>,
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
            busy: crate::busy::Settings::default(),
            compact_database: true,
            overrides: Overrides::default(),
            hash_files_at_once: oneup_exec::Parallel::HASHES as u32,
        }
    }
}

/// The memory profiles, by name, with their database cache sizes (KiB).
pub const MEMORY_PROFILES: &[(&str, u32)] =
    &[("minimal", 4 * 1024), ("balanced", 16 * 1024), ("performance", 64 * 1024)];

impl Agent {
    /// The Graph client's request limiter for the agent and `oneup sync`,
    /// sized by the operations at once (`oneup_exec::Parallel::requests`).
    pub fn requests(&self) -> std::sync::Arc<oneup_graph::Aimd> {
        let (start, most) = self.parallel().requests();
        oneup_graph::Aimd::new(start, most)
    }

    /// The database cache: the profile's unless set.
    pub fn cache_kib(&self) -> u32 {
        match self.overrides.database_cache_mib {
            Some(m) => m.saturating_mul(1024),
            None => MEMORY_PROFILES.iter().find(|(n, _)| *n == self.memory).map_or(16 * 1024, |(_, k)| *k),
        }
    }

    /// How many operations run at once: the profile's unless set.
    pub fn parallel(&self) -> oneup_exec::Parallel {
        let p = oneup_exec::Parallel::for_profile(&self.memory);
        let o = &self.overrides;
        let or = |set: Option<u32>, profile: usize| set.map_or(profile, |n| n as usize);
        oneup_exec::Parallel {
            ops: or(o.operations_at_once, p.ops),
            large_uploads: or(o.large_uploads_at_once, p.large_uploads),
            hashes: self.hash_files_at_once.max(1) as usize,
        }
    }

    /// How much memory an upload may hold: the profile's unless set.
    pub fn upload_memory(&self) -> oneup_exec::UploadMemory {
        match self.overrides.upload_fragment_mib {
            Some(m) => oneup_exec::UploadMemory { fragment: (m as usize) << 20 },
            None => oneup_exec::UploadMemory::for_profile(&self.memory),
        }
    }

    /// Every number the memory profile sets, and the files hashed at once,
    /// as the agent runs with them.
    pub fn tuned(&self) -> Vec<Tuned> {
        let profile = Agent { overrides: Overrides::default(), ..self.clone() };
        let o = &self.overrides;
        // Fragments are whole 320 KiB units, so the profiles' aren't whole
        // MiB: shown to a tenth.
        let fragment = |a: &Agent| {
            let bytes = oneup_graph::fragment::Sizer::new(a.upload_memory().fragment).max_bytes();
            (bytes as f64 / f64::from(1 << 20) * 10.0).round() / 10.0
        };
        let (now, then) = (self.parallel(), profile.parallel());
        let row = |knob, value: f64, default: f64, set: Option<u32>| Tuned { knob, value, default, set };
        vec![
            row(
                DATABASE_CACHE_MIB,
                f64::from(self.cache_kib() / 1024),
                f64::from(profile.cache_kib() / 1024),
                o.database_cache_mib,
            ),
            row(OPERATIONS_AT_ONCE, now.ops as f64, then.ops as f64, o.operations_at_once),
            row(LARGE_UPLOADS_AT_ONCE, now.large_uploads as f64, then.large_uploads as f64, o.large_uploads_at_once),
            row(UPLOAD_FRAGMENT_MIB, fragment(self), fragment(&profile), o.upload_fragment_mib),
            row(
                HASH_FILES_AT_ONCE,
                f64::from(self.hash_files_at_once),
                oneup_exec::Parallel::HASHES as f64,
                (self.hash_files_at_once as usize != oneup_exec::Parallel::HASHES).then_some(self.hash_files_at_once),
            ),
        ]
    }

    /// Sets (`Some`) or clears (`None`: the profile's) the override `key`,
    /// checked against its bounds.
    pub fn set_override(&mut self, key: &str, n: Option<u64>) -> Result<()> {
        let o = &mut self.overrides;
        let (knob, slot) = match key {
            "database_cache_mib" => (DATABASE_CACHE_MIB, &mut o.database_cache_mib),
            "operations_at_once" => (OPERATIONS_AT_ONCE, &mut o.operations_at_once),
            "large_uploads_at_once" => (LARGE_UPLOADS_AT_ONCE, &mut o.large_uploads_at_once),
            "upload_fragment_mib" => (UPLOAD_FRAGMENT_MIB, &mut o.upload_fragment_mib),
            _ => bail!("{key}: not a memory setting"),
        };
        *slot = n.map(|n| knob.check(n)).transpose()?;
        Ok(())
    }
}

pub fn path(data_dir: &Path) -> PathBuf {
    data_dir.join(FILE)
}

pub(crate) fn read_doc(data_dir: &Path) -> Result<Option<DocumentMut>> {
    let p = path(data_dir);
    match std::fs::read_to_string(&p) {
        Ok(text) => Ok(Some(text.parse::<DocumentMut>().with_context(|| format!("reading {}", p.display()))?)),
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(None),
        Err(e) => Err(e).with_context(|| format!("reading {}", p.display())),
    }
}

pub(crate) fn section<'d>(doc: &'d DocumentMut, name: &str) -> Result<Option<&'d Table>> {
    match doc.get(name) {
        Some(Item::Table(t)) => Ok(Some(t)),
        Some(_) => bail!("{FILE}: `{name}` must be a section ([{name}])"),
        None => Ok(None),
    }
}

pub(crate) fn strings(t: &Table, key: &str) -> Result<Option<Vec<String>>> {
    let Some(item) = t.get(key) else { return Ok(None) };
    let bad = || format!("{FILE}: {key} must be a list of strings");
    let Some(arr) = item.as_array() else { bail!(bad()) };
    arr.iter().map(|v| v.as_str().map(str::to_owned).with_context(bad)).collect::<Result<Vec<_>>>().map(Some)
}

pub(crate) fn number(t: &Table, key: &str) -> Result<Option<u64>> {
    let Some(item) = t.get(key) else { return Ok(None) };
    let n = item.as_integer().with_context(|| format!("{FILE}: {key} must be a whole number"))?;
    Ok(Some(u64::try_from(n).with_context(|| format!("{FILE}: {key} can't be negative"))?))
}

/// Sets `entries` in section `name` (created with comment `intro` if
/// missing), in place where a key exists so the comments around it stay,
/// and writes the file: whole, flushed, then renamed into place.
pub(crate) fn save(data_dir: &Path, name: &str, intro: &str, entries: Vec<(&str, Item)>) -> Result<()> {
    save_or_remove(data_dir, name, intro, entries.into_iter().map(|(k, v)| (k, Some(v))).collect())
}

/// [`save`], removing the keys given `None` (a setting back to its default).
fn save_or_remove(data_dir: &Path, name: &str, intro: &str, entries: Vec<(&str, Option<Item>)>) -> Result<()> {
    let existing = read_doc(data_dir)?;
    let fresh = existing.is_none();
    let mut doc = existing.unwrap_or_default();
    put_section(&mut doc, fresh, name, intro, entries)?;
    write_doc(data_dir, &doc)
}

/// Writes the whole file at once: flushed, then renamed into place.
fn write_doc(data_dir: &Path, doc: &DocumentMut) -> Result<()> {
    let p = path(data_dir);
    crate::ownfile::write(&p, doc.to_string().as_bytes()).with_context(|| format!("writing {}", p.display()))
}

/// [`save_or_remove`]'s change, in a document being edited.
fn put_section(
    doc: &mut DocumentMut,
    fresh: bool,
    name: &str,
    intro: &str,
    entries: Vec<(&str, Option<Item>)>,
) -> Result<()> {
    if !doc.contains_table(name) {
        let mut t = Table::new();
        t.decor_mut().set_prefix(if fresh { format!("{HEADER}\n{intro}") } else { format!("\n{intro}") });
        doc.insert(name, Item::Table(t));
    }
    let t = doc[name].as_table_mut().with_context(|| format!("[{name}] must be a section"))?;
    for (key, v) in entries {
        match (t.get_mut(key), v) {
            (Some(item), Some(v)) => *item = v,
            (None, Some(v)) => _ = t.insert(key, v),
            (Some(_), None) => _ = t.remove(key),
            (None, None) => {}
        }
    }
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
    let pin = match rules_table {
        Some(t) => strings(t, "pin")?.unwrap_or_default(),
        None => Vec::new(),
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
        Some((presets, exclude)) => Rules {
            presets: presets.map_or_else(stored, Ok)?,
            exclude: exclude.unwrap_or_default(),
            paused,
            cloud,
            pin,
        },
        None => Rules { presets: stored()?, exclude: Vec::new(), paused, cloud, pin },
    };
    rules.rule_set()?; // unknown presets are reported here, not later
    Ok(rules)
}

/// `[pause] folders` alone, without the store [`load_rules`] needs (the
/// problems list); a file that can't be read has none.
pub fn paused_folders(data_dir: &Path) -> Vec<String> {
    let doc = read_doc(data_dir).ok().flatten();
    let t = doc.as_ref().and_then(|d| section(d, "pause").ok().flatten());
    t.and_then(|t| strings(t, "folders").ok().flatten()).unwrap_or_default()
}

/// Writes `rules` into `config.toml`, keeping everything else in the file
/// as it was.
pub fn save_rules(data_dir: &Path, rules: &Rules) -> Result<()> {
    let list = |l: &Vec<String>| value(l.iter().collect::<Array>());
    let mut entries = vec![
        ("presets", list(&rules.presets)),
        ("exclude", list(&rules.exclude)),
        ("cloud_copies", value(rules.cloud.name())),
    ];
    // Written once there are pin rules, so other files stay as they were.
    let has_pin = read_doc(data_dir)?
        .is_some_and(|d| d.get("rules").and_then(Item::as_table).is_some_and(|t| t.contains_key("pin")));
    if !rules.pin.is_empty() || has_pin {
        entries.push(("pin", list(&rules.pin)));
    }
    save(
        data_dir,
        "rules",
        "# What syncs. presets: dev, temp, os-junk (`oneup rules` explains them).\n\
         # exclude: your own rules, gitignore style, relative to the synced folder.\n\
         # pin: rules for what Files On-Demand always keeps on this PC.\n",
        entries,
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
    if let Some(b) = flag("hold_busy_files")? {
        a.busy.on = b;
    }
    if let Some(n) = number(t, "busy_changes")? {
        a.busy.changes = u32::try_from(n).unwrap_or(u32::MAX);
    }
    let seconds =
        |key: &str| -> Result<Option<std::time::Duration>> { Ok(number(t, key)?.map(std::time::Duration::from_secs)) };
    if let Some(d) = seconds("busy_window_s")? {
        a.busy.window = d;
    }
    if let Some(d) = seconds("busy_quiet_s")? {
        a.busy.quiet = d;
    }
    if let Some(d) = seconds("busy_max_s")? {
        a.busy.max = d;
    }
    check_busy(&a.busy).with_context(|| format!("{FILE}: [agent]"))?;
    if let Some(b) = flag("compact_database")? {
        a.compact_database = b;
    }
    for key in OVERRIDE_KEYS {
        if let Some(n) = number(t, key)? {
            a.set_override(key, Some(n)).with_context(|| format!("{FILE}: [agent]"))?;
        }
    }
    if let Some(n) = number(t, HASH_FILES_AT_ONCE.key)? {
        a.hash_files_at_once = HASH_FILES_AT_ONCE.check(n).with_context(|| format!("{FILE}: [agent]"))?;
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

/// Whether the settings for files that keep changing make sense (the same
/// checks for `config.toml`, `oneup settings` and the settings window).
pub fn check_busy(b: &crate::busy::Settings) -> Result<()> {
    use crate::busy::{MAX_CHANGES, MAX_DURATION};
    if !(2..=MAX_CHANGES).contains(&b.changes) {
        bail!("busy_changes must be from 2 to {MAX_CHANGES}");
    }
    for (key, d) in [("busy_window_s", b.window), ("busy_quiet_s", b.quiet), ("busy_max_s", b.max)] {
        if d.as_secs() == 0 || d > MAX_DURATION {
            bail!("{key} must be from 1 to {} seconds", MAX_DURATION.as_secs());
        }
    }
    if b.max < b.quiet {
        bail!("busy_max_s can't be less than busy_quiet_s");
    }
    Ok(())
}

const AGENT_INTRO: &str = "# How the background agent uses this PC. Caps in KiB/s, 0 = none.\n";

pub fn save_agent(data_dir: &Path, a: &Agent) -> Result<()> {
    save_or_remove(data_dir, "agent", AGENT_INTRO, agent_entries(a))
}

/// `[agent]` and whatever `more` changes, written in one go: settings that
/// must change together (`crate::network`'s metered choice and
/// `pause_on_metered`) never end up half saved.
pub(crate) fn save_agent_with(
    data_dir: &Path,
    a: &Agent,
    more: impl FnOnce(&mut DocumentMut) -> Result<()>,
) -> Result<()> {
    let existing = read_doc(data_dir)?;
    let fresh = existing.is_none();
    let mut doc = existing.unwrap_or_default();
    put_section(&mut doc, fresh, "agent", AGENT_INTRO, agent_entries(a))?;
    more(&mut doc)?;
    write_doc(data_dir, &doc)
}

fn agent_entries(a: &Agent) -> Vec<(&'static str, Option<Item>)> {
    let n = |v: u64| value(i64::try_from(v).unwrap_or(i64::MAX));
    let mut entries: Vec<(&str, Option<Item>)> = vec![
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
        ("hold_busy_files", value(a.busy.on)),
        ("busy_changes", value(i64::from(a.busy.changes))),
        ("busy_window_s", n(a.busy.window.as_secs())),
        ("busy_quiet_s", n(a.busy.quiet.as_secs())),
        ("busy_max_s", n(a.busy.max.as_secs())),
        ("compact_database", value(a.compact_database)),
        ("hash_files_at_once", value(i64::from(a.hash_files_at_once))),
    ]
    .into_iter()
    .map(|(k, v)| (k, Some(v)))
    .collect::<Vec<_>>();
    // Unset: the memory profile's, so the key goes.
    let o = &a.overrides;
    for (key, v) in OVERRIDE_KEYS.into_iter().zip([
        o.database_cache_mib,
        o.operations_at_once,
        o.large_uploads_at_once,
        o.upload_fragment_mib,
    ]) {
        entries.push((key, v.map(|v| value(i64::from(v)))));
    }
    entries
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

/// For settings the agent reads again at every round (`[agent]`, `[space]`,
/// `[priority]`): `loaded` if it read, else the last good value this
/// process read for `data_dir`, so a mistake made while editing
/// config.toml doesn't stop syncing. An error only when there's none (a
/// one-off command, or an agent that started with the mistake). Not for
/// `[rules]`: syncing with rules other than the user's could send or
/// remove the wrong items.
pub fn kept<T: Clone + Send + 'static>(data_dir: &Path, loaded: Result<T>) -> Result<T> {
    use std::any::{Any, TypeId};
    // A handful of entries: one per data folder and kind of settings.
    type Last = Vec<(PathBuf, TypeId, Box<dyn Any + Send>)>;
    static LAST: std::sync::Mutex<Last> = std::sync::Mutex::new(Vec::new());
    let mut last = LAST.lock().unwrap_or_else(std::sync::PoisonError::into_inner);
    let at = last.iter().position(|(d, t, _)| d == data_dir && *t == TypeId::of::<T>());
    match loaded {
        Ok(v) => {
            let entry = (data_dir.to_path_buf(), TypeId::of::<T>(), Box::new(v.clone()) as Box<dyn Any + Send>);
            match at {
                Some(i) => last[i] = entry,
                None => last.push(entry),
            }
            Ok(v)
        }
        Err(e) => match at.and_then(|i| last[i].2.downcast_ref::<T>()) {
            Some(v) => {
                tracing::debug!(error = %format!("{e:#}"), "config.toml: keeping the previous settings");
                Ok(v.clone())
            }
            None => Err(e),
        },
    }
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

/// The `[search]` section (`crate::search`).
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Search {
    /// Also ask OneDrive to search inside files (sends what's searched for
    /// to Microsoft). Off unless turned on.
    pub onedrive: bool,
    /// The key combination that opens search from anywhere (`Ctrl+Alt+F`);
    /// empty: none.
    pub hotkey: String,
}

pub fn load_search(data_dir: &Path) -> Result<Search> {
    let mut s = Search::default();
    let Some(doc) = read_doc(data_dir)? else { return Ok(s) };
    let Some(t) = section(&doc, "search")? else { return Ok(s) };
    if let Some(b) = t.get("onedrive") {
        s.onedrive = b.as_bool().with_context(|| format!("{FILE}: search.onedrive must be true or false"))?;
    }
    if let Some(h) = t.get("hotkey") {
        let h = h.as_str().with_context(|| format!("{FILE}: search.hotkey must be a string"))?.trim();
        if !h.is_empty() {
            crate::search::Hotkey::parse(h).with_context(|| format!("{FILE}: search.hotkey"))?;
        }
        s.hotkey = h.to_string();
    }
    Ok(s)
}

pub fn save_search(data_dir: &Path, s: &Search) -> Result<()> {
    save(
        data_dir,
        "search",
        "# Search: onedrive = true also asks OneDrive to search inside files (sends the search text to Microsoft);
         # hotkey = \"Ctrl+Alt+F\" opens search from anywhere (\"\" for none).
",
        vec![("onedrive", value(s.onedrive)), ("hotkey", value(s.hotkey.as_str()))],
    )
}

/// `[hooks]` and the `[[hook]]` list: programs the agent starts on sync
/// events (`crate::hooks`, `docs/architecture/hooks.md`).
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Hooks {
    /// `[hooks] paused`: none start, none are queued.
    pub paused: bool,
    pub list: Vec<oneup_hooks::HookSpec>,
}

/// Reads the hooks. A hook whose shape is wrong is still listed (the agent
/// says why it doesn't run it); only a value of the wrong type, or a name
/// used twice, is an error for the whole list.
pub fn load_hooks(data_dir: &Path) -> Result<Hooks> {
    use oneup_hooks::HookSpec;
    use std::time::Duration;
    let mut h = Hooks::default();
    let Some(doc) = read_doc(data_dir)? else { return Ok(h) };
    if let Some(t) = section(&doc, "hooks")? {
        h.paused = match t.get("paused") {
            Some(v) => v.as_bool().with_context(|| format!("{FILE}: hooks.paused must be true or false"))?,
            None => false,
        };
    }
    let tables = match doc.get("hook") {
        None => return Ok(h),
        Some(Item::ArrayOfTables(a)) => a,
        Some(_) => bail!("{FILE}: each hook is a [[hook]] section"),
    };
    for (i, t) in tables.iter().enumerate() {
        let at = || format!("{FILE}: hook {}", i + 1);
        let text = |key: &str| -> Result<Option<String>> {
            t.get(key)
                .map(|v| v.as_str().map(str::to_owned).with_context(|| format!("{}: {key} must be text", at())))
                .transpose()
        };
        let on: Vec<String> = match t.get("on") {
            Some(v) if v.is_str() => vec![v.as_str().unwrap_or_default().to_string()],
            Some(_) => strings(t, "on").with_context(at)?.unwrap_or_default(),
            None => Vec::new(),
        };
        let mut kinds = Vec::new();
        for k in &on {
            let names: Vec<&str> = oneup_ipc::EventKind::ALL.iter().map(|k| k.name()).collect();
            kinds.push(
                oneup_ipc::EventKind::from_name(k)
                    .with_context(|| format!("{}: no event {k:?}; use {}", at(), names.join(", ")))?,
            );
        }
        let secs = |key: &str, default: Duration| -> Result<Duration> {
            Ok(number(t, key).with_context(at)?.map_or(default, Duration::from_secs))
        };
        let name = text("name")?.unwrap_or_default();
        let spec = HookSpec {
            name: name.trim().to_string(),
            on: kinds,
            paths: strings(t, "paths").with_context(at)?.unwrap_or_default(),
            program: text("program")?.unwrap_or_default(),
            args: strings(t, "args").with_context(at)?.unwrap_or_default(),
            timeout: secs("timeout_s", oneup_hooks::DEFAULT_TIMEOUT)?,
            per_minute: number(t, "per_minute")
                .with_context(at)?
                .map_or(oneup_hooks::DEFAULT_PER_MINUTE, |n| u32::try_from(n).unwrap_or(u32::MAX)),
            coalesce: secs("coalesce_s", oneup_hooks::DEFAULT_COALESCE)?,
            enabled: match t.get("enabled") {
                Some(v) => v.as_bool().with_context(|| format!("{}: enabled must be true or false", at()))?,
                None => true,
            },
        };
        if !spec.name.is_empty() && h.list.iter().any(|o| o.name.eq_ignore_ascii_case(&spec.name)) {
            bail!("{}: the name {:?} is used twice", at(), spec.name);
        }
        h.list.push(spec);
    }
    Ok(h)
}

/// Turns one hook on or off, by name, keeping everything else in the file.
pub fn set_hook_enabled(data_dir: &Path, name: &str, on: bool) -> Result<()> {
    let mut doc = read_doc(data_dir)?.with_context(|| format!("no hook called {name:?}"))?;
    let Some(Item::ArrayOfTables(tables)) = doc.get_mut("hook") else { bail!("no hook called {name:?}") };
    let t = tables
        .iter_mut()
        .find(|t| t.get("name").and_then(|v| v.as_str()).is_some_and(|n| n.trim().eq_ignore_ascii_case(name.trim())))
        .with_context(|| format!("no hook called {name:?}"))?;
    t["enabled"] = value(on);
    let p = path(data_dir);
    crate::ownfile::write(&p, doc.to_string().as_bytes()).with_context(|| format!("writing {}", p.display()))
}

/// Pauses or resumes every hook (`[hooks] paused`).
pub fn set_hooks_paused(data_dir: &Path, paused: bool) -> Result<()> {
    save(
        data_dir,
        "hooks",
        "# Hooks: programs OneUpDrive starts on sync events (each is a [[hook]] section).\n",
        vec![("paused", value(paused))],
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

    /// The agent reads [agent], [space] and [priority] every round: a
    /// mistake made while editing config.toml keeps the last good settings
    /// instead of failing the rounds. With nothing read before, it's an
    /// error, as for a one-off command.
    #[test]
    fn a_broken_file_keeps_the_last_good_settings() {
        let d = tmp("kept");
        std::fs::write(path(&d), "[agent]\nupload_kib_per_s = 300\n").unwrap();
        assert_eq!(kept(&d, load_agent(&d)).unwrap().upload_kib, 300);
        std::fs::write(path(&d), "[agent]\nupload_kib_per_s = -1\n").unwrap();
        assert_eq!(kept(&d, load_agent(&d)).unwrap().upload_kib, 300, "the last good");
        std::fs::write(path(&d), "[agent\n").unwrap();
        assert_eq!(kept(&d, load_agent(&d)).unwrap().upload_kib, 300, "unreadable");
        assert!(kept(&d, load_space(&d)).is_err(), "no good [space] read yet");
        std::fs::write(path(&d), "[agent]\nupload_kib_per_s = 5\n").unwrap();
        assert_eq!(kept(&d, load_agent(&d)).unwrap().upload_kib, 5, "a good file again");
        let other = tmp("kept-other");
        std::fs::write(path(&other), "[agent\n").unwrap();
        assert!(kept(&other, load_agent(&other)).is_err(), "kept per data folder");
        let _ = std::fs::remove_dir_all(&d);
        let _ = std::fs::remove_dir_all(&other);
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
            compact_database: false,
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
    fn busy_file_settings_round_trip_and_are_checked() {
        let d = tmp("busy");
        let busy = crate::busy::Settings {
            on: false,
            changes: 3,
            window: std::time::Duration::from_secs(10),
            quiet: std::time::Duration::from_secs(30),
            max: std::time::Duration::from_secs(120),
        };
        let a = Agent { busy, ..Agent::default() };
        save_agent(&d, &a).unwrap();
        assert_eq!(load_agent(&d).unwrap(), a);
        for bad in [
            "busy_changes = 1",
            "busy_changes = 101",
            "busy_quiet_s = 0",
            "busy_max_s = 86401",
            "busy_quiet_s = 60\nbusy_max_s = 30",
            "hold_busy_files = 1",
        ] {
            std::fs::write(path(&d), format!("[agent]\n{bad}\n")).unwrap();
            assert!(load_agent(&d).is_err(), "{bad}");
        }
        let _ = std::fs::remove_dir_all(&d);
    }

    /// The memory profile's numbers can be set one by one; what isn't set
    /// stays the profile's, and setting one back to the profile's removes
    /// its key.
    #[test]
    fn memory_overrides_and_hashing_round_trip_and_are_checked() {
        let d = tmp("overrides");
        let profile = Agent { memory: "minimal".into(), ..Agent::default() };
        let mut a = profile.clone();
        a.set_override("database_cache_mib", Some(200)).unwrap();
        a.set_override("operations_at_once", Some(12)).unwrap();
        a.set_override("upload_fragment_mib", Some(20)).unwrap();
        a.hash_files_at_once = 3;
        save_agent(&d, &a).unwrap();
        let read = load_agent(&d).unwrap();
        assert_eq!(read, a);
        assert_eq!(read.cache_kib(), 200 * 1024);
        assert_eq!(read.parallel(), oneup_exec::Parallel { ops: 12, large_uploads: 1, hashes: 3 });
        assert_eq!(read.upload_memory().fragment, 20 << 20);
        let tuned = read.tuned();
        let row = |key: &str| *tuned.iter().find(|t| t.knob.key == key).unwrap();
        assert_eq!((row("database_cache_mib").value, row("database_cache_mib").default), (200.0, 4.0));
        assert_eq!((row("operations_at_once").set, row("large_uploads_at_once").set), (Some(12), None));
        assert_eq!(row("upload_fragment_mib").default, 10.0, "minimal's 32 units of 320 KiB");
        assert_eq!((row("hash_files_at_once").value, row("hash_files_at_once").set), (3.0, Some(3)));

        // Back to the profile's: the key goes, the rest stays.
        let mut back = read.clone();
        back.set_override("database_cache_mib", None).unwrap();
        save_agent(&d, &back).unwrap();
        let text = std::fs::read_to_string(path(&d)).unwrap();
        assert!(!text.contains("database_cache_mib") && text.contains("operations_at_once = 12"), "{text}");
        assert_eq!(load_agent(&d).unwrap().cache_kib(), profile.cache_kib());
        assert_eq!(Agent::default().parallel(), oneup_exec::Parallel::for_profile("balanced"));

        for bad in [
            "database_cache_mib = 0",
            "database_cache_mib = 1025",
            "operations_at_once = 33",
            "large_uploads_at_once = 9",
            "upload_fragment_mib = 1",
            "upload_fragment_mib = 60",
            "hash_files_at_once = 0",
            "hash_files_at_once = 17",
            "operations_at_once = -1",
            "operations_at_once = \"8\"",
        ] {
            std::fs::write(path(&d), format!("[agent]\n{bad}\n")).unwrap();
            let e = load_agent(&d).expect_err(bad);
            let key = bad.split(' ').next().unwrap();
            assert!(format!("{e:#}").contains(key), "{bad}: {e:#}");
        }
        assert!(Agent::default().set_override("memory_profile", Some(1)).is_err());
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
    fn hooks_are_read_and_turned_on_and_off_in_place() {
        let d = tmp("hooks");
        assert_eq!(load_hooks(&d).unwrap(), Hooks::default());
        std::fs::write(
            path(&d),
            r#"[agent]
tray_icon = false

# my hook
[[hook]]
name = "thumbs"
on = ["synced", "conflict"]
paths = ["/Photos/"]
program = 'C:\Tools\thumb.exe'
args = ["{path}"]
timeout_s = 30

[[hook]]
name = "note"
on = "safety_stop"
program = "notify.exe"
"#,
        )
        .unwrap();
        let h = load_hooks(&d).unwrap();
        assert!(!h.paused);
        assert_eq!(h.list.len(), 2);
        let t = &h.list[0];
        assert_eq!((t.name.as_str(), t.program.as_str(), t.timeout.as_secs()), ("thumbs", r"C:\Tools\thumb.exe", 30));
        assert_eq!(t.on, [oneup_ipc::EventKind::Synced, oneup_ipc::EventKind::Conflict]);
        assert_eq!((t.paths.as_slice(), t.per_minute, t.enabled), (&["/Photos/".to_string()][..], 60, true));
        assert_eq!(h.list[1].on, [oneup_ipc::EventKind::SafetyStop]);

        set_hook_enabled(&d, "THUMBS", false).unwrap();
        set_hooks_paused(&d, true).unwrap();
        let h = load_hooks(&d).unwrap();
        assert!(h.paused && !h.list[0].enabled && h.list[1].enabled);
        let text = std::fs::read_to_string(path(&d)).unwrap();
        assert!(text.contains("# my hook") && text.contains("tray_icon = false"), "{text}");
        assert!(set_hook_enabled(&d, "nope", true).is_err());

        for bad in [
            "[[hook]]\nname = \"a\"\non = [\"deleted\"]\n",
            "[[hook]]\nname = \"a\"\ntimeout_s = -1\n",
            "[[hook]]\nname = \"a\"\n[[hook]]\nname = \"A\"\n",
            "hook = 1\n",
        ] {
            std::fs::write(path(&d), bad).unwrap();
            assert!(load_hooks(&d).is_err(), "{bad}");
        }
        let _ = std::fs::remove_dir_all(&d);
    }

    #[test]
    fn typed_rules_are_checked() {
        assert!(check_rule("*.iso").is_ok() && check_rule("!keep/").is_ok());
        assert!(check_rule("# note").is_err() && check_rule("  ").is_err() && check_rule("a\nb").is_err());
    }
}
