//! Starting the background agent without a console window, and starting it
//! at sign-in.
//!
//! * [`start_agent`] launches `oneupd.exe` (next to the running executable)
//!   hidden and in its own process group, so it outlives the terminal or
//!   launcher that started it and Ctrl+C there doesn't reach it. Its output
//!   goes to [`LOG_FILE`] in the data folder; the previous run's is kept as
//!   `oneupd.log.1`.
//! * **Start at sign-in** is a per-user `Run` value (no admin rights) that
//!   runs `oneupd-start.exe`, a launcher without a console window that calls
//!   [`start_agent`]. Off unless the user turns it on (`oneup autostart on`).

use std::path::{Path, PathBuf};

use anyhow::{Context, Result};

/// In the data folder: the hidden agent's output.
pub const LOG_FILE: &str = "oneupd.log";
/// Where per-user programs started at sign-in are listed.
pub const RUN_KEY: &str = r"Software\Microsoft\Windows\CurrentVersion\Run";

/// `oneupd.exe` (or another program of ours) next to the running executable.
pub fn sibling(name: &str) -> Result<PathBuf> {
    let exe = std::env::current_exe().context("can't tell where this program is")?;
    let path = exe.with_file_name(name);
    if !path.exists() {
        anyhow::bail!("{} is missing next to {}", name, exe.display());
    }
    Ok(path)
}

/// Launches the agent for `data_dir` hidden (see the module docs) and
/// returns it. The caller checks first that none is running.
#[cfg(windows)]
pub fn start_agent(data_dir: &Path) -> Result<std::process::Child> {
    start_agent_with(data_dir, None)
}

/// [`start_agent`] for a vault pair: its keys go to the agent through its
/// standard input and are never written to disk (`crate::vaultpair`).
#[cfg(windows)]
pub fn start_vault_agent(data_dir: &Path, key: &oneup_vault::key::Masterkey) -> Result<std::process::Child> {
    start_agent_with(data_dir, Some(key))
}

#[cfg(windows)]
fn start_agent_with(data_dir: &Path, key: Option<&oneup_vault::key::Masterkey>) -> Result<std::process::Child> {
    const CREATE_NEW_PROCESS_GROUP: u32 = 0x0000_0200;
    const CREATE_NO_WINDOW: u32 = 0x0800_0000;

    let exe = sibling("oneupd.exe")?;
    let log = data_dir.join(LOG_FILE);
    // Our own files in the data folder: the last run's output is kept.
    if log.exists() {
        let _ = std::fs::rename(&log, data_dir.join(format!("{LOG_FILE}.1")));
    }
    let out = std::fs::File::create(&log).with_context(|| format!("creating {}", log.display()))?;
    let err = out.try_clone()?;
    // Otherwise the agent also inherits the terminal's output pipe and keeps
    // a caller that reads it (a script, `oneup start | …`) waiting.
    oneup_sys::inherit::keep_std_handles();
    let mut cmd = std::process::Command::new(exe);
    cmd.arg("--data-dir").arg(data_dir);
    if key.is_some() {
        cmd.arg("--key-stdin").stdin(std::process::Stdio::piped());
    } else {
        cmd.stdin(std::process::Stdio::null());
    }
    cmd.stdout(out).stderr(err);
    let mut child =
        spawn_outside_job(&mut cmd, CREATE_NO_WINDOW | CREATE_NEW_PROCESS_GROUP).context("starting oneupd")?;
    if let Some(k) = key {
        use std::io::Write as _;
        let mut stdin = child.stdin.take().context("no pipe to the agent")?;
        stdin.write_all(crate::vaultpair::key_line(k).as_bytes()).context("handing the vault's keys to the agent")?;
    }
    Ok(child)
}

/// Starts the notification-area icon (`oneup-tray.exe`) for `data_dir`.
/// It keeps to one instance a data folder by itself, so starting it again
/// is harmless.
#[cfg(windows)]
pub fn start_tray(data_dir: &Path) -> Result<()> {
    start_tray_with(data_dir, &[])
}

/// [`start_tray`] with more arguments (`--start` also starts syncing).
#[cfg(windows)]
pub fn start_tray_with(data_dir: &Path, more: &[&str]) -> Result<()> {
    const CREATE_NEW_PROCESS_GROUP: u32 = 0x0000_0200;
    let mut cmd = std::process::Command::new(sibling("oneup-tray.exe")?);
    cmd.arg("--data-dir")
        .arg(data_dir)
        .args(more)
        .stdin(std::process::Stdio::null())
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::null());
    spawn_outside_job(&mut cmd, CREATE_NEW_PROCESS_GROUP).context("starting oneup-tray")?;
    Ok(())
}

/// Starts `cmd` outside the caller's Windows job, if it's in one that
/// allows that (and as usual otherwise). A program Explorer started for
/// OneUpDrive's commands or status flyout runs in the job of the package's
/// COM helper, which Windows ends with everything in it: an agent or icon
/// started from there must not go with it.
#[cfg(windows)]
fn spawn_outside_job(cmd: &mut std::process::Command, flags: u32) -> std::io::Result<std::process::Child> {
    use std::os::windows::process::CommandExt as _;
    const CREATE_BREAKAWAY_FROM_JOB: u32 = 0x0100_0000;
    match cmd.creation_flags(flags | CREATE_BREAKAWAY_FROM_JOB).spawn() {
        // Access denied: the job doesn't allow it (or there's none that
        // does); start it inside, as before.
        Err(e) if e.raw_os_error() == Some(5) => cmd.creation_flags(flags).spawn(),
        r => r,
    }
}

/// Opens the settings window (`oneup-settings.exe`) for `data_dir`; one a
/// data folder, so opening it again brings the open one to the front.
#[cfg(windows)]
pub fn start_settings(data_dir: &Path) -> Result<()> {
    start_settings_on(data_dir, "")
}

/// [`start_settings`] on `page` (`oneup-settings --page`), which an open
/// window switches to as well.
#[cfg(windows)]
pub fn start_settings_on(data_dir: &Path, page: &str) -> Result<()> {
    const CREATE_NEW_PROCESS_GROUP: u32 = 0x0000_0200;
    let mut cmd = std::process::Command::new(sibling("oneup-settings.exe")?);
    cmd.arg("--data-dir").arg(data_dir);
    if !page.is_empty() {
        cmd.arg("--page").arg(page);
    }
    cmd.stdin(std::process::Stdio::null()).stdout(std::process::Stdio::null()).stderr(std::process::Stdio::null());
    spawn_outside_job(&mut cmd, CREATE_NEW_PROCESS_GROUP).context("opening the settings window")?;
    Ok(())
}

/// The agents that start with the main one of `main`: its extra folder
/// pairs (`crate::pairs`), and its vaults whose keys are remembered (the
/// others wait for their password). Those already running are left alone;
/// returns the ones that failed to start, with why.
#[cfg(windows)]
pub async fn start_others(main: &Path) -> Vec<(String, String)> {
    let mut dirs: Vec<(String, PathBuf)> =
        crate::pairs::list(main).into_iter().map(|p| (format!("folder pair {}", p.name), p.dir)).collect();
    dirs.extend(
        crate::vaultpair::list(main)
            .into_iter()
            .filter(|v| v.dir.join(crate::vaultpair::KEY_FILE).exists())
            .map(|v| (format!("vault {}", v.name), v.dir)),
    );
    let mut failed = Vec::new();
    for (what, dir) in dirs {
        if oneup_ipc::connect(&dir).await.is_ok() {
            continue;
        }
        if let Err(e) = start_agent(&dir) {
            failed.push((what, format!("{e:#}")));
        }
    }
    failed
}

/// Every data folder that runs alongside `main` (its vaults and extra
/// pairs), for stopping them all.
pub fn children(main: &Path) -> Vec<PathBuf> {
    let mut out: Vec<PathBuf> = crate::vaultpair::list(main).into_iter().map(|v| v.dir).collect();
    out.extend(crate::pairs::list(main).into_iter().map(|p| p.dir));
    out
}

/// Starts the icon if the settings want it, when the agent starts.
#[cfg(windows)]
pub fn start_tray_if_wanted(data_dir: &Path) {
    // A vault or extra pair shows in its main data folder's icon.
    if !crate::vaultpair::is_child_dir(data_dir)
        && crate::config::load_agent(data_dir).is_ok_and(|a| a.tray_icon)
        && let Err(e) = start_tray(data_dir)
    {
        tracing::debug!(error = %e, "no tray icon");
    }
}

/// The `Run` value's name for `data_dir`: one per data folder, readable in
/// Task Manager's start-up list.
pub fn run_value_name(data_dir: &Path) -> String {
    let leaf = data_dir.file_name().map(|n| n.to_string_lossy().into_owned()).unwrap_or_default();
    format!("OneUpDrive agent ({leaf})")
}

/// The command the `Run` value holds.
pub fn run_command(launcher: &Path, data_dir: &Path) -> String {
    format!("\"{}\" --data-dir \"{}\"", launcher.display(), data_dir.display())
}

/// Turns start at sign-in on or off for `data_dir`, in `key` (the real
/// [`RUN_KEY`], or a scratch key in tests). Returns whether anything changed.
#[cfg(windows)]
pub fn set_autostart(key: &str, data_dir: &Path, launcher: Option<&Path>) -> Result<bool> {
    use oneup_sys::user_registry as reg;
    let name = run_value_name(data_dir);
    match launcher {
        Some(l) => {
            let cmd = run_command(l, data_dir);
            if reg::get(key, &name)?.as_deref() == Some(cmd.as_str()) {
                return Ok(false);
            }
            reg::set(key, &name, &cmd)?;
            Ok(true)
        }
        None => Ok(reg::remove(key, &name)?),
    }
}

/// The command set to start the agent of `data_dir` at sign-in, if any.
#[cfg(windows)]
pub fn autostart(key: &str, data_dir: &Path) -> Result<Option<String>> {
    Ok(oneup_sys::user_registry::get(key, &run_value_name(data_dir))?)
}

#[cfg(all(test, windows))]
mod tests {
    #![allow(clippy::unwrap_used)]
    use super::*;

    #[test]
    fn autostart_is_one_value_per_data_folder_and_off_by_default() {
        let key = format!(r"Software\OneUpDrive-test-autostart-{}", std::process::id());
        let a = Path::new(r"C:\Users\x\AppData\Local\OneUpDrive-test");
        let b = Path::new(r"C:\Users\x\AppData\Local\OneUpDrive");
        let launcher = Path::new(r"C:\Program Files\OneUpDrive\oneupd-start.exe");
        assert_eq!(autostart(&key, a).unwrap(), None);
        assert!(set_autostart(&key, a, Some(launcher)).unwrap());
        assert!(!set_autostart(&key, a, Some(launcher)).unwrap(), "already on: no change");
        assert_eq!(
            autostart(&key, a).unwrap().as_deref(),
            Some(
                r#""C:\Program Files\OneUpDrive\oneupd-start.exe" --data-dir "C:\Users\x\AppData\Local\OneUpDrive-test""#
            )
        );
        assert_eq!(autostart(&key, b).unwrap(), None, "another data folder is separate");
        assert!(set_autostart(&key, a, None).unwrap());
        assert!(!set_autostart(&key, a, None).unwrap());
        oneup_sys::user_registry::remove_key(&key).unwrap();
    }
}
