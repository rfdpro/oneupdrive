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
    let search = config::load_search(dir)?;
    let interval = crate::pace::load(dir)?;
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
        // The memory profile's numbers and the files hashed at once, for
        // the General page's Memory and hashing section: `value` null is
        // the default.
        "tuning": a.tuned().iter().map(|t| json!({
            "key": t.knob.key,
            "label": t.knob.label,
            "min": t.knob.min,
            "max": t.knob.max,
            "default": t.default,
            "value": t.set,
        })).collect::<Vec<_>>(),
        "autostart": autostart,
        "rules": {
            "presets": presets,
            "exclude": r.exclude,
            "paused": r.paused,
            "cloud": r.cloud.name(),
            "pin": r.pin,
            "pin_waiting": crate::pinrules::waiting(&store)?,
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
        "search": { "onedrive": search.onedrive, "hotkey": search.hotkey },
        "databases": databases_json(dir)?,
        "sync": { "interval_minutes": interval.minutes, "keep_push": interval.keep_push },
        "network": network_json(dir, &a)?,
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
    // A number, or null for the default (the memory profile's).
    let tuned = |k: &str| -> Result<Option<Option<u64>>> {
        match v.get(k) {
            None => Ok(None),
            Some(Value::Null) => Ok(Some(None)),
            Some(x) => x.as_u64().map(|n| Some(Some(n))).with_context(|| format!("{k}: expected a whole number")),
        }
    };
    for key in config::OVERRIDE_KEYS {
        if let Some(n) = tuned(key)? {
            a.set_override(key, n)?;
        }
    }
    let hashing = config::HASH_FILES_AT_ONCE;
    if let Some(n) = tuned(hashing.key)? {
        a.hash_files_at_once = match n {
            Some(n) => hashing.check(n)?,
            None => Agent::default().hash_files_at_once,
        };
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

fn layer_json(l: &crate::network::Layer) -> Value {
    json!({
        "transfer": l.transfer.map(crate::network::Transfer::name),
        "small_mb": l.small_mb,
        "upload_kib": l.upload_kib,
        "download_kib": l.download_kib,
        "interval_minutes": l.interval_minutes,
        "keep_push": l.keep_push.map(|b| if b { "on" } else { "off" }),
        "budget_mb": l.budget_mb,
    })
}

/// The network page's profiles: this network (and its own settings, if
/// any) and the metered profile, with "paused" as `[agent]` says.
fn network_json(dir: &Path, a: &Agent) -> Result<Value> {
    let nets = crate::network::load(dir)?;
    let n = oneup_sys::network::current();
    let own = n.id.as_deref().and_then(|id| nets.get(id)).map(layer_json);
    let mut metered = layer_json(&nets.metered);
    let transfer = match nets.metered.transfer {
        _ if a.pause_on_metered => "paused",
        Some(t) if t != crate::network::Transfer::Paused => t.name(),
        _ => "everything",
    };
    metered["transfer"] = json!(transfer);
    Ok(json!({
        "this": { "id": n.id, "name": n.name, "metered": n.metered, "connectivity": n.connectivity.name() },
        "own": own,
        "metered": metered,
        "small_mb_default": crate::network::SMALL_MB,
    }))
}

/// A profile's settings changed by the fields present in `v`: a number
/// sets one, `null` or `""` goes back to the layer below.
pub fn layer_with(mut l: crate::network::Layer, v: &Value) -> Result<crate::network::Layer> {
    let num = |k: &str| -> Result<Option<Option<u64>>> {
        match v.get(k) {
            None => Ok(None),
            Some(Value::Null) => Ok(Some(None)),
            Some(Value::String(s)) if s.trim().is_empty() => Ok(Some(None)),
            Some(x) => Ok(Some(Some(x.as_u64().with_context(|| format!("{k}: expected a whole number"))?))),
        }
    };
    if let Some(t) = v.get("transfer") {
        l.transfer = match t.as_str().unwrap_or_default() {
            "" => None,
            name => Some(crate::network::Transfer::from_name(name).context("transfer: unknown choice")?),
        };
    }
    if let Some(n) = num("small_mb")? {
        l.small_mb = n;
    }
    if let Some(n) = num("upload_kib")? {
        l.upload_kib = n;
    }
    if let Some(n) = num("download_kib")? {
        l.download_kib = n;
    }
    if let Some(n) = num("interval_minutes")? {
        l.interval_minutes = n.map(crate::pace::check_minutes).transpose()?;
    }
    if let Some(n) = num("budget_mb")? {
        l.budget_mb = n;
    }
    // "on", "off", a boolean, or empty/null for the layer below.
    if let Some(k) = v.get("keep_push") {
        l.keep_push = match k {
            Value::Null => None,
            Value::Bool(b) => Some(*b),
            Value::String(s) if s.is_empty() => None,
            Value::String(s) if s == "on" => Some(true),
            Value::String(s) if s == "off" => Some(false),
            _ => bail!("keep_push: on, off or empty"),
        };
    }
    crate::network::check(&l)?;
    Ok(l)
}

/// Saves the network page's change: to `"target": "this"` network (or
/// forgets its settings) or the `"metered"` profile.
fn save_network(dir: &Path, v: &Value) -> Result<()> {
    let mut nets = crate::network::load(dir)?;
    match v.get("target").and_then(Value::as_str) {
        Some("metered") => {
            nets.metered = layer_with(nets.metered.clone(), v)?;
            // With `pause_on_metered`, in one write (`network::save_with_agent`).
            let mut a = config::load_agent(dir)?;
            if let Some(t) = v.get("transfer").and_then(Value::as_str) {
                a.pause_on_metered = t == "paused";
                if a.pause_on_metered {
                    nets.metered.transfer = None;
                }
            }
            return crate::network::save_with_agent(dir, &nets, &a);
        }
        Some("this") => {
            let (id, name) = crate::network::this_network()?;
            if v.get("forget").and_then(Value::as_bool) == Some(true) {
                nets.forget(&id);
            } else {
                let l = layer_with(nets.get(&id).cloned().unwrap_or_default(), v)?;
                let l = crate::network::Layer { name: l.name.clone().or(Some(name)), ..l };
                *nets.get_mut(&id) = l;
            }
        }
        _ => bail!("network: target is this or metered"),
    }
    crate::network::save(dir, &nets)
}

/// The sync interval (`[sync]`) changed by the fields present in `v`.
pub fn sync_with(mut i: crate::pace::Interval, v: &Value) -> Result<crate::pace::Interval> {
    if let Some(n) = get(v, "interval_minutes") {
        i.minutes = crate::pace::check_minutes(n.as_u64().context("interval_minutes: expected a whole number")?)?;
    }
    if let Some(b) = get(v, "keep_push") {
        i.keep_push = b.as_bool().context("keep_push: expected on or off")?;
    }
    Ok(i)
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
    if let Some(lines) = list("pin")? {
        let lines: Vec<String> = lines.into_iter().map(|l| l.trim().to_string()).filter(|l| !l.is_empty()).collect();
        for l in &lines {
            config::check_rule(l)?;
        }
        r.pin = lines;
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

/// The window's `tree_sync`: an item ticked (`on`) or, after its preview,
/// unticked. Unticking with the preview's token (`expect`) excludes only
/// the item that preview showed (`filetree::exclude_checked`).
pub fn tree_sync(dir: &Path, args: &Value) -> Result<String> {
    let on = args.get("on").and_then(Value::as_bool).unwrap_or(false);
    match args.get("expect").filter(|e| !e.is_null()) {
        Some(expect) if !on => crate::filetree::exclude_checked(dir, expect),
        _ => {
            let rel = args.get("rel").and_then(Value::as_str).unwrap_or_default();
            let folder = args.get("folder").and_then(Value::as_bool).unwrap_or(false);
            crate::filetree::set_sync(dir, rel, folder, on)
        }
    }
}

/// Saves pin rules `lines` (each checked) the person saw `bytes` of
/// downloads for, and records that yes for exactly those rules
/// (`crate::pinrules::approve`).
pub fn save_pins(dir: &Path, lines: &[String], bytes: u64) -> Result<()> {
    let mut store = crate::setup::open_store(dir, 1024)?;
    let r = rules_with(config::load_rules(dir, &store)?, &json!({ "pin": lines }))?;
    config::save_rules(dir, &r)?;
    crate::pinrules::approve(&mut store, &r.pin_set().fingerprint(), bytes)
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

/// The `[search]` changed by the fields present in `v` (a hotkey written
/// as `Hotkey` writes it; empty or `off`: none).
pub fn search_with(mut s: config::Search, v: &Value) -> Result<config::Search> {
    if let Some(b) = get(v, "onedrive") {
        s.onedrive = b.as_bool().context("onedrive: on or off")?;
    }
    if let Some(h) = get(v, "hotkey") {
        let h = h.as_str().context("hotkey: keys like Ctrl+Alt+F")?.trim();
        s.hotkey = if h.is_empty() || h.eq_ignore_ascii_case("off") {
            String::new()
        } else {
            crate::search::Hotkey::parse(h)?.to_string()
        };
    }
    Ok(s)
}

/// The `[databases]` settings, with every database preset (on or not),
/// for **Files → Exclude → Databases**. What waits comes from the agent's
/// status (`databases`).
pub fn databases_json(dir: &Path) -> Result<Value> {
    let d = config::load_databases(dir)?;
    let presets: Vec<Value> = oneup_rules::database::DATABASE_PRESETS
        .iter()
        .map(|p| {
            json!({ "name": p.name, "summary": p.summary, "lines": p.lines, "on": d.presets.iter().any(|x| x == p.name) })
        })
        .collect();
    Ok(json!({ "wait": d.wait, "presets": presets, "patterns": d.patterns, "keep_syncing": d.keep_syncing }))
}

/// The `[databases]` section changed by the fields present in `v`:
/// `wait`, `presets` (the names on), `patterns`, and `keep_syncing` (a
/// path, `/Mail/archive.pst`, added to the list).
pub fn databases_with(mut d: config::DatabaseSettings, v: &Value) -> Result<config::DatabaseSettings> {
    if let Some(w) = get(v, "wait") {
        d.wait = w.as_bool().context("wait: on or off")?;
    }
    let list = |k: &str| -> Result<Option<Vec<String>>> {
        get(v, k)
            .map(|x| {
                let all = x.as_array().with_context(|| format!("{k}: expected a list"))?;
                Ok(all
                    .iter()
                    .filter_map(|l| l.as_str())
                    .map(|l| l.trim().to_string())
                    .filter(|l| !l.is_empty())
                    .collect())
            })
            .transpose()
    };
    if let Some(p) = list("presets")? {
        d.presets = p;
    }
    if let Some(p) = list("patterns")? {
        for l in &p {
            config::check_rule(l)?;
        }
        d.patterns = p;
    }
    if let Some(k) = get(v, "keep_syncing") {
        let k = k.as_str().context("keep_syncing: a path")?;
        let k = crate::databases::rel_path(k);
        if !crate::databases::kept_syncing(&d.keep_syncing, &k) {
            d.keep_syncing.push(k);
        }
    }
    d.databases()?;
    Ok(d)
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

/// For each item, whether a folder above it is listed for the same reason
/// (both held), the paths compared as Windows compares names.
fn nested(items: &[crate::unsynced::Item]) -> Vec<bool> {
    let tops: Vec<&crate::unsynced::Item> = items.iter().filter(|i| i.why.held()).collect();
    items
        .iter()
        .map(|i| i.why.held() && tops.iter().any(|t| t.why == i.why && crate::resolve::below(&i.path, &t.path)))
        .collect()
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
    let nested_all = nested(&r.items);
    let mut items: Vec<Value> = Vec::with_capacity(r.items.len());
    for (i, &nested) in r.items.iter().zip(&nested_all) {
        // Which item the page shows: an answer from it is for that one only
        // (`unsynced_preview`). Without one there's nothing to bind it to,
        // so only an exclusion (a rule, by path) is offered.
        let subject = store.read(|o| crate::resolve::subject(o, i))?;
        let choices: Vec<Value> = choices(i.why, backup)
            .iter()
            .filter(|c| subject.is_some() || **c == Choice::Exclude)
            // Bringing an item back into a folder that's gone too waits
            // for that folder's answer.
            .filter(|c| !(nested && matches!(c, Choice::Restore | Choice::Upload)))
            .map(|c| json!({ "code": c.code(), "label": c.label(), "deletes": c.deletes(), "bulk": c.in_bulk() }))
            .collect();
        let here =
            local.as_ref().is_some_and(|l| std::fs::symlink_metadata(l.join(i.path.trim_start_matches('/'))).is_ok());
        let waits = queued.iter().find(|q| q.path == i.path).map(|q| q.choice.label());
        items.push(json!({ "path": i.path, "why": i.why.title(), "detail": i.why.label(), "kind": i.why.code(),
            "choices": choices, "nested": nested, "here": here, "queued": waits, "subject": subject }));
    }
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
/// `subject`: for one item, which item the window listed
/// (`resolve::subject`), required; refused if it's another now or can't
/// be told.
pub fn unsynced_preview(
    dir: &Path,
    path: Option<&str>,
    subject: Option<&str>,
    choice: &str,
    name: Option<&str>,
) -> Result<Value> {
    let choice = crate::resolve::Choice::from_code(choice).with_context(|| format!("unknown choice: {choice}"))?;
    let store = crate::setup::open_store(dir, 1024)?;
    let backup = crate::flow::get(&store)? == crate::flow::Flow::Backup;
    let record = crate::unsynced::load(&store)?;
    let paths = path.map(|p| vec![p.to_string()]);
    let items = crate::resolve::select(&record, Some(choice), paths.as_deref(), backup)?;
    if let [item] = items.as_slice()
        && path.is_some()
    {
        let now = store.read(|o| crate::resolve::subject(o, item))?;
        if subject.is_none() || now.as_deref() != subject {
            bail!("{} changed since it was listed; look again", item.path);
        }
    }
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
            json!({ "path": c.path, "folder": c.folder, "items": c.items, "unlisted": c.unlisted, "exact": c.exact,
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

/// Asks the agent to put a kept item back where it was (never over
/// anything: a taken name gets " (restored N)"); returns where it came
/// from (`crate::restores`).
pub fn kept_restore(dir: &Path, id: &str) -> Result<String> {
    let mut store = crate::setup::open_store(dir, 1024)?;
    crate::restores::kept(&mut store, id, None)
}

/// Asks the agent to bring back deleted item `seq` from OneDrive's recycle
/// bin; returns where it goes (`crate::restores`).
pub fn cloud_bin_restore(dir: &Path, seq: i64) -> Result<String> {
    crate::setup::require_test_account(dir)?;
    if crate::vaultpair::is_vault_dir(dir) {
        anyhow::bail!("this is an encrypted vault: OneDrive only has its encrypted files");
    }
    let mut store = crate::setup::open_store(dir, 1024)?;
    crate::restores::deleted(&mut store, seq)
}

/// The Recovery page's rewind: the one under way, if any, and how far
/// back the history reaches (`crate::rewind`).
pub fn rewind_state(dir: &Path) -> Result<Value> {
    let store = crate::setup::open_store(dir, 1024)?;
    let (job, since) = store.read(|o| Ok((crate::rewind::load(o)?, o.history_since()?)))?;
    let since = (since != i64::MAX).then_some(since);
    let job = job.map(|j| {
        json!({
            "folder": j.folder, "at_ms": j.at_ms, "started_ms": j.started_ms,
            "approved": j.approval.is_some(), "ran": j.ran, "left_alone": j.left_alone, "failed": j.failed,
        })
    });
    // The code of its stop, while it waits for the approval.
    let code = std::fs::read_dir(dir.join(crate::stoplist::DIR))
        .into_iter()
        .flatten()
        .flatten()
        .filter_map(|e| e.file_name().to_str()?.strip_suffix(".txt").map(str::to_string))
        .find(|c| crate::stoplist::read(dir, c, "", 0).is_some_and(|l| l.header.contains("a rewind would make")));
    Ok(json!({ "since_ms": since, "job": job, "code": code }))
}

/// The Recovery page's conflicts (`crate::conflicts`).
pub fn conflicts(dir: &Path) -> Result<Value> {
    let store = crate::setup::open_store(dir, 1024)?;
    let listed = store.read(crate::conflicts::list)?;
    Ok(listed.iter().map(crate::conflicts::to_json).collect())
}

/// Queues keeping one copy of a conflict, as the page listed it (`keep`:
/// `original` or `copy`).
pub fn conflict_keep(dir: &Path, listed: &Value, keep: &str) -> Result<()> {
    let c = crate::conflicts::from_json(listed).context("not a conflict")?;
    let Some(k) = crate::conflicts::parse_keep(keep)? else { return Ok(()) };
    let mut store = crate::setup::open_store(dir, 1024)?;
    crate::conflicts::answer(&mut store, &c, k)
}

/// The Recovery page's items deleted from OneDrive (`crate::cloudbin`).
pub fn cloud_bin(dir: &Path) -> Result<Value> {
    let store = crate::setup::open_store(dir, 1024)?;
    let listed = store.read(|o| crate::cloudbin::list(o, crate::cloudbin::MAX))?;
    Ok(listed.iter().map(crate::cloudbin::to_json).collect())
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
        "search" => {
            let s = search_with(config::load_search(dir)?, v)?;
            config::save_search(dir, &s)?;
            // The icon in the notification area holds the hotkey.
            crate::tray::settings_changed(dir);
            Ok(())
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
        "databases" => {
            let d = databases_with(config::load_databases(dir)?, v)?;
            config::save_databases(dir, &d)?;
            // Excluding one of the files warned about: a rule for that file.
            if let Some(p) = get(v, "exclude").and_then(Value::as_str) {
                let store = crate::setup::open_store(dir, 1024)?;
                let mut r = config::load_rules(dir, &store)?;
                if let Some(rule) = oneup_rules::literal_path_rules(&crate::databases::rel_path(p)).pop()
                    && !r.exclude.contains(&rule)
                {
                    r.exclude.push(rule);
                    config::save_rules(dir, &r)?;
                }
            }
            Ok(())
        }
        "sync" => {
            let i = sync_with(crate::pace::load(dir)?, v)?;
            crate::pace::save(dir, &i)
        }
        "network" => save_network(dir, v),
        "autostart" => {
            let on = v.as_bool().context("autostart: expected on or off")?;
            let launcher = if on { Some(crate::launch::sibling("oneupd-start.exe")?) } else { None };
            crate::launch::set_autostart(crate::launch::RUN_KEY, dir, launcher.as_deref()).map(|_| ())
        }
        other => bail!("nothing called {other} to save"),
    }
}

/// The Duplicates view (`crate::duplicates`): a page of groups after
/// `after` (a cursor a page gave), the removal waiting for approval if
/// any, and how many files only here are hashed.
pub fn duplicates(dir: &Path, after: Option<&str>, limit: usize) -> Result<Value> {
    let store = crate::setup::open_store(dir, 1024)?;
    let after = after.filter(|a| !a.is_empty()).map(crate::duplicates::parse_cursor).transpose()?;
    let page = crate::duplicates::list(dir, &store, after, limit)?;
    let mut v = crate::duplicates::page_json(&page);
    v["waiting"] = match store.read(crate::duplicates::queued)? {
        Some(w) => json!({ "copies": w.copies.len(), "code": w.code }),
        None => Value::Null,
    };
    v["hashed"] = Value::from(store.read(|o| o.local_only_hash_count())?);
    Ok(v)
}

/// The copies the window chose: `[{path, key}]` as [`duplicates`] listed them.
/// The paths the window sent only name a copy in a refusal: what's
/// previewed and removed is worked out from the store.
fn chosen_copies(copies: &[Value]) -> Result<Vec<oneup_exec::dupes::Chosen>> {
    copies
        .iter()
        .map(|c| {
            let path = c["path"].as_str().unwrap_or_default();
            crate::duplicates::parse_key(&c["key"]).with_context(|| format!("{path}: not a listed copy"))
        })
        .collect()
}

fn prepared_json(p: &crate::duplicates::Prepared) -> Value {
    let lines: Vec<Value> = p.lines.iter().map(|l| json!({ "path": l.path, "size": l.size, "to": l.to })).collect();
    let left: Vec<Value> = p.left.iter().map(|(path, why)| json!({ "path": path, "why": why })).collect();
    json!({ "lines": lines, "left": left, "bytes": p.bytes, "code": p.code })
}

/// What removing the chosen copies would do now, and its code.
pub fn duplicates_preview(dir: &Path, copies: &[Value]) -> Result<Value> {
    let mut store = crate::setup::open_store(dir, 1024)?;
    let chosen = chosen_copies(copies)?;
    Ok(prepared_json(&crate::duplicates::prepare(dir, &mut store, &chosen)?))
}

/// Hands removing the chosen copies to the agent, if it still does exactly
/// what the preview with code `expect` showed. The agent then asks for
/// approval like any safety stop.
pub fn duplicates_remove(dir: &Path, copies: &[Value], expect: &str) -> Result<Value> {
    let mut store = crate::setup::open_store(dir, 1024)?;
    let chosen = chosen_copies(copies)?;
    Ok(prepared_json(&crate::duplicates::ask(dir, &mut store, &chosen, expect)?))
}

/// Drops the removal waiting for approval.
pub fn duplicates_cancel(dir: &Path) -> Result<Value> {
    let mut store = crate::setup::open_store(dir, 1024)?;
    Ok(Value::from(crate::duplicates::cancel(&mut store)?))
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used)]
    use super::*;

    #[test]
    fn an_item_is_nested_under_a_folder_listed_for_the_same_reason_as_names_compare() {
        use crate::unsynced::{Item, Why};
        let item = |path: &str, why| Item { path: path.into(), why };
        let items = [
            item("/ΟΔΟΣ", Why::GoneHere),
            item("/οδοσ/a", Why::GoneHere),
            item("/οδοσ/b", Why::GoneThere),
            item("/ΟΔΟΣx/c", Why::GoneHere),
            item("/ſa", Why::GoneHere),
            item("/Sa/d", Why::GoneHere),
        ];
        assert_eq!(nested(&items), [false, true, false, false, false, true]);
    }

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
        // The memory profile's numbers: set, back to the profile's (null),
        // and checked like config.toml.
        let a = agent_with(Agent::default(), &json!({ "operations_at_once": 4, "hash_files_at_once": 2 })).unwrap();
        assert_eq!((a.overrides.operations_at_once, a.hash_files_at_once), (Some(4), 2));
        let a = agent_with(a, &json!({ "operations_at_once": null, "hash_files_at_once": null })).unwrap();
        assert_eq!(a, Agent::default());
        for bad in [
            json!({ "database_cache_mib": 0 }),
            json!({ "upload_fragment_mib": 60 }),
            json!({ "hash_files_at_once": 17 }),
            json!({ "large_uploads_at_once": "2" }),
            json!({ "operations_at_once": 2.5 }),
        ] {
            assert!(agent_with(Agent::default(), &bad).is_err(), "{bad}");
        }
    }

    #[test]
    fn network_profile_changes_are_checked_and_can_be_undone() {
        use crate::network::{Layer, Transfer};
        let l = layer_with(Layer::default(), &json!({ "transfer": "small", "small_mb": 5, "budget_mb": "" })).unwrap();
        assert_eq!((l.transfer, l.small_mb, l.budget_mb), (Some(Transfer::Small), Some(5), None));
        let l = layer_with(l, &json!({ "transfer": "", "small_mb": null })).unwrap();
        assert_eq!(l, Layer::default(), "back to the layer below");
        assert!(layer_with(Layer::default(), &json!({ "transfer": "most" })).is_err());
        assert!(layer_with(Layer::default(), &json!({ "small_mb": 0 })).is_err());
        assert!(layer_with(Layer::default(), &json!({ "interval_minutes": 99999 })).is_err());
        // The window's metered caps and keep-push, as config and CLI have them.
        let l =
            layer_with(Layer::default(), &json!({ "upload_kib": 64, "download_kib": 128, "keep_push": "on" })).unwrap();
        assert_eq!((l.upload_kib, l.download_kib, l.keep_push), (Some(64), Some(128), Some(true)));
        assert_eq!(layer_json(&l)["keep_push"], "on");
        assert_eq!(layer_with(l, &json!({ "keep_push": "" })).unwrap().keep_push, None);
        assert!(layer_with(Layer::default(), &json!({ "keep_push": "maybe" })).is_err());
    }

    #[test]
    fn the_sync_interval_is_checked() {
        let i = sync_with(crate::pace::Interval::default(), &json!({ "interval_minutes": 15 })).unwrap();
        assert_eq!((i.minutes, i.keep_push), (15, false));
        let i = sync_with(i, &json!({ "keep_push": true })).unwrap();
        assert_eq!((i.minutes, i.keep_push), (15, true));
        assert!(sync_with(i, &json!({ "interval_minutes": 5000 })).is_err());
        assert!(sync_with(i, &json!({ "interval_minutes": -1 })).is_err());
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
    fn search_changes_are_checked() {
        let s = search_with(config::Search::default(), &json!({ "onedrive": true, "hotkey": "ctrl+alt+f" })).unwrap();
        assert_eq!((s.onedrive, s.hotkey.as_str()), (true, "Ctrl+Alt+F"));
        let s = search_with(s, &json!({ "hotkey": "off" })).unwrap();
        assert_eq!((s.onedrive, s.hotkey.as_str()), (true, ""));
        assert!(search_with(config::Search::default(), &json!({ "hotkey": "F" })).is_err());
        assert!(search_with(config::Search::default(), &json!({ "onedrive": "yes" })).is_err());
    }

    #[test]
    fn database_changes_are_checked() {
        let v = json!({ "wait": false, "presets": ["firebird"], "patterns": ["*.ledger", " "], "keep_syncing": "VM\\disk.vhdx" });
        let d = databases_with(config::DatabaseSettings::default(), &v).unwrap();
        assert!(!d.wait);
        assert_eq!(
            (d.presets.as_slice(), d.patterns.as_slice()),
            (&["firebird".to_string()][..], &["*.ledger".to_string()][..])
        );
        assert_eq!(d.keep_syncing, ["/VM/disk.vhdx"]);
        let d = databases_with(d, &json!({ "keep_syncing": "/vm/DISK.vhdx" })).unwrap();
        assert_eq!(d.keep_syncing.len(), 1, "already kept");
        assert!(databases_with(config::DatabaseSettings::default(), &json!({ "presets": ["nope"] })).is_err());
        assert!(databases_with(config::DatabaseSettings::default(), &json!({ "patterns": ["#x"] })).is_err());
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
        use oneup_sync::{Base, LocalId, NodeKind, ROOT, RemoteId};
        let dir = std::env::temp_dir().join(format!("oneup-settingsapi-unsynced-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        {
            let mut s = crate::setup::open_store(&dir, 1024).unwrap();
            let row = |id: &str, parent: Option<&str>, name: &str, kind| oneup_store::RemoteRow {
                id: id.into(),
                parent_id: parent.map(Into::into),
                name: name.into(),
                kind,
                size: 0,
                etag: Some("e1".into()),
                qxh: None,
                mtime_ms: None,
            };
            let base = |sid, parent, name: &str, local: u128, remote: &str| Base {
                sid,
                parent,
                name: name.into(),
                kind: NodeKind::Folder,
                size: 0,
                hash: None,
                local: Some(LocalId(local)),
                local_mtime: 0,
                remote: Some(RemoteId(remote.into())),
                remote_etag: "e1".into(),
            };
            s.write(|o| {
                o.upsert_item(&row("root", None, "", oneup_store::Kind::Root), false)?;
                o.upsert_item(&row("r1", Some("root"), "old", oneup_store::Kind::Folder), false)?;
                o.synced_put(&base(ROOT, None, "", 1, "root"))?;
                o.synced_put(&base(1, Some(ROOT), "old", 5, "r1"))?;
                let record =
                    r#"{"items":[["/old","gone_here"],["/old/a.txt","gone_here"],["/bad|name","invalid_name"]]}"#;
                o.meta_set(crate::unsynced::KEY, record)
            })
            .unwrap();
            let r = crate::resolve::Request {
                path: "/old".into(),
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
        assert_eq!((&v["items"][0]["subject"], &v["items"][0]["queued"]), (&json!("s:1"), &json!("Restore")));
        assert_eq!(v["items"][0]["choices"][0]["deletes"], true);
        // Inside a folder gone too, and with no record of its own here:
        // nothing to bind an answer to, so nothing is offered.
        assert_eq!(v["items"][1]["nested"], true);
        assert!(codes(1).is_empty());
        assert_eq!((codes(2), &v["items"][2]["kind"]), (Vec::<&str>::new(), &json!("invalid_name")));
        // The preview is for the item listed, which the page must say.
        let refused = |subject| {
            let e = unsynced_preview(&dir, Some("/old"), subject, "restore", None).unwrap_err();
            assert!(format!("{e:#}").contains("changed since it was listed"), "{e:#}");
        };
        refused(Some("s:99"));
        refused(None);
        let pv = unsynced_preview(&dir, Some("/old"), Some("s:1"), "delete-in-onedrive", None).unwrap();
        assert_eq!(pv["tops"], json!(["/old"]));
        let _ = std::fs::remove_dir_all(&dir);
    }
}
