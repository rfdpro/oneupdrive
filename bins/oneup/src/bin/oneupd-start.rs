//! `oneupd-start`: starts the background agent without a console window.
//! This is what start at sign-in runs (`oneup autostart on`); `oneup start`
//! does the same from a terminal. A Windows-subsystem program, so nothing
//! flashes on screen. If the agent is already running, it only starts the
//! notification-area icon, as it does after starting the agent (unless
//! that's turned off in the settings). A
//! problem is written to the data folder's `oneupd.log`, as there is no
//! console to show it.
#![windows_subsystem = "windows"]

use std::path::PathBuf;

fn main() {
    let mut args = std::env::args_os().skip(1);
    let mut data_dir: Option<PathBuf> = None;
    while let Some(a) = args.next() {
        if a == "--data-dir" {
            data_dir = args.next().map(PathBuf::from);
        }
    }
    let Some(dir) = data_dir else { return };
    let Ok(rt) = tokio::runtime::Builder::new_current_thread().enable_all().build() else { return };
    let running = rt.block_on(async { oneup_ipc::connect(&dir).await.is_ok() });
    #[cfg(windows)]
    {
        if !running && let Err(e) = oneup_app::launch::start_agent(&dir) {
            let _ = std::fs::write(dir.join(oneup_app::launch::LOG_FILE), format!("oneupd-start: {e:#}\n"));
            return;
        }
        // Each failure is in that agent's own log.
        drop(rt.block_on(oneup_app::launch::start_others(&dir)));
        oneup_app::launch::start_tray_if_wanted(&dir);
    }
    #[cfg(not(windows))]
    let _ = running;
}
