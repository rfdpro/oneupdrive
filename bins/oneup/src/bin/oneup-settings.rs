#![windows_subsystem = "windows"]

#[cfg(windows)]
#[path = "../settings_window.rs"]
mod settings_window;

#[cfg(windows)]
fn main() -> anyhow::Result<()> {
    settings_window::run(std::env::args_os())
}

#[cfg(not(windows))]
fn main() {}
