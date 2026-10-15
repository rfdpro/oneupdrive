//! `oneup-open`: what Windows runs to open a Word, Excel or PowerPoint file
//! once the person has made OneUpDrive the default program for it
//! (`oneup_app::officeopen`). A file in a folder OneUpDrive syncs opens from
//! OneDrive with AutoSave; every other file opens in Office as it would
//! without OneUpDrive. No window of its own.

#![windows_subsystem = "windows"]

use std::path::PathBuf;

use oneup_app::officeopen::{self, Choice};

fn main() {
    let Some(path) = std::env::args_os().nth(1).map(PathBuf::from) else {
        return;
    };
    let choice = match tokio::runtime::Builder::new_current_thread().enable_all().build() {
        Ok(rt) => rt.block_on(officeopen::choose(&path)),
        Err(e) => Choice::Local(e.to_string()),
    };
    if let Choice::Cloud(uri) = &choice
        && oneup_sys::assoc::open(uri).is_ok()
    {
        return;
    }
    if let Err(e) = officeopen::open_here(&path) {
        oneup_sys::tray::error_box(
            "OneUpDrive",
            &format!("Couldn't open {} in Office: {e:#}", path.file_name().unwrap_or_default().to_string_lossy()),
        );
    }
}
