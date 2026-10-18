//! What the settings window (`oneup-settings`) reads and changes, as JSON:
//! the same settings the command line has (`config.toml`, the version
//! store's settings, start at sign-in), with the same checks. No Windows UI
//! here, so it's tested like the rest; the window's own code only moves
//! messages between the page and these functions (and the agent's control
//! pipe).

use std::path::Path;

use anyhow::{Context, Result, bail};
use oneup_gate::OverflowPolicy;
use serde_json::{Value, json};

use crate::config::{self, Agent, CloudCopies, Rules};

/// Everything the window shows, read fresh.
pub fn snapshot(dir: &Path) -> Result<Value> {
    let store = crate::setup::open_store(dir, 1024)?;
    let a = config::load_agent(dir)?;
    let r = config::load_rules(dir, &store)?;
    let trim = crate::versions::trim_settings(&store)?;
    let policy = crate::versions::policy(&store)?;
    let local = store.read(|o| o.meta_get(crate::sync::KEY_LOCAL))?;
    let remote = store.read(|o| o.meta_get(crate::sync::KEY_REMOTE))?;
    let token = store.read(|o| o.meta_get(crate::sync::KEY_TOKEN))?.unwrap_or_default();
    let on_demand = local.as_deref().is_some_and(|l| crate::ondemand::is_on(Path::new(l), &token));
    let autostart = crate::launch::autostart(crate::launch::RUN_KEY, dir).ok().flatten().is_some();
    let space = config::load_space(dir)?;
    let updates = config::load_updates(dir)?;
    let (first, last) = config::load_priority(dir)?;
    let vaults: Vec<Value> = crate::vaultpair::list(dir)
        .iter()
        .map(|e| {
            let (folder, od) = crate::vaultpair::describe(e).unwrap_or_default();
            json!({
                "name": e.name,
                "remote": e.remote,
                "local": shown(&folder.to_string_lossy()),
                "on_demand": od,
                "remembered": e.dir.join(crate::vaultpair::KEY_FILE).exists(),
                "hello": e.dir.join(crate::vaultpair::HELLO_FILE).exists(),
            })
        })
        .collect();
    let presets: Vec<Value> = oneup_rules::PRESETS
        .iter()
        .map(|p| json!({ "name": p.name, "summary": p.summary, "on": r.presets.iter().any(|x| x == p.name) }))
        .collect();
    // In a pre-release build, setting a pair up is for test accounts only.
    let setup = match crate::setup::require_test_account(dir) {
        Ok(()) => json!({ "allowed": true }),
        Err(e) => json!({ "allowed": false, "why": format!("{e:#}") }),
    };
    let app = crate::moveover::app();
    let signed_as = crate::accounts::signed_in_as(dir);
    Ok(json!({
        "version": env!("CARGO_PKG_VERSION"),
        "data_dir": shown(&dir.to_string_lossy()),
        "test_account": crate::test_account::load(dir)?.is_some(),
        // The stored sign-in only; nothing is sent anywhere to find out.
        "signed_in": dir.join("token.dpapi").exists(),
        "setup": setup,
        // A folder of its own, apart from the OneDrive app's (`%USERPROFILE%\OneDrive`).
        "whole_drive": crate::sync::WHOLE_DRIVE_ALLOWED,
        "suggested_local": std::env::var_os("USERPROFILE").map(|h| Path::new(&h).join("OneUpDrive").to_string_lossy().into_owned()),
        // Setup offers moving this folder over instead.
        "app_folder": app.as_ref().map(crate::moveover::App::to_json),
        // Which account signed in (as `oneup login` noted it), and whether
        // it's the one the OneDrive app syncs (null: can't tell).
        "signed_in_as": signed_as.as_ref().map(|s| s.name.clone()),
        "same_account_as_app": match (&app, &signed_as) {
            (Some(a), Some(s)) if a.drive.is_some() => Some(!crate::moveover::other_account(a, Some(&s.drive))),
            _ => None,
        },
        "carried": crate::moveover::carried_waiting(dir),
        "pair": {
            "local": local.as_deref().map(shown),
            "remote": remote,
            "on_demand": on_demand,
            "backup": crate::flow::get(&store)? == crate::flow::Flow::Backup,
        },
        "agent": {
            "upload_kib": a.upload_kib,
            "download_kib": a.download_kib,
            "memory": a.memory,
            "background": a.background,
            "pause_on_metered": a.pause_on_metered,
            "pause_on_battery_saver": a.pause_on_battery_saver,
            "pause_on_full_screen": a.pause_on_full_screen,
            "pause_on_full_screen_video": a.pause_on_full_screen_video,
            "quiet_hours": a.quiet_hours,
            "cap_hours": a.cap_hours,
            "tray_icon": a.tray_icon,
            "track_apps": a.track_apps,
            "hold_busy_files": a.busy.on,
            "busy_quiet_s": a.busy.quiet.as_secs(),
        },
        "autostart": autostart,
        "rules": {
            "presets": presets,
            "exclude": r.exclude,
            "paused": r.paused,
            "cloud": r.cloud.name(),
        },
        "versions": {
            "keep_replaced": policy.keep_replaced,
            "overflow": if policy.overflow == OverflowPolicy::Keep { "keep" } else { "hold" },
            "rotate": trim.rotate,
            "min_days": trim.min_age_days,
            "max_gib": trim.max_gib,
            "max_percent": trim.max_percent,
        },
        "vaults": vaults,
        "hello_available": oneup_sys::hello::available(),
        "space": {
            "free_unused_days": space.free_unused_days,
            "free_when_low": space.free_when_low,
            "low_disk_percent": space.low_disk_percent,
        },
        "safety": {
            "max_changes": policy.max_count,
            "max_percent": policy.max_percent,
            "content_guard": policy.max_suspicious,
        },
        "updates": { "auto": updates.auto, "channel": updates.channel },
        "onedrive_app": {
            "folders": config::load_onedrive_app(dir)?.folders,
            "top_files": config::load_onedrive_app(dir)?.top_files,
        },
        "office_integration": crate::officeint::is_on(dir),
        "experimental": config::load_experimental(dir)?,
        "office_opener": { "registered": crate::officeopen::is_registered(), "defaults": crate::officeopen::defaults() },
        "priority": { "first": first, "last": last },
        "kept_folder": local.as_deref().map(|l| shown(&Path::new(l).join(oneup_rules::ENGINE_DIR).join("versions").to_string_lossy())),
    }))
}

/// The extra folder pairs (`crate::pairs`), as the window lists them
/// (whether each runs is the window's to ask).
pub fn pairs_json(dir: &Path) -> Result<Value> {
    let mut out = Vec::new();
    for e in crate::pairs::list(dir) {
        let d = crate::pairs::describe(&e)?;
        let known = match &d.kind {
            crate::pairs::Kind::Known(k) => Some(k.name()),
            _ => None,
        };
        out.push(json!({
            "name": e.name,
            "remote": e.remote,
            "local": shown(&d.local.to_string_lossy()),
            "kind": d.kind.label(),
            "shared": matches!(d.kind, crate::pairs::Kind::Shared { .. }),
            "known": known,
            "backup": d.flow == crate::flow::Flow::Backup,
            "on_demand": d.on_demand,
        }));
    }
    Ok(Value::from(out))
}

/// Windows' own folders for PC folder backup: where each is, and whether
/// it's backed up already (by a pair here, as part of the main synced
/// folder, or by another app such as OneDrive).
pub fn known_json(dir: &Path) -> Result<Value> {
    let store = crate::setup::open_store(dir, 1024)?;
    let main_local = store.read(|o| o.meta_get(crate::sync::KEY_LOCAL))?.map(std::path::PathBuf::from);
    let pairs: Vec<(crate::pairs::Entry, crate::pairs::Described)> =
        crate::pairs::list(dir).into_iter().filter_map(|e| crate::pairs::describe(&e).ok().map(|d| (e, d))).collect();
    let mut out = Vec::new();
    for k in oneup_sys::known::Known::ALL {
        let Ok(path) = k.path() else { continue };
        let by_pair = pairs.iter().find(|(_, d)| d.kind == crate::pairs::Kind::Known(k)).map(|(e, _)| e.name.clone());
        let in_main = main_local.as_ref().is_some_and(|m| {
            let m = std::fs::canonicalize(m).unwrap_or_else(|_| m.clone());
            oneup_local::coexist::is_within(&std::fs::canonicalize(&path).unwrap_or_else(|_| path.clone()), &m)
        });
        let other = oneup_cfapi::other_sync_root(&path).map(|i| i.provider_name);
        out.push(json!({
            "name": k.name(),
            "path": shown(&path.to_string_lossy()),
            "pair": by_pair,
            "in_main": in_main,
            "other": other,
        }));
    }
    Ok(Value::from(out))
}

/// The accounts on this PC (`crate::accounts`), `current` first.
pub fn accounts_json(current: &Path) -> Value {
    let mut all = crate::accounts::list();
    all.sort_by_key(|a| !crate::test_account::same_dir(&a.dir, current));
    Value::from(
        all.into_iter()
            .map(|a| {
                json!({
                    "name": a.name,
                    "dir": shown(&a.dir.to_string_lossy()),
                    "signed_in": a.signed_in,
                    "test_account": a.test_account,
                    "current": crate::test_account::same_dir(&a.dir, current),
                })
            })
            .collect::<Vec<_>>(),
    )
}

/// `\\?\C:\x` → `C:\x`, as people know paths.
fn shown(p: &str) -> String {
    p.strip_prefix(r"\\?\").unwrap_or(p).to_string()
}

/// One of the largest files in OneDrive ([`largest`]).
#[derive(Debug, Clone)]
pub struct Large {
    /// Its path in OneDrive (`/Videos/trip.mp4`).
    pub path: String,
    pub size: i64,
    /// Where it is on this PC, if it's in the synced folder.
    pub local: Option<std::path::PathBuf>,
    pub state: LargeState,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum LargeState {
    /// Its content is on this PC.
    Here,
    /// On this PC as an online-only file.
    OnlineOnly,
    /// In the synced folder, but not on this PC (yet).
    Missing,
    /// Outside the synced folder.
    NotSynced,
}

impl LargeState {
    pub fn name(self) -> &'static str {
        match self {
            LargeState::Here => "here",
            LargeState::OnlineOnly => "online-only",
            LargeState::Missing => "not here yet",
            LargeState::NotSynced => "not synced",
        }
    }
}

/// The `n` largest files in OneDrive (as far as the local mirror lists
/// them), largest first, and where each is on this PC. Reads only the
/// database and file attributes; never a file's content.
pub fn largest(dir: &Path, n: usize) -> Result<Vec<Large>> {
    // Attributes must show placeholders as they are (AGENTS.md).
    oneup_cfapi::expose_placeholders()?;
    let store = crate::setup::open_store(dir, 1024)?;
    let local = store.read(|o| o.meta_get(crate::sync::KEY_LOCAL))?;
    let remote = store.read(|o| o.meta_get(crate::sync::KEY_REMOTE))?;
    let rows = store.read(|o| {
        o.remote_largest(n)?
            .into_iter()
            .map(|r| Ok((o.path_of(&r.id)?.unwrap_or(r.name), r.size)))
            .collect::<oneup_store::Result<Vec<_>>>()
    })?;
    Ok(rows
        .into_iter()
        .map(|(path, size)| {
            let inside = match (&local, &remote) {
                (Some(l), Some(r)) => {
                    oneup_sync::strip_folded_prefix(&path, r).filter(|rest| rest.starts_with('/')).map(|rest| {
                        rest.split('/').filter(|p| !p.is_empty()).fold(std::path::PathBuf::from(l), |p, x| p.join(x))
                    })
                }
                _ => None,
            };
            let state = match &inside {
                None => LargeState::NotSynced,
                Some(p) => match std::fs::metadata(p) {
                    Ok(m) if std::os::windows::fs::MetadataExt::file_attributes(&m) & 0x0040_0000 != 0 => {
                        LargeState::OnlineOnly
                    }
                    Ok(_) => LargeState::Here,
                    Err(_) => LargeState::Missing,
                },
            };
            Large { path, size, local: inside.map(|p| std::path::PathBuf::from(shown(&p.to_string_lossy()))), state }
        })
        .collect())
}

/// [`largest`] as the window shows it.
pub fn largest_json(dir: &Path, n: usize) -> Result<Value> {
    Ok(Value::from(
        largest(dir, n)?
            .iter()
            .map(|l| {
                json!({
                    "path": l.path,
                    "size": l.size,
                    "local": l.local.as_ref().map(|p| p.to_string_lossy().into_owned()),
                    "state": l.state.name(),
                })
            })
            .collect::<Vec<_>>(),
    ))
}

/// Whether the data folder syncs nothing yet (the window's "Set up" page).
pub fn needs_setup(dir: &Path) -> bool {
    !dir.join("state.db").exists()
        || crate::setup::open_store(dir, 1024)
            .and_then(|s| Ok(s.read(|o| o.meta_get(crate::sync::KEY_LOCAL))?))
            .is_ok_and(|l| l.is_none())
}

fn get<'a>(v: &'a Value, k: &str) -> Option<&'a Value> {
    v.get(k).filter(|x| !x.is_null())
}

fn hours(v: &Value, k: &str) -> Result<Option<Option<String>>> {
    match v.get(k) {
        None => Ok(None),
        Some(Value::Null) => Ok(Some(None)),
        Some(Value::String(s)) if s.trim().is_empty() => Ok(Some(None)),
        Some(Value::String(s)) => {
            crate::autopause::parse_quiet_hours(s.trim())?;
            Ok(Some(Some(s.trim().to_string())))
        }
        Some(_) => bail!("{k}: expected HH:MM-HH:MM"),
    }
}

/// The `[agent]` settings changed by the fields present in `v`.
pub fn agent_with(mut a: Agent, v: &Value) -> Result<Agent> {
    let num =
        |k: &str| get(v, k).map(|x| x.as_u64().with_context(|| format!("{k}: expected a whole number"))).transpose();
    let flag = |k: &str| get(v, k).map(|x| x.as_bool().with_context(|| format!("{k}: expected on or off"))).transpose();
    if let Some(n) = num("upload_kib")? {
        a.upload_kib = n;
    }
    if let Some(n) = num("download_kib")? {
        a.download_kib = n;
    }
    if let Some(m) = get(v, "memory") {
        let m = m.as_str().unwrap_or_default();
        if !config::MEMORY_PROFILES.iter().any(|(n, _)| *n == m) {
            bail!("memory: minimal, balanced or performance");
        }
        a.memory = m.to_string();
    }
    for (k, f) in [
        ("background", &mut a.background),
        ("pause_on_metered", &mut a.pause_on_metered),
        ("pause_on_battery_saver", &mut a.pause_on_battery_saver),
        ("pause_on_full_screen", &mut a.pause_on_full_screen),
        ("pause_on_full_screen_video", &mut a.pause_on_full_screen_video),
        ("tray_icon", &mut a.tray_icon),
        ("track_apps", &mut a.track_apps),
        ("hold_busy_files", &mut a.busy.on),
    ] {
        if let Some(b) = flag(k)? {
            *f = b;
        }
    }
    if let Some(n) = num("busy_quiet_s")? {
        a.busy.quiet = std::time::Duration::from_secs(n);
        // Raising the wait past the longest one raises that too.
        a.busy.max = a.busy.max.max(a.busy.quiet);
    }
    config::check_busy(&a.busy)?;
    if let Some(q) = hours(v, "quiet_hours")? {
        a.quiet_hours = q;
    }
    if let Some(q) = hours(v, "cap_hours")? {
        a.cap_hours = q;
    }
    Ok(a)
}

/// The `[rules]` changed by the fields present in `v` (each rule checked).
pub fn rules_with(mut r: Rules, v: &Value) -> Result<Rules> {
    let list = |k: &str| -> Result<Option<Vec<String>>> {
        get(v, k)
            .map(|x| {
                x.as_array()
                    .with_context(|| format!("{k}: expected a list"))?
                    .iter()
                    .map(|s| s.as_str().map(str::to_string).with_context(|| format!("{k}: expected text")))
                    .collect()
            })
            .transpose()
    };
    if let Some(p) = list("presets")? {
        for name in &p {
            if oneup_rules::preset(name).is_none() {
                bail!("no preset called {name}");
            }
        }
        r.presets = p;
    }
    if let Some(lines) = list("exclude")? {
        let lines: Vec<String> = lines.into_iter().map(|l| l.trim().to_string()).filter(|l| !l.is_empty()).collect();
        for l in &lines {
            config::check_rule(l)?;
        }
        r.exclude = lines;
    }
    if let Some(c) = get(v, "cloud") {
        r.cloud = CloudCopies::from_name(c.as_str().unwrap_or_default()).context("cloud: keep, prune or pull")?;
    }
    if let Some(p) = list("paused")? {
        // Folders inside the synced folder, `/`-separated, as `oneup pause --folder` saves them.
        r.paused = p
            .into_iter()
            .map(|f| f.trim().trim_matches(['/', '\\']).replace('\\', "/"))
            .filter(|f| !f.is_empty())
            .collect();
    }
    Ok(r)
}

/// The `[space]` changed by the fields present in `v`.
pub fn space_with(mut s: config::Space, v: &Value) -> Result<config::Space> {
    if let Some(d) = get(v, "free_unused_days") {
        s.free_unused_days = d
            .as_u64()
            .and_then(|d| u32::try_from(d).ok())
            .filter(|d| *d <= 3650)
            .context("free_unused_days: 0 (never) to 3650 days")?;
    }
    if let Some(b) = get(v, "free_when_low") {
        s.free_when_low = b.as_bool().context("free_when_low: on or off")?;
    }
    if let Some(p) = get(v, "low_disk_percent") {
        s.low_disk_percent = p
            .as_u64()
            .and_then(|p| u32::try_from(p).ok())
            .filter(|p| (1..=50).contains(p))
            .context("low_disk_percent: 1 to 50")?;
    }
    Ok(s)
}

/// The safety-stop changes in `v` (`crate::versions::Safety`).
pub fn safety_changes(v: &Value) -> Result<crate::versions::Safety> {
    let num = |k: &str| {
        get(v, k)
            .map(|x| x.as_u64().and_then(|n| usize::try_from(n).ok()).with_context(|| format!("{k}: a whole number")))
            .transpose()
    };
    Ok(crate::versions::Safety {
        max_changes: num("max_changes")?,
        max_percent: num("max_percent")?.map(|n| u32::try_from(n).unwrap_or(u32::MAX)),
        content_guard: num("content_guard")?,
    })
}

/// The `[updates]` changed by the fields present in `v`.
pub fn updates_with(mut u: config::Updates, v: &Value) -> Result<config::Updates> {
    if let Some(b) = get(v, "auto") {
        u.auto = b.as_bool().context("auto: on or off")?;
    }
    if let Some(c) = get(v, "channel") {
        let c = c.as_str().unwrap_or_default();
        if c != "stable" && c != "preview" {
            bail!("channel: stable or preview");
        }
        u.channel = c.to_string();
    }
    Ok(u)
}

/// The version-store changes in `v`.
pub fn versions_changes(v: &Value) -> Result<crate::versions::Changes> {
    let flag = |k: &str| get(v, k).map(|x| x.as_bool().with_context(|| format!("{k}: expected on or off"))).transpose();
    let float = |k: &str| get(v, k).map(|x| x.as_f64().with_context(|| format!("{k}: expected a number"))).transpose();
    Ok(crate::versions::Changes {
        min_days: get(v, "min_days")
            .map(|x| x.as_u64().and_then(|n| u32::try_from(n).ok()).context("min_days: expected a whole number"))
            .transpose()?,
        max_gib: float("max_gib")?,
        max_percent: float("max_percent")?,
        rotate: flag("rotate")?,
        overflow: get(v, "overflow")
            .map(|x| match x.as_str() {
                Some("keep") => Ok(OverflowPolicy::Keep),
                Some("hold") => Ok(OverflowPolicy::Hold),
                _ => bail!("overflow: hold or keep"),
            })
            .transpose()?,
        keep_replaced: flag("keep_replaced")?,
    })
}

/// The first-run setup's choices (the window's "Set up" page), checked as
/// `oneup sync` would; the window runs `oneup` with [`Setup::preview`],
/// then [`Setup::pair`], then starts the agent.
#[derive(Debug, PartialEq, Eq)]
pub struct Setup {
    pub local: std::path::PathBuf,
    /// `/A/B`; the whole drive only once [`crate::sync::WHOLE_DRIVE_ALLOWED`].
    pub remote: String,
    /// `none` or `dev`.
    pub preset: String,
    pub on_demand: bool,
}

/// How many changes the preview lists (the totals cover the rest).
pub const PREVIEW_LINES: usize = 200;

impl Setup {
    pub fn from_json(v: &Value) -> Result<Self> {
        let local = v.get("local").and_then(Value::as_str).map(str::trim).unwrap_or_default();
        if local.is_empty() {
            bail!("choose a folder on this PC");
        }
        let local = std::path::PathBuf::from(local);
        if !local.is_absolute() {
            bail!("choose a folder on this PC (a full path, like C:\\Users\\you\\OneUpDrive)");
        }
        // A new folder (the suggested one) is made, if its parent is there.
        if !local.exists() && local.parent().is_some_and(Path::is_dir) {
            std::fs::create_dir(&local).with_context(|| format!("creating {}", local.display()))?;
        }
        if !local.is_dir() {
            bail!("{} isn't a folder", local.display());
        }
        let remote = crate::sync::normalize_remote(v.get("remote").and_then(Value::as_str).unwrap_or_default())?;
        let preset = v.get("preset").and_then(Value::as_str).unwrap_or("dev");
        if crate::setup::preset_rules(preset).is_none() {
            bail!("unknown list of things to leave out: {preset}");
        }
        let on_demand = v.get("on_demand").and_then(Value::as_bool).unwrap_or(true);
        Ok(Setup { local, remote, preset: preset.to_string(), on_demand })
    }

    fn sync(&self, dir: &Path) -> Vec<std::ffi::OsString> {
        let mut a: Vec<std::ffi::OsString> = vec!["--data-dir".into(), dir.into(), "sync".into()];
        a.push(self.local.clone().into());
        a.extend(["--remote".into(), self.remote.clone().into(), "--exclude".into(), self.preset.clone().into()]);
        a
    }

    /// `oneup` arguments for the preview (changes nothing).
    pub fn preview(&self, dir: &Path) -> Vec<std::ffi::OsString> {
        let mut a = self.sync(dir);
        a.extend(["--dry-run".into(), "--limit".into(), PREVIEW_LINES.to_string().into()]);
        a
    }

    /// `oneup` arguments that set the pair up for the agent.
    pub fn pair(&self, dir: &Path) -> Vec<std::ffi::OsString> {
        let mut a = self.sync(dir);
        a.push("--pair-only".into());
        if self.on_demand {
            a.push("--on-demand".into());
        }
        a
    }
}

/// What sync did with `rel` in the data folder's synced folder (everything
/// if empty), newest first, as `oneup activity` shows it.
pub fn activity(dir: &Path, rel: &str, limit: usize) -> Result<Value> {
    use oneup_store::ActivityOutcome as O;
    let store = crate::setup::open_store(dir, 1024)?;
    let rel = rel.trim_matches('/');
    let path = (!rel.is_empty()).then(|| format!("/{rel}"));
    let list = store.read(|o| o.activity_recent(path.as_deref(), limit))?;
    Ok(list
        .into_iter()
        .map(|e| {
            let outcome = match e.outcome {
                O::Done => "done",
                O::Skipped => "skipped",
                O::Failed => "failed",
                O::Waiting => "waiting",
                O::Note => "note",
            };
            json!({ "at_ms": e.at_ms, "outcome": outcome, "action": e.action, "path": e.path, "detail": e.detail })
        })
        .collect())
}

/// What can't sync until the user changes it, as `oneup unsynced` lists it,
/// with what can be done about each (`crate::resolve`): `choices`, and the
/// answer that waits for the agent's next round, if one does. `nested`: a
/// folder above it is listed for the same reason, and answers for it.
pub fn unsynced(dir: &Path) -> Result<Value> {
    use crate::resolve::{Choice, choices};
    let store = crate::setup::open_store(dir, 1024)?;
    let r = crate::unsynced::load(&store)?;
    let backup = crate::flow::get(&store)? == crate::flow::Flow::Backup;
    let queued = store.read(crate::resolve::queued)?;
    let local = store.read(|o| o.meta_get(crate::sync::KEY_LOCAL))?.map(std::path::PathBuf::from);
    let tops: Vec<(String, crate::unsynced::Why)> =
        r.items.iter().filter(|i| i.why.held()).map(|i| (format!("{}/", i.path.to_lowercase()), i.why)).collect();
    let items: Vec<Value> = r
        .items
        .iter()
        .map(|i| {
            let lower = i.path.to_lowercase();
            let nested = i.why.held() && tops.iter().any(|(t, w)| *w == i.why && lower.starts_with(t.as_str()));
            let choices: Vec<Value> = choices(i.why, backup)
                .iter()
                // Bringing an item back into a folder that's gone too waits
                // for that folder's answer.
                .filter(|c| !(nested && matches!(c, Choice::Restore | Choice::Upload)))
                .map(|c| json!({ "code": c.code(), "label": c.label(), "deletes": c.deletes(), "bulk": c.in_bulk() }))
                .collect();
            let here = local
                .as_ref()
                .is_some_and(|l| std::fs::symlink_metadata(l.join(i.path.trim_start_matches('/'))).is_ok());
            let waits = queued.iter().find(|q| q.path.eq_ignore_ascii_case(&i.path)).map(|q| q.choice.label());
            json!({ "path": i.path, "why": i.why.title(), "detail": i.why.label(), "kind": i.why.code(),
                "choices": choices, "nested": nested, "here": here, "queued": waits })
        })
        .collect();
    Ok(json!({ "items": items, "more": r.more, "backup": backup }))
}

/// What's inside a folder held as not empty (`crate::resolve::inside`).
pub fn unsynced_inside(dir: &Path, path: &str) -> Result<Value> {
    let store = crate::setup::open_store(dir, 1024)?;
    let c = store.read(|o| crate::resolve::inside(o, path, 200))?;
    let items: Vec<Value> =
        c.items.iter().map(|k| json!({ "name": k.name, "folder": k.folder, "excluded": k.excluded })).collect();
    Ok(json!({ "side": c.side, "total": c.total, "items": items }))
}

/// What a choice would do, before the window asks (`resolve::prepare`, as
/// `oneup unsynced resolve` works it out): how many items it concerns,
/// folders' contents included, and the items refused now. `path`: one
/// listed item; `None`: every item the choice fits.
pub fn unsynced_preview(dir: &Path, path: Option<&str>, choice: &str, name: Option<&str>) -> Result<Value> {
    let choice = crate::resolve::Choice::from_code(choice).with_context(|| format!("unknown choice: {choice}"))?;
    let store = crate::setup::open_store(dir, 1024)?;
    let backup = crate::flow::get(&store)? == crate::flow::Flow::Backup;
    let record = crate::unsynced::load(&store)?;
    let paths = path.map(|p| vec![p.to_string()]);
    let items = crate::resolve::select(&record, Some(choice), paths.as_deref(), backup)?;
    let p = crate::resolve::prepare(&store, &items, choice, name, backup)?;
    let tops: Vec<&str> = p.asked.iter().map(|r| r.path.as_str()).collect();
    Ok(json!({ "items": p.items, "tops": tops, "refused": p.refused, "code": p.code }))
}

/// The lists of the safety stops standing now (`crate::stoplist`), each
/// with up to `limit` of its lines matching `filter`.
pub fn stops(dir: &Path, filter: &str, limit: usize) -> Result<Value> {
    let mut codes: Vec<String> = std::fs::read_dir(dir.join(crate::stoplist::DIR))
        .into_iter()
        .flatten()
        .flatten()
        .filter_map(|e| e.file_name().to_str()?.strip_suffix(".txt").map(str::to_string))
        .collect();
    codes.sort();
    Ok(codes
        .iter()
        .filter_map(|code| {
            let l = crate::stoplist::read(dir, code, filter, limit)?;
            let rows: Vec<Value> = l
                .rows
                .iter()
                .map(|r| json!({ "action": r.action, "path": r.path, "to": r.to, "note": r.note }))
                .collect();
            Some(json!({ "code": code, "header": l.header, "rows": rows, "matching": l.matching,
                "unwritten": l.unwritten }))
        })
        .collect())
}

/// What's in OneDrive but excluded by the rules, as `oneup cloud prune`
/// lists it (without removing anything). Asks OneDrive how much is in the
/// folders it never listed.
pub async fn cloud_copies(dir: &Path, graph: &oneup_graph::Graph) -> Result<Value> {
    use crate::prune;
    let (found, kept, setting) = {
        let store = crate::setup::open_store(dir, 1024)?;
        let setting = config::load_rules(dir, &store)?.cloud;
        let (rules, _) = crate::rulefiles::load(dir, &store, &mut Default::default())?;
        let all = prune::candidates(&store, &rules, prune::Scope::All, setting != CloudCopies::Keep)?;
        let n = all.len();
        let found = prune::without_kept(&store, all)?;
        let kept = n - found.len();
        (found, kept, setting)
    };
    let found = prune::counted(graph, found).await?;
    let items: Vec<Value> = found
        .iter()
        .map(|c| {
            json!({ "path": c.path, "folder": c.folder, "items": c.items, "unlisted": c.unlisted,
                // A folder OneDrive hasn't sized yet reports 0: unknown.
                "bytes": (!c.folder || c.size > 0).then_some(c.size.max(0)),
                "missing": c.fetch.len() as u64 + c.waiting, "here": c.ready() })
        })
        .collect();
    Ok(json!({ "items": items, "kept": kept, "setting": setting.name() }))
}

/// What sync kept on this PC for the data folder's synced folder (files it
/// removed or replaced, in its version store), newest first.
pub fn kept(dir: &Path) -> Result<Value> {
    let store = crate::setup::open_store(dir, 1024)?;
    let mut slots = crate::sync::open_paired(&store)?.versions()?;
    slots.sort_by_key(|s| std::cmp::Reverse(s.at_ms));
    Ok(slots
        .into_iter()
        .map(|s| {
            json!({ "id": s.id, "at_ms": s.at_ms, "name": s.name, "origin": s.origin, "folder": s.is_dir, "bytes": s.bytes })
        })
        .collect())
}

/// Puts a kept item back where it was (never over anything: a taken name
/// gets " (restored N)"); returns where. Run with the agent stopped.
pub fn kept_restore(dir: &Path, id: &str) -> Result<String> {
    let _lock = crate::setup::DirLock::acquire(dir).context("OneUpDrive is still syncing this folder")?;
    let store = crate::setup::open_store(dir, 1024)?;
    Ok(crate::sync::open_paired(&store)?.restore(id, None)?)
}

/// Saves one section the window changed.
pub fn save(dir: &Path, section: &str, v: &Value) -> Result<()> {
    match section {
        "agent" => {
            let a = agent_with(config::load_agent(dir)?, v)?;
            config::save_agent(dir, &a)
        }
        "rules" => {
            let store = crate::setup::open_store(dir, 1024)?;
            let r = rules_with(config::load_rules(dir, &store)?, v)?;
            config::save_rules(dir, &r)
        }
        "versions" => {
            let mut store = crate::setup::open_store(dir, 1024)?;
            crate::versions::set(&mut store, &versions_changes(v)?)
        }
        "space" => {
            let s = space_with(config::load_space(dir)?, v)?;
            config::save_space(dir, &s)
        }
        "safety" => {
            let mut store = crate::setup::open_store(dir, 1024)?;
            crate::versions::set_safety(&mut store, &safety_changes(v)?)
        }
        "updates" => {
            let u = updates_with(config::load_updates(dir)?, v)?;
            config::save_updates(dir, &u)
        }
        "priority" => {
            let (mut first, mut last) = config::load_priority(dir)?;
            let lines = |k: &str| -> Result<Option<Vec<String>>> {
                get(v, k)
                    .map(|x| {
                        let all = x.as_array().with_context(|| format!("{k}: expected a list"))?;
                        let mut out = Vec::new();
                        for l in all {
                            let l = l.as_str().unwrap_or_default().trim();
                            if !l.is_empty() {
                                config::check_rule(l)?;
                                out.push(l.to_string());
                            }
                        }
                        Ok(out)
                    })
                    .transpose()
            };
            if let Some(f) = lines("first")? {
                first = f;
            }
            if let Some(l) = lines("last")? {
                last = l;
            }
            config::save_priority(dir, &first, &last)
        }
        "onedrive_app" => {
            let mut a = config::load_onedrive_app(dir)?;
            if let Some(list) = get(v, "folders") {
                let list = list.as_array().context("folders: expected a list")?;
                a.folders = list
                    .iter()
                    .filter_map(|f| f.as_str())
                    .map(|f| f.trim().trim_matches(['/', '\\']).replace('\\', "/"))
                    .filter(|f| !f.is_empty())
                    .map(|f| format!("/{f}"))
                    .collect();
            }
            if let Some(t) = get(v, "top_files") {
                a.top_files = t.as_bool().context("top_files: on or off")?;
            }
            config::save_onedrive_app(dir, &a)
        }
        "autostart" => {
            let on = v.as_bool().context("autostart: expected on or off")?;
            let launcher = if on { Some(crate::launch::sibling("oneupd-start.exe")?) } else { None };
            crate::launch::set_autostart(crate::launch::RUN_KEY, dir, launcher.as_deref()).map(|_| ())
        }
        other => bail!("nothing called {other} to save"),
    }
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used)]
    use super::*;

    #[test]
    fn agent_changes_are_checked_and_partial() {
        let a = agent_with(Agent::default(), &json!({ "upload_kib": 500, "quiet_hours": "22:00-07:00" })).unwrap();
        assert_eq!((a.upload_kib, a.quiet_hours.as_deref(), a.memory.as_str()), (500, Some("22:00-07:00"), "balanced"));
        let a = agent_with(a, &json!({ "quiet_hours": "", "background": false })).unwrap();
        assert_eq!((a.quiet_hours, a.background), (None, false));
        assert!(agent_with(Agent::default(), &json!({ "memory": "huge" })).is_err());
        assert!(agent_with(Agent::default(), &json!({ "cap_hours": "25:00-26:00" })).is_err());
        assert!(agent_with(Agent::default(), &json!({ "upload_kib": -3 })).is_err());
        let a = agent_with(Agent::default(), &json!({ "hold_busy_files": false, "busy_quiet_s": 900 })).unwrap();
        assert!(!a.busy.on);
        assert_eq!(a.busy.quiet.as_secs(), 900);
        assert_eq!(a.busy.max.as_secs(), 900, "the longest wait follows a longer quiet time");
        assert!(agent_with(Agent::default(), &json!({ "busy_quiet_s": 0 })).is_err());
    }

    #[test]
    fn rule_changes_are_checked() {
        let r = rules_with(
            Rules::default(),
            &json!({ "presets": ["dev"], "exclude": ["*.iso", "  ", "build/"], "cloud": "pull" }),
        )
        .unwrap();
        assert_eq!(
            (r.presets, r.exclude, r.cloud),
            (vec!["dev".to_string()], vec!["*.iso".into(), "build/".into()], CloudCopies::Pull)
        );
        assert!(rules_with(Rules::default(), &json!({ "presets": ["nope"] })).is_err());
        assert!(rules_with(Rules::default(), &json!({ "cloud": "delete" })).is_err());
        assert!(rules_with(Rules::default(), &json!({ "exclude": ["# just a comment"] })).is_err());
    }

    #[test]
    fn setup_choices_are_checked_and_become_sync_arguments() {
        let here = std::env::temp_dir();
        let v =
            json!({ "local": here.to_string_lossy(), "remote": "Docs\\Work/", "preset": "none", "on_demand": true });
        let s = Setup::from_json(&v).unwrap();
        assert_eq!((s.remote.as_str(), s.preset.as_str(), s.on_demand), ("/Docs/Work", "none", true));
        let dir = Path::new("D");
        let text = |a: Vec<std::ffi::OsString>| a.iter().map(|x| x.to_string_lossy().into_owned()).collect::<Vec<_>>();
        let p = text(s.preview(dir));
        assert_eq!(&p[..3], ["--data-dir", "D", "sync"]);
        assert!(p.ends_with(&["--dry-run".into(), "--limit".into(), PREVIEW_LINES.to_string()]));
        let p = text(s.pair(dir));
        assert!(p.ends_with(&["--pair-only".into(), "--on-demand".into()]));
        assert!(!p.contains(&"--dry-run".to_string()));

        let bad = |v: Value| Setup::from_json(&v).is_err();
        assert!(bad(json!({ "local": "", "remote": "/A" })));
        assert!(bad(json!({ "local": "relative\\x", "remote": "/A" })));
        let deep = here.join("does-not-exist-oneup").join("nor-this");
        assert!(bad(json!({ "local": deep.to_string_lossy(), "remote": "/A" })), "only a folder whose parent is there");
        // A new folder (the suggested one) is made.
        let new = here.join(format!("oneup-setup-new-{}", std::process::id()));
        assert!(!bad(json!({ "local": new.to_string_lossy(), "remote": "/A" })));
        assert!(new.is_dir());
        std::fs::remove_dir(&new).unwrap();
        let whole = json!({ "local": here.to_string_lossy(), "remote": "/" });
        assert_eq!(bad(whole), !crate::sync::WHOLE_DRIVE_ALLOWED, "the whole drive, once released");
        assert!(bad(json!({ "local": here.to_string_lossy(), "remote": "/A", "preset": "most" })));
    }

    #[test]
    fn space_safety_and_update_changes_are_checked() {
        let s =
            space_with(config::Space::default(), &json!({ "free_unused_days": 30, "free_when_low": true })).unwrap();
        assert_eq!((s.free_unused_days, s.free_when_low, s.low_disk_percent), (30, true, 10));
        assert!(space_with(config::Space::default(), &json!({ "low_disk_percent": 80 })).is_err());
        let c = safety_changes(&json!({ "content_guard": 0, "max_changes": 500 })).unwrap();
        assert_eq!((c.content_guard, c.max_changes, c.max_percent), (Some(0), Some(500), None));
        assert!(safety_changes(&json!({ "max_changes": -1 })).is_err());
        let u = updates_with(config::Updates::default(), &json!({ "channel": "preview", "auto": false })).unwrap();
        assert_eq!((u.channel.as_str(), u.auto), ("preview", false));
        assert!(updates_with(config::Updates::default(), &json!({ "channel": "nightly" })).is_err());
        let r = rules_with(Rules::default(), &json!({ "paused": ["\\Big\\Videos\\", "", "/Work/"] })).unwrap();
        assert_eq!(r.paused, vec!["Big/Videos".to_string(), "Work".into()]);
    }

    #[test]
    fn version_store_changes_are_checked() {
        let c = versions_changes(&json!({ "overflow": "keep", "rotate": true, "max_gib": 2.5 })).unwrap();
        assert_eq!(
            (c.overflow, c.rotate, c.max_gib, c.keep_replaced),
            (Some(OverflowPolicy::Keep), Some(true), Some(2.5), None)
        );
        assert!(versions_changes(&json!({ "overflow": "drop" })).is_err());
    }

    #[test]
    fn sync_failures_come_with_their_choices_and_what_waits() {
        let dir = std::env::temp_dir().join(format!("oneup-settingsapi-unsynced-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        {
            let mut s = crate::setup::open_store(&dir, 1024).unwrap();
            let record = r#"{"items":[["/old","gone_here"],["/old/a.txt","gone_here"],["/bad|name","invalid_name"]]}"#;
            s.write(|o| o.meta_set(crate::unsynced::KEY, record)).unwrap();
            let r = crate::resolve::Request {
                path: "/OLD".into(),
                choice: crate::resolve::Choice::Restore,
                name: None,
                id: None,
            };
            crate::resolve::ask(&mut s, &[r]).unwrap();
        }
        let v = unsynced(&dir).unwrap();
        let codes = |i: usize| -> Vec<&str> {
            v["items"][i]["choices"].as_array().unwrap().iter().map(|c| c["code"].as_str().unwrap()).collect()
        };
        assert_eq!(codes(0), ["delete-in-onedrive", "restore"]);
        assert_eq!(v["items"][0]["queued"], "Restore");
        // Inside a folder gone too: brought back with that folder only.
        assert_eq!(v["items"][1]["nested"], true);
        assert_eq!(codes(1), ["delete-in-onedrive"]);
        assert_eq!((codes(2), &v["items"][2]["kind"]), (vec!["rename"], &json!("invalid_name")));
        assert_eq!(v["items"][0]["choices"][0]["deletes"], true);
        let _ = std::fs::remove_dir_all(&dir);
    }
}
