//! `oneup-tray`: OneUpDrive's icon in the notification area. A thin client
//! of the agent's control pipe (`oneup_ipc`): it shows how syncing goes,
//! offers pause, resume, sync now and the decisions the agent waits for,
//! and notifies about those. It changes nothing by itself; every action is
//! a call the CLI could make too.
//!
//! Two threads: this one runs the icon (Windows' message loop); a second
//! one follows the agent with long-polling `watch` calls, so nothing polls
//! while the agent is up. While it isn't, it looks again after a growing
//! delay (up to a minute), or at once after "Start" from the menu.
#![windows_subsystem = "windows"]

#[cfg(windows)]
fn main() {
    // Installing, updating or uninstalling calls this program first.
    oneup_app::install::hooks();
    oneup_app::setup::tls();
    if let Err(e) = win::run() {
        oneup_sys::tray::error_box("OneUpDrive", &format!("The OneUpDrive icon couldn't start: {e:#}"));
    }
}

#[cfg(not(windows))]
fn main() {}

#[cfg(windows)]
mod win {
    use std::path::{Path, PathBuf};
    use std::sync::{Arc, Mutex};
    use std::time::Duration;

    use anyhow::{Context, Result};
    use oneup_app::tray::{self, Notices, View, id};
    use oneup_ipc::{Answer, Call, ClientError, Direction, Status};
    use oneup_sys::tray::{Buttons, Event, Icon, Kind, Reply, Tray, Waker};
    use tokio::sync::{Notify, mpsc};

    /// What the follower thread found, for the icon's thread.
    struct Shared {
        view: View,
        /// A failed menu action, to show once.
        failure: Option<String>,
        /// An update downloaded and checked, ready to install.
        update: Option<oneup_app::updates::Ready>,
        /// `[search]` changed: take up its hotkey.
        search_changed: bool,
    }

    enum Do {
        Call(Call),
        Start,
    }

    fn data_dir() -> Result<PathBuf> {
        let mut args = std::env::args_os().skip(1);
        while let Some(a) = args.next() {
            if a == "--data-dir" {
                return args.next().map(PathBuf::from).context("--data-dir needs a folder");
            }
        }
        oneup_app::test_account::default_dir()
    }

    pub fn run() -> Result<()> {
        let dir = data_dir()?;
        // From the Start menu (no arguments): the settings window opens (or
        // comes to the front), whether or not the icon is already showing.
        let plain = std::env::args_os().len() == 1;
        let Some(_instance) = oneup_sys::tray::single_instance(&tray::instance_name(&dir))? else {
            if plain {
                oneup_app::launch::start_settings(&dir)?;
            }
            return Ok(()); // Already showing for this data folder.
        };
        // From the Start menu, at sign-in and after an update: syncing
        // starts too (and the vaults whose keys are remembered).
        if plain || std::env::args_os().any(|a| a == "--start") {
            start_all(&dir);
        }
        if plain && !oneup_app::settingsapi::needs_setup(&dir) {
            let _ = oneup_app::launch::start_settings(&dir);
        }
        // Installing an update stopped every account (the updater ends all
        // of OneUpDrive's programs): bring back the other accounts that
        // start at sign-in, each with its own icon.
        if std::env::args_os().any(|a| a == "--after-update") {
            for a in oneup_app::accounts::list() {
                let started = oneup_app::launch::autostart(oneup_app::launch::RUN_KEY, &a.dir).ok().flatten().is_some();
                if started && !oneup_app::test_account::same_dir(&a.dir, &dir) {
                    let _ = oneup_app::launch::start_tray_with(&a.dir, &["--start"]);
                }
            }
        }
        let icon = Tray::new()?;
        let shared =
            Arc::new(Mutex::new(Shared { view: View::Connecting, failure: None, update: None, search_changed: false }));
        {
            let (dir, shared, waker) = (dir.clone(), Arc::clone(&shared), icon.waker());
            std::thread::Builder::new()
                .name("updates".into())
                .spawn(move || look_for_updates(&dir, &shared, &waker))
                .context("starting a thread")?;
        }
        {
            let (dir, shared, waker) = (dir.clone(), Arc::clone(&shared), icon.waker());
            std::thread::Builder::new()
                .name("search settings".into())
                .spawn(move || follow_search_settings(&dir, &shared, &waker))
                .context("starting a thread")?;
        }
        let (tx, rx) = mpsc::unbounded_channel();
        {
            let (dir, shared, waker) = (dir.clone(), Arc::clone(&shared), icon.waker());
            std::thread::Builder::new()
                .name("follow".into())
                .spawn(move || {
                    let Ok(rt) = tokio::runtime::Builder::new_current_thread().enable_all().build() else { return };
                    rt.block_on(follow(dir, shared, waker, rx));
                })
                .context("starting a thread")?;
        }
        let mut ui = Ui {
            icon: &icon,
            dir,
            tx,
            notices: Notices::default(),
            view: View::Connecting,
            look: None,
            shared: Arc::clone(&shared),
            update: None,
            hotkey: String::new(),
        };
        ui.set_hotkey();
        ui.refresh(&shared);
        while let Some(e) = icon.next() {
            match e {
                Event::Wake => ui.refresh(&shared),
                Event::Select { x, y } | Event::Menu { x, y } => ui.menu(x, y),
                Event::NotificationClicked => ui.review_first(),
                Event::Hotkey => ui.show_list("find"),
                Event::Exit => break,
            }
        }
        Ok(())
    }

    struct Ui<'a> {
        icon: &'a Tray,
        dir: PathBuf,
        tx: mpsc::UnboundedSender<Do>,
        notices: Notices,
        view: View,
        look: Option<(tray::Look, String)>,
        shared: Arc<Mutex<Shared>>,
        /// The version of an update ready to install (announced once).
        update: Option<String>,
        /// The search hotkey held now (`[search] hotkey`; empty: none).
        hotkey: String,
    }

    /// Waits for `[search]` to change (`oneup_app::tray::settings_changed`)
    /// and tells the icon's thread, which holds the hotkey.
    fn follow_search_settings(dir: &Path, shared: &Mutex<Shared>, waker: &Waker) {
        let Ok(signal) = oneup_sys::signal::Signal::new(&tray::settings_signal(dir)) else { return };
        loop {
            if signal.wait(Duration::from_secs(24 * 3600)) {
                shared.lock().unwrap_or_else(|e| e.into_inner()).search_changed = true;
                waker.wake();
            }
        }
    }

    /// Starts the agent and the remembered vaults that aren't running.
    fn start_all(dir: &Path) {
        let Ok(rt) = tokio::runtime::Builder::new_current_thread().enable_all().build() else { return };
        // Nothing to sync yet: the settings window's setup comes first.
        if oneup_app::settingsapi::needs_setup(dir) {
            let _ = oneup_app::launch::start_settings(dir);
            return;
        }
        rt.block_on(async {
            if oneup_ipc::connect(dir).await.is_err() {
                let _ = oneup_app::launch::start_agent(dir);
            }
            drop(oneup_app::launch::start_others(dir).await);
        });
    }

    /// A few minutes after starting, then daily, or at once when the
    /// settings window's "Check now" asks: download and check an update
    /// (`oneup_app::updates`), and tell the icon's thread. The daily check
    /// runs only when updates install automatically; asking always checks.
    fn look_for_updates(dir: &Path, shared: &Mutex<Shared>, waker: &Waker) {
        let signal = oneup_sys::signal::Signal::new(&oneup_app::updates::check_signal(dir)).ok();
        let wait = |d: Duration| match &signal {
            Some(s) => s.wait(d),
            None => {
                std::thread::sleep(d);
                false
            }
        };
        let mut asked = wait(Duration::from_secs(150));
        loop {
            let auto = oneup_app::config::load_updates(dir).is_ok_and(|u| u.auto);
            let found = match oneup_app::updates::manager(dir) {
                Ok(Some(um)) if auto || asked => oneup_app::updates::check(um),
                _ => Ok(None),
            };
            match found {
                Ok(Some(ready)) => {
                    shared.lock().unwrap_or_else(|e| e.into_inner()).update = Some(ready);
                    waker.wake();
                    return;
                }
                Ok(None) => {}
                Err(e) => tracing::info!(error = %format!("{e:#}"), "no update installed"),
            }
            asked = wait(Duration::from_secs(24 * 3600));
        }
    }

    impl Ui<'_> {
        fn lock(shared: &Mutex<Shared>) -> std::sync::MutexGuard<'_, Shared> {
            shared.lock().unwrap_or_else(|e| e.into_inner())
        }

        /// Holds the search hotkey `[search]` names (none if it names none),
        /// and says so once if another program holds it.
        fn set_hotkey(&mut self) {
            let wanted = oneup_app::config::load_search(&self.dir).map(|s| s.hotkey).unwrap_or_default();
            if wanted == self.hotkey {
                return;
            }
            let keys = oneup_app::search::Hotkey::parse(&wanted).ok().map(|h| (h.modifiers(), h.key));
            match self.icon.set_hotkey(keys) {
                Ok(()) => self.hotkey = wanted,
                Err(_) => {
                    self.hotkey.clear();
                    let _ = self.icon.notify(
                        "Search hotkey unavailable",
                        &format!("Another program uses {wanted}. Choose other keys in Settings, Search."),
                        Kind::Warning,
                    );
                }
            }
        }

        fn refresh(&mut self, shared: &Mutex<Shared>) {
            let (view, failure, update, search_changed) = {
                let mut s = Self::lock(shared);
                let changed = std::mem::take(&mut s.search_changed);
                (s.view.clone(), s.failure.take(), s.update.as_ref().map(|r| r.version.clone()), changed)
            };
            if search_changed {
                self.set_hotkey();
            }
            let auto = oneup_app::config::load_updates(&self.dir).is_ok_and(|u| u.auto);
            if update.is_some() && self.update.is_none() {
                self.update.clone_from(&update);
                let how = if auto { "installs when sync is idle" } else { "is ready to install from this icon's menu" };
                let _ = self.icon.notify(
                    "OneUpDrive update ready",
                    &format!("Version {} {how}.", update.unwrap_or_default()),
                    Kind::Info,
                );
            }
            let now = (tray::look(&view), tray::tip(&view));
            if self.look.as_ref() != Some(&now) {
                let size = oneup_sys::tray::icon_size();
                match Icon::from_rgba(&tray::render(now.0, size), size) {
                    // Failing leaves the old picture; it's tried again on
                    // the next change (or when Explorer comes back).
                    Ok(i) => {
                        if self.icon.set(i, &now.1).is_ok() {
                            self.look = Some(now);
                        }
                    }
                    Err(e) => tracing::debug!(error = %e, "no icon picture"),
                }
            }
            for n in self.notices.update(&view) {
                let _ = self.icon.notify(&n.title, &n.text, n.kind);
            }
            self.view = view;
            if let Some(f) = failure {
                self.icon.ask("OneUpDrive", &f, Kind::Error, Buttons::Ok);
            }
            // Installs by itself while nothing syncs and no vault would be
            // locked by the restart.
            let idle = match &self.view {
                View::NotRunning | View::Stopped(_) => true,
                View::Running(s) => {
                    matches!(s.activity, oneup_ipc::Activity::Idle)
                        && s.safety_stops.is_empty()
                        && !s.overflow.as_ref().is_some_and(|o| o.waiting)
                }
                _ => false,
            };
            if auto && self.update.is_some() && idle && !self.vault_needs_password() {
                self.install_update();
            }
        }

        /// An unlocked vault whose keys aren't remembered (a restart would
        /// lock it).
        fn vault_needs_password(&self) -> bool {
            oneup_app::vaultpair::list(&self.dir).iter().any(|v| {
                !v.dir.join(oneup_app::vaultpair::KEY_FILE).exists()
                    && std::fs::OpenOptions::new()
                        .write(true)
                        .open(v.dir.join("sync.lock"))
                        .is_err_and(|e| e.raw_os_error() == Some(32))
            })
        }

        /// Stops the agents, installs the update and starts everything again
        /// (this program exits meanwhile).
        fn install_update(&mut self) {
            let Some(ready) = Self::lock(&self.shared).update.take() else { return };
            self.update = None;
            let dir = self.dir.clone();
            // Every account: the updater ends all of OneUpDrive's programs,
            // so each agent is asked to stop at a safe point first.
            let mut dirs: Vec<PathBuf> = Vec::new();
            for a in oneup_app::accounts::list() {
                dirs.extend(oneup_app::launch::children(&a.dir));
                dirs.push(a.dir);
            }
            let shared = Arc::clone(&self.shared);
            let _ = std::thread::Builder::new().name("install".into()).spawn(move || {
                if let Ok(rt) = tokio::runtime::Builder::new_current_thread().enable_all().build() {
                    rt.block_on(oneup_app::install::stop_agents(&dirs, Duration::from_secs(120)));
                }
                let args = vec![
                    "--data-dir".to_string(),
                    dir.display().to_string(),
                    "--start".to_string(),
                    "--after-update".to_string(),
                ];
                if let Err(e) = oneup_app::updates::apply(ready, args) {
                    let mut s = shared.lock().unwrap_or_else(|e| e.into_inner());
                    s.failure = Some(format!("{e:#}"));
                }
            });
        }

        /// Opens the settings window on one of the Activity page's lists
        /// (`activity`, `unsynced`, `excluded` or `stops`), or another page.
        fn show_list(&self, page: &str) {
            if let Err(e) = oneup_app::launch::start_settings_on(&self.dir, page) {
                self.icon.ask("OneUpDrive", &format!("{e:#}"), Kind::Error, Buttons::Ok);
            }
        }

        fn send(&self, d: Do) {
            let _ = self.tx.send(d);
        }

        fn menu(&mut self, x: i32, y: i32) {
            let items = tray::menu(&self.view, self.update.as_deref());
            let chosen = match self.icon.menu(x, y, &items) {
                Ok(Some(c)) => c,
                _ => return,
            };
            let status = match &self.view {
                View::Running(s) => Some(s.clone()),
                _ => None,
            };
            match chosen {
                id::OPEN_FOLDER => {
                    if let Some(s) = &status {
                        let _ = std::process::Command::new("explorer.exe").arg(&s.local).spawn();
                    }
                }
                id::SYNC_NOW => self.send(Do::Call(Call::SyncNow)),
                id::RESUME => self.send(Do::Call(Call::Resume { direction: Direction::Both })),
                c if (id::PAUSE..id::PAUSE + tray::PAUSES.len() as u32).contains(&c) => {
                    let minutes = tray::PAUSES[(c - id::PAUSE) as usize].1;
                    self.send(Do::Call(Call::Pause { direction: Direction::Both, minutes }));
                }
                c if (id::REVIEW_STOP..id::REVIEW_STOP + 8).contains(&c) => {
                    if let Some(s) = &status {
                        self.review_stop(s, (c - id::REVIEW_STOP) as usize);
                    }
                }
                id::REVIEW_OVERFLOW => {
                    if let Some(s) = &status {
                        self.review_overflow(s);
                    }
                }
                id::REVIEW_PRUNE => {
                    if let Some(s) = &status {
                        self.review_prune(s);
                    }
                }
                id::LIST_PRUNE => self.show_list("excluded"),
                // The settings window has the whole story: what happened,
                // what to do, Retry now, the log, and details to copy.
                id::PROBLEM => self.show_list("problem"),
                id::LIST_UNSYNCED => self.show_list("unsynced"),
                id::ACTIVITY => self.show_list("activity"),
                id::START => self.send(Do::Start),
                id::STOP => {
                    self.notices.stop_expected = true;
                    self.send(Do::Call(Call::Stop));
                }
                id::UPDATE => self.install_update(),
                id::SEARCH => self.show_list("find"),
                id::SETTINGS | id::STATUS => {
                    if let Err(e) = oneup_app::launch::start_settings(&self.dir) {
                        self.icon.ask("OneUpDrive", &format!("{e:#}"), Kind::Error, Buttons::Ok);
                    }
                }
                id::CLOSE => self.icon.quit(),
                _ => {}
            }
        }

        /// A notification was clicked: review what it was about.
        fn review_first(&mut self) {
            let s = match &self.view {
                View::Running(s) => s.clone(),
                View::Stopped(_) => return self.show_list("problem"),
                _ => return,
            };
            if !s.safety_stops.is_empty() {
                self.review_stop(&s, 0);
            } else if s.overflow.as_ref().is_some_and(|o| o.waiting) {
                self.review_overflow(&s);
            } else if s.prune_offer.is_some() {
                self.review_prune(&s);
            } else if s.problem.is_some() {
                self.show_list("problem");
            }
        }

        fn review_stop(&self, s: &Status, i: usize) {
            let Some(st) = s.safety_stops.get(i) else { return };
            let side = if st.side == "local" { "on this PC" } else { "in OneDrive" };
            let earlier =
                if st.recent > 0 { format!(", on top of {} in the last 15 minutes", st.recent) } else { String::new() };
            // A removal is approved only with its paths in view.
            let removal = (st.kind == "removal").then(|| oneup_app::duplicates::approval_text(&self.dir, &st.code, 10));
            if removal == Some(None) {
                let why = "The list of what goes isn't ready yet. Try again in a moment.";
                self.icon.ask("OneUpDrive", why, Kind::Info, Buttons::Ok);
                return;
            }
            let text = if let Some(Some(text)) = removal {
                text
            } else if st.kind == "content" {
                format!(
                    "{} changed files{earlier} look encrypted, possibly by ransomware.\n\n\
                     They weren't uploaded. Open a few. If they don't open, choose Not now, disconnect this PC \
                     from the network and get help: OneDrive still has the good versions.",
                    st.count
                )
            } else {
                format!(
                    "{} of {} synced items {side} would be deleted, overwritten or moved{earlier}.\n\n\
                     Nothing changes until you decide. If you didn't expect this, check the files first.",
                    st.items(),
                    st.synced_items
                )
            };
            const GO: i32 = 100;
            const SHOW: i32 = 101;
            const LATER: i32 = 102;
            let go = match st.kind.as_str() {
                "content" => "Upload them\nThey open fine.",
                "removal" => "Remove\nRemove the copies listed.",
                _ => "Go ahead\nMake these changes now.",
            };
            let choices = [
                (GO, go),
                (SHOW, "Show the files\nEach item and what would happen to it."),
                (LATER, "Not now\nNothing changes. Review later from this icon."),
            ];
            loop {
                // Enter shows the files: it never approves.
                match self.icon.choose("Safety stop", &text, Kind::Warning, &choices, SHOW) {
                    Some(GO) => {
                        self.send(Do::Call(Call::Answer { code: st.code.clone(), answer: Answer::Approve }));
                        return;
                    }
                    Some(SHOW) => {
                        if !oneup_app::stoplist::path(&self.dir, &st.code).exists() {
                            let why = "The list isn't ready yet. Try again in a few minutes.";
                            self.icon.ask("OneUpDrive", why, Kind::Info, Buttons::Ok);
                        } else {
                            self.show_list("stops");
                        }
                    }
                    _ => return,
                }
            }
        }

        fn review_overflow(&self, s: &Status) {
            let Some(o) = s.overflow.as_ref().filter(|o| o.waiting) else { return };
            let size = |b: u64| oneup_app::versions::size(b);
            let text = format!(
                "{} files ({}) were deleted in OneDrive. Their copies don't fit in the space left for kept \
                 copies ({}). They're still on this PC, untouched.\n\n\
                 Yes: remove them here and keep copies anyway (over the limit).\n\
                 No: remove them here without a copy (OneDrive's recycle bin has them for a while).\n\
                 Cancel: decide later.",
                o.count,
                size(o.bytes),
                size(o.room)
            );
            let answer = match self.icon.ask("OneUpDrive", &text, Kind::Warning, Buttons::YesNoCancel) {
                Reply::Yes => Answer::Keep,
                Reply::No => {
                    let sure = format!("Remove {} files from this PC without keeping a copy?", o.count);
                    if self.icon.ask("OneUpDrive", &sure, Kind::Warning, Buttons::YesNo) != Reply::Yes {
                        return;
                    }
                    Answer::Discard
                }
                _ => return,
            };
            self.send(Do::Call(Call::Answer { code: o.code.clone(), answer }));
        }
    }

    impl Ui<'_> {
        /// Confirms removing the excluded items the agent found in OneDrive,
        /// and offers to always do so (`cloud_copies = "prune"`).
        fn review_prune(&self, s: &Status) {
            let Some(o) = &s.prune_offer else { return };
            // OneDrive reports a folder's size some time after a change.
            let bytes = if o.bytes > 0 { format!(" ({})", oneup_app::versions::size(o.bytes)) } else { String::new() };
            let text = format!(
                "{} excluded items{bytes} are still in OneDrive.\n\n\
                 Delete from OneDrive: they're downloaded first if needed, then moved to OneDrive's recycle bin \
                 (kept up to 30 days) and leave your other devices. The copies here stay.\n\n\
                 Keep in OneDrive: their changes still come here, nothing is uploaded, and you won't be asked again.",
                o.items
            );
            const DELETE: i32 = 100;
            const KEEP: i32 = 101;
            const SHOW: i32 = 102;
            const LATER: i32 = 103;
            let choices = [
                (DELETE, "Delete from OneDrive\nKeep them only on this PC."),
                (KEEP, "Keep in OneDrive\nDownload only, never upload."),
                (SHOW, "Show list"),
                (LATER, "Decide later\nReview later from this icon."),
            ];
            loop {
                match self.icon.choose("OneUpDrive", &text, Kind::Question, &choices, KEEP) {
                    Some(DELETE) => break,
                    Some(KEEP) => {
                        // Sets `cloud_copies = "pull"` if it was `keep`, and
                        // answers the agent's offer by its code.
                        if let Err(e) = oneup(&self.dir, &["cloud", "prune", "--keep"]) {
                            self.icon.ask(
                                "OneUpDrive",
                                &format!("Nothing was changed: {e:#}"),
                                Kind::Error,
                                Buttons::Ok,
                            );
                        }
                        return;
                    }
                    Some(SHOW) => self.show_list("excluded"),
                    _ => return,
                }
            }
            let always = "Do this automatically from now on? Excluded items in OneDrive are downloaded if \
                          needed, then deleted from OneDrive. Large deletions still stop and ask.\n\n\
                          No: ask each time.";
            if self.icon.ask("OneUpDrive", always, Kind::Question, Buttons::YesNo) == Reply::Yes
                && let Err(e) = oneup(&self.dir, &["rules", "--cloud", "prune"])
            {
                self.icon.ask("OneUpDrive", &format!("The setting wasn't changed: {e:#}"), Kind::Error, Buttons::Ok);
            }
            self.send(Do::Call(Call::Answer { code: o.code.clone(), answer: Answer::Prune }));
        }
    }

    /// Runs `oneup ARGS` without a window and waits for it.
    fn oneup(dir: &Path, args: &[&str]) -> Result<()> {
        use std::os::windows::process::CommandExt as _;
        const CREATE_NO_WINDOW: u32 = 0x0800_0000;
        let out = std::process::Command::new(oneup_app::launch::sibling("oneup.exe")?)
            .arg("--data-dir")
            .arg(dir)
            .args(args)
            .stdin(std::process::Stdio::null())
            .creation_flags(CREATE_NO_WINDOW)
            .output()
            .context("running oneup")?;
        if !out.status.success() {
            anyhow::bail!("{}", String::from_utf8_lossy(&out.stderr).trim());
        }
        Ok(())
    }

    /// The shortest time between two statuses shown.
    const SHOWN_EVERY: Duration = Duration::from_millis(250);

    fn publish(shared: &Mutex<Shared>, waker: &Waker, f: impl FnOnce(&mut Shared)) {
        f(&mut shared.lock().unwrap_or_else(|e| e.into_inner()));
        waker.wake();
    }

    /// Follows the agent (see the module docs) and carries out the menu's
    /// actions.
    async fn follow(dir: PathBuf, shared: Arc<Mutex<Shared>>, waker: Waker, mut rx: mpsc::UnboundedReceiver<Do>) {
        let kick = Arc::new(Notify::new());
        {
            let (dir, shared, waker, kick) = (dir.clone(), Arc::clone(&shared), waker.clone(), Arc::clone(&kick));
            tokio::spawn(async move {
                while let Some(d) = rx.recv().await {
                    act(&dir, &shared, &waker, &kick, d).await;
                }
            });
        }
        let mut wait = Duration::from_secs(2);
        loop {
            match oneup_ipc::connect(&dir).await {
                Ok(mut c) => {
                    wait = Duration::from_secs(2);
                    // Any revision but the agent's answers at once.
                    let mut since = u64::MAX;
                    // Until it fails: the agent stopped (or was replaced).
                    while let Ok(s) = c.call(&Call::Watch { since }).await {
                        since = s.rev;
                        publish(&shared, &waker, |sh| sh.view = View::Running(Box::new(s)));
                        // While syncing, the status changes with every item:
                        // a few looks a second are plenty for the icon and
                        // spare Explorer an update per file.
                        tokio::time::sleep(SHOWN_EVERY).await;
                    }
                    continue;
                }
                Err(ClientError::NotRunning) => {
                    // Why it stopped, if an error stopped it: the agent saves
                    // that just after its pipe closes, so look twice.
                    let mut why = oneup_app::problem::load_stopped(&dir);
                    if why.is_none() {
                        tokio::time::sleep(Duration::from_secs(1)).await;
                        why = oneup_app::problem::load_stopped(&dir);
                    }
                    let view = why.map_or(View::NotRunning, |p| View::Stopped(Box::new(p)));
                    publish(&shared, &waker, |sh| sh.view = view);
                }
                Err(e) => {
                    let e = e.to_string();
                    publish(&shared, &waker, |sh| sh.view = View::Unreachable(e));
                }
            }
            tokio::select! {
                _ = tokio::time::sleep(wait) => {}
                // Just started from the menu: look again soon.
                _ = kick.notified() => wait = Duration::from_secs(1),
            }
            wait = (wait * 2).min(Duration::from_secs(60));
        }
    }

    async fn act(dir: &Path, shared: &Mutex<Shared>, waker: &Waker, kick: &Notify, d: Do) {
        let failure = match d {
            Do::Call(call) => match oneup_ipc::connect(dir).await {
                // The watch reports the new status; only failures matter here.
                Ok(mut c) => c.call(&call).await.err().map(|e| e.to_string()),
                Err(e) => Some(e.to_string()),
            },
            Do::Start => {
                let dir = dir.to_path_buf();
                let started = tokio::task::spawn_blocking(move || -> Result<()> {
                    oneup_app::setup::require_test_account(&dir)?;
                    oneup_app::launch::start_agent(&dir)?;
                    Ok(())
                })
                .await;
                // The follower looks for it now rather than after its delay.
                kick.notify_one();
                match started {
                    Ok(Ok(())) => None,
                    Ok(Err(e)) => Some(format!("{e:#}")),
                    Err(e) => Some(e.to_string()),
                }
            }
        };
        if let Some(f) = failure {
            publish(shared, waker, |sh| sh.failure = Some(f));
        }
    }
}
