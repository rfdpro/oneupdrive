//! What a directory entry is, and whether the scanner descends into it.
//!
//! Pure functions of the listing metadata (plus the exclusion rules), so the
//! decisions are unit-tested without touching a disk.

use oneup_ntfs::dirent::{DirEntry, FILE_ATTRIBUTE_REPARSE_POINT};
use oneup_rules::ExcludeSet;
use oneup_store::{LocalKind, LocalRow, Skip};

/// `IO_REPARSE_TAG_CLOUD` and its variants `CLOUD_1`..`CLOUD_F` differ only
/// in bits 12..16.
const CLOUD_TAG_MASK: u32 = 0xFFFF_0FFF;
const IO_REPARSE_TAG_CLOUD: u32 = 0x9000_001A;

/// Whether a reparse tag marks a Cloud Files placeholder (OneDrive, ...).
pub fn is_cloud_tag(tag: u32) -> bool {
    tag & CLOUD_TAG_MASK == IO_REPARSE_TAG_CLOUD
}

/// The kind of entry, as far as the scanner is concerned.
///
/// Directories that are reparse points are only treated as folders when they
/// are cloud placeholders. Everything else (symlinks, junctions, mount points,
/// and tags we don't know such as ProjFS or container layers) becomes
/// [`LocalKind::Link`] and is never followed: its contents live elsewhere, may
/// be virtualised by another driver, or could loop back into the tree.
pub fn kind_of(e: &DirEntry) -> LocalKind {
    if e.is_link() {
        return LocalKind::Link;
    }
    if e.is_dir() {
        let reparse = e.attributes & FILE_ATTRIBUTE_REPARSE_POINT != 0;
        if reparse && !is_cloud_tag(e.reparse_tag) {
            return LocalKind::Link;
        }
        return LocalKind::Folder;
    }
    LocalKind::File
}

/// Whether listing this folder would make a cloud provider fetch its children.
///
/// Two independent signals, either of which is enough: the recall attributes
/// in the listing metadata, and the Cloud Files API's `PARTIAL` state. Both
/// were observed on an unpopulated placeholder folder, and both cleared once
/// it had been listed.
pub fn is_unpopulated(attributes: u32, reparse_tag: u32) -> bool {
    use oneup_ntfs::dirent::{
        FILE_ATTRIBUTE_OFFLINE, FILE_ATTRIBUTE_RECALL_ON_DATA_ACCESS, FILE_ATTRIBUTE_RECALL_ON_OPEN,
    };
    attributes & (FILE_ATTRIBUTE_RECALL_ON_OPEN | FILE_ATTRIBUTE_RECALL_ON_DATA_ACCESS | FILE_ATTRIBUTE_OFFLINE) != 0
        || oneup_cfapi::placeholder_state(attributes, reparse_tag).is_partial()
}

/// Turns a listed entry into the row to store. The caller descends into the
/// entry only if the row is a folder with [`Skip::None`].
///
/// Order of the folder checks matters: an excluded folder is reported as
/// excluded even if it is also unpopulated, because exclusion is the user's
/// decision and the more useful thing to show.
///
/// `at_root`: the entry is directly inside the scanned folder, where
/// OneUpDrive's own folder ([`oneup_rules::ENGINE_DIR`]) lives; it is
/// always excluded.
pub fn to_row(e: &DirEntry, parent: u128, at_root: bool, rules: &ExcludeSet) -> LocalRow {
    let kind = kind_of(e);
    let skip = match kind {
        LocalKind::Folder if at_root && oneup_rules::is_engine_dir(&e.name) => Skip::Excluded,
        LocalKind::Folder if rules.excludes_folder(&e.name) => Skip::Excluded,
        LocalKind::Folder if is_unpopulated(e.attributes, e.reparse_tag) => Skip::Unpopulated,
        _ => Skip::None,
    };
    LocalRow {
        file_id: e.file_id.0,
        parent_id: Some(parent),
        name: e.name.clone(),
        name_lossy: e.name_lossy,
        kind,
        // Folder sizes reported by the listing are meaningless (usually 0).
        size: if kind == LocalKind::File { i64::try_from(e.size).unwrap_or(i64::MAX) } else { 0 },
        mtime: e.modified,
        attributes: e.attributes,
        reparse_tag: e.reparse_tag,
        skip,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use oneup_ntfs::FileId;
    use oneup_ntfs::dirent::{
        FILE_ATTRIBUTE_DIRECTORY, FILE_ATTRIBUTE_RECALL_ON_DATA_ACCESS, FILE_ATTRIBUTE_RECALL_ON_OPEN,
        IO_REPARSE_TAG_MOUNT_POINT, IO_REPARSE_TAG_SYMLINK,
    };

    const CLOUD_6: u32 = 0x9000_601A;
    const PROJFS: u32 = 0x9000_001C;

    fn entry(name: &str, attributes: u32, reparse_tag: u32) -> DirEntry {
        DirEntry {
            file_id: FileId(42),
            name: name.into(),
            name_lossy: false,
            size: 5,
            attributes,
            reparse_tag,
            created: 1,
            modified: 2,
            changed: 3,
        }
    }

    const DIR: u32 = FILE_ATTRIBUTE_DIRECTORY;
    const DIR_RP: u32 = FILE_ATTRIBUTE_DIRECTORY | FILE_ATTRIBUTE_REPARSE_POINT;

    #[test]
    fn kinds() {
        assert_eq!(kind_of(&entry("f", 0x20, 0)), LocalKind::File);
        assert_eq!(kind_of(&entry("d", DIR, 0)), LocalKind::Folder);
        assert_eq!(kind_of(&entry("j", DIR_RP, IO_REPARSE_TAG_MOUNT_POINT)), LocalKind::Link);
        assert_eq!(kind_of(&entry("s", DIR_RP, IO_REPARSE_TAG_SYMLINK)), LocalKind::Link);
        assert_eq!(kind_of(&entry("p", DIR_RP, PROJFS)), LocalKind::Link, "unknown reparse dirs aren't followed");
        assert_eq!(kind_of(&entry("c", DIR_RP, CLOUD_6)), LocalKind::Folder, "cloud placeholder dirs are folders");
        assert_eq!(kind_of(&entry("fs", FILE_ATTRIBUTE_REPARSE_POINT, IO_REPARSE_TAG_SYMLINK)), LocalKind::Link);
        // Reparse *files* other than links (e.g. cloud placeholders) are files.
        assert_eq!(kind_of(&entry("cf", FILE_ATTRIBUTE_REPARSE_POINT, CLOUD_6)), LocalKind::File);
    }

    #[test]
    fn cloud_tags() {
        assert!(is_cloud_tag(0x9000_001A));
        assert!(is_cloud_tag(0x9000_F01A));
        assert!(!is_cloud_tag(PROJFS));
        assert!(!is_cloud_tag(IO_REPARSE_TAG_SYMLINK));
    }

    #[test]
    fn skips() {
        let rules = ExcludeSet::dev();
        let row = |e: DirEntry| to_row(&e, 1, false, &rules);
        // Our own folder: excluded only directly in the sync folder.
        assert_eq!(to_row(&entry(".OneUp", DIR, 0), 1, true, &rules).skip, Skip::Excluded);
        assert_eq!(row(entry(".oneup", DIR, 0)).skip, Skip::None);
        assert_eq!(to_row(&entry(".oneup", DIR, 0), 1, true, &ExcludeSet::none()).skip, Skip::Excluded);
        assert_eq!(row(entry("Node_Modules", DIR, 0)).skip, Skip::Excluded);
        assert_eq!(row(entry("src", DIR, 0)).skip, Skip::None);
        let lazy = entry("lazy", DIR_RP | FILE_ATTRIBUTE_RECALL_ON_OPEN, CLOUD_6);
        assert_eq!(row(lazy).skip, Skip::Unpopulated);
        let lazy_junk = entry("node_modules", DIR_RP | FILE_ATTRIBUTE_RECALL_ON_OPEN, CLOUD_6);
        assert_eq!(row(lazy_junk).skip, Skip::Excluded, "exclusion wins");
        // Files are never skipped, even when online-only; links neither.
        let online = row(entry("big.iso", FILE_ATTRIBUTE_RECALL_ON_DATA_ACCESS, CLOUD_6));
        assert_eq!((online.kind, online.skip, online.size), (LocalKind::File, Skip::None, 5));
        // A *file* named like an excluded folder is not excluded.
        assert_eq!(row(entry("target", 0x20, 0)).skip, Skip::None);
        assert_eq!(row(entry("j", DIR_RP, IO_REPARSE_TAG_MOUNT_POINT)).skip, Skip::None);
    }

    #[test]
    fn folder_sizes_are_zeroed_and_metadata_kept() {
        let r = to_row(&entry("d", DIR, 0), 9, false, &ExcludeSet::none());
        assert_eq!((r.size, r.parent_id, r.mtime, r.file_id), (0, Some(9), 2, 42));
    }
}
