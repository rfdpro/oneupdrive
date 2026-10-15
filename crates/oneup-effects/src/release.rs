//! Finishing another program's stalled unlinking, for a [`Release`] token
//! (see `oneup_gate::release`). Windows then removes the online-only
//! placeholders in the folder and turns the rest into ordinary files, as an
//! ordinary unlinking would (measured, `oneup-cfapi` tests
//! `a_stalled_unlink_can_be_finished_by_another_program` and
//! `a_folder_held_open_is_left_orphaned_until_released_again`).
//!
//! Windows' own pass skips any folder a program has open (its current
//! folder), and on a PC where many programs last used Documents that can't
//! be avoided. [`clear_left_behind`] then finishes it item by item, which
//! a program's current folder doesn't block (measured:
//! `left_behind_placeholders_are_cleared_item_by_item_while_held`).

use std::path::{Path, PathBuf};

use oneup_cfapi::provider;
use oneup_gate::release::Release;
use oneup_ntfs::FileId;
use oneup_ntfs::win;

use crate::local::{Error, Result};

/// A placeholder a release left behind, as the scan found it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct LeftItem {
    pub path: PathBuf,
    pub id: u128,
    pub folder: bool,
    /// The file's size the scan saw (0 for folders).
    pub size: i64,
}

/// What [`clear_left_behind`] did.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Cleared {
    /// Online-only files removed (their content is in OneDrive).
    pub removed: u64,
    /// Files with all their content here, and folders, made ordinary.
    pub files: u64,
    pub folders: u64,
    /// Items that weren't what the scan saw any more, or that Windows
    /// refused: left as they are.
    pub skipped: u64,
}

/// The short-lived registration that releases placeholders left behind.
const CLEANUP_NAME: &str = "OneUpDrive cleanup";
const CLEANUP_ID: u128 = 0x6f6e_6575_7064_7269_7665_636c_6561_6e75;

/// Releases the folder, if, just before, it's still registered by the same
/// provider with the same identity (or, for placeholders left behind, by no
/// one), and none of the app's processes runs (one could be serving or
/// changing its placeholders). Nothing is done otherwise: [`Error::Changed`]
/// or [`Error::Busy`]. Windows reports success even where a program held a
/// folder open; the caller rescans to find out.
pub fn release_root(a: &Release) -> Result<()> {
    let now = provider::registration(a.root());
    match (a.held(), &now) {
        (Some((provider, identity)), Some((name, id)))
            if name == provider && id.as_slice() == identity && name != oneup_cfapi::PROVIDER_NAME => {}
        (None, None) => {}
        _ => return Err(Error::Changed),
    }
    let running = oneup_sys::processes::inside_dir(a.app_dir()).map_err(|_| Error::Busy)?;
    if !running.is_empty() {
        return Err(Error::Busy);
    }
    if a.held().is_none() {
        provider::register_as(a.root(), CLEANUP_NAME, CLEANUP_ID, b"oneup-cleanup", false).map_err(win_error)?;
    }
    provider::unregister(a.root()).map_err(win_error)?;
    if provider::registration(a.root()).is_some() {
        return Err(Error::Changed);
    }
    Ok(())
}

/// For a [`Release`] of placeholders left behind ([`Release::held`] is
/// `None`): each one, re-checked just before (the same file ID, size, and
/// still a cloud placeholder), becomes what an ordinary unlinking makes of
/// it: a file without all its content here is removed (the gate checked
/// it's in OneDrive; a file only partly here can't hold edits, which
/// download it in full first), a file with all its content here or a folder
/// becomes an ordinary one with its content. `files` first, then `folders`,
/// deepest first. Judged by the handle's reparse tag and attributes:
/// Windows won't describe a placeholder no sync root holds.
pub fn clear_left_behind(
    a: &Release,
    files: impl IntoIterator<Item = LeftItem>,
    folders: impl IntoIterator<Item = LeftItem>,
) -> Result<Cleared> {
    if a.held().is_some() || provider::registration(a.root()).is_some() {
        return Err(Error::Changed);
    }
    let running = oneup_sys::processes::inside_dir(a.app_dir()).map_err(|_| Error::Busy)?;
    if !running.is_empty() {
        return Err(Error::Busy);
    }
    // Windows may disguise placeholders from this process (AGENTS.md):
    // judged disguised, an online-only file would look ordinary.
    oneup_cfapi::expose_placeholders().map_err(|e| Error::Io(std::io::Error::other(e)))?;
    let mut done = Cleared::default();
    for item in files.into_iter().chain(folders) {
        match clear_one(a.root(), &item) {
            Ok(Some(Outcome::Removed)) => done.removed += 1,
            Ok(Some(Outcome::Reverted)) if item.folder => done.folders += 1,
            Ok(Some(Outcome::Reverted)) => done.files += 1,
            Ok(None) => {}
            Err(e) => {
                tracing::debug!(error = %e, "a placeholder left behind was left as it is");
                done.skipped += 1;
            }
        }
    }
    Ok(done)
}

enum Outcome {
    Removed,
    Reverted,
}

/// Every cloud placeholder's reparse tag, whichever variant.
const CLOUD_TAG_MASK: u32 = 0xFFFF_0FFF;
const IO_REPARSE_TAG_CLOUD: u32 = 0x9000_001A;

/// `None`: already an ordinary item.
fn clear_one(root: &Path, item: &LeftItem) -> Result<Option<Outcome>> {
    if !item.path.starts_with(root) {
        return Err(Error::Outside);
    }
    // Attribute (and, for a file, delete) access only: data access would
    // ask for content, and a folder some program sits in doesn't share
    // delete access.
    let h = if item.folder {
        win::open_path_for_attributes(&item.path)?
    } else {
        win::open_path_for_dehydrated(&item.path)?
    };
    let id = FileId(item.id);
    if win::file_id(&h)?.1 != id {
        return Err(Error::Changed);
    }
    let entry = win::stat_handle(&h, id)?.ok_or(Error::Changed)?;
    if entry.is_dir() != item.folder || (!item.folder && i64::try_from(entry.size).ok() != Some(item.size)) {
        return Err(Error::Changed);
    }
    if entry.reparse_tag & CLOUD_TAG_MASK != IO_REPARSE_TAG_CLOUD {
        return Ok(None);
    }
    if !item.folder && entry.is_recall_on_access() {
        win::delete_handle(&h)?;
        return Ok(Some(Outcome::Removed));
    }
    provider::revert_orphan(&h.as_file()).map_err(win_error)?;
    Ok(Some(Outcome::Reverted))
}

fn win_error(e: provider::Error) -> Error {
    match e {
        provider::Error::Io(io) => Error::Io(io),
        other => {
            tracing::warn!(error = %other, "Windows refused to release the folder");
            Error::Changed
        }
    }
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used)]
    use super::*;
    use oneup_gate::release::{Evidence, Holder, review_release};

    fn token(root: &Path, app_dir: &Path, held: Option<&[u8]>, left_behind: u64) -> Release {
        review_release(&Evidence {
            root: root.to_path_buf(),
            held: held.map(|identity| Holder {
                provider: "FakeOneDrive".into(),
                identity: identity.to_vec(),
                ours: false,
            }),
            left_behind,
            account_removed: true,
            app_dir: app_dir.to_path_buf(),
            app_running: 0,
            same_account: Some(true),
            scanned_at_ms: Some(1),
            listed_at_ms: Some(1),
            now_ms: 1,
            in_cloud: (1, 0),
            missing: (0, 0),
            choices_recorded: true,
            confirmed: true,
        })
        .unwrap()
    }

    fn placeholder(name: &str) -> provider::NewPlaceholder<'_> {
        provider::NewPlaceholder { name, identity: b"OD!a", size: 5, modified: 133_000_000_000_000_000, folder: false }
    }

    #[test]
    fn releases_only_the_folder_the_gate_saw() {
        let base = std::env::temp_dir().join(format!("oneup-effects-release-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&base);
        let (root, app) = (base.join("root"), base.join("app"));
        std::fs::create_dir_all(&root).unwrap();
        std::fs::create_dir_all(&app).unwrap();
        provider::register_as(&root, "FakeOneDrive", 0x0dd0_0dd0_0dd0_0dd0_0dd0_0dd0_0dd0_0dd2, b"their-root", false)
            .unwrap();
        provider::create(&root, &[placeholder("a.txt")]).unwrap();

        // Registered with another identity than the gate saw: left alone.
        assert!(matches!(release_root(&token(&root, &app, Some(b"other"), 0)), Err(Error::Changed)));
        // Registered, though the gate saw it left behind with no root: left alone.
        assert!(matches!(release_root(&token(&root, &app, None, 1)), Err(Error::Changed)));
        assert!(provider::registration(&root).is_some() && root.join("a.txt").exists());
        // A process of the app's is running: left alone.
        let system = std::path::PathBuf::from(std::env::var_os("SystemRoot").unwrap()).join("System32");
        assert!(matches!(release_root(&token(&root, &system, Some(b"their-root"), 0)), Err(Error::Busy)));
        assert!(provider::registration(&root).is_some());

        release_root(&token(&root, &app, Some(b"their-root"), 0)).unwrap();
        assert!(provider::registration(&root).is_none());
        assert!(!root.join("a.txt").exists(), "online-only: removed by Windows");
        // Not a sync root any more: nothing to release as held.
        assert!(matches!(release_root(&token(&root, &app, Some(b"their-root"), 0)), Err(Error::Changed)));
        let _ = std::fs::remove_dir_all(&base);
    }

    /// Windows' pass skips a folder a program has open; item by item isn't
    /// blocked by it (measured).
    #[test]
    fn left_behind_placeholders_are_cleared_item_by_item_while_held() {
        let base = std::env::temp_dir().join(format!("oneup-effects-items-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&base);
        let (root, app) = (base.join("root"), base.join("app"));
        std::fs::create_dir_all(&root).unwrap();
        std::fs::create_dir_all(&app).unwrap();
        provider::register_as(&root, "FakeOneDrive", 0x0dd0_0dd0_0dd0_0dd0_0dd0_0dd0_0dd0_0dd9, b"their-root", false)
            .unwrap();
        let folder = |name| provider::NewPlaceholder {
            name,
            identity: b"OD!d",
            size: 0,
            modified: 133_000_000_000_000_000,
            folder: true,
        };
        provider::create(&root, &[folder("docs")]).unwrap();
        let docs = root.join("docs");
        provider::create(&docs, &[placeholder("online.txt"), placeholder("partial.txt"), folder("sub")]).unwrap();
        // A file with its content here, and an empty one: placeholders
        // made from ordinary files.
        std::fs::write(docs.join("here.txt"), b"content").unwrap();
        std::fs::write(docs.join("empty.txt"), b"").unwrap();
        for n in ["here.txt", "empty.txt"] {
            provider::Exclusive::open(&docs.join(n)).unwrap().convert(b"OD!x").unwrap();
        }
        // Another program sits in docs throughout.
        let mut holder = std::process::Command::new("cmd")
            .args(["/C", "pause"])
            .current_dir(&docs)
            .stdin(std::process::Stdio::piped())
            .stdout(std::process::Stdio::null())
            .spawn()
            .unwrap();
        std::thread::sleep(std::time::Duration::from_millis(500));
        provider::unregister(&root).unwrap();
        let is_placeholder =
            |p: &Path| provider::open(p).ok().and_then(|f| provider::info(&f).ok().flatten()).is_some();
        assert!(is_placeholder(&docs) && docs.join("online.txt").exists(), "left behind");

        let item = |p: &Path, folder: bool, size: i64| {
            let h = win::open_path_for_attributes(p).unwrap();
            LeftItem { path: p.to_path_buf(), id: win::file_id(&h).unwrap().1.0, folder, size }
        };
        let mut moved = item(&docs.join("partial.txt"), false, 5);
        moved.id ^= 1; // not what the scan saw any more
        let files = vec![
            item(&docs.join("online.txt"), false, 5),
            item(&docs.join("here.txt"), false, 7),
            item(&docs.join("empty.txt"), false, 0),
            moved,
        ];
        let folders = vec![item(&docs.join("sub"), true, 0), item(&docs, true, 0)];
        let done = clear_left_behind(&token(&root, &app, None, 6), files, folders).unwrap();
        assert_eq!(done, Cleared { removed: 1, files: 2, folders: 2, skipped: 1 });
        assert!(!docs.join("online.txt").exists(), "online-only: removed");
        assert_eq!(std::fs::read(docs.join("here.txt")).unwrap(), b"content");
        assert!(!is_placeholder(&docs.join("here.txt")) && !is_placeholder(&docs.join("empty.txt")));
        assert!(!is_placeholder(&docs) && !is_placeholder(&docs.join("sub")), "folders ordinary, the held one too");
        assert!(docs.join("partial.txt").exists(), "the changed one is left as it is");
        let _ = holder.kill();
        let _ = holder.wait();
        let _ = std::fs::remove_dir_all(&base);
    }

    #[test]
    fn placeholders_left_behind_are_released_again() {
        let base = std::env::temp_dir().join(format!("oneup-effects-left-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&base);
        let (root, app) = (base.join("root"), base.join("app"));
        let docs = root.join("docs");
        std::fs::create_dir_all(&docs).unwrap();
        std::fs::create_dir_all(&app).unwrap();
        provider::register_as(&root, "FakeOneDrive", 0x0dd0_0dd0_0dd0_0dd0_0dd0_0dd0_0dd0_0dd5, b"their-root", false)
            .unwrap();
        provider::create(&docs, &[placeholder("a.txt")]).unwrap();
        // Released while another program sits in docs: left behind there.
        let mut holder = std::process::Command::new("cmd")
            .args(["/C", "pause"])
            .current_dir(&docs)
            .stdin(std::process::Stdio::piped())
            .stdout(std::process::Stdio::null())
            .spawn()
            .unwrap();
        std::thread::sleep(std::time::Duration::from_millis(500));
        provider::unregister(&root).unwrap();
        let _ = holder.kill();
        let _ = holder.wait();
        assert!(provider::registration(&root).is_none() && docs.join("a.txt").exists());

        release_root(&token(&root, &app, None, 1)).unwrap();
        assert!(provider::registration(&root).is_none(), "the cleanup registration is gone too");
        assert!(!docs.join("a.txt").exists(), "online-only: removed by Windows this time");
        let _ = std::fs::remove_dir_all(&base);
    }
}
