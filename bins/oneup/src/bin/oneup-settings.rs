//! `oneup-settings`: OneUpDrive's settings window, opened from the
//! tray icon's menu. A page (`settings_ui.html`, built into the program) in
//! a WebView2 window (`oneup_sys::webview`); the page asks, this program
//! answers: settings through `oneup_app::settingsapi` (the same checks as
//! the command line), the agent through its control pipe, vaults through
//! `oneup_app::vaultpair`, and the first-run setup by running `oneup` (the
//! same command a person would type). It follows the agent with the pipe's
//! `watch` call, so it never polls. One window per data folder; it uses
//! memory only while open.
#![windows_subsystem = "windows"]

#[cfg(windows)]
fn main() -> anyhow::Result<()> {
    win::main()
}

#[cfg(not(windows))]
fn main() {}

#[cfg(windows)]
mod win {
    use std::os::windows::process::CommandExt;
    use std::path::{Path, PathBuf};
    use std::sync::Arc;

    use anyhow::{Context, Result, bail};
    use clap::Parser;
    use oneup_app::{settingsapi, vaultpair};
    use oneup_ipc::{Answer, Call, Direction};
    use oneup_sys::webview::{self, Poster};
    use serde_json::{Value, json};

    const PAGE: &str = include_str!("../settings_ui.html");

    #[derive(Parser)]
    #[command(name = "oneup-settings", version, about = "OneUpDrive settings")]
    struct Args {
        /// The data folder whose settings to show
        #[arg(long)]
        data_dir: Option<PathBuf>,
        /// Open on this page (files, apps, activity, unsynced, excluded,
        /// stops, general, accounts, office, safety, network, about)
        #[arg(long, default_value = "")]
        page: String,
        /// For the "item" page: the file or folder to show
        #[arg(long)]
        item: Option<PathBuf>,
    }

    pub fn main() -> Result<()> {
        oneup_app::setup::tls();
        let args = Args::parse();
        let dir = match args.data_dir {
            Some(d) => d,
            None => oneup_app::test_account::default_dir()?,
        };
        let name = oneup_app::tray::instance_name(&dir).replace("-tray-", "-settings-");
        let start: String = args.page.chars().filter(char::is_ascii_lowercase).collect();
        let item = args.item.as_ref().map(|p| p.to_string_lossy().into_owned()).unwrap_or_default();
        let Some(_instance) = oneup_sys::tray::single_instance(&name)? else {
            webview::focus_existing(&start, &item);
            return Ok(());
        };
        let rt = Arc::new(
            tokio::runtime::Builder::new_multi_thread().worker_threads(2).enable_all().build().context("runtime")?,
        );
        let dark = oneup_sys::user_registry::get_dword(
            r"Software\Microsoft\Windows\CurrentVersion\Themes\Personalize",
            "AppsUseLightTheme",
        )
        .ok()
        .flatten()
        .is_some_and(|v| v == 0);
        let webdata = dir.join("settings-window");
        let page = PAGE
            .replace("{{START_PAGE}}", &start)
            // A JSON string inside the page's script; `<` escaped so it can't end it.
            .replace("\"{{START_ITEM}}\"", &Value::from(item).to_string().replace('<', "\\u003c"));
        let opts =
            webview::Options { title: "OneUpDrive", width: 1140, height: 760, data_dir: &webdata, html: &page, dark };
        let mut watching = false;
        let d = dir.clone();
        webview::run(&opts, move |text, poster| {
            if !watching {
                watching = true;
                rt.spawn(watch(d.clone(), poster.clone()));
            }
            let Ok(msg) = serde_json::from_str::<Value>(&text) else { return };
            let id = msg.get("id").cloned().unwrap_or(Value::Null);
            let op = msg.get("op").and_then(Value::as_str).unwrap_or_default().to_string();
            let args = msg.get("args").cloned().unwrap_or(Value::Null);
            let (d, poster) = (d.clone(), poster.clone());
            rt.spawn(async move {
                // A request about one root of the Files page (a folder pair
                // or vault) works on that root's own data folder: `handle`
                // gets it as `dir`, and must not look `root` up again (a
                // pair's data folder has no pairs of its own).
                let target = match args.get("root").and_then(Value::as_str) {
                    Some(root) => root_dir(&d, root),
                    None => Ok(d.clone()),
                };
                let result = match target {
                    Ok(t) => handle(&t, &op, &args, &poster).await,
                    Err(e) => Err(e),
                };
                let reply = match result {
                    Ok(v) => json!({ "id": id, "ok": v }),
                    Err(e) => json!({ "id": id, "error": format!("{e:#}") }),
                };
                poster.post(reply.to_string());
            });
        })?;
        Ok(())
    }

    /// Follows the agent and tells the page whenever its status changes
    /// (`null` while it isn't running).
    async fn watch(dir: PathBuf, poster: Poster) {
        let mut since = 0;
        loop {
            match oneup_ipc::connect(&dir).await {
                Ok(mut c) => {
                    while let Ok(s) = c.call(&Call::Watch { since }).await {
                        since = s.rev;
                        poster.post(json!({ "event": "status", "status": s }).to_string());
                    }
                }
                Err(_) => {
                    let stopped = oneup_app::problem::load_stopped(&dir);
                    poster.post(json!({ "event": "status", "status": null, "stopped": stopped }).to_string());
                }
            }
            since = 0;
            // Only while the window is open and the agent isn't running: a
            // cheap check whether it has started.
            tokio::time::sleep(std::time::Duration::from_secs(5)).await;
        }
    }

    async fn agent(dir: &Path, call: Call) -> Result<Value> {
        let mut c = oneup_ipc::connect(dir).await.context("OneUpDrive isn't running")?;
        Ok(serde_json::to_value(c.call(&call).await?)?)
    }

    fn open(path: &Path) -> Result<()> {
        std::process::Command::new("explorer.exe").arg(path).spawn().context("opening Explorer")?;
        Ok(())
    }

    fn pair(dir: &Path, args: &Value) -> Result<oneup_app::pairs::Entry> {
        oneup_app::pairs::find(dir, args.get("name").and_then(Value::as_str).unwrap_or_default())
    }

    /// Stops the agent of `dir` and waits (bounded) until it's gone.
    /// Returns whether it was running.
    async fn stop_and_wait(dir: &Path) -> bool {
        let was = agent(dir, Call::Stop).await.is_ok();
        for _ in 0..120 {
            if oneup_ipc::connect(dir).await.is_err() {
                break;
            }
            tokio::time::sleep(std::time::Duration::from_millis(500)).await;
        }
        was
    }

    /// Adds a pair (on a thread of its own: it holds databases across
    /// waits) and starts its agent.
    async fn add_pair(dir: &Path, req: oneup_app::pairs::Request) -> Result<Value> {
        let http = oneup_app::setup::http()?;
        let auth = oneup_app::setup::auth(dir, None, &http)?;
        if !auth.is_signed_in()? {
            bail!("sign in first");
        }
        let (d, rt) = (dir.to_path_buf(), tokio::runtime::Handle::current());
        let e = tokio::task::spawn_blocking(move || rt.block_on(oneup_app::pairs::add(&d, http, auth, &req))).await??;
        drop(oneup_app::launch::start_agent(&e.dir)?);
        Ok(json!({ "name": e.name }))
    }

    fn flag(args: &Value, k: &str) -> bool {
        args.get(k).and_then(Value::as_bool) == Some(true)
    }

    fn graph(dir: &Path) -> Result<Arc<oneup_graph::Graph>> {
        let http = oneup_app::setup::http()?;
        let auth = oneup_app::setup::auth(dir, None, &http)?;
        Ok(Arc::new(oneup_graph::Graph::new(http, auth, oneup_graph::Aimd::new(2, 4))))
    }

    /// A vault's keys: remembered on this PC, through Windows Hello (`hello`),
    /// or from the password in `args`.
    async fn vault_keys(dir: &Path, e: &vaultpair::Entry, args: &Value) -> Result<oneup_vault::key::Masterkey> {
        if let Some(k) = vaultpair::load_key(&e.dir)? {
            return Ok(k);
        }
        let pw = args.get("password").and_then(Value::as_str).unwrap_or_default();
        if !pw.is_empty() {
            return vaultpair::keys(dir, &graph(dir)?, e, pw).await;
        }
        if e.dir.join(vaultpair::HELLO_FILE).exists() {
            let d = e.dir.clone();
            if let Some(k) = tokio::task::spawn_blocking(move || vaultpair::load_key_hello(&d)).await?? {
                return Ok(k);
            }
        }
        bail!("type the vault's password")
    }

    fn vault(dir: &Path, args: &Value) -> Result<vaultpair::Entry> {
        let name = args.get("name").and_then(Value::as_str).unwrap_or_default();
        vaultpair::list(dir)
            .into_iter()
            .find(|e| e.name.eq_ignore_ascii_case(name))
            .with_context(|| format!("no vault called {name}"))
    }

    /// Runs `oneup` without a window and returns what it printed; an error
    /// carries the output too.
    async fn oneup(args: Vec<std::ffi::OsString>) -> Result<String> {
        const CREATE_NO_WINDOW: u32 = 0x0800_0000;
        let exe = oneup_app::launch::sibling("oneup.exe")?;
        let out = tokio::task::spawn_blocking(move || {
            std::process::Command::new(exe)
                .args(args)
                .creation_flags(CREATE_NO_WINDOW)
                .stdin(std::process::Stdio::null())
                .output()
        })
        .await?
        .context("running oneup")?;
        let text = format!("{}{}", String::from_utf8_lossy(&out.stdout), String::from_utf8_lossy(&out.stderr));
        if !out.status.success() {
            bail!("{}", text.trim());
        }
        Ok(text)
    }

    /// The data folder of a Files-page root: `main`, `pair:<name>` or
    /// `vault:<name>`.
    fn root_dir(main: &Path, root: &str) -> Result<PathBuf> {
        if root == "main" {
            return Ok(main.to_path_buf());
        }
        if let Some(name) = root.strip_prefix("pair:") {
            return Ok(oneup_app::pairs::find(main, name)?.dir);
        }
        if let Some(name) = root.strip_prefix("vault:") {
            return oneup_app::vaultpair::list(main)
                .into_iter()
                .find(|e| e.name == name)
                .map(|e| e.dir)
                .with_context(|| format!("no vault called {name}"));
        }
        bail!("unknown folder {root}")
    }

    /// A count the page asks for, `default` if it doesn't, at most 20,000
    /// (more rows than that is too many for a page).
    fn count(args: &Value, k: &str, default: usize) -> usize {
        args.get(k).and_then(Value::as_u64).map_or(default, |n| n as usize).clamp(1, 20_000)
    }

    fn text(args: &Value, k: &str) -> String {
        args.get(k).and_then(Value::as_str).unwrap_or_default().to_string()
    }

    async fn handle(dir: &Path, op: &str, args: &Value, poster: &Poster) -> Result<Value> {
        match op {
            "roots" => {
                let mut out = Vec::new();
                if let Ok(store) = oneup_app::setup::open_store(dir, 1024)
                    && let Ok((local, remote, on_demand)) = oneup_app::filetree::pair_info(&store)
                {
                    out.push(json!({ "id": "main", "kind": "main", "name": "Main folder",
                        "local": local.to_string_lossy().trim_start_matches(r"\\?\"), "remote": remote,
                        "on_demand": on_demand, "running": oneup_ipc::connect(dir).await.is_ok(),
                        "backup": oneup_app::flow::get(&store)? == oneup_app::flow::Flow::Backup }));
                }
                for mut p in settingsapi::pairs_json(dir)?.as_array().cloned().unwrap_or_default() {
                    let name = text(&p, "name");
                    let running = match oneup_app::pairs::find(dir, &name) {
                        Ok(e) => oneup_ipc::connect(&e.dir).await.is_ok(),
                        Err(_) => false,
                    };
                    p["running"] = Value::from(running);
                    p["id"] = Value::from(format!("pair:{name}"));
                    out.push(p);
                }
                for v in settingsapi::snapshot(dir)?["vaults"].as_array().cloned().unwrap_or_default() {
                    let mut v = v;
                    let name = text(&v, "name");
                    let running = match root_dir(dir, &format!("vault:{name}")) {
                        Ok(d) => oneup_ipc::connect(&d).await.is_ok(),
                        Err(_) => false,
                    };
                    v["running"] = Value::from(running);
                    v["kind"] = Value::from("vault");
                    v["id"] = Value::from(format!("vault:{name}"));
                    out.push(v);
                }
                Ok(Value::from(out))
            }
            "tree" => {
                let (d, rel) = (dir.to_path_buf(), text(args, "rel"));
                tokio::task::spawn_blocking(move || oneup_app::filetree::list(&d, &rel)).await?
            }
            "remove_preview" => {
                // What stopping a folder's sync would take away from this PC.
                let store = oneup_app::setup::open_store(dir, 1024)?;
                let (local, _, on_demand) = oneup_app::filetree::pair_info(&store)?;
                let (files, bytes) = if on_demand { store.read(|o| o.local_online_only())? } else { (0, 0) };
                let free = oneup_sys::disk::free_bytes(&local).unwrap_or(0);
                Ok(json!({ "on_demand": on_demand, "online_files": files, "online_bytes": bytes, "free_bytes": free }))
            }
            "main_unpair" => {
                stop_and_wait(dir).await;
                let d = dir.to_path_buf();
                let local = tokio::task::spawn_blocking(move || oneup_app::pairs::unpair(&d)).await??;
                Ok(Value::from(local.to_string_lossy().into_owned()))
            }
            "tree_sizes" => {
                let (d, rel) = (dir.to_path_buf(), text(args, "rel"));
                tokio::task::spawn_blocking(move || oneup_app::filetree::sizes(&d, &rel)).await?
            }
            "tree_find" => {
                let (d, q) = (dir.to_path_buf(), text(args, "q"));
                tokio::task::spawn_blocking(move || oneup_app::filetree::find(&d, &q, 100)).await?
            }
            "update_check" => {
                let Some(um) = oneup_app::updates::manager(dir)? else {
                    return Ok(json!({ "title": "Updates unavailable",
                        "detail": "This copy wasn't installed with the installer." }));
                };
                let ready = tokio::task::spawn_blocking(move || oneup_app::updates::check(um)).await??;
                // The tray installs it (it stops and restarts syncing around it).
                let told = ready.is_some()
                    && oneup_sys::signal::raise(&oneup_app::updates::check_signal(dir)).unwrap_or(false);
                let auto = oneup_app::config::load_updates(dir).is_ok_and(|u| u.auto);
                Ok(match ready {
                    Some(r) => json!({ "title": format!("Version {} is ready", r.version),
                        "detail": if !told {
                            "Start OneUpDrive from the Start menu to install it."
                        } else if auto {
                            "Installs when sync is idle."
                        } else {
                            "Install it from the OneUpDrive icon's menu."
                        } }),
                    None => {
                        json!({ "title": "Up to date", "detail": format!("Version {}", env!("CARGO_PKG_VERSION")) })
                    }
                })
            }
            "rule_files" => {
                let d = dir.to_path_buf();
                Ok(Value::from(tokio::task::spawn_blocking(move || oneup_app::filetree::rule_files(&d)).await??))
            }
            "tree_sync" => {
                let msg =
                    oneup_app::filetree::set_sync(dir, &text(args, "rel"), flag(args, "folder"), flag(args, "on"))?;
                let _ = agent(dir, Call::SyncNow).await;
                Ok(Value::from(msg))
            }
            "tree_pause" => {
                let msg = oneup_app::filetree::set_paused(dir, &text(args, "rel"), flag(args, "on"))?;
                let _ = agent(dir, Call::SyncNow).await;
                Ok(Value::from(msg))
            }
            "tree_order" => {
                let (rel, folder) = (text(args, "rel"), flag(args, "folder"));
                if let Some(which) = args.get("which").and_then(Value::as_str) {
                    oneup_app::filetree::set_order(dir, &rel, folder, which)?;
                    let _ = agent(dir, Call::SyncNow).await;
                }
                Ok(Value::from(oneup_app::filetree::order(dir, &rel, folder)?))
            }
            "open_path" => {
                let path = args.get("path").and_then(Value::as_str).context("no path")?;
                oneup_sys::assoc::open(path)?;
                Ok(Value::Null)
            }
            "pick_folder" => {
                let p = poster.clone();
                let picked = tokio::task::spawn_blocking(move || p.pick_folder("Choose the folder to sync")).await??;
                Ok(picked.map_or(Value::Null, |p| Value::from(p.to_string_lossy().into_owned())))
            }
            "pick_file" => {
                let p = poster.clone();
                let picked = tokio::task::spawn_blocking(move || p.pick_file("Choose a file or folder")).await??;
                Ok(picked.map_or(Value::Null, |p| Value::from(p.to_string_lossy().into_owned())))
            }
            "apps" => {
                let days = args.get("days").and_then(Value::as_u64).unwrap_or(7);
                oneup_app::apps::summary(dir, u32::try_from(days).unwrap_or(7))
            }
            "app_files" => {
                let exe = args.get("exe").and_then(Value::as_str);
                let limit = args.get("limit").and_then(Value::as_u64).unwrap_or(50);
                oneup_app::apps::recent_files(dir, exe, usize::try_from(limit).unwrap_or(50).min(500))
            }
            "largest" => {
                let d = dir.to_path_buf();
                tokio::task::spawn_blocking(move || settingsapi::largest_json(&d, 10)).await?
            }
            "reveal" => {
                let path = args.get("path").and_then(Value::as_str).context("no path")?;
                std::process::Command::new("explorer.exe")
                    .arg(format!("/select,{path}"))
                    .spawn()
                    .context("opening Explorer")?;
                Ok(Value::Null)
            }
            "placement" => {
                let path = args.get("path").and_then(Value::as_str).context("no path")?;
                let verb = if args.get("keep").and_then(Value::as_bool) == Some(true) { "pin" } else { "free" };
                let a: Vec<std::ffi::OsString> = vec!["--data-dir".into(), dir.into(), verb.into(), path.into()];
                Ok(Value::from(oneup(a).await?))
            }
            "explain" => {
                let path = args.get("path").and_then(Value::as_str).context("no path")?;
                let a: Vec<std::ffi::OsString> = vec!["--data-dir".into(), dir.into(), "explain".into(), path.into()];
                Ok(Value::from(oneup(a).await?))
            }
            "on_demand" => {
                // Turning it on or off needs the data folder to itself.
                let on = args.get("on").and_then(Value::as_bool).context("on or off")?;
                let was_running = agent(dir, Call::Stop).await.is_ok();
                for _ in 0..60 {
                    if oneup_ipc::connect(dir).await.is_err() {
                        break;
                    }
                    tokio::time::sleep(std::time::Duration::from_millis(500)).await;
                }
                let a: Vec<std::ffi::OsString> =
                    vec!["--data-dir".into(), dir.into(), "on-demand".into(), if on { "on" } else { "off" }.into()];
                let out = oneup(a).await;
                if was_running {
                    oneup_app::launch::start_agent(dir)?;
                }
                Ok(Value::from(out?))
            }
            "vault_add" => {
                let s = |k: &str| args.get(k).and_then(Value::as_str).unwrap_or_default().trim().to_string();
                let (remote, local, password) = (s("remote"), s("local"), s("password"));
                if remote.is_empty() || local.is_empty() || password.is_empty() {
                    bail!("fill in the vault's OneDrive folder, a folder on this PC and the password");
                }
                let full = args.get("full").and_then(Value::as_bool) == Some(true);
                let remember = args.get("remember").and_then(Value::as_bool) == Some(true);
                let http = oneup_app::setup::http()?;
                let auth = oneup_app::setup::auth(dir, None, &http)?;
                let graph = Arc::new(oneup_graph::Graph::new(http, auth, oneup_graph::Aimd::new(2, 4)));
                // It holds the database across waits, so it runs on a thread of its own.
                let (d, rt) = (dir.to_path_buf(), tokio::runtime::Handle::current());
                let added = tokio::task::spawn_blocking(move || {
                    rt.block_on(vaultpair::add(&d, graph, &remote, Path::new(&local), !full, &password, remember))
                })
                .await??;
                drop(oneup_app::launch::start_vault_agent(&added.entry.dir, &added.key)?);
                Ok(json!({ "name": added.entry.name }))
            }
            "vault_create" => {
                let s = |k: &str| args.get(k).and_then(Value::as_str).unwrap_or_default().trim().to_string();
                let (remote, local) = (s("remote"), s("local"));
                let password = args.get("password").and_then(Value::as_str).unwrap_or_default().to_string();
                if remote.is_empty() || local.is_empty() {
                    bail!("fill in the new vault's OneDrive folder and a folder on this PC");
                }
                let (full, remember) = (flag(args, "full"), flag(args, "remember"));
                let g = graph(dir)?;
                let (d, rt) = (dir.to_path_buf(), tokio::runtime::Handle::current());
                let added = tokio::task::spawn_blocking(move || {
                    rt.block_on(vaultpair::create(&d, g, &remote, Path::new(&local), !full, &password, remember))
                })
                .await??;
                drop(oneup_app::launch::start_vault_agent(&added.entry.dir, &added.key)?);
                let recovery = oneup_vault::recovery::recovery_key(&added.key);
                Ok(json!({ "name": added.entry.name, "recovery": recovery.as_str() }))
            }
            "vault_recovery" => {
                let e = vault(dir, args)?;
                let key = vault_keys(dir, &e, args).await?;
                Ok(Value::from(oneup_vault::recovery::recovery_key(&key).as_str()))
            }
            "vault_password" => {
                let e = vault(dir, args)?;
                let new = args.get("new").and_then(Value::as_str).unwrap_or_default().to_string();
                let key = match args.get("recovery").and_then(Value::as_str).filter(|r| !r.trim().is_empty()) {
                    Some(r) => vaultpair::keys_from_recovery(dir, &graph(dir)?, &e, r).await?,
                    None => vault_keys(dir, &e, args).await?,
                };
                vaultpair::set_password(dir, graph(dir)?, &e, &key, &new).await?;
                Ok(Value::Null)
            }
            "vault_hello" => {
                let e = vault(dir, args)?;
                if flag(args, "on") {
                    let key = vault_keys(dir, &e, args).await?;
                    let d = e.dir.clone();
                    tokio::task::spawn_blocking(move || vaultpair::save_key_hello(&d, &key)).await??;
                } else {
                    vaultpair::forget_hello(&e.dir)?;
                }
                Ok(Value::Null)
            }
            "vault_remove" => {
                let e = vault(dir, args)?;
                let a: Vec<std::ffi::OsString> = vec![
                    "--data-dir".into(),
                    dir.into(),
                    "vault".into(),
                    "remove".into(),
                    e.name.into(),
                    "--yes".into(),
                ];
                Ok(Value::from(oneup(a).await?))
            }
            "sign_in" => {
                // `oneup login` (with its checks for a test account), hidden;
                // it opens the browser, where signing in is the person's, and
                // ends once they're done (or it failed).
                let mut a: Vec<std::ffi::OsString> = vec!["--data-dir".into(), dir.into(), "login".into()];
                if oneup_app::test_account::load(dir)?.is_some() {
                    a.push("--test-account".into());
                }
                // Also reading what others share (Folders shared with you).
                if flag(args, "shared") {
                    a.push("--shared-folders".into());
                }
                Ok(Value::from(oneup(a).await?))
            }
            "activity" => {
                let (d, rel) = (dir.to_path_buf(), text(args, "rel"));
                tokio::task::spawn_blocking(move || settingsapi::activity(&d, &rel, 40)).await?
            }
            // The Activity page's lists.
            "activity_log" => {
                let (d, limit) = (dir.to_path_buf(), count(args, "limit", 200));
                tokio::task::spawn_blocking(move || settingsapi::activity(&d, "", limit)).await?
            }
            "unsynced" => {
                let d = dir.to_path_buf();
                tokio::task::spawn_blocking(move || settingsapi::unsynced(&d)).await?
            }
            "unsynced_preview" => {
                let (d, choice) = (dir.to_path_buf(), text(args, "choice"));
                let path = args.get("path").and_then(Value::as_str).map(str::to_string);
                let name = args.get("name").and_then(Value::as_str).map(str::to_string);
                tokio::task::spawn_blocking(move || {
                    settingsapi::unsynced_preview(&d, path.as_deref(), &choice, name.as_deref())
                })
                .await?
            }
            "unsynced_inside" => {
                let (d, path) = (dir.to_path_buf(), text(args, "path"));
                tokio::task::spawn_blocking(move || settingsapi::unsynced_inside(&d, &path)).await?
            }
            // A sync failure's button: `oneup unsynced resolve`, with the same
            // checks as typed. The page asked before a deletion, so it goes
            // ahead (`--yes`); a choice for every item of a kind says how many
            // the page showed, refused if the list changed since.
            "unsynced_resolve" => {
                let choice = text(args, "choice");
                if oneup_app::resolve::Choice::from_code(&choice).is_none() {
                    bail!("unknown choice: {choice}");
                }
                let mut a: Vec<std::ffi::OsString> =
                    vec!["--data-dir".into(), dir.into(), "unsynced".into(), "resolve".into()];
                if flag(args, "all") {
                    a.push("--all".into());
                } else {
                    a.push(text(args, "path").into());
                }
                if let Some(n) = args.get("expect").and_then(Value::as_u64) {
                    a.extend(["--expect".into(), n.to_string().into()]);
                }
                a.push(
                    if choice == "rename" { format!("--rename={}", text(args, "name")) } else { format!("--{choice}") }
                        .into(),
                );
                a.push("--yes".into());
                Ok(Value::from(oneup(a).await?))
            }
            "stops" => {
                let (d, filter, limit) = (dir.to_path_buf(), text(args, "filter"), count(args, "limit", 500));
                tokio::task::spawn_blocking(move || settingsapi::stops(&d, &filter, limit)).await?
            }
            "cloud_copies" => {
                let http = oneup_app::setup::http()?;
                let auth = oneup_app::setup::auth(dir, None, &http)?;
                if !auth.is_signed_in()? {
                    bail!("sign in first");
                }
                let graph = oneup_graph::Graph::new(http, auth, oneup_graph::Aimd::new(2, 4));
                settingsapi::cloud_copies(dir, &graph).await
            }
            "kept" => {
                let d = dir.to_path_buf();
                tokio::task::spawn_blocking(move || settingsapi::kept(&d)).await?
            }
            "kept_restore" => {
                // The agent is stopped around it: restoring writes into the folder.
                let (d, id) = (dir.to_path_buf(), text(args, "id"));
                let was = stop_and_wait(&d).await;
                let d2 = d.clone();
                let at = tokio::task::spawn_blocking(move || settingsapi::kept_restore(&d2, &id)).await;
                if was {
                    oneup_app::launch::start_agent(&d)?;
                }
                Ok(Value::from(at??))
            }
            "shortcuts" => {
                let d = dir.to_path_buf();
                tokio::task::spawn_blocking(move || -> Result<Value> {
                    let s = oneup_app::shortcuts::find(&d, None)?;
                    Ok(oneup_app::shortcuts::scan_json(&d, &s))
                })
                .await?
            }
            "shortcuts_fix" => {
                let d = dir.to_path_buf();
                let links: Vec<String> = args
                    .get("links")
                    .and_then(Value::as_array)
                    .map(|a| a.iter().filter_map(Value::as_str).map(str::to_owned).collect())
                    .unwrap_or_default();
                tokio::task::spawn_blocking(move || -> Result<Value> {
                    let s = oneup_app::shortcuts::find(&d, None)?;
                    let chosen: Vec<_> =
                        s.found.into_iter().filter(|f| links.iter().any(|l| Path::new(l) == f.link)).collect();
                    let o = oneup_app::shortcuts::fix(&d, &chosen)?;
                    Ok(json!({ "changed": o.changed, "skipped": o.skipped.len() }))
                })
                .await?
            }
            "shortcuts_undo" => {
                let d = dir.to_path_buf();
                tokio::task::spawn_blocking(move || -> Result<Value> {
                    let o = oneup_app::shortcuts::undo(&d)?;
                    Ok(json!({ "changed": o.changed, "skipped": o.skipped.len() }))
                })
                .await?
            }
            "sync_first" => agent(dir, Call::SyncFirst { path: text(args, "rel") }).await,
            "sign_out" => {
                // Syncing stops with it; files stay where they are.
                stop_and_wait(dir).await;
                oneup(vec!["--data-dir".into(), dir.into(), "logout".into()]).await?;
                Ok(Value::Null)
            }
            // Moving over from the OneDrive app (`oneup_app::moveover`): the
            // app's folder now, then the read-only steps of the check, each
            // its own `oneup` run so the window can say which is under way.
            "move_state" => Ok(oneup_app::moveover::app().map_or(Value::Null, |a| a.to_json())),
            "move_scan" | "move_list" => {
                let app = oneup_app::moveover::app().context("no OneDrive app folder on this PC")?;
                let preset = text(args, "preset");
                if oneup_app::setup::preset_rules(&preset).is_none() {
                    bail!("unknown list of things to exclude: {preset}");
                }
                let mut a: Vec<std::ffi::OsString> = vec!["--data-dir".into(), dir.into()];
                if op == "move_scan" {
                    a.extend(["scan".into(), app.folder.into()]);
                } else {
                    a.push("mirror".into());
                }
                a.extend(["--exclude".into(), preset.into()]);
                oneup(a).await?;
                Ok(Value::Null)
            }
            // A stalled unlinking: one step of `oneup migrate finish-unlink`
            // (restart, reset or release), its result the last line printed.
            "move_fix" => {
                let step = text(args, "step");
                if !["restart", "reset", "release"].contains(&step.as_str()) {
                    bail!("unknown step: {step}");
                }
                let a: Vec<std::ffi::OsString> = vec![
                    "--data-dir".into(),
                    dir.into(),
                    "migrate".into(),
                    "finish-unlink".into(),
                    "--step".into(),
                    step.into(),
                    "--yes".into(),
                    "--json".into(),
                ];
                let out = oneup(a).await?;
                out.lines()
                    .rev()
                    .find_map(|l| serde_json::from_str::<Value>(l.trim()).ok().filter(Value::is_object))
                    .context("finishing the unlinking said nothing")
            }
            "move_check" => {
                let a: Vec<std::ffi::OsString> =
                    vec!["--data-dir".into(), dir.into(), "migrate".into(), "check".into(), "--json".into()];
                Ok(serde_json::from_str(oneup(a).await?.trim())?)
            }
            "setup_preview" => {
                let s = settingsapi::Setup::from_json(args)?;
                Ok(Value::from(oneup(s.preview(dir)).await?))
            }
            "setup_start" => {
                let s = settingsapi::Setup::from_json(args)?;
                let text = oneup(s.pair(dir)).await?;
                if flag(args, "autostart") {
                    let launcher = oneup_app::launch::sibling("oneupd-start.exe")?;
                    oneup_app::launch::set_autostart(oneup_app::launch::RUN_KEY, dir, Some(&launcher))?;
                }
                oneup_app::launch::start_agent(dir)?;
                oneup_app::launch::start_tray_if_wanted(dir);
                Ok(Value::from(text))
            }
            "snapshot" => settingsapi::snapshot(dir),
            "status" => Ok(agent(dir, Call::Status).await.unwrap_or(Value::Null)),
            // Why the agent stopped, if an error stopped it.
            "stopped" => Ok(json!(oneup_app::problem::load_stopped(dir))),
            "save" => {
                let section = args.get("section").and_then(Value::as_str).unwrap_or_default();
                settingsapi::save(dir, section, args.get("value").unwrap_or(&Value::Null))?;
                // The agent re-reads its settings when nudged.
                let _ = agent(dir, Call::SyncNow).await;
                Ok(Value::Null)
            }
            "control" => {
                let call = match args.get("call").and_then(Value::as_str) {
                    Some("sync_now") => Call::SyncNow,
                    Some("pause") => Call::Pause { direction: Direction::Both, minutes: None },
                    Some("resume") => Call::Resume { direction: Direction::Both },
                    Some("stop") => Call::Stop,
                    // Accepts the agent's offer to bring excluded items here
                    // and remove them from OneDrive (by its code).
                    Some("prune") => Call::Answer { code: text(args, "code"), answer: Answer::Prune },
                    // Settles the offer: the items stay in OneDrive.
                    Some("keep_in_cloud") => Call::Answer { code: text(args, "code"), answer: Answer::KeepInCloud },
                    _ => bail!("unknown control call"),
                };
                agent(dir, call).await
            }
            "start" => {
                oneup_app::launch::start_agent(dir)?;
                Ok(Value::Null)
            }
            "open" => {
                match args.get("what").and_then(Value::as_str) {
                    Some("folder") => {
                        let s = settingsapi::snapshot(dir)?;
                        let local = s.pointer("/pair/local").and_then(Value::as_str).context("no synced folder yet")?;
                        open(Path::new(local))?;
                    }
                    Some("data") => open(dir)?,
                    Some("log") => open(&dir.join(oneup_app::launch::LOG_FILE))?,
                    Some("kept") => {
                        let s = settingsapi::snapshot(dir)?;
                        let kept = s.get("kept_folder").and_then(Value::as_str).context("no synced folder yet")?;
                        open(Path::new(kept))?;
                    }
                    Some("vault") => {
                        let (local, _) = vaultpair::describe(&vault(dir, args)?)?;
                        open(&local)?;
                    }
                    Some("pair") => {
                        let l = oneup_app::pairs::describe(&pair(dir, args)?)?.local.to_string_lossy().into_owned();
                        open(Path::new(l.strip_prefix(r"\\?\").unwrap_or(&l)))?;
                    }
                    _ => bail!("nothing to open"),
                }
                Ok(Value::Null)
            }
            "office_open" => {
                let path = args.get("path").and_then(Value::as_str).context("no file")?;
                let uri = oneup_app::item::office_uri(Path::new(path)).await?;
                oneup_app::item::launch(&uri)?;
                Ok(Value::Null)
            }
            "office_opener" => {
                if flag(args, "on") {
                    let exe = oneup_app::launch::sibling("oneup-open.exe")?;
                    let types = oneup_app::officeopen::register(&exe)?;
                    oneup_app::officeopen::choose_in_settings()?;
                    Ok(Value::from(types))
                } else {
                    Ok(Value::from(oneup_app::officeopen::unregister()?))
                }
            }
            "office_defaults" => {
                oneup_app::officeopen::choose_in_settings()?;
                Ok(Value::Null)
            }
            "office_integration" => {
                if flag(args, "on") {
                    Ok(Value::from(oneup_app::officeint::turn_on(dir).await?))
                } else {
                    oneup_app::officeint::turn_off(dir)?;
                    Ok(Value::Null)
                }
            }
            "item_versions" => {
                let path = args.get("path").and_then(Value::as_str).context("no file")?;
                oneup_app::item::versions_json(Path::new(path)).await
            }
            "item_open" => {
                let s = |k: &str| args.get(k).and_then(Value::as_str).unwrap_or_default().to_string();
                let to = oneup_app::item::open_version(Path::new(&s("path")), &s("id"), &s("label")).await?;
                std::process::Command::new("explorer.exe").arg(&to).spawn().context("opening it")?;
                Ok(Value::from(to.to_string_lossy().into_owned()))
            }
            "item_restore" => {
                let s = |k: &str| args.get(k).and_then(Value::as_str).unwrap_or_default().to_string();
                oneup_app::item::restore_version(Path::new(&s("path")), &s("id")).await?;
                Ok(Value::Null)
            }
            "item_share" => {
                let path = args.get("path").and_then(Value::as_str).context("no file")?;
                let link = oneup_app::item::share(Path::new(path), flag(args, "edit")).await?;
                oneup_sys::clipboard::set_text(&link)?;
                Ok(Value::from(link))
            }
            "pairs" => {
                let list = settingsapi::pairs_json(dir)?;
                let mut out = Vec::new();
                for mut p in list.as_array().cloned().unwrap_or_default() {
                    let name = p.get("name").and_then(Value::as_str).unwrap_or_default().to_string();
                    let running = match oneup_app::pairs::find(dir, &name) {
                        Ok(e) => oneup_ipc::connect(&e.dir).await.is_ok(),
                        Err(_) => false,
                    };
                    p["running"] = Value::from(running);
                    out.push(p);
                }
                Ok(Value::from(out))
            }
            "known" => settingsapi::known_json(dir),
            "pair_add" => {
                let s = |k: &str| args.get(k).and_then(Value::as_str).unwrap_or_default().trim().to_string();
                let (remote, local) = (s("remote"), s("local"));
                if remote.is_empty() || local.is_empty() {
                    bail!("fill in the OneDrive folder and the folder on this PC");
                }
                let backup = flag(args, "backup");
                let req = oneup_app::pairs::Request {
                    cloud: oneup_app::pairs::Cloud::Path(remote),
                    local: PathBuf::from(local),
                    flow: if backup { oneup_app::flow::Flow::Backup } else { oneup_app::flow::Flow::TwoWay },
                    on_demand: flag(args, "on_demand") && !backup,
                    known: None,
                };
                add_pair(dir, req).await
            }
            "pair_add_known" => {
                let name = args.get("name").and_then(Value::as_str).unwrap_or_default();
                let k = oneup_sys::known::Known::parse(name).context("not one of Windows' folders")?;
                let flow =
                    if flag(args, "two_way") { oneup_app::flow::Flow::TwoWay } else { oneup_app::flow::Flow::Backup };
                add_pair(dir, oneup_app::pairs::known_request(k, flow)?).await
            }
            "shared_list" => {
                let http = oneup_app::setup::http()?;
                let auth = oneup_app::setup::auth(dir, None, &http)?;
                let graph = oneup_graph::Graph::new(http, auth, oneup_graph::Aimd::new(2, 4));
                let found = graph.shared_folders().await.map_err(|e| match e.status() {
                    Some(401 | 403) => anyhow::anyhow!(
                        "OneDrive didn't allow listing what's shared with you. That needs a broader permission: \
                         sign in again and allow reading files others share."
                    ),
                    _ => e.into(),
                })?;
                Ok(Value::from(
                    found
                        .into_iter()
                        .map(|f| json!({ "name": f.name, "owner": f.owner, "drive_id": f.drive_id, "id": f.id }))
                        .collect::<Vec<_>>(),
                ))
            }
            "pair_add_shared" => {
                let s = |k: &str| args.get(k).and_then(Value::as_str).unwrap_or_default().trim().to_string();
                let local = s("local");
                if local.is_empty() {
                    bail!("choose a folder on this PC");
                }
                let shared = oneup_graph::SharedFolder {
                    name: s("name"),
                    drive_id: s("drive_id"),
                    id: s("id"),
                    owner: Some(s("owner")).filter(|o| !o.is_empty()),
                };
                let backup = flag(args, "backup");
                let req = oneup_app::pairs::Request {
                    cloud: oneup_app::pairs::Cloud::Shared(shared),
                    local: PathBuf::from(local),
                    flow: if backup { oneup_app::flow::Flow::Backup } else { oneup_app::flow::Flow::TwoWay },
                    on_demand: flag(args, "on_demand") && !backup,
                    known: None,
                };
                add_pair(dir, req).await
            }
            "pair_start" => {
                drop(oneup_app::launch::start_agent(&pair(dir, args)?.dir)?);
                Ok(Value::Null)
            }
            "pair_stop" => {
                agent(&pair(dir, args)?.dir, Call::Stop).await?;
                Ok(Value::Null)
            }
            "pair_remove" => {
                let e = pair(dir, args)?;
                stop_and_wait(&e.dir).await;
                let d = tokio::task::spawn_blocking(move || oneup_app::pairs::remove(&e)).await??;
                Ok(Value::from(d.local.to_string_lossy().into_owned()))
            }
            "flow" => {
                // For the main pair (no name) or an extra one; the change
                // needs the data folder to itself.
                let target = match args.get("name").and_then(Value::as_str) {
                    Some(_) => pair(dir, args)?.dir,
                    None => dir.to_path_buf(),
                };
                let to = if flag(args, "backup") { "backup" } else { "both" };
                let was_running = stop_and_wait(&target).await;
                let a: Vec<std::ffi::OsString> =
                    vec!["--data-dir".into(), target.clone().into(), "flow".into(), to.into()];
                let out = oneup(a).await;
                if was_running {
                    oneup_app::launch::start_agent(&target)?;
                }
                Ok(Value::from(out?))
            }
            "accounts" => Ok(settingsapi::accounts_json(dir)),
            "account_add" => {
                let name = args.get("name").and_then(Value::as_str).unwrap_or_default();
                let new = oneup_app::accounts::add(name)?;
                oneup_app::launch::start_settings(&new)?;
                Ok(Value::Null)
            }
            "account_open" => {
                let want = args.get("dir").and_then(Value::as_str).context("which account")?;
                let a = oneup_app::accounts::list()
                    .into_iter()
                    .find(|a| oneup_app::test_account::same_dir(&a.dir, Path::new(want)))
                    .context("no such account")?;
                oneup_app::launch::start_settings(&a.dir)?;
                Ok(Value::Null)
            }
            "vault_state" => {
                let e = vault(dir, args)?;
                let running = oneup_ipc::connect(&e.dir).await.is_ok();
                Ok(json!({ "running": running }))
            }
            "vault_lock" => {
                let e = vault(dir, args)?;
                agent(&e.dir, Call::Stop).await?;
                Ok(Value::Null)
            }
            "vault_unlock" => {
                let e = vault(dir, args)?;
                let key = match args.get("password").and_then(Value::as_str) {
                    Some(pw) if !pw.is_empty() => {
                        let key = vaultpair::keys(dir, &graph(dir)?, &e, pw).await?;
                        if args.get("remember").and_then(Value::as_bool) == Some(true) {
                            vaultpair::save_key(&e.dir, &key)?;
                        }
                        Some(key)
                    }
                    _ if flag(args, "hello") => {
                        let d = e.dir.clone();
                        tokio::task::spawn_blocking(move || vaultpair::load_key_hello(&d)).await??
                    }
                    _ => None,
                };
                match key {
                    Some(k) => drop(oneup_app::launch::start_vault_agent(&e.dir, &k)?),
                    None if e.dir.join(vaultpair::KEY_FILE).exists() => drop(oneup_app::launch::start_agent(&e.dir)?),
                    None => bail!("type the vault's password"),
                }
                Ok(Value::Null)
            }
            other => bail!("unknown request {other}"),
        }
    }

    #[cfg(test)]
    mod tests {
        #![allow(clippy::unwrap_used)]
        use super::*;

        /// `handle` gets a root's own data folder: looking the root up again
        /// from there fails (it broke Recent activity, Kept on this PC and
        /// Sync this first for folder pairs and vaults).
        #[test]
        fn a_root_resolves_once() {
            let main = std::env::temp_dir().join(format!("oneup-settings-root-{}", std::process::id()));
            let _ = std::fs::remove_dir_all(&main);
            let pair = main.join(oneup_app::pairs::PAIRS).join("docs");
            std::fs::create_dir_all(&pair).unwrap();
            std::fs::write(pair.join(oneup_app::pairs::MARKER), "/Docs").unwrap();
            assert_eq!(root_dir(&main, "main").unwrap(), main);
            let dir = root_dir(&main, "pair:docs").unwrap();
            assert_eq!(dir, pair);
            assert!(root_dir(&dir, "pair:docs").is_err());
            std::fs::remove_dir_all(&main).unwrap();
        }
    }
}
