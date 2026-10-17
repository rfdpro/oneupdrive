//! Writing OneUpDrive's own small files in a data folder (settings, the
//! saved pause, a vault's sealed keys) so that a crash or power cut leaves
//! either the old file or the new one, never an empty or partial one.
//!
//! The rename alone isn't enough: after a power cut NTFS can keep the
//! rename (its metadata is journaled) but not the data, leaving an empty
//! file in place of the old one. An empty pause file reads as "not paused",
//! an empty config.toml as "no rules". So the data is flushed to disk
//! before the rename. These are our own files, not user content, so they
//! are exempt from the effects layer.

use std::io::Write as _;
use std::path::Path;
use std::sync::atomic::{AtomicU64, Ordering::Relaxed};

/// Replaces `path` with `bytes`: a uniquely named file next to it is
/// written and flushed to disk, then renamed over `path` (which replaces
/// it on Windows). Two writers never share a temp file; on an error the
/// temp file is removed and `path` is left as it was.
pub fn write(path: &Path, bytes: &[u8]) -> std::io::Result<()> {
    static SEQ: AtomicU64 = AtomicU64::new(0);
    let name = path.file_name().ok_or_else(|| std::io::Error::other("not a file path"))?;
    let mut tmp_name = name.to_os_string();
    tmp_name.push(format!(".{}-{}.tmp", std::process::id(), SEQ.fetch_add(1, Relaxed)));
    let tmp = path.with_file_name(tmp_name);
    let written = (|| {
        let mut f = std::fs::OpenOptions::new().write(true).create_new(true).open(&tmp)?;
        f.write_all(bytes)?;
        f.sync_all()?;
        drop(f);
        std::fs::rename(&tmp, path)
    })();
    if written.is_err() {
        let _ = std::fs::remove_file(&tmp);
    }
    written
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn replaces_the_file_and_leaves_no_temp_file() {
        let d = std::env::temp_dir().join(format!("oneup-ownfile-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&d);
        std::fs::create_dir_all(&d).unwrap();
        let p = d.join("config.toml");
        write(&p, b"first").unwrap();
        write(&p, b"second").unwrap();
        assert_eq!(std::fs::read(&p).unwrap(), b"second");
        let names: Vec<_> = std::fs::read_dir(&d).unwrap().map(|e| e.unwrap().file_name()).collect();
        assert_eq!(names, vec![std::ffi::OsString::from("config.toml")], "no temp file left behind");
        // A folder that isn't there: an error, and nothing made.
        assert!(write(&d.join("missing").join("x"), b"x").is_err());
        let _ = std::fs::remove_dir_all(&d);
    }
}
