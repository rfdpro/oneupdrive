//! A reviewed portable document in OneDrive. Credentials, executable paths,
//! per-PC choices and destructive policies never cross this boundary.
use crate::config;
use anyhow::{Context, Result, bail};
use oneup_graph::{DriveItem, Graph};
use oneup_sync::{RemoteId, RemotePre};
use serde_json::{Value, json};
use sha2::{Digest, Sha256};
use std::{io::Write, path::Path};

pub const NAME: &str = "oneup.settings";
const STATE: &str = "shared-settings.json";
const LIMIT: usize = 1024 * 1024;

/// Serializes this feature's UI, CLI and background writers. Busy
/// background work waits for the next ordinary sync round.
fn writer(dir: &Path) -> Result<Option<std::fs::File>> {
    std::fs::create_dir_all(dir)?;
    let mut opts = std::fs::OpenOptions::new();
    opts.read(true).write(true).create(true).truncate(false);
    #[cfg(windows)]
    {
        use std::os::windows::fs::OpenOptionsExt;
        opts.share_mode(0);
    }
    match opts.open(dir.join("shared-settings.lock")) {
        Ok(f) => Ok(Some(f)),
        Err(e) if matches!(e.raw_os_error(), Some(32 | 33)) => Ok(None),
        Err(e) => Err(e.into()),
    }
}

fn only(v: &Value, keys: &[&str]) -> Result<()> {
    let o = v.as_object().context("settings must be an object")?;
    for key in o.keys() {
        if !keys.contains(&key.as_str()) {
            bail!("unsupported shared setting: {key}");
        }
    }
    for key in keys {
        if !o.contains_key(*key) {
            bail!("missing shared setting: {key}");
        }
    }
    Ok(())
}

pub fn validate(v: &Value) -> Result<()> {
    only(v, &["format", "version", "scope", "rules", "sync", "databases"])?;
    if v["format"] != "OneUpDrive" || v["version"] != 1 {
        bail!("unsupported oneup.settings format or version");
    }
    let scope = v["scope"].as_str().context("shared settings need a OneDrive folder")?;
    if crate::sync::normalize_remote(scope)? != scope {
        bail!("invalid shared settings folder");
    }
    only(&v["rules"], &["presets", "exclude"])?;
    only(&v["sync"], &["interval_minutes", "keep_push"])?;
    only(&v["databases"], &["wait", "presets", "patterns"])?;
    if !v["sync"]["interval_minutes"].is_u64()
        || !v["sync"]["keep_push"].is_boolean()
        || !v["databases"]["wait"].is_boolean()
    {
        bail!("invalid shared setting type");
    }
    for (section, key) in
        [("rules", "presets"), ("rules", "exclude"), ("databases", "presets"), ("databases", "patterns")]
    {
        if !v[section][key].as_array().is_some_and(|a| a.iter().all(Value::is_string)) {
            bail!("invalid shared setting list");
        }
    }
    crate::settingsapi::rules_with(config::Rules::default(), &v["rules"])?;
    crate::settingsapi::sync_with(crate::pace::Interval::default(), &v["sync"])?;
    crate::settingsapi::databases_with(config::DatabaseSettings::default(), &v["databases"])?;
    if serde_json::to_vec(v)?.len() > LIMIT {
        bail!("shared settings exceed 1 MiB");
    }
    Ok(())
}

pub fn portable(dir: &Path) -> Result<Value> {
    let store = crate::setup::open_store(dir, 1024)?;
    let r = config::load_rules(dir, &store)?;
    let s = crate::pace::load(dir)?;
    let d = config::load_databases(dir)?;
    let scope = store
        .read(|o| o.meta_get(crate::sync::KEY_REMOTE))?
        .unwrap_or_else(|| state(dir)["scope"].as_str().unwrap_or("/").to_owned());
    let v = json!({"format":"OneUpDrive","version":1,"scope":scope,
        "rules":{"presets":r.presets,"exclude":r.exclude},
        "sync":{"interval_minutes":s.minutes,"keep_push":s.keep_push},
        "databases":{"wait":d.wait,"presets":d.presets,"patterns":d.patterns}});
    validate(&v)?;
    Ok(v)
}

struct Bounded(Vec<u8>);
impl Write for Bounded {
    fn write(&mut self, b: &[u8]) -> std::io::Result<usize> {
        if b.len() > LIMIT - self.0.len() {
            return Err(std::io::Error::other("shared settings exceed 1 MiB"));
        }
        self.0.extend_from_slice(b);
        Ok(b.len())
    }
    fn flush(&mut self) -> std::io::Result<()> {
        Ok(())
    }
}

async fn remote(graph: &Graph) -> Result<Option<(DriveItem, Value)>> {
    let Some(item) = graph.item_by_path(NAME).await? else { return Ok(None) };
    if item.e_tag.as_deref().is_none_or(str::is_empty) {
        bail!("OneDrive did not provide a shared settings version; review again");
    }
    if item.file.is_none() || item.folder.is_some() || item.size.is_none_or(|n| n < 0 || n > LIMIT as i64) {
        bail!("oneup.settings is not a portable settings file");
    }
    let info = graph.download_info(&item.id).await?.context("shared settings disappeared; review again")?;
    if info.e_tag != item.e_tag {
        bail!("shared settings changed; review again");
    }
    let mut out = Bounded(Vec::new());
    graph.download_to(info.download_url.as_deref().context("shared settings cannot be downloaded")?, &mut out).await?;
    let mut h = oneup_hash::QuickXorHash::new();
    h.update(&out.0);
    if item.size != Some(out.0.len() as i64) || item.quick_xor_hash() != Some(h.finalize()) {
        bail!("shared settings transfer did not verify");
    }
    let now = graph.item(&item.id).await?.context("shared settings disappeared; review again")?;
    if now.e_tag != item.e_tag {
        bail!("shared settings changed; review again");
    }
    let v: Value = serde_json::from_slice(&out.0).context("invalid oneup.settings")?;
    validate(&v)?;
    Ok(Some((item, v)))
}

fn code(v: &Value) -> Result<String> {
    Ok(format!("{:x}", Sha256::digest(serde_json::to_vec(v)?)))
}
fn config_stamp(dir: &Path) -> Result<String> {
    let bytes = match std::fs::read(config::path(dir)) {
        Ok(b) => b,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => Vec::new(),
        Err(e) => return Err(e.into()),
    };
    Ok(format!("{:x}", Sha256::digest(bytes)))
}
pub fn state(dir: &Path) -> Value {
    std::fs::read(dir.join(STATE))
        .ok()
        .and_then(|b| serde_json::from_slice(&b).ok())
        .filter(Value::is_object)
        .unwrap_or(json!({"enabled":false}))
}
fn save_state(dir: &Path, v: &Value) -> Result<()> {
    crate::ownfile::write(&dir.join(STATE), &serde_json::to_vec(v)?)?;
    Ok(())
}
pub fn disable(dir: &Path) -> Result<()> {
    let _writer = writer(dir)?.context("another settings action is in progress")?;
    save_state(dir, &json!({"enabled":false}))
}

pub async fn preview(dir: &Path, graph: &Graph) -> Result<Value> {
    let local = portable(dir)?;
    let cloud = remote(graph).await?;
    let root = graph.root().await?;
    let mut p = json!({"root":root.id,"local":local,"cloud":cloud.as_ref().map(|(_,v)|v),
        "item":cloud.as_ref().map(|(i,_)|i.id.as_str()),"etag":cloud.as_ref().and_then(|(i,_)|i.e_tag.as_deref()),
        "config_stamp":config_stamp(dir)?});
    let c = code(&p)?;
    p["code"] = json!(c);
    let st = state(dir);
    p["enabled"] = st["enabled"].clone();
    p["error"] = st["error"].clone();
    Ok(p)
}

fn checked(p: &Value, expect: &str) -> Result<()> {
    if expect.is_empty() || p["code"].as_str() != Some(expect) {
        bail!("settings changed since the preview; review again");
    }
    Ok(())
}

/// Writes all portable keys in one atomic config replacement. Local keys
/// in those sections and every other section are preserved.
pub fn import_document(dir: &Path, v: &Value) -> Result<()> {
    validate(v)?;
    let store = crate::setup::open_store(dir, 1024)?;
    if let Some(remote) = store.read(|o| o.meta_get(crate::sync::KEY_REMOTE))?
        && remote != v["scope"]
    {
        bail!("shared settings concern another OneDrive folder");
    }
    let mut doc = config::read_doc(dir)?.unwrap_or_default();
    for section in ["rules", "sync", "databases"] {
        if !doc.contains_table(section) {
            doc[section] = toml_edit::Item::Table(toml_edit::Table::new());
        }
        for (key, x) in v[section].as_object().context("invalid portable section")? {
            let item = match x {
                Value::Bool(b) => toml_edit::value(*b),
                Value::Number(n) => toml_edit::value(n.as_i64().context("invalid setting number")?),
                Value::Array(a) => toml_edit::value(
                    a.iter()
                        .map(|x| x.as_str().context("invalid shared setting list"))
                        .collect::<Result<toml_edit::Array>>()?,
                ),
                _ => bail!("invalid portable setting"),
            };
            doc[section][key] = item;
        }
    }
    crate::ownfile::write(&config::path(dir), doc.to_string().as_bytes())?;
    Ok(())
}

pub async fn import(dir: &Path, graph: &Graph, expect: &str, automatic: bool) -> Result<()> {
    let _writer = writer(dir)?.context("another settings action is in progress")?;
    let p = preview(dir, graph).await?;
    checked(&p, expect)?;
    if p["cloud"].is_null() {
        bail!("no shared settings in OneDrive");
    }
    if config_stamp(dir)? != p["config_stamp"] {
        bail!("local settings changed; review again");
    }
    import_document(dir, &p["cloud"])?;
    save_state(
        dir,
        &json!({"enabled":automatic,"root":p["root"],"etag":p["etag"],"digest":code(&p["cloud"])?,"scope":p["cloud"]["scope"]}),
    )
}

pub async fn publish(dir: &Path, graph: &Graph, expect: &str, automatic: bool) -> Result<()> {
    let _writer = writer(dir)?.context("another settings action is in progress")?;
    let p = preview(dir, graph).await?;
    checked(&p, expect)?;
    publish_preview(dir, graph, &p, automatic).await
}
async fn publish_preview(dir: &Path, graph: &Graph, p: &Value, automatic: bool) -> Result<()> {
    validate(&p["local"])?;
    let store = crate::setup::open_store(dir, 1024)?;
    if crate::versions::policy(&store)?.panic || crate::control::paused_in(dir, crate::control::now_ms()) {
        bail!("settings publication waits while sync is paused");
    }
    let root = graph.root().await?;
    if p["root"] != root.id {
        bail!("OneDrive account changed; review settings again");
    }
    if config_stamp(dir)? != p["config_stamp"] {
        bail!("local settings changed; review again");
    }
    let parent = RemoteId(root.id);
    let replace = if let Some(id) = p["item"].as_str() {
        Some(RemotePre {
            id: RemoteId(id.into()),
            etag: p["etag"].as_str().context("shared settings have no eTag")?.into(),
        })
    } else {
        None
    };
    let token = oneup_gate::settings::review(parent, replace, serde_json::to_vec_pretty(&p["local"])?, true)
        .context("settings publication refused")?;
    // Durable intent before the mutation. A crash after the upload is
    // recovered by comparing the cloud document, without uploading again.
    let mut pending = state(dir);
    pending["enabled"] = json!(automatic);
    pending["root"] = p["root"].clone();
    pending["etag"] = p["etag"].clone();
    pending["pending"] = p["local"].clone();
    save_state(dir, &pending)?;
    let item = oneup_effects::settings::publish(graph, token).await?;
    save_state(
        dir,
        &json!({"enabled":automatic,"root":p["root"],"etag":item.e_tag,"digest":code(&p["local"])?,"scope":p["local"]["scope"]}),
    )?;
    let mut store = crate::setup::open_store(dir, 1024)?;
    let entry = oneup_store::ActivityEntry {
        seq: 0,
        at_ms: crate::control::now_ms(),
        outcome: oneup_store::ActivityOutcome::Done,
        action: "settings".into(),
        path: String::new(),
        detail: "Portable settings published to OneDrive".into(),
    };
    store.write(|o| o.activity_add(&[entry]))?;
    Ok(())
}

/// Called only when a sync round is allowed to upload. Unchanged settings
/// require no network requests. A remote change is never overwritten by a
/// background publication: it waits for another reviewed action.
pub async fn publish_changed(dir: &Path, graph: &Graph) -> Result<()> {
    let Some(_writer) = writer(dir)? else { return Ok(()) };
    let result = publish_current(dir, graph).await;
    if result.is_err() {
        record_failure(dir);
    }
    result
}

async fn publish_current(dir: &Path, graph: &Graph) -> Result<()> {
    let st = state(dir);
    let automatic = st["enabled"] == true;
    let pending = !st["pending"].is_null();
    if (!automatic && !pending) || st["error"].is_string() {
        return Ok(());
    }
    let local = portable(dir)?;
    if !pending && st["digest"].as_str() == Some(code(&local)?.as_str()) {
        return Ok(());
    }
    let mut p = preview(dir, graph).await?;
    if p["root"] != st["root"] {
        bail!("OneDrive account changed; review settings again");
    }
    if pending && !automatic {
        p["local"] = st["pending"].clone();
    }
    if pending && p["cloud"] == st["pending"] {
        return save_state(
            dir,
            &json!({"enabled":automatic,"root":p["root"],"etag":p["etag"],
            "digest":code(&st["pending"])?,"scope":st["pending"]["scope"]}),
        );
    }
    if p["cloud"] == p["local"] {
        return save_state(
            dir,
            &json!({"enabled":automatic,"root":p["root"],"etag":p["etag"],"digest":code(&p["local"])?,"scope":p["local"]["scope"]}),
        );
    }
    if p["etag"] != st["etag"] {
        bail!("shared settings changed in OneDrive; review them in Settings > General");
    }
    publish_preview(dir, graph, &p, automatic).await
}

fn record_failure(dir: &Path) {
    let mut st = state(dir);
    st["error"] = json!("Shared settings could not be published. Review them in Settings > General.");
    if let Ok(mut store) = crate::setup::open_store(dir, 1024) {
        let entry = oneup_store::ActivityEntry {
            seq: 0,
            at_ms: crate::control::now_ms(),
            outcome: oneup_store::ActivityOutcome::Failed,
            action: "settings".into(),
            path: String::new(),
            detail: "Shared settings publication stopped; review Settings > General".into(),
        };
        let _ = store.write(|o| o.activity_add(&[entry]));
    }
    let _ = save_state(dir, &st);
}

pub async fn background(dir: &Path, graph: &Graph) {
    let _ = publish_changed(dir, graph).await;
}

#[cfg(test)]
mod tests {
    use super::*;
    #[cfg(windows)]
    #[test]
    fn an_in_progress_settings_action_prevents_disable_from_overwriting_its_state() {
        let dir = std::env::temp_dir().join(format!("oneup-settings-lock-{}", std::process::id()));
        let guard = writer(&dir).unwrap().unwrap();
        assert!(writer(&dir).unwrap().is_none());
        assert!(disable(&dir).is_err());
        drop(guard);
        crate::ownfile::write(&dir.join(STATE), b"[]").unwrap();
        assert_eq!(state(&dir)["enabled"], false, "invalid state disables publication");
        disable(&dir).unwrap();
        assert_eq!(state(&dir)["enabled"], false);
        std::fs::remove_dir_all(dir).unwrap();
    }
    #[test]
    fn portable_import_preserves_machine_settings_and_rejects_unsafe_fields() {
        let dir = std::env::temp_dir().join(format!("oneup-portable-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let original = "[rules]\npresets=[]\nexclude=['old/']\ncloud_copies='keep'\npin=['/Local']\n[pause]\nfolders=['/Private']\n[agent]\nmemory='minimal'\n[custom]\npath='C:\\Machine'\n";
        crate::ownfile::write(&config::path(&dir), original.as_bytes()).unwrap();
        let mut doc = portable(&dir).unwrap();
        assert!(!doc.to_string().contains("Machine"));
        assert!(!doc.to_string().contains("Private"));
        assert!(!doc.to_string().contains("cloud_copies"));
        doc["rules"]["exclude"] = json!(["build/", "*.cache"]);
        doc["sync"]["interval_minutes"] = json!(15);
        import_document(&dir, &doc).unwrap();
        let text = std::fs::read_to_string(config::path(&dir)).unwrap();
        let parsed = text.parse::<toml_edit::DocumentMut>().unwrap();
        assert_eq!(parsed["agent"]["memory"].as_str(), Some("minimal"));
        assert_eq!(parsed["rules"]["cloud_copies"].as_str(), Some("keep"));
        assert_eq!(parsed["custom"]["path"].as_str(), Some("C:\\Machine"));
        assert_eq!(parsed["sync"]["interval_minutes"].as_integer(), Some(15));
        for invalid in [
            {
                let mut x = doc.clone();
                x["hooks"] = json!({"program":"evil"});
                x
            },
            {
                let mut x = doc.clone();
                x["rules"]["cloud"] = json!("prune");
                x
            },
            {
                let mut x = doc.clone();
                x["version"] = json!(2);
                x
            },
            {
                let mut x = doc.clone();
                x["rules"]["exclude"] = Value::Null;
                x
            },
            {
                let mut x = doc.clone();
                x["sync"]["interval_minutes"] = json!(9999);
                x
            },
        ] {
            assert!(import_document(&dir, &invalid).is_err());
            assert_eq!(std::fs::read_to_string(config::path(&dir)).unwrap(), text);
        }
        for (section, key) in
            [("rules", "presets"), ("rules", "exclude"), ("databases", "presets"), ("databases", "patterns")]
        {
            for element in [Value::Null, json!(123), json!(false), json!({"bad": "value"})] {
                let mut invalid = doc.clone();
                invalid[section][key] = json!([element]);
                assert!(import_document(&dir, &invalid).is_err(), "{section}.{key} must contain only strings");
                assert_eq!(std::fs::read_to_string(config::path(&dir)).unwrap(), text);
            }
        }
        std::fs::remove_dir_all(dir).unwrap();
    }
}
